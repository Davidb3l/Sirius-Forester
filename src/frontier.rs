//! The integration frontier (SIRF-30) and sequence collisions (SIRF-31).
//!
//! Every defect the Lydgr fleet run's reviewers could not see existed only
//! BETWEEN branches: two migrations on the same parent, two features each
//! correct and inconsistent together. A per-branch reviewer cannot see a
//! sibling it has never been shown. So Sirius reviews each change AS IT WILL
//! LAND: merged onto the *frontier* — the current base tip plus every
//! in-flight sibling branch awaiting integration (a speculative merge queue,
//! as Zuul/Bors do it). Cross-branch clashes become mechanical facts:
//! a merge conflict naming the sibling, or a sequence slot two branches claim.
//!
//! `run.rs` owns the processes; this module owns the git and the decisions.

use crate::config::SequenceSpec;
use crate::gitrange::run_git;
use crate::review::{fnv1a, Finding};
use crate::shell::Runner;
use regex::Regex;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// At most this many siblings (the oldest — next in line to merge) are
/// merged into a REVIEW frontier. Sequence checks and `sirius integrate`
/// always use every sibling.
pub const MAX_SIBLINGS: usize = 12;

/// Another issue's completed branch awaiting integration.
#[derive(Debug, Clone, PartialEq)]
pub struct Sibling {
    pub issue: String,
    pub title: String,
    pub branch: String,
    pub tip: String,
    /// Files it changes relative to the current base (`cur...tip`).
    pub files: Vec<String>,
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Every issue awaiting integration (`target_status`), key → title — ONE
/// `amt issue list`, not an `issue show` per branch: completed branches are
/// never deleted, so a per-branch scan grows without bound, and a branch
/// whose issue was deleted is simply not awaiting anything.
pub fn awaiting(
    amt: &crate::amt::Amt,
    target_status: &str,
) -> Result<HashMap<String, String>, String> {
    let rows = amt.issue_list_status(target_status)?;
    Ok(rows
        .iter()
        .filter_map(|v| {
            let id = v["id"].as_str()?;
            Some((
                id.to_uppercase(),
                v["title"].as_str().unwrap_or_default().to_string(),
            ))
        })
        .collect())
}

/// The siblings of `own_issue`: local `sirius/*` branches not merged into
/// `cur` whose issue is in `awaiting` (this also drops squash-merged or
/// abandoned work, whose branches stay "unmerged" forever), oldest
/// completion first. `awaiting` is `Err` when the board could not be read.
///
/// `complete` is `false` only on a TRANSIENT failure (the board or git could
/// not answer): facts built on it must not be read as "the collision went
/// away" (fail closed) — and it heals on the next round.
pub fn siblings(
    runner: &dyn Runner,
    awaiting: &Result<HashMap<String, String>, String>,
    cur: &str,
    own_issue: &str,
) -> Discovery {
    let awaiting = match awaiting {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sirius: cannot read the board ({e}) — reviewing without siblings");
            return Discovery::default();
        }
    };
    let own = format!("sirius/{}", own_issue.to_lowercase());
    let out = match run_git(
        runner,
        &[
            "for-each-ref",
            "--sort=committerdate",
            &format!("--no-merged={cur}"),
            "--format=%(refname:short)%09%(objectname)",
            "refs/heads/sirius/",
        ],
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("sirius: cannot list sibling branches ({e}) — reviewing without them");
            return Discovery::default();
        }
    };
    let mut sibs = Vec::new();
    for line in lines(&out.stdout) {
        let Some((branch, tip)) = line.split_once('\t') else {
            continue;
        };
        if branch == own {
            continue;
        }
        let Some(key) = branch.strip_prefix("sirius/") else {
            continue;
        };
        let issue = key.to_uppercase();
        let Some(title) = awaiting.get(&issue) else {
            continue;
        };
        let files = run_git(runner, &["diff", "--name-only", &format!("{cur}...{tip}")])
            .map(|o| lines(&o.stdout))
            .unwrap_or_default();
        sibs.push(Sibling {
            issue,
            title: title.clone(),
            branch: branch.to_string(),
            tip: tip.to_string(),
            files,
        });
    }
    Discovery {
        sibs,
        complete: true,
    }
}

/// What [`siblings`] found.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub sibs: Vec<Sibling>,
    pub complete: bool,
}

enum Merge {
    Clean,
    Conflicts(Vec<String>),
    Failed(String),
}

/// The `git -c` settings for a THROWAWAY merge (never on any branch): a
/// fixed identity, no repo hooks (a commitlint `commit-msg` hook rejecting
/// the message would fail every review; a husky `post-merge` would run
/// installs in the review tree), and no rerere recording.
pub const THROWAWAY_MERGE_CONFIG: [&str; 8] = [
    "-c",
    "user.name=sirius",
    "-c",
    "user.email=sirius@localhost",
    "-c",
    "core.hooksPath=/nonexistent/sirius-no-hooks",
    "-c",
    "rerere.enabled=false",
];

