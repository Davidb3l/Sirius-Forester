//! `sirius integrate` (SIRF-32): run the integration command on the frontier
//! BEFORE anything merges.
//!
//! In the Lydgr run, Dragonfly needed a Lua flag and every BullMQ enqueue
//! failed live — unit tests stub the queue, and no per-branch review or gate
//! ever ran the integrated system. This builds the frontier (the base tip plus
//! every sibling awaiting integration, `frontier::build`), points
//! `refs/sirius/frontier` at it, and runs `integration.cmd` there. Red files
//! one issue and records the stop-the-line state the run loop honors.

use crate::amt::Amt;
use crate::config::Config;
use crate::frontier;
use crate::gitrange::run_git;
use crate::ledger::Ledger;
use crate::shell::{AgentRunOpts, Runner};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

/// The ledger meta key holding [`RedState`] while integration is red.
pub const RED_KEY: &str = "integration_red";
/// The ref that always names the last integrated frontier.
pub const FRONTIER_REF: &str = "refs/sirius/frontier";
/// Board writes by `sirius integrate` are attributed to this author.
const AUTHOR: &str = "sirius/integrate";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedState {
    pub at: String,
    pub frontier: String,
    #[serde(default)]
    pub issue: Option<String>,
}

/// The current red state, if integration is red.
pub fn red_state(ledger: &Ledger) -> Option<RedState> {
    ledger
        .meta(RED_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// `sirius integrate --json` (CONTRACTS §2).
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub ok: bool,
    pub base_ref: String,
    pub frontier: String,
    pub included: Vec<String>,
    pub left_out: Vec<Value>,
    pub ran: bool,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub log: Option<String>,
    pub issue: Option<String>,
    pub red: bool,
}

fn is_closed(status: &str) -> bool {
    matches!(status, "done" | "canceled" | "cancelled")
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Build the frontier, run `integration.cmd` on it, and keep the red state.
/// `runner` runs git and the command from the repo root; `sirius_dir` is the
/// absolute `.sirius/`.
pub fn integrate(
    runner: &dyn Runner,
    amt: &Amt,
    ledger: &Ledger,
    cfg: &Config,
    sirius_dir: &Path,
) -> Result<Report, String> {
    let base_ref = match cfg.review.base_ref.clone() {
        Some(r) => r,
        None => run_git(runner, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|o| o.stdout.trim().to_string())
            .ok()
            .filter(|r| !r.is_empty() && r != "HEAD")
            .ok_or("no base_ref: set review.base_ref or run on a branch (HEAD is detached)")?,
    };
    let cur = run_git(
        runner,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )
    .map(|o| o.stdout.trim().to_string())
    .map_err(|e| format!("base_ref `{base_ref}` does not resolve: {e}"))?;
    let status_of = |issue: &str| {
        amt.issue_show(issue).ok().map(|v| {
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            (s("status"), s("title"))
        })
    };
    let sibs = frontier::siblings(runner, &status_of, &cur, "", &cfg.target_status);

    let t = sirius_dir.join("worktrees").join("integrate");
    let t_str = t.to_string_lossy().to_string();
    let _ = run_git(runner, &["worktree", "remove", "--force", &t_str]);
    let _ = std::fs::remove_dir_all(&t);
    run_git(runner, &["worktree", "add", "--detach", &t_str, &cur])
        .map_err(|e| format!("cannot create the integration tree: {e}"))?;
    let result = run_in_tree(runner, cfg, sirius_dir, &t_str, &base_ref, &cur, &sibs);
    let _ = run_git(runner, &["worktree", "remove", "--force", &t_str]);
    let _ = std::fs::remove_dir_all(&t);
    let mut report = result?;

    let prior = red_state(ledger);
    if report.ok {
        if let Some(issue) = prior.as_ref().and_then(|p| p.issue.as_deref()) {
            let _ = amt.comment_as(
                issue,
                &format!(
                    "sirius integrate: GREEN again at {} (included: {}).",
                    report.frontier,
                    list_or_none(&report.included)
                ),
                AUTHOR,
            );
        }
        ledger
            .set_meta(RED_KEY, None)
            .map_err(|e| format!("cannot clear the red state: {e}"))?;
        return Ok(report);
    }
    let body = red_body(cfg, &report, ledger_log_tail(&report));
    // One issue per red streak: while the last one is open, comment on it.
    let open = prior.and_then(|p| p.issue).filter(|i| {
        amt.issue_show(i)
            .map(|v| !is_closed(v["status"].as_str().unwrap_or_default()))
            .unwrap_or(false)
    });
    report.issue = match open {
        Some(i) => {
            let _ = amt.comment_as(&i, &format!("Still red.\n\n{body}"), AUTHOR);
            Some(i)
        }
        None => match amt.issue_create(
            "Integration red: the frontier fails integration.cmd",
            &body,
            "urgent",
            &["integration"],
        ) {
            Ok(i) => Some(i),
            Err(e) => {
                eprintln!("sirius: could not file the integration issue: {e}");
                None
            }
        },
    };
    report.red = true;
    let state = RedState {
        at: crate::ledger::now_iso8601(),
        frontier: report.frontier.clone(),
        issue: report.issue.clone(),
    };
    ledger
        .set_meta(RED_KEY, Some(&json!(state).to_string()))
        .map_err(|e| format!("cannot record the red state: {e}"))?;
    Ok(report)
}

fn list_or_none(v: &[String]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

fn ledger_log_tail(report: &Report) -> String {
    report
        .log
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|l| tail(&l, 40))
        .unwrap_or_default()
}

fn red_body(cfg: &Config, r: &Report, log_tail: String) -> String {
    let mut s = format!(
        "`{}` failed on the integration frontier {} ({} + in-flight work).\n\nIncluded (merged in this order): {}\nExit: {}{}\nLog: {}",
        cfg.integration.cmd.as_deref().unwrap_or_default(),
        r.frontier,
        r.base_ref,
        list_or_none(&r.included),
        r.exit.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
        if r.timed_out { " (timed out)" } else { "" },
        r.log.as_deref().unwrap_or("(none)"),
    );
    if !r.left_out.is_empty() {
        s.push_str(&format!(
            "\nLeft out (conflict with earlier siblings): {}",
            r.left_out
                .iter()
                .filter_map(|v| v["issue"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s.push_str(
        "\n\nEvery included issue passed its own gate and review; the failure is in how they combine. Reproduce with `git checkout refs/sirius/frontier`.",
    );
    if !log_tail.is_empty() {
        s.push_str(&format!("\n\nLast lines:\n```\n{log_tail}\n```"));
    }
    s
}

fn run_in_tree(
    runner: &dyn Runner,
    cfg: &Config,
    sirius_dir: &Path,
    t: &str,
    base_ref: &str,
    cur: &str,
    sibs: &[frontier::Sibling],
) -> Result<Report, String> {
    let built = frontier::build(runner, t, cur, sibs, &HashSet::new())?;
    run_git(runner, &["update-ref", FRONTIER_REF, &built.tip])
        .map_err(|e| format!("cannot update {FRONTIER_REF}: {e}"))?;
    let mut report = Report {
        ok: true,
        base_ref: base_ref.to_string(),
        frontier: built.tip.clone(),
        included: built
            .merged
            .iter()
            .map(|&i| sibs[i].issue.clone())
            .collect(),
        left_out: built
            .left_out
            .iter()
            .map(|(s, f)| json!({"issue": s.issue, "files": f}))
            .collect(),
        ran: false,
        exit: None,
        timed_out: false,
        log: None,
        issue: None,
        red: false,
    };
    let Some(cmd) = cfg
        .integration
        .cmd
        .as_deref()
        .filter(|c| !c.trim().is_empty())
    else {
        return Ok(report);
    };
    let log = sirius_dir.join("logs").join(format!(
        "integrate-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));
    let opts = AgentRunOpts {
        timeout: Duration::from_secs(cfg.integration.timeout_secs.max(1)),
        heartbeat_interval: Duration::from_secs(60),
        log_path: Some(log.clone()),
        env: vec![
            ("SIRIUS_INTEGRATION_DIR".into(), t.to_string()),
            ("SIRIUS_FRONTIER".into(), built.tip.clone()),
            ("SIRIUS_BASE_REF".into(), base_ref.to_string()),
        ],
    };
    let full = format!("cd \"$SIRIUS_INTEGRATION_DIR\" || exit 1; {cmd}");
    let ran = runner.run_agent("sh", &["-c", &full], &opts, &mut || {});
    report.ran = true;
    report.log = Some(log.display().to_string());
    match ran {
        Ok(o) => {
            report.timed_out = o.timed_out();
            report.exit = o.output().code;
            report.ok = o.success() && !o.timed_out();
        }
        Err(e) => return Err(format!("cannot run integration.cmd: {e}")),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::MockRunner;

    fn cfg(cmd: Option<&str>) -> Config {
        let mut c = Config::default();
        c.review.base_ref = Some("main".into());
        c.integration.cmd = cmd.map(String::from);
        c
    }

    fn program(m: &MockRunner, exit: i32) {
        m.expect(&["git", "rev-parse", "--verify"], 0, "tip0\n");
        m.expect(&["git", "for-each-ref"], 0, "sirius/amt-7\tsib7\n");
        m.expect(
            &["amt", "--json", "issue", "show", "AMT-7"],
            0,
            r#"{"id":"AMT-7","status":"in_review","title":"Queue jobs"}"#,
        );
        m.expect(&["sh", "-c"], exit, "boom: ERR unknown command 'EVAL'");
    }

    fn dir() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("sirius-integrate-{}-{n}", std::process::id()))
    }

    #[test]
    fn red_files_one_issue_then_comments_then_green_clears() {
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let d = dir();
        let t = d.join("worktrees").join("integrate").display().to_string();

        // Run 1: red → one issue filed, red recorded.
        program(&m, 1);
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        m.expect(
            &["amt", "--json", "issue", "create"],
            0,
            r#"{"id":"AMT-90"}"#,
        );
        let r = integrate(&m, &amt, &led, &cfg(Some("./ci.sh")), &d).unwrap();
        assert!(!r.ok && r.red && r.ran);
        assert_eq!(r.exit, Some(1));
        assert_eq!(r.included, vec!["AMT-7"]);
        assert_eq!(r.frontier, "front1");
        assert_eq!(r.issue.as_deref(), Some("AMT-90"));
        let rec = m.recorded();
        assert!(
            rec.iter()
                .any(|c| c == "git update-ref refs/sirius/frontier front1"),
            "{rec:?}"
        );
        assert!(rec.iter().any(|c| c.starts_with(&format!("git -C {t} "))
            && c.contains(" merge ")
            && c.ends_with("sib7")));
        assert!(rec
            .iter()
            .any(|c| c == &format!("git worktree remove --force {t}")));
        let create = rec
            .iter()
            .find(|c| c.starts_with("amt --json issue create"))
            .unwrap();
        assert!(
            create.contains("AMT-7") && create.contains("--label integration"),
            "{create}"
        );
        assert_eq!(red_state(&led).unwrap().issue.as_deref(), Some("AMT-90"));

        // Run 2: still red, issue open → a comment, not a second issue.
        program(&m, 1);
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front2\n");
        m.expect(
            &["amt", "--json", "issue", "show", "AMT-90"],
            0,
            r#"{"id":"AMT-90","status":"todo"}"#,
        );
        let r = integrate(&m, &amt, &led, &cfg(Some("./ci.sh")), &d).unwrap();
        assert_eq!(r.issue.as_deref(), Some("AMT-90"));
        let creates = m
            .recorded()
            .iter()
            .filter(|c| c.starts_with("amt --json issue create"))
            .count();
        assert_eq!(creates, 1, "one issue per red streak");
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.starts_with("amt --json issue comment AMT-90 -m Still red.")));
        assert_eq!(red_state(&led).unwrap().frontier, "front2");

        // Run 3: green → red cleared, the issue told.
        program(&m, 0);
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front3\n");
        let r = integrate(&m, &amt, &led, &cfg(Some("./ci.sh")), &d).unwrap();
        assert!(r.ok && !r.red);
        assert_eq!(red_state(&led), None);
        assert!(m.recorded().iter().any(|c| c.starts_with(
            "amt --json issue comment AMT-90 -m sirius integrate: GREEN again at front3"
        )));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_closed_red_issue_gets_a_fresh_one() {
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        led.set_meta(
            RED_KEY,
            Some(r#"{"at":"x","frontier":"f","issue":"AMT-90"}"#),
        )
        .unwrap();
        let d = dir();
        let t = d.join("worktrees").join("integrate").display().to_string();
        program(&m, 2);
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        m.expect(
            &["amt", "--json", "issue", "show", "AMT-90"],
            0,
            r#"{"id":"AMT-90","status":"done"}"#,
        );
        m.expect(
            &["amt", "--json", "issue", "create"],
            0,
            r#"{"id":"AMT-91"}"#,
        );
        let r = integrate(&m, &amt, &led, &cfg(Some("./ci.sh")), &d).unwrap();
        assert_eq!(r.issue.as_deref(), Some("AMT-91"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_cmd_builds_and_reports_only() {
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let d = dir();
        let t = d.join("worktrees").join("integrate").display().to_string();
        program(&m, 1); // the sh expectation is never consumed
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        let r = integrate(&m, &amt, &led, &cfg(None), &d).unwrap();
        assert!(r.ok && !r.ran && !r.red);
        assert!(!m.recorded().iter().any(|c| c.starts_with("sh -c")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
