//! The Gate — `sirius gate AMT-n` (PRD §F2, M2; SIRF-5 / D-3).
//!
//! **`hayven affected-tests` is a SELECTOR, not a runner.** Its exit code means
//! "selection computed," never "the tests pass" — a gate keyed on that exit code
//! passes with zero tests executed (both real gate runs to date did exactly
//! that: `roots` matched, `tests: []`, exit 0). So Sirius owns the run-the-tests
//! half itself (D-3):
//!
//!   1. resolve the changed files from a git range,
//!   2. ask `hayven affected-tests --changed <files> --json` which tests they
//!      can affect,
//!   3. decide whether that selection can be *trusted to be complete* — and on
//!      **any doubt** (empty/stale selection, unparseable output, a hub/config
//!      change, no runnable ids) fall back to the FULL suite,
//!   4. run the chosen tests via `gate.test_cmd`, and
//!   5. take the verdict from the **test runner's** exit code.
//!
//! The governing rule is "ran too much, never missed a test." A pass advances
//! the issue via `amt issue update --status <target>`; a fail files the failure
//! as an issue comment and leaves the status untouched.

use crate::amt::Amt;
use crate::config::{GateConfig, GateFallback};
use crate::hayven::Hayven;
use crate::ledger::Ledger;
use crate::shell::Runner;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub issue: String,
    pub tier: String,
    pub passed: bool,
    pub advanced_to: Option<String>,
    /// Count of tests actually run (0 for a full-suite run — the runner decides).
    pub tests_selected: usize,
    /// The runnable test ids the gate ran (subset runs); empty for full-suite.
    pub test_ids: Vec<String>,
    pub comment_filed: bool,
    /// How the gate ran: `subset(n)`, `full-suite`, `blocked`, `pass-with-warning`,
    /// `unconfigured`, or `skipped`.
    pub plan: String,
    /// Whether a test command was actually executed.
    pub ran_tests: bool,
}

// ── Selection ───────────────────────────────────────────────────────────────

/// The parsed answer from `hayven affected-tests --changed <files> --json`.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// The command exited 0 and its stdout parsed as JSON.
    pub ok: bool,
    /// How many changed files mapped to an indexed entity. 0 ⇒ nothing resolved,
    /// so the selection cannot vouch for completeness.
    pub roots: usize,
    /// The selector's self-reported caveat (e.g. "no traces yet … may UNDER-report").
    pub note: String,
    /// Concrete runnable test ids (`tests[].runnable`) to hand a test runner.
    pub runnables: Vec<String>,
    pub detail: String,
}

/// Select over the changed files. Never returns an error — any failure surfaces
/// as `ok:false`, which the planner reads as doubt.
pub fn select(hv: &Hayven, changed_files: &[String]) -> Selection {
    let (ok, parsed, detail) = hv.affected_tests_changed(changed_files);
    let mut sel = Selection {
        ok,
        detail,
        ..Default::default()
    };
    if let Some(v) = parsed {
        sel.roots = v
            .get("roots")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        sel.note = v
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(tests) = v.get("tests").and_then(Value::as_array) {
            for t in tests {
                // A test entry may be an object with a `runnable`, or a bare id.
                if let Some(r) = t.get("runnable").and_then(Value::as_str) {
                    sel.runnables.push(r.to_string());
                } else if let Some(s) = t.as_str() {
                    sel.runnables.push(s.to_string());
                }
            }
        }
    } else {
        sel.ok = false;
    }
    sel
}

/// The selector self-reports when it may be under-reporting (no traces, cold or
/// stale index). Any such note voids trust in a *narrow* selection.
fn note_is_suspect(note: &str) -> bool {
    let n = note.to_lowercase();
    [
        "stale",
        "cold",
        "under-report",
        "under report",
        "under_report",
        "no traces",
        "may under",
    ]
    .iter()
    .any(|k| n.contains(k))
}

/// A changed file whose blast radius the static graph can't bound — build/config/
/// dependency/CI files that can break anything. These always force a full run,
/// even if the selection looks narrow.
fn is_global_impact(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    if path.contains("/.github/") || path.starts_with(".github/") {
        return true;
    }
    matches!(
        base,
        "Cargo.toml"
            | "Cargo.lock"
            | "build.rs"
            | "package.json"
            | "package-lock.json"
            | "bun.lockb"
            | "tsconfig.json"
            | "conftest.py"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "tox.ini"
            | "pytest.ini"
    ) || base.starts_with("requirements")
}