/// Merge `rev` into the tree at `t` as a throwaway merge commit. A
/// conflicted merge is aborted.
fn merge_into(runner: &dyn Runner, t: &str, rev: &str, msg: &str) -> Merge {
    let mut args = vec!["-C", t];
    args.extend(THROWAWAY_MERGE_CONFIG);
    // No `--no-verify` (git >= 2.24 only): the null hooksPath above
    // already disables every hook.
    args.extend(["merge", "--no-ff", "--no-edit", "-m", msg, rev]);
    let merged = run_git(runner, &args);
    let Err(e) = merged else {
        return Merge::Clean;
    };
    let conflicts = run_git(runner, &["-C", t, "diff", "--name-only", "--diff-filter=U"])
        .map(|o| lines(&o.stdout))
        .unwrap_or_default();
    let _ = run_git(runner, &["-C", t, "merge", "--abort"]);
    if conflicts.is_empty() {
        Merge::Failed(e)
    } else {
        Merge::Conflicts(conflicts)
    }
}

/// How preparing the frontier review tree ended.
pub enum Prepared {
    /// `t`'s HEAD is the work merged onto `frontier`; review `frontier..HEAD`.
    Ready {
        frontier: String,
        merged: Vec<Sibling>,
        /// Siblings that conflict with the ones merged before them.
        left_out: Vec<(Sibling, Vec<String>)>,
        sibling_conflicts: Vec<Finding>,
    },
    /// The work conflicts with the base itself in these files.
    BaseConflict {
        files: Vec<String>,
        sibling_conflicts: Vec<Finding>,
    },
    Failed(String),
}

/// A conflict between this work and an in-flight sibling.
fn sibling_conflict(s: &Sibling, files: &[String]) -> Finding {
    Finding {
        id: format!("AUTO-sib-{}", s.issue),
        kind: "sibling-conflict".into(),
        confidence: "confirmed".into(),
        file: files.first().cloned(),
        line: None,
        summary: format!(
            "conflicts with in-flight {} (\"{}\", {}) in {}",
            s.issue,
            s.title,
            s.branch,
            files.join(", ")
        ),
        scenario: format!(
            "{} is awaiting integration and changes the same lines; whichever of the two merges second hits this conflict",
            s.issue
        ),
        fix: format!(
            "coordinate with {}: check both changes still agree, and resolve the conflict when the first one lands",
            s.issue
        ),
        response: None,
    }
}

/// A frontier built in a throwaway tree.
pub struct Built {
    /// The frontier commit (the tree's HEAD).
    pub tip: String,
    /// Indexes into the siblings, in merge order.
    pub merged: Vec<usize>,
    /// Siblings that conflict with the ones merged before them.
    pub left_out: Vec<(Sibling, Vec<String>)>,
}

/// Reset the throwaway tree `t` to `cur` and merge every sibling not in
/// `excluded`, in order; one that conflicts with the frontier so far is left
/// out. Shared by the frontier review and `sirius integrate` (SIRF-32).
pub fn build(
    runner: &dyn Runner,
    t: &str,
    cur: &str,
    sibs: &[Sibling],
    excluded: &HashSet<usize>,
) -> Result<Built, String> {
    for args in [&["reset", "-q", "--hard", cur][..], &["clean", "-fdq"][..]] {
        let full: Vec<&str> = ["-C", t].iter().chain(args).copied().collect();
        run_git(runner, &full).map_err(|e| format!("cannot reset the frontier tree: {e}"))?;
    }
    let mut merged = Vec::new();
    let mut left_out = Vec::new();
    for (i, s) in sibs.iter().enumerate() {
        if excluded.contains(&i) {
            continue;
        }
        match merge_into(
            runner,
            t,
            &s.tip,
            &format!("sirius frontier: + {}", s.issue),
        ) {
            Merge::Clean => merged.push(i),
            Merge::Conflicts(f) => left_out.push((s.clone(), f)),
            Merge::Failed(e) => {
                return Err(format!(
                    "merging {} into the frontier failed: {e}",
                    s.branch
                ))
            }
        }
    }
    let tip = match run_git(runner, &["-C", t, "rev-parse", "HEAD"]) {
        Ok(o) if !o.stdout.trim().is_empty() => o.stdout.trim().to_string(),
        Ok(_) => return Err("the frontier tree has no HEAD".into()),
        Err(e) => return Err(e),
    };
    Ok(Built {
        tip,
        merged,
        left_out,
    })
}

