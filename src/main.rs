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
mod workspace;

use amt::Amt;
use bridge::LinkKind;
use clap::Parser;
use cli::{Cli, Command};
use config::Config;
use hayven::Hayven;
use ledger::Ledger;
use serde_json::{json, Value};
use shell::RealRunner;
use std::io::Write;
use std::process::ExitCode;
use workspace::Workspace;

const SIRIUS_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runner = RealRunner;
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
            json,
        } => cmd_gate(&ws, &runner, &issue, tier, target_status, json),
        Command::Run {
            workers,
            agent_cmd,
            from,
            max_iterations,
            json,
        } => cmd_run(
            &ws,
            &runner,
            workers,
            &agent_cmd,
            from,
            max_iterations,
            json,
        ),
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

fn cmd_doctor(ws: &Workspace, runner: &RealRunner, json: bool) -> u8 {
    let report = doctor::run(ws, runner);
    if json {
        let checks: Vec<Value> = report
            .checks
            .iter()
            .map(|c| json!({"name": c.name, "pass": c.pass, "detail": c.detail}))
            .collect();
        print_json(&json!({"ok": report.ok, "checks": checks}));
    } else {
        for c in &report.checks {
            println!(
                "[{}] {} — {}",
                if c.pass { "OK" } else { "FAIL" },
                c.name,
                c.detail
            );
        }
        println!(
            "{}",
            if report.ok {
                "all contract facts hold"
            } else {
                "CONTRACT DRIFT DETECTED"
            }
        );
    }
    if report.ok {
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

fn cmd_gate(
    ws: &Workspace,
    runner: &RealRunner,
    issue: &str,
    tier: Option<String>,
    target_status: Option<String>,
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

    match gate::run_gate(&amt, &hv, &ledger, issue, &tier, &target) {
        Ok(o) => {
            if json {
                print_json(&json!({
                    "ok": o.passed,
                    "issue": o.issue,
                    "tier": o.tier,
                    "gate": if o.passed { "pass" } else { "fail" },
                    "advanced_to": o.advanced_to,
                    "tests_selected": o.tests_selected,
                    "comment_filed": o.comment_filed
                }));
            } else {
                println!(
                    "gate {} for {}: {} ({} tests){}",
                    o.tier,
                    o.issue,
                    if o.passed { "PASS" } else { "FAIL" },
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
    _json: bool,
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
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();

    // v1 runs workers sequentially in one foreground process (a killable loop,
    // ROADMAP §"A daemon" rationale); concurrency cap bounds the roster names.
    let n = workers.max(1).min(cfg.worker_concurrency.max(1));
    let names: Vec<String> = tree_names(n);

    // Sanity: every phase name we emit is in the documented set (CONTRACTS §2).
    debug_assert!(run::PHASES.contains(&"claim") && run::PHASES.contains(&"release"));

    let mut iterations = 0u32;
    let mut idle_rounds = 0u32;
    let mut consecutive_overlaps = 0u32;
    loop {
        let mut any_work = false;
        for name in &names {
            if max_iterations > 0 && iterations >= max_iterations {
                let _ = lock.flush();
                return 0;
            }
            let outcome = run::run_iteration(
                &amt,
                &hv,
                &ledger,
                &cfg,
                runner,
                name,
                from.as_deref(),
                agent_cmd,
                &mut lock,
            );
            iterations += 1;
            match outcome {
                run::IterationOutcome::NoWork { .. } => {}
                run::IterationOutcome::ReleasedOverlap => {
                    // Contention backoff (config-driven, exponential + clamped).
                    let delay = cfg.backoff_delay_ms(consecutive_overlaps);
                    consecutive_overlaps = consecutive_overlaps.saturating_add(1);
                    ledger
                        .log_policy_event(
                            None,
                            "retry_budget",
                            &serde_json::json!({"backoff_ms": delay}),
                        )
                        .ok();
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    any_work = true;
                }
                run::IterationOutcome::Error(e) => {
                    eprint_err(&format!("{name}: {e}"));
                    any_work = true;
                }
                _ => {
                    consecutive_overlaps = 0;
                    any_work = true;
                }
            }
        }
        if any_work {
            idle_rounds = 0;
        } else {
            idle_rounds += 1;
            // No work across a full round of workers → stop (v1 foreground loop).
            if idle_rounds >= 1 {
                let _ = lock.flush();
                return 0;
            }
        }
    }
}

/// Worker tree names, deterministic and stable (PRD §4).
fn tree_names(n: u32) -> Vec<String> {
    const TREES: &[&str] = &[
        "oak", "rowan", "birch", "ash", "elm", "cedar", "maple", "pine",
    ];
    (0..n as usize)
        .map(|i| format!("sirius/{}", TREES.get(i).copied().unwrap_or("oak")))
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
    fn issue_ref_detection() {
        assert!(regex_is_issue("AMT-7"));
        assert!(!regex_is_issue("some::symbol"));
        assert!(!regex_is_issue("AMT-7-extra"));
    }
}
