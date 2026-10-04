//! `sirius review-canary` (SIRF-35): measure reviewer recall on REAL misses.
//!
//! "The review came back clean" is only worth something if the reviewer
//! catches bugs. Every recorded escape with a fix commit is a known bug that
//! once got past review here: revert its fix onto the current base and the
//! bug is back, in this codebase, in this code style. Replaying those (plus
//! hand-written `.sirius/canaries/*.patch` mutations) through the configured
//! reviewer — exactly as a review round runs it, in a throwaway worktree —
//! scores recall; one benign CONTROL change scores false positives.

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
    /// The false-positive control: a patch file, or the built-in doc file.
    Control(Option<PathBuf>),
}

impl Source {
    fn label(&self) -> String {
        match self {
            Source::Escape(e) => format!("escape:{}", e.id),
            Source::Patch(p) | Source::Control(Some(p)) => format!(
                "patch:{}",
                p.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default()
            ),
            Source::Control(None) => "control:built-in".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CanaryResult {
    pub source: String,
    pub issue: Option<String>,
    pub kind: Option<String>,
    /// `caught | missed | stale | error` (the control: `clean | flagged`).
    pub result: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub ok: bool,
    pub base: String,
    pub model: Option<String>,
    /// Canaries that applied and were reviewed (stale and errored excluded).
    pub total: usize,
    pub caught: usize,
    pub stale: usize,
    pub recall: Option<f64>,
    pub false_positives: usize,
    pub canaries: Vec<CanaryResult>,
}

/// Does a finding's file name one of the files the canary changed?
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
        .any(|t| t == file || file.ends_with(&format!("/{t}")))
}

/// The canaries to replay: escapes with a fix (newest first), then the
/// `.sirius/canaries/*.patch` files (sorted) — at most `n`.
fn sources(ledger: &Ledger, sirius_dir: &Path, n: usize) -> (Vec<Source>, Source) {
    let mut out: Vec<Source> = ledger
        .escapes(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.fix_commit.is_some())
        .map(Source::Escape)
        .collect();
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

/// Apply a canary in the tree at `t` (detached at `cur`) and commit it.
/// `Ok(None)` = stale (no longer applies, or applies as a no-op).
fn apply(runner: &dyn Runner, t: &str, src: &Source) -> Result<Option<Vec<String>>, String> {
    let git = |args: &[&str]| {
        let mut full = vec!["-C", t];
        full.extend(crate::frontier::THROWAWAY_MERGE_CONFIG);
        full.extend(args);
        run_git(runner, &full)
    };
    let applied = match src {
        Source::Escape(e) => {
            let fix = e.fix_commit.as_deref().unwrap_or_default();
            let r = git(&["revert", "--no-commit", fix]);
            if r.is_err() {
                let _ = git(&["revert", "--abort"]);
            }
            r.is_ok()
        }
        Source::Patch(p) | Source::Control(Some(p)) => {
            git(&["apply", "--index", &p.to_string_lossy()]).is_ok()
        }
        Source::Control(None) => {
            let f = Path::new(t).join("SIRIUS_CANARY_CONTROL.md");
            std::fs::write(
                &f,
                "# Notes\n\nThis page collects onboarding notes for new contributors.\n",
            )
            .map_err(|e| format!("cannot write the control file: {e}"))?;
            git(&["add", "SIRIUS_CANARY_CONTROL.md"]).is_ok()
        }
    };
    // Nothing staged = the change is already absent (e.g. the fix was
    // itself reverted since): there is nothing for a reviewer to find.
    if !applied || git(&["diff", "--cached", "--quiet"]).is_ok() {
        return Ok(None);
    }
    git(&[
        "commit",
        "--no-verify",
        "-qm",
        &format!("sirius canary: {}", src.label()),
    ])?;
    let touched = git(&["diff", "--name-only", "HEAD~1", "HEAD"])?
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    Ok(Some(touched))
}

struct Ctx<'a> {
    runner: &'a dyn Runner,
    cfg: &'a Config,
    sirius_dir: &'a Path,
    prompt: &'a str,
    review_cmd: &'a str,
    model: Option<String>,
    cur: &'a str,
    escapes: Vec<EscapeRow>,
    automated: Vec<(String, String)>,
}

/// Replay one canary; `Ok(findings, touched)` or a result string on failure.
fn replay(
    cx: &Ctx,
    src: &Source,
    i: usize,
) -> Result<(Vec<Finding>, Vec<String>), (String, String)> {
    let t_path = cx
        .sirius_dir
        .join("worktrees")
        .join(format!("canary-{}", std::process::id()));
    let t = t_path.to_string_lossy().to_string();
    let _ = run_git(cx.runner, &["worktree", "remove", "--force", &t]);
    let _ = std::fs::remove_dir_all(&t_path);
    run_git(cx.runner, &["worktree", "add", "--detach", &t, cx.cur]).map_err(|e| {
        (
            "error".to_string(),
            format!("cannot create the canary tree: {e}"),
        )
    })?;
    let result = (|| {
        let touched = match apply(cx.runner, &t, src) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Err((
                    "stale".to_string(),
                    "no longer applies to the current base".to_string(),
                ))
            }
            Err(e) => return Err(("error".to_string(), e)),
        };
        let (issue, skip) = match src {
            Source::Escape(e) => (e.issue.clone(), Some(e.id)),
            _ => ("CANARY".to_string(), None),
        };
        let reviews = cx.sirius_dir.join("reviews");
        let _ = std::fs::create_dir_all(&reviews);
        let stem = format!("canary-{}-{i}", std::process::id());
        let out_path = reviews.join(format!("{stem}.json"));
        let _ = std::fs::remove_file(&out_path);
        let kv = |k: &str, v: &str| (k.to_string(), v.to_string());
        let mut vars = vec![
            kv("SIRIUS_ISSUE", &issue),
            kv("SIRIUS_WORKER", "sirius/canary"),
            kv("AMT_AGENT", "sirius/canary"),
            kv("SIRIUS_WORKTREE", &t),
            kv("SIRIUS_BASE", cx.cur),
            kv("SIRIUS_PHASE", "review"),
            kv("SIRIUS_REVIEW_DIR", &t),
            kv("SIRIUS_DIFF_RANGE", &format!("{}..HEAD", cx.cur)),
            kv("SIRIUS_ROUND", "1"),
            kv("SIRIUS_REVIEW_OUT", &out_path.to_string_lossy()),
            kv("SIRIUS_FRONTIER", ""),
            kv("SIRIUS_SIBLING_BRANCHES", ""),
            kv("SIRIUS_SIBLINGS", "(none)"),
            // Its own escape left out: the canary must not carry its answer.
            kv(
                "SIRIUS_ESCAPES",
                &crate::escape::patterns_section(
                    &cx.escapes,
                    &cx.automated,
                    skip,
                    crate::escape::PROMPT_TOP,
                ),
            ),
        ];
        let rendered = crate::review::render_prompt(
            cx.prompt,
            &[
                vars.clone(),
                vec![kv("SIRIUS_REVIEW_FINDINGS", "(none — this is round 1)")],
            ]
            .concat(),
        );
        let prompt_path = reviews.join(format!("{stem}-prompt.md"));
        std::fs::write(&prompt_path, rendered)
            .map_err(|e| ("error".to_string(), format!("cannot write the prompt: {e}")))?;
        vars.push(kv("SIRIUS_REVIEW_PROMPT", &prompt_path.to_string_lossy()));
        if let Some(m) = &cx.model {
            vars.push(kv("ANTHROPIC_MODEL", m));
            vars.push(kv("SIRIUS_REVIEW_MODEL", m));
        }
        let log = cx.sirius_dir.join("logs").join(format!("{stem}.log"));
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(cx.cfg.review.timeout_secs.max(1)),
            heartbeat_interval: Duration::from_secs(60),
            log_path: Some(log.clone()),
            env: vars,
        };
        let cmd = format!(
            "cd \"$SIRIUS_REVIEW_DIR\" || exit 1; {}",
            crate::run::template_cmd(cx.review_cmd, &issue, "sirius/canary", cx.model.as_deref())
        );
        let ran = cx
            .runner
            .run_agent("sh", &["-c", &cmd], &opts, &mut || {})
            .map_err(|e| ("error".to_string(), format!("reviewer did not run: {e}")))?;
        if ran.timed_out() {
            return Err(("error".into(), "reviewer timed out".into()));
        }
        let raw = crate::review::read_review_output(
            &out_path,
            Some(&log),
            ran.output().code == Some(0),
            1,
        )
        .ok_or_else(|| {
            (
                "error".to_string(),
                format!("no findings JSON (log {})", log.display()),
            )
        })?;
        let report = crate::review::parse_review(&raw, 1).map_err(|e| ("error".to_string(), e))?;
        Ok((report.findings, touched))
    })();
    let _ = run_git(cx.runner, &["worktree", "remove", "--force", &t]);
    let _ = std::fs::remove_dir_all(&t_path);
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
    let model = crate::models::review_model(crate::models::active(&cfg.models, false));
    let cx = Ctx {
        runner,
        cfg,
        sirius_dir,
        prompt,
        review_cmd,
        model,
        cur: &cur,
        escapes: ledger.escapes(None).unwrap_or_default(),
        automated: ledger.automated_kinds().unwrap_or_default(),
    };
    let (canaries, control) = sources(ledger, sirius_dir, n);
    let blocking = |f: &Finding| crate::review::is_blocking(f, &cfg.review.block_on);
    let mut results = Vec::new();
    let (mut total, mut caught, mut stale) = (0, 0, 0);
    for (i, src) in canaries.iter().enumerate() {
        let (issue, kind) = match src {
            Source::Escape(e) => (Some(e.issue.clone()), Some(e.kind.clone())),
            _ => (None, None),
        };
        let (result, detail) = match replay(&cx, src, i) {
            Ok((findings, touched)) => {
                total += 1;
                let hit: Vec<&Finding> = findings
                    .iter()
                    .filter(|f| {
                        blocking(f) && names_touched(f, &touched, &cx_tree_prefix(sirius_dir))
                    })
                    .collect();
                if hit.is_empty() {
                    (
                        "missed".to_string(),
                        format!(
                            "{} finding(s), none blocking on {}",
                            findings.len(),
                            touched.join(", ")
                        ),
                    )
                } else {
                    caught += 1;
                    ("caught".to_string(), hit[0].summary.clone())
                }
            }
            Err((r, d)) => {
                if r == "stale" {
                    stale += 1;
                }
                (r, d)
            }
        };
        results.push(CanaryResult {
            source: src.label(),
            issue,
            kind,
            result,
            detail,
        });
    }
    let (fp, control_result) = match replay(&cx, &control, canaries.len()) {
        Ok((findings, _)) => {
            let fp = findings.iter().filter(|f| blocking(f)).count();
            (
                fp,
                CanaryResult {
                    source: control.label(),
                    issue: None,
                    kind: None,
                    result: if fp == 0 { "clean" } else { "flagged" }.into(),
                    detail: format!("{fp} confirmed blocking finding(s) on a benign change"),
                },
            )
        }
        Err((r, d)) => (
            0,
            CanaryResult {
                source: control.label(),
                issue: None,
                kind: None,
                result: r,
                detail: d,
            },
        ),
    };
    results.push(control_result);
    let report = Report {
        ok: true,
        base: cur.clone(),
        model: cx.model.clone(),
        total,
        caught,
        stale,
        recall: (total > 0).then(|| caught as f64 / total as f64),
        false_positives: fp,
        canaries: results,
    };
    ledger
        .insert_canary_run(
            report.model.as_deref(),
            total,
            caught,
            fp,
            &json!(report.canaries),
        )
        .map_err(|e| format!("cannot record the canary run: {e}"))?;
    Ok(report)
}