// ── Plan ─────────────────────────────────────────────────────────────────────

/// What the gate has decided to run.
#[derive(Debug, Clone, PartialEq)]
pub enum GatePlan {
    /// A trusted narrow selection — run exactly these runnable ids.
    Subset(Vec<String>, String),
    /// Doubt (or a hub/config change) → run the whole suite. Carries the reason.
    Full(String),
    /// Doubt, and policy (`fallback = fail`) says block rather than run all.
    Block(String),
    /// Doubt, and policy (`fallback = pass-with-warning`) says advance anyway.
    WarnPass(String),
}

/// Decide what to run from a selection and the fallback policy. Pure and
/// unit-testable. The trust bar for a *narrow* run is deliberately high: the
/// command succeeded, at least one changed file resolved, there are runnable
/// ids, the note raises no under-reporting flag, and no global-impact file
/// changed. Anything short of all of that defers to `fallback`.
pub fn decide_plan(sel: &Selection, changed_files: &[String], fallback: GateFallback) -> GatePlan {
    if let Some(f) = changed_files.iter().find(|f| is_global_impact(f)) {
        return GatePlan::Full(format!("global-impact file changed ({f})"));
    }
    let trustworthy =
        sel.ok && sel.roots > 0 && !sel.runnables.is_empty() && !note_is_suspect(&sel.note);
    if trustworthy {
        let reason = format!("{} affected test(s) selected", sel.runnables.len());
        return GatePlan::Subset(sel.runnables.clone(), reason);
    }
    let reason = doubt_reason(sel);
    match fallback {
        GateFallback::FullSuite => GatePlan::Full(reason),
        GateFallback::Fail => GatePlan::Block(reason),
        GateFallback::PassWithWarning => GatePlan::WarnPass(reason),
    }
}

/// A human reason the selection wasn't trusted, most-specific first.
fn doubt_reason(sel: &Selection) -> String {
    if !sel.ok {
        format!(
            "selector failed or returned no JSON ({})",
            first_line(&sel.detail)
        )
    } else if sel.roots == 0 {
        "no changed file mapped to an indexed entity (roots=0)".into()
    } else if note_is_suspect(&sel.note) {
        format!("selector may under-report (note: {})", sel.note)
    } else if sel.runnables.is_empty() {
        "selector produced no runnable test ids".into()
    } else {
        "selection not trustworthy".into()
    }
}

// ── Execute ──────────────────────────────────────────────────────────────────

/// The result of running (or declining to run) the planned tests.
#[derive(Debug, Clone)]
pub struct GateVerdict {
    pub passed: bool,
    /// `subset(n)` / `full-suite` / `blocked` / `pass-with-warning` / `unconfigured`.
    pub plan: String,
    pub reason: String,
    pub ran_tests: bool,
    pub tests_run: usize,
    /// The runnable test ids the gate selected and ran (subset runs). Empty for
    /// a full-suite run (the runner picks) or when no tests ran.
    pub test_ids: Vec<String>,
    pub detail: String,
}

/// Execute a plan against the configured `test_cmd`. Fail-closed: if a run is
/// required but no command is configured, the gate does NOT pass.
pub fn execute_plan(runner: &dyn Runner, test_cmd: Option<&str>, plan: GatePlan) -> GateVerdict {
    match plan {
        GatePlan::Block(reason) => GateVerdict {
            passed: false,
            plan: "blocked".into(),
            reason,
            ran_tests: false,
            tests_run: 0,
            test_ids: vec![],
            detail: String::new(),
        },
        GatePlan::WarnPass(reason) => GateVerdict {
            passed: true,
            plan: "pass-with-warning".into(),
            reason,
            ran_tests: false,
            tests_run: 0,
            test_ids: vec![],
            detail: String::new(),
        },
        GatePlan::Full(reason) => run_cmd(runner, test_cmd, &[], "full-suite", reason),
        GatePlan::Subset(ids, reason) => {
            let label = format!("subset({})", ids.len());
            run_cmd(runner, test_cmd, &ids, &label, reason)
        }
    }
}

