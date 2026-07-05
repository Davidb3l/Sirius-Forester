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
use crate::shell::{AgentOutcome, AgentRunOpts, Runner};
use serde_json::{json, Value};
use std::io::Write;
use std::time::Duration;

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
    /// All entities claimed; carry the claim ids to release in reverse, plus the
    /// TRUE per-entity oracle verdict parallel to `claim_ids` (SIRF-9): each is
    /// `"registered"` (claimed clean) or `"forced"` (oracle-conflicted then
    /// forced under `Oracle202::ForceWithBudget`). Recorded verbatim in the
    /// ledger so we never fabricate a uniform `"registered"` vector.
    Locked {
        claim_ids: Vec<String>,
        verdicts: Vec<&'static str>,
    },
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
    // Parallel to `acquired`: how each held claim was obtained (SIRF-9).
    let mut verdicts: Vec<&'static str> = Vec::new();
    for ent in entities {
        match hv.claim(std::slice::from_ref(ent), &intent, false) {
            ClaimVerdict::Registered { claim_id } => {
                // SIRF-8: a Registered verdict with NO claim id means the daemon
                // gave us nothing we can hand back to `hayven release`. Pushing
                // the entity NAME as a fake id (the old behavior) later silently
                // fails the release and leaks the lease. Treat a missing id as a
                // claim failure and unwind — the safe choice: we never manage a
                // lease we cannot release, and the caller releases what we hold.
                match claim_id {
                    Some(id) => {
                        acquired.push(id);
                        verdicts.push("registered");
                    }
                    None => {
                        ledger
                            .log_policy_event(
                                None,
                                "backoff_409",
                                &json!({"issue": issue, "entity": ent, "detail": "claim registered without a claim id — cannot manage lease"}),
                            )
                            .ok();
                        return LockResult::Overlap {
                            blocker: format!("{ent}: claim registered without a claim id"),
                            acquired,
                        };
                    }
                }
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
                            // SIRF-8: same missing-id guard as the clean path — a
                            // forced claim with no id is unmanageable, so unwind.
                            ClaimVerdict::Registered {
                                claim_id: Some(id),
                            } => {
                                acquired.push(id);
                                verdicts.push("forced");
                            }
                            ClaimVerdict::Registered { claim_id: None } => {
                                ledger
                                    .log_policy_event(
                                        None,
                                        "backoff_409",
                                        &json!({"issue": issue, "entity": ent, "detail": "forced claim registered without a claim id — cannot manage lease"}),
                                    )
                                    .ok();
                                return LockResult::Overlap {
                                    blocker: format!("{ent}: forced claim registered without a claim id"),
                                    acquired,
                                };
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
        verdicts,
    }
}

/// Bounded release-retry policy (SIRF-8), mirroring the SIRF-4 reverse-stamp
/// pattern in `bridge.rs`: a release can transiently fail (daemon reindex, amt
/// mid-startup), and dropping the result with `let _ =` leaks the lock silently.
/// So each release is retried once with a short backoff before we give up.
const RELEASE_ATTEMPTS: u32 = 2;
#[cfg(not(test))]
const RELEASE_BASE_MS: u64 = 250;
#[cfg(test)]
const RELEASE_BASE_MS: u64 = 0; // no real sleeps under test

/// Release ONE Hayvenhurst entity claim, retrying once on a transient failure
/// and logging + recording a ledger `release_failure` policy event if it never
/// lands (SIRF-8). Returns true once released, false if every attempt failed.
fn release_entity_checked(hv: &Hayven, ledger: &Ledger, issue: &str, claim_id: &str) -> bool {
    for attempt in 0..RELEASE_ATTEMPTS {
        match hv.release(claim_id) {
            Ok(()) => return true,
            Err(e) => {
                if attempt + 1 < RELEASE_ATTEMPTS {
                    let backoff = RELEASE_BASE_MS.saturating_mul(1u64 << attempt);
                    std::thread::sleep(Duration::from_millis(backoff));
                    continue;
                }
                // Final failure: shout to stderr AND record a ledger event so a
                // leaked lock is never silent.
                eprintln!(
                    "sirius: FAILED to release hayven claim {claim_id} for {issue}: {e}"
                );
                ledger
                    .log_policy_event(
                        None,
                        "release_failure",
                        &json!({"issue": issue, "claim_id": claim_id, "error": e}),
                    )
                    .ok();
                return false;
            }
        }
    }
    false
}

/// Release entity claims in reverse order (claim-order law: release in reverse).
/// Each release is retried + logged on failure so a leaked lock is never silent
/// (SIRF-8). `ledger`/`issue` are threaded through for the `release_failure`
/// policy event.
pub fn release_entities(hv: &Hayven, ledger: &Ledger, issue: &str, claim_ids: &[String]) {
    for id in claim_ids.iter().rev() {
        release_entity_checked(hv, ledger, issue, id);
    }
}

/// Release an Ametrite ISSUE with the same retry-once + log-on-failure policy as
/// `release_entities` (SIRF-8): the old `let _ = amt.release(...)` swallowed
/// failures, leaving the issue silently locked to a dead worker.
fn release_issue_checked(
    amt: &Amt,
    ledger: &Ledger,
    issue: &str,
    worker: &str,
    status: Option<&str>,
    comment: Option<&str>,
) {
    for attempt in 0..RELEASE_ATTEMPTS {
        match amt.release(issue, worker, status, comment) {
            Ok(()) => return,
            Err(e) => {
                if attempt + 1 < RELEASE_ATTEMPTS {
                    let backoff = RELEASE_BASE_MS.saturating_mul(1u64 << attempt);
                    std::thread::sleep(Duration::from_millis(backoff));
                    continue;
                }
                eprintln!("sirius: FAILED to release amt issue {issue}: {e}");
                ledger
                    .log_policy_event(
                        None,
                        "release_failure",
                        &json!({"issue": issue, "claim_id": issue, "worker": worker, "error": e}),
                    )
                    .ok();
                return;
            }
        }
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
            LockResult::Locked {
                claim_ids: ids,
                verdicts,
            } => {
                // SIRF-9: record the TRUE per-entity verdict (registered/forced),
                // not a fabricated all-"registered" vector.
                oracle_verdicts = verdicts.iter().map(|v| v.to_string()).collect();
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
                release_entities(hv, ledger, &issue, &acquired);
                release_issue_checked(
                    amt,
                    ledger,
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
                release_entities(hv, ledger, &issue, &acquired);
                release_issue_checked(
                    amt,
                    ledger,
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
                        // SIRF-9: this path BACKED OFF — it did not force. Record
                        // "backoff", not the old (dishonest) "forced".
                        &["backoff".into()],
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

    // 5+6. WORK then GATE, retried as a unit up to `config.retry_budget` times
    //    (SIRF-9). A gate FAIL used to deadend on the FIRST failure — the budget
    //    was dead. Now a failing gate re-runs the WORK+GATE sequence (a fresh
    //    agent attempt over the same claimed issue/entities) until it passes or
    //    the budget is spent, then files the deadend + releases un-advanced.
    //    Semantics preserved from the sibling work:
    //      * SIRF-6: a gate that never passes still ends by releasing the issue
    //        back to `todo` un-advanced (handled after the loop).
    //      * SIRF-7: a KILLED (timed-out) agent must NOT consume retries — a hung
    //        agent should not loop. The timeout branch returns immediately from
    //        inside the loop, so it never reaches the retry decision.
    //    The held leases stay claimed across attempts (we never release between
    //    tries); each attempt re-heartbeats before spawning.
    // Entities whose Hayvenhurst claims we actually hold and must keep alive.
    let held_entities: Vec<String> = if claim_ids.is_empty() {
        Vec::new()
    } else {
        entities.clone()
    };
    let lock_intent = format!("{issue}: {title}");
    // `retry_budget` is the max number of WORK+GATE attempts (min 1 — a budget
    // of 0/1 yields a single attempt, matching the pre-SIRF-9 one-shot loop).
    let max_attempts = config.retry_budget.max(1);
    // Assigned on every non-returning path out of the loop below (the loop runs
    // at least once and sets both before any `break`); the timeout path returns.
    let mut work_ok;
    let mut gate_result;
    let mut attempt: u32 = 0;
    loop {
        // WORK: spawn the agent command under supervision (SIRF-7). One beat
        //    fires before the spawn, and then a periodic heartbeat renews BOTH
        //    leases — the amt issue via `amt.heartbeat` and each held Hayvenhurst
        //    claim by re-claiming the same entities/intent (same agent + same id
        //    = refresh, not a collision). This closes the double-claim race for
        //    any agent run longer than amt's 900s lease. A configurable timeout
        //    kills a hung/runaway agent; on expiry the iteration FAILS (released
        //    without advancing, plus a deadend note). The agent's output is
        //    captured to a durable log so it no longer vanishes on success.
        let _ = amt.heartbeat(&issue, worker);
        let mut heartbeat = || {
            // Renew the Ametrite issue lease.
            let _ = amt.heartbeat(&issue, worker);
            // Renew each held Hayvenhurst entity claim (re-claim = refresh).
            if !held_entities.is_empty() {
                let _ = hv.claim(&held_entities, &lock_intent, false);
            }
        };
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(config.agent_timeout_secs),
            heartbeat_interval: Duration::from_secs(config.heartbeat_interval_secs()),
            log_path: agent_log_path(&issue),
        };
        let work = runner.run_agent("sh", &["-c", agent_cmd], &opts, &mut heartbeat);
        let timed_out = work.as_ref().map(AgentOutcome::timed_out).unwrap_or(false);
        work_ok = work.as_ref().map(AgentOutcome::success).unwrap_or(false);
        // Agent exit code, from the captured output (durably logged by the runner).
        let agent_code = work.as_ref().ok().and_then(|w| w.output().code);
        emit_event(
            out,
            worker,
            Some(&issue),
            "work",
            json!({"agent_ok": work_ok, "timed_out": timed_out, "exit": agent_code, "attempt": attempt + 1}),
        );

        // On a timeout the agent was killed. The iteration must FAIL immediately:
        // release the held entity claims (reverse), return the issue to `todo`
        // un-advanced, and file a deadend note so the next agent does not
        // re-derive the hang. A killed agent does NOT consume the retry budget
        // (SIRF-7 / SIRF-9): we return straight out of the loop rather than
        // looping back to WORK.
        if timed_out {
            release_entities(hv, ledger, &issue, &claim_ids);
            release_issue_checked(
                amt,
                ledger,
                &issue,
                worker,
                Some("todo"),
                Some(&format!(
                    "sirius: released — agent timed out after {}s (killed)",
                    config.agent_timeout_secs
                )),
            );
            emit_event(
                out,
                worker,
                Some(&issue),
                "release",
                json!({"reason": "agent_timeout", "advanced": false}),
            );
            ledger
                .finish_iteration(
                    iter_id,
                    &entities,
                    "agent_timeout",
                    None,
                    &oracle_verdicts,
                    None,
                    Some(start.elapsed().as_millis() as i64),
                    None,
                )
                .ok();
            ledger.upsert_worker(worker, "idle").ok();
            file_deadend(hv, &entities, &issue, "agent timed out");
            return IterationOutcome::Deadend;
        }

        // GATE — select over the agent's ACTUAL changes, run the tests, and take
        //    the verdict from the test runner (never from the selector's exit
        //    code). `hayven affected-tests` only selects; on any doubt the gate
        //    runs the full suite. See gate.rs / SIRF-5 / D-3.
        let changed_files = crate::gitrange::changed_files(runner, None).unwrap_or_default();
        let verdict = if changed_files.is_empty() {
            // The agent changed nothing → there is nothing to gate.
            None
        } else {
            Some(crate::gate::evaluate(hv, runner, &config.gate, &changed_files))
        };
        gate_result = match &verdict {
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
                "plan": verdict.as_ref().map(|v| v.plan.clone()),
                "tests_run": verdict.as_ref().map(|v| v.tests_run),
                "attempt": attempt + 1,
            }),
        );

        // Retry decision (SIRF-9): only a FAIL is retryable, and only while the
        // budget has attempts left. Each retry is recorded as a policy event so
        // the ledger shows the honest attempt history.
        if gate_result == "fail" && attempt + 1 < max_attempts {
            ledger
                .log_policy_event(
                    Some(iter_id),
                    "retry_budget",
                    &json!({"issue": issue, "attempt": attempt + 1, "max_attempts": max_attempts}),
                )
                .ok();
            emit_event(
                out,
                worker,
                Some(&issue),
                "work",
                json!({"retrying": true, "attempt": attempt + 1, "max_attempts": max_attempts}),
            );
            attempt += 1;
            continue;
        }
        break;
    }

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
    release_entities(hv, ledger, &issue, &claim_ids);
    let advanced = gate_result == "pass" || (gate_result == "skipped" && work_ok);
    let (release_status, release_comment): (&str, Option<&str>) = if advanced {
        (config.target_status.as_str(), None)
    } else {
        ("todo", Some("sirius: released without advancing — gate did not pass"))
    };
    release_issue_checked(amt, ledger, &issue, worker, Some(release_status), release_comment);
    emit_event(
        out,
        worker,
        Some(&issue),
        "release",
        json!({"status": release_status, "advanced": advanced}),
    );

    let outcome = if gate_result == "fail" {
        "gate_failed"
    } else if advanced {
        "completed"
    } else {
        // Gate was skipped (the agent produced nothing to gate) AND the run did
        // not succeed — nothing advanced, so record the honest failure rather
        // than claiming completion. (review fix)
        "error"
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

/// Durable log path for one agent run (SIRF-7): `.sirius/logs/<issue>-<ts>.log`.
/// Relative to the cwd (the loop runs from the repo root beside `.sirius/`).
/// Returns `None` only if the system clock is before the epoch (never, in
/// practice) — the directory is created lazily by the writer.
fn agent_log_path(issue: &str) -> Option<std::path::PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    // Sanitize the issue ref for a filename (AMT-7 → AMT-7 is fine; guard slashes).
    let safe: String = issue
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    Some(std::path::PathBuf::from(".sirius/logs").join(format!("{safe}-{ts}.log")))
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
            LockResult::Locked { claim_ids, verdicts } => {
                assert_eq!(claim_ids, vec!["c1", "c2"]);
                // Both claimed clean → both "registered" (SIRF-9).
                assert_eq!(verdicts, vec!["registered", "registered"]);
            }
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
        let led2 = Ledger::open_in_memory().unwrap();
        release_entities(&hv2, &led2, "AMT-7", &["c1".into(), "c2".into()]);
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
            LockResult::Locked { claim_ids, verdicts } => {
                assert_eq!(claim_ids, vec!["forced"]);
                // The entity was oracle-conflicted then FORCED → "forced" (SIRF-9).
                assert_eq!(verdicts, vec!["forced"]);
            }
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
        // (SIRF-6 release path; SIRF-5 real test run.) With `retry_budget: 1`
        // the single gate fail exhausts the budget immediately — the multi-
        // attempt retry loop is covered by `retry_budget_reruns_work_gate_*`.
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
        let c = Config {
            retry_budget: 1,
            ..cfg()
        };
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &c,
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

    #[test]
    fn agent_timeout_kills_releases_without_advancing_and_files_deadend() {
        // SIRF-7: a hung agent times out. The runner reports TimedOut; the
        // iteration must fail — release the entity claim, return the issue to
        // `todo` un-advanced, never gate, never receipt, and file a deadend.
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"id":"AMT-11","title":"Hang"}"#,
        );
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
        // pre-spawn heartbeat + the periodic beats fired by the sim.
        m.expect(&["amt", "--json", "claim", "--issue"], 0, r#"{"id":"AMT-11"}"#);
        // work: the agent command itself (recorded), then the sim times it out.
        m.expect(&["sh", "-c"], 0, "");
        // release entity + issue (back to todo), then the deadend note.
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-11"}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        // Arm a timeout that fires two heartbeats before the kill.
        m.arm_agent_timeout(2);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt, &hv, &led, &cfg(), &m, "sirius/oak", Some("todo"), "sleep 999", &mut out,
        );
        assert_eq!(o, IterationOutcome::Deadend);

        let calls = m.recorded();
        // The issue was released to todo, never advanced, never gated/receipted.
        let release = calls
            .iter()
            .find(|c| c.contains("amt --json release AMT-11"))
            .expect("issue released");
        assert!(release.contains("--status todo"));
        assert!(!calls.iter().any(|c| c.contains("issue update")));
        assert!(!calls.iter().any(|c| c.contains("amt --json decide")));
        assert!(!calls.iter().any(|c| c.contains("git diff")), "no gate on timeout");
        // A deadend note was filed naming the timeout.
        assert!(calls.iter().any(|c| c.contains("hayven remember") && c.contains("timed out")));

        // Ledger honesty: outcome agent_timeout, no gate, no receipt.
        let (outcome, gate, rcpt): (String, Option<String>, Option<i64>) = led
            .conn
            .query_row(
                "SELECT outcome, gate_result, receipt_id FROM iterations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(outcome, "agent_timeout");
        assert!(gate.is_none());
        assert!(rcpt.is_none());

        // NDJSON marks the timeout and the un-advanced release.
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\"timed_out\":true"));
        assert!(s.contains("\"reason\":\"agent_timeout\""));
    }

    #[test]
    fn heartbeat_renews_both_leases_while_agent_runs() {
        // SIRF-7: while the agent runs, each beat must renew BOTH leases — the
        // amt issue (re-claim by --issue) and the held Hayvenhurst entity
        // (re-claim = refresh). Arm three beats and count the renewals.
        let m = MockRunner::new();
        m.expect(&["amt", "--json", "claim"], 0, r#"{"id":"AMT-12","title":"Long"}"#);
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
        m.expect(&["amt", "--json", "claim", "--issue"], 0, r#"{"id":"AMT-12"}"#);
        m.expect(&["sh", "-c"], 0, "");
        m.expect(&["git", "diff"], 0, ""); // no changes → gate skipped
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-12"}"#);
        // Normal (non-timeout) return, firing three heartbeats mid-run.
        m.arm_agent_heartbeats(3);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let _ = run_iteration(
            &amt, &hv, &led, &cfg(), &m, "sirius/oak", Some("todo"), "long-cmd", &mut out,
        );

        let calls = m.recorded();
        // One pre-spawn heartbeat + three periodic beats = four issue renewals.
        let issue_renews = calls
            .iter()
            .filter(|c| c.contains("amt --json claim --issue AMT-12"))
            .count();
        assert_eq!(issue_renews, 4, "1 pre-spawn + 3 periodic issue heartbeats");
        // The initial lock claim + three periodic entity refreshes = four claims.
        let entity_claims = calls
            .iter()
            .filter(|c| c.contains("hayven claim e1"))
            .count();
        assert_eq!(entity_claims, 4, "1 lock + 3 periodic entity refreshes");
    }

    // ---- SIRF-8: release-failure retry + logging ------------------------

    #[test]
    fn release_entity_retries_and_recovers_transient_failure() {
        // SIRF-8: the first `hayven release` fails transiently; the retry (a
        // second call, which falls through to the mock's benign success) lands.
        // No `release_failure` event is logged because the lease was freed.
        let m = MockRunner::new();
        m.push(MockResponse::new(&["hayven", "release"], 1, "", "daemon busy"));
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        release_entities(&hv, &led, "AMT-7", &["c1".into()]);
        // Two attempts were made (the failure + the recovering retry).
        let releases = m
            .recorded()
            .iter()
            .filter(|c| c.contains("hayven release"))
            .count();
        assert_eq!(releases, 2, "one failure + one recovering retry");
        assert_eq!(led.count_policy_events("release_failure", 100).unwrap(), 0);
    }

    #[test]
    fn release_entity_logs_policy_event_when_every_attempt_fails() {
        // SIRF-8: both attempts fail → a `release_failure` policy event is
        // recorded (the leaked lock is no longer silent) after RELEASE_ATTEMPTS.
        let m = MockRunner::new();
        for _ in 0..RELEASE_ATTEMPTS {
            m.push(MockResponse::new(&["hayven", "release"], 1, "", "still down"));
        }
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        release_entities(&hv, &led, "AMT-7", &["c1".into()]);
        let releases = m
            .recorded()
            .iter()
            .filter(|c| c.contains("hayven release"))
            .count();
        assert_eq!(releases, RELEASE_ATTEMPTS as usize);
        assert_eq!(led.count_policy_events("release_failure", 100).unwrap(), 1);
    }

    #[test]
    fn issue_release_logs_policy_event_when_every_attempt_fails() {
        // SIRF-8: the amt issue release also retries + logs on total failure,
        // rather than swallowing the error with `let _ =`.
        let m = MockRunner::new();
        for _ in 0..RELEASE_ATTEMPTS {
            m.push(MockResponse::new(
                &["amt", "--json", "release"],
                1,
                "",
                "amt unavailable",
            ));
        }
        let amt = Amt::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        release_issue_checked(&amt, &led, "AMT-7", "sirius/oak", Some("todo"), None);
        let releases = m
            .recorded()
            .iter()
            .filter(|c| c.contains("amt --json release"))
            .count();
        assert_eq!(releases, RELEASE_ATTEMPTS as usize);
        assert_eq!(led.count_policy_events("release_failure", 100).unwrap(), 1);
    }

    // ---- SIRF-8: missing claim id is treated as a claim failure ---------

    #[test]
    fn missing_claim_id_unwinds_instead_of_pushing_entity_name() {
        // SIRF-8: hayven returns Registered with NO id (exit 0, no JSON id). We
        // must NOT push the entity name as a bogus claim id (which would later
        // silently fail `hayven release`). Instead we treat it as a claim
        // failure and unwind — releasing what we already hold.
        let m = MockRunner::new();
        // e1 claims clean with a real id.
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        // e2 registers but returns no id at all.
        m.expect(&["hayven", "claim"], 0, "");
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let r = lock_entities(&hv, &led, &cfg(), "AMT-7", "t", &["e1".into(), "e2".into()]);
        match r {
            LockResult::Overlap { blocker, acquired } => {
                assert!(blocker.contains("e2"), "blocker names the id-less entity");
                assert!(blocker.contains("without a claim id"));
                // Only the real id was acquired; the entity NAME was never pushed.
                assert_eq!(acquired, vec!["c1"]);
            }
            other => panic!("expected Overlap on missing id, got {other:?}"),
        }
        // The failure was recorded, not swallowed.
        assert_eq!(led.count_policy_events("backoff_409", 100).unwrap(), 1);
    }

    // ---- SIRF-9: retry_budget reruns the WORK+GATE sequence -------------

    /// Program a full happy-path claim→map→lock→brief prefix, then leave the
    /// WORK/GATE/RELEASE calls for the caller to queue per scenario. The mock's
    /// longest-prefix matching + benign-default keeps incidental calls quiet.
    fn program_prefix(m: &MockRunner, issue: &str) {
        m.expect(&["amt", "--json", "claim"], 0, &format!(r#"{{"id":"{issue}","title":"T"}}"#));
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
    }

    #[test]
    fn retry_budget_reruns_work_gate_until_pass() {
        // SIRF-9: retry_budget=2. The FIRST gate attempt fails; the loop re-runs
        // WORK+GATE; the SECOND attempt passes → the iteration COMPLETES and
        // advances. A retry policy event is recorded for the first failure.
        let m = MockRunner::new();
        program_prefix(&m, "AMT-20");
        // Attempt 1: work ok, gate runs the suite and FAILS.
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(&["hayven", "affected-tests"], 0, r#"{"roots":["run"],"note":"doubt","tests":[]}"#);
        m.push(MockResponse::new(&["sh", "-c"], 101, "test result: FAILED. 1 failed", ""));
        // Attempt 2: work ok, gate runs the suite and PASSES.
        m.expect(&["sh", "-c"], 0, ""); // agent (2nd attempt)
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(&["hayven", "affected-tests"], 0, r#"{"roots":["run"],"note":"doubt","tests":[]}"#);
        m.expect(&["sh", "-c"], 0, "test result: ok");
        // advance + receipt + release
        m.expect(&["amt", "--json", "issue", "update"], 0, r#"{"id":"AMT-20"}"#);
        m.expect(&["amt", "--json", "decide"], 0, r#"{"id":"D-1","resolves":"AMT-20"}"#);
        m.expect(&["amt", "--json", "decision", "show"], 0, r#"{"id":"D-1","resolves":"AMT-20"}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-20"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config { retry_budget: 2, ..cfg() };
        let o = run_iteration(
            &amt, &hv, &led, &c, &m, "sirius/oak", Some("todo"), "true", &mut out,
        );
        assert_eq!(o, IterationOutcome::Completed);

        // The agent ran twice (two attempts).
        let agent_runs = m.recorded().iter().filter(|c| **c == "sh -c true").count();
        assert_eq!(agent_runs, 2, "one retry means two agent runs");
        // Exactly one retry policy event for the first failed attempt.
        assert_eq!(led.count_policy_events("retry_budget", 100).unwrap(), 1);
        // Ledger: advanced + gate pass on the final attempt.
        let (outcome, gate): (String, String) = led
            .conn
            .query_row("SELECT outcome, gate_result FROM iterations", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(outcome, "completed");
        assert_eq!(gate, "pass");
        // NDJSON marks a retry.
        assert!(String::from_utf8(out).unwrap().contains("\"retrying\":true"));
    }

    #[test]
    fn retry_budget_exhausts_and_deadends_after_all_attempts_fail() {
        // SIRF-9: retry_budget=2. BOTH attempts fail the gate → the budget is
        // spent, the issue is released un-advanced (SIRF-6), a deadend is filed,
        // and TWO agent runs happened (one retry). One retry event is recorded
        // (only the non-final failure triggers a retry).
        let m = MockRunner::new();
        program_prefix(&m, "AMT-21");
        // Attempt 1: fail.
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(&["hayven", "affected-tests"], 0, r#"{"roots":["run"],"note":"doubt","tests":[]}"#);
        m.push(MockResponse::new(&["sh", "-c"], 101, "test result: FAILED. 1 failed", ""));
        // Attempt 2: fail again.
        m.expect(&["sh", "-c"], 0, ""); // agent (2nd)
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(&["hayven", "affected-tests"], 0, r#"{"roots":["run"],"note":"doubt","tests":[]}"#);
        m.push(MockResponse::new(&["sh", "-c"], 101, "test result: FAILED. 1 failed", ""));
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-21"}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config { retry_budget: 2, ..cfg() };
        let o = run_iteration(
            &amt, &hv, &led, &c, &m, "sirius/oak", Some("todo"), "true", &mut out,
        );
        assert_eq!(o, IterationOutcome::Deadend);

        let calls = m.recorded();
        // Two agent runs (attempt 1 + one retry).
        let agent_runs = calls.iter().filter(|c| **c == "sh -c true").count();
        assert_eq!(agent_runs, 2);
        // Exactly one retry event (the final failure does not schedule a retry).
        assert_eq!(led.count_policy_events("retry_budget", 100).unwrap(), 1);
        // SIRF-6: released back to todo un-advanced, never promoted.
        let release = calls
            .iter()
            .find(|c| c.contains("amt --json release AMT-21"))
            .expect("issue released");
        assert!(release.contains("--status todo"));
        assert!(!calls.iter().any(|c| c.contains("issue update")));
        // A deadend note was filed.
        assert!(calls.iter().any(|c| c.contains("hayven remember")));
        let (outcome, gate): (String, String) = led
            .conn
            .query_row("SELECT outcome, gate_result FROM iterations", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(outcome, "gate_failed");
        assert_eq!(gate, "fail");
    }

    #[test]
    fn agent_timeout_does_not_consume_retry_budget() {
        // SIRF-7 + SIRF-9: a KILLED agent must deadend immediately without
        // looping — a hung agent should not be retried. Even with retry_budget=3,
        // a timeout yields exactly ONE agent run and zero retry events.
        let m = MockRunner::new();
        program_prefix(&m, "AMT-22");
        m.expect(&["sh", "-c"], 0, ""); // the (single) agent run, then timed out
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-22"}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        m.arm_agent_timeout(1);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config { retry_budget: 3, ..cfg() };
        let o = run_iteration(
            &amt, &hv, &led, &c, &m, "sirius/oak", Some("todo"), "sleep 999", &mut out,
        );
        assert_eq!(o, IterationOutcome::Deadend);
        // Exactly one agent run — the timeout did NOT loop back.
        let agent_runs = m.recorded().iter().filter(|c| **c == "sh -c sleep 999").count();
        assert_eq!(agent_runs, 1, "a killed agent must not consume retries");
        assert_eq!(led.count_policy_events("retry_budget", 100).unwrap(), 0);
    }

    // ---- SIRF-9: honest oracle-verdict recording -----------------------

    #[test]
    fn forced_oracle_verdict_is_recorded_truthfully_in_the_ledger() {
        // SIRF-9: an entity is oracle-conflicted then FORCED. The finished
        // iteration's oracle_verdicts must record "forced" for that entity, not
        // a fabricated "registered".
        let m = MockRunner::new();
        m.expect(&["amt", "--json", "claim"], 0, r#"{"id":"AMT-23","title":"T"}"#);
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        // Lock: e1 → oracle conflict (exit 3), then forced (exit 0 with id).
        m.push(MockResponse::new(&["hayven", "claim"], 3, "", "adjacency"));
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
        // work ok, no changes → gate skipped → completes.
        m.expect(&["sh", "-c"], 0, "");
        m.expect(&["git", "diff"], 0, "");
        m.expect(&["amt", "--json", "decide"], 0, r#"{"id":"D-1","resolves":"AMT-23"}"#);
        m.expect(&["amt", "--json", "decision", "show"], 0, r#"{"id":"D-1","resolves":"AMT-23"}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-23"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config { oracle_202: Oracle202::ForceWithBudget, ..cfg() };
        let o = run_iteration(
            &amt, &hv, &led, &c, &m, "sirius/oak", Some("todo"), "true", &mut out,
        );
        assert_eq!(o, IterationOutcome::Completed);
        // The stored oracle_verdicts JSON reflects the FORCE, not "registered".
        let verdicts: String = led
            .conn
            .query_row("SELECT oracle_verdicts FROM iterations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(verdicts, r#"["forced"]"#);
    }

    #[test]
    fn oracle_backoff_release_records_backoff_not_forced() {
        // SIRF-9: the OracleBackoff finish path BACKED OFF (did not force). The
        // ledger must record "backoff", not the old dishonest "forced".
        let m = MockRunner::new();
        m.expect(&["amt", "--json", "claim"], 0, r#"{"id":"AMT-24","title":"T"}"#);
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        // Lock: e1 → oracle conflict (exit 3), policy is BackOff → back off.
        m.push(MockResponse::new(&["hayven", "claim"], 3, "", "adjacency"));
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-24"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config { oracle_202: Oracle202::BackOff, ..cfg() };
        let o = run_iteration(
            &amt, &hv, &led, &c, &m, "sirius/oak", Some("todo"), "true", &mut out,
        );
        assert_eq!(o, IterationOutcome::ReleasedOverlap);
        let verdicts: String = led
            .conn
            .query_row("SELECT oracle_verdicts FROM iterations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(verdicts, r#"["backoff"]"#);
    }
}
