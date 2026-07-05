//! The Loop — `sirius run` (PRD §F3 / §9, M3) + adaptive claiming (M5).
//!
//! Claim order is LAW: Ametrite issue first, Hayvenhurst entities second;
//! release in reverse. On an entity-claim 409, release the issue back with a
//! comment naming the blocker — never hold an issue while spinning on a lock.
//!
//! Emits NDJSON iteration events (CONTRACTS §2). Writes one ledger `iterations`
//! row per pass.

use crate::amt::{Amt, ClaimResult};
use crate::config::{ClaimMode, Config, Oracle202};
use crate::hayven::{ClaimVerdict, Hayven};
use crate::ledger::Ledger;
use crate::shell::Runner;
use serde_json::{json, Value};
use std::io::Write;

/// A phase in the iteration, used in NDJSON `phase` fields.
pub const PHASES: &[&str] = &[
    "claim", "map", "lock", "brief", "work", "gate", "receipt", "release",
];

/// Emit one NDJSON event to `out` (stdout in production).
pub fn emit_event(
    out: &mut dyn Write,
    worker: &str,
    issue: Option<&str>,
    phase: &str,
    extra: Value,
) {
    let mut obj = json!({
        "event": "iteration",
        "worker": worker,
        "phase": phase,
    });
    if let Some(i) = issue {
        obj["issue"] = json!(i);
    }
    if let Value::Object(map) = extra {
        if let Value::Object(base) = &mut obj {
            for (k, v) in map {
                base.insert(k, v);
            }
        }
    }
    let _ = writeln!(out, "{obj}");
}

/// The decision an adaptive claimer makes for an iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDecision {
    /// Pre-emptively claim entities before work.
    PreClaim,
    /// Skip pre-claiming; rely on the gate.
    RelyOnGate,
}

/// Contention threshold: this many recent 409s in the sampled window flips
/// adaptive mode from RelyOnGate to PreClaim.
pub const ADAPTIVE_409_THRESHOLD: i64 = 2;
pub const ADAPTIVE_WINDOW: i64 = 20;

/// Decide whether to pre-claim entities for this iteration.
pub fn claim_decision(mode: ClaimMode, ledger: &Ledger) -> ClaimDecision {
    match mode {
        ClaimMode::Always => ClaimDecision::PreClaim,
        ClaimMode::Never => ClaimDecision::RelyOnGate,
        ClaimMode::Adaptive => {
            let recent_409 = ledger
                .count_policy_events("backoff_409", ADAPTIVE_WINDOW)
                .unwrap_or(0);
            if recent_409 >= ADAPTIVE_409_THRESHOLD {
                ClaimDecision::PreClaim
            } else {
                ClaimDecision::RelyOnGate
            }
        }
    }
}

/// Extract the issue id from an `amt claim` success object.
pub fn issue_id(v: &Value) -> Option<String> {
    v.get("id").and_then(Value::as_str).map(String::from)
}

