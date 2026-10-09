//! Resolving a git range to changed files, then to Hayvenhurst symbols.
//!
//! Used by `sirius link ... --changed` and the gate. We shell `git diff` for
//! the file list, then map the changed files to entities by PATH via
//! `hayven affected-tests --changed` (its `roots`) — never by basename.

use crate::hayven::Hayven;
use crate::shell::Runner;
use serde_json::Value;

/// Run one git command, mapping every failure mode (spawn error, non-zero
/// exit) to a single `Err(detail)`. THE one home for the run-git-or-explain
/// idiom — hand-rolled copies of this drift on error formatting.
pub fn run_git(runner: &dyn Runner, args: &[&str]) -> Result<crate::shell::CmdOutput, String> {
    let out = runner.run("git", args).map_err(|e| e.to_string())?;
    if !out.success() {
        let stderr = out.stderr.trim();
        return Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr.to_string()
        });
    }
    Ok(out)
}

/// Render a filesystem path for handing to `git`.
///
/// On Windows `std::fs::canonicalize` — which `Workspace::discover` uses to
/// resolve the workspace root — returns EXTENDED-LENGTH "verbatim" paths:
/// `\\?\C:\...`. Rust's own file APIs accept those happily, so they flow
/// through the codebase unnoticed. Git for Windows does not: it is MSYS-based
/// and rewrites `\\?\C:\...` into `//?/C:/...`, which it then cannot create.
///
///     fatal: could not create leading directories of
///     '//?/C:/.../.sirius/worktrees/sirius-oak/.git': Invalid argument
///
/// That killed `sirius run` on Windows outright — every worker died at the
/// worktree step, before a single issue was claimed (SF-16, observed
/// 2026-08-25). Proven rather than inferred: handing `git worktree add` the
/// same absolute path twice, plain succeeds (exit 0) and verbatim fails
/// (exit 128) with the message above.
///
/// So every path handed to git goes through here. `\\?\UNC\server\share` is
/// the verbatim spelling of `\\server\share` and needs the extra rewrite;
/// anything else — including all unix paths — passes through untouched.
pub fn git_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    }
}

/// Split command stdout into trimmed, non-empty lines.
fn stdout_lines(out: &crate::shell::CmdOutput) -> Vec<String> {
    out.stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(unquote_path)
        .collect()
}

/// Undo git's `core.quotePath` C-style quoting of a path (`"caf\303\251.rs"`
/// → `café.rs`): git quotes any path with a non-ASCII byte, a quote, a
/// backslash, or a control character. Without this a receipt would stamp the
/// quoted string, which names no file. Shas and ref names never start with
/// `"`, so every other line passes through untouched.
fn unquote_path(l: &str) -> String {
    let Some(inner) = l
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .filter(|_| l.len() >= 2)
    else {
        return l.to_string();
    };
    let b = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = b[i + 1];
        let oct = |x: u8| (b'0'..=b'7').contains(&x);
        if oct(c) && i + 3 < b.len() && oct(b[i + 2]) && oct(b[i + 3]) {
            out.push(((c - b'0') << 6) | ((b[i + 2] - b'0') << 3) | (b[i + 3] - b'0'));
            i += 4;
            continue;
        }
        out.push(match c {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'v' => 0x0b,
            other => other, // `\\` and `\"`
        });
        i += 2;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// List files changed in a git range (default: working tree vs HEAD).
pub fn changed_files(runner: &dyn Runner, range: Option<&str>) -> Result<Vec<String>, String> {
    // `git diff --name-only <range>`; with no range, diff HEAD (staged+unstaged).
    let out = run_git(runner, &["diff", "--name-only", range.unwrap_or("HEAD")])?;
    Ok(stdout_lines(&out))
}

/// The current HEAD commit id — captured BEFORE an agent runs so the gate can
/// diff against a fixed baseline instead of whatever HEAD means afterwards.
pub fn head_rev(runner: &dyn Runner) -> Result<String, String> {
    let out = run_git(runner, &["rev-parse", "HEAD"])?;
    let rev = out.stdout.trim().to_string();
    if rev.is_empty() {
        return Err("git rev-parse HEAD produced no output".into());
    }
    Ok(rev)
}

/// Untracked (never-`git add`ed) files. `git diff` cannot see these, but a new
/// module or test file is as real a change as an edit.
pub fn untracked_files(runner: &dyn Runner) -> Result<Vec<String>, String> {
    let out = run_git(runner, &["ls-files", "--others", "--exclude-standard"])?;
    Ok(stdout_lines(&out))
}

/// Everything changed since `base` (a commit id captured before the work):
/// committed + staged + unstaged (`git diff <base>`) plus NEW untracked files.
/// SIRF-11: `git diff HEAD` alone is blind to work an agent COMMITTED (agents
/// routinely commit) and to new untracked files — both used to read as
/// "nothing changed", which skipped the gate entirely. With no `base` this
/// degrades to the old worktree-vs-HEAD diff, still plus new untracked files.
///
/// `pre_untracked` is the untracked snapshot taken BEFORE the work: untracked
/// files that already existed (developer scratch files, un-ignored artifacts)
/// are not the agent's doing, and counting them made every iteration look like
/// it changed something — which could gate (and even advance) an issue whose
/// agent did nothing at all.
pub fn changed_since(
    runner: &dyn Runner,
    base: Option<&str>,
    pre_untracked: &[String],
) -> Result<Vec<String>, String> {
    let mut files = changed_files(runner, base)?;
    // HashSets: on repos with thousands of un-ignored untracked files, the
    // naive Vec::contains scans are O(U x (P + C)) per gate evaluation.
    let pre: std::collections::HashSet<&str> = pre_untracked.iter().map(String::as_str).collect();
    let seen: std::collections::HashSet<String> = files.iter().cloned().collect();
    for f in untracked_files(runner)? {
        if !pre.contains(f.as_str()) && !seen.contains(&f) {
            files.push(f);
        }
    }
    Ok(files)
}

/// What a git range resolved to: the changed files and the entities in them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangedSymbols {
    pub files: Vec<String>,
    pub symbols: Vec<String>,
}

