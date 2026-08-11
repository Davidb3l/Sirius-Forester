//! Sirius Forester — `sirius` binary entry point.
//!
//! Exit codes (CONTRACTS §2): 0 ok, 1 operational failure, 2 usage error,
//! 3 gate/oracle "blocked" (soft). stdout carries the single `--json` object;
//! all logs go to stderr.

mod amt;
mod bridge;
mod cli;
mod config;
mod doctor;
mod gate;
mod gitrange;
mod hayven;
mod ledger;
mod run;
mod shell;
mod spine;
mod workspace;

use amt::Amt;
use bridge::LinkKind;
use clap::Parser;
use cli::{Cli, Command};
use config::Config;
use hayven::Hayven;
use ledger::Ledger;
use serde_json::{json, Value};
use shell::{RealRunner, Runner};
use std::io::Write;
use std::process::ExitCode;
use workspace::Workspace;

const SIRIUS_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runner = RealRunner::default();
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let ws = Workspace::discover(&cwd);

    let code = match cli.command {
        Command::Init { json } => cmd_init(&ws, json),
        Command::Doctor { json } => cmd_doctor(&ws, &runner, json),
        Command::Link {
            issue,
            decision,
            symbols,
            changed,
            range,
            json,
        } => cmd_link(&ws, &runner, issue, decision, symbols, changed, range, json),
        Command::Why { target, json } => cmd_why(&ws, &runner, &target, json),
        Command::Gate {
            issue,
            tier,
            target_status,
            range,
            json,
        } => cmd_gate(&ws, &runner, &issue, tier, target_status, range, json),
        Command::Run {
            workers,
            agent_cmd,
            from,
            max_iterations,
            json: _, // contract-compat no-op: run always streams NDJSON
        } => cmd_run(&ws, &runner, workers, &agent_cmd, from, max_iterations),
    };
    ExitCode::from(code)
}

/// Print a JSON object to stdout (the CONTRACTS §2 contract: one object, stdout).
fn print_json(v: &Value) {
    println!("{v}");
}

fn eprint_err(msg: &str) {
    eprintln!("sirius: {msg}");
}

fn load_config(ws: &Workspace) -> Result<Config, u8> {
    Config::load(&ws.config_path()).map_err(|e| {
        eprint_err(&e);
        1
    })
}

fn open_ledger(ws: &Workspace) -> Result<Ledger, u8> {
    let path = ws.ledger_path();
    if !path.exists() {
        eprint_err("no ledger found — run `sirius init` first");
        return Err(1);
    }
    Ledger::open(&path).map_err(|e| {
        eprint_err(&format!("cannot open ledger: {e}"));
        1
    })
}

// ---- init --------------------------------------------------------------

fn cmd_init(ws: &Workspace, json: bool) -> u8 {
    let dir = ws.sirius_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprint_err(&format!("cannot create {}: {e}", dir.display()));
        return 1;
    }
    // Self-ignoring .gitignore (PRD §3).
    if let Err(e) = std::fs::write(dir.join(".gitignore"), "*\n") {
        eprint_err(&format!("cannot write .sirius/.gitignore: {e}"));
        return 1;
    }
    // Committed-defaults config (M5), only if absent.
    let cfg_path = ws.config_path();
    if !cfg_path.exists() {
        if let Err(e) = std::fs::write(&cfg_path, Config::default_json()) {
            eprint_err(&format!("cannot write config.json: {e}"));
            return 1;
        }
    }
    let ledger_path = ws.ledger_path();
    match Ledger::create(&ledger_path, SIRIUS_VERSION) {
        Ok(_) => {
            let rel = ".sirius/sirius.db";
            if json {
                print_json(
                    &json!({"ok": true, "ledger": rel, "schema_version": ledger::SCHEMA_VERSION}),
                );
            } else {
                println!(
                    "initialized ledger at {} (schema v{})",
                    ledger_path.display(),
                    ledger::SCHEMA_VERSION
                );
            }
            0
        }
        Err(e) => {
            eprint_err(&format!("cannot create ledger: {e}"));
            1
        }
    }
}

// ---- doctor ------------------------------------------------------------

