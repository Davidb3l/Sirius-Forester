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
use crate::shell::{resolve_shell, run_in_shell, Runner, ShellCmd};
use serde_json::Value;

/// Stable machine-readable verdict codes (SF-11).
///
/// The `plan` label is HUMAN copy — it has already been reworded once and will
/// be again. A script that has to tell "your tests failed" from "this workspace
/// was never configured to run tests" must not have to string-match it, and it
/// cannot use the exit code either: `sirius gate` deliberately keeps exit 3 for
/// BOTH (see `run_gate`). These codes are the contract for that distinction.
pub mod reason_code {
    /// Tests ran and the runner said pass.
    pub const PASS: &str = "pass";
    /// Tests ran and the runner said fail — a genuine, retryable blocked gate.
    pub const TESTS_FAILED: &str = "tests_failed";
    /// `gate.fallback = fail` refused to run the whole suite on doubt.
    pub const BLOCKED_BY_POLICY: &str = "blocked_by_policy";
    /// `gate.fallback = pass-with-warning` advanced without running anything.
    pub const PASSED_WITHOUT_TESTS: &str = "passed_without_tests";
    /// `gate.test_cmd` is unset: this workspace CANNOT run tests. Structural —
    /// no retry, no fresh agent run, and no code change will alter it.
    pub const UNCONFIGURED_TEST_CMD: &str = "unconfigured_test_cmd";
    /// The shell itself could not be spawned (SF-15). Also not about the code.
    pub const SHELL_SPAWN_FAILED: &str = "shell_spawn_failed";
}

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
    /// `unconfigured`, or `skipped`. HUMAN copy — see [`reason_code`].
    pub plan: String,
    /// Stable machine-readable code for the verdict; one of [`reason_code`].
    /// Surfaced by `cmd_gate` in the `--json` envelope, so a consumer can tell
    /// an unconfigured workspace from a genuinely failing gate without parsing
    /// the human `plan` string (both exit 3).
    pub reason_code: &'static str,
    /// Human explanation behind `reason_code`, carrying any remedy.
    pub reason: String,
    /// TRUE when no fresh attempt can change this verdict (today: an
    /// unconfigured `test_cmd`). Mirrors [`GateVerdict::structural`].
    pub structural: bool,
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
    // EVERY changed file must have resolved to an indexed entity — a partial
    // mapping (roots < changed) means the unmapped files contributed nothing
    // to the selection, which is exactly the "missed a test" hole the module
    // header forbids. `roots > 0` alone blessed a 1-of-4 mapping.
    //
    // KNOWN RESIDUAL: `roots` is the daemon's root-entity count, not a
    // per-file mapping — one file resolving to several entities can mask
    // another file resolving to none. Closing that fully needs a per-file
    // answer from `hayven affected-tests`; until then this cardinality check
    // is strictly tighter than the old bar, never looser.
    let fully_mapped = sel.roots >= changed_files.len();
    // Runnable ids come from daemon JSON. A dash-leading id survives
    // shell_quote's allowlist and reaches the test runner AS A FLAG
    // (`pytest -q --co` exits 0 running nothing) — refuse to trust any.
    let ids_sane = !sel.runnables.is_empty() && sel.runnables.iter().all(|r| !r.starts_with('-'));
    let trustworthy = sel.ok && fully_mapped && ids_sane && !note_is_suspect(&sel.note);
    if trustworthy {
        let reason = format!("{} affected test(s) selected", sel.runnables.len());
        return GatePlan::Subset(sel.runnables.clone(), reason);
    }
    fallback_plan(fallback, doubt_reason(sel, changed_files.len()))
}

/// The ONE fallback-policy → plan mapping, shared by `decide_plan` and
/// `evaluate_doubt` so the doubt path and the normal path can never diverge
/// on what a fallback policy means.
fn fallback_plan(fallback: GateFallback, reason: String) -> GatePlan {
    match fallback {
        GateFallback::FullSuite => GatePlan::Full(reason),
        GateFallback::Fail => GatePlan::Block(reason),
        GateFallback::PassWithWarning => GatePlan::WarnPass(reason),
    }
}