/// Findings may name files by absolute path inside the canary tree.
fn cx_tree_prefix(sirius_dir: &Path) -> String {
    sirius_dir
        .join("worktrees")
        .join(format!("canary-{}", std::process::id()))
        .to_string_lossy()
        .to_string()
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
            std::fs::create_dir_all(dir.join(".sirius")).unwrap();
            let repo = Repo {
                r: RealRunner {
                    cwd: Some(dir.clone()),
                },
                dir,
            };
            repo.git(&["init", "-q", "-b", "main"]);
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
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A stand-in reviewer: with a STRONG prompt it finds the planted
    /// `UNSAFE` line in the diff; with a weak one it reports nothing.
    fn reviewer(repo: &Repo) -> String {
        let script = repo.dir.join(".sirius/reviewer.sh");
        std::fs::write(
            &script,
            r#"if grep -q STRONG "$SIRIUS_REVIEW_PROMPT"; then
  f=$(git diff --name-only $SIRIUS_DIFF_RANGE | while read p; do git diff $SIRIUS_DIFF_RANGE -- "$p" | grep -q '^+.*UNSAFE' && echo "$p"; done | head -1)
  if [ -n "$f" ]; then
    echo "{\"findings\":[{\"id\":\"R1-1\",\"kind\":\"bug\",\"confidence\":\"confirmed\",\"file\":\"$f\",\"line\":1,\"summary\":\"unsafe call\",\"scenario\":\"x\",\"fix\":\"y\"}]}"
    exit 0
  fi
fi
echo '{"findings":[]}'
"#,
        )
        .unwrap();
        format!("sh {}", script.display())
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
        // A hand-written mutation canary, and a stale escape whose fix no
        // longer reverts cleanly.
        std::fs::create_dir_all(repo.dir.join(".sirius/canaries")).unwrap();
        repo.commit("other.txt", "fine\n", "other");
        std::fs::write(
            repo.dir.join(".sirius/canaries/swallow.patch"),
            "--- a/other.txt\n+++ b/other.txt\n@@ -1 +1 @@\n-fine\n+UNSAFE swallow()\n",
        )
        .unwrap();
        let led = Ledger::open_in_memory().unwrap();
        led.insert_escape(
            "LYD-4",
            "unsafe-call",
            "UNSAFE call shipped",
            Some("e2e"),
            Some(&fix),
        )
        .unwrap();
        led.insert_escape(
            "LYD-9",
            "gone",
            "stale",
            None,
            Some("0000000000000000000000000000000000000000"),
        )
        .unwrap();
        let sirius_dir = repo.dir.join(".sirius");
        let c = cfg(reviewer(&repo));

        let strong = run(
            &repo.r,
            &led,
            &c,
            &sirius_dir,
            "STRONG review of $SIRIUS_DIFF_RANGE",
            10,
        )
        .unwrap();
        assert_eq!(
            (strong.total, strong.caught, strong.stale),
            (2, 2, 1),
            "{strong:#?}"
        );
        assert_eq!(strong.recall, Some(1.0));
        assert_eq!(strong.false_positives, 0, "the control is clean");
        let weak = run(
            &repo.r,
            &led,
            &c,
            &sirius_dir,
            "review $SIRIUS_DIFF_RANGE",
            10,
        )
        .unwrap();
        assert_eq!(weak.recall, Some(0.0), "{weak:#?}");
        assert!(
            weak.recall < strong.recall,
            "a weakened prompt scores lower"
        );
        // Throwaway trees are gone; the base is untouched.
        assert!(!sirius_dir
            .join(format!("worktrees/canary-{}", std::process::id()))
            .exists());
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("pay.txt")).unwrap(),
            "safe()\n"
        );
    }

    #[test]
    fn a_canary_never_sees_its_own_escape_in_the_prompt() {
        let repo = Repo::new("ownescape");
        repo.commit("pay.txt", "safe()\n", "base");
        repo.commit("pay.txt", "UNSAFE()\n", "bug");
        let fix = repo.commit("pay.txt", "safe()\n", "fix");
        let led = Ledger::open_in_memory().unwrap();
        led.insert_escape("LYD-4", "secret-kind", "THE ANSWER", None, Some(&fix))
            .unwrap();
        let sirius_dir = repo.dir.join(".sirius");
        let r = run(
            &repo.r,
            &led,
            &cfg(reviewer(&repo)),
            &sirius_dir,
            "$SIRIUS_ESCAPES",
            10,
        )
        .unwrap();
        assert_eq!(r.total, 1);
        // Canary #0 is the escape itself (the control, #1, may list it).
        let own = std::fs::read_to_string(
            sirius_dir
                .join("reviews")
                .join(format!("canary-{}-0-prompt.md", std::process::id())),
        )
        .unwrap();
        assert!(!own.contains("THE ANSWER"), "{own}");
        let control = std::fs::read_to_string(
            sirius_dir
                .join("reviews")
                .join(format!("canary-{}-1-prompt.md", std::process::id())),
        )
        .unwrap();
        assert!(
            control.contains("THE ANSWER"),
            "the control prompt is the real one"
        );
    }

    #[test]
    fn no_reviewer_is_an_error() {
        let repo = Repo::new("noreviewer");
        let led = Ledger::open_in_memory().unwrap();
        let e = run(&repo.r, &led, &Config::default(), &repo.dir, "", 10).unwrap_err();
        assert!(e.contains("review.cmd"), "{e}");
    }
}