/// The Console's URL, honoring its port override (SUITE_CONTRACTS §3.2: a tool
/// reports the address its UI is *currently* served on, so a moved port is
/// reported correctly).
///
/// Only the namespaced `SIRIUS_CONSOLE_PORT` is read. Deliberately NOT the bare
/// `PORT`: doctor is usually spawned as a child (a suite hub probing peers), and
/// a parent that exports `PORT` for its own listener would otherwise make us
/// advertise the parent's port as our UI. §3.2 says a tool honors *its own*
/// override, not whatever generic variable happens to be in the environment.
fn console_ui_url() -> String {
    let port = std::env::var("SIRIUS_CONSOLE_PORT")
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(1777);
    format!("http://localhost:{port}")
}

fn cmd_doctor(ws: &Workspace, runner: &RealRunner, json: bool) -> u8 {
    let report = doctor::run(ws, runner);
    if json {
        // SUITE_CONTRACTS §3 envelope. `pass` is kept for existing consumers;
        // `ok` is the spec's name for the same bit (additive, not a rename).
        let checks: Vec<Value> = report
            .checks
            .iter()
            .map(|c| json!({"name": c.name, "ok": c.pass, "pass": c.pass, "detail": c.detail, "gating": c.gating}))
            .collect();
        print_json(&json!({
            "tool": "sirius",
            "version": env!("CARGO_PKG_VERSION"),
            "schemaVersion": 1,
            "ok": report.ok,
            "capabilities": ["ui"],
            "ui": console_ui_url(),
            "checks": checks,
        }));
    } else {
        for c in &report.checks {
            // An advisory (non-gating) failure is a WARN: worth fixing, but it
            // does not mean the contract facts drifted.
            let tag = match (c.pass, c.gating) {
                (true, _) => "OK",
                (false, true) => "FAIL",
                (false, false) => "WARN",
            };
            println!("[{tag}] {} — {}", c.name, c.detail);
        }
        let advisories_warned = report.checks.iter().any(|c| !c.pass && !c.gating);
        println!(
            "{}",
            match (report.ok, advisories_warned) {
                (true, false) => "all contract facts hold",
                (true, true) => "all contract facts hold (advisory warnings above)",
                (false, _) => "CONTRACT DRIFT DETECTED",
            }
        );
    }
    doctor_exit_code(json, report.ok)
}

/// Exit code for `doctor`, per SUITE_CONTRACTS §3/§3.1.
///
/// `--json` is the discovery handshake: a peer that exits non-zero is ABSENT
/// ("nothing trustworthy was said"), while exit 0 + a valid envelope carrying
/// `ok: false` is PRESENT-BUT-UNHEALTHY. Exiting 1 on a failing check would
/// make an installed-but-degraded sirius indistinguishable from an uninstalled
/// one, and the Suite Hub's amber row unreachable. Health lives in the `ok`
/// field; non-zero here is reserved for "no envelope could be produced at all".
///
/// Human mode keeps `ok ? 0 : 1` so `sirius doctor` remains a usable CI/shell
/// gate for contract drift (§4's operational exit codes).
fn doctor_exit_code(json: bool, ok: bool) -> u8 {
    if json || ok {
        0
    } else {
        1
    }
}