/// Resolve a git range's changed files to Hayvenhurst entity ids, by PATH.
///
/// SIRF-20: this used to `hayven query <file stem>` per file — a full-text
/// search on the BASENAME, so a commit touching one package's `types.ts`
/// stamped every `types`/`index` module in every package of a monorepo
/// (83 symbols from 5 files, observed), writing false provenance into the
/// ledger. `hayven affected-tests --changed <files>` reports `roots`: the
/// daemon's own path-exact file → entity resolution (the same mapping the
/// gate trusts). Unindexed files (lockfiles, configs) contribute nothing.
/// A daemon that cannot answer is an ERROR, never a silently empty stamp.
pub fn changed_symbols(
    runner: &dyn Runner,
    hv: &Hayven,
    range: Option<&str>,
    fleet: Option<FleetBase<'_>>,
) -> Result<ChangedSymbols, String> {
    let fleet = if range.is_none() {
        fleet.filter(|f| usable_fleet_base(runner, f.base))
    } else {
        None
    };
    let files = match (range, fleet) {
        (Some(r), _) => changed_files(runner, Some(r))?,
        // Inside a fleet iteration the base the worker started from is known
        // ($SIRIUS_BASE): the change is this issue's line of work since then
        // ([`fleet_line_files`]) plus anything still uncommitted. A worker
        // that did nothing (HEAD == base, clean tree) links nothing — never
        // the base branch's last commit.
        (None, Some(f)) => {
            let mut files = fleet_line_files(runner, f)?;
            for x in changed_files(runner, None)? {
                if !files.contains(&x) {
                    files.push(x);
                }
            }
            if files.is_empty() {
                // New files only, not yet committed: say so rather than
                // silently linking nothing (they are not stamped either — a
                // non-isolated run can't tell them from scratch files).
                refuse_if_untracked_pending(runner)?;
            }
            files
        }
        (None, None) => solo_changed_files(runner)?,
    };
    if files.is_empty() {
        return Ok(ChangedSymbols::default());
    }
    let (ok, parsed, detail) = hv.affected_tests_changed(&files);
    let v = match parsed {
        Some(v) if ok => v,
        _ => {
            return Err(format!(
                "hayven could not map the changed files to entities: {}",
                detail.lines().next().unwrap_or("no output")
            ))
        }
    };
    let roots = v
        .get("roots")
        .and_then(Value::as_array)
        .ok_or_else(|| "hayven affected-tests returned no \"roots\" array".to_string())?;
    let mut symbols: Vec<String> = Vec::new();
    for r in roots {
        let id = r
            .as_str()
            .or_else(|| r.get("id").and_then(Value::as_str))
            .map(String::from);
        if let Some(id) = id {
            if !symbols.contains(&id) {
                symbols.push(id);
            }
        }
    }
    Ok(ChangedSymbols { files, symbols })
}

/// A fleet iteration's starting point, from its env: `$SIRIUS_BASE` and, when
/// held work from an earlier run was merged in, `$SIRIUS_RESUMED_FROM`.
#[derive(Debug, Clone, Copy)]
pub struct FleetBase<'a> {
    pub base: &'a str,
    pub resumed_from: Option<&'a str>,
    /// `$SIRIUS_ISSUE`: whose held/wip/branch refs mark this issue's own work.
    pub issue: Option<&'a str>,
    /// `$SIRIUS_BASE_REF`: the branch the fleet lands on. Commits already on
    /// it are never this issue's to stamp.
    pub base_ref: Option<&'a str>,
}

/// How many resume merges [`fleet_line_files`] follows in all. Each is one
/// earlier hold or preserve of the same issue (a claim can stack two: held,
/// then wip); more than this is pathological.
const MAX_RESUME_MERGES: usize = 16;

/// The files this issue's own commits touched since the fleet base.
///
/// The worker's line is the FIRST-PARENT walk from HEAD: it starts detached at
/// the base, and a fix round merges the CURRENT base into it — so a plain
/// `base..HEAD` (tree diff or log) would also stamp every other worker's
/// commit that merge brought in. `--no-merges` alone does not help: it drops
/// the merge commits, not the commits they reach. Commits already on the
/// fleet's base branch (`$SIRIUS_BASE_REF`) are excluded outright, which also
/// covers a worker that brought the base in by fast-forward or rebase — no
/// merge commit there to recognise.
///
/// Held or wip work resumed from an earlier hold is the second parent that IS
/// this issue's: `resume_work` merges it before the agent runs — the held
/// commit, then the preserved wip on top when the wip does not contain it, so
/// a line can carry STACKED resume merges, and an agent can merge its
/// `$SIRIUS_PRIOR_WORK` mid-line. EVERY first-parent merge on the line is
/// therefore checked, and followed (its second parent's own line walked the
/// same way) when sirius recorded that parent as this issue's work
/// ([`is_resume_parent`]: named by `$SIRIUS_RESUMED_FROM`, the tip of one of
/// this issue's own refs — `sirius/<issue>`, `refs/sirius/<kind>/<issue>[/…]`
/// — or inside one and in no other issue's ref) — ancestry, so neither a
/// commit-message hook nor a human's branch on the held sha can hide it. A
/// worker's own merge of the base or of a sibling is not followed, even
/// when a hold of this issue contains that merge. Followed lines still lose
/// landed and foreign commits.
fn fleet_line_files(runner: &dyn Runner, f: FleetBase<'_>) -> Result<Vec<String>, String> {
    let landed: Vec<String> = match f.base_ref.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => match run_git(
            runner,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{r}^{{commit}}"),
            ],
        ) {
            Ok(o) if !o.stdout.trim().is_empty() => {
                vec!["--not".to_string(), o.stdout.trim().to_string()]
            }
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    let (own_refs, other_refs) = issue_refs(runner, f.issue)?;
    // What this iteration resumed is this issue's work by definition, held
    // by whatever ref: never foreign.
    let mut own_tips = own_refs.clone();
    if let Some(r) = f.resumed_from.map(str::trim).filter(|r| !r.is_empty()) {
        // Only a real commit: a bad value must not fail the whole link.
        let spec = format!("{r}^{{commit}}");
        // Push the RESOLVED sha, never the raw input: `^HEAD` passes the
        // check, and after `--not` would turn this issue's line foreign.
        if let Ok(o) = run_git(runner, &["rev-parse", "--verify", "--quiet", &spec]) {
            let sha = o.stdout.trim();
            if !sha.is_empty() && !sha.starts_with('^') {
                own_tips.push(sha.to_string());
            }
        }
    }
    let foreign = if other_refs.is_empty() {
        std::collections::HashSet::new()
    } else {
        foreign_commits(runner, f.base, &own_tips, &landed)?
    };
    let named = f.resumed_from.map(str::trim).filter(|r| !r.is_empty());
    let mut files: Vec<String> = Vec::new();
    // Lines still to walk (HEAD's, then every resume merge's second parent),
    // and every tip already queued — a sha reachable by two resume merges is
    // walked once.
    let mut queue: std::collections::VecDeque<String> = ["HEAD".to_string()].into();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut followed = 0usize;
    let mut capped = false;
    let own_set: std::collections::HashSet<&str> = own_refs.iter().map(String::as_str).collect();
    let other_set: std::collections::HashSet<&str> =
        other_refs.iter().map(String::as_str).collect();
    let landed_tip = landed.get(1).map(String::as_str);
    while let Some(tip) = queue.pop_front() {
        let range = format!("{}..{tip}", f.base);
        let mut log_args: Vec<&str> = vec![
            "log",
            "--first-parent",
            "--no-merges",
            "--name-only",
            // NUL opens each commit's record: a path can never contain it
            // (`@types/x.d.ts` would pass for an `@<sha>` header).
            "--format=%x00%H",
            // A user's `log.showSignature` would add lines read as paths.
            "--no-show-signature",
            &range,
        ];
        log_args.extend(landed.iter().map(String::as_str));
        // A foreign commit's file list is skipped.
        let mut skipping = false;
        for x in stdout_lines(&run_git(runner, &log_args)?) {
            if let Some(sha) = x.strip_prefix('\0') {
                skipping = foreign.contains(sha);
            } else if !skipping && !files.contains(&x) {
                files.push(x);
            }
        }
        // Every merge on this line (oldest first, so the bottom resume is
        // followed before anything stacked above it): `<sha> <p1> <p2> …`.
        let mut line_args: Vec<&str> = vec![
            "rev-list",
            "--first-parent",
            "--parents",
            "--reverse",
            &range,
        ];
        line_args.extend(landed.iter().map(String::as_str));
        for l in stdout_lines(&run_git(runner, &line_args)?) {
            for second in l.split_whitespace().skip(2) {
                if seen.contains(second) {
                    continue;
                }
                if followed >= MAX_RESUME_MERGES {
                    if !capped {
                        capped = true;
                        eprintln!(
                            "sirius: link: more than {MAX_RESUME_MERGES} resume merges on this line — the older ones are not followed"
                        );
                    }
                    continue;
                }
                if is_resume_parent(runner, second, named, &own_set, &other_set, landed_tip)? {
                    seen.insert(second.to_string());
                    followed += 1;
                    queue.push_back(second.to_string());
                }
            }
        }
    }
    Ok(files)
}