/// Build the frontier in the throwaway tree `t` (detached at `cur`) and merge
/// the work (`head`) onto it. A conflict on a file a merged sibling touched
/// is that sibling's: it is reported and the frontier rebuilt without it, so
/// the review still happens. A conflict no sibling explains is the base's.
pub fn prepare(runner: &dyn Runner, t: &str, cur: &str, head: &str, sibs: &[Sibling]) -> Prepared {
    let mut excluded: HashSet<usize> = HashSet::new();
    let mut sibling_conflicts = Vec::new();
    loop {
        let Built {
            tip: frontier,
            merged,
            left_out,
        } = match build(runner, t, cur, sibs, &excluded) {
            Ok(b) => b,
            Err(e) => return Prepared::Failed(e),
        };
        match merge_into(runner, t, head, "sirius: review merge onto the frontier") {
            Merge::Clean => {
                return Prepared::Ready {
                    frontier,
                    merged: merged.iter().map(|&i| sibs[i].clone()).collect(),
                    left_out,
                    sibling_conflicts,
                }
            }
            Merge::Failed(e) => {
                return Prepared::Failed(format!("merging onto the frontier failed: {e}"))
            }
            Merge::Conflicts(files) => {
                let suspects: Vec<usize> = merged
                    .iter()
                    .copied()
                    .filter(|&i| sibs[i].files.iter().any(|f| files.contains(f)))
                    .collect();
                if suspects.is_empty() {
                    return Prepared::BaseConflict {
                        files,
                        sibling_conflicts,
                    };
                }
                // Touching the same FILE is not causing the conflict: probe.
                // If the work conflicts with the bare base, it is the base's.
                match probe(runner, t, cur, None, head) {
                    Ok(Some(f)) => {
                        return Prepared::BaseConflict {
                            files: f,
                            sibling_conflicts,
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return Prepared::Failed(e),
                }
                // Otherwise blame each suspect that conflicts with the work on
                // its own; if only their combination does, blame them all.
                let mut blamed = Vec::new();
                for &i in &suspects {
                    match probe(runner, t, cur, Some(&sibs[i].tip), head) {
                        Ok(Some(f)) => blamed.push((i, f)),
                        Ok(None) => {}
                        Err(e) => return Prepared::Failed(e),
                    }
                }
                if blamed.is_empty() {
                    blamed = suspects
                        .iter()
                        .map(|&i| {
                            let mine = files
                                .iter()
                                .filter(|f| sibs[i].files.contains(f))
                                .cloned()
                                .collect();
                            (i, mine)
                        })
                        .collect();
                }
                for (i, f) in blamed {
                    sibling_conflicts.push(sibling_conflict(&sibs[i], &f));
                    excluded.insert(i);
                }
            }
        }
    }
}

/// Does `head` conflict when merged onto `cur` (+ `sibling`, if given)?
/// `Some(files)` if so. A sibling that does not merge cleanly onto the bare
/// base is not the culprit here (`None`). Leaves `t` in a throwaway state;
/// `build` resets it.
fn probe(
    runner: &dyn Runner,
    t: &str,
    cur: &str,
    sibling: Option<&str>,
    head: &str,
) -> Result<Option<Vec<String>>, String> {
    for args in [&["reset", "-q", "--hard", cur][..], &["clean", "-fdq"][..]] {
        let full: Vec<&str> = ["-C", t].iter().chain(args).copied().collect();
        run_git(runner, &full).map_err(|e| format!("cannot reset the frontier tree: {e}"))?;
    }
    if let Some(sib) = sibling {
        match merge_into(runner, t, sib, "sirius: conflict probe") {
            Merge::Clean => {}
            Merge::Conflicts(_) => return Ok(None),
            Merge::Failed(e) => return Err(e),
        }
    }
    match merge_into(runner, t, head, "sirius: conflict probe") {
        Merge::Clean => Ok(None),
        Merge::Conflicts(f) => Ok(Some(f)),
        Merge::Failed(e) => Err(e),
    }
}

/// `$SIRIUS_SIBLINGS`: what else is in flight, and where it overlaps `ours`.
pub fn render_siblings(
    merged: &[Sibling],
    left_out: &[(Sibling, Vec<String>)],
    conflicted: &[Finding],
    ours: &[String],
) -> String {
    const SHOW: usize = 8;
    let line = |s: &Sibling, note: &str| {
        let mut l = format!("- {} \"{}\" ({}{note}): ", s.issue, s.title, s.branch);
        let shown: Vec<&str> = s.files.iter().take(SHOW).map(String::as_str).collect();
        l.push_str(&shown.join(", "));
        if s.files.len() > SHOW {
            l.push_str(&format!(", … ({} files)", s.files.len()));
        }
        let overlap: Vec<&str> = s
            .files
            .iter()
            .filter(|f| ours.contains(f))
            .map(String::as_str)
            .collect();
        if !overlap.is_empty() {
            l.push_str(&format!(" — OVERLAPS this diff: {}", overlap.join(", ")));
        }
        l
    };
    let mut out: Vec<String> = merged.iter().map(|s| line(s, "")).collect();
    for (s, _) in left_out {
        out.push(line(
            s,
            "; NOT in the review tree — conflicts with earlier siblings",
        ));
    }
    for f in conflicted {
        out.push(format!(
            "- {} — NOT in the review tree: {}",
            f.id.trim_start_matches("AUTO-sib-"),
            f.summary
        ));
    }
    if out.is_empty() {
        "(none)".into()
    } else {
        out.join("\n")
    }
}

// ---- SIRF-31: sequence collisions -----------------------------------------

/// `dir` as git prints paths: no leading `./`, no trailing `/`.
fn norm_dir(dir: &str) -> &str {
    dir.trim_start_matches("./").trim_end_matches('/')
}

/// One sequence entry: its name and its content (blob/tree id) — two
/// entries with the SAME name but different content still collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub oid: String,
}

/// The direct children of `dir` at `rev`. A missing dir is empty; a git
/// failure is an error (never "no entries"). `-z` keeps names unquoted.
pub fn seq_entries(runner: &dyn Runner, rev: &str, dir: &str) -> Result<Vec<Entry>, String> {
    let prefix = format!("{}/", norm_dir(dir));
    let out = run_git(runner, &["ls-tree", "-z", rev, "--", &prefix])?;
    Ok(out
        .stdout
        .split('\0')
        .filter_map(|rec| {
            // "<mode> <type> <oid>\t<path>"
            let (meta, path) = rec.split_once('\t')?;
            let oid = meta.split_whitespace().nth(2)?;
            let name = path.strip_prefix(&prefix)?;
            (!name.is_empty()).then(|| Entry {
                name: name.to_string(),
                oid: oid.to_string(),
            })
        })
        .collect())
}

fn cmp_keys(a: &str, b: &str) -> Ordering {
    match (a.parse::<u128>(), b.parse::<u128>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

fn keyed<'e>(re: &Regex, entries: impl IntoIterator<Item = &'e Entry>) -> Vec<(&'e Entry, String)> {
    entries
        .into_iter()
        .filter_map(|e| {
            re.captures(&e.name)
                .and_then(|c| c.get(1))
                .map(|k| (e, k.as_str().to_string()))
        })
        .collect()
}

/// The sequence entries one sibling contributes: `(issue, entries)`.
pub type SiblingEntries = (String, Vec<Entry>);

/// Collisions for one sequence. `launch` / `ours` / `current` are the dir's
/// entries at the launch base, the work, and the current base tip; each
/// sibling's are at its tip. An entry is OURS only if it is on neither the
/// launch base nor the current base (a base merged into the work brings the
/// base's entries along — those are not this branch's to number).
pub fn sequence_findings(
    spec: &SequenceSpec,
    launch: &[Entry],
    ours: &[Entry],
    current: &[Entry],
    sibs: &[SiblingEntries],
) -> Result<Vec<Finding>, String> {
    let re =
        Regex::new(&spec.key).map_err(|e| format!("review.sequences key `{}`: {e}", spec.key))?;
    if re.captures_len() < 2 {
        // No group 1 = no keys = a check that silently never fires.
        return Err(format!(
            "review.sequences key `{}` has no capture group — wrap the key part in ( )",
            spec.key
        ));
    }
    let dir = norm_dir(&spec.dir);
    // OURS by NAME: editing an entry that already exists (a typo fix, a
    // Diesel up.sql touch-up) is not taking a new slot.
    let named = |set: &[Entry], e: &Entry| set.iter().any(|x| x.name == e.name);
    let added = ours
        .iter()
        .filter(|e| !named(launch, e) && !named(current, e));
    let cur_keys = keyed(&re, current);
    let mut out = Vec::new();
    for (mine, k) in keyed(&re, added) {
        let path = format!("{dir}/{}", mine.name);
        // The base's highest entry at or after our slot.
        if let Some((c, _)) = cur_keys
            .iter()
            .filter(|(_, ck)| cmp_keys(ck, &k) != Ordering::Less)
            .max_by(|a, b| cmp_keys(&a.1, &b.1))
        {
            out.push(Finding {
                id: format!("AUTO-seq-{:08x}", fnv1a(&format!("{path}|base")) as u32),
                kind: "conflict".into(),
                confidence: "confirmed".into(),
                file: Some(path.clone()),
                line: None,
                summary: format!(
                    "`{path}` does not come after `{dir}/{}`, already on the base",
                    c.name
                ),
                scenario: format!(
                    "the base already has `{}` at or after slot {k}: this entry was generated on an older parent, and merged as is the sequence forks",
                    c.name
                ),
                fix: "merge the current base, delete this entry, and regenerate it on top".into(),
                response: None,
            });
        }
        for (issue, entries) in sibs {
            // The same entry (same name AND content — e.g. the sibling's
            // work merged in) is not a collision; a same-named rewrite is.
            let theirs = entries.iter().filter(|e| !named(current, e) && *e != mine);
            if let Some((s, _)) = keyed(&re, theirs)
                .into_iter()
                .find(|(_, sk)| cmp_keys(sk, &k) == Ordering::Equal)
            {
                out.push(Finding {
                    id: format!("AUTO-seq-{:08x}", fnv1a(&format!("{path}|{issue}")) as u32),
                    kind: "conflict".into(),
                    confidence: "confirmed".into(),
                    file: Some(path.clone()),
                    line: None,
                    summary: format!(
                        "`{path}` claims sequence slot {k}, which in-flight {issue} also claims (`{dir}/{}`)",
                        s.name
                    ),
                    scenario: format!(
                        "{issue} is awaiting integration with its own entry at slot {k}; once both merge, the sequence has two entries on one parent"
                    ),
                    fix: format!(
                        "only one branch can own slot {k}: regenerate this entry after {issue} lands (merge its work first if the generator needs its state)"
                    ),
                    response: None,
                });
            }
        }
    }
    Ok(out)
}

/// Every sequence collision for the work at `head`, and whether the check
/// was COMPLETE (a git or config error makes it `false`: the caller must not
/// read a missing fact as "resolved").
pub fn sequence_collisions(
    runner: &dyn Runner,
    specs: &[SequenceSpec],
    launch: &str,
    head: &str,
    cur: &str,
    sibs: &[Sibling],
) -> (Vec<Finding>, bool) {
    let mut out = Vec::new();
    let mut complete = true;
    for spec in specs {
        let entries = || -> Result<Vec<Finding>, String> {
            let mut sib_entries: Vec<SiblingEntries> = Vec::new();
            for s in sibs {
                sib_entries.push((s.issue.clone(), seq_entries(runner, &s.tip, &spec.dir)?));
            }
            sequence_findings(
                spec,
                &seq_entries(runner, launch, &spec.dir)?,
                &seq_entries(runner, head, &spec.dir)?,
                &seq_entries(runner, cur, &spec.dir)?,
                &sib_entries,
            )
        };
        match entries() {
            Ok(f) => out.extend(f),
            Err(e) => {
                eprintln!("sirius: sequence check on `{}` failed: {e}", spec.dir);
                complete = false;
            }
        }
    }
    (out, complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::RealRunner;

    fn v(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|s| Entry {
                name: s.to_string(),
                oid: "x".into(),
            })
            .collect()
    }

    fn spec(dir: &str) -> SequenceSpec {
        SequenceSpec {
            dir: dir.into(),
            key: r"^(\d+)".into(),
        }
    }

    #[test]
    fn a_new_entry_after_the_base_is_clean() {
        let f = sequence_findings(
            &spec("m"),
            &v(&["0001_a.sql", "meta"]),
            &v(&["0001_a.sql", "0002_b.sql", "meta"]),
            &v(&["0001_a.sql", "meta"]),
            &[],
        )
        .unwrap();
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn an_entry_on_an_old_parent_collides_with_the_base() {
        // The base moved on to 0002 while this branch generated its own 0002.
        let f = sequence_findings(
            &spec("m"),
            &v(&["0001_a.sql"]),
            &v(&["0001_a.sql", "0002_mine.sql"]),
            &v(&["0001_a.sql", "0002_theirs.sql", "0003_x.sql"]),
            &[],
        )
        .unwrap();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].kind, "conflict");
        assert!(
            f[0].summary.contains("m/0003_x.sql"),
            "names the base's highest: {}",
            f[0].summary
        );
        assert!(f[0].id.starts_with("AUTO-seq-"));
    }

    #[test]
    fn a_sibling_claiming_the_same_slot_collides() {
        let f = sequence_findings(
            &spec("m"),
            &v(&["0001_a.sql"]),
            &v(&["0001_a.sql", "0002_mine.sql"]),
            &v(&["0001_a.sql"]),
            &[
                ("SIRF-12".into(), v(&["0001_a.sql", "0002_theirs.sql"])),
                ("SIRF-13".into(), v(&["0001_a.sql", "0003_later.sql"])),
            ],
        )
        .unwrap();
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].summary.contains("SIRF-12"), "{}", f[0].summary);
        assert!(f[0].fix.contains("after SIRF-12 lands"), "{}", f[0].fix);
    }

    #[test]
    fn base_entries_brought_in_by_a_merge_are_not_ours() {
        // The worker merged the current base (0002, 0003) into its branch
        // and regenerated as 0004: nothing collides.
        let f = sequence_findings(
            &spec("m"),
            &v(&["0001_a.sql"]),
            &v(&["0001_a.sql", "0002_b.sql", "0003_c.sql", "0004_mine.sql"]),
            &v(&["0001_a.sql", "0002_b.sql", "0003_c.sql"]),
            &[],
        )
        .unwrap();
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_same_named_entry_with_different_content_collides() {
        // Review N10: deterministic generator names (`0002_add_x`) in two
        // parallel workers — same name, different migration.
        let mut theirs = v(&["0001_a", "0002_add_x"]);
        theirs[1].oid = "other".into();
        let f = sequence_findings(
            &spec("m"),
            &v(&["0001_a"]),
            &v(&["0001_a", "0002_add_x"]),
            &v(&["0001_a"]),
            &[("SIRF-12".into(), theirs)],
        )
        .unwrap();
        assert_eq!(f.len(), 1, "{f:?}");
        // The SAME entry (their work merged into ours) is not a collision.
        let same = sequence_findings(
            &spec("m"),
            &v(&["0001_a"]),
            &v(&["0001_a", "0002_add_x"]),
            &v(&["0001_a"]),
            &[("SIRF-12".into(), v(&["0001_a", "0002_add_x"]))],
        )
        .unwrap();
        assert!(same.is_empty(), "{same:?}");
    }

    #[test]
    fn editing_an_existing_entry_is_not_a_collision() {
        // Verification V2: a typo fix in 0001_a (new content, same name).
        let mut ours = v(&["0001_a", "0002_b"]);
        ours[0].oid = "edited".into();
        let f = sequence_findings(
            &spec("m"),
            &v(&["0001_a", "0002_b"]),
            &ours,
            &v(&["0001_a", "0002_b"]),
            &[],
        )
        .unwrap();
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn ids_are_stable_and_keys_compare_numerically() {
        let run = || {
            sequence_findings(
                &spec("m"),
                &v(&["9_a"]),
                &v(&["9_a", "10_mine"]),
                &v(&["9_a", "10_other"]),
                &[],
            )
            .unwrap()
        };
        let (a, b) = (run(), run());
        assert_eq!(a.len(), 1, "10 vs 10 collides (numeric, not lexical)");
        assert_eq!(a[0].id, b[0].id, "a fact keeps its id across rounds");
        // Lexically "10" < "9"; numerically 10 > 9 — so 10 after 9 is clean.
        let clean = sequence_findings(
            &spec("m"),
            &v(&["9_a"]),
            &v(&["9_a", "10_mine"]),
            &v(&["9_a"]),
            &[],
        )
        .unwrap();
        assert!(clean.is_empty(), "{clean:?}");
    }

    #[test]
    fn a_bad_key_regex_is_an_error_not_a_panic() {
        let bad = SequenceSpec {
            dir: "m".into(),
            key: "(".into(),
        };
        assert!(sequence_findings(&bad, &[], &[], &[], &[]).is_err());
    }

    // ---- real git ---------------------------------------------------------

    struct Repo {
        dir: std::path::PathBuf,
        r: RealRunner,
    }

    impl Repo {
        fn new(tag: &str) -> Repo {
            let dir =
                std::env::temp_dir().join(format!("sirius-frontier-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let r = RealRunner {
                cwd: Some(dir.clone()),
            };
            let repo = Repo { dir, r };
            repo.git(&["init", "-q", "-b", "main"]);
            // Byte-exact assertions: never let a CRLF checkout (Windows'
            // core.autocrlf=true) rewrite what the test wrote.
            repo.git(&["config", "core.autocrlf", "false"]);
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
            full.extend(args);
            run_git(&self.r, &full)
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
                .stdout
                .trim()
                .to_string()
        }
        fn write(&self, path: &str, body: &str) {
            let p = self.dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        fn commit(&self, msg: &str) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-qm", msg]);
            self.git(&["rev-parse", "HEAD"])
        }
        /// A branch off `from` with `files` written, committed.
        fn branch(&self, name: &str, from: &str, files: &[(&str, &str)]) -> String {
            self.git(&["checkout", "-q", "-b", name, from]);
            for (p, b) in files {
                self.write(p, b);
            }
            let tip = self.commit(name);
            self.git(&["checkout", "-q", "main"]);
            tip
        }
        fn tree(&self, at: &str) -> String {
            let t = self.dir.with_extension("review");
            let _ = std::fs::remove_dir_all(&t);
            let t_str = t.to_string_lossy().to_string();
            self.git(&["worktree", "add", "-q", "--detach", &t_str, at]);
            t_str
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.dir.with_extension("review"));
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// SIRF-1/2 await integration; SIRF-3 is done (squash-merged); a
    /// branch whose issue is gone (sirius/sirf-7) is simply not awaiting.
    fn awaiting_map() -> Result<HashMap<String, String>, String> {
        Ok([("SIRF-1", "one"), ("SIRF-2", "two"), ("SIRF-9", "ours")]
            .into_iter()
            .map(|(k, t)| (k.to_string(), t.to_string()))
            .collect())
    }

    #[test]
    fn siblings_are_unmerged_awaiting_integration_and_not_our_own() {
        let repo = Repo::new("sibs");
        repo.write("a.txt", "a\n");
        let base = repo.commit("base");
        repo.branch("sirius/sirf-1", &base, &[("one.txt", "1\n")]);
        repo.branch("sirius/sirf-2", &base, &[("two.txt", "2\n")]);
        repo.branch("sirius/sirf-3", &base, &[("three.txt", "3\n")]); // done (squashed)
        repo.branch("sirius/sirf-7", &base, &[("seven.txt", "7\n")]); // issue deleted
        repo.branch("sirius/sirf-9", &base, &[("nine.txt", "9\n")]); // our own
        let merged_in = repo.branch("sirius/sirf-4", &base, &[("four.txt", "4\n")]);
        repo.git(&["merge", "-q", "--no-ff", "--no-edit", &merged_in]); // already in main
        let cur = repo.git(&["rev-parse", "main"]);
        let d = siblings(&repo.r, &awaiting_map(), &cur, "SIRF-9");
        assert!(
            d.complete,
            "stale and orphaned branches never make it incomplete"
        );
        assert!(!siblings(&repo.r, &Err("busy".into()), &cur, "SIRF-9").complete);
        let s = d.sibs;
        let issues: Vec<&str> = s.iter().map(|s| s.issue.as_str()).collect();
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(issues.contains(&"SIRF-1") && issues.contains(&"SIRF-2"));
        let one = s.iter().find(|s| s.issue == "SIRF-1").unwrap();
        assert_eq!(one.files, vec!["one.txt"]);
        assert_eq!(one.branch, "sirius/sirf-1");
    }

    #[test]
    fn frontier_merges_siblings_and_reviews_only_our_change() {
        let repo = Repo::new("ready");
        repo.write("a.txt", "a\n");
        let base = repo.commit("base");
        let s1 = repo.branch("sirius/sirf-1", &base, &[("one.txt", "1\n")]);
        let head = repo.branch("work", &base, &[("mine.txt", "m\n")]);
        let sibs = vec![Sibling {
            issue: "SIRF-1".into(),
            title: "t".into(),
            branch: "sirius/sirf-1".into(),
            tip: s1,
            files: vec!["one.txt".into()],
        }];
        let t = repo.tree(&base);
        match prepare(&repo.r, &t, &base, &head, &sibs) {
            Prepared::Ready {
                frontier,
                merged,
                left_out,
                sibling_conflicts,
            } => {
                assert_eq!(merged.len(), 1);
                assert!(left_out.is_empty() && sibling_conflicts.is_empty());
                let tr = RealRunner {
                    cwd: Some(t.clone().into()),
                };
                let diff =
                    run_git(&tr, &["diff", "--name-only", &format!("{frontier}..HEAD")]).unwrap();
                assert_eq!(diff.stdout.trim(), "mine.txt", "only this issue's change");
                assert!(
                    std::path::Path::new(&t).join("one.txt").exists(),
                    "the sibling is in the tree"
                );
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn a_sibling_conflict_is_named_and_the_review_still_happens() {
        let repo = Repo::new("sibconf");
        repo.write("shared.txt", "x\n");
        let base = repo.commit("base");
        let s1 = repo.branch("sirius/sirf-1", &base, &[("shared.txt", "theirs\n")]);
        let s2 = repo.branch("sirius/sirf-2", &base, &[("other.txt", "o\n")]);
        let head = repo.branch("work", &base, &[("shared.txt", "mine\n")]);
        let sibs = vec![
            Sibling {
                issue: "SIRF-1".into(),
                title: "t1".into(),
                branch: "sirius/sirf-1".into(),
                tip: s1,
                files: vec!["shared.txt".into()],
            },
            Sibling {
                issue: "SIRF-2".into(),
                title: "t2".into(),
                branch: "sirius/sirf-2".into(),
                tip: s2,
                files: vec!["other.txt".into()],
            },
        ];
        let t = repo.tree(&base);
        match prepare(&repo.r, &t, &base, &head, &sibs) {
            Prepared::Ready {
                merged,
                sibling_conflicts,
                ..
            } => {
                assert_eq!(sibling_conflicts.len(), 1);
                let f = &sibling_conflicts[0];
                assert_eq!(f.kind, "sibling-conflict");
                assert_eq!(f.id, "AUTO-sib-SIRF-1");
                assert!(f.summary.contains("shared.txt"), "{}", f.summary);
                let names: Vec<&str> = merged.iter().map(|s| s.issue.as_str()).collect();
                assert_eq!(
                    names,
                    vec!["SIRF-2"],
                    "the conflicting sibling is dropped, the other kept"
                );
                let body =
                    std::fs::read_to_string(std::path::Path::new(&t).join("shared.txt")).unwrap();
                assert_eq!(
                    body, "mine\n",
                    "the review tree holds the work, cleanly merged"
                );
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn a_conflict_no_sibling_explains_is_the_bases() {
        let repo = Repo::new("baseconf");
        repo.write("f.txt", "x\n");
        let launch = repo.commit("base");
        let head = repo.branch("work", &launch, &[("f.txt", "mine\n")]);
        repo.write("f.txt", "base moved\n");
        let cur = repo.commit("moved");
        let t = repo.tree(&cur);
        match prepare(&repo.r, &t, &cur, &head, &[]) {
            Prepared::BaseConflict { files, .. } => assert_eq!(files, vec!["f.txt"]),
            _ => panic!("expected BaseConflict"),
        }
    }

    #[test]
    fn a_sibling_editing_another_part_of_the_file_is_not_blamed() {
        // Review F2: the base moved and conflicts with line 1; the sibling
        // only touched line 8 of the same file. Base conflict, no sibling.
        let repo = Repo::new("blame");
        let body = |l1: &str, l8: &str| format!("{l1}\n2\n3\n4\n5\n6\n7\n{l8}\n");
        repo.write("f.txt", &body("1", "8"));
        let launch = repo.commit("base");
        let head = repo.branch("work", &launch, &[("f.txt", &body("mine", "8"))]);
        let s1 = repo.branch("sirius/sirf-1", &launch, &[("f.txt", &body("1", "theirs"))]);
        repo.write("f.txt", &body("base moved", "8"));
        let cur = repo.commit("moved");
        let sibs = vec![Sibling {
            issue: "SIRF-1".into(),
            title: "t".into(),
            branch: "sirius/sirf-1".into(),
            tip: s1,
            files: vec!["f.txt".into()],
        }];
        let t = repo.tree(&cur);
        match prepare(&repo.r, &t, &cur, &head, &sibs) {
            Prepared::BaseConflict {
                files,
                sibling_conflicts,
            } => {
                assert_eq!(files, vec!["f.txt"]);
                assert!(sibling_conflicts.is_empty(), "{sibling_conflicts:?}");
            }
            _ => panic!("expected BaseConflict"),
        }
    }

    #[test]
    fn repo_hooks_never_fail_a_throwaway_merge() {
        // Review F5: a commitlint-style commit-msg hook rejecting
        // "sirius frontier: + X" must not fail every review.
        let repo = Repo::new("hooks");
        repo.write("a.txt", "a\n");
        let base = repo.commit("base");
        let s1 = repo.branch("sirius/sirf-1", &base, &[("one.txt", "1\n")]);
        let head = repo.branch("work", &base, &[("mine.txt", "m\n")]);
        for hook in ["commit-msg", "pre-merge-commit"] {
            let p = repo.dir.join(".git/hooks").join(hook);
            std::fs::write(&p, "#!/bin/sh\nexit 1\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let sibs = vec![Sibling {
            issue: "SIRF-1".into(),
            title: "t".into(),
            branch: "sirius/sirf-1".into(),
            tip: s1,
            files: vec!["one.txt".into()],
        }];
        let t = repo.tree(&base);
        assert!(matches!(
            prepare(&repo.r, &t, &base, &head, &sibs),
            Prepared::Ready { .. }
        ));
    }

    #[test]
    fn a_key_without_a_capture_group_is_an_error_not_a_silent_no_op() {
        let s = SequenceSpec {
            dir: "m".into(),
            key: r"^\d+".into(),
        };
        let e = sequence_findings(&s, &[], &[], &[], &[]).unwrap_err();
        assert!(e.contains("capture group"), "{e}");
    }

    #[test]
    fn sequence_collisions_read_the_dir_from_git() {
        let repo = Repo::new("seq");
        repo.write("db/m/0001_init.sql", "-- 1\n");
        repo.write("db/m/meta/_journal.json", "{}\n");
        let base = repo.commit("base");
        let s1 = repo.branch(
            "sirius/sirf-1",
            &base,
            &[("db/m/0002_theirs.sql", "-- t\n")],
        );
        let head = repo.branch("work", &base, &[("db/m/0002_mine.sql", "-- m\n")]);
        let sibs = vec![Sibling {
            issue: "SIRF-1".into(),
            title: "t".into(),
            branch: "sirius/sirf-1".into(),
            tip: s1,
            files: vec![],
        }];
        let (f, complete) =
            sequence_collisions(&repo.r, &[spec("./db/m/")], &base, &head, &base, &sibs);
        assert!(complete);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].summary.contains("SIRF-1") && f[0].summary.contains("0002_theirs.sql"));
        assert_eq!(f[0].file.as_deref(), Some("db/m/0002_mine.sql"));
    }
}