/// Run `test_cmd` (optionally with selected ids appended) via `sh -c` and read
/// the verdict from its exit code.
fn run_cmd(
    runner: &dyn Runner,
    test_cmd: Option<&str>,
    ids: &[String],
    plan_label: &str,
    reason: String,
) -> GateVerdict {
    let Some(cmd) = test_cmd else {
        return GateVerdict {
            passed: false,
            plan: "unconfigured".into(),
            reason: "gate.test_cmd is not set — cannot run tests, refusing to pass".into(),
            ran_tests: false,
            tests_run: 0,
            test_ids: vec![],
            detail: String::new(),
        };
    };
    let mut full = cmd.to_string();
    for id in ids {
        full.push(' ');
        full.push_str(&shell_quote(id));
    }
    match runner.run("sh", &["-c", &full]) {
        Ok(out) => GateVerdict {
            passed: out.success(),
            plan: plan_label.to_string(),
            reason,
            ran_tests: true,
            tests_run: ids.len(),
            test_ids: ids.to_vec(),
            detail: last_lines(&out.stdout, &out.stderr, 20),
        },
        Err(e) => GateVerdict {
            passed: false,
            plan: plan_label.to_string(),
            reason,
            ran_tests: false,
            tests_run: 0,
            // The command failed to spawn: no tests ran, so per the field's
            // contract (ids the gate *ran*) this is empty, not the selection.
            test_ids: vec![],
            detail: e.to_string(),
        },
    }
}

/// Evaluate the gate for a set of changed files, free of any Ametrite side
/// effects. The loop and the CLI both call this; each applies its own
/// advance/comment afterwards.
pub fn evaluate(
    hv: &Hayven,
    runner: &dyn Runner,
    gate: &GateConfig,
    changed_files: &[String],
) -> GateVerdict {
    let sel = select(hv, changed_files);
    let plan = decide_plan(&sel, changed_files, gate.fallback);
    execute_plan(runner, gate.test_cmd.as_deref(), plan)
}

// ── CLI orchestration ────────────────────────────────────────────────────────