/// Is `parent` (a non-first parent of a merge on this issue's line) work
/// this issue resumed? Sirius recorded it as such when it is:
/// - the commit `$SIRIUS_RESUMED_FROM` names, or
/// - the TIP of one of this issue's own refs (held, wip, wip-failed — what
///   `resume_work` merges, or `$SIRIUS_PRIOR_WORK` an agent merged), or
/// - inside one of this issue's own refs (an older hold under a newer one)
///   AND inside no other issue's ref AND not already on the base branch.
///
/// Containment alone is not enough: an own ref (a hold, a parked attempt)
/// can contain a merge of a SIBLING's branch or of the base, whose parent it
/// then also contains — following those would stamp other work as ours.
/// One `for-each-ref --contains` per candidate answers all three.
fn is_resume_parent(
    runner: &dyn Runner,
    parent: &str,
    named: Option<&str>,
    own: &std::collections::HashSet<&str>,
    other: &std::collections::HashSet<&str>,
    landed_tip: Option<&str>,
) -> Result<bool, String> {
    if named.is_some_and(|r| parent.starts_with(r) || r.starts_with(parent)) {
        return Ok(true);
    }
    if own.is_empty() {
        return Ok(false);
    }
    let out = run_git(
        runner,
        &[
            "for-each-ref",
            "--contains",
            parent,
            "--format=%(objectname) %(refname)",
            "refs/heads/sirius/",
            "refs/sirius/",
        ],
    )?;
    let (mut own_tip, mut in_own, mut in_other) = (false, false, false);
    for l in stdout_lines(&out) {
        let Some((sha, r)) = l.split_once(' ') else {
            continue;
        };
        if own.contains(r) {
            in_own = true;
            own_tip |= sha == parent;
        } else if other.contains(r) {
            in_other = true;
        }
    }
    if own_tip {
        return Ok(true);
    }
    if !in_own || in_other {
        return Ok(false);
    }
    Ok(!landed_tip
        .is_some_and(|t| run_git(runner, &["merge-base", "--is-ancestor", parent, t]).is_ok()))
}

/// Issue-scoped sirius refs, split into this issue's own and every other
/// issue's: `refs/heads/sirius/<key>` and `refs/sirius/<kind>/<key>[/…]` (held,
/// held-superseded/…, wip, …) — the key is always the 4th path segment, matched
/// whole and case-insensitively, so `amt-1` never claims `amt-10`'s refs.
/// Refs that name no issue (e.g. `refs/sirius/frontier`) are in neither list.
fn issue_refs(
    runner: &dyn Runner,
    issue: Option<&str>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let key = issue
        .map(|i| i.trim().to_lowercase())
        .filter(|i| !i.is_empty());
    let out = run_git(
        runner,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/sirius/",
            "refs/sirius/",
        ],
    )?;
    let (mut own, mut others) = (Vec::new(), Vec::new());
    for r in stdout_lines(&out) {
        let Some(seg) = r.split('/').nth(3) else {
            continue;
        };
        if key.as_deref().is_some_and(|k| seg.eq_ignore_ascii_case(k)) {
            own.push(r);
        } else {
            others.push(r);
        }
    }
    Ok((own, others))
}

/// Commits only OTHER issues' refs hold: reachable from another issue's
/// branch/held/wip ref but from none of this issue's, and not already at or
/// below the base. A worker that fast-forwards onto a sibling's `sirius/<x>`
/// takes those commits into its own first-parent line with no merge commit
/// to recognise — they are the sibling's work, not this issue's. Commits this
/// issue's refs also hold (a sibling that merged our earlier hold) stay ours.
///
/// The other issues' refs are named by GLOB, never listed: completed
/// `sirius/<issue>` branches are kept forever, and one argument per ref would
/// outgrow Windows' 32 KiB command line after ~1,200 issues. This issue's own
/// refs match the globs too — harmless, they are subtracted by `--not`. Both
/// glob depths are given because whether `*` crosses `/` varies by git version.
fn foreign_commits(
    runner: &dyn Runner,
    base: &str,
    own_tips: &[String],
    landed: &[String],
) -> Result<std::collections::HashSet<String>, String> {
    let mut args: Vec<&str> = vec![
        "rev-list",
        "--glob=refs/heads/sirius/*",
        "--glob=refs/sirius/*/*",
        "--glob=refs/sirius/*/*/*",
        "--not",
        base,
    ];
    args.extend(own_tips.iter().map(String::as_str));
    // `landed` is `["--not", <base-branch tip>]` (or empty): already negated.
    args.extend(landed.iter().skip(1).map(String::as_str));
    Ok(stdout_lines(&run_git(runner, &args)?).into_iter().collect())
}

/// Whether `base` names a commit HEAD descends from. Anything else — a stale
/// or foreign SHA left exported in a human's shell — is ignored rather than
/// trusted (exit 1 "not an ancestor" and 128 "bad revision" alike).
fn usable_fleet_base(runner: &dyn Runner, base: &str) -> bool {
    let base = base.trim();
    !base.is_empty() && run_git(runner, &["merge-base", "--is-ancestor", base, "HEAD"]).is_ok()
}