// ---- link --------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_link(
    ws: &Workspace,
    runner: &RealRunner,
    issue: Option<String>,
    decision: Option<String>,
    mut symbols: Vec<String>,
    changed: bool,
    range: Option<String>,
    json: bool,
) -> u8 {
    let ledger = match open_ledger(ws) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let amt = Amt::new(runner);
    let hv = Hayven::new(runner);

    let (kind, r#ref) = match (&issue, &decision) {
        (Some(i), None) => (LinkKind::Issue, i.clone()),
        (None, Some(d)) => (LinkKind::Decision, d.clone()),
        _ => {
            eprint_err("provide exactly one of <issue> or --decision <ref>");
            return 2;
        }
    };

    if changed {
        match gitrange::changed_symbols(runner, &hv, range.as_deref()) {
            Ok(mut s) => symbols.append(&mut s),
            Err(e) => {
                eprint_err(&format!("--changed resolution failed: {e}"));
                return 1;
            }
        }
    }
    // Dedup.
    symbols.dedup();
    let symbols: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        symbols
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect()
    };

    match bridge::link(&amt, &hv, &ledger, kind, &r#ref, &symbols, None) {
        Ok(r) => {
            // Spine (§2): the receipt is durably filed inside bridge::link above,
            // so this is a past-tense fact. Best-effort; never fails the command.
            {
                let is_issue = r.kind.as_str() == "issue";
                let extra_ref = if is_issue {
                    spine::issue_ref(&r.r#ref)
                } else {
                    format!("amt:decision/{}", r.r#ref.trim_start_matches("D-"))
                };
                let mut data = json!({ "symbols": r.symbols.clone() });
                if is_issue {
                    data["issue"] = json!(r.r#ref);
                } else {
                    data["decision"] = json!(r.r#ref);
                }
                spine::Spine::new(&ws.root).emit(
                    "receipt.filed",
                    vec![spine::receipt_ref(r.receipt_id), extra_ref],
                    data,
                );
            }
            if json {
                print_json(&json!({
                    "ok": true,
                    "receipt_id": r.receipt_id,
                    "kind": r.kind.as_str(),
                    "ref": r.r#ref,
                    "symbols": r.symbols,
                    "forward_ok": r.forward_ok,
                    "reverse_ok": r.reverse_ok
                }));
            } else {
                println!(
                    "linked {} {} → {} symbols (forward: {}, reverse: {})",
                    r.kind.as_str(),
                    r.r#ref,
                    r.symbols.len(),
                    r.forward_ok,
                    r.reverse_ok
                );
            }
            0
        }
        Err(e) => {
            eprint_err(&e);
            1
        }
    }
}

// ---- why ---------------------------------------------------------------

fn cmd_why(ws: &Workspace, runner: &RealRunner, target: &str, json: bool) -> u8 {
    // The ledger isn't strictly needed for why, but require a workspace.
    let _ = ws;
    let amt = Amt::new(runner);
    let hv = Hayven::new(runner);

    let is_issue = regex_is_issue(target);
    if is_issue {
        match bridge::why_issue(&amt, target) {
            Ok(w) => {
                if json {
                    print_json(
                        &json!({"ref": w.r#ref, "symbols": w.symbols, "decisions": w.decisions}),
                    );
                } else {
                    println!(
                        "{}: symbols {:?}, decisions {:?}",
                        w.r#ref, w.symbols, w.decisions
                    );
                }
                0
            }
            Err(e) => {
                eprint_err(&e);
                1
            }
        }
    } else {
        match bridge::why_symbol(&amt, &hv, target) {
            Ok(w) => {
                if json {
                    let issues: Vec<Value> = w
                        .issues
                        .iter()
                        .map(|(r, t)| json!({"ref": r, "title": t}))
                        .collect();
                    let decisions: Vec<Value> = w
                        .decisions
                        .iter()
                        .map(|(r, s)| json!({"ref": r, "summary": s}))
                        .collect();
                    print_json(
                        &json!({"symbol": w.symbol, "issues": issues, "decisions": decisions}),
                    );
                } else {
                    println!("{}:", w.symbol);
                    for (r, t) in &w.issues {
                        println!("  issue {r}: {t}");
                    }
                    for (r, s) in &w.decisions {
                        println!("  decision {r}: {s}");
                    }
                }
                0
            }
            Err(e) => {
                eprint_err(&e);
                1
            }
        }
    }
}

fn regex_is_issue(target: &str) -> bool {
    regex::Regex::new(r"^AMT-\d+$").unwrap().is_match(target)
}

// ---- gate --------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_gate(
    ws: &Workspace,
    runner: &RealRunner,
    issue: &str,
    tier: Option<String>,
    target_status: Option<String>,
    range: Option<String>,
    json: bool,
) -> u8 {
    let ledger = match open_ledger(ws) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let cfg = match load_config(ws) {
        Ok(c) => c,
        Err(c) => return c,
    };
    let amt = Amt::new(runner);
    let hv = Hayven::new(runner);
    let tier = tier.unwrap_or(cfg.gate_tier);
    let target = target_status.unwrap_or(cfg.target_status);

    match gate::run_gate(
        &amt,
        &hv,
        &ledger,
        runner,
        &cfg.gate,
        issue,
        &tier,
        &target,
        range.as_deref(),
    ) {
        Ok(o) => {
            // Spine (§2): amt status advance / fail comment already applied
            // inside run_gate. Best-effort; never fails the command.
            spine::Spine::new(&ws.root).emit(
                if o.passed {
                    "gate.passed"
                } else {
                    "gate.failed"
                },
                vec![spine::issue_ref(&o.issue)],
                json!({ "issue": o.issue.clone(), "tests": o.test_ids.clone() }),
            );
            if json {
                print_json(&json!({
                    "ok": o.passed,
                    "issue": o.issue,
                    "tier": o.tier,
                    "gate": if o.passed { "pass" } else { "fail" },
                    "plan": o.plan,
                    "ran_tests": o.ran_tests,
                    "advanced_to": o.advanced_to,
                    "tests_selected": o.tests_selected,
                    "comment_filed": o.comment_filed
                }));
            } else {
                println!(
                    "gate {} for {}: {} [{}] ({} tests){}",
                    o.tier,
                    o.issue,
                    if o.passed { "PASS" } else { "FAIL" },
                    o.plan,
                    o.tests_selected,
                    o.advanced_to
                        .as_ref()
                        .map(|s| format!(" → {s}"))
                        .unwrap_or_default()
                );
            }
            if o.passed {
                0
            } else {
                3 // soft "blocked" per CONTRACTS §2.
            }
        }
        Err(e) => {
            eprint_err(&e);
            1
        }
    }
}

// ---- run ---------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    ws: &Workspace,
    runner: &RealRunner,
    workers: u32,
    agent_cmd: &str,
    from: Option<String>,
    max_iterations: u32,
) -> u8 {
    // Validate the ledger up front for the friendly "run `sirius init` first"
    // message; workers open their OWN connections (rusqlite Connection is not
    // Sync — WAL + busy_timeout serialize the concurrent writers).
    match open_ledger(ws) {
        Ok(l) => drop(l),
        Err(c) => return c,
    }
    let cfg = match load_config(ws) {
        Ok(c) => c,
        Err(c) => return c,
    };
    let _ = runner; // workers construct their own (RealRunner is a unit type)
    let spine = spine::Spine::new(&ws.root);

    // Workers run as REAL parallel threads. The old v1 loop ran them
    // sequentially in one process, so "--workers 3" gave three roster names
    // and ZERO parallelism — a fleet whose agents run one at a time is slower
    // than any orchestrator that fans out, which defeated the point of a
    // foreman (field-observed: the fleet was killed for being slower than
    // hand-run subagents). Claim atomicity (amt), entity locks (hayven), and
    // per-worker ledger connections make concurrent iterations safe.
    let n = workers.max(1).min(cfg.worker_concurrency.max(1));
    let names: Vec<String> = tree_names(n);

    // Sanity: every phase name we emit is in the documented set (CONTRACTS §2).
    debug_assert!(run::PHASES.contains(&"claim") && run::PHASES.contains(&"release"));

    // An unconfigured gate fails EVERY iteration closed (by design), so the
    // loop would burn a full agent run per issue and advance NOTHING —
    // observed in the field. Refuse to start rather than letting the operator
    // discover it one expensive agent run at a time. (pass-with-warning is
    // the one fallback that can advance without a test_cmd.)
    if cfg.gate.test_cmd.is_none() && cfg.gate.fallback != config::GateFallback::PassWithWarning {
        eprint_err(
            "gate.test_cmd is not set — every gate would fail closed and no issue could \
             advance; set gate.test_cmd in .sirius/config.json (or gate.fallback to \
             \"pass-with-warning\" to advance ungated) and rerun",
        );
        return 1;
    }

    let iterations = std::sync::atomic::AtomicU32::new(0);
    let any_failed = std::sync::atomic::AtomicBool::new(false);
    let ledger_path = ws.ledger_path();
    std::thread::scope(|s| {
        for name in &names {
            s.spawn(|| {
                worker_loop(
                    name,
                    &ledger_path,
                    &cfg,
                    agent_cmd,
                    from.as_deref(),
                    max_iterations,
                    &iterations,
                    &any_failed,
                    &spine,
                );
            });
        }
    });
    u8::from(any_failed.load(std::sync::atomic::Ordering::SeqCst))
}

/// One worker's whole run: claim-and-work until the board is dry, the shared
/// iteration budget is spent, or the per-worker error budget trips.
#[allow(clippy::too_many_arguments)]
fn worker_loop(
    name: &str,
    ledger_path: &std::path::Path,
    cfg: &Config,
    agent_cmd: &str,
    from: Option<&str>,
    max_iterations: u32,
    iterations: &std::sync::atomic::AtomicU32,
    any_failed: &std::sync::atomic::AtomicBool,
    spine: &spine::Spine,
) {
    use std::sync::atomic::Ordering;
    const ERROR_BUDGET: u32 = 5;
    // How many consecutive "board momentarily empty" probes (retry_after set —
    // issues exist but are leased) a worker waits through before giving up.
    const NOWORK_PROBES: u32 = 3;
    const NOWORK_WAIT_CAP_SECS: u64 = 30;

    // amt/hayven speak to the REPO (process cwd); the agent, git, and the
    // gate's test run are scoped to this worker's PRIVATE worktree. Parallel
    // agents in one shared checkout would cross-contaminate every baseline
    // diff (worker A's gate would test worker B's half-written code) and race
    // on git's index.lock — isolation is what makes the parallel fleet sound.
    let repo_runner = RealRunner::default();
    let base = match crate::gitrange::head_rev(&repo_runner) {
        Ok(b) => b,
        Err(e) => {
            eprint_err(&format!("{name}: cannot resolve fleet base commit: {e}"));
            any_failed.store(true, Ordering::SeqCst);
            return;
        }
    };
    let wt_path = ledger_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("worktrees")
        .join(name.replace('/', "-"));
    let wt_str = wt_path.to_string_lossy().to_string();
    // Clear any stale worktree left by a killed run, then create fresh.
    let _ = repo_runner.run("git", &["worktree", "remove", "--force", &wt_str]);
    let _ = repo_runner.run("git", &["worktree", "prune"]);
    let _ = std::fs::remove_dir_all(&wt_path);
    match repo_runner.run("git", &["worktree", "add", "--detach", &wt_str, &base]) {
        Ok(o) if o.success() => {}
        other => {
            let detail = match other {
                Ok(o) => o.stderr.trim().to_string(),
                Err(e) => e.to_string(),
            };
            // NO silent fallback to the shared checkout — that would be the
            // unsound configuration this design exists to prevent.
            eprint_err(&format!(
                "{name}: cannot create worktree {wt_str}: {detail}"
            ));
            any_failed.store(true, Ordering::SeqCst);
            return;
        }
    }
    let agent_runner = RealRunner {
        cwd: Some(wt_path.clone()),
    };

    let ledger = match Ledger::open(ledger_path) {
        Ok(l) => l,
        Err(e) => {
            eprint_err(&format!("{name}: cannot open ledger: {e}"));
            any_failed.store(true, Ordering::SeqCst);
            return;
        }
    };
    let amt = Amt::new(&repo_runner);
    let hv = Hayven::new(&repo_runner);
    let mut out = StdoutLineWriter;
    let mut consecutive_overlaps = 0u32;
    let mut consecutive_errors = 0u32;
    let mut nowork_probes = 0u32;
    loop {
        // Reserve an iteration slot from the SHARED budget before claiming.
        if max_iterations > 0 && iterations.fetch_add(1, Ordering::SeqCst) >= max_iterations {
            break;
        }
        let outcome = run::run_iteration(
            &amt,
            &hv,
            &ledger,
            cfg,
            &agent_runner,
            name,
            from,
            agent_cmd,
            &mut out,
            Some(spine),
            Some(&base),
        );
        match outcome {
            run::IterationOutcome::NoWork { retry_after } => {
                // retry_after set means issues EXIST but are leased right now
                // (e.g. by sibling workers) — wait briefly and re-probe before
                // giving up, so the pool doesn't drain while work can still
                // come back to the board. A bare NoWork means truly dry: done.
                match retry_after {
                    Some(secs) if nowork_probes < NOWORK_PROBES => {
                        nowork_probes += 1;
                        std::thread::sleep(std::time::Duration::from_secs(
                            secs.min(NOWORK_WAIT_CAP_SECS),
                        ));
                    }
                    _ => break,
                }
            }
            run::IterationOutcome::ReleasedOverlap => {
                // Contention backoff (config-driven, exponential + clamped).
                let delay = cfg.backoff_delay_ms(consecutive_overlaps);
                consecutive_overlaps = consecutive_overlaps.saturating_add(1);
                consecutive_errors = 0;
                nowork_probes = 0;
                if let Err(e) = ledger.log_policy_event(
                    None,
                    "retry_budget",
                    &serde_json::json!({"backoff_ms": delay, "worker": name}),
                ) {
                    eprint_err(&format!("ledger write failed (log_policy_event): {e}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            run::IterationOutcome::Error(e) => {
                eprint_err(&format!("{name}: {e}"));
                consecutive_errors = consecutive_errors.saturating_add(1);
                nowork_probes = 0;
                if consecutive_errors >= ERROR_BUDGET {
                    eprint_err(&format!(
                        "{name}: {consecutive_errors} consecutive errors — stopping this worker (fix the cause and rerun)"
                    ));
                    any_failed.store(true, Ordering::SeqCst);
                    break;
                }
                // Same clamped backoff as contention: a persistent error must
                // not spin the loop hot.
                std::thread::sleep(std::time::Duration::from_millis(
                    cfg.backoff_delay_ms(consecutive_errors),
                ));
            }
            _ => {
                consecutive_overlaps = 0;
                consecutive_errors = 0;
                nowork_probes = 0;
            }
        }
    }
    // Tear the worktree down; completed work is safe — each issue's commits
    // live on its `sirius/<issue>` branch in the SHARED .git, and abandoned
    // failed-iteration leftovers are already recorded as deadends.
    let _ = repo_runner.run("git", &["worktree", "remove", "--force", &wt_str]);
}

/// `Write` adapter that locks stdout PER WRITE. `emit_event` sends each NDJSON
/// event as one `write_all`, so lines from parallel workers never interleave.
struct StdoutLineWriter;

impl Write for StdoutLineWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut h = std::io::stdout().lock();
        h.write_all(buf)?;
        h.flush()?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().lock().flush()
    }
}

/// Worker tree names, deterministic and stable (PRD §4).
fn tree_names(n: u32) -> Vec<String> {
    const TREES: &[&str] = &[
        "oak", "rowan", "birch", "ash", "elm", "cedar", "maple", "pine",
    ];
    (0..n as usize)
        .map(|i| match TREES.get(i) {
            Some(t) => format!("sirius/{t}"),
            // Past the named roster, stay UNIQUE — the old fallback named every
            // extra worker "sirius/oak", colliding with worker 1's identity in
            // amt claims, heartbeats, and releases.
            None => format!("sirius/tree{}", i + 1),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_names_are_stable() {
        assert_eq!(
            tree_names(3),
            vec!["sirius/oak", "sirius/rowan", "sirius/birch"]
        );
    }

    #[test]
    fn tree_names_stay_unique_past_the_roster() {
        // The old fallback named every worker past the 8-name roster
        // "sirius/oak" — a duplicate agent identity in claims/heartbeats.
        let names = tree_names(12);
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate worker names: {names:?}"
        );
    }

    #[test]
    fn issue_ref_detection() {
        assert!(regex_is_issue("AMT-7"));
        assert!(!regex_is_issue("some::symbol"));
        assert!(!regex_is_issue("AMT-7-extra"));
    }

    /// SUITE_CONTRACTS §3.1: under `--json`, an unhealthy-but-speaking tool
    /// MUST still exit 0 (present-but-unhealthy), or consumers classify it as
    /// absent and its failing checks are never shown. Human mode stays a gate.
    #[test]
    fn doctor_json_reports_health_in_the_envelope_not_the_exit_code() {
        assert_eq!(doctor_exit_code(true, true), 0);
        assert_eq!(
            doctor_exit_code(true, false),
            0,
            "§3.1 present-but-unhealthy"
        );
        assert_eq!(doctor_exit_code(false, true), 0);
        assert_eq!(
            doctor_exit_code(false, false),
            1,
            "human mode gates on drift"
        );
    }
}
