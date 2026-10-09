//! Sirius Forester — `sirius` binary entry point.
//!
//! Exit codes (CONTRACTS §2): 0 ok, 1 operational failure, 2 usage error,
//! 3 gate/oracle "blocked" (soft). stdout carries the single `--json` object;
//! all logs go to stderr.

mod amt;
mod bridge;
mod canary;
mod cli;
mod config;
mod doctor;
mod escape;
mod frontier;
mod gate;
mod gitrange;
mod hayven;
mod integrate;
mod ledger;
mod models;
mod review;
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
        Command::Escape {
            issue,
            kind,
            message,
            found_by,
            fix,
            automated_by,
            list,
            json,
        } => cmd_escape(
            &ws,
            &runner,
            EscapeArgs {
                issue,
                kind,
                message,
                found_by,
                fix,
                automated_by,
                list,
            },
            json,
        ),
        Command::ReviewCanary { n, json } => cmd_review_canary(&ws, &runner, n, json),
        Command::Integrate { clear_red, json } => cmd_integrate(&ws, &runner, clear_red, json),
        Command::Run {
            workers,
            agent_cmd,
            from,
            max_iterations,
            review_cmd,
            model,
            review_model,
            allow_default_model,
            json: _, // contract-compat no-op: run always streams NDJSON
        } => cmd_run(
            &ws,
            workers,
            &agent_cmd,
            from,
            max_iterations,
            review_cmd,
            RunModels {
                model,
                review_model,
                allow_default_model,
            },
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
        // SF-11: pre-fill a DETECTED test command instead of writing
        // `"test_cmd": null`. A null command makes the very first gate on the
        // new workspace fail closed — after an agent has already done real work
        // — and strands the issue in in_progress. Still emits null when nothing
        // is detectable; `sirius doctor` then fails loudly with the reason.
        if let Err(e) = std::fs::write(&cfg_path, Config::default_json_for_root(&ws.root)) {
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

    let mut changed_files: Option<usize> = None;
    // Only an iteration's own env: SIRIUS_BASE travels with SIRIUS_ISSUE
    // (run.rs base_env), never alone in a human shell.
    let fleet_base = std::env::var("SIRIUS_ISSUE")
        .ok()
        .and_then(|_| std::env::var("SIRIUS_BASE").ok());
    let resumed_from = std::env::var("SIRIUS_RESUMED_FROM").ok();
    let issue_env = std::env::var("SIRIUS_ISSUE").ok();
    let base_ref_env = std::env::var("SIRIUS_BASE_REF").ok();
    if changed {
        match gitrange::changed_symbols(
            runner,
            &hv,
            range.as_deref(),
            fleet_base.as_deref().map(|base| gitrange::FleetBase {
                base,
                resumed_from: resumed_from.as_deref(),
                issue: issue_env.as_deref(),
                base_ref: base_ref_env.as_deref(),
            }),
        ) {
            Ok(c) => {
                // The count is printed WITH its file count (SIRF-20) so an
                // over-broad stamp is visible. (No ratio heuristic: hayven's
                // roots are every entity in a changed file, so one large
                // file legitimately yields ~100.)
                changed_files = Some(c.files.len());
                symbols.extend(c.symbols);
            }
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
                    "reverse_ok": r.reverse_ok,
                    "changed_files": changed_files
                }));
            } else {
                println!(
                    "linked {} {} → {} symbols{} (forward: {}, reverse: {})",
                    r.kind.as_str(),
                    r.r#ref,
                    r.symbols.len(),
                    changed_files
                        .map(|n| format!(" from {n} changed file(s)"))
                        .unwrap_or_default(),
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
                // The review history (SIRF-23), when this repo has a ledger.
                let ledger = ws
                    .ledger_path()
                    .exists()
                    .then(|| Ledger::open(&ws.ledger_path()).ok())
                    .flatten();
                let rounds = ledger
                    .as_ref()
                    .and_then(|l| l.review_rounds_for_issue(target).ok())
                    .unwrap_or_default();
                // SIRF-35: what escaped this issue's review.
                let escapes = ledger
                    .as_ref()
                    .and_then(|l| l.escapes(Some(target)).ok())
                    .unwrap_or_default();
                if json {
                    let review: Vec<Value> = rounds
                        .iter()
                        .map(|r| {
                            json!({
                                "round": r.round, "result": r.result,
                                "confirmed": r.confirmed, "notes": r.notes,
                                "worker": r.worker, "at": r.created_at,
                                "findings": serde_json::from_str::<Value>(&r.findings).unwrap_or(Value::Null),
                            })
                        })
                        .collect();
                    print_json(
                        &json!({"ref": w.r#ref, "symbols": w.symbols, "decisions": w.decisions, "review": review,
                                "escapes": escapes.iter().map(|e| json!({"id": e.id, "kind": e.kind,
                                    "summary": e.summary, "found_by": e.found_by, "fix": e.fix_commit,
                                    "at": e.created_at})).collect::<Vec<_>>()}),
                    );
                } else {
                    println!(
                        "{}: symbols {:?}, decisions {:?}",
                        w.r#ref, w.symbols, w.decisions
                    );
                    for r in &rounds {
                        println!(
                            "  review round {} ({}): {} — {} confirmed, {} notes",
                            r.round,
                            r.worker.as_deref().unwrap_or("?"),
                            r.result,
                            r.confirmed,
                            r.notes
                        );
                    }
                    for e in &escapes {
                        println!("  ESCAPED [{}]: {} ({})", e.kind, e.summary, e.created_at);
                    }
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

/// An issue key: any amt workspace prefix (`AMT-7`, `GRA-12`, `SIRF-23`,
/// `BC9-1`), matching the issue-ref rule `extract_refs` uses. SIRF-15: this
/// was hard-coded to `^AMT-\d+$`, so every custom-prefix key fell through to
/// the SYMBOL path and came back as a silent empty success.
fn regex_is_issue(target: &str) -> bool {
    // amt prefixes are 1–16 alphanumerics starting with a letter (so `X-1`
    // is a real key); only `D-n` — a decision ref — is excluded.
    regex::Regex::new(r"^[A-Za-z][A-Za-z0-9]*-\d+$")
        .unwrap()
        .is_match(target)
        && !regex::Regex::new(r"^[Dd]-\d+$").unwrap().is_match(target)
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
                    // SF-11: exit 3 is shared with a genuinely blocked gate, so
                    // the machine-readable discriminator rides in the envelope
                    // rather than minting a fourth exit code that every 0/1/2/3
                    // consumer would misread.
                    "reason_code": o.reason_code,
                    "structural": o.structural,
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
                // SF-11: `FAIL [unconfigured] (0 tests)` reads as a test
                // failure and tells the operator nothing actionable. The one
                // case a human cannot act on without help gets the remedy
                // spelled out. stderr, so stdout consumers are unaffected.
                if o.reason_code == gate::reason_code::UNCONFIGURED_TEST_CMD {
                    eprintln!("{}", o.reason);
                }
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

// ---- escape + review-canary (SIRF-35) --------------------------------------

struct EscapeArgs {
    issue: Option<String>,
    kind: Option<String>,
    message: Option<String>,
    found_by: Option<String>,
    fix: Option<String>,
    automated_by: Option<String>,
    list: bool,
}

fn cmd_escape(ws: &Workspace, runner: &RealRunner, a: EscapeArgs, json: bool) -> u8 {
    let ledger = match open_ledger(ws) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let usage = |m: &str| {
        eprint_err(m);
        2
    };
    if a.list {
        let escapes = ledger.escapes(None).unwrap_or_default();
        let automated = ledger.automated_kinds().unwrap_or_default();
        let mut kinds: Vec<Value> = Vec::new();
        for e in &escapes {
            if kinds.iter().any(|k| k["kind"] == e.kind.as_str()) {
                continue;
            }
            let count = escapes.iter().filter(|x| x.kind == e.kind).count();
            let by = automated.iter().find(|(k, _)| k == &e.kind).map(|(_, b)| b);
            kinds.push(json!({"kind": e.kind, "count": count, "last_at": e.created_at, "automated_by": by}));
        }
        kinds.sort_by(|x, y| y["count"].as_u64().cmp(&x["count"].as_u64()));
        let rows: Vec<Value> = escapes
            .iter()
            .map(|e| {
                json!({"id": e.id, "issue": e.issue, "kind": e.kind, "summary": e.summary,
                            "found_by": e.found_by, "fix": e.fix_commit, "at": e.created_at})
            })
            .collect();
        if json {
            print_json(&json!({"kinds": kinds, "escapes": rows}));
        } else {
            for k in &kinds {
                println!(
                    "{:<24} {}×{}",
                    k["kind"].as_str().unwrap_or_default(),
                    k["count"],
                    k["automated_by"]
                        .as_str()
                        .map(|b| format!("  (automated by {b})"))
                        .unwrap_or_default()
                );
            }
        }
        return 0;
    }
    let Some(kind) = a.kind.as_deref() else {
        return usage("--kind is required (or --list)");
    };
    if let Err(e) = escape::validate_kind(kind) {
        return usage(&e);
    }
    if let Some(by) = a.automated_by.as_deref() {
        if a.issue.is_some() || a.message.is_some() {
            return usage("--automated-by retires a kind; pass only --kind and --automated-by");
        }
        // The check must exist: retiring a kind on a typo'd path would hide
        // it from every review with nothing catching it.
        if by.trim().is_empty() || !ws.root.join(by).exists() {
            return usage(&format!(
                "--automated-by `{by}` is not a file or directory in this repo"
            ));
        }
        if let Err(e) = ledger.set_kind_automated(kind, by) {
            eprint_err(&format!("cannot record: {e}"));
            return 1;
        }
        let n = ledger
            .escapes(None)
            .unwrap_or_default()
            .iter()
            .filter(|e| e.kind == kind)
            .count();
        if json {
            print_json(&json!({"ok": true, "kind": kind, "automated_by": by, "escapes": n}));
        } else {
            println!("{kind}: retired from the review prompt — automated by {by}");
        }
        return 0;
    }
    let (Some(issue), Some(message)) = (a.issue.as_deref(), a.message.as_deref()) else {
        return usage("recording an escape needs <ISSUE>, --kind and -m \"<what escaped>\"");
    };
    if !regex_is_issue(issue) {
        return usage(&format!("`{issue}` is not an issue key (PREFIX-n)"));
    }
    if message.trim().is_empty() {
        return usage("-m must say what escaped");
    }
    // The issue must exist; store its canonical key (why/list match exactly).
    let amt = Amt::new(runner);
    let issue = match amt.issue_show(issue) {
        Ok(v) => v["id"].as_str().unwrap_or(issue).to_string(),
        Err(e) => {
            eprint_err(&format!("no such issue `{issue}`: {e}"));
            return 1;
        }
    };
    let issue = issue.as_str();
    // A fix commit must resolve NOW — it is what a canary will revert.
    let fix = match a.fix.as_deref() {
        Some(f) => match gitrange::run_git(
            runner,
            &["rev-parse", "--verify", &format!("{f}^{{commit}}")],
        ) {
            Ok(o) => Some(o.stdout.trim().to_string()),
            Err(e) => {
                eprint_err(&format!("--fix `{f}` is not a commit: {e}"));
                return 1;
            }
        },
        None => None,
    };
    let id = match ledger.insert_escape(issue, kind, message, a.found_by.as_deref(), fix.as_deref())
    {
        Ok(id) => id,
        Err(e) => {
            eprint_err(&format!("cannot record the escape: {e}"));
            return 1;
        }
    };
    let kind_count = ledger
        .escapes(None)
        .unwrap_or_default()
        .iter()
        .filter(|e| e.kind == kind)
        .count();
    // A kind that escapes AGAIN after it was automated: its check missed
    // this one. Un-retire it — it goes back into every review prompt.
    let unretired = ledger
        .automated_kinds()
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == kind)
        .map(|(_, b)| b);
    if unretired.is_some() {
        if let Err(e) = ledger.unset_kind_automated(kind) {
            eprint_err(&format!("cannot un-retire `{kind}`: {e}"));
        }
    }
    let nudge = match &unretired {
        Some(by) => Some(format!(
            "`{kind}` was automated by {by}, but this one got past it — the kind is back in the review prompt; strengthen the check, then retire it again"
        )),
        None => escape::nudge(kind, kind_count, false),
    };
    let rounds = ledger.review_rounds_for_issue(issue).unwrap_or_default();
    // On the board, against the issue that shipped it, next to its review.
    let commented = amt.comment_as(
        issue,
        &format!(
            "sirius: ESCAPED DEFECT [{kind}] — {message}{}{} · this issue's review: {} round(s){}{}",
            a.found_by
                .as_deref()
                .map(|f| format!(" (found by {f})"))
                .unwrap_or_default(),
            fix.as_deref()
                .map(|f| format!(" · fixed in {f}"))
                .unwrap_or_default(),
            rounds.len(),
            rounds
                .last()
                .map(|r| format!(", last {}", r.result))
                .unwrap_or_default(),
            nudge.as_deref().map(|n| format!("\n\n{n}")).unwrap_or_default()
        ),
        "sirius",
    );
    if let Err(e) = commented {
        eprint_err(&format!(
            "warning: recorded, but the board comment on {issue} failed: {e}"
        ));
    }
    spine::Spine::new(&ws.root).emit(
        "escape.recorded",
        vec![spine::issue_ref(issue)],
        json!({ "kind": kind, "fix": fix, "found_by": a.found_by }),
    );
    if json {
        print_json(&json!({"ok": true, "id": id, "issue": issue, "kind": kind,
                           "kind_count": kind_count, "unretired": unretired, "nudge": nudge}));
    } else {
        println!("recorded escape #{id} [{kind}] against {issue}");
        if let Some(n) = nudge {
            println!("{n}");
        }
    }
    0
}

fn cmd_review_canary(ws: &Workspace, runner: &RealRunner, n: usize, json: bool) -> u8 {
    let ledger = match open_ledger(ws) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let cfg = match load_config(ws) {
        Ok(c) => c,
        Err(c) => return c,
    };
    let sirius_abs = {
        let d = ws.sirius_dir();
        if d.is_absolute() {
            d
        } else {
            std::env::current_dir().map(|c| c.join(&d)).unwrap_or(d)
        }
    };
    // The prompt a real review would get (override or built-in).
    let prompt = if cfg.review.cmd.is_some() {
        match load_review_prompt(ws, &cfg) {
            Ok(p) => p,
            Err(e) => {
                eprint_err(&e);
                return 1;
            }
        }
    } else {
        String::new()
    };
    if n == 0 {
        eprint_err("--n must be at least 1");
        return 2;
    }
    match canary::run(runner, &ledger, &cfg, &sirius_abs, &prompt, n) {
        Ok(r) => {
            if json {
                print_json(&json!(r));
            } else {
                for c in &r.canaries {
                    println!("{:<8} {:<28} {}", c.result, c.source, c.detail);
                }
                println!(
                    "recall {} ({} caught of {} reviewed + {} reviewer error(s); {} stale) · control: {} · reviewer model {}",
                    r.recall
                        .map(|x| format!("{:.0}%", x * 100.0))
                        .unwrap_or_else(|| "n/a".into()),
                    r.caught,
                    r.total,
                    r.errors,
                    r.stale,
                    r.false_positives
                        .map(|n| format!("{n} false positive(s)"))
                        .unwrap_or_else(|| "did not run".into()),
                    r.model.as_deref().unwrap_or("(default)")
                );
            }
            0
        }
        Err(e) => {
            eprint_err(&e);
            if json {
                print_json(&json!({"ok": false, "error": e}));
            }
            1
        }
    }
}

// ---- integrate ---------------------------------------------------------

fn cmd_integrate(ws: &Workspace, runner: &RealRunner, clear_red: bool, json: bool) -> u8 {
    let ledger = match open_ledger(ws) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let cfg = match load_config(ws) {
        Ok(c) => c,
        Err(c) => return c,
    };
    let amt = Amt::new(runner);
    let sirius_abs = {
        let d = ws.sirius_dir();
        if d.is_absolute() {
            d
        } else {
            std::env::current_dir().map(|c| c.join(&d)).unwrap_or(d)
        }
    };
    if clear_red {
        return match integrate::clear_red(&amt, &ledger, &sirius_abs) {
            Ok(issue) => {
                spine::Spine::new(&ws.root).emit(
                    "integration.cleared",
                    issue.iter().map(|i| spine::issue_ref(i)).collect(),
                    json!({ "by": "hand" }),
                );
                if json {
                    print_json(&json!({"ok": true, "cleared": true, "issue": issue}));
                } else {
                    println!("integration red state cleared by hand");
                }
                0
            }
            Err(e) => {
                eprint_err(&e);
                1
            }
        };
    }
    match integrate::integrate(runner, &amt, &ledger, &cfg, &sirius_abs) {
        Ok(r) => {
            let mut refs = vec![];
            if let Some(i) = &r.issue {
                refs.push(spine::issue_ref(i));
            }
            let (event, exit, label) = integrate::verdict(&r);
            spine::Spine::new(&ws.root).emit(
                event,
                refs,
                json!({ "frontier": r.frontier.clone(), "included": r.included.clone(), "exit": r.exit }),
            );
            if json {
                print_json(&json!(r));
            } else {
                println!(
                    "integration {} at {} ({} + {}){}{}",
                    label,
                    r.frontier,
                    r.base_ref,
                    if r.included.is_empty() {
                        "no siblings".to_string()
                    } else {
                        r.included.join(", ")
                    },
                    r.issue
                        .as_ref()
                        .map(|i| format!(" — filed/updated {i}"))
                        .unwrap_or_default(),
                    r.log
                        .as_ref()
                        .map(|l| format!(" — log {l}"))
                        .unwrap_or_default()
                );
            }
            exit
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
    workers: Option<u32>,
    agent_cmd: &str,
    from: Option<String>,
    max_iterations: u32,
    review_cmd: Option<String>,
    run_models: RunModels,
) -> u8 {
    // Validate the ledger up front for the friendly "run `sirius init` first"
    // message; workers open their OWN connections (rusqlite Connection is not
    // Sync — WAL + busy_timeout serialize the concurrent writers).
    match open_ledger(ws) {
        Ok(l) => drop(l),
        Err(c) => return c,
    }
    let mut cfg = match load_config(ws) {
        Ok(c) => c,
        Err(c) => return c,
    };
    // `--review-cmd` overrides `review.cmd` (SIRF-23); an empty string turns
    // a configured review stage off for this run.
    if let Some(rc) = review_cmd {
        cfg.review.cmd = Some(rc).filter(|c| !c.trim().is_empty());
    }
    // SIRF-26: the fleet's model is explicit, visible, and never a silent
    // inheritance of the global CLI default.
    let parent = std::env::var("SIRIUS_PARENT_MODEL").ok();
    let model_source = match models::resolve(
        &mut cfg.models,
        run_models.model.as_deref(),
        run_models.review_model.as_deref(),
        run_models.allow_default_model,
        parent.as_deref(),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprint_err(&e);
            return 2;
        }
    };
    if let Err(e) = check_models(ws, &cfg, agent_cmd) {
        eprint_err(&e);
        return 2;
    }
    // SF-14: every worker spawns --agent-cmd per claimed issue. When its
    // program is not on PATH no agent can ever start, and the fleet claims
    // issues only to release every one. That is the NORMAL state of a Claude
    // Code desktop or web session (no `claude` binary on PATH) — exactly where
    // the documented launch was being tried. Refuse up front and name the way
    // forward. A command too dynamic to read statically is let through
    // (`agent_program` returns None): a false "not found" would refuse a
    // perfectly good fleet.
    if let Some(prog) = shell::agent_program(agent_cmd) {
        if shell::find_program(&prog).is_none() {
            eprint_err(&format!(
                "--agent-cmd runs `{prog}`, which is not on PATH — no worker could start \
                 an agent, so the fleet would claim issues only to release every one. In a \
                 Claude Code desktop or web session there is usually no agent CLI on PATH: \
                 drive iterations by hand instead (the sirius skill's solo mode — claim → \
                 map → lock → brief → work → gate → receipt → release), or install the \
                 agent CLI and rerun"
            ));
            return 2;
        }
    }
    for m in models::named_models(&cfg.models) {
        if models::looks_like_alias(&m) {
            eprint_err(&format!(
                "warning: model `{m}` looks like an ALIAS — aliases resolve silently (on 2026-10-03 `fable[1m]` became claude-fable-5); prefer an explicit id"
            ));
        }
    }

    let spine = spine::Spine::new(&ws.root);

    // Workers run as REAL parallel threads. The old v1 loop ran them
    // sequentially in one process, so "--workers 3" gave three roster names
    // and ZERO parallelism — a fleet whose agents run one at a time is slower
    // than any orchestrator that fans out, which defeated the point of a
    // foreman (field-observed: the fleet was killed for being slower than
    // hand-run subagents). Claim atomicity (amt), entity locks (hayven), and
    // per-worker ledger connections make concurrent iterations safe.
    //
    // SIRF-50: an explicit `--workers N` WINS; `worker_concurrency` is only
    // the default when the flag is absent. It used to silently cap the flag
    // (`--workers 4` ran 3 and the 4th ticket waited).
    let workers = resolve_workers(workers, cfg.worker_concurrency);
    let names: Vec<String> = tree_names(workers.count);

    // Sanity: every phase name we emit is in the documented set (CONTRACTS §2).
    debug_assert!(run::PHASES.contains(&"claim") && run::PHASES.contains(&"release"));

    // An unconfigured gate fails EVERY iteration closed (by design), so the
    // loop would burn a full agent run per issue and advance NOTHING —
    // observed in the field. Refuse to start rather than letting the operator
    // discover it one expensive agent run at a time. (pass-with-warning is
    // the one fallback that can advance without a test_cmd.)
    let test_cmd_unset = cfg
        .gate
        .test_cmd
        .as_deref()
        .map_or(true, |c| c.trim().is_empty());
    if test_cmd_unset && cfg.gate.fallback != config::GateFallback::PassWithWarning {
        eprint_err(
            "gate.test_cmd is not set — every gate would fail closed and no issue could \
             advance; set gate.test_cmd in .sirius/config.json (or gate.fallback to \
             \"pass-with-warning\" to advance ungated) and rerun",
        );
        return 1;
    }

    // One fleet per repo: a second `sirius run` would force-remove the first
    // fleet's LIVE worktrees out from under its agents. A pidfile guard —
    // stale entries (dead pid) are taken over, a live one refuses.
    // SIRF-32: never launch onto a red frontier when the line is blocked.
    if let Ok(l) = Ledger::open(&ws.ledger_path()) {
        if let Some(why) = integrate::block_reason(&cfg, &l) {
            eprint_err(&format!("refusing to launch: {why}"));
            return 3;
        }
    }
    let lock_path = ws.sirius_dir().join("run.pid");
    if let Ok(prev) = std::fs::read_to_string(&lock_path) {
        let prev = prev.trim();
        if let Ok(pid) = prev.parse::<i32>() {
            let alive = libc_kill_probe(pid);
            if alive {
                eprint_err(&format!(
                    "another `sirius run` appears active in this repo (pid {pid}, {}) — one fleet per repo; stop it first or remove the file if it is stale",
                    lock_path.display()
                ));
                return 1;
            }
        }
    }
    if let Err(e) = std::fs::write(&lock_path, std::process::id().to_string()) {
        eprint_err(&format!("cannot write {}: {e}", lock_path.display()));
        return 1;
    }

    // Resolve the fleet base ONCE and build every worktree serially, before
    // any thread exists: concurrent `git worktree add/prune` contend on .git
    // admin locks (intermittent startup failures), and per-worker rev-parse
    // could hand workers divergent baselines if HEAD moved mid-startup.
    let repo_runner = RealRunner::default();
    let base = match gitrange::head_rev(&repo_runner) {
        Ok(b) => b,
        Err(e) => {
            eprint_err(&format!("cannot resolve fleet base commit: {e}"));
            let _ = std::fs::remove_file(&lock_path);
            return 1;
        }
    };
    // The branch HEAD pointed to at launch: the default review base for
    // `current-base-merge` (its CURRENT tip is what the review merges onto).
    let base_ref = repo_runner
        .run("git", &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout.trim().to_string())
        .filter(|r| !r.is_empty() && r != "HEAD");
    // Absolute paths: worktrees, review files, and the reviewer's cwd must
    // not depend on where each child process happens to run.
    let sirius_abs = {
        let d = ws.sirius_dir();
        if d.is_absolute() {
            d
        } else {
            std::env::current_dir().map(|c| c.join(&d)).unwrap_or(d)
        }
    };
    let review_prompt = match load_review_prompt(ws, &cfg) {
        Ok(p) => p,
        Err(e) => {
            eprint_err(&e);
            let _ = std::fs::remove_file(&lock_path);
            return 1;
        }
    };
    let worktrees_root = sirius_abs.join("worktrees");
    let _ = repo_runner.run("git", &["worktree", "prune"]);
    let mut assignments: Vec<(String, std::path::PathBuf)> = Vec::new();
    for name in &names {
        let wt_path = worktrees_root.join(name.replace('/', "-"));
        // NOT `to_string_lossy()`: the workspace root is canonicalized, so on
        // Windows this path carries the `\\?\` verbatim prefix, which git
        // rewrites to `//?/C:/...` and then cannot create. That failed every
        // worker at the worktree step — no fleet on Windows at all (SF-16).
        let wt_str = gitrange::git_path(&wt_path);
        // Clear any stale worktree left by a killed run, then create fresh.
        let _ = repo_runner.run("git", &["worktree", "remove", "--force", &wt_str]);
        let _ = std::fs::remove_dir_all(&wt_path);
        // NO silent fallback to the shared checkout — that would be the
        // unsound configuration the isolation design exists to prevent.
        if let Err(e) = gitrange::run_git(
            &repo_runner,
            &["worktree", "add", "--detach", &wt_str, &base],
        ) {
            eprint_err(&format!("{name}: cannot create worktree {wt_str}: {e}"));
            let _ = std::fs::remove_file(&lock_path);
            return 1;
        }
        assignments.push((name.clone(), wt_path));
    }

    // SIRF-50: a fresh worktree has the tracked files and nothing else — no
    // node_modules, no .venv — so a gate over it failed on the ENVIRONMENT.
    // Run the setup command in each worktree, serially (package managers
    // contend on their shared caches), before any agent starts. Detection
    // reads the first worktree: it IS the base commit's tree, so an untracked
    // lockfile in the main checkout cannot pick a command the worktrees can't
    // run. The iteration's reset is `git clean -fd` (no -x), which keeps
    // ignored dirs like node_modules; an iteration re-runs setup only when
    // its (fresh, SIRF-42) base changed a lockfile — see the stamp in run.rs.
    let setup = assignments
        .first()
        .and_then(|(_, wt)| cfg.worktree.resolve(wt));
    // Normalize to the EFFECTIVE command so the loop's env-fault re-gate
    // (run.rs) reads exactly what ran here: Some(cmd), or "" = none.
    cfg.worktree.setup_cmd = Some(setup.as_ref().map(|(c, _)| c.clone()).unwrap_or_default());
    let mut setup_failed: Vec<(String, String)> = Vec::new();
    if let Some((cmd, source)) = &setup {
        eprint_err(&setup_note(cmd, source));
        let sh = shell::resolve_shell();
        assignments.retain(|(name, wt_path)| {
            eprint_err(&format!("{name}: running worktree setup…"));
            let wt_runner = RealRunner {
                cwd: Some(wt_path.clone()),
            };
            let logs = ws.sirius_dir().join("logs");
            let _ = std::fs::create_dir_all(&logs);
            let opts = gate::SetupOpts {
                timeout: cfg.worktree.setup_timeout(),
                heartbeat_interval: std::time::Duration::from_secs(60),
                log_path: Some(logs.join(format!("setup-{name}.log"))),
            };
            match gate::run_setup(&wt_runner, &sh, cmd, &opts, &mut || {}) {
                Ok(()) => {
                    // What later iterations compare against (SIRF-50).
                    gate::write_setup_stamp(&wt_runner);
                    true
                }
                Err(e) => {
                    // That worker does not start — like a worktree that could
                    // not be created, but without sinking its siblings.
                    eprint_err(&format!("{name}: {e}\n{name}: not starting this worker"));
                    let wt_str = gitrange::git_path(wt_path);
                    let _ = repo_runner.run("git", &["worktree", "remove", "--force", &wt_str]);
                    setup_failed.push((name.clone(), e));
                    false
                }
            }
        });
    }
    let setup_failed_events: Vec<Value> = setup_failed
        .iter()
        .map(|(name, e)| {
            json!({
                "event": "fleet", "phase": "setup_failed", "worker": name,
                "cmd": setup.as_ref().map(|(c, _)| c.as_str()), "error": e,
            })
        })
        .collect();
    if assignments.is_empty() {
        for ev in &setup_failed_events {
            StdoutLineWriter
                .write_all(format!("{ev}\n").as_bytes())
                .ok();
        }
        eprint_err(
            "worktree setup failed for every worker — not starting the fleet. Fix the \
             command, or set worktree.setup_cmd in .sirius/config.json (\"\" disables setup)",
        );
        let _ = std::fs::remove_file(&lock_path);
        return 1;
    }
    let started = assignments.len() as u32;
    eprint_err(&format!(
        "workers: {started} ({}){}",
        workers.why,
        if setup_failed.is_empty() {
            String::new()
        } else {
            format!(
                " — {} of {} not started: worktree setup failed",
                setup_failed.len(),
                workers.count
            )
        }
    ));

    let iterations = std::sync::atomic::AtomicU32::new(0);
    // A worker that never started (its worktree setup failed) is a failure
    // the exit code must carry — exit 0 would hide it from a wrapper.
    let any_failed = std::sync::atomic::AtomicBool::new(!setup_failed.is_empty());
    // SF-14: did ANY worker find work? `iterations` cannot say — it only
    // counts when --max-iterations is set.
    let claimed_any = std::sync::atomic::AtomicBool::new(false);
    let ledger_path = ws.ledger_path();
    // SIRF-26: one pause flag for the whole fleet (a usage limit stops all).
    let pause: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    // SIRF-27: one fallback switch for the whole fleet, too.
    let fallback: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    // SIRF-31: one registry of work under review, shared by every worker.
    let inflight: std::sync::Arc<std::sync::Mutex<run::Inflight>> = Default::default();
    let fleets: Vec<run::Fleet> = assignments
        .iter()
        .map(|(_, wt_path)| run::Fleet {
            base: base.clone(),
            base_ref: base_ref.clone(),
            worktree: wt_path.clone(),
            sirius_dir: sirius_abs.clone(),
            review_prompt: review_prompt.clone(),
            pause: pause.clone(),
            fallback: fallback.clone(),
            inflight: inflight.clone(),
        })
        .collect();
    // Emitted once every launch check has passed — visible from the first event on (the claim events carry it per ticket).
    StdoutLineWriter
        .write_all(
            format!(
                "{}\n",
                json!({
                    "event": "fleet", "phase": "start",
                    "models": {
                        "default": cfg.models.default, "source": model_source,
                        "review": models::review_model(&cfg.models),
                        "fix_floor": cfg.models.fix_floor, "routes": cfg.models.routes,
                        "fallback": cfg.models.fallback,
                    },
                    // SIRF-50 (additive): the count that actually started,
                    // where it came from, and the worktree setup that ran.
                    "workers": started,
                    "workers_source": workers.source,
                    "workers_why": workers.why,
                    "setup": setup.as_ref().map(|(cmd, source)| json!({
                        "cmd": cmd,
                        "detected_from": match source {
                            config::SetupSource::Config => None,
                            config::SetupSource::Detected(f) => Some(f.as_str()),
                        },
                        "failed": setup_failed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
                    })),
                })
            )
            .as_bytes(),
        )
        .ok();
    // After the start event (still the FIRST event of a launch): one line per
    // worker whose worktree setup failed.
    for ev in &setup_failed_events {
        StdoutLineWriter
            .write_all(format!("{ev}\n").as_bytes())
            .ok();
    }
    std::thread::scope(|s| {
        for ((name, _), fleet) in assignments.iter().zip(&fleets) {
            s.spawn(|| {
                worker_loop(
                    name,
                    fleet,
                    &ledger_path,
                    &cfg,
                    agent_cmd,
                    from.as_deref(),
                    max_iterations,
                    &iterations,
                    &any_failed,
                    &claimed_any,
                    &spine,
                );
            });
        }
    });
    // Tear the worktrees down; completed work is safe — each issue's commits
    // live on its `sirius/<issue>` branch in the SHARED .git.
    for (_, wt_path) in &assignments {
        // Same verbatim-prefix hazard as creation above: a teardown git could
        // not read the path either, leaving orphaned worktrees behind.
        let wt_str = gitrange::git_path(wt_path);
        let _ = repo_runner.run("git", &["worktree", "remove", "--force", &wt_str]);
    }
    let _ = std::fs::remove_file(&lock_path);
    if fleets.first().is_some_and(run::Fleet::on_fallback)
        && fleets.first().and_then(run::Fleet::paused).is_none()
    {
        eprint_err("note: this run finished on the FALLBACK models (a fleet-wide stop was hit on the primary tier); the next launch starts on the primary tier again");
    }
    // Exit 4 = the fleet PAUSED on a usage limit (CONTRACTS §2): nothing is
    // broken, so a wrapper can wait for the limit to reset and relaunch.
    if let Some(reason) = fleets.first().and_then(run::Fleet::paused) {
        StdoutLineWriter
            .write_all(
                format!(
                    "{}\n",
                    json!({"event": "fleet", "phase": "paused", "reason": reason})
                )
                .as_bytes(),
            )
            .ok();
        eprint_err(&if reason.starts_with("integration red") {
            format!("fleet STOPPED — {reason}; unworked issues were left in todo.")
        } else {
            format!(
                "fleet PAUSED — an agent hit a fleet-wide stop: \"{reason}\". Unworked issues were left in todo; fix the cause (limit reset, `claude update`, `/login`) and relaunch."
            )
        });
        return 4;
    }
    // SF-14: name where the work is parked when `--from` found none of it.
    // Without --from, amt claims from every claimable stage, so there is
    // nowhere else for work to hide and nothing to say.
    if let Some(from) = from.as_deref() {
        if !claimed_any.load(std::sync::atomic::Ordering::SeqCst) {
            let amt = Amt::new(&repo_runner);
            let searched: Vec<&str> = from.split(',').map(str::trim).collect();
            let parked: Vec<(&str, usize)> = ["backlog", "todo"]
                .into_iter()
                .filter(|stage| !searched.contains(stage))
                .map(|stage| {
                    let n = amt.issue_list_status(stage).map(|v| v.len());
                    (stage, n.unwrap_or(0))
                })
                .collect();
            if let Some(hint) = no_work_hint(from, &parked) {
                eprint_err(&hint);
            }
        }
    }
    u8::from(any_failed.load(std::sync::atomic::Ordering::SeqCst))
}

/// SF-14: a run that claimed nothing exited silently, which reads as "the board
/// is done" — even when every issue was simply parked in a stage `--from`
/// excluded (the documented launch says `--from todo`, and a fresh board is all
/// backlog). Name where the work actually is. Pure over the per-stage counts so
/// it is testable without a board; `None` when nothing is parked elsewhere.
fn no_work_hint(from: &str, parked: &[(&str, usize)]) -> Option<String> {
    let waiting: Vec<String> = parked
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(stage, n)| format!("{n} in {stage}"))
        .collect();
    if waiting.is_empty() {
        return None;
    }
    Some(format!(
        "no claimable work in `{from}`, but {} — move it to `{from}`, or pass --from with that stage",
        waiting.join(", ")
    ))
}

/// `sirius run`'s model flags (SIRF-26).
struct RunModels {
    model: Option<String>,
    review_model: Option<String>,
    allow_default_model: bool,
}

/// Refuse a launch whose models are not explicit (SIRF-26): with no resolved
/// worker model every un-routed agent silently inherits its CLI's default —
/// the Lydgr incident. Name what it WOULD be, and require an explicit opt-in.
fn check_models(ws: &Workspace, cfg: &Config, agent_cmd: &str) -> Result<(), String> {
    models::validate(&cfg.models)?;
    let uses_placeholder = agent_cmd.contains("{model}")
        || cfg
            .review
            .cmd
            .as_deref()
            .is_some_and(|c| c.contains("{model}"));
    if cfg.models.default.is_some() {
        return Ok(());
    }
    if uses_placeholder {
        return Err("--agent-cmd / review.cmd use `{model}` but no model is set — pass --model <id> or set models.default".into());
    }
    if cfg.models.allow_default {
        eprint_err("warning: no worker model set (--allow-default-model) — agents use their CLI's own default");
        return Ok(());
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let would = match models::claude_default_model(&ws.root, home.as_deref()) {
        Some((m, from)) => format!("`{m}` (from {from})"),
        None => "the CLI's built-in default".into(),
    };
    Err(format!(
        "no model set for the fleet — every Claude worker{} would silently run on {would}, the way the 2026-10-03 Lydgr fleet burned a weekly limit. Pass --model <exact id> (a launching Claude session: your OWN model id), set models.default in .sirius/config.json, or pass --allow-default-model to accept that default",
        if cfg.review.cmd.is_some() { " and reviewer" } else { "" }
    ))
}

/// The reviewer prompt template (SIRF-23): the BUILT-IN default unless a
/// file exists at `review.prompt_file`, which then overrides it. Nothing is
/// written: a materialized copy of the default would pin the repo to that
/// release's wording forever, and every later prompt fix would silently miss
/// it. Only an UNREADABLE existing file is an error.
fn load_review_prompt(ws: &Workspace, cfg: &Config) -> Result<String, String> {
    if cfg.review.cmd.is_none() {
        return Ok(String::new());
    }
    let path = ws.root.join(&cfg.review.prompt_file);
    match std::fs::read_to_string(&path) {
        // An untouched copy that an earlier release wrote is not an override.
        Ok(p) if review::is_stale_shipped_prompt(&p) => {
            eprint_err(&format!(
                "{} is an unedited copy of an older built-in review prompt — using the current built-in (delete the file to silence this)",
                path.display()
            ));
            Ok(review::DEFAULT_PROMPT.to_string())
        }
        Ok(p) => Ok(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(review::DEFAULT_PROMPT.to_string())
        }
        Err(e) => Err(format!("cannot read review prompt {}: {e}", path.display())),
    }
}

/// Best-effort "is this pid alive" via `kill -0` semantics, without a libc
/// dependency: `ps -p <pid>` exit status. Only used to detect a stale fleet
/// pidfile, so a false "alive" merely makes the operator remove the file.
fn libc_kill_probe(pid: i32) -> bool {
    // Windows has no `ps` (and MSYS `ps` cannot see native pids): every live
    // holder read as dead and every lock was taken over (SIRF-32 review).
    if cfg!(windows) {
        let filter = format!("PID eq {pid}");
        return RealRunner::default()
            .run("tasklist", &["/FI", &filter, "/NH", "/FO", "CSV"])
            .map(|o| o.success() && o.stdout.contains(&format!("\"{pid}\"")))
            .unwrap_or(false);
    }
    RealRunner::default()
        .run("ps", &["-p", &pid.to_string()])
        .map(|o| o.success())
        .unwrap_or(false)
}

/// One worker's whole run: claim-and-work until the board is dry, the shared
/// iteration budget is spent, or the per-worker error budget trips.
#[allow(clippy::too_many_arguments)]
fn worker_loop(
    name: &str,
    fleet: &run::Fleet,
    ledger_path: &std::path::Path,
    cfg: &Config,
    agent_cmd: &str,
    from: Option<&str>,
    max_iterations: u32,
    iterations: &std::sync::atomic::AtomicU32,
    any_failed: &std::sync::atomic::AtomicBool,
    claimed_any: &std::sync::atomic::AtomicBool,
    spine: &spine::Spine,
) {
    use std::sync::atomic::Ordering;
    const ERROR_BUDGET: u32 = 5;
    // How many consecutive "board momentarily empty" probes (retry_after set —
    // issues exist but are leased) a worker waits through before giving up.
    const NOWORK_PROBES: u32 = 3;
    const NOWORK_WAIT_CAP_SECS: u64 = 30;

    // amt/hayven speak to the REPO (process cwd); the agent, git, and the
    // gate's test run are scoped to this worker's PRIVATE worktree (created
    // serially by cmd_run before any thread spawned). Parallel agents in one
    // shared checkout would cross-contaminate every baseline diff (worker A's
    // gate would test worker B's half-written code) and race on git's
    // index.lock — isolation is what makes the parallel fleet sound.
    let repo_runner = RealRunner::default();
    let agent_runner = RealRunner {
        cwd: Some(fleet.worktree.clone()),
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
        // SIRF-26: a usage limit anywhere in the fleet stops EVERY worker
        // from claiming — the board is left as-is, not churned through.
        if fleet.paused().is_some() {
            break;
        }
        // SIRF-32: a red integration (on_fail: block) stops every worker
        // from claiming — no new work lands on a frontier that is broken.
        if let Some(why) = integrate::block_reason(cfg, &ledger) {
            fleet.pause_with(&why);
            break;
        }
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
            Some(fleet),
        );
        if !matches!(outcome, run::IterationOutcome::NoWork { .. }) {
            claimed_any.store(true, Ordering::SeqCst);
        }
        match outcome {
            run::IterationOutcome::NoWork { retry_after } => {
                // An idle probe did no work — refund its budget slot so
                // --max-iterations counts real iterations, not empty probes.
                if max_iterations > 0 {
                    iterations.fetch_sub(1, Ordering::SeqCst);
                }
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
                run::ledger_warn(
                    "log_policy_event",
                    ledger.log_policy_event(
                        None,
                        "retry_budget",
                        &serde_json::json!({"backoff_ms": delay, "worker": name}),
                    ),
                );
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
            run::IterationOutcome::Paused(_) => break,
            _ => {
                consecutive_overlaps = 0;
                consecutive_errors = 0;
                nowork_probes = 0;
            }
        }
    }
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

/// How many workers a launch runs, and why (SIRF-50 #2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerCount {
    count: u32,
    /// `"flag"` or `"default"` — on the `fleet` start event.
    source: &'static str,
    /// Human explanation for stderr and the start event.
    why: String,
}

/// An explicit `--workers N` wins; `worker_concurrency` is the count only
/// when the flag is absent. It used to CAP the flag silently — `--workers 4`
/// ran 3 and nothing said so. Either way the count is at least 1.
fn resolve_workers(flag: Option<u32>, worker_concurrency: u32) -> WorkerCount {
    match flag {
        Some(n) => {
            let count = n.max(1);
            let why = if n == 0 {
                "--workers 0 raised to 1".to_string()
            } else if n > worker_concurrency {
                // It used to CAP the flag silently (SIRF-50) — say it no longer does.
                format!("--workers; above worker_concurrency {worker_concurrency}, which no longer caps it")
            } else {
                "--workers".to_string()
            };
            WorkerCount {
                count,
                source: "flag",
                why,
            }
        }
        // No flag ⇒ ONE worker, as always: worker_concurrency is not a
        // default, so a plain `sirius run` never multiplies agent spend.
        None => WorkerCount {
            count: 1,
            source: "default",
            why: "default; pass --workers N to run more in parallel".to_string(),
        },
    }
}

/// The stderr line naming the worktree setup that will run (SIRF-50).
fn setup_note(cmd: &str, source: &config::SetupSource) -> String {
    match source {
        config::SetupSource::Config => format!("worktree setup: {cmd} (worktree.setup_cmd)"),
        config::SetupSource::Detected(from) => format!(
            "worktree setup: {cmd} (detected from {from}; set worktree.setup_cmd to override, \"\" to disable)"
        ),
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
    fn a_red_integration_blocks_only_when_configured_to() {
        let led = Ledger::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.integration.on_fail = config::IntegrationOnFail::Block;
        assert_eq!(integrate::block_reason(&cfg, &led), None, "green: no block");
        led.set_meta(
            integrate::RED_KEY,
            Some(r#"{"at":"t","frontier":"f1","issue":"AMT-90"}"#),
        )
        .unwrap();
        let why = integrate::block_reason(&cfg, &led).unwrap();
        assert!(why.starts_with("integration red at f1 (AMT-90)"), "{why}");
        led.set_meta(integrate::RED_KEY, Some("not json")).unwrap();
        let why = integrate::block_reason(&cfg, &led).unwrap();
        assert!(
            why.starts_with("integration red"),
            "unreadable state holds: {why}"
        );
        cfg.integration.on_fail = config::IntegrationOnFail::Warn;
        assert_eq!(
            integrate::block_reason(&cfg, &led),
            None,
            "warn never blocks"
        );
    }

    #[test]
    fn tree_names_are_stable() {
        assert_eq!(
            tree_names(3),
            vec!["sirius/oak", "sirius/rowan", "sirius/birch"]
        );
    }

    fn ws_at(dir: &std::path::Path) -> Workspace {
        Workspace {
            root: dir.to_path_buf(),
            ametrite_db: None,
            hayven_dir: None,
        }
    }

    #[test]
    fn launch_without_a_model_is_refused_and_names_the_inherited_default() {
        // SIRF-26: never silently inherit the CLI's global default.
        let dir = std::env::temp_dir().join(format!("sirius-models-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude/settings.json"),
            r#"{"model":"fable[1m]"}"#,
        )
        .unwrap();
        let ws = ws_at(&dir);
        let mut cfg = Config::default();
        let e = check_models(&ws, &cfg, "claude -p go").unwrap_err();
        assert!(e.contains("no model set") && e.contains("fable[1m]"), "{e}");
        assert!(
            e.contains("--model") && e.contains("--allow-default-model"),
            "{e}"
        );
        // Explicitly allowed → proceeds.
        cfg.models.allow_default = true;
        assert!(check_models(&ws, &cfg, "claude -p go").is_ok());
        // ...but never with a `{model}` placeholder it cannot fill.
        assert!(check_models(&ws, &cfg, "x --model {model}").is_err());
        // A resolved model → fine.
        cfg.models.default = Some("claude-sonnet-5-5".into());
        assert!(check_models(&ws, &cfg, "x --model {model}").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // SIRF-50 #2: `--workers 4` with worker_concurrency 3 ran 3, silently.
    // The flag wins; without it one worker runs; either way it says why.
    #[test]
    fn explicit_workers_flag_wins_over_worker_concurrency() {
        let w = resolve_workers(Some(4), 3);
        assert_eq!((w.count, w.source), (4, "flag"));
        assert!(w.why.contains("above worker_concurrency 3"), "{}", w.why);
        let w = resolve_workers(Some(2), 3);
        assert_eq!(
            (w.count, w.source, w.why.as_str()),
            (2, "flag", "--workers")
        );
        // Absent flag ⇒ one worker, whatever worker_concurrency says.
        let w = resolve_workers(None, 5);
        assert_eq!((w.count, w.source), (1, "default"));
        assert!(w.why.contains("--workers"), "{}", w.why);
        // Never zero workers.
        assert_eq!(resolve_workers(Some(0), 3).count, 1);
        assert_eq!(resolve_workers(None, 0).count, 1);
    }

    #[test]
    fn setup_note_names_the_lockfile_and_the_override() {
        let n = setup_note(
            "bun install --frozen-lockfile",
            &config::SetupSource::Detected("bun.lock".into()),
        );
        assert_eq!(
            n,
            "worktree setup: bun install --frozen-lockfile (detected from bun.lock; set worktree.setup_cmd to override, \"\" to disable)"
        );
        assert_eq!(
            setup_note("make deps", &config::SetupSource::Config),
            "worktree setup: make deps (worktree.setup_cmd)"
        );
    }

    // SIRF-50: `sirius init` in a bun repo writes the setup command it will
    // run, beside the detected test command.
    #[test]
    fn init_writes_a_detected_setup_cmd_for_a_bun_repo() {
        let dir = std::env::temp_dir().join(format!("sirius-init-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bun.lock"), "{}").unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"test":"bun test"}}"#,
        )
        .unwrap();
        let ws = ws_at(&dir);
        assert_eq!(cmd_init(&ws, true), 0);
        let cfg = Config::load(&ws.config_path()).unwrap();
        assert_eq!(
            cfg.worktree.setup_cmd.as_deref(),
            Some("bun install --frozen-lockfile")
        );
        assert_eq!(cfg.gate.test_cmd.as_deref(), Some("bun test"));
        let _ = std::fs::remove_dir_all(&dir);
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
        // SIRF-15: any workspace prefix dispatches as an issue.
        assert!(regex_is_issue("GRA-12"));
        assert!(regex_is_issue("SIRF-23"));
        assert!(regex_is_issue("BC9-1"));
        // SF-12: the prefix is workspace-configurable — every one of these must
        // reach the ISSUE reader, not the symbol reader.
        assert!(regex_is_issue("AJ-8"));
        assert!(regex_is_issue("SF-12"));
        // Decisions keep their own single-letter namespace.
        assert!(!regex_is_issue("D-1"));
        assert!(!regex_is_issue("some::symbol"));
        assert!(!regex_is_issue("src/review/glob_regex"));
        assert!(!regex_is_issue("AMT-7-extra"));
        assert!(
            regex_is_issue("X-1"),
            "single-letter prefixes are real amt keys"
        );
        assert!(!regex_is_issue("D-3"), "a decision ref is not an issue");
        assert!(!regex_is_issue("src/a-1"));
    }

    /// SF-14 field case: the documented launch says `--from todo`, a fresh
    /// board is all backlog, and the run used to exit silently — read as done.
    #[test]
    fn no_work_hint_names_where_the_work_is_parked() {
        let hint = no_work_hint("todo", &[("backlog", 12)]).expect("work is parked");
        assert!(hint.contains("`todo`"), "{hint}");
        assert!(hint.contains("12 in backlog"), "{hint}");
        assert!(hint.contains("--from"), "{hint}");
    }

    /// A genuinely empty board has nothing to report — no false alarm.
    #[test]
    fn no_work_hint_is_silent_when_nothing_is_parked() {
        assert_eq!(no_work_hint("todo", &[("backlog", 0)]), None);
        assert_eq!(no_work_hint("todo,backlog", &[]), None);
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