/// Untracked paths the suite's own tools write into a repo (event spine,
/// ledgers, indexes) — never the caller's work.
const SUITE_DIRS: [&str; 4] = [".suite/", ".sirius/", ".ametrite/", ".hayven/"];

/// SF-12, solo (no `--range`, no fleet base): "working tree vs HEAD". The
/// documented worker order (work → gate → COMMIT → receipt) leaves that empty,
/// and `sirius link --changed` used to no-op with "no symbols to link" — the
/// receipt silently never got filed. So a clean tree means the commit just
/// made: diff HEAD~1..HEAD.
///
/// "Clean" must count untracked files: `git diff HEAD` never shows them, so
/// new, uncommitted files would otherwise send us to the PREVIOUS commit —
/// someone else's work — and stamp its symbols on this issue. Refuse instead
/// (suite-owned dirs excepted: `.suite/events` is written on every run).
fn solo_changed_files(runner: &dyn Runner) -> Result<Vec<String>, String> {
    let files = changed_files(runner, None)?;
    if !files.is_empty() {
        return Ok(files);
    }
    refuse_if_untracked_pending(runner)?;
    // A repo whose HEAD is the root commit has no HEAD~1; that is not an error
    // here, there is simply nothing earlier to diff against. `--verify --quiet`
    // exits non-zero in that case, which run_git turns into an Err we
    // deliberately swallow.
    match run_git(runner, &["rev-parse", "--verify", "--quiet", "HEAD~1"]) {
        Ok(prev) if !prev.stdout.trim().is_empty() => changed_files(runner, Some("HEAD~1..HEAD")),
        _ => Ok(Vec::new()),
    }
}

/// Err when untracked files (suite-owned dirs excepted) are pending: the work
/// exists but is not committed, so no commit range can stand in for it.
fn refuse_if_untracked_pending(runner: &dyn Runner) -> Result<(), String> {
    let untracked: Vec<String> = untracked_files(runner)?
        .into_iter()
        .filter(|f| !SUITE_DIRS.iter().any(|d| f.starts_with(d)))
        .collect();
    if let Some(first) = untracked.first() {
        return Err(format!(
            "no tracked changes against HEAD, but {} untracked file(s) (e.g. `{first}`) — \
             commit (or `git add`) the work first, or pass --range; refusing to guess \
             which commit is yours",
            untracked.len()
        ));
    }
    Ok(())
}

