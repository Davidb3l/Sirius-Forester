//! `sirius review-canary` (SIRF-35): measure reviewer recall on REAL misses.
//!
//! "The review came back clean" is only worth something if the reviewer
//! catches bugs. Every recorded escape with a fix commit is a known bug that
//! once got past review here: revert its fix onto the current base and the
//! bug is back, in this codebase, in this code style. Replaying those (plus
//! hand-written `.sirius/canaries/*.patch` mutations) through the configured
//! reviewer — with a review round's prompt and findings handling, in a
//! throwaway worktree — scores recall; one benign CONTROL change scores
//! false positives.
//!
//! A canary is BLIND on every channel Sirius controls: a neutral issue key,
//! worker name and commit message; a base commit with no history (so `git
//! log` shows no "fix the bug" message); and its prompt's escape patterns
//! leave out every escape sharing its kind or fix. A reviewer that goes out
//! of its way to search the repo's other refs or the ledger can still find
//! answers — recall is a measurement, not a sandbox.

use crate::config::Config;
use crate::gitrange::run_git;
use crate::ledger::{EscapeRow, Ledger};
use crate::review::Finding;
use crate::shell::{AgentRunOpts, Runner};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What one canary is.
#[derive(Debug, Clone)]
enum Source {
    /// An escape with a fix commit: the fix, reverted.
    Escape(EscapeRow),
    /// A hand-written mutation patch.
    Patch(PathBuf),
    /// The false-positive control: a patch file, or a built-in benign change.
    Control(Option<PathBuf>),
}

impl Source {
    fn label(&self) -> String {
        let name = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        match self {
            Source::Escape(e) => format!("escape:{}", e.id),
            Source::Patch(p) => format!("patch:{}", name(p)),
            Source::Control(Some(p)) => format!("control:{}", name(p)),
            Source::Control(None) => "control:built-in".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CanaryResult {
    pub source: String,
    pub issue: Option<String>,
    pub kind: Option<String>,
    /// `caught | missed | stale | error`; the control: `clean | flagged | stale | error`.
    pub result: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub ok: bool,
    pub base: String,
    pub model: Option<String>,
    /// Canaries that applied and were reviewed.
    pub total: usize,
    pub caught: usize,
    /// Canaries that no longer apply to the base (not scored).
    pub stale: usize,
    /// Canaries the reviewer failed on (crash, timeout, no JSON) — scored
    /// as MISSES: a reviewer that fails on hard canaries must not look better.
    pub errors: usize,
    /// `caught / (total + errors)`; `None` when nothing was scored.
    pub recall: Option<f64>,
    /// Confirmed blocking findings on the control; `None` if it did not run.
    pub false_positives: Option<usize>,
    pub canaries: Vec<CanaryResult>,
}

/// Test files are where a reverted fix's regression test lives — "the test
/// was removed" is a finding on a touched file, but it is not catching the
/// bug. Only non-test files count toward a catch.
fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    p.split('/')
        .any(|seg| matches!(seg, "test" | "tests" | "__tests__" | "spec" | "specs"))
        || file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.contains("_spec.")
}

/// Does a finding's file name one of the (non-test) files the canary changed?
fn names_touched(f: &Finding, touched: &[String], tree: &str) -> bool {
    let Some(file) = f.file.as_deref() else {
        return false;
    };
    let file = file
        .strip_prefix(tree)
        .unwrap_or(file)
        .trim_start_matches('/')
        .trim_start_matches("./");
    touched
        .iter()
        .filter(|t| !is_test_path(t))
        .any(|t| t == file || file.ends_with(&format!("/{t}")))
}

/// The canaries to replay: escapes with a fix (newest first, one per fix
/// commit), then the `.sirius/canaries/*.patch` files (sorted) — at most
/// `n` — and the control.
fn sources(ledger: &Ledger, sirius_dir: &Path, n: usize) -> (Vec<Source>, Source) {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Source> = Vec::new();
    for e in ledger.escapes(None).unwrap_or_default() {
        let Some(fix) = e.fix_commit.clone() else {
            continue;
        };
        if !seen.contains(&fix) {
            seen.push(fix);
            out.push(Source::Escape(e));
        }
    }
    let dir = sirius_dir.join("canaries");
    let mut patches: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "patch"))
                .collect()
        })
        .unwrap_or_default();
    patches.sort();
    let control = dir.join("control.patch");
    out.extend(
        patches
            .into_iter()
            .filter(|p| p != &control)
            .map(Source::Patch),
    );
    out.truncate(n);
    let control = Source::Control(control.exists().then_some(control));
    (out, control)
}