/// Extract the issue title.
pub fn issue_title(v: &Value) -> String {
    v.get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Result of trying to lock a set of entities in claim order.
#[derive(Debug, Clone)]
pub enum LockResult {
    /// All entities claimed; carry the claim ids to release in reverse.
    Locked { claim_ids: Vec<String> },
    /// Hard overlap on an entity — the issue must be released. Names blocker.
    Overlap {
        blocker: String,
        /// Already-acquired claim ids that must be released (reverse order).
        acquired: Vec<String>,
    },
    /// Soft oracle conflict handled per policy.
    OracleBackoff {
        detail: String,
        acquired: Vec<String>,
    },
}

/// Attempt to claim every entity for an issue, in order, honoring the oracle-202
/// policy. Releases nothing here — the caller unwinds `acquired` on failure.
pub fn lock_entities(
    hv: &Hayven,
    ledger: &Ledger,
    config: &Config,
    issue: &str,
    title: &str,
    entities: &[String],
) -> LockResult {
    let intent = format!("{issue}: {title}");
    let mut acquired: Vec<String> = Vec::new();
    for ent in entities {
        match hv.claim(std::slice::from_ref(ent), &intent, false) {
            ClaimVerdict::Registered { claim_id } => {
                acquired.push(claim_id.unwrap_or_else(|| ent.clone()));
            }
            ClaimVerdict::Overlap { detail } => {
                ledger
                    .log_policy_event(
                        None,
                        "backoff_409",
                        &json!({"issue": issue, "entity": ent, "detail": detail}),
                    )
                    .ok();
                return LockResult::Overlap {
                    blocker: format!("{ent}: {detail}"),
                    acquired,
                };
            }
            ClaimVerdict::OracleConflict { detail } => {
                ledger
                    .log_policy_event(None, "oracle_202", &json!({"issue": issue, "entity": ent, "policy": format!("{:?}", config.oracle_202)}))
                    .ok();
                match config.oracle_202 {
                    Oracle202::BackOff => {
                        return LockResult::OracleBackoff { detail, acquired };
                    }
                    Oracle202::ForceWithBudget => {
                        // Force the claim, spending budget (token accounting is
                        // the agent's; we just record the force).
                        match hv.claim(std::slice::from_ref(ent), &intent, true) {
                            ClaimVerdict::Registered { claim_id } => {
                                acquired.push(claim_id.unwrap_or_else(|| ent.clone()));
                            }
                            other => {
                                return LockResult::OracleBackoff {
                                    detail: format!("force failed: {other:?}"),
                                    acquired,
                                };
                            }
                        }
                    }
                }
            }
            ClaimVerdict::Error { detail } => {
                return LockResult::OracleBackoff { detail, acquired };
            }
        }
    }
    LockResult::Locked {
        claim_ids: acquired,
    }
}

/// Release entity claims in reverse order (claim-order law: release in reverse).
pub fn release_entities(hv: &Hayven, claim_ids: &[String]) {
    for id in claim_ids.iter().rev() {
        let _ = hv.release(id);
    }
}

/// A single worker's outcome for one iteration, for the ledger + tests.
#[derive(Debug, Clone, PartialEq)]
pub enum IterationOutcome {
    /// No work; caller should honor `retry_after` / stop.
    NoWork { retry_after: Option<u64> },
    /// Completed and gated.
    Completed,
    /// Released back due to entity overlap (409).
    ReleasedOverlap,
    /// Released due to retry-budget exhaustion (deadend note filed).
    Deadend,
    /// An operational error.
    Error(String),
}

/// Run ONE iteration for a worker. Deterministic and fully mockable — the whole
/// loop's correctness (claim order, 409 unwind, receipts) is tested through this.
#[allow(clippy::too_many_arguments)]
pub fn run_iteration(
    amt: &Amt,
    hv: &Hayven,
    ledger: &Ledger,
    config: &Config,
    runner: &dyn Runner,
    worker: &str,
    from: Option<&str>,
    agent_cmd: &str,
    out: &mut dyn Write,
) -> IterationOutcome {
    ledger.upsert_worker(worker, "working").ok();

    // 1. CLAIM the issue (Ametrite first — claim-order law).
    let claim = amt.claim(worker, from);
    let issue_val = match claim {
        ClaimResult::Claimed(v) => v,
        ClaimResult::NoWork {
            retry_after,
            reason,
        } => {
            emit_event(
                out,
                worker,
                None,
                "claim",
                json!({"claimed": false, "reason": reason, "retry_after": retry_after}),
            );
            ledger.upsert_worker(worker, "idle").ok();
            return IterationOutcome::NoWork { retry_after };
        }
        ClaimResult::Error(e) => {
            emit_event(out, worker, None, "claim", json!({"error": e}));
            return IterationOutcome::Error(e);
        }
    };
    let issue = match issue_id(&issue_val) {
        Some(i) => i,
        None => return IterationOutcome::Error("claim returned no issue id".into()),
    };
    let title = issue_title(&issue_val);
    let iter_id = ledger.start_iteration(worker, Some(&issue)).unwrap_or(-1);
    let start = std::time::Instant::now();
    emit_event(
        out,
        worker,
        Some(&issue),
        "claim",
        json!({"claimed": true, "title": title}),
    );

    // 2. MAP issue → symbols (Hayvenhurst query + impact for blast radius).
    let mut entities: Vec<String> = Vec::new();
    if let Ok(q) = hv.query(&title) {
        entities = crate::gitrange::extract_ids(&q);
    }
    // Blast radius: expand each mapped symbol via `hayven impact` (PRD §9.2).
    let mut blast = 0usize;
    for sym in entities.clone() {
        if let Ok(imp) = hv.impact(&sym) {
            blast += crate::gitrange::extract_ids(&imp).len();
        }
    }
    emit_event(
        out,
        worker,
        Some(&issue),
        "map",
        json!({"entities": entities, "blast_radius": blast}),
    );

    // Adaptive: decide whether to pre-claim.
    let decision = claim_decision(config.claim_mode, ledger);
    ledger
        .log_policy_event(Some(iter_id), "concurrency", &json!({"claim_mode": format!("{:?}", config.claim_mode), "decision": format!("{:?}", decision)}))
        .ok();

    // 3. LOCK entities (Hayvenhurst second) — unless policy says rely on gate.
    let mut claim_ids: Vec<String> = Vec::new();
    let mut oracle_verdicts: Vec<String> = Vec::new();
    if config.claim_order_enforced && decision == ClaimDecision::PreClaim && !entities.is_empty() {
        match lock_entities(hv, ledger, config, &issue, &title, &entities) {
            LockResult::Locked { claim_ids: ids } => {
                oracle_verdicts = ids.iter().map(|_| "registered".to_string()).collect();
                claim_ids = ids;
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "lock",
                    json!({"locked": claim_ids.len()}),
                );
            }
            LockResult::Overlap { blocker, acquired } => {
                // Release any acquired entity claims (reverse), then release the
                // issue with a comment naming the blocker (claim-order law).
                release_entities(hv, &acquired);
                let _ = amt.release(
                    &issue,
                    worker,
                    Some("todo"),
                    Some(&format!(
                        "sirius: released — entity claim blocked by {blocker}"
                    )),
                );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "release",
                    json!({"reason": "entity_overlap", "blocker": blocker}),
                );
                ledger
                    .finish_iteration(
                        iter_id,
                        &entities,
                        "released",
                        None,
                        &["blocked".into()],
                        None,
                        Some(start.elapsed().as_millis() as i64),
                        None,
                    )
                    .ok();
                ledger.upsert_worker(worker, "idle").ok();
                return IterationOutcome::ReleasedOverlap;
            }
            LockResult::OracleBackoff { detail, acquired } => {
                release_entities(hv, &acquired);
                let _ = amt.release(
                    &issue,
                    worker,
                    Some("todo"),
                    Some(&format!(
                        "sirius: released — oracle/claim backoff: {detail}"
                    )),
                );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "release",
                    json!({"reason": "oracle_backoff", "detail": detail}),
                );
                ledger
                    .finish_iteration(
                        iter_id,
                        &entities,
                        "released",
                        None,
                        &["forced".into()],
                        None,
                        Some(start.elapsed().as_millis() as i64),
                        None,
                    )
                    .ok();
                ledger.upsert_worker(worker, "idle").ok();
                return IterationOutcome::ReleasedOverlap;
            }
        }
    }

    // 4. BRIEF: assemble context + recall (best-effort; the pack goes to the agent
    //    via env/args — here we just note it was assembled).
    let mut brief_entities = 0usize;
    for ent in &entities {
        if hv.context(ent).is_ok() {
            brief_entities += 1;
        }
        let _ = hv.recall_node(ent);
    }
    emit_event(
        out,
        worker,
        Some(&issue),
        "brief",
        json!({"context_packs": brief_entities}),
    );

    // 5. WORK: spawn the agent command; heartbeat both leases first.
    let _ = amt.heartbeat(&issue, worker);
    let work = runner.run("sh", &["-c", agent_cmd]);
    let work_ok = work.as_ref().map(|o| o.success()).unwrap_or(false);
    emit_event(
        out,
        worker,
        Some(&issue),
        "work",
        json!({"agent_ok": work_ok}),
    );

    // 6. GATE — select over the agent's ACTUAL changes, run the tests, and take
    //    the verdict from the test runner (never from the selector's exit code).
    //    `hayven affected-tests` only selects; on any doubt the gate runs the
    //    full suite. See gate.rs / SIRF-5 / D-3.
    let changed_files = crate::gitrange::changed_files(runner, None).unwrap_or_default();
    let gate_verdict = if changed_files.is_empty() {
        // The agent changed nothing → there is nothing to gate.
        None
    } else {
        Some(crate::gate::evaluate(hv, runner, &config.gate, &changed_files))
    };
    let gate_result = match &gate_verdict {
        Some(v) if v.passed => {
            let _ = amt.update_status(&issue, &config.target_status);
            "pass"
        }
        Some(v) => {
            let _ = amt.comment(
                &issue,
                &format!("sirius: gate failed [{}]: {}", v.plan, v.reason),
            );
            "fail"
        }
        None => "skipped",
    };
    emit_event(
        out,
        worker,
        Some(&issue),
        "gate",
        json!({
            "result": gate_result,
            "plan": gate_verdict.as_ref().map(|v| v.plan.clone()),
            "tests_run": gate_verdict.as_ref().map(|v| v.tests_run),
        }),
    );

    // 7. RECEIPT: decide + two-way link (only on a passing/complete iteration).
    let mut receipt_id: Option<i64> = None;
    if gate_result == "pass" || (gate_result == "skipped" && work_ok) {
        if let Ok(decision_ref) = amt.decide(
            &issue,
            &format!("Resolved {issue} via sirius"),
            "See linked entities.",
        ) {
            if let Ok(rec) = crate::bridge::link(
                amt,
                hv,
                ledger,
                crate::bridge::LinkKind::Decision,
                &decision_ref,
                &entities,
                Some(worker),
            ) {
                receipt_id = Some(rec.receipt_id);
            }
        }
        emit_event(
            out,
            worker,
            Some(&issue),
            "receipt",
            json!({"receipt_id": receipt_id}),
        );
    }

    // 8. RELEASE: entities first (reverse), then close out the issue.
    //    A failed gate must NOT advance the issue — holding a failing change back
    //    is the gate's whole purpose (gate.rs contract: "fail files a comment and
    //    leaves status untouched"). Only a passing gate — or a skipped gate over a
    //    successful agent run — releases to `target_status`. Otherwise the issue
    //    returns to `todo`: re-claimable, but un-promoted (matching the entity-
    //    overlap release path above). (SIRF-6)
    release_entities(hv, &claim_ids);
    let advanced = gate_result == "pass" || (gate_result == "skipped" && work_ok);
    let (release_status, release_comment): (&str, Option<&str>) = if advanced {
        (config.target_status.as_str(), None)
    } else {
        ("todo", Some("sirius: released without advancing — gate did not pass"))
    };
    let _ = amt.release(&issue, worker, Some(release_status), release_comment);
    emit_event(
        out,
        worker,
        Some(&issue),
        "release",
        json!({"status": release_status, "advanced": advanced}),
    );

    let outcome = if gate_result == "fail" {
        "gate_failed"
    } else {
        "completed"
    };
    ledger
        .finish_iteration(
            iter_id,
            &entities,
            outcome,
            Some(gate_result),
            &oracle_verdicts,
            None,
            Some(start.elapsed().as_millis() as i64),
            receipt_id,
        )
        .ok();
    ledger.upsert_worker(worker, "idle").ok();

    if gate_result == "fail" {
        // Record the failure as a deadend note so the next agent does not
        // re-derive it (PRD §F3 retry-budget exhaustion behavior).
        file_deadend(hv, &entities, &issue, "gate failed (affected-tests)");
        IterationOutcome::Deadend
    } else {
        IterationOutcome::Completed
    }
}