/// A human reason the selection wasn't trusted, most-specific first.
fn doubt_reason(sel: &Selection, changed: usize) -> String {
    if !sel.ok {
        format!(
            "selector failed or returned no JSON ({})",
            first_line(&sel.detail)
        )
    } else if sel.roots == 0 {
        "no changed file mapped to an indexed entity (roots=0)".into()
    } else if sel.roots < changed {
        format!(
            "only {} of {changed} changed file(s) mapped to indexed entities",
            sel.roots
        )
    } else if note_is_suspect(&sel.note) {
        format!("selector may under-report (note: {})", sel.note)
    } else if sel.runnables.is_empty() {
        "selector produced no runnable test ids".into()
    } else if sel.runnables.iter().any(|r| r.starts_with('-')) {
        "selector returned flag-like runnable id(s) — refusing to pass them to the test runner"
            .into()
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
    /// Stable machine-readable code for this verdict; one of [`reason_code`].
    pub reason_code: &'static str,
    /// TRUE when the verdict cannot change on a fresh attempt (today: an
    /// unconfigured `test_cmd`). Typed here — at the source of the verdict —
    /// so retry policy never string-matches the human-facing `plan` label.
    pub structural: bool,
    /// SIRF-50: the output line that makes this failure look like an
    /// ENVIRONMENT fault (a missing dependency, a test runner not installed)
    /// rather than a code fault. Only ever set on a `tests_failed` verdict;
    /// scanned over the FULL output, not just `detail`'s tail.
    pub env_fault: Option<String>,
}

/// Execute a plan against the configured `test_cmd`, through `shell`.
/// Fail-closed: if a run is required but no command is configured, the gate
/// does NOT pass.
///
/// The shell is a PARAMETER, not a constant: `resolve_shell()` legitimately
/// answers `/bin/sh`, a Git-for-Windows `sh.exe`, or `cmd.exe` depending on the
/// machine (SF-15), and a gate test that had to guess which one would be a test
/// of the developer's box.
pub fn execute_plan(
    runner: &dyn Runner,
    shell: &ShellCmd,
    test_cmd: Option<&str>,
    plan: GatePlan,
) -> GateVerdict {
    match plan {
        GatePlan::Block(reason) => GateVerdict {
            passed: false,
            structural: false,
            reason_code: reason_code::BLOCKED_BY_POLICY,
            plan: "blocked".into(),
            reason,
            ran_tests: false,
            tests_run: 0,
            test_ids: vec![],
            detail: String::new(),
            env_fault: None,
        },
        GatePlan::WarnPass(reason) => GateVerdict {
            passed: true,
            structural: false,
            reason_code: reason_code::PASSED_WITHOUT_TESTS,
            plan: "pass-with-warning".into(),
            reason,
            ran_tests: false,
            tests_run: 0,
            test_ids: vec![],
            detail: String::new(),
            env_fault: None,
        },
        GatePlan::Full(reason) => run_cmd(runner, shell, test_cmd, &[], "full-suite", reason),
        GatePlan::Subset(ids, reason) => {
            let label = format!("subset({})", ids.len());
            run_cmd(runner, shell, test_cmd, &ids, &label, reason)
        }
    }
}

/// Run `test_cmd` (optionally with selected ids appended) through `shell` and
/// read the verdict from its exit code.
fn run_cmd(
    runner: &dyn Runner,
    shell: &ShellCmd,
    test_cmd: Option<&str>,
    ids: &[String],
    plan_label: &str,
    reason: String,
) -> GateVerdict {
    // A blank command is no command: it would reach the shell, run nothing,
    // exit 0, and pass everything. Fail it closed like an unset one.
    let Some(cmd) = test_cmd.filter(|c| !c.trim().is_empty()) else {
        return GateVerdict {
            passed: false,
            // The one STRUCTURAL verdict: no test_cmd exists, so no fresh
            // attempt can change this — retry policy reads this field.
            structural: true,
            reason_code: reason_code::UNCONFIGURED_TEST_CMD,
            plan: "unconfigured".into(),
            // SF-11: this is NOT "your tests failed" — this workspace has never
            // been able to run tests at all, and every gate here has failed
            // closed since `sirius init`. Say which, and say what to do; the
            // repo-specific command comes from `sirius doctor`, which knows the
            // workspace root (the gate does not).
            reason: "gate.test_cmd is not set — this workspace cannot run tests, so the \
                     gate refuses to pass (no test failure occurred). Set gate.test_cmd in \
                     .sirius/config.json; `sirius doctor` prints the command detected for \
                     this repo."
                .into(),
            ran_tests: false,
            tests_run: 0,
            test_ids: vec![],
            detail: String::new(),
            env_fault: None,
        };
    };
    let mut full = cmd.to_string();
    for id in ids {
        full.push(' ');
        full.push_str(&shell_quote(id, shell.posix));
    }
    match run_in_shell(runner, shell, &full) {
        Ok(out) => GateVerdict {
            passed: out.success(),
            structural: false,
            reason_code: if out.success() {
                reason_code::PASS
            } else {
                reason_code::TESTS_FAILED
            },
            plan: plan_label.to_string(),
            reason,
            ran_tests: true,
            tests_run: ids.len(),
            test_ids: ids.to_vec(),
            detail: last_lines(&out.stdout, &out.stderr, 20),
            env_fault: if out.success() {
                None
            } else {
                env_fault_line(&out.stdout, &out.stderr, out.code, cmd)
            },
        },
        Err(e) => GateVerdict {
            passed: false,
            structural: false,
            // SF-15: the SHELL did not start, so nothing about the code was
            // tested. `e` already names the shell (see shell::run_in_shell) —
            // the old bare "program not found" read as a missing test binary.
            reason_code: reason_code::SHELL_SPAWN_FAILED,
            plan: plan_label.to_string(),
            reason,
            ran_tests: false,
            tests_run: 0,
            // The command failed to spawn: no tests ran, so per the field's
            // contract (ids the gate *ran*) this is empty, not the selection.
            test_ids: vec![],
            detail: e,
            env_fault: None,
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
    execute_plan(runner, &resolve_shell(), gate.test_cmd.as_deref(), plan)
}

/// Gate under maximal doubt — the changed-file set itself is UNKNOWN (e.g. git
/// failed), so no selection is possible and no narrow run can be trusted. Goes
/// straight to the fallback policy. SIRF-11: the loop used to fold a git error
/// into "nothing changed" and skip the gate entirely — fail-open; this is the
/// fail-closed replacement.
pub fn evaluate_doubt(runner: &dyn Runner, gate: &GateConfig, reason: &str) -> GateVerdict {
    let plan = fallback_plan(gate.fallback, reason.to_string());
    execute_plan(runner, &resolve_shell(), gate.test_cmd.as_deref(), plan)
}

/// The fleet's gate: an isolated worktree is invisible to the code-graph
/// daemon (it watches the main checkout), so `affected-tests` would select
/// from the PRE-change graph and a narrow subset could miss tests the change
/// newly affects. Selection is therefore skipped and the fallback policy runs.
///
/// SIRF-48: the reason used to read "isolated worktree is not visible to the
/// code-graph daemon — selection cannot be trusted", and it was the ONLY text
/// in the failure comment — workers and humans alike read it as the CAUSE of
/// the failure. It is a routine note about which tests ran, and now says so.
pub fn evaluate_isolated(runner: &dyn Runner, gate: &GateConfig) -> GateVerdict {
    evaluate_doubt(runner, gate, &isolated_reason(gate.fallback))
}

fn isolated_reason(fallback: GateFallback) -> String {
    let then = match fallback {
        GateFallback::FullSuite => "ran the full suite",
        GateFallback::Fail => "gate.fallback=fail blocks instead of running the full suite",
        GateFallback::PassWithWarning => {
            "gate.fallback=pass-with-warning advances without running tests"
        }
    };
    format!(
        "selection skipped in an isolated worktree (the code graph watches the main checkout) — {then}"
    )
}

// ── Failure diagnostics (SIRF-48 / SIRF-50) ──────────────────────────────────

/// How much of a failing gate's output is surfaced — in the issue comment, on
/// the NDJSON `gate` event and the ledger's `gate_tier` event (`error_tail`),
/// and to the next attempt (`SIRIUS_LAST_GATE_TAIL`).
pub const ERROR_TAIL_LINES: usize = 15;
/// Byte cap on that tail (the END is kept — the summary and the last failure).
pub const ERROR_TAIL_MAX_BYTES: usize = 2048;

/// The last [`ERROR_TAIL_LINES`] non-empty lines of a verdict's output,
/// capped at [`ERROR_TAIL_MAX_BYTES`] (keeping the end, cut on a line boundary
/// where one exists, marked with a leading `…`). Empty when nothing ran.
pub fn error_tail(v: &GateVerdict) -> String {
    tail_of(&v.detail)
}

fn tail_of(output: &str) -> String {
    // Control characters (a NUL above all) cannot go into an environment
    // variable — SIRIUS_LAST_GATE_TAIL would make every retry fail to spawn —
    // and garble a comment. Keep tabs; lines are split below.
    let output: String = output
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(ERROR_TAIL_LINES);
    let tail = lines[start..].join("\n");
    if tail.len() <= ERROR_TAIL_MAX_BYTES {
        return tail;
    }
    // The marker counts toward the cap, so the whole tail stays ≤ the cap.
    const MARK: &str = "…";
    let mut cut = tail.len() - (ERROR_TAIL_MAX_BYTES - MARK.len());
    while !tail.is_char_boundary(cut) {
        cut += 1;
    }
    // Prefer starting on a whole line (unless the cut already does, or that
    // would leave nothing).
    if tail.as_bytes()[cut - 1] != b'\n' {
        if let Some(nl) = tail[cut..].find('\n') {
            if cut + nl + 1 < tail.len() {
                cut += nl + 1;
            }
        }
    }
    format!("{MARK}{}", &tail[cut..])
}

/// `text` in a Markdown code fence that its own backticks cannot close.
fn fenced(text: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}text\n{text}\n{fence}")
}

/// The fleet's failure comment for `v`. When the gate produced output, its
/// tail leads and the selection/plan note is demoted to a trailing line — the
/// note is never the cause of a red suite (SIRF-48). With no output (blocked,
/// unconfigured), the reason IS the explanation and stays inline. `env_note`
/// says what the env-fault re-gate did, when one was considered.
pub fn loop_fail_comment(v: &GateVerdict, env_note: Option<&str>) -> String {
    let tail = error_tail(v);
    let mut body = if tail.is_empty() {
        format!(
            "sirius: gate failed [{}, {}]: {}",
            v.plan, v.reason_code, v.reason
        )
    } else {
        format!(
            "sirius: gate failed [{}, {}] — output tail:\n{}\nPlan: {}",
            v.plan,
            v.reason_code,
            fenced(&tail),
            v.reason
        )
    };
    if let Some(note) = env_note {
        body.push('\n');
        body.push_str(note);
    }
    body
}

/// Output patterns that mean "a dependency is missing from this environment",
/// not "the code is wrong" (SIRF-50). Matched case-sensitively, per line.
const ENV_FAULT_PATTERNS: &[&str] = &[
    "Cannot find module",
    "Cannot find package",
    "ERR_MODULE_NOT_FOUND",
    "ModuleNotFoundError: No module named",
    "error[E0463]: can't find crate",
];

/// The first output line that marks a failing gate as an ENVIRONMENT fault, if
/// any (SIRF-50):
/// - a missing-dependency message ([`ENV_FAULT_PATTERNS`]) — EXCEPT a
///   `Cannot find module/package` naming a relative or absolute PATH
///   (`'./utlis'`, `'/abs/x.js'`): that is the code importing a file that does
///   not exist, a code fault no setup run can fix;
/// - the shell reporting the test runner ITSELF missing (`<runner>: command
///   not found` / `<runner>: not found`, runner = `test_cmd`'s program);
/// - exit 127 (the shell's "command not found") with such a line for any
///   command — e.g. `npm test` running `vitest` with no `node_modules/.bin`.
///
/// A false positive costs one setup re-run and one re-gate (bounded: once per
/// iteration); a false negative costs a work attempt — so the bar is
/// "plausibly the environment", not proof.
pub fn env_fault_line(
    stdout: &str,
    stderr: &str,
    code: Option<i32>,
    test_cmd: &str,
) -> Option<String> {
    let runner_prog = crate::shell::agent_program(test_cmd).map(|p| {
        // `/usr/local/bin/bun` is reported by the shell under its full path,
        // but match the basename too.
        p.rsplit('/').next().unwrap_or(&p).to_string()
    });
    for line in stdout.lines().chain(stderr.lines()) {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Some(p) = ENV_FAULT_PATTERNS.iter().find(|p| l.contains(**p)) {
            if names_a_path(l, p) {
                continue;
            }
            return Some(l.to_string());
        }
        let not_found = l.ends_with(": command not found") || l.ends_with(": not found");
        if !not_found {
            continue;
        }
        // Anchored: `prog` must be a whole word (line start, or after a
        // space, `:` or `/`) — `go` must not match "asset logo: not found".
        let runner_missing = runner_prog.as_deref().is_some_and(|prog| {
            [": command not found", ": not found"].iter().any(|suffix| {
                l.strip_suffix(&format!("{prog}{suffix}"))
                    .is_some_and(|before| before.is_empty() || before.ends_with([' ', ':', '/']))
            })
        });
        if runner_missing || code == Some(127) {
            return Some(l.to_string());
        }
    }
    None
}

/// True when the `Cannot find module|package` on `line` names a file PATH
/// (`'./x'`, `"../x"`, `'/abs/x'`) rather than a package.
fn names_a_path(line: &str, pattern: &str) -> bool {
    if !pattern.starts_with("Cannot find") {
        return false;
    }
    let Some(at) = line.find(pattern) else {
        return false;
    };
    let rest = line[at + pattern.len()..].trim_start();
    let rest = rest.trim_start_matches(['\'', '"', '`']);
    let b = rest.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    rest.starts_with('.') || rest.starts_with('/') || drive
}

/// Run a worktree setup command (`worktree.setup_cmd`, SIRF-50) through
/// `shell`, with the runner's cwd (the worker's worktree). `Err` names the
/// command and carries its exit code and output tail.
pub fn run_setup(
    runner: &dyn Runner,
    shell: &ShellCmd,
    cmd: &str,
    opts: &SetupOpts,
    heartbeat: &mut dyn FnMut(),
) -> Result<(), String> {
    use crate::shell::{AgentOutcome, AgentRunOpts, CmdOutput};
    use std::time::{Duration, Instant};
    // Package managers contend on their shared caches: launch-time setup is
    // serial, and a mid-run re-run must not overlap a sibling's. WAITING for
    // a sibling keeps our leases alive — a sibling's slow install must not
    // let this worker's issue lapse to another claimant.
    static SETUP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let mut last_beat = Instant::now();
    let _serial = loop {
        match SETUP_LOCK.try_lock() {
            Ok(g) => break g,
            Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                if last_beat.elapsed() >= opts.heartbeat_interval {
                    heartbeat();
                    last_beat = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    };
    // Supervised, with a wall clock: a stalled registry must not hang the
    // worker (or, at launch, the whole fleet) forever.
    let run_opts = AgentRunOpts {
        timeout: opts.timeout,
        heartbeat_interval: opts.heartbeat_interval,
        log_path: opts.log_path.clone(),
        env: Vec::new(),
    };
    let tail = |out: &CmdOutput| {
        let from_log = opts
            .log_path
            .as_ref()
            // Lossy: one non-UTF-8 byte in an installer's output must not
            // drop the whole tail.
            .and_then(|p| std::fs::read(p).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let (o, e) = if out.stdout.is_empty() && out.stderr.is_empty() {
            (from_log.as_str(), "")
        } else {
            (out.stdout.as_str(), out.stderr.as_str())
        };
        tail_of(&last_lines(o, e, ERROR_TAIL_LINES))
    };
    match runner.run_agent(
        &shell.program,
        &[shell.flag.as_str(), cmd],
        &run_opts,
        heartbeat,
    ) {
        Ok(AgentOutcome::Exited(out)) if out.success() => Ok(()),
        Ok(AgentOutcome::Exited(out)) => {
            let code = out
                .code
                .map_or_else(|| "on a signal".to_string(), |c| format!("with code {c}"));
            Err(format!(
                "worktree setup `{cmd}` exited {code}:\n{}",
                tail(&out)
            ))
        }
        Ok(AgentOutcome::TimedOut { output, .. }) => Err(format!(
            "worktree setup `{cmd}` timed out after {}s (killed; worktree.setup_timeout_secs):\n{}",
            opts.timeout.as_secs(),
            tail(&output)
        )),
        Err(e) => Err(format!(
            "worktree setup `{cmd}` could not start (shell `{}`): {e}",
            shell.describe()
        )),
    }
}

/// How [`run_setup`] runs: its wall clock, how often to renew leases while
/// it waits or runs, and where its output goes (the failure tail is read
/// back from there — a supervised run streams to the log, not to memory).
#[derive(Debug, Clone)]
pub struct SetupOpts {
    pub timeout: std::time::Duration,
    pub heartbeat_interval: std::time::Duration,
    pub log_path: Option<std::path::PathBuf>,
}

/// The lockfiles whose content a worktree's installed dependencies follow:
/// when an iteration's base changes any of them, setup runs again (SIRF-50).
pub const SETUP_LOCKFILES: [&str; 6] = [
    "bun.lock",
    "bun.lockb",
    "pnpm-lock.yaml",
    "yarn.lock",
    "package-lock.json",
    "uv.lock",
];

/// The lockfiles in the WORKING TREE (`<name> <blob id>` per lockfile
/// present) — what a setup run installs from, whether committed or not (an
/// env-fault re-run installs from the agent's tree). Empty when none exists
/// (or this is not a git worktree).
pub fn setup_fingerprint(runner: &dyn Runner) -> String {
    let top = match crate::gitrange::run_git(runner, &["rev-parse", "--show-toplevel"]) {
        Ok(o) if !o.stdout.trim().is_empty() => std::path::PathBuf::from(o.stdout.trim()),
        _ => return String::new(),
    };
    let present: Vec<&str> = SETUP_LOCKFILES
        .iter()
        .copied()
        .filter(|f| top.join(f).is_file())
        .collect();
    if present.is_empty() {
        return String::new();
    }
    let mut args = vec!["hash-object", "--"];
    args.extend(&present);
    match crate::gitrange::run_git(runner, &args) {
        Ok(o) => present
            .iter()
            .zip(o.stdout.lines())
            .map(|(f, h)| format!("{f} {}", h.trim()))
            .collect::<Vec<_>>()
            .join("\n"),
        // Unreadable: a value no stamp ever equals — stale, set up again.
        Err(_) => "unreadable".into(),
    }
}

/// Where a worktree records the fingerprint its dependencies were installed
/// from: inside its own git dir, so it is never a file the agent's work could
/// commit. `None` when the git dir cannot be resolved.
pub fn setup_stamp_path(runner: &dyn Runner) -> Option<std::path::PathBuf> {
    crate::gitrange::run_git(runner, &["rev-parse", "--absolute-git-dir"])
        .ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|d| !d.is_empty())
        .map(|d| std::path::PathBuf::from(d).join("sirius-setup-stamp"))
}

/// Separates the two fingerprints in a stamp.
const STAMP_SPLIT: &str = "\n--- after setup ---\n";

/// Record that setup just ran: `installed_from` is [`setup_fingerprint`]
/// taken BEFORE it ran (what it installed from), and the fingerprint now
/// is what it left behind — a setup command that rewrites the lockfile
/// (a non-frozen `npm install`) must not read as "the agent changed it", nor
/// make every later reset look stale.
pub fn write_setup_stamp(runner: &dyn Runner, installed_from: &str) {
    if let Some(p) = setup_stamp_path(runner) {
        let after = setup_fingerprint(runner);
        let _ = std::fs::write(p, format!("{installed_from}{STAMP_SPLIT}{after}"));
    }
}

/// (installed from, left behind). A stamp of one value (older sirius) is
/// both.
fn read_stamp(p: &std::path::Path) -> Option<(String, String)> {
    let t = std::fs::read_to_string(p).ok()?;
    Some(match t.split_once(STAMP_SPLIT) {
        Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
        None => (t.trim().to_string(), t.trim().to_string()),
    })
}

/// What a dirtied stamp holds: equal to no fingerprint, so the next
/// [`setup_is_stale`] check is stale.
const STAMP_DIRTY: &str = "dirty: the lockfiles changed in the worktree after setup";

/// Called BEFORE an iteration's reset: when the working tree's lockfiles
/// match neither what setup installed from nor what it left behind — an
/// agent ran `bun add` / `npm install`, so the installed dependencies follow
/// ITS lockfile — the stamp is dirtied. The reset puts the base's lockfile
/// back but not the dependencies, so the next issue must set up again.
/// `true` when it dirtied the stamp.
pub fn mark_setup_dirty_if_changed(runner: &dyn Runner) -> bool {
    let Some(p) = setup_stamp_path(runner) else {
        return false;
    };
    match read_stamp(&p) {
        Some((from, after)) => {
            let now = setup_fingerprint(runner);
            now != from && now != after && std::fs::write(&p, STAMP_DIRTY).is_ok()
        }
        None => false,
    }
}

/// True when the working tree's lockfiles (after an iteration's reset: the
/// base's) differ from those setup last installed from — a fresh base
/// (SIRF-42) that bumped a dependency, or an earlier agent that installed
/// its own ([`mark_setup_dirty_if_changed`]). `false` when no stamp can be
/// read (nothing to compare against: the env-fault re-gate still covers a
/// missing module).
pub fn setup_is_stale(runner: &dyn Runner) -> bool {
    let Some(p) = setup_stamp_path(runner) else {
        return false;
    };
    match read_stamp(&p) {
        Some((from, _)) => from != setup_fingerprint(runner),
        None => false,
    }
}

// ── CLI orchestration ────────────────────────────────────────────────────────

/// Run the gate for an issue: resolve changed files (git range), evaluate, then
/// advance on pass / comment on fail. `range` defaults to working-tree vs HEAD.
///
/// SF-11 exit-code note: an UNCONFIGURED gate and a genuinely failing gate both
/// still surface as `sirius gate` exit 3. That is deliberate. Exit 3 is
/// published as "gate blocked" in CONTRACTS §2 and in the sirius worker Skill,
/// and the worker loop keys its release-without-advancing path on it; minting a
/// fourth code would silently reclassify the unconfigured case as an unhandled
/// failure in every consumer that only knows 0/1/2/3. The distinction is
/// carried instead by [`GateOutcome::reason_code`] (stable) and
/// [`GateOutcome::structural`] — machine-readable, additive, and impossible to
/// misread as "the tests failed".
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
                    "reason": v.reason, "reason_code": v.reason_code,
                    "tests_run": v.tests_run, "advanced_to": target_status
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
            reason_code: v.reason_code,
            reason: v.reason,
            structural: v.structural,
            ran_tests: v.ran_tests,
        })
    } else {
        // Name the CODE in the comment too: a reader of the issue thread must
        // be able to tell "the suite went red" from "this workspace was never
        // wired to run a suite" without decoding the plan label.
        //
        // SIRF-48: the comment used to carry only the FIRST of the kept output
        // lines — usually a progress line, never the failure. It now carries
        // the output tail (the failing tests and the runner's summary).
        let tail = error_tail(&v);
        let mut body = format!(
            "sirius gate FAILED (tier {tier}, plan {}, reason {}): {}.",
            v.plan, v.reason_code, v.reason,
        );
        if !tail.is_empty() {
            body.push_str("\nOutput tail:\n");
            body.push_str(&fenced(&tail));
        }
        if let Some(line) = &v.env_fault {
            body.push_str(&format!(
                "\nThis looks like a missing dependency in the environment, not the code: `{}`",
                line.replace('`', "'")
            ));
        }
        let comment_filed = amt.comment(issue, &body).is_ok();
        ledger
            .log_policy_event(
                None,
                "gate_tier",
                &serde_json::json!({
                    "issue": issue, "tier": tier, "result": "fail", "plan": v.plan,
                    "reason": v.reason, "reason_code": v.reason_code,
                    "structural": v.structural, "tests_run": v.tests_run,
                    "error_tail": tail, "env_fault": v.env_fault.is_some()
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
            reason_code: v.reason_code,
            reason: v.reason,
            structural: v.structural,
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

/// Minimal quoting for a test id appended to `gate.test_cmd`: POSIX single
/// quotes for `sh -c`; for `cmd.exe` (no POSIX sh on a Windows box), see
/// [`cmd_quote`] — cmd keeps single quotes literally, so a pytest `test_x[a b]`
/// id would reach the runner as `'test_x[a` + `b]'` and match nothing.
fn shell_quote(s: &str, posix: bool) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '='))
    {
        s.to_string()
    } else if posix {
        format!("'{}'", s.replace('\'', r"'\''"))
    } else {
        cmd_quote(s)
    }
}

/// Quote `s` as ONE argument for a program launched by `cmd.exe`, in two
/// layers. Inner: the MSVC argv rules the test runner itself parses (wrap in
/// `"`, escape an embedded `"` as `\"`, double backslashes that precede a
/// quote). Outer: caret-escape every character cmd would act on — including
/// the quotes themselves, because cmd toggles its quote state on each `"` and
/// ignores the backslash, so an id like `a "b & c"` would otherwise put the `&`
/// outside cmd's quotes and split the command. `^%` likewise stops `%VAR%`
/// expansion. cmd strips the carets and hands the inner layer through intact.
fn cmd_quote(s: &str) -> String {
    let mut msvc = String::from("\"");
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                msvc.push_str(&"\\".repeat(backslashes * 2 + 1));
                msvc.push('"');
                backslashes = 0;
            }
            _ => {
                msvc.push_str(&"\\".repeat(backslashes));
                msvc.push(c);
                backslashes = 0;
            }
        }
    }
    msvc.push_str(&"\\".repeat(backslashes * 2));
    msvc.push('"');
    let mut out = String::with_capacity(msvc.len() * 2);
    for c in msvc.chars() {
        if matches!(c, '^' | '&' | '|' | '<' | '>' | '(' | ')' | '%' | '!' | '"') {
            out.push('^');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};

    /// A FIXED shell for tests. `resolve_shell()` answers differently on a
    /// Linux box, a Git-Bash box, and a bare-PowerShell box (SF-15); pinning it
    /// here keeps every gate assertion about the GATE rather than about which
    /// shell the developer happens to have installed.
    fn test_shell() -> ShellCmd {
        ShellCmd::posix_sh()
    }

    #[test]
    fn blank_test_cmd_fails_closed_like_an_unset_one() {
        let m = MockRunner::new();
        let v = run_cmd(&m, &test_shell(), Some("   "), &[], "full", "x".into());
        assert!(!v.passed && v.structural, "{v:?}");
        assert_eq!(v.reason_code, reason_code::UNCONFIGURED_TEST_CMD);
        assert!(
            m.recorded().is_empty(),
            "nothing may run: {:?}",
            m.recorded()
        );
    }

    #[test]
    fn test_ids_are_quoted_for_the_resolved_shell() {
        assert_eq!(shell_quote("tests/a.py::t", true), "tests/a.py::t");
        assert_eq!(shell_quote("t[a b]", true), "'t[a b]'");
        assert_eq!(shell_quote("it's", true), r"'it'\''s'");
        // cmd.exe: MSVC-quoted, then every cmd metachar (quotes too) careted.
        assert_eq!(shell_quote("t[a b]", false), r#"^"t[a b]^""#);
        assert_eq!(shell_quote(r#"a "b & c""#, false), r#"^"a \^"b ^& c\^"^""#);
        assert_eq!(shell_quote("t[%PATH%]", false), r#"^"t[^%PATH^%]^""#);
        // A trailing backslash must not escape the closing quote.
        assert_eq!(shell_quote(r"dir\", false), r#"^"dir\\^""#);
    }

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

    // A partially-mapped change set (roots < changed files) means the unmapped
    // files contributed NOTHING to the selection — that is doubt, never a
    // trusted narrow run. `roots > 0` used to bless a 1-of-2 mapping.
    #[test]
    fn partial_root_mapping_is_doubt() {
        let sel = Selection {
            ok: true,
            roots: 1,
            runnables: vec!["t::a".into()],
            ..Default::default()
        };
        let changed = vec!["src/a.rs".to_string(), "queries/report.sql".to_string()];
        match decide_plan(&sel, &changed, GateFallback::FullSuite) {
            GatePlan::Full(reason) => assert!(reason.contains("only 1 of 2"), "{reason}"),
            other => panic!("partial mapping must not run a narrow subset: {other:?}"),
        }
    }

    // A dash-leading runnable id from the daemon would reach the test runner
    // as a FLAG (`pytest -q --co` exits 0 running nothing) — never trust one.
    #[test]
    fn flag_like_runnable_id_is_doubt() {
        let sel = Selection {
            ok: true,
            roots: 1,
            runnables: vec!["--co".into()],
            ..Default::default()
        };
        let changed = vec!["src/a.py".to_string()];
        match decide_plan(&sel, &changed, GateFallback::FullSuite) {
            GatePlan::Full(reason) => assert!(reason.contains("flag-like"), "{reason}"),
            other => panic!("flag-like id must be doubt: {other:?}"),
        }
    }

    // SIRF-11: when the changed-file set is UNKNOWN (git failed), the gate goes
    // straight to the fallback policy — never a skip, never a narrow subset.
    #[test]
    fn evaluate_doubt_fails_closed_per_policy() {
        // full-suite fallback: the suite actually runs and its verdict rules.
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "ok");
        let g = GateConfig {
            test_cmd: Some("run-suite".into()),
            fallback: GateFallback::FullSuite,
        };
        let v = evaluate_doubt(&m, &g, "cannot determine changed files: boom");
        assert!(v.ran_tests && v.passed);
        assert_eq!(v.plan, "full-suite");

        // fail fallback: blocked without running anything.
        let g2 = GateConfig {
            test_cmd: Some("run-suite".into()),
            fallback: GateFallback::Fail,
        };
        let v2 = evaluate_doubt(&MockRunner::new(), &g2, "boom");
        assert!(!v2.passed && !v2.ran_tests);

        // unconfigured test_cmd under full-suite doubt: fail-closed.
        let g3 = GateConfig {
            test_cmd: None,
            fallback: GateFallback::FullSuite,
        };
        let v3 = evaluate_doubt(&MockRunner::new(), &g3, "boom");
        assert!(!v3.passed);
        assert_eq!(v3.plan, "unconfigured");
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
        let v = execute_plan(
            &m,
            &test_shell(),
            Some("cargo test"),
            GatePlan::Full("doubt".into()),
        );
        assert!(v.passed);
        assert!(v.ran_tests);
        assert_eq!(v.plan, "full-suite");
        // The full suite command was run, verbatim, with no selected ids.
        assert_eq!(m.recorded()[0], "/bin/sh -c cargo test");
    }

    #[test]
    fn execute_full_suite_fails_when_tests_fail() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["sh", "-c"], 101, "", "test failed"));
        let v = execute_plan(
            &m,
            &test_shell(),
            Some("cargo test"),
            GatePlan::Full("doubt".into()),
        );
        assert!(!v.passed);
        assert!(v.ran_tests);
    }

    #[test]
    fn execute_subset_appends_selected_ids() {
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "ok");
        let v = execute_plan(
            &m,
            &test_shell(),
            Some("pytest -q"),
            GatePlan::Subset(vec!["tests/test_x.py::test_a".into()], "1".into()),
        );
        assert!(v.passed);
        assert_eq!(v.tests_run, 1);
        assert_eq!(
            m.recorded()[0],
            "/bin/sh -c pytest -q tests/test_x.py::test_a"
        );
    }

    #[test]
    fn execute_without_test_cmd_fails_closed() {
        let m = MockRunner::new();
        let v = execute_plan(&m, &test_shell(), None, GatePlan::Full("doubt".into()));
        assert!(!v.passed);
        assert!(!v.ran_tests);
        assert_eq!(v.plan, "unconfigured");
        // No command was run.
        assert_eq!(m.call_count(), 0);
    }

    // SF-11: "this workspace cannot run tests" and "your tests failed" are both
    // exit 3 on purpose (see run_gate). They must NEVER be the same reason_code,
    // and the unconfigured one must not read like a test failure.
    #[test]
    fn unconfigured_and_failing_gates_carry_different_reason_codes() {
        let unconfigured = execute_plan(
            &MockRunner::new(),
            &test_shell(),
            None,
            GatePlan::Full("doubt".into()),
        );
        let m = MockRunner::new();
        m.push(MockResponse::new(&["sh", "-c"], 101, "", "1 failed"));
        let failed = execute_plan(
            &m,
            &test_shell(),
            Some("cargo test"),
            GatePlan::Full("doubt".into()),
        );

        assert!(!unconfigured.passed && !failed.passed, "both are failures");
        assert_eq!(unconfigured.reason_code, reason_code::UNCONFIGURED_TEST_CMD);
        assert_eq!(failed.reason_code, reason_code::TESTS_FAILED);
        assert_ne!(unconfigured.reason_code, failed.reason_code);
        // Only the unconfigured one is structural (no retry can fix it).
        assert!(unconfigured.structural);
        assert!(!failed.structural);
        // …and it says so in words, with the remedy.
        assert!(
            unconfigured.reason.contains("no test failure occurred"),
            "{}",
            unconfigured.reason
        );
        assert!(
            unconfigured.reason.contains("gate.test_cmd"),
            "{}",
            unconfigured.reason
        );
    }

    // The other two policy verdicts get their own codes so a script never has
    // to infer policy from the human plan label.
    #[test]
    fn policy_verdicts_carry_their_own_reason_codes() {
        let m = MockRunner::new();
        let blocked = execute_plan(&m, &test_shell(), Some("x"), GatePlan::Block("d".into()));
        assert_eq!(blocked.reason_code, reason_code::BLOCKED_BY_POLICY);
        assert!(!blocked.passed && !blocked.structural);
        let warned = execute_plan(&m, &test_shell(), Some("x"), GatePlan::WarnPass("d".into()));
        assert_eq!(warned.reason_code, reason_code::PASSED_WITHOUT_TESTS);
        assert!(warned.passed && !warned.ran_tests);
    }

    // SF-15: the shell failing to START is not a test failure. The reporter
    // whose gate died with a bare "program not found" went looking for a
    // missing test binary; the verdict must say it was the shell, and name it.
    #[test]
    fn shell_that_cannot_start_is_not_reported_as_a_test_failure() {
        struct NoShell;
        impl Runner for NoShell {
            fn run(&self, _p: &str, _a: &[&str]) -> std::io::Result<crate::shell::CmdOutput> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "program not found",
                ))
            }
        }
        let v = execute_plan(
            &NoShell,
            &test_shell(),
            Some("cargo test --workspace && bun run check"),
            GatePlan::Full("doubt".into()),
        );
        assert!(!v.passed);
        assert!(!v.ran_tests, "nothing ran — the shell never started");
        assert_eq!(v.reason_code, reason_code::SHELL_SPAWN_FAILED);
        assert!(
            v.detail.contains("/bin/sh"),
            "must name the shell: {}",
            v.detail
        );
        assert!(
            v.detail.contains("not your test binary"),
            "must not read as a missing test binary: {}",
            v.detail
        );
    }

    // The compound command from the SF-15 report reaches the shell as ONE
    // script string — that is what makes `&&` an operator instead of an argv
    // element the test binary would choke on.
    #[test]
    fn compound_test_cmd_is_handed_to_the_shell_as_one_script() {
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "ok");
        let v = execute_plan(
            &m,
            &test_shell(),
            Some("cargo test --workspace && bun run check"),
            GatePlan::Full("doubt".into()),
        );
        assert!(v.passed);
        assert_eq!(
            m.recorded()[0],
            "/bin/sh -c cargo test --workspace && bun run check"
        );
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
        assert_eq!(o.reason_code, reason_code::UNCONFIGURED_TEST_CMD);
        assert!(o.structural, "no retry can fix an absent test_cmd");
        assert!(!o.ran_tests);
        assert!(o.advanced_to.is_none());
        // The issue comment must carry the code, so a human reading the thread
        // sees "never configured", not "the suite went red".
        let comment = m
            .recorded()
            .into_iter()
            .find(|c| c.contains("issue comment"))
            .expect("a fail comment is filed");
        assert!(
            comment.contains(reason_code::UNCONFIGURED_TEST_CMD),
            "{comment}"
        );
    }

    // ---- SIRF-48 / SIRF-50 failure diagnostics ---------------------------

    fn verdict_with_detail(detail: &str) -> GateVerdict {
        GateVerdict {
            passed: false,
            plan: "full-suite".into(),
            reason: "r".into(),
            ran_tests: true,
            tests_run: 0,
            test_ids: vec![],
            detail: detail.into(),
            reason_code: reason_code::TESTS_FAILED,
            structural: false,
            env_fault: None,
        }
    }

    #[test]
    fn error_tail_keeps_the_last_lines_and_caps_bytes() {
        let detail: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let tail = error_tail(&verdict_with_detail(&detail.join("\n")));
        assert_eq!(tail.lines().count(), ERROR_TAIL_LINES);
        assert!(
            tail.starts_with("line 6\n") && tail.ends_with("line 20"),
            "{tail}"
        );
        // Oversized lines: the END is kept, within the byte cap, starting on
        // a whole line and marked as cut.
        let long: Vec<String> = (0..15)
            .map(|i| format!("{i}:{}", "é".repeat(200)))
            .collect();
        let tail = error_tail(&verdict_with_detail(&long.join("\n")));
        assert!(tail.len() <= ERROR_TAIL_MAX_BYTES, "{}", tail.len());
        assert!(tail.starts_with('…'));
        assert!(tail.ends_with(&long[14]));
        let first = tail.trim_start_matches('…').lines().next().unwrap();
        assert!(long.iter().any(|l| l == first), "cut mid-line: {first}");
        // Nothing ran ⇒ nothing to show.
        assert_eq!(error_tail(&verdict_with_detail("")), "");
    }

    #[test]
    fn fence_outlasts_backticks_in_the_output() {
        assert_eq!(fenced("x"), "```text\nx\n```");
        assert_eq!(fenced("a ```` b"), "`````text\na ```` b\n`````");
    }

    #[test]
    fn env_fault_lines_are_dependency_misses_not_code_faults() {
        let f = |out: &str| env_fault_line(out, "", Some(1), "bun test");
        // The MainSpanX shapes, plus Node/Python/Rust equivalents.
        assert!(f("error: Cannot find module \"react\" from \"/w/src/a.ts\"").is_some());
        assert!(f("Error: Cannot find module 'clsx'").is_some());
        assert!(
            f("Error [ERR_MODULE_NOT_FOUND]: Cannot find package 'zod' imported from /w/x.js")
                .is_some()
        );
        assert!(f("ModuleNotFoundError: No module named 'requests'").is_some());
        assert!(f("error[E0463]: can't find crate for `serde`").is_some());
        // A relative/absolute PATH that does not resolve is the code's fault.
        assert_eq!(f("Error: Cannot find module './utlis'"), None);
        assert_eq!(
            f("Cannot find module '/w/src/gone.js' imported from /w/x.js"),
            None
        );
        // An ordinary failure is not an env fault.
        assert_eq!(f("expected 2, received 3\n 1 fail"), None);
        // The test runner itself missing (as the shell reports it)…
        assert!(env_fault_line("", "sh: bun: command not found", Some(127), "bun test").is_some());
        assert!(env_fault_line("", "/bin/sh: 1: bun: not found", Some(2), "bun test").is_some());
        // …or any command the shell could not find, when it exited 127
        // (`npm test` → `vitest` with no node_modules/.bin).
        assert!(
            env_fault_line("", "sh: vitest: command not found", Some(127), "npm test").is_some()
        );
        // …but a test's own "not found" output with an ordinary exit is not.
        assert_eq!(
            env_fault_line("user: not found", "", Some(1), "npm test"),
            None
        );
        // The runner name is matched as a whole word: `go` is not "logo".
        assert_eq!(
            env_fault_line("asset logo: not found", "", Some(1), "go test ./..."),
            None
        );
        assert!(env_fault_line("sh: go: not found", "", Some(1), "go test ./...").is_some());
        // A Windows absolute path is a path, not a package.
        assert_eq!(f(r"Error: Cannot find module 'C:\w\gone.js'"), None);
    }

    // run_cmd scans the FULL output (not just the kept 20-line detail) and
    // only flags failures.
    #[test]
    fn run_cmd_flags_env_faults_only_on_failure() {
        let mut out = String::from("error: Cannot find module \"react\"\n");
        for i in 0..40 {
            out.push_str(&format!("(pass) test {i}\n"));
        }
        let m = MockRunner::new();
        m.push(MockResponse::new(&["sh", "-c"], 1, &out, ""));
        let v = execute_plan(
            &m,
            &test_shell(),
            Some("bun test"),
            GatePlan::Full("d".into()),
        );
        assert!(!v.detail.contains("Cannot find module"), "outside the tail");
        assert_eq!(
            v.env_fault.as_deref(),
            Some("error: Cannot find module \"react\"")
        );
        let m = MockRunner::new();
        m.push(MockResponse::new(&["sh", "-c"], 0, &out, ""));
        let v = execute_plan(
            &m,
            &test_shell(),
            Some("bun test"),
            GatePlan::Full("d".into()),
        );
        assert!(v.passed && v.env_fault.is_none());
    }

    #[test]
    fn isolated_gate_reason_reads_as_a_note_not_a_cause() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["sh", "-c"], 1, "1 fail", ""));
        let v = evaluate_isolated(&m, &gate_cfg(Some("t"), GateFallback::FullSuite));
        assert_eq!(v.plan, "full-suite");
        assert_eq!(
            v.reason,
            "selection skipped in an isolated worktree (the code graph watches the main checkout) — ran the full suite"
        );
        let v = evaluate_isolated(&MockRunner::new(), &gate_cfg(Some("t"), GateFallback::Fail));
        assert!(
            v.reason.contains("gate.fallback=fail blocks"),
            "{}",
            v.reason
        );
    }

    #[test]
    fn loop_fail_comment_leads_with_the_tail_and_demotes_the_plan_note() {
        let v = verdict_with_detail("FAILED a::b\n1 failed");
        let c = loop_fail_comment(&v, Some("Environment fault suspected"));
        assert_eq!(
            c,
            "sirius: gate failed [full-suite, tests_failed] — output tail:\n```text\nFAILED a::b\n1 failed\n```\nPlan: r\nEnvironment fault suspected"
        );
        // No output (blocked/unconfigured): the reason IS the explanation.
        let mut blocked = verdict_with_detail("");
        blocked.plan = "blocked".into();
        blocked.reason_code = reason_code::BLOCKED_BY_POLICY;
        assert_eq!(
            loop_fail_comment(&blocked, None),
            "sirius: gate failed [blocked, blocked_by_policy]: r"
        );
    }

    fn setup_opts() -> SetupOpts {
        SetupOpts {
            timeout: std::time::Duration::from_secs(60),
            heartbeat_interval: std::time::Duration::from_secs(60),
            log_path: None,
        }
    }

    /// SIRF-50 × SIRF-42: the stamp tracks the lockfiles setup installed
    /// from; a base that changes one makes the worktree stale (real git).
    #[test]
    fn setup_goes_stale_when_the_base_changes_a_lockfile() {
        let dir = std::env::temp_dir().join(format!("sirius-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = crate::shell::RealRunner {
            cwd: Some(dir.clone()),
        };
        let git = |args: &[&str]| {
            let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
            full.extend(args);
            crate::gitrange::run_git(&r, &full).unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("bun.lock"), "react@18").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "a"]);
        assert!(!setup_is_stale(&r), "no stamp yet: nothing to compare");
        write_setup_stamp(&r, &setup_fingerprint(&r));
        assert!(!setup_is_stale(&r));
        std::fs::write(dir.join("README"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "unrelated"]);
        assert!(!setup_is_stale(&r), "a non-lockfile change is not stale");
        std::fs::write(dir.join("bun.lock"), "react@19").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "bump"]);
        assert!(setup_is_stale(&r), "a lockfile bump is stale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 3: an agent that installed its own dependency (lockfile changed
    /// in the tree, work not landed) leaves the next issue stale after the
    /// reset puts the base's lockfile back.
    #[test]
    fn an_agents_own_install_makes_the_next_iteration_stale() {
        let dir = std::env::temp_dir().join(format!("sirius-stamp2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = crate::shell::RealRunner {
            cwd: Some(dir.clone()),
        };
        let git = |args: &[&str]| {
            let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
            full.extend(args);
            crate::gitrange::run_git(&r, &full).unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("bun.lock"), "react@18").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "a"]);
        write_setup_stamp(&r, &setup_fingerprint(&r));
        assert!(
            !mark_setup_dirty_if_changed(&r),
            "unchanged: nothing to mark"
        );
        // The agent's `bun add` (uncommitted), then the next iteration's reset.
        std::fs::write(dir.join("bun.lock"), "react@18 zod@3").unwrap();
        assert!(mark_setup_dirty_if_changed(&r));
        git(&["reset", "-q", "--hard", "HEAD"]);
        assert!(setup_is_stale(&r), "set up again for the next issue");
        // The env-fault re-run installs from the working tree: stamp that.
        std::fs::write(dir.join("bun.lock"), "react@18 zod@3").unwrap();
        write_setup_stamp(&r, &setup_fingerprint(&r));
        assert!(!setup_is_stale(&r));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review: a setup command that REWRITES the lockfile (a non-frozen
    /// install) is not "stale" after every reset, nor "the agent's install".
    #[test]
    fn a_setup_that_rewrites_the_lockfile_is_not_stale_forever() {
        let dir = std::env::temp_dir().join(format!("sirius-stamp3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = crate::shell::RealRunner {
            cwd: Some(dir.clone()),
        };
        let git = |args: &[&str]| {
            let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
            full.extend(args);
            crate::gitrange::run_git(&r, &full).unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("package-lock.json"), "L0").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "a"]);
        let before = setup_fingerprint(&r);
        std::fs::write(dir.join("package-lock.json"), "L1").unwrap(); // setup rewrote it
        write_setup_stamp(&r, &before);
        assert!(!mark_setup_dirty_if_changed(&r), "setup's own rewrite");
        git(&["reset", "-q", "--hard", "HEAD"]);
        assert!(
            !setup_is_stale(&r),
            "the base's lockfile is what it installed from"
        );
        assert!(
            !mark_setup_dirty_if_changed(&r),
            "back at the base: unchanged"
        );
        std::fs::write(dir.join("package-lock.json"), "L2").unwrap(); // the agent's install
        assert!(mark_setup_dirty_if_changed(&r));
        git(&["reset", "-q", "--hard", "HEAD"]);
        assert!(setup_is_stale(&r));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 4: a non-UTF-8 byte in a failing setup's log keeps its tail.
    #[cfg(unix)]
    #[test]
    fn a_setup_tail_survives_a_non_utf8_byte() {
        let log =
            std::env::temp_dir().join(format!("sirius-setup-tail-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let opts = SetupOpts {
            timeout: std::time::Duration::from_secs(30),
            heartbeat_interval: std::time::Duration::from_secs(60),
            log_path: Some(log.clone()),
        };
        let sh = crate::shell::resolve_shell();
        let err = run_setup(
            &crate::shell::RealRunner::default(),
            &sh,
            "X=found; printf 'bad \\377 byte\\nerror: real-%s\\n' \"$X\"; exit 3",
            &opts,
            &mut || {},
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&log);
        // Only the OUTPUT says `real-found` (the command line does not).
        assert!(err.contains("real-found"), "{err}");
    }

    /// A stalled install is killed at the setup wall clock, not waited on.
    #[cfg(unix)]
    #[test]
    fn a_hung_setup_times_out() {
        let r = crate::shell::RealRunner::default();
        let opts = SetupOpts {
            timeout: std::time::Duration::from_secs(1),
            heartbeat_interval: std::time::Duration::from_secs(60),
            log_path: None,
        };
        let t0 = std::time::Instant::now();
        let e = run_setup(&r, &ShellCmd::posix_sh(), "sleep 30", &opts, &mut || {}).unwrap_err();
        assert!(e.contains("timed out after 1s"), "{e}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_nul_in_the_output_never_reaches_the_tail() {
        let t = tail_of("ok\nbad\0byte\x07here\n\tindented");
        assert!(!t.contains('\0') && !t.contains('\x07'), "{t:?}");
        assert!(
            t.contains("bad byte here") && t.contains("\tindented"),
            "{t:?}"
        );
    }

    #[test]
    fn run_setup_names_the_command_and_its_output_on_failure() {
        let m = MockRunner::new();
        m.expect(&["sh", "-c", "bun install --frozen-lockfile"], 0, "ok");
        assert!(run_setup(
            &m,
            &test_shell(),
            "bun install --frozen-lockfile",
            &setup_opts(),
            &mut || {}
        )
        .is_ok());
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["sh", "-c"],
            1,
            "",
            "error: lockfile had changes, but lockfile is frozen",
        ));
        let e = run_setup(
            &m,
            &test_shell(),
            "bun install --frozen-lockfile",
            &setup_opts(),
            &mut || {},
        )
        .unwrap_err();
        assert!(
            e.contains("`bun install --frozen-lockfile` exited with code 1"),
            "{e}"
        );
        assert!(e.contains("lockfile is frozen"), "{e}");
    }

    // SIRF-48: the CLI comment carries the output tail (it used to carry only
    // the first kept line), and the ledger's gate_tier event carries it too.
    #[test]
    fn run_gate_fail_comment_and_ledger_event_carry_the_tail() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"no traces yet — may UNDER-report","tests":[]}"#,
        );
        m.push(MockResponse::new(
            &["sh", "-c"],
            101,
            "running 40 tests\ntest db::fires ... FAILED\ntest result: FAILED. 1 failed",
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
        let comment = m
            .recorded()
            .into_iter()
            .find(|c| c.contains("issue comment"))
            .unwrap();
        assert!(comment.contains("```text\nrunning 40 tests\n"), "{comment}");
        assert!(comment.contains("test db::fires ... FAILED"), "{comment}");
        let detail: String = led
            .conn
            .query_row(
                "SELECT detail FROM policy_events WHERE kind = 'gate_tier'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let ev: serde_json::Value = serde_json::from_str(&detail).unwrap();
        assert!(ev["error_tail"]
            .as_str()
            .unwrap()
            .ends_with("test result: FAILED. 1 failed"));
        assert_eq!(ev["env_fault"], false);
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
