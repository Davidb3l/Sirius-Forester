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
use crate::config::{Config, IntegrationOnFail};
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

/// The current red state, if integration is red (display only — the
/// blocking decision is [`block_reason`], which fails closed).
pub fn red_state(ledger: &Ledger) -> Option<RedState> {
    ledger
        .meta(RED_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// With `integration.on_fail = block`: why the line is stopped, if it is.
/// Fails CLOSED — a red state Sirius cannot read is a reason to hold, after
/// one retry for a transiently busy ledger. Every reason starts
/// "integration red" (the run loop words its stop message by that).
pub fn block_reason(cfg: &Config, ledger: &Ledger) -> Option<String> {
    if cfg.integration.on_fail != IntegrationOnFail::Block {
        return None;
    }
    let read = ledger.meta(RED_KEY).or_else(|_| {
        std::thread::sleep(Duration::from_secs(1));
        ledger.meta(RED_KEY)
    });
    match read {
        Ok(None) => None,
        Ok(Some(raw)) => Some(match serde_json::from_str::<RedState>(&raw) {
            Ok(r) => format!(
                "integration red at {}{} — fix it and run `sirius integrate` until green (or `sirius integrate --clear-red` to override)",
                r.frontier,
                r.issue.map(|i| format!(" ({i})")).unwrap_or_default()
            ),
            Err(_) => "integration red (state unreadable) — run `sirius integrate` (or `--clear-red`)".into(),
        }),
        Err(e) => Some(format!(
            "integration red? the ledger cannot say ({e}) — holding until it can"
        )),
    }
}

/// `sirius integrate --json` (CONTRACTS §2).
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// `integration.cmd` ran and passed (true with nothing to run).
    pub ok: bool,
    pub base_ref: String,
    pub frontier: String,
    pub included: Vec<String>,
    pub left_out: Vec<Value>,
    /// The board and git answered, so every sibling awaiting integration
    /// was found (a transient failure makes this `false`; the next run
    /// heals it). Only a pass over a complete discovery clears red. Siblings
    /// in `left_out` conflict TEXTUALLY with earlier ones — the merge order's
    /// problem, flagged by the review stage — and do not keep the line red.
    pub complete: bool,
    pub ran: bool,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub log: Option<String>,
    pub issue: Option<String>,
    /// The red state after this run.
    pub red: bool,
}

/// How a run reads to the outside — the spine event, the exit code, and the
/// human verdict. The exit code follows the LINE: 3 while red, whatever the
/// command did (CONTRACTS §2).
pub fn verdict(r: &Report) -> (&'static str, u8, &'static str) {
    let exit = if r.red { 3 } else { 0 };
    match (r.ran, r.ok, r.red) {
        (false, _, true) => (
            "integration.built",
            exit,
            "frontier built (no integration.cmd) — still RED",
        ),
        (false, _, false) => (
            "integration.built",
            exit,
            "frontier built (no integration.cmd)",
        ),
        (true, false, _) => ("integration.failed", exit, "RED"),
        (true, true, true) => (
            "integration.partial",
            exit,
            "passed over a PARTIAL discovery — still RED",
        ),
        (true, true, false) => ("integration.passed", exit, "GREEN"),
    }
}

fn is_closed(status: &str) -> bool {
    matches!(status, "done" | "canceled" | "cancelled")
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// One `sirius integrate` per repo at a time: two would remove each
/// other's tree mid-run (a verdict over a deleted directory) and race the
/// red state. A pidfile created ATOMICALLY with its content (written to a
/// private temp file, then hard-linked into place — the link fails if the
/// lock exists), so no reader ever sees an empty lock. A dead holder is
/// taken over; Drop removes the lock only while it is still ours.
struct Lock {
    path: std::path::PathBuf,
    pid: String,
}

impl Lock {
    fn acquire(sirius_dir: &Path) -> Result<Lock, String> {
        let _ = std::fs::create_dir_all(sirius_dir);
        let path = sirius_dir.join("integrate.lock");
        let pid = std::process::id().to_string();
        let tmp = sirius_dir.join(format!("integrate.lock.{pid}"));
        std::fs::write(&tmp, &pid).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        let taken = (|| {
            for _ in 0..2 {
                match std::fs::hard_link(&tmp, &path) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let holder = std::fs::read_to_string(&path).unwrap_or_default();
                        match holder.trim().parse::<i32>() {
                            Ok(p) if crate::libc_kill_probe(p) => {
                                return Err(format!(
                                    "another `sirius integrate` is running (pid {p}, {})",
                                    path.display()
                                ))
                            }
                            _ => {
                                // Dead holder: take it over by RENAMING it
                                // away (atomic — only one taker wins), then
                                // put it back if it changed underneath us.
                                let grave = path.with_extension(format!("dead.{pid}"));
                                if std::fs::rename(&path, &grave).is_ok() {
                                    let moved = std::fs::read_to_string(&grave).unwrap_or_default();
                                    if moved != holder {
                                        // A live taker's lock: restore it.
                                        let _ = std::fs::hard_link(&grave, &path);
                                    }
                                    let _ = std::fs::remove_file(&grave);
                                }
                            }
                        }
                    }
                    Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
                }
            }
            Err(format!("cannot take {}", path.display()))
        })();
        let _ = std::fs::remove_file(&tmp);
        taken.map(|()| Lock { path, pid })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .trim()
            == self.pid
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `sirius integrate --clear-red`: a human's explicit override of the red
/// state (e.g. the failure is understood and accepted). Says so on the red
/// issue. Returns the issue it was attached to, if any.
pub fn clear_red(amt: &Amt, ledger: &Ledger, sirius_dir: &Path) -> Result<Option<String>, String> {
    let _lock = Lock::acquire(sirius_dir)?;
    let prior = red_state(ledger);
    ledger
        .set_meta(RED_KEY, None)
        .map_err(|e| format!("cannot clear the red state: {e}"))?;
    let issue = prior.and_then(|p| p.issue);
    if let Some(i) = &issue {
        let _ = amt.comment_as(
            i,
            "sirius integrate --clear-red: the red state was cleared BY HAND — the line is open again without a green run.",
            AUTHOR,
        );
    }
    Ok(issue)
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
    let _lock = Lock::acquire(sirius_dir)?;
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
    // EVERY sibling awaiting integration, from one board query.
    let found = frontier::siblings(
        runner,
        &frontier::awaiting(amt, &cfg.target_status),
        &cur,
        "",
    );

    let t = sirius_dir.join("worktrees").join("integrate");
    let t_str = crate::gitrange::git_path(&t);
    let _ = run_git(runner, &["worktree", "remove", "--force", &t_str]);
    let _ = std::fs::remove_dir_all(&t);
    run_git(runner, &["worktree", "add", "--detach", &t_str, &cur])
        .map_err(|e| format!("cannot create the integration tree: {e}"))?;
    let result = run_in_tree(runner, cfg, sirius_dir, &t_str, &base_ref, &cur, &found);
    let _ = run_git(runner, &["worktree", "remove", "--force", &t_str]);
    let _ = std::fs::remove_dir_all(&t);
    let mut report = result?;

    let prior = red_state(ledger);
    report.issue = prior.as_ref().and_then(|p| p.issue.clone());
    report.red = prior.is_some();
    // Nothing tested is not green: the red state stays exactly as it was.
    if !report.ran {
        return Ok(report);
    }
    if report.ok {
        if !report.complete {
            // A pass over PART of the in-flight work proves nothing about
            // the rest — it never clears a red state.
            if let Some(issue) = &report.issue {
                let _ = amt.comment_as(
                    issue,
                    &format!(
                        "sirius integrate: passed at {}, but the board or git could not list every in-flight issue — still red; the next run retries.",
                        report.frontier
                    ),
                    AUTHOR,
                );
            }
            return Ok(report);
        }
        if let Some(issue) = &report.issue {
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
        report.red = false;
        return Ok(report);
    }
    // RED. Record it FIRST — the line stops even if filing the issue fails.
    let record = |issue: Option<String>| {
        let state = RedState {
            at: crate::ledger::now_iso8601(),
            frontier: report.frontier.clone(),
            issue,
        };
        ledger
            .set_meta(RED_KEY, Some(&json!(state).to_string()))
            .map_err(|e| format!("cannot record the red state: {e}"))
    };
    record(report.issue.clone())?;
    let body = red_body(cfg, &report, log_tail(&report));
    // One issue per red streak: while the last one is open (or its state
    // cannot be read), comment on it instead of filing another.
    let open = report.issue.clone().filter(|i| {
        amt.issue_show(i)
            .map(|v| !is_closed(v["status"].as_str().unwrap_or_default()))
            .unwrap_or(true)
    });
    let issue = match open {
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
    record(issue.clone())?;
    report.issue = issue;
    report.red = true;
    Ok(report)
}

fn list_or_none(v: &[String]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

fn left_out_issues(r: &Report) -> Vec<String> {
    r.left_out
        .iter()
        .filter_map(|v| v["issue"].as_str().map(String::from))
        .collect()
}

fn log_tail(report: &Report) -> String {
    report
        .log
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|l| tail(&l, 40))
        .unwrap_or_default()
}

fn red_body(cfg: &Config, r: &Report, log_tail: String) -> String {
    let cmd = cfg.integration.cmd.as_deref().unwrap_or_default();
    let mut s = if r.included.is_empty() {
        format!(
            "`{cmd}` fails on {} itself ({}) — no in-flight work was merged, so the base is red.",
            r.base_ref, r.frontier
        )
    } else {
        format!(
            "`{cmd}` fails on the integration frontier {} = {} + {} (merged in this order). The failure is in the base or in how these changes combine.",
            r.frontier,
            r.base_ref,
            r.included.join(", ")
        )
    };
    s.push_str(&format!(
        "\n\nExit: {}{}\nLog: {}",
        r.exit.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
        if r.timed_out { " (timed out)" } else { "" },
        r.log.as_deref().unwrap_or("(none)"),
    ));
    if !r.left_out.is_empty() {
        s.push_str(&format!(
            "\nLeft out (conflict with earlier siblings): {}",
            left_out_issues(r).join(", ")
        ));
    }
    s.push_str(&format!(
        "\n\nReproduce: `git checkout {}` (refs/sirius/frontier moves with every run).",
        r.frontier
    ));
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
    found: &frontier::Discovery,
) -> Result<Report, String> {
    let sibs = &found.sibs;
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
        complete: found.complete,
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
        "integrate-{}-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
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
            &["amt", "--json", "issue", "list", "--status", "in_review"],
            0,
            r#"[{"id":"AMT-7","status":"in_review","title":"Queue jobs"}]"#,
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

    fn red(led: &Ledger) {
        led.set_meta(
            RED_KEY,
            Some(r#"{"at":"x","frontier":"f0","issue":"AMT-90"}"#),
        )
        .unwrap();
    }

    #[test]
    fn nothing_tested_is_not_green() {
        // Review F3: with no integration.cmd the red state stays untouched.
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        red(&led);
        let d = dir();
        let t = d.join("worktrees").join("integrate").display().to_string();
        program(&m, 0);
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        let r = integrate(&m, &amt, &led, &cfg(None), &d).unwrap();
        assert!(!r.ran && r.red, "still red");
        assert!(red_state(&led).is_some());
        assert!(!m.recorded().iter().any(|c| c.contains("issue comment")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pass_over_part_of_the_in_flight_work_never_clears_red() {
        // Review F4: the board could not be read — siblings may be missing.
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        red(&led);
        let d = dir();
        let t = d.join("worktrees").join("integrate").display().to_string();
        m.expect(&["git", "rev-parse", "--verify"], 0, "tip0\n");
        m.expect(&["git", "for-each-ref"], 0, "sirius/amt-7\tsib7\n");
        m.expect(&["amt", "--json", "issue", "list"], 1, "database is locked");
        m.expect(&["sh", "-c"], 0, "ok");
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        let r = integrate(&m, &amt, &led, &cfg(Some("./ci.sh")), &d).unwrap();
        assert!(r.ok && !r.complete && r.red, "{r:?}");
        assert!(red_state(&led).is_some());
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("issue comment AMT-90")
                && c.contains("could not list every in-flight issue")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn red_is_recorded_even_when_filing_the_issue_fails() {
        // The line stops whether or not amt can take the issue. (Recording
        // BEFORE filing — so a crash inside `amt issue create` cannot leave
        // the line open — is structural in `integrate`; a mock cannot crash
        // mid-call, so this test covers only the failed-filing outcome.)
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let d = dir();
        let t = d.join("worktrees").join("integrate").display().to_string();
        program(&m, 1);
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        m.expect(&["amt", "--json", "issue", "create"], 1, "boom");
        let r = integrate(&m, &amt, &led, &cfg(Some("./ci.sh")), &d).unwrap();
        assert!(r.red && r.issue.is_none());
        assert_eq!(red_state(&led).unwrap().frontier, "front1");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_textually_conflicting_sibling_does_not_wedge_red() {
        // Review N1: two in-flight siblings that conflict with each other
        // are the merge order's problem; a pass over the rest clears red.
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        red(&led);
        let d = dir();
        let t = d.join("worktrees").join("integrate").display().to_string();
        m.expect(&["git", "rev-parse", "--verify"], 0, "tip0\n");
        m.expect(
            &["git", "for-each-ref"],
            0,
            "sirius/amt-7\tsib7\nsirius/amt-8\tsib8\n",
        );
        m.expect(
            &["amt", "--json", "issue", "list", "--status", "in_review"],
            0,
            r#"[{"id":"AMT-7","title":"a"},{"id":"AMT-8","title":"b"}]"#,
        );
        // sib7 merges; sib8 conflicts with it.
        m.expect(&["git", "-C", &t, "-c"], 0, "");
        m.push(crate::shell::MockResponse::new(
            &["git", "-C", &t, "-c"],
            1,
            "",
            "CONFLICT",
        ));
        m.expect(&["git", "-C", &t, "diff"], 0, "shared.txt\n");
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        m.expect(&["sh", "-c"], 0, "ok");
        let r = integrate(&m, &amt, &led, &cfg(Some("./ci.sh")), &d).unwrap();
        assert_eq!(r.included, vec!["AMT-7"]);
        assert_eq!(r.left_out.len(), 1);
        assert!(r.ok && r.complete && !r.red, "{r:?}");
        assert_eq!(red_state(&led), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn clear_red_is_an_explicit_override() {
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        red(&led);
        let d = dir();
        assert_eq!(
            clear_red(&amt, &led, &d).unwrap().as_deref(),
            Some("AMT-90")
        );
        assert_eq!(red_state(&led), None);
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("issue comment AMT-90") && c.contains("BY HAND")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_exit_code_and_event_follow_the_line() {
        let r = |ran, ok, red| Report {
            ok,
            base_ref: "main".into(),
            frontier: "f".into(),
            included: vec![],
            left_out: vec![],
            complete: true,
            ran,
            exit: None,
            timed_out: false,
            log: None,
            issue: None,
            red,
        };
        assert_eq!(verdict(&r(true, true, false)).0, "integration.passed");
        assert_eq!(verdict(&r(true, true, false)).1, 0);
        assert_eq!(
            verdict(&r(true, true, true)),
            (
                "integration.partial",
                3,
                "passed over a PARTIAL discovery — still RED"
            )
        );
        assert_eq!(verdict(&r(true, false, true)).1, 3);
        assert_eq!(
            verdict(&r(false, true, true)).1,
            3,
            "untested while red is still red"
        );
        assert_eq!(
            verdict(&r(false, true, false)),
            (
                "integration.built",
                0,
                "frontier built (no integration.cmd)"
            )
        );
    }

    #[test]
    fn one_integrate_at_a_time() {
        // Review F2: a second run would delete the first one's tree mid-run.
        let d = dir();
        let first = Lock::acquire(&d).unwrap();
        let e = Lock::acquire(&d).err().unwrap();
        assert!(e.contains("another `sirius integrate` is running"), "{e}");
        drop(first);
        assert!(Lock::acquire(&d).is_ok(), "released on drop");
        // A dead holder is taken over.
        std::fs::write(d.join("integrate.lock"), "999999").unwrap();
        assert!(Lock::acquire(&d).is_ok());
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