/// File a deadend fleet-memory note when a retry budget is exhausted (PRD §F3).
pub fn file_deadend(hv: &Hayven, entities: &[String], issue: &str, reason: &str) {
    let note = format!("deadend: {issue} exhausted retries — {reason}");
    let primary = entities.first().map(|s| s.as_str());
    let _ = hv.remember(&note, primary, "deadend", entities);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};

    fn cfg() -> Config {
        Config {
            claim_mode: ClaimMode::Always,
            // A test command so the gate can actually run (fail-closed otherwise).
            gate: crate::config::GateConfig {
                test_cmd: Some("run-suite".into()),
                fallback: crate::config::GateFallback::FullSuite,
            },
            ..Config::default()
        }
    }

    #[test]
    fn claim_order_locks_in_order_and_releases_reverse() {
        let m = MockRunner::new();
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c2"}"#);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let r = lock_entities(&hv, &led, &cfg(), "AMT-7", "t", &["e1".into(), "e2".into()]);
        match r {
            LockResult::Locked { claim_ids } => assert_eq!(claim_ids, vec!["c1", "c2"]),
            other => panic!("expected Locked, got {other:?}"),
        }
        // Locked e1 then e2, in order.
        let calls = m.recorded();
        assert!(calls[0].contains("hayven claim e1"));
        assert!(calls[1].contains("hayven claim e2"));

        // Release reverse.
        let m2 = MockRunner::new();
        m2.expect(&["hayven", "release"], 0, "ok");
        m2.expect(&["hayven", "release"], 0, "ok");
        let hv2 = Hayven::new(&m2);
        release_entities(&hv2, &["c1".into(), "c2".into()]);
        let rel = m2.recorded();
        assert!(rel[0].contains("release c2"));
        assert!(rel[1].contains("release c1"));
    }

    #[test]
    fn overlap_on_second_entity_unwinds_first() {
        let m = MockRunner::new();
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.push(MockResponse::new(
            &["hayven", "claim"],
            1,
            "",
            "held by other/agent",
        ));
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let r = lock_entities(&hv, &led, &cfg(), "AMT-7", "t", &["e1".into(), "e2".into()]);
        match r {
            LockResult::Overlap { blocker, acquired } => {
                assert!(blocker.contains("e2"));
                assert_eq!(acquired, vec!["c1"]); // c1 must be released by caller
            }
            other => panic!("expected Overlap, got {other:?}"),
        }
        // A 409 policy event was logged.
        assert_eq!(led.count_policy_events("backoff_409", 100).unwrap(), 1);
    }

    #[test]
    fn oracle_conflict_backs_off_by_default() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["hayven", "claim"], 3, "", "adjacency"));
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let c = Config {
            claim_mode: ClaimMode::Always,
            oracle_202: Oracle202::BackOff,
            ..Config::default()
        };
        let r = lock_entities(&hv, &led, &c, "AMT-7", "t", &["e1".into()]);
        assert!(matches!(r, LockResult::OracleBackoff { .. }));
        assert_eq!(led.count_policy_events("oracle_202", 100).unwrap(), 1);
    }

    #[test]
    fn oracle_force_with_budget_forces_claim() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["hayven", "claim"], 3, "", "adjacency"));
        m.push(MockResponse::new(
            &["hayven", "claim"],
            0,
            r#"{"id":"forced"}"#,
            "",
        ));
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let c = Config {
            claim_mode: ClaimMode::Always,
            oracle_202: Oracle202::ForceWithBudget,
            ..Config::default()
        };
        let r = lock_entities(&hv, &led, &c, "AMT-7", "t", &["e1".into()]);
        match r {
            LockResult::Locked { claim_ids } => assert_eq!(claim_ids, vec!["forced"]),
            other => panic!("expected Locked, got {other:?}"),
        }
        // Second call used --force.
        assert!(m.recorded()[1].contains("--force"));
    }

    #[test]
    fn adaptive_relies_on_gate_when_calm() {
        let led = Ledger::open_in_memory().unwrap();
        assert_eq!(
            claim_decision(ClaimMode::Adaptive, &led),
            ClaimDecision::RelyOnGate
        );
    }

    #[test]
    fn adaptive_preclaims_under_contention() {
        let led = Ledger::open_in_memory().unwrap();
        for _ in 0..ADAPTIVE_409_THRESHOLD {
            led.log_policy_event(None, "backoff_409", &json!({}))
                .unwrap();
        }
        assert_eq!(
            claim_decision(ClaimMode::Adaptive, &led),
            ClaimDecision::PreClaim
        );
    }

    #[test]
    fn no_work_short_circuits() {
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"claimed":false,"retry_after":30}"#,
        );
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &cfg(),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
        );
        assert_eq!(
            o,
            IterationOutcome::NoWork {
                retry_after: Some(30)
            }
        );
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("\"claimed\":false"));
    }

    #[test]
    fn full_iteration_completes_and_writes_ledger_row() {
        let m = MockRunner::new();
        // claim → issue
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"id":"AMT-7","title":"Fix"}"#,
        );
        // map → one entity
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        // lock e1
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        // brief: context + recall
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
        // heartbeat (re-claim by issue id)
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"id":"AMT-7"}"#,
        );
        // work (sh -c) → success
        m.expect(&["sh", "-c"], 0, "");
        // gate: changed files → selector (untraced → doubt) → full suite runs → pass
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"no traces yet — may UNDER-report","tests":[]}"#,
        );
        m.expect(&["sh", "-c"], 0, "test result: ok");
        // gate advance
        m.expect(
            &["amt", "--json", "issue", "update"],
            0,
            r#"{"id":"AMT-7"}"#,
        );
        // receipt: decide → D-1
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-1","resolves":"AMT-7"}"#,
        );
        // link decision: decision show → resolves, comment, remember
        m.expect(
            &["amt", "--json", "decision", "show"],
            0,
            r#"{"id":"D-1","resolves":"AMT-7"}"#,
        );
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        // release entity + issue
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-7"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &cfg(),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
        );
        assert_eq!(o, IterationOutcome::Completed);

        // Exactly one iteration row, outcome completed, gate pass, receipt set.
        let (n, outcome, gate, rcpt): (i64, String, String, Option<i64>) = led
            .conn
            .query_row(
                "SELECT COUNT(*), MAX(outcome), MAX(gate_result), MAX(receipt_id) FROM iterations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(outcome, "completed");
        assert_eq!(gate, "pass");
        assert!(rcpt.is_some());

        // NDJSON emitted a receipt and release phase.
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\"phase\":\"receipt\""));
        assert!(s.contains("\"phase\":\"release\""));
    }

    #[test]
    fn failed_gate_does_not_advance_the_issue() {
        // Same happy path up to the gate, but the gate RUNS the tests and they
        // fail (a real regression). The issue must be released back to `todo`,
        // NOT promoted to `target_status`, and no receipt may be filed.
        // (SIRF-6 release path; SIRF-5 real test run.)
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"id":"AMT-9","title":"Regress"}"#,
        );
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"id":"AMT-9"}"#,
        );
        // work agent (sh -c) → success
        m.expect(&["sh", "-c"], 0, "");
        // Gate: changed files → selector (untraced → doubt) → full suite RUNS
        // and FAILS (exit 101) → the gate fails on the runner's verdict.
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"may UNDER-report","tests":[]}"#,
        );
        m.push(MockResponse::new(
            &["sh", "-c"],
            101,
            "test result: FAILED. 1 failed",
            "",
        ));
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-9"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &cfg(),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
        );
        // A failed gate ends the iteration as a deadend, not a completion.
        assert_eq!(o, IterationOutcome::Deadend);

        // The issue was released to `todo`, never advanced to `in_review`, and
        // no status-update advance was issued.
        let calls = m.recorded();
        let release = calls
            .iter()
            .find(|c| c.contains("amt --json release AMT-9"))
            .expect("issue was released");
        assert!(
            release.contains("--status todo"),
            "gate-failed issue must return to todo, got: {release}"
        );
        assert!(
            !release.contains("in_review"),
            "gate-failed issue must not be promoted, got: {release}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("issue update")),
            "no status advance may be issued on a failed gate"
        );
        // No receipt (decide/link) was filed for a failing iteration.
        assert!(!calls.iter().any(|c| c.contains("amt --json decide")));

        // Ledger records the honest outcome: gate_failed, no receipt.
        let (outcome, gate, rcpt): (String, String, Option<i64>) = led
            .conn
            .query_row(
                "SELECT outcome, gate_result, receipt_id FROM iterations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(outcome, "gate_failed");
        assert_eq!(gate, "fail");
        assert!(rcpt.is_none());

        // The release NDJSON reflects the un-advanced status.
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\"advanced\":false"));
    }
}