/// Run the gate for an issue: resolve changed files (git range), evaluate, then
/// advance on pass / comment on fail. `range` defaults to working-tree vs HEAD.
#[allow(clippy::too_many_arguments)]
pub fn run_gate(
    amt: &Amt,
    hv: &Hayven,
    ledger: &Ledger,
    runner: &dyn Runner,
    gate: &GateConfig,
    issue: &str,
    tier: &str,
    target_status: &str,
    range: Option<&str>,
) -> Result<GateOutcome, String> {
    let changed = crate::gitrange::changed_files(runner, range)?;
    if changed.is_empty() {
        return Err(format!(
            "cannot gate {issue}: no changed files in range {} — make the change first",
            range.unwrap_or("working tree vs HEAD")
        ));
    }

    let v = evaluate(hv, runner, gate, &changed);

    if v.passed {
        amt.update_status(issue, target_status)?;
        // A pass-with-warning advanced without running tests — say so loudly.
        if v.plan == "pass-with-warning" {
            let _ = amt.comment(
                issue,
                &format!(
                    "sirius gate PASS-WITH-WARNING (tier {tier}): {} — advanced WITHOUT running tests. Set gate.fallback=full-suite to run them.",
                    v.reason
                ),
            );
        }
        ledger
            .log_policy_event(
                None,
                "gate_tier",
                &serde_json::json!({
                    "issue": issue, "tier": tier, "result": "pass", "plan": v.plan,
                    "reason": v.reason, "tests_run": v.tests_run, "advanced_to": target_status
                }),
            )
            .ok();
        Ok(GateOutcome {
            issue: issue.to_string(),
            tier: tier.to_string(),
            passed: true,
            advanced_to: Some(target_status.to_string()),
            tests_selected: v.tests_run,
            test_ids: v.test_ids,
            comment_filed: v.plan == "pass-with-warning",
            plan: v.plan,
            ran_tests: v.ran_tests,
        })
    } else {
        let body = format!(
            "sirius gate FAILED (tier {tier}, plan {}): {}. {}",
            v.plan,
            v.reason,
            first_line(&v.detail)
        );
        let comment_filed = amt.comment(issue, &body).is_ok();
        ledger
            .log_policy_event(
                None,
                "gate_tier",
                &serde_json::json!({
                    "issue": issue, "tier": tier, "result": "fail", "plan": v.plan,
                    "reason": v.reason, "tests_run": v.tests_run
                }),
            )
            .ok();
        Ok(GateOutcome {
            issue: issue.to_string(),
            tier: tier.to_string(),
            passed: false,
            advanced_to: None,
            tests_selected: v.tests_run,
            test_ids: v.test_ids,
            comment_filed,
            plan: v.plan,
            ran_tests: v.ran_tests,
        })
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// Keep the last `n` non-empty lines of combined stdout+stderr — enough to show
/// the failing test without dumping the whole run into a comment.
fn last_lines(stdout: &str, stderr: &str, n: usize) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for l in stdout.lines().chain(stderr.lines()) {
        if !l.trim().is_empty() {
            lines.push(l.trim_end());
        }
    }
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Minimal POSIX single-quote for a test id passed to `sh -c`.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '='))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};

    fn sel_json(ok: bool, roots: usize, note: &str, runnables: &[&str]) -> Selection {
        let m = MockRunner::new();
        let roots_arr: Vec<String> = (0..roots).map(|i| format!("r{i}")).collect();
        let tests: Vec<serde_json::Value> = runnables
            .iter()
            .map(|r| serde_json::json!({ "runnable": r }))
            .collect();
        let body = serde_json::json!({ "roots": roots_arr, "note": note, "tests": tests });
        m.push(MockResponse::new(
            &["hayven", "affected-tests"],
            if ok { 0 } else { 1 },
            &body.to_string(),
            "",
        ));
        let hv = Hayven::new(&m);
        select(&hv, &["src/a.rs".into()])
    }

    #[test]
    fn select_parses_roots_note_runnables() {
        let s = sel_json(true, 3, "clean", &["t::a", "t::b"]);
        assert!(s.ok);
        assert_eq!(s.roots, 3);
        assert_eq!(s.runnables, vec!["t::a", "t::b"]);
    }

    #[test]
    fn empty_changed_files_is_doubt() {
        let m = MockRunner::new();
        let hv = Hayven::new(&m);
        let s = select(&hv, &[]);
        assert!(!s.ok);
        // No hayven call was made for an empty file set.
        assert_eq!(m.call_count(), 0);
    }

    #[test]
    fn trusted_selection_runs_subset() {
        let s = sel_json(true, 2, "traced", &["t::a"]);
        let plan = decide_plan(&s, &["src/a.rs".into()], GateFallback::FullSuite);
        assert!(matches!(plan, GatePlan::Subset(ref ids, _) if ids == &["t::a"]));
    }

    #[test]
    fn under_report_note_forces_full_suite() {
        // The exact note this repo's untraced index emits.
        let s = sel_json(
            true,
            5,
            "no traces yet — static only, may UNDER-report",
            &["t::a"],
        );
        let plan = decide_plan(&s, &["src/a.rs".into()], GateFallback::FullSuite);
        assert!(matches!(plan, GatePlan::Full(_)), "got {plan:?}");
    }

    #[test]
    fn zero_roots_forces_full_suite() {
        let s = sel_json(true, 0, "clean", &["t::a"]);
        let plan = decide_plan(&s, &["src/a.rs".into()], GateFallback::FullSuite);
        assert!(matches!(plan, GatePlan::Full(_)));
    }

    #[test]
    fn empty_selection_forces_full_suite() {
        let s = sel_json(true, 3, "clean", &[]);
        let plan = decide_plan(&s, &["src/a.rs".into()], GateFallback::FullSuite);
        assert!(matches!(plan, GatePlan::Full(_)));
    }

    #[test]
    fn global_impact_file_forces_full_even_with_narrow_selection() {
        let s = sel_json(true, 2, "traced", &["t::a"]);
        let plan = decide_plan(&s, &["Cargo.toml".into()], GateFallback::FullSuite);
        assert!(matches!(plan, GatePlan::Full(ref r) if r.contains("Cargo.toml")));
    }

    #[test]
    fn fallback_fail_blocks_on_doubt() {
        let s = sel_json(false, 0, "", &[]);
        let plan = decide_plan(&s, &["src/a.rs".into()], GateFallback::Fail);
        assert!(matches!(plan, GatePlan::Block(_)));
    }

    #[test]
    fn fallback_warn_passes_on_doubt() {
        let s = sel_json(false, 0, "", &[]);
        let plan = decide_plan(&s, &["src/a.rs".into()], GateFallback::PassWithWarning);
        assert!(matches!(plan, GatePlan::WarnPass(_)));
    }

    #[test]
    fn execute_full_suite_passes_when_tests_pass() {
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "ok");
        let v = execute_plan(&m, Some("cargo test"), GatePlan::Full("doubt".into()));
        assert!(v.passed);
        assert!(v.ran_tests);
        assert_eq!(v.plan, "full-suite");
        // The full suite command was run, verbatim, with no selected ids.
        assert_eq!(m.recorded()[0], "sh -c cargo test");
    }

    #[test]
    fn execute_full_suite_fails_when_tests_fail() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["sh", "-c"], 101, "", "test failed"));
        let v = execute_plan(&m, Some("cargo test"), GatePlan::Full("doubt".into()));
        assert!(!v.passed);
        assert!(v.ran_tests);
    }

    #[test]
    fn execute_subset_appends_selected_ids() {
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "ok");
        let v = execute_plan(
            &m,
            Some("pytest -q"),
            GatePlan::Subset(vec!["tests/test_x.py::test_a".into()], "1".into()),
        );
        assert!(v.passed);
        assert_eq!(v.tests_run, 1);
        assert_eq!(m.recorded()[0], "sh -c pytest -q tests/test_x.py::test_a");
    }

    #[test]
    fn execute_without_test_cmd_fails_closed() {
        let m = MockRunner::new();
        let v = execute_plan(&m, None, GatePlan::Full("doubt".into()));
        assert!(!v.passed);
        assert!(!v.ran_tests);
        assert_eq!(v.plan, "unconfigured");
        // No command was run.
        assert_eq!(m.call_count(), 0);
    }

    fn gate_cfg(cmd: Option<&str>, fb: GateFallback) -> GateConfig {
        GateConfig {
            test_cmd: cmd.map(|s| s.to_string()),
            fallback: fb,
        }
    }

    #[test]
    fn run_gate_pass_advances_status() {
        let m = MockRunner::new();
        // changed files
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        // selector: untraced → doubt → full suite
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"no traces yet — may UNDER-report","tests":[]}"#,
        );
        // full suite runs and passes
        m.expect(&["sh", "-c"], 0, "test result: ok");
        // advance
        m.expect(
            &["amt", "--json", "issue", "update"],
            0,
            r#"{"id":"AMT-7"}"#,
        );
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let cfg = gate_cfg(Some("cargo test"), GateFallback::FullSuite);
        let o = run_gate(
            &amt,
            &hv,
            &led,
            &m,
            &cfg,
            "AMT-7",
            "safe",
            "in_review",
            None,
        )
        .unwrap();
        assert!(o.passed);
        assert_eq!(o.plan, "full-suite");
        assert!(o.ran_tests);
        assert_eq!(o.advanced_to.as_deref(), Some("in_review"));
        assert!(m.recorded().iter().any(|c| c.contains("issue update")));
        assert!(!m.recorded().iter().any(|c| c.contains("issue comment")));
    }

    #[test]
    fn run_gate_blocks_when_selected_tests_fail() {
        // The exit criterion in code form: a real regression makes the suite
        // fail, and the gate must NOT advance the issue.
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"no traces yet — may UNDER-report","tests":[]}"#,
        );
        // full suite runs and FAILS
        m.push(MockResponse::new(
            &["sh", "-c"],
            101,
            "test result: FAILED. 1 failed",
            "",
        ));
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let cfg = gate_cfg(Some("cargo test"), GateFallback::FullSuite);
        let o = run_gate(
            &amt,
            &hv,
            &led,
            &m,
            &cfg,
            "AMT-7",
            "safe",
            "in_review",
            None,
        )
        .unwrap();
        assert!(!o.passed);
        assert!(o.advanced_to.is_none());
        assert!(o.comment_filed);
        assert!(m.recorded().iter().any(|c| c.contains("issue comment")));
        assert!(!m.recorded().iter().any(|c| c.contains("issue update")));
    }

    #[test]
    fn run_gate_fails_closed_without_test_cmd() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"may UNDER-report","tests":[]}"#,
        );
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let cfg = gate_cfg(None, GateFallback::FullSuite);
        let o = run_gate(
            &amt,
            &hv,
            &led,
            &m,
            &cfg,
            "AMT-7",
            "safe",
            "in_review",
            None,
        )
        .unwrap();
        assert!(!o.passed);
        assert_eq!(o.plan, "unconfigured");
        assert!(!o.ran_tests);
        assert!(o.advanced_to.is_none());
    }

    #[test]
    fn run_gate_errors_without_changed_files() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "");
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let cfg = gate_cfg(Some("cargo test"), GateFallback::FullSuite);
        assert!(run_gate(
            &amt,
            &hv,
            &led,
            &m,
            &cfg,
            "AMT-7",
            "safe",
            "in_review",
            None
        )
        .is_err());
    }
}