/// Remove the trees and files of canary runs whose process is gone (a run
/// killed mid-review would otherwise leave a reintroduced bug checked out
/// under `.sirius/worktrees/` forever).
fn sweep_dead_runs(runner: &dyn Runner, sirius_dir: &Path) {
    let dead = |name: &str| {
        name.strip_prefix("canary-")
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|pid| pid.parse::<i32>().ok())
            .is_some_and(|pid| pid as u32 != std::process::id() && !crate::libc_kill_probe(pid))
    };
    for sub in ["worktrees", "reviews", "logs"] {
        let Ok(rd) = std::fs::read_dir(sirius_dir.join(sub)) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !dead(&name) {
                continue;
            }
            let path = entry.path();
            if sub == "worktrees" {
                let _ = run_git(
                    runner,
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &crate::gitrange::git_path(&path),
                    ],
                );
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// A benign change for the built-in control: one trailing newline on the
/// first tracked text file a review would NOT skip — a real (if trivial)
/// code change, unlike a doc file that `skip_paths` would never review.
fn benign_change(runner: &dyn Runner, t: &str, skip_paths: &[String]) -> Option<String> {
    let files = run_git(runner, &["-C", t, "ls-files"]).ok()?;
    files
        .stdout
        .lines()
        .map(str::trim)
        .filter(|f| !f.is_empty() && !crate::review::all_skippable(&[f.to_string()], skip_paths))
        .find(|f| {
            std::fs::read(Path::new(t).join(f))
                .map(|b| !b.is_empty() && !b.iter().take(8192).any(|&c| c == 0))
                .unwrap_or(false)
        })
        .map(String::from)
}

/// Why a canary did not get reviewed.
enum Skip {
    Stale(String),
    Error(String),
}

/// Apply a canary in the tree at `t` and commit it (neutral message).
fn apply(runner: &dyn Runner, t: &str, src: &Source, cfg: &Config) -> Result<Vec<String>, Skip> {
    let git = |args: &[&str]| {
        let mut full = vec!["-C", t];
        full.extend(crate::frontier::THROWAWAY_MERGE_CONFIG);
        full.extend(args);
        run_git(runner, &full)
    };
    match src {
        Source::Escape(e) => {
            let fix = e.fix_commit.as_deref().unwrap_or_default();
            // A fix that landed as a merge reverts against its mainline.
            let parents = git(&["rev-list", "--parents", "-n", "1", fix])
                .map(|o| o.stdout.split_whitespace().count().saturating_sub(1))
                .map_err(|e| Skip::Error(format!("the fix commit is not readable: {e}")))?;
            let mut args = vec!["revert", "--no-commit"];
            if parents > 1 {
                args.extend(["-m", "1"]);
            }
            args.push(fix);
            if let Err(err) = git(&args) {
                let conflicted = git(&["diff", "--name-only", "--diff-filter=U"])
                    .map(|o| !o.stdout.trim().is_empty())
                    .unwrap_or(false);
                let _ = git(&["revert", "--abort"]);
                return Err(if conflicted {
                    Skip::Stale("the fix no longer reverts cleanly onto the current base".into())
                } else {
                    Skip::Error(format!("git revert failed: {err}"))
                });
            }
        }
        Source::Patch(p) | Source::Control(Some(p)) => {
            if let Err(err) = git(&["apply", "--index", &crate::gitrange::git_path(p)]) {
                return Err(Skip::Stale(format!("the patch no longer applies: {err}")));
            }
        }
        Source::Control(None) => {
            let f = benign_change(runner, t, &cfg.review.skip_paths)
                .ok_or_else(|| Skip::Stale("no reviewable text file for the control".into()))?;
            let path = Path::new(t).join(&f);
            let mut body = std::fs::read(&path).map_err(|e| Skip::Error(e.to_string()))?;
            body.push(b'\n');
            std::fs::write(&path, body).map_err(|e| Skip::Error(e.to_string()))?;
            git(&["add", &f]).map_err(Skip::Error)?;
        }
    }
    // Nothing staged = the change is already absent: nothing to find.
    if git(&["diff", "--cached", "--quiet"]).is_ok() {
        return Err(Skip::Stale("applies as a no-op on the current base".into()));
    }
    git(&["commit", "--no-verify", "-qm", "wip"]).map_err(Skip::Error)?;
    git(&["diff", "--name-only", "HEAD~1", "HEAD"])
        .map(|o| {
            o.stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .map_err(Skip::Error)
}

struct Ctx<'a> {
    runner: &'a dyn Runner,
    cfg: &'a Config,
    sirius_dir: &'a Path,
    prompt: &'a str,
    review_cmd: &'a str,
    model: Option<String>,
    /// A history-less commit with the base's tree: the canary's parent.
    base: String,
    escapes: Vec<EscapeRow>,
    automated: Vec<(String, String)>,
}

fn tree_of(sirius_dir: &Path) -> PathBuf {
    sirius_dir
        .join("worktrees")
        .join(format!("canary-{}", std::process::id()))
}

/// Replay one canary: its findings (reviewer AUTO- ids dropped, as a round
/// does) and the files it changed.
fn replay(cx: &Ctx, src: &Source, i: usize) -> Result<(Vec<Finding>, Vec<String>), Skip> {
    let t_path = tree_of(cx.sirius_dir);
    let t = crate::gitrange::git_path(&t_path);
    let _ = run_git(cx.runner, &["worktree", "remove", "--force", &t]);
    let _ = std::fs::remove_dir_all(&t_path);
    run_git(cx.runner, &["worktree", "add", "--detach", &t, &cx.base])
        .map_err(|e| Skip::Error(format!("cannot create the canary tree: {e}")))?;
    let stem = format!("canary-{}-{i}", std::process::id());
    let reviews = cx.sirius_dir.join("reviews");
    let out_path = reviews.join(format!("{stem}.json"));
    let prompt_path = reviews.join(format!("{stem}-prompt.md"));
    let log = cx.sirius_dir.join("logs").join(format!("{stem}.log"));
    let result = (|| {
        let touched = apply(cx.runner, &t, src, cx.cfg)?;
        // Blind: a neutral key (no board history to read), and none of the
        // escapes that share this canary's kind or fix.
        let issue = format!("CANARY-{}", i + 1);
        // Every kind recorded against this fix is the answer, not just the
        // record this canary was built from.
        let fix = match src {
            Source::Escape(e) => e.fix_commit.clone(),
            _ => None,
        };
        let kinds: Vec<&str> = cx
            .escapes
            .iter()
            .filter(|e| fix.is_some() && e.fix_commit == fix)
            .map(|e| e.kind.as_str())
            .collect();
        let skip = |e: &EscapeRow| {
            kinds.contains(&e.kind.as_str()) || (fix.is_some() && e.fix_commit == fix)
        };
        let _ = std::fs::create_dir_all(&reviews);
        let _ = std::fs::remove_file(&out_path);
        let kv = |k: &str, v: &str| (k.to_string(), v.to_string());
        let mut vars = vec![
            kv("SIRIUS_ISSUE", &issue),
            kv("SIRIUS_WORKER", "sirius/reviewer"),
            kv("AMT_AGENT", "sirius/reviewer"),
            kv("SIRIUS_WORKTREE", &t),
            kv("SIRIUS_BASE", &cx.base),
            kv("SIRIUS_PHASE", "review"),
            // SIRF-52: the same env a real review round gets.
            kv(crate::shell::BG_WAIT_CEILING_ENV, "0"),
            kv("SIRIUS_REVIEW_DIR", &t),
            kv("SIRIUS_DIFF_RANGE", &format!("{}..HEAD", cx.base)),
            kv("SIRIUS_ROUND", "1"),
            kv("SIRIUS_REVIEW_OUT", &out_path.to_string_lossy()),
            kv("SIRIUS_FRONTIER", ""),
            kv("SIRIUS_SIBLING_BRANCHES", ""),
            kv("SIRIUS_SIBLINGS", "(none)"),
            kv(
                "SIRIUS_ESCAPES",
                &crate::escape::patterns_section(
                    &cx.escapes,
                    &cx.automated,
                    &skip,
                    crate::escape::PROMPT_TOP,
                ),
            ),
        ];
        // The prompt a round would build (placeholders, then any section a
        // custom template lacks).
        let mut rendered = crate::review::render_prompt(
            cx.prompt,
            &[
                vars.clone(),
                vec![kv("SIRIUS_REVIEW_FINDINGS", "(none — this is round 1)")],
            ]
            .concat(),
        );
        let escapes_text = vars
            .iter()
            .find(|(k, _)| k == "SIRIUS_ESCAPES")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        crate::review::append_missing_section(
            &mut rendered,
            cx.prompt,
            "SIRIUS_ESCAPES",
            &escapes_text,
            crate::review::ESCAPES_HEADING,
        );
        std::fs::write(&prompt_path, rendered)
            .map_err(|e| Skip::Error(format!("cannot write the prompt: {e}")))?;
        vars.push(kv("SIRIUS_REVIEW_PROMPT", &prompt_path.to_string_lossy()));
        if let Some(m) = &cx.model {
            vars.push(kv("ANTHROPIC_MODEL", m));
            vars.push(kv("SIRIUS_REVIEW_MODEL", m));
        }
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(cx.cfg.review.timeout_secs.max(1)),
            heartbeat_interval: Duration::from_secs(60),
            log_path: Some(log.clone()),
            env: vars,
        };
        let cmd = format!(
            "cd \"$SIRIUS_REVIEW_DIR\" || exit 1; {}",
            crate::run::template_cmd(
                cx.review_cmd,
                &issue,
                "sirius/reviewer",
                cx.model.as_deref()
            )
        );
        let ran = cx
            .runner
            .run_agent("sh", &["-c", &cmd], &opts, &mut || {})
            .map_err(|e| Skip::Error(format!("reviewer did not run: {e}")))?;
        if ran.timed_out() {
            return Err(Skip::Error("reviewer timed out".into()));
        }
        let raw = crate::review::read_review_output(
            &out_path,
            Some(&log),
            ran.output().code == Some(0),
            1,
        )
        .ok_or_else(|| Skip::Error("reviewer produced no findings JSON".into()))?;
        let mut report = crate::review::parse_review(&raw, 1).map_err(Skip::Error)?;
        report.findings.retain(|f| !crate::review::is_auto(&f.id));
        Ok((report.findings, touched))
    })();
    let _ = run_git(cx.runner, &["worktree", "remove", "--force", &t]);
    let _ = std::fs::remove_dir_all(&t_path);
    // The prompt, output and log describe the answer: never leave them for
    // a later reviewer to find.
    for f in [&out_path, &prompt_path, &log] {
        let _ = std::fs::remove_file(f);
    }
    result
}

/// Replay up to `n` canaries and the control; record the run.
pub fn run(
    runner: &dyn Runner,
    ledger: &Ledger,
    cfg: &Config,
    sirius_dir: &Path,
    prompt: &str,
    n: usize,
) -> Result<Report, String> {
    let review_cmd = cfg
        .review
        .cmd
        .as_deref()
        .filter(|c| !c.trim().is_empty())
        .ok_or("review.cmd is not set — there is no reviewer to measure")?;
    let base_ref = match cfg.review.base_ref.clone() {
        Some(r) => r,
        None => run_git(runner, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|o| o.stdout.trim().to_string())
            .ok()
            .filter(|r| !r.is_empty() && r != "HEAD")
            .ok_or("no base_ref: set review.base_ref or run on a branch")?,
    };
    let cur = run_git(
        runner,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )
    .map(|o| o.stdout.trim().to_string())
    .map_err(|e| format!("base_ref `{base_ref}` does not resolve: {e}"))?;
    // The base's TREE as a parentless commit: no history to read the fix from.
    let mut args: Vec<&str> = crate::frontier::THROWAWAY_MERGE_CONFIG.to_vec();
    let tree = format!("{cur}^{{tree}}");
    args.extend(["commit-tree", tree.as_str(), "-m", "base"]);
    let base = run_git(runner, &args)
        .map(|o| o.stdout.trim().to_string())
        .map_err(|e| format!("cannot snapshot the base: {e}"))?;
    sweep_dead_runs(runner, sirius_dir);
    let model = crate::models::review_model(crate::models::active(&cfg.models, false));
    let cx = Ctx {
        runner,
        cfg,
        sirius_dir,
        prompt,
        review_cmd,
        model,
        base,
        escapes: ledger.escapes(None).unwrap_or_default(),
        automated: ledger.automated_kinds().unwrap_or_default(),
    };
    let (canaries, control) = sources(ledger, sirius_dir, n);
    let tree_prefix = tree_of(sirius_dir).to_string_lossy().to_string();
    let blocking = |f: &Finding| crate::review::is_blocking(f, &cfg.review.block_on);
    let mut results = Vec::new();
    let (mut total, mut caught, mut stale, mut errors) = (0, 0, 0, 0);
    for (i, src) in canaries.iter().enumerate() {
        let (issue, kind) = match src {
            Source::Escape(e) => (Some(e.issue.clone()), Some(e.kind.clone())),
            _ => (None, None),
        };
        let (result, detail) = match replay(&cx, src, i) {
            Ok((findings, touched)) => {
                total += 1;
                let hit = findings
                    .iter()
                    .find(|f| blocking(f) && names_touched(f, &touched, &tree_prefix));
                match hit {
                    Some(f) => {
                        caught += 1;
                        ("caught", f.summary.clone())
                    }
                    None => (
                        "missed",
                        format!(
                            "{} finding(s), none blocking on a changed non-test file ({})",
                            findings.len(),
                            touched.join(", ")
                        ),
                    ),
                }
            }
            Err(Skip::Stale(d)) => {
                stale += 1;
                ("stale", d)
            }
            Err(Skip::Error(d)) => {
                errors += 1;
                ("error", d)
            }
        };
        results.push(CanaryResult {
            source: src.label(),
            issue,
            kind,
            result: result.into(),
            detail,
        });
    }
    let (false_positives, result, detail) = match replay(&cx, &control, canaries.len()) {
        Ok((findings, _)) => {
            let fp = findings.iter().filter(|f| blocking(f)).count();
            (
                Some(fp),
                if fp == 0 { "clean" } else { "flagged" },
                format!("{fp} confirmed blocking finding(s) on a benign change"),
            )
        }
        Err(Skip::Stale(d)) => (None, "stale", d),
        Err(Skip::Error(d)) => (None, "error", d),
    };
    results.push(CanaryResult {
        source: control.label(),
        issue: None,
        kind: None,
        result: result.into(),
        detail,
    });
    let scored = total + errors;
    let report = Report {
        ok: true,
        base: cur,
        model: cx.model.clone(),
        total,
        caught,
        stale,
        errors,
        recall: (scored > 0).then(|| caught as f64 / scored as f64),
        false_positives,
        canaries: results,
    };
    ledger
        .insert_canary_run(
            report.model.as_deref(),
            scored,
            caught,
            report.false_positives.unwrap_or(0),
            &json!({"canaries": report.canaries, "errors": errors, "stale": stale,
                    "control_ran": report.false_positives.is_some()}),
        )
        .map_err(|e| format!("cannot record the canary run: {e}"))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::RealRunner;

    struct Repo {
        dir: PathBuf,
        r: RealRunner,
    }

    impl Repo {
        fn new(tag: &str) -> Repo {
            let dir =
                std::env::temp_dir().join(format!("sirius-canary-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".sirius/captures")).unwrap();
            let repo = Repo {
                r: RealRunner {
                    cwd: Some(dir.clone()),
                },
                dir,
            };
            repo.git(&["init", "-q", "-b", "main"]);
            // Byte-exact assertions: never let a CRLF checkout (Windows'
            // core.autocrlf=true) rewrite what the test wrote.
            repo.git(&["config", "core.autocrlf", "false"]);
            std::fs::write(repo.dir.join(".gitignore"), ".sirius/\n").unwrap();
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
        fn commit(&self, path: &str, body: &str, msg: &str) -> String {
            std::fs::write(self.dir.join(path), body).unwrap();
            self.git(&["add", "-A"]);
            self.git(&["commit", "-qm", msg]);
            self.git(&["rev-parse", "HEAD"])
        }
        fn sirius(&self) -> PathBuf {
            self.dir.join(".sirius")
        }
        fn captured(&self, issue: &str) -> String {
            std::fs::read_to_string(self.sirius().join(format!("captures/{issue}.md")))
                .unwrap_or_default()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A path as `sh` must see it: on Windows `\\` is a separator `sh -c`
    /// would eat, so forward slashes; on unix a `\\` is a legal filename
    /// character and stays (callers quote the path).
    fn sh_path(p: &Path) -> String {
        let s = p.display().to_string();
        if cfg!(windows) {
            s.replace('\\', "/")
        } else {
            s
        }
    }

    /// A stand-in reviewer: it records what it was shown (prompt + git log),
    /// and with a STRONG prompt it reports an added `UNSAFE` line.
    fn reviewer(repo: &Repo) -> String {
        let cap = repo.sirius().join("captures");
        let script = repo.sirius().join("reviewer.sh");
        std::fs::write(
            &script,
            format!(
                r#"cp "$SIRIUS_REVIEW_PROMPT" "{cap}/$SIRIUS_ISSUE.md"
git log --oneline >> "{cap}/$SIRIUS_ISSUE.md"
if grep -q STRONG "$SIRIUS_REVIEW_PROMPT"; then
  f=$(git diff --name-only $SIRIUS_DIFF_RANGE | while read p; do git diff $SIRIUS_DIFF_RANGE -- "$p" | grep -q '^+.*UNSAFE' && echo "$p"; done | head -1)
  if [ -n "$f" ]; then
    echo "{{\"findings\":[{{\"id\":\"R1-1\",\"kind\":\"bug\",\"confidence\":\"confirmed\",\"file\":\"$f\",\"line\":1,\"summary\":\"unsafe call\",\"scenario\":\"x\",\"fix\":\"y\"}}]}}"
    exit 0
  fi
fi
echo '{{"findings":[]}}'
"#,
                cap = sh_path(&cap)
            ),
        )
        .unwrap();
        format!("sh '{}'", sh_path(&script))
    }

    fn cfg(cmd: String) -> Config {
        let mut c = Config::default();
        c.review.cmd = Some(cmd);
        c.review.base_ref = Some("main".into());
        c
    }

    #[test]
    fn real_misses_score_recall_and_a_weaker_prompt_scores_lower() {
        let repo = Repo::new("recall");
        repo.commit("pay.txt", "safe()\n", "base");
        repo.commit("pay.txt", "UNSAFE()\n", "feature (shipped the bug)");
        let fix = repo.commit("pay.txt", "safe()\n", "fix the escaped bug");
        // A fix whose code moved on since: its revert conflicts (stale).
        repo.commit("cfg.txt", "v1\n", "cfg v1");
        let moved = repo.commit("cfg.txt", "v2\n", "cfg fix");
        repo.commit("cfg.txt", "v3\n", "cfg moved on");
        repo.commit("other.txt", "fine\n", "other");
        std::fs::create_dir_all(repo.sirius().join("canaries")).unwrap();
        std::fs::write(
            repo.sirius().join("canaries/swallow.patch"),
            "--- a/other.txt\n+++ b/other.txt\n@@ -1 +1 @@\n-fine\n+UNSAFE swallow()\n",
        )
        .unwrap();
        let led = Ledger::open_in_memory().unwrap();
        led.insert_escape(
            "LYD-4",
            "unsafe-call",
            "UNSAFE call",
            Some("e2e"),
            Some(&fix),
        )
        .unwrap();
        led.insert_escape("LYD-5", "cfg", "cfg", None, Some(&moved))
            .unwrap();
        led.insert_escape("LYD-9", "gone", "gc'd", None, Some(&"0".repeat(40)))
            .unwrap();
        let c = cfg(reviewer(&repo));

        let strong = run(
            &repo.r,
            &led,
            &c,
            &repo.sirius(),
            "STRONG $SIRIUS_DIFF_RANGE",
            10,
        )
        .unwrap();
        let res: Vec<(&str, &str)> = strong
            .canaries
            .iter()
            .map(|c| (c.source.as_str(), c.result.as_str()))
            .collect();
        assert_eq!(
            (strong.total, strong.caught, strong.stale, strong.errors),
            (2, 2, 1, 1),
            "{res:?}"
        );
        // An unreadable fix is an ERROR, scored as a miss — never "stale".
        assert_eq!(strong.recall, Some(2.0 / 3.0));
        assert_eq!(strong.false_positives, Some(0), "the control is clean");
        let weak = run(
            &repo.r,
            &led,
            &c,
            &repo.sirius(),
            "review $SIRIUS_DIFF_RANGE",
            10,
        )
        .unwrap();
        assert_eq!(weak.recall, Some(0.0), "{weak:#?}");
        assert!(
            weak.recall < strong.recall,
            "a weakened prompt scores lower"
        );
        assert!(!tree_of(&repo.sirius()).exists());
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("pay.txt")).unwrap(),
            "safe()\n"
        );
    }

    #[test]
    fn a_canary_is_blind_to_its_answer() {
        // Review F1/F2/F7: no real issue key (whose board comment states the
        // answer), no same-kind or same-fix escape in the prompt, no "fix the
        // bug" in the tree's history, no named canary commit.
        let repo = Repo::new("blind");
        repo.commit("pay.txt", "safe()\n", "base");
        repo.commit("pay.txt", "UNSAFE()\n", "bug");
        let fix = repo.commit("pay.txt", "safe()\n", "fix THE ESCAPED BUG");
        let led = Ledger::open_in_memory().unwrap();
        led.insert_escape("LYD-1", "secret-kind", "ANSWER ONE", None, Some(&fix))
            .unwrap();
        led.insert_escape("LYD-2", "secret-kind", "ANSWER TWO", None, None)
            .unwrap();
        led.insert_escape("LYD-3", "other-kind", "unrelated", None, Some(&fix))
            .unwrap();
        led.insert_escape("LYD-4", "visible-kind", "VISIBLE", None, None)
            .unwrap();
        let r = run(
            &repo.r,
            &led,
            &cfg(reviewer(&repo)),
            &repo.sirius(),
            "$SIRIUS_ESCAPES",
            10,
        )
        .unwrap();
        assert_eq!(r.total, 1, "one canary per fix commit: {:#?}", r.canaries);
        let seen = repo.captured("CANARY-1");
        assert!(!seen.is_empty(), "the reviewer ran under a neutral key");
        for leak in [
            "ANSWER ONE",
            "ANSWER TWO",
            "unrelated",
            "ESCAPED BUG",
            "escape:",
        ] {
            assert!(!seen.contains(leak), "leaked `{leak}`:\n{seen}");
        }
        assert!(
            seen.contains("VISIBLE"),
            "other kinds still guide the review:\n{seen}"
        );
    }

    #[test]
    fn a_fix_that_landed_as_a_merge_still_reverts() {
        // Review F3: fixes land through merges in a fleet.
        let repo = Repo::new("merge");
        repo.commit("pay.txt", "safe()\n", "base");
        repo.commit("pay.txt", "UNSAFE()\n", "bug");
        repo.git(&["checkout", "-qb", "fixbr"]);
        repo.commit("pay.txt", "safe()\n", "fix");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit("unrelated.txt", "x\n", "meanwhile");
        repo.git(&["merge", "-q", "--no-ff", "--no-edit", "fixbr"]);
        let merge = repo.git(&["rev-parse", "HEAD"]);
        let led = Ledger::open_in_memory().unwrap();
        led.insert_escape("LYD-2", "unsafe-call", "x", None, Some(&merge))
            .unwrap();
        let r = run(
            &repo.r,
            &led,
            &cfg(reviewer(&repo)),
            &repo.sirius(),
            "STRONG",
            10,
        )
        .unwrap();
        assert_eq!((r.total, r.caught, r.stale), (1, 1, 0), "{:#?}", r.canaries);
    }

    #[test]
    fn dead_runs_are_swept() {
        // Review F4: a killed run left its tree (the bug, checked out) and
        // its prompt (the answer) behind.
        let repo = Repo::new("sweep");
        repo.commit("a.txt", "a\n", "base");
        let dead = repo.sirius().join("worktrees/canary-999999");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::create_dir_all(repo.sirius().join("reviews")).unwrap();
        let stale_prompt = repo.sirius().join("reviews/canary-999999-0-prompt.md");
        std::fs::write(&stale_prompt, "the answer").unwrap();
        let led = Ledger::open_in_memory().unwrap();
        run(
            &repo.r,
            &led,
            &cfg(reviewer(&repo)),
            &repo.sirius(),
            "x",
            10,
        )
        .unwrap();
        assert!(!dead.exists() && !stale_prompt.exists());
    }

    #[test]
    fn a_finding_on_the_removed_regression_test_is_not_a_catch() {
        // Review F6: reverting a fix also removes its test.
        let f = |file: &str| Finding {
            id: "R1-1".into(),
            kind: "bug".into(),
            confidence: "confirmed".into(),
            file: Some(file.into()),
            line: None,
            summary: "s".into(),
            scenario: String::new(),
            fix: String::new(),
            response: None,
        };
        let touched = vec!["src/pay.rs".to_string(), "tests/pay_test.rs".to_string()];
        assert!(names_touched(&f("src/pay.rs"), &touched, "/t"));
        assert!(names_touched(&f("/t/src/pay.rs"), &touched, "/t"));
        assert!(!names_touched(&f("tests/pay_test.rs"), &touched, "/t"));
        for p in [
            "web/src/a.test.ts",
            "pkg/x_test.go",
            "spec/a_spec.rb",
            "test_a.py",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        assert!(!is_test_path("src/contest.rs"));
    }

    #[test]
    fn no_reviewer_is_an_error() {
        let repo = Repo::new("noreviewer");
        let led = Ledger::open_in_memory().unwrap();
        let e = run(&repo.r, &led, &Config::default(), &repo.dir, "", 10).unwrap_err();
        assert!(e.contains("review.cmd"), "{e}");
    }
}