/// Pull entity ids out of a `hayven query` result (`{"hits":[{"id":..}]}`).
pub fn extract_ids(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let arr = v
        .get("hits")
        .or_else(|| v.get("results"))
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    for item in arr {
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            out.push(id.to_string());
        } else if let Some(id) = item.as_str() {
            out.push(id.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};

    /// The SF-16 regression: a verbatim path reaches git as `//?/C:/...` and
    /// it cannot create the leading directories, so no worker worktree is ever
    /// built and the whole fleet is dead on Windows.
    #[test]
    fn git_path_strips_the_windows_verbatim_prefix() {
        assert_eq!(
            git_path(std::path::Path::new(
                r"\\?\C:\repo\.sirius\worktrees\sirius-oak"
            )),
            r"C:\repo\.sirius\worktrees\sirius-oak"
        );
    }

    /// `\\?\UNC\server\share` is the verbatim spelling of `\\server\share`;
    /// stripping only the prefix would leave the bogus path `UNC\server\share`.
    #[test]
    fn git_path_rewrites_verbatim_unc_back_to_a_real_unc_path() {
        assert_eq!(
            git_path(std::path::Path::new(r"\\?\UNC\server\share\repo")),
            r"\\server\share\repo"
        );
    }

    #[test]
    fn git_path_leaves_ordinary_paths_alone() {
        assert_eq!(git_path(std::path::Path::new(r"C:\repo\wt")), r"C:\repo\wt");
        assert_eq!(
            git_path(std::path::Path::new("/home/u/repo/wt")),
            "/home/u/repo/wt"
        );
        // A real UNC path is already what git wants — do not touch it.
        assert_eq!(
            git_path(std::path::Path::new(r"\\server\share")),
            r"\\server\share"
        );
    }

    #[test]
    fn changed_files_splits_lines() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "src/a.rs\nsrc/b.rs\n\n");
        let files = changed_files(&m, None).unwrap();
        assert_eq!(files, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(m.recorded()[0], "git diff --name-only HEAD");
    }

    #[test]
    fn changed_files_uses_range() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "x.rs\n");
        changed_files(&m, Some("main..HEAD")).unwrap();
        assert_eq!(m.recorded()[0], "git diff --name-only main..HEAD");
    }

    // SIRF-11: the gate's changed-file view must include committed work and
    // untracked files — `git diff HEAD` alone reads both as "nothing changed".
    #[test]
    fn changed_since_sees_committed_and_untracked_work() {
        let m = MockRunner::new();
        // Diff vs the pre-work baseline picks up the agent's COMMITTED change…
        m.expect(&["git", "diff", "--name-only", "base123"], 0, "src/a.rs\n");
        // …and ls-files adds the never-`git add`ed NEW file (dup deduped;
        // TODO.txt existed before the work, so it is NOT the agent's change).
        m.expect(
            &["git", "ls-files"],
            0,
            "src/new_test.rs\nsrc/a.rs\nTODO.txt\n",
        );
        let pre = vec!["TODO.txt".to_string()];
        let files = changed_since(&m, Some("base123"), &pre).unwrap();
        assert_eq!(files, vec!["src/a.rs", "src/new_test.rs"]);
        assert_eq!(m.recorded()[0], "git diff --name-only base123");
        assert_eq!(m.recorded()[1], "git ls-files --others --exclude-standard");
    }

    #[test]
    fn changed_since_propagates_git_errors() {
        // A git failure must be an ERROR the caller can fail closed on — not
        // an empty list (the old fold that skipped the gate).
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["git", "diff"],
            128,
            "",
            "fatal: not a git repository",
        ));
        assert!(changed_since(&m, None, &[]).unwrap_err().contains("fatal"));
    }

    #[test]
    fn head_rev_requires_real_output() {
        // Empty stdout (e.g. a mock's benign default, or a repo with no HEAD)
        // must be an error, never an empty baseline string.
        let m = MockRunner::new();
        m.expect(&["git", "rev-parse"], 0, "\n");
        assert!(head_rev(&m).is_err());
        m.expect(&["git", "rev-parse"], 0, "base123\n");
        assert_eq!(head_rev(&m).unwrap(), "base123");
    }

    #[test]
    fn extract_ids_from_hits() {
        let v = serde_json::json!({"hits":[{"id":"a::f"},{"id":"b::g"}]});
        assert_eq!(extract_ids(&v), vec!["a::f", "b::g"]);
    }

    /// SF-12: committing before filing the receipt is the natural order, and it
    /// used to make `--changed` resolve nothing at all.
    #[test]
    fn changed_symbols_falls_back_to_the_last_commit_when_the_tree_is_clean() {
        let m = MockRunner::new();
        // Working tree vs HEAD: clean, because the agent committed.
        m.push(MockResponse::new(
            &["git", "diff", "--name-only", "HEAD"],
            0,
            "",
            "",
        ));
        m.push(MockResponse::new(
            &["git", "rev-parse", "--verify"],
            0,
            "abc123\n",
            "",
        ));
        m.push(MockResponse::new(
            &["git", "diff", "--name-only", "HEAD~1..HEAD"],
            0,
            "src/math.rs\n",
            "",
        ));
        // Path-exact resolution (SIRF-20): the committed file's own entities.
        m.push(MockResponse::new(
            &["hayven", "affected-tests"],
            0,
            r#"{"changed":["src/math.rs"],"roots":["src/math::add"],"tests":[]}"#,
            "",
        ));
        let hv = Hayven::new(&m);
        let got = changed_symbols(&m, &hv, None, None).unwrap();
        assert_eq!(got.files, vec!["src/math.rs"]);
        assert_eq!(got.symbols, vec!["src/math::add"]);
    }

    /// New, uncommitted files are work in progress, not a clean tree: the
    /// fallback must refuse rather than stamp the PREVIOUS commit's symbols.
    #[test]
    fn changed_symbols_refuses_to_guess_with_untracked_work() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "");
        m.expect(&["git", "ls-files", "--others"], 0, "src/new.rs\n");
        m.expect(&["git", "rev-parse", "--verify"], 0, "abc123\n");
        let hv = Hayven::new(&m);
        let e = changed_symbols(&m, &hv, None, None).unwrap_err();
        assert!(e.contains("src/new.rs"), "{e}");
        assert!(
            !m.recorded().iter().any(|c| c.contains("HEAD~1")),
            "{:?}",
            m.recorded()
        );
    }

    /// In a fleet iteration ($SIRIUS_BASE known) the change is the worker's own
    /// commits since its base (merges excluded) plus anything uncommitted —
    /// not just the last commit, and not a tree diff that would pull in other
    /// workers' files merged from the base.
    #[test]
    fn changed_symbols_uses_the_fleet_base_when_known() {
        let m = MockRunner::new();
        m.expect(&["git", "merge-base", "--is-ancestor"], 0, "");
        m.expect(
            &["git", "log", "--first-parent"],
            0,
            "src/a.rs\n\nsrc/b.rs\nsrc/a.rs\n",
        );
        m.expect(
            &["git", "diff", "--name-only", "HEAD"],
            0,
            "src/c.rs\nsrc/b.rs\n",
        );
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["src/a","src/b","src/c"],"tests":[]}"#,
        );
        let hv = Hayven::new(&m);
        let got = changed_symbols(
            &m,
            &hv,
            None,
            Some(FleetBase {
                base: "base1",
                resumed_from: None,
                issue: None,
                base_ref: None,
            }),
        )
        .unwrap();
        assert_eq!(got.files, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        let calls = m.recorded();
        assert!(
            calls.iter().any(|c| c
                == "git log --first-parent --no-merges --name-only --format=%x00%H --no-show-signature base1..HEAD"),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| c.contains("HEAD~1")), "{calls:?}");
    }

    /// Fleet worker with only new, uncommitted files: refuse loudly instead of
    /// silently linking nothing.
    #[test]
    fn changed_symbols_fleet_refuses_with_only_untracked_work() {
        let m = MockRunner::new();
        m.expect(&["git", "merge-base", "--is-ancestor"], 0, "");
        m.expect(&["git", "log"], 0, "");
        m.expect(&["git", "diff"], 0, "");
        m.expect(&["git", "ls-files", "--others"], 0, "src/brand_new.rs\n");
        let hv = Hayven::new(&m);
        let fleet = FleetBase {
            base: "base1",
            resumed_from: None,
            issue: None,
            base_ref: None,
        };
        let e = changed_symbols(&m, &hv, None, Some(fleet)).unwrap_err();
        assert!(e.contains("src/brand_new.rs"), "{e}");
    }

    /// A stale or foreign $SIRIUS_BASE (not an ancestor of HEAD) is ignored:
    /// the solo rule applies, instead of stamping everything since that SHA.
    #[test]
    fn changed_symbols_ignores_a_base_head_does_not_descend_from() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["git", "merge-base"], 1, "", ""));
        m.expect(&["git", "diff"], 0, "src/a.rs\n");
        m.expect(&["hayven", "affected-tests"], 0, r#"{"roots":["src/a"]}"#);
        let hv = Hayven::new(&m);
        let got = changed_symbols(
            &m,
            &hv,
            None,
            Some(FleetBase {
                base: "oldsha",
                resumed_from: None,
                issue: None,
                base_ref: None,
            }),
        )
        .unwrap();
        assert_eq!(got.files, vec!["src/a.rs"]);
        assert!(!m.recorded().iter().any(|c| c.starts_with("git log")));
    }

    /// The suite's own untracked output (`.suite/events/*.jsonl` is written on
    /// every run, and target repos rarely ignore it) is not pending work.
    #[test]
    fn changed_symbols_solo_ignores_suite_owned_untracked_files() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["git", "diff", "--name-only", "HEAD"],
            0,
            "",
            "",
        ));
        m.expect(
            &["git", "ls-files", "--others"],
            0,
            ".suite/events/2026-10-08.jsonl\n.hayven/x\n",
        );
        m.expect(&["git", "rev-parse", "--verify"], 0, "abc123\n");
        m.push(MockResponse::new(
            &["git", "diff", "--name-only", "HEAD~1..HEAD"],
            0,
            "src/math.rs\n",
            "",
        ));
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["src/math"]}"#,
        );
        let hv = Hayven::new(&m);
        let got = changed_symbols(&m, &hv, None, None).unwrap();
        assert_eq!(got.files, vec!["src/math.rs"]);
    }

    /// A worker that committed nothing (HEAD == base) links nothing — never
    /// the base branch's last commit, which is someone else's work.
    #[test]
    fn changed_symbols_with_no_fleet_commits_is_empty() {
        let m = MockRunner::new();
        m.expect(&["git", "merge-base", "--is-ancestor"], 0, "");
        m.expect(&["git", "log"], 0, "");
        m.expect(&["git", "diff"], 0, "");
        m.expect(&["git", "rev-parse", "--verify"], 0, "abc123\n");
        let hv = Hayven::new(&m);
        let got = changed_symbols(
            &m,
            &hv,
            None,
            Some(FleetBase {
                base: "base1",
                resumed_from: None,
                issue: None,
                base_ref: None,
            }),
        )
        .unwrap();
        assert_eq!(got, ChangedSymbols::default());
        assert!(!m.recorded().iter().any(|c| c.contains("HEAD~1")));
    }

    /// The fallback must NOT override an explicit range: an empty answer to a
    /// range the caller named is the truth, not a footgun.
    #[test]
    fn changed_symbols_does_not_second_guess_an_explicit_range() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["git", "diff"], 0, "", ""));
        let hv = Hayven::new(&m);
        let got = changed_symbols(&m, &hv, Some("main..HEAD"), None).unwrap();
        assert_eq!(got, ChangedSymbols::default());
        assert_eq!(
            m.recorded().len(),
            1,
            "no fallback probe: {:?}",
            m.recorded()
        );
    }

    /// A root-commit repo has no HEAD~1; that must stay an empty answer, not an
    /// error.
    #[test]
    fn changed_symbols_survives_a_repo_with_no_parent_commit() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["git", "diff"], 0, "", ""));
        m.push(MockResponse::new(
            &["git", "rev-parse", "--verify"],
            1,
            "",
            "",
        ));
        let hv = Hayven::new(&m);
        let got = changed_symbols(&m, &hv, None, None).unwrap();
        assert_eq!(got, ChangedSymbols::default());
    }

    #[test]
    fn changed_symbols_resolves_by_path_not_basename() {
        // SIRF-20 regression: two same-named modules in different packages.
        // Only the changed one may be stamped — never b/types via basename.
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "a/types.ts\nbun.lock\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"changed":["a/types.ts","bun.lock"],"roots":["a/types","a/types/Foo","a/types"],"tests":[]}"#,
        );
        // If anything still searched by basename, this would leak b/types in.
        m.expect(
            &["hayven", "query"],
            0,
            r#"{"hits":[{"id":"a/types"},{"id":"b/types"}]}"#,
        );
        let hv = Hayven::new(&m);
        let got = changed_symbols(&m, &hv, Some("x~1..x"), None).unwrap();
        assert_eq!(got.files, vec!["a/types.ts", "bun.lock"]);
        assert_eq!(
            got.symbols,
            vec!["a/types", "a/types/Foo"],
            "deduped, path-exact"
        );
        let calls = m.recorded();
        assert!(calls
            .iter()
            .any(|c| c == "hayven affected-tests --changed a/types.ts,bun.lock --json"));
        assert!(
            !calls.iter().any(|c| c.starts_with("hayven query")),
            "{calls:?}"
        );
    }

    #[test]
    fn changed_symbols_errors_when_hayven_cannot_answer() {
        // A daemon failure must not become a silently EMPTY (or partial) stamp.
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "src/a.rs\n");
        m.push(MockResponse::new(
            &["hayven", "affected-tests"],
            1,
            "",
            "daemon serves a DIFFERENT project",
        ));
        let hv = Hayven::new(&m);
        let e = changed_symbols(&m, &hv, None, None).unwrap_err();
        assert!(e.contains("DIFFERENT project"), "{e}");
        // An empty range is just empty — not an error, and no hayven call.
        let m2 = MockRunner::new();
        m2.expect(&["git", "diff"], 0, "");
        let hv2 = Hayven::new(&m2);
        assert_eq!(
            changed_symbols(&m2, &hv2, None, None).unwrap(),
            ChangedSymbols::default()
        );
        assert!(!m2.recorded().iter().any(|c| c.starts_with("hayven")));
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        assert_eq!(unquote_path(r#""caf\303\251.rs""#), "café.rs");
        assert_eq!(unquote_path(r#""a\"b\\c\td""#), "a\"b\\c\td");
        assert_eq!(unquote_path("src/plain.rs"), "src/plain.rs");
        assert_eq!(unquote_path("\""), "\"");
        assert_eq!(unquote_path("0123abcd"), "0123abcd");
    }

    // ---- fleet line walk, against real git --------------------------------

    struct Repo(crate::shell::RealRunner);

    impl Repo {
        fn new(tag: &str) -> Repo {
            let dir =
                std::env::temp_dir().join(format!("sirius-line-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let r = Repo(crate::shell::RealRunner { cwd: Some(dir) });
            r.git(&["init", "-q", "-b", "main"]);
            r
        }
        fn git(&self, args: &[&str]) -> String {
            let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
            full.extend(args);
            run_git(&self.0, &full)
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
                .stdout
                .trim()
                .to_string()
        }
        /// Commit one new file; returns the new HEAD.
        fn file(&self, name: &str) -> String {
            std::fs::write(self.0.cwd.as_ref().unwrap().join(name), name).unwrap();
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", name]);
            self.git(&["rev-parse", "HEAD"])
        }
        /// Another worker's issue landing on main as a --no-ff merge.
        fn land_on_main(&self, name: &str) -> String {
            let back = self.git(&["rev-parse", "HEAD"]);
            self.git(&["switch", "-q", "main"]);
            self.git(&["switch", "-q", "-c", name]);
            self.file(name);
            self.git(&["switch", "-q", "main"]);
            self.git(&["merge", "-q", "--no-ff", "--no-edit", name]);
            let tip = self.git(&["rev-parse", "HEAD"]);
            self.git(&["switch", "-q", "--detach", &back]);
            tip
        }
        fn line(&self, base: &str, resumed: Option<&str>) -> Vec<String> {
            self.line_with(base, resumed, Some("AMT-1"), Some("main"))
        }
        fn line_with(
            &self,
            base: &str,
            resumed: Option<&str>,
            issue: Option<&str>,
            base_ref: Option<&str>,
        ) -> Vec<String> {
            let mut got = fleet_line_files(
                &self.0,
                FleetBase {
                    base,
                    resumed_from: resumed,
                    issue,
                    base_ref,
                },
            )
            .unwrap();
            got.sort();
            got
        }
        fn drop(self) {
            let _ = std::fs::remove_dir_all(self.0.cwd.unwrap());
        }
    }

    /// A fix round merges the moved base in; same-run resume fast-forwards a
    /// held line that contains that merge. Neither may stamp other.txt.
    #[test]
    fn fleet_line_excludes_base_merged_in_by_a_fix_round_and_same_run_resume() {
        let r = Repo::new("samerun");
        let base = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &base]);
        r.file("mine.txt");
        let cur = r.land_on_main("other.txt");
        r.git(&["merge", "-q", "--no-edit", &cur]); // fix round
        r.file("mine2.txt");
        assert_eq!(r.line(&base, None), vec!["mine.txt", "mine2.txt"]);
        // Held, then resumed in the same run: worktree reset to base, merge
        // of the held sha fast-forwards.
        let held = r.git(&["rev-parse", "HEAD"]);
        r.git(&["switch", "-q", "--detach", &base]);
        r.git(&["merge", "-q", "--no-edit", &held]);
        r.file("mine3.txt");
        assert_eq!(
            r.line(&base, Some(&held)),
            vec!["mine.txt", "mine2.txt", "mine3.txt"]
        );
        r.drop();
    }

    /// Cross-run resume chain: each run's base already holds the earlier
    /// landed work, and the held line comes back as a non-ff resume merge.
    #[test]
    fn fleet_line_follows_a_resume_chain_across_runs() {
        let r = Repo::new("chain");
        let b1 = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &b1]);
        let h0 = r.file("h0.txt");
        r.git(&["switch", "-q", "main"]);
        r.file("landed1.txt");
        let b2 = r.git(&["rev-parse", "HEAD"]);
        r.git(&["switch", "-q", "--detach", &b2]);
        r.git(&["merge", "-q", "--no-edit", &h0]); // run 2 resumes h0
        let h1 = r.file("w.txt");
        r.git(&["switch", "-q", "main"]);
        r.file("landed2.txt");
        let b3 = r.git(&["rev-parse", "HEAD"]);
        r.git(&["switch", "-q", "--detach", &b3]);
        r.git(&["merge", "-q", "--no-edit", &h1]); // run 3 resumes h1
        r.file("w2.txt");
        // Held work is on the issue's held ref until the completion stamp.
        r.git(&["update-ref", "refs/sirius/held/amt-1", &h1]);
        assert_eq!(r.line(&b3, Some(&h1)), vec!["h0.txt", "w.txt", "w2.txt"]);
        r.drop();
    }

    /// This run's resume FAST-FORWARDS (main has not moved since the last
    /// hold), so the line's bottom is the OLDER cross-run resume merge — its
    /// second parent is h0, not $SIRIUS_RESUMED_FROM. h0's work still counts.
    #[test]
    fn fleet_line_follows_an_older_resume_under_a_fast_forward_resume() {
        let r = Repo::new("ffresume");
        let b1 = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &b1]);
        let h0 = r.file("h0.txt");
        r.git(&["switch", "-q", "main"]);
        r.file("landed1.txt");
        let b2 = r.git(&["rev-parse", "HEAD"]);
        r.git(&["switch", "-q", "--detach", &b2]);
        r.git(&["merge", "-q", "--no-edit", &h0]); // run 2 resumes h0 (non-ff)
        let h1 = r.file("w.txt");
        // Run 3, main unmoved at b2: resuming h1 fast-forwards.
        r.git(&["switch", "-q", "--detach", &b2]);
        r.git(&["merge", "-q", "--no-edit", &h1]);
        assert_eq!(
            r.git(&["rev-parse", "HEAD"]),
            h1,
            "the resume fast-forwarded"
        );
        r.file("w2.txt");
        r.git(&["update-ref", "refs/sirius/held/amt-1", &h1]);
        assert_eq!(r.line(&b2, Some(&h1)), vec!["h0.txt", "w.txt", "w2.txt"]);
        // A commit-message hook can rewrite resume_held's merge subject, and a
        // human can branch the held sha to inspect it: recognition is by the
        // issue's own refs, so neither hides h0's work.
        r.git(&["branch", "inspect-x", &h0]);
        assert_eq!(r.line(&b2, Some(&h1)), vec!["h0.txt", "w.txt", "w2.txt"]);
        r.drop();
    }

    /// The same opening merge done BY SHA gets resume_held's exact subject
    /// (`Merge commit '<sha>'`). With main advanced linearly, following it
    /// would stamp main's own commits — it must be told apart by having
    /// landed on a branch.
    #[test]
    fn fleet_line_does_not_follow_an_opening_merge_of_the_base_by_sha() {
        let r = Repo::new("bysha");
        let base = r.file("base.txt");
        r.git(&["switch", "-q", "main"]);
        let main_tip = r.file("other.txt"); // linear: no merge commit on main
        r.git(&["switch", "-q", "--detach", &base]);
        r.file("mine0.txt");
        r.git(&["reset", "-q", "--hard", &base]);
        r.git(&["merge", "-q", "--no-ff", "--no-edit", &main_tip]);
        r.file("mine.txt");
        assert_eq!(r.line(&base, None), vec!["mine.txt"]);
        r.drop();
    }

    /// A resume merge whose subject a commit-msg hook rewrote is still a
    /// resume: it is recognised by the issue's held ref, not by its message.
    #[test]
    fn fleet_line_follows_a_resume_merge_with_a_rewritten_subject() {
        let r = Repo::new("hooked");
        let b1 = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &b1]);
        let h0 = r.file("h0.txt");
        r.git(&["switch", "-q", "main"]);
        r.file("landed1.txt");
        let b2 = r.git(&["rev-parse", "HEAD"]);
        r.git(&["switch", "-q", "--detach", &b2]);
        r.git(&["merge", "-q", "--no-edit", "-m", "[AMT-1] resumed", &h0]);
        let h1 = r.file("w.txt");
        r.git(&["switch", "-q", "--detach", &b2]);
        r.git(&["merge", "-q", "--no-edit", &h1]); // fast-forward
        r.file("w2.txt");
        r.git(&["update-ref", "refs/sirius/held/amt-1", &h1]);
        assert_eq!(r.line(&b2, Some(&h1)), vec!["h0.txt", "w.txt", "w2.txt"]);
        // Another issue's refs never vouch for it: `amt-10` is not `amt-1`.
        r.git(&["update-ref", "-d", "refs/sirius/held/amt-1"]);
        r.git(&["update-ref", "refs/sirius/held/amt-10", &h1]);
        assert_eq!(r.line(&b2, Some(&h1)), vec!["w.txt", "w2.txt"]);
        r.drop();
    }

    /// The base brought in by FAST-FORWARD (main moved linearly) leaves no
    /// merge commit to recognise: commits on the base branch are excluded
    /// outright, so main's own commits are never stamped.
    #[test]
    fn fleet_line_excludes_base_commits_brought_in_by_fast_forward() {
        let r = Repo::new("ffbase");
        let base = r.file("base.txt");
        r.git(&["switch", "-q", "main"]);
        let main_tip = r.file("other.txt");
        r.git(&["switch", "-q", "--detach", &base]);
        r.git(&["merge", "-q", "--no-edit", &main_tip]); // fast-forward
        r.file("mine.txt");
        assert_eq!(r.line(&base, None), vec!["mine.txt"]);
        // Without a known base branch the commit cannot be told apart.
        assert_eq!(
            r.line_with(&base, None, Some("AMT-1"), None),
            vec!["mine.txt", "other.txt"]
        );
        r.drop();
    }

    /// Fast-forwarding onto a sibling's in-flight branch brings its commits
    /// into this line with no merge commit: they are the sibling's, excluded
    /// because only the sibling's refs hold them. Our own earlier hold that
    /// the sibling merged stays ours.
    #[test]
    fn fleet_line_excludes_a_siblings_commits_but_keeps_ours_it_merged() {
        let r = Repo::new("sibling");
        let base = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &base]);
        let h0 = r.file("h0.txt"); // this issue's earlier hold
        r.git(&["update-ref", "refs/sirius/held/amt-1", &h0]);
        r.git(&["switch", "-q", "--detach", &base]);
        r.git(&["merge", "-q", "--no-edit", &h0]); // sibling builds on our hold (ff)
        let sib = r.file("sib.txt");
        r.git(&["branch", "sirius/amt-2", &sib]);
        // This issue resumes h0 and then fast-forwards onto the sibling.
        r.git(&["switch", "-q", "--detach", &base]);
        r.git(&["merge", "-q", "--no-edit", &h0]);
        r.git(&["merge", "-q", "--no-edit", "sirius/amt-2"]);
        r.file("mine.txt");
        assert_eq!(r.line(&base, Some(&h0)), vec!["h0.txt", "mine.txt"]);
        r.drop();
    }

    /// Paths that begin with `@` (`@types/`, `@scope/` — common in TS repos)
    /// are paths, never commit headers: ours are kept, a sibling's skipped.
    #[test]
    fn fleet_line_handles_paths_starting_with_at() {
        let r = Repo::new("atpaths");
        let base = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &base]);
        std::fs::create_dir_all(r.0.cwd.as_ref().unwrap().join("@types")).unwrap();
        r.file("@types/sib.d.ts");
        let sib = r.file("zsib.rs");
        r.git(&["branch", "sirius/amt-2", &sib]);
        r.git(&["switch", "-q", "--detach", &base]);
        r.git(&["merge", "-q", "--no-edit", "sirius/amt-2"]); // fast-forward
        std::fs::create_dir_all(r.0.cwd.as_ref().unwrap().join("@scope")).unwrap();
        r.file("@scope/mine.ts");
        assert_eq!(r.line(&base, None), vec!["@scope/mine.ts"]);
        r.drop();
    }

    /// A worker that OPENS with its own `git merge main` leaves a merge at the
    /// bottom of its line too — that is not a resume and must not be walked.
    #[test]
    fn fleet_line_does_not_follow_a_workers_own_opening_merge() {
        let r = Repo::new("opening");
        let base = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &base]);
        let main_tip = r.land_on_main("other.txt");
        r.git(&["merge", "-q", "--no-ff", "--no-edit", &main_tip]);
        r.file("mine.txt");
        assert_eq!(r.line(&base, None), vec!["mine.txt"]);
        r.drop();
    }

    /// A claim that resumes held work AND a preserved wip that does not
    /// contain it stacks two resume merges; the wip merge sits mid-line,
    /// above the held one. Both lines are this issue's work.
    #[test]
    fn fleet_line_follows_stacked_held_and_wip_resume_merges() {
        let r = Repo::new("stacked");
        let b1 = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &b1]);
        let held = r.file("h.txt");
        r.git(&["update-ref", "refs/sirius/held/amt-1", &held]);
        r.git(&["switch", "-q", "--detach", &b1]);
        let wip = r.file("w.txt");
        r.git(&["update-ref", "refs/sirius/wip/amt-1", &wip]);
        r.git(&["switch", "-q", "main"]);
        let b2 = r.file("landed.txt");
        r.git(&["switch", "-q", "--detach", &b2]);
        r.git(&["merge", "-q", "--no-edit", &held]);
        r.git(&["merge", "-q", "--no-edit", &wip]);
        r.file("mine.txt");
        assert_eq!(r.line(&b2, Some(&wip)), vec!["h.txt", "mine.txt", "w.txt"]);
        // Recognised by the issue's refs alone, too (no RESUMED_FROM).
        assert_eq!(r.line(&b2, None), vec!["h.txt", "mine.txt", "w.txt"]);
        r.drop();
    }

    /// An agent that merges its `$SIRIUS_PRIOR_WORK` (wip-failed) mid-line
    /// gets that work stamped; a sibling merged the same way does not.
    #[test]
    fn fleet_line_follows_prior_work_merged_mid_line_but_not_a_sibling() {
        let r = Repo::new("midprior");
        let b1 = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &b1]);
        let prior = r.file("p.txt");
        r.git(&["update-ref", "refs/sirius/wip-failed/amt-1", &prior]);
        r.git(&["switch", "-q", "--detach", &b1]);
        let sib = r.file("sib.txt");
        r.git(&["branch", "sirius/amt-2", &sib]);
        r.git(&["switch", "-q", "--detach", &b1]);
        r.file("mine.txt");
        r.git(&["merge", "-q", "--no-edit", &prior]);
        r.git(&["merge", "-q", "--no-edit", "sirius/amt-2"]);
        r.file("mine2.txt");
        assert_eq!(r.line(&b1, None), vec!["mine.txt", "mine2.txt", "p.txt"]);
        r.drop();
    }

    /// A hold of this issue that CONTAINS a merge of a sibling's branch does
    /// not make that sibling's commits ours when the hold is resumed: the
    /// sibling's parent is inside another issue's ref.
    #[test]
    fn fleet_line_does_not_follow_a_sibling_merge_inside_a_resumed_hold() {
        let r = Repo::new("heldsib");
        let b1 = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &b1]);
        let sib = r.file("sib.txt");
        r.git(&["branch", "sirius/amt-2", &sib]);
        r.git(&["switch", "-q", "--detach", &b1]);
        r.file("mine1.txt");
        r.git(&["merge", "-q", "--no-edit", "sirius/amt-2"]);
        let held = r.file("mine1b.txt");
        r.git(&["update-ref", "refs/sirius/held/amt-1", &held]);
        r.git(&["switch", "-q", "main"]);
        let b2 = r.file("landed.txt");
        r.git(&["switch", "-q", "--detach", &b2]);
        r.git(&["merge", "-q", "--no-edit", &held]);
        r.file("mine2.txt");
        let want = vec!["mine1.txt", "mine1b.txt", "mine2.txt"];
        assert_eq!(r.line(&b2, Some(&held)), want);
        assert_eq!(r.line_with(&b2, Some(&held), Some("AMT-1"), None), want);
        r.drop();
    }

    /// A parked (discarded) attempt that merged a sibling makes the next
    /// attempt's merge of the same sibling no more ours.
    #[test]
    fn fleet_line_does_not_follow_a_sibling_a_parked_attempt_also_merged() {
        let r = Repo::new("parkedsib");
        let b1 = r.file("base.txt");
        r.git(&["switch", "-q", "--detach", &b1]);
        let sib = r.file("sib.txt");
        r.git(&["branch", "sirius/amt-2", &sib]);
        r.git(&["switch", "-q", "--detach", &b1]);
        r.git(&["merge", "-q", "--no-ff", "--no-edit", "sirius/amt-2"]);
        let parked = r.file("try1.txt");
        r.git(&["update-ref", "refs/sirius/wip-superseded/amt-1/x", &parked]);
        r.git(&["switch", "-q", "--detach", &b1]);
        r.file("mine.txt");
        r.git(&["merge", "-q", "--no-edit", "sirius/amt-2"]);
        assert_eq!(r.line(&b1, None), vec!["mine.txt"]);
        r.drop();
    }
}
