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

/// A phase in the iteration, used in NDJSON `phase` fields. `review` and `fix`
/// appear only when the SIRF-23 review stage is enabled (`review.cmd`).
pub const PHASES: &[&str] = &[
    "claim", "map", "lock", "brief", "work", "gate", "review", "fix", "receipt", "release",
];

/// What an isolated (fleet) iteration knows about its workspace. `None` in
/// `run_iteration` means "not isolated" (tests / a user's own checkout): no
/// resets, no per-issue branches, and no review stage.
#[derive(Debug, Clone)]
pub struct Fleet {
    /// The launch base commit every worktree was created from. SIRF-42: an
    /// iteration resets to the FRESH tip of the fleet's base ref instead
    /// ([`iteration_base`]) and falls back to this only when no base ref
    /// exists or it does not resolve.
    pub base: String,
    /// The branch HEAD pointed to at launch — the default `review.base_ref`
    /// for `current-base-merge`. `None` when launched on a detached HEAD.
    pub base_ref: Option<String>,
    /// This worker's private worktree (absolute).
    pub worktree: std::path::PathBuf,
    /// The repo's `.sirius/` directory (absolute): review files live under
    /// `reviews/`, throwaway merge trees under `worktrees/`.
    pub sirius_dir: std::path::PathBuf,
    /// The reviewer prompt template (`review.prompt_file`, or the default).
    pub review_prompt: String,
    /// SIRF-26: set (to the agent's message) when an agent hits a usage/plan
    /// limit. Shared by every worker of the fleet: once set, nobody claims
    /// again — the board is left alone instead of churned through.
    pub pause: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// SIRF-27: set (to the triggering message) once the fleet switched to
    /// `models.fallback`. Shared like `pause`.
    pub fallback: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// SIRF-31: this fleet's work currently between review and release,
    /// shared by every worker. Completed siblings are found by their
    /// `sirius/*` branch, but two workers reviewing AT THE SAME TIME have no
    /// branch yet — the Lydgr case (parallel workers each generating the
    /// next migration). The peer that reached review first owns a slot.
    pub inflight: std::sync::Arc<std::sync::Mutex<Inflight>>,
}

/// See [`Fleet::inflight`].
#[derive(Debug, Default)]
pub struct Inflight {
    next: u64,
    peers: Vec<InflightPeer>,
}

#[derive(Debug, Clone)]
struct InflightPeer {
    seq: u64,
    worker: String,
    issue: String,
    head: String,
}

impl Fleet {
    /// Pause the whole fleet (first reason wins).
    pub fn pause_with(&self, reason: &str) {
        let mut p = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        if p.is_none() {
            *p = Some(reason.to_string());
        }
    }

    pub fn paused(&self) -> Option<String> {
        self.pause.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Record (or update) `worker`'s checkpoint under review for `issue`. A
    /// re-registration of the same issue keeps its place in line.
    pub fn register_inflight(&self, worker: &str, issue: &str, head: &str) {
        let mut g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = g
            .peers
            .iter_mut()
            .find(|p| p.worker == worker && p.issue == issue)
        {
            p.head = head.to_string();
            return;
        }
        g.peers.retain(|p| p.worker != worker);
        g.next += 1;
        let seq = g.next;
        g.peers.push(InflightPeer {
            seq,
            worker: worker.to_string(),
            issue: issue.to_string(),
            head: head.to_string(),
        });
    }

    /// `(issue, head)` of every peer that reached review before `worker`.
    pub fn peers_before(&self, worker: &str) -> Vec<(String, String)> {
        let g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mine) = g.peers.iter().find(|p| p.worker == worker).map(|p| p.seq) else {
            return Vec::new();
        };
        g.peers
            .iter()
            .filter(|p| p.seq < mine)
            .map(|p| (p.issue.clone(), p.head.clone()))
            .collect()
    }

    /// `worker`'s iteration ended (its work is stamped + released, or gone).
    pub fn clear_inflight(&self, worker: &str) {
        let mut g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        g.peers.retain(|p| p.worker != worker);
    }

    /// Is the fleet running on its fallback tier?
    pub fn on_fallback(&self) -> bool {
        self.fallback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Switch the fleet to its fallback tier; true for the FIRST switcher.
    fn switch_to_fallback(&self, reason: &str) -> bool {
        let mut f = self.fallback.lock().unwrap_or_else(|e| e.into_inner());
        if f.is_none() {
            *f = Some(reason.to_string());
            true
        } else {
            false
        }
    }
}

/// What a usage-limit hit does (SIRF-27).
enum LimitAction {
    /// Retry on the fallback tier (the agent ran on the primary tier and a
    /// fallback exists). `first` = this hit made the switch.
    Fallback { first: bool },
    /// Pause the whole fleet (no fallback, or it ran on the fallback already).
    Pause,
}

/// Decide by the tier the failed agent ACTUALLY ran on: a hit on the primary
/// tier switches (even if a sibling already switched — that hit was still a
/// primary one); a hit on the fallback tier has nowhere left to go.
fn on_usage_limit(
    fleet: &Fleet,
    cfg: &Config,
    ran_on_fallback: bool,
    stop: &crate::models::FleetStop,
) -> LimitAction {
    // A logged-out CLI fails on EVERY tier — falling back would only waste a
    // spawn per worker and misreport the cause. Pause straight away.
    let tier_specific = stop.kind != crate::models::StopKind::Login;
    if tier_specific && !ran_on_fallback && cfg.models.fallback.is_some() {
        LimitAction::Fallback {
            first: fleet.switch_to_fallback(&stop.line),
        }
    } else {
        fleet.pause_with(&stop.line);
        LimitAction::Pause
    }
}

/// Announce the switch to the fallback tier (once per fleet): NDJSON + stderr.
fn announce_fallback(out: &mut dyn Write, worker: &str, issue: &str, line: &str, cfg: &Config) {
    let fb = crate::models::active(&cfg.models, true);
    let _ = out.write_all(
        format!(
            "{}\n",
            json!({"event": "fleet", "phase": "fallback", "worker": worker, "issue": issue,
                   "reason": line,
                   "models": {"default": fb.default, "review": crate::models::review_model(fb)}})
        )
        .as_bytes(),
    );
    eprintln!(
        "sirius: fleet stop (\"{line}\") — the fleet switched to its FALLBACK models (workers {}, reviewer {})",
        fb.default.as_deref().unwrap_or("?"),
        crate::models::review_model(fb).as_deref().unwrap_or("?")
    );
}

/// Fill `{issue}` / `{worker}` / `{model}` in an agent or reviewer command
/// (SIRF-22 #4, SIRF-26). Issue keys and worker ids are `[A-Za-z0-9/_-]`;
/// explicit model ids are too, but an ALIAS like `fable[1m]` contains glob
/// characters — quote `"{model}"` in commands (launch warns on aliases). With
/// no model, `{model}` is left as-is (launch refuses that — `check_models`).
pub fn template_cmd(cmd: &str, issue: &str, worker: &str, model: Option<&str>) -> String {
    let out = cmd.replace("{issue}", issue).replace("{worker}", worker);
    match model {
        Some(m) => out.replace("{model}", m),
        None => out,
    }
}

/// The last value of `k` in an env list ("" if absent).
fn env_value<'e>(env: &'e [(String, String)], k: &str) -> &'e str {
    env.iter()
        .rev()
        .find(|(key, _)| key == k)
        .map(|(_, v)| v.as_str())
        .unwrap_or_default()
}

/// An owned env pair.
fn kv(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_string(), v.into())
}

/// The result of the WORK⇄GATE loop (also reused by review fix rounds).
pub enum WorkGate {
    /// The loop ran to a verdict. `exit`/`log` describe the LAST agent run, so
    /// a release comment can say what actually failed (SIRF-22 #8).
    Done {
        work_ok: bool,
        gate_result: &'static str,
        exit: Option<i32>,
        log: Option<std::path::PathBuf>,
        /// The agent was killed by the timeout. Only reported for FIX rounds
        /// (a timed-out WORK pass is terminal → `Exit`): the review stage must
        /// revert to its last reviewed checkpoint, not abandon passing work.
        timed_out: bool,
    },
    /// A terminal path (lease lost, agent killed by timeout) that already did
    /// its own release/ledger bookkeeping — the caller just returns this.
    Exit(IterationOutcome),
}

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
    // ONE write_all per event: `writeln!` may issue several write() calls for
    // a single line, which interleaves lines when workers share stdout.
    let _ = out.write_all(format!("{obj}\n").as_bytes());
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

/// Ledger writes are best-effort — the loop must not die on telemetry — but
/// never SILENT: a field audit found fleets that ran for hours against a
/// broken ledger with zero diagnostics (every failure was `.ok()`-swallowed).
/// Any failed write now says so on stderr.
pub(crate) fn ledger_warn<T>(what: &str, r: rusqlite::Result<T>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("sirius: ledger write failed ({what}): {e}");
            None
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
    /// OPERATIONAL failure (daemon unreachable, wrong project, crashed CLI) —
    /// not contention. Kept distinct from OracleBackoff so the caller returns
    /// `IterationOutcome::Error` and the run loop's error budget can trip;
    /// folding this into the backoff path made a wrong-project daemon retry
    /// forever with the error counter reset on every pass.
    Failed {
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
                        ledger_warn("log_policy_event", ledger.log_policy_event(
                                None,
                                "claim_anomaly",
                                &json!({"issue": issue, "entity": ent, "detail": "claim registered without a claim id — cannot manage lease"}),
                            ));
                        return LockResult::Overlap {
                            blocker: format!("{ent}: claim registered without a claim id"),
                            acquired,
                        };
                    }
                }
            }
            ClaimVerdict::Overlap { detail } => {
                ledger_warn(
                    "log_policy_event",
                    ledger.log_policy_event(
                        None,
                        "backoff_409",
                        &json!({"issue": issue, "entity": ent, "detail": detail}),
                    ),
                );
                return LockResult::Overlap {
                    blocker: format!("{ent}: {detail}"),
                    acquired,
                };
            }
            ClaimVerdict::OracleConflict { detail } => {
                ledger_warn("log_policy_event", ledger.log_policy_event(None, "oracle_202", &json!({"issue": issue, "entity": ent, "policy": format!("{:?}", config.oracle_202)})));
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
                            ClaimVerdict::Registered { claim_id: Some(id) } => {
                                acquired.push(id);
                                verdicts.push("forced");
                            }
                            ClaimVerdict::Registered { claim_id: None } => {
                                ledger_warn("log_policy_event", ledger.log_policy_event(
                                        None,
                                        "claim_anomaly",
                                        &json!({"issue": issue, "entity": ent, "detail": "forced claim registered without a claim id — cannot manage lease"}),
                                    ));
                                return LockResult::Overlap {
                                    blocker: format!(
                                        "{ent}: forced claim registered without a claim id"
                                    ),
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
                return LockResult::Failed { detail, acquired };
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
                eprintln!("sirius: FAILED to release hayven claim {claim_id} for {issue}: {e}");
                ledger_warn(
                    "log_policy_event",
                    ledger.log_policy_event(
                        None,
                        "release_failure",
                        &json!({"issue": issue, "claim_id": claim_id, "error": e}),
                    ),
                );
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
                ledger_warn(
                    "log_policy_event",
                    ledger.log_policy_event(
                        None,
                        "release_failure",
                        &json!({"issue": issue, "claim_id": issue, "worker": worker, "error": e}),
                    ),
                );
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
    /// SIRF-26: an agent hit a usage/plan limit — the issue went back to
    /// `todo` untouched and the whole fleet stops claiming.
    Paused(String),
}

/// Clears a worker's in-flight registration however its iteration ends —
/// a panic included (a phantom peer would block every later slot).
struct InflightGuard<'f> {
    fleet: Option<&'f Fleet>,
    worker: String,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        if let Some(f) = self.fleet {
            f.clear_inflight(&self.worker);
        }
    }
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
    spine: Option<&crate::spine::Spine>,
    // When Some: `runner` is scoped to this worker's PRIVATE git worktree,
    // and the iteration may safely hard-reset it to its base and put each
    // issue's work on its own branch. NEVER pass Some for a runner that
    // targets the user's own checkout — the reset would destroy their work.
    fleet: Option<&Fleet>,
) -> IterationOutcome {
    let _inflight = InflightGuard {
        fleet,
        worker: worker.to_string(),
    };
    ledger_warn("upsert_worker", ledger.upsert_worker(worker, "working"));

    // Suite spine (§2): past-tense job/gate/receipt facts to <root>/.suite/.
    // Best-effort and optional (None disables it, e.g. in tests). `out` is the
    // NDJSON stream; the spine is a separate file sink with its own vocabulary
    // (job.*/gate.*/receipt.* vs iteration phases), so the two never collide.
    let emit_spine = |ty: &str, refs: Vec<String>, data: Value| {
        if let Some(sp) = spine {
            sp.emit(ty, refs, data);
        }
    };
    let emit_job = |ty: &str, issue: &str| {
        if let Some(sp) = spine {
            sp.emit(
                ty,
                vec![
                    crate::spine::issue_ref(issue),
                    crate::spine::worker_ref(worker),
                ],
                json!({ "issue": issue, "worker": worker }),
            );
        }
    };

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
            ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
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
    // SIRF-26: route this TICKET to a model by its labels (first match wins);
    // fix rounds of un-routed tickets rise to `models.fix_floor`.
    let labels: Vec<String> = issue_val
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    // The tier in force at claim time (shown on the claim event); each spawn
    // re-reads it, since the fleet may switch to its fallback mid-iteration.
    let claim_tier = crate::models::active(&config.models, fleet.is_some_and(Fleet::on_fallback));
    let model_work = crate::models::model_for(claim_tier, &labels, "work");
    let model_review = crate::models::review_model(claim_tier);
    // A failed start_iteration is warned (not fatal) and leaves iter_id = -1;
    // `iter_ref` keeps that sentinel OUT of policy_events rows, whose FK on
    // iterations(id) would reject -1 — another formerly-silent write failure.
    let iter_id = ledger_warn(
        "start_iteration",
        ledger.start_iteration(worker, Some(&issue)),
    )
    .unwrap_or(-1);
    let iter_ref = (iter_id >= 0).then_some(iter_id);
    let start = std::time::Instant::now();
    // The branch this fleet lands on (the review's base ref, else the branch
    // launched from) — exported as SIRIUS_BASE_REF below, and the ref this
    // iteration's base is resolved from.
    let base_ref: Option<String> = config
        .review
        .base_ref
        .clone()
        .or_else(|| fleet.and_then(|f| f.base_ref.clone()));
    // SIRF-42: the base THIS iteration builds on is the base ref's tip NOW,
    // not the commit snapshotted at launch — in a serial blocker chain every
    // later ticket must see its merged predecessors. Resolved once here and
    // used for everything this iteration used the launch base for: the
    // worktree reset, the gate baseline / SIRIUS_BASE, preserved-work
    // containment, and the review's diff ranges.
    let iter_base: Option<String> =
        fleet.map(|f| iteration_base(runner, base_ref.as_deref(), &f.base));
    let isolate_base: Option<&str> = iter_base.as_deref();
    let mut claim_ev =
        json!({"claimed": true, "title": title, "model": model_work, "review_model": model_review});
    if let Some(b) = isolate_base {
        claim_ev["base_sha"] = json!(b);
    }
    emit_event(out, worker, Some(&issue), "claim", claim_ev);
    // Durable: the Ametrite claim + ledger.start_iteration above.
    emit_job("job.dispatched", &issue);

    // ISOLATED WORKSPACE (parallel fleet): reset the private worktree to the
    // fleet base, DETACHED. Detached is load-bearing: working on a named
    // branch parks it "checked out" in this worktree, and git then refuses
    // the same branch in a sibling worktree — a re-claimed issue would
    // ping-pong as spurious errors between workers. The per-issue branch is
    // stamped at completion instead (see the preserve step). Resetting is
    // safe ONLY because the worktree belongs to this worker alone; abandoned
    // leftovers being discarded were already recorded as deadends. Runs AFTER
    // start_iteration + the claim event so even a prep failure leaves the
    // documented one-iterations-row-per-pass audit trail and a visible
    // release on the NDJSON stream.
    //
    // SIRF-41 asked for "a named branch from the start; never a detached
    // HEAD" so a killed agent's commits are never left dangling. The INTENT
    // — no work is ever unreferenced — is met with refs instead: every
    // non-advancing exit pins the worktree's work at refs/sirius/wip*/<issue>
    // BEFORE the issue is released ([`preserve_wip`]), and a branch an agent
    // checks out is pinned and detached after it exits ([`guard_branch`]).
    // The worktree itself stays detached for the sibling-worktree reason
    // above.
    //
    // The leading plumbing detach (HEAD := base, no branch moved, index and
    // files untouched) makes sure the `reset --hard` that follows can never
    // drag a branch some earlier process left checked out here back to the
    // base — and, unlike `checkout --detach`, it works on an unborn branch
    // and with an unmerged index, so such a leftover cannot wedge the worker.
    if let Some(base) = isolate_base {
        for args in [
            &["update-ref", "--no-deref", "HEAD", base][..],
            &["reset", "--hard", base][..],
            &["clean", "-fd"][..],
            &["checkout", "--detach", base][..],
        ] {
            if let Err(detail) = crate::gitrange::run_git(runner, args) {
                // A worktree we cannot reset is a workspace we cannot trust —
                // fail the iteration rather than gate over an unknown baseline.
                // Nothing to preserve (SIRF-41): no work for THIS issue has
                // started, and whatever the tree still holds was preserved
                // (or stamped) when its own iteration released.
                release_issue_checked(
                    amt,
                    ledger,
                    &issue,
                    worker,
                    Some("todo"),
                    Some("sirius: released — worktree preparation failed"),
                );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "release",
                    json!({"reason": "worktree_prep_failed", "detail": detail}),
                );
                ledger_warn(
                    "finish_iteration",
                    ledger.finish_iteration(
                        iter_id,
                        &[],
                        "error",
                        None,
                        &[],
                        None,
                        Some(start.elapsed().as_millis() as i64),
                        None,
                    ),
                );
                ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
                emit_job("job.blocked", &issue);
                return IterationOutcome::Error(format!(
                    "worktree prep failed (git {}): {detail}",
                    args.join(" ")
                ));
            }
        }
    }
    // SIRF-32: work a red frontier held back is RESUMED, not redone: merge
    // the exact held commit (if its branch still points there) onto the base.
    // SIRF-41: so is work an interrupted iteration preserved (refs/sirius/wip);
    // work that was judged and failed is only pointed at (SIRIUS_PRIOR_WORK).
    let resume: Resume = match isolate_base {
        Some(base) => resume_work(amt, runner, &issue, worker, base),
        None => Resume::default(),
    };
    let resumed: Option<String> = resume.resumed.clone();
    // What this iteration starts from: the base plus anything resumed. Work
    // is "new" (worth preserving) only past this point — resumed work is
    // still referenced by the ref it came from.
    let work_floor: Option<String> = isolate_base.map(|base| match &resumed {
        Some(_) => crate::gitrange::head_rev(runner).unwrap_or_else(|_| base.to_string()),
        None => base.to_string(),
    });
    // SIRF-41: preserve the worktree's work BEFORE any non-advancing release
    // — another worker may claim the issue the instant it is released, and
    // this worker's next reset wipes the tree. Fleet-only (a user's own
    // checkout is never committed to); a no-op when nothing new exists.
    let preserve = |kind: WipKind| -> WipResult {
        match (isolate_base, &work_floor) {
            (Some(base), Some(floor)) => preserve_wip(runner, &issue, floor, base, kind),
            _ => Ok(None),
        }
    };
    // Every release back to `todo` short of the final one goes through here,
    // so no release path can skip the preserve step. The comment names where
    // the work went; the caller adds `wip_ref` to its release event.
    let release_back = |comment: &str, kind: WipKind| -> WipResult {
        let wip = preserve(kind);
        release_issue_checked(
            amt,
            ledger,
            &issue,
            worker,
            Some("todo"),
            Some(&format!("{comment}{}", wip_note(&wip))),
        );
        wip
    };

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
    ledger_warn("log_policy_event", ledger.log_policy_event(iter_ref, "concurrency", &json!({"claim_mode": format!("{:?}", config.claim_mode), "decision": format!("{:?}", decision)})));

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
                let wip = release_back(
                    &format!("sirius: released — entity claim blocked by {blocker}"),
                    WipKind::Resume,
                );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "release",
                    with_wip(
                        json!({"reason": "entity_overlap", "blocker": blocker}),
                        &wip,
                    ),
                );
                ledger_warn(
                    "finish_iteration",
                    ledger.finish_iteration(
                        iter_id,
                        &entities,
                        "released",
                        None,
                        &["blocked".into()],
                        None,
                        Some(start.elapsed().as_millis() as i64),
                        None,
                    ),
                );
                ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
                emit_job("job.blocked", &issue);
                return IterationOutcome::ReleasedOverlap;
            }
            LockResult::OracleBackoff { detail, acquired } => {
                release_entities(hv, ledger, &issue, &acquired);
                let wip = release_back(
                    &format!("sirius: released — oracle/claim backoff: {detail}"),
                    WipKind::Resume,
                );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "release",
                    with_wip(json!({"reason": "oracle_backoff", "detail": detail}), &wip),
                );
                ledger_warn(
                    "finish_iteration",
                    ledger.finish_iteration(
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
                    ),
                );
                ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
                emit_job("job.blocked", &issue);
                return IterationOutcome::ReleasedOverlap;
            }
            LockResult::Failed { detail, acquired } => {
                // Operational failure — release what we hold, hand the issue
                // back, and surface an ERROR so the run loop's budget/backoff
                // applies (a wrong-project daemon must not masquerade as
                // contention and retry forever).
                release_entities(hv, ledger, &issue, &acquired);
                let wip = release_back(
                    &format!("sirius: released — hayven claim error: {detail}"),
                    WipKind::Resume,
                );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "release",
                    with_wip(json!({"reason": "claim_error", "detail": detail}), &wip),
                );
                ledger_warn(
                    "finish_iteration",
                    ledger.finish_iteration(
                        iter_id,
                        &entities,
                        "error",
                        None,
                        &["error".into()],
                        None,
                        Some(start.elapsed().as_millis() as i64),
                        None,
                    ),
                );
                ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
                emit_job("job.blocked", &issue);
                return IterationOutcome::Error(format!("hayven claim failed: {detail}"));
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
    // SIRF-11: pin the pre-work baseline BEFORE the agent runs. The gate diffs
    // against this commit, not bare HEAD — agents routinely COMMIT their work,
    // and a worktree-vs-HEAD diff over a committed change is empty, which used
    // to skip the gate and advance the issue with zero tests run. Best-effort:
    // if HEAD is unresolvable (fresh repo), the gate falls back to the old diff.
    // In an isolated worktree the baseline IS the fleet base (the reset above
    // guarantees it), and the post-clean tree has no pre-existing untracked
    // files to snapshot.
    // A baseline that cannot be captured is DOUBT, not "empty" — a silently
    // empty untracked snapshot would attribute every pre-existing scratch
    // file to the agent, and a silently missing head would fall back to the
    // HEAD-blind diff SIRF-11 exists to prevent. `baseline_err` forces the
    // gate through the doubt path below.
    let mut baseline_err: Option<String> = None;
    let (pre_head, pre_untracked) = match isolate_base {
        Some(base) => (Some(base.to_string()), Vec::new()),
        None => {
            let head = match crate::gitrange::head_rev(runner) {
                Ok(h) => Some(h),
                Err(e) => {
                    baseline_err = Some(format!("cannot resolve pre-work HEAD: {e}"));
                    None
                }
            };
            // Snapshot the untracked files that ALREADY exist — pre-existing
            // scratch files are not the agent's change, and counting them
            // would make every iteration gate (and potentially advance) over
            // work nobody did.
            let untracked = match crate::gitrange::untracked_files(runner) {
                Ok(u) => u,
                Err(e) => {
                    baseline_err = Some(format!("cannot snapshot untracked files: {e}"));
                    Vec::new()
                }
            };
            (head, untracked)
        }
    };
    // The shared lease renewal: fired before every agent spawn and on the
    // heartbeat while the agent (or reviewer) runs.
    let renew = || {
        // Renew the Ametrite issue lease. Mid-run we cannot abort the child
        // from here, but a refusal is never silent.
        if let Err(e) = amt.heartbeat(&issue, worker) {
            eprintln!("sirius: {e} for {issue} (agent still running)");
        }
        // Renew each held Hayvenhurst entity claim (re-claim = refresh) — and
        // say so when the renewal comes back as anything but ours.
        if !held_entities.is_empty() {
            match hv.claim(&held_entities, &lock_intent, false) {
                crate::hayven::ClaimVerdict::Registered { .. } => {}
                other => eprintln!(
                    "sirius: entity lease renewal for {issue} returned {other:?} (agent still running)"
                ),
            }
        }
    };
    // A refused lease refresh mid-iteration: the issue may now belong to
    // someone else. Release OUR entity claims, record the error, and do NOT
    // release the issue — the lease is not ours to release. Shared by the
    // pre-spawn check (work/fix) and the pre-review check (SIRF-23).
    let lease_lost = |out: &mut dyn Write, reason: &str, when: &str| -> IterationOutcome {
        release_entities(hv, ledger, &issue, &claim_ids);
        // SIRF-41: the issue is not ours to release, but the work in this
        // tree is — and the next reset would wipe it. Pin it, and say where
        // on the board (whoever holds the issue now resumes it next claim).
        let wip = preserve(WipKind::Resume);
        if !matches!(wip, Ok(None)) {
            let _ = amt.comment_as(
                &issue,
                &format!(
                    "sirius: lost the lease before {when} ({reason}){}",
                    wip_note(&wip)
                ),
                worker,
            );
        }
        emit_event(
            out,
            worker,
            Some(&issue),
            "release",
            with_wip(json!({"reason": "lease_lost", "detail": reason}), &wip),
        );
        ledger_warn(
            "finish_iteration",
            ledger.finish_iteration(
                iter_id,
                &entities,
                "error",
                None,
                &oracle_verdicts,
                None,
                Some(start.elapsed().as_millis() as i64),
                None,
            ),
        );
        ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
        emit_job("job.blocked", &issue);
        IterationOutcome::Error(format!("lease on {issue} lost before {when}: {reason}"))
    };
    // SIRF-26: an agent hit a usage limit. Hand the issue back to `todo`
    // UNTOUCHED (it never got a real attempt), say why on the board, and
    // stop — the worker loop sees `Paused` and the fleet stops claiming.
    let usage_paused = |out: &mut dyn Write, line: &str| -> IterationOutcome {
        release_entities(hv, ledger, &issue, &claim_ids);
        // The agent may have worked for a while before the limit hit: that
        // partial work is resumed on the next claim (SIRF-41).
        let wip = release_back(
            &format!(
                "sirius: fleet paused — a fleet-wide stop (\"{line}\"). {issue} is back in todo (not advanced, no deadend filed); restart the fleet once the limit resets."
            ),
            WipKind::Resume,
        );
        emit_event(
            out,
            worker,
            Some(&issue),
            "release",
            with_wip(
                json!({"reason": "usage_limit", "detail": line, "advanced": false}),
                &wip,
            ),
        );
        ledger_warn(
            "finish_iteration",
            ledger.finish_iteration(
                iter_id,
                &entities,
                "released",
                None,
                &oracle_verdicts,
                None,
                Some(start.elapsed().as_millis() as i64),
                None,
            ),
        );
        ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
        emit_spine(
            "fleet.paused",
            vec![crate::spine::issue_ref(&issue)],
            json!({"issue": issue.as_str(), "reason": line}),
        );
        emit_job("job.blocked", &issue);
        IterationOutcome::Paused(line.to_string())
    };
    // Renew now and SAY whether the amt lease is still ours (a transient amt
    // failure only warns — the lease may well still be ours).
    let renew_checked = || -> Result<(), String> {
        match amt.heartbeat(&issue, worker) {
            Ok(()) => {}
            Err(crate::amt::HeartbeatError::Refused(reason)) => return Err(reason),
            Err(e) => eprintln!("sirius: {e} for {issue} (continuing — may be transient)"),
        }
        if !held_entities.is_empty() {
            match hv.claim(&held_entities, &lock_intent, false) {
                crate::hayven::ClaimVerdict::Registered { .. } => {}
                other => eprintln!("sirius: entity lease renewal for {issue} returned {other:?}"),
            }
        }
        Ok(())
    };
    // The env every agent/reviewer process gets (SIRF-22 #4/#5, SIRF-23).
    let base_env: Vec<(String, String)> = vec![
        kv("SIRIUS_ISSUE", issue.clone()),
        kv("SIRIUS_WORKER", worker),
        kv("AMT_AGENT", worker),
        kv(
            "SIRIUS_WORKTREE",
            fleet
                .map(|f| f.worktree.display().to_string())
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|d| d.display().to_string())
                })
                .unwrap_or_default(),
        ),
        kv("SIRIUS_BASE", pre_head.clone().unwrap_or_default()),
        kv(crate::shell::BG_WAIT_CEILING_ENV, "0"), // SIRF-52: sirius owns the timeout
    ];
    let mut base_env = base_env;
    if let Some(sha) = &resumed {
        base_env.push(kv("SIRIUS_RESUMED_FROM", sha.as_str()));
    }
    // SIRF-41: which ref the resumed work came from, and earlier work that
    // was judged and failed (gate deadend / review release) — not merged,
    // but there for the agent to look at.
    if let Some(r) = &resume.resume_ref {
        base_env.push(kv("SIRIUS_RESUME_REF", r.as_str()));
    }
    if let Some((r, _)) = &resume.prior_work {
        base_env.push(kv("SIRIUS_PRIOR_WORK", r.as_str()));
    }
    // The branch this fleet lands on (the review's base ref, else the branch
    // launched from): `sirius link --changed` never stamps commits already on
    // it as this issue's work. The SAME ref this iteration's base came from.
    if let Some(r) = base_ref.clone() {
        base_env.push(kv("SIRIUS_BASE_REF", r));
    }
    // WORK⇄GATE as a reusable unit: the initial work pass (phase `work`) and
    // every review fix round (phase `fix`) run through the same supervision,
    // heartbeat, timeout, log capture, baseline diff, and retry budget.
    let run_work_gate = |out: &mut dyn Write,
                         phase: &str,
                         round: u32,
                         extra_env: &[(String, String)]|
     -> WorkGate {
        let mut work_ok;
        let mut gate_result;
        let mut attempt: u32 = 0;
        let mut last_exit: Option<i32>;
        let mut last_log: Option<std::path::PathBuf>;
        // SIRF-50: the env-fault re-gate (re-run worktree setup, gate again)
        // happens at most ONCE per WORK⇄GATE unit, and only in a fleet
        // worktree with a setup command (cmd_run resolves an unset
        // `worktree.setup_cmd` to the detected one before any worker starts).
        let mut setup_rerun_used = false;
        let setup_cmd = isolate_base
            .and(config.worktree.setup_cmd.as_deref())
            .filter(|c| !c.trim().is_empty());
        // SIRF-48: the previous attempt's failing gate output, handed to the
        // next attempt as SIRIUS_LAST_GATE_TAIL.
        let mut last_gate_tail: Option<String> = None;
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
            // Pre-spawn lease check: a REFUSED refresh (amt's exit-0
            // `claimed:false` answer — the lease lapsed and someone else may hold
            // the issue) must ABORT before the expensive agent spawns; warning
            // and spawning anyway (the old behavior) put two agents on one issue.
            // A transient `Failed` (amt hiccup) only warns — the lease may well
            // still be ours.
            // SIRF-26: another worker hit a usage limit — spawn nothing more.
            if let Some(line) = fleet.and_then(Fleet::paused) {
                if phase == "work" {
                    return WorkGate::Exit(usage_paused(out, &line));
                }
                return WorkGate::Done {
                    work_ok: false,
                    gate_result: "skipped",
                    exit: None,
                    log: None,
                    timed_out: false,
                };
            }
            match amt.heartbeat(&issue, worker) {
                Ok(()) => {}
                Err(crate::amt::HeartbeatError::Refused(reason)) => {
                    return WorkGate::Exit(lease_lost(out, &reason, "agent spawn"));
                }
                Err(e) => eprintln!("sirius: {e} for {issue} (continuing — may be transient)"),
            }
            let mut heartbeat = || renew();
            // The agent contract (SIRF-22 #4/#5): the agent is TOLD which issue
            // it claimed, as whom, where, and in which phase — no more
            // reverse-engineering the worker id from the worktree name — and
            // AMT_AGENT attributes its board writes to `sirius/<tree>`, not the
            // human running the fleet.
            let mut env = base_env.clone();
            env.push(kv("SIRIUS_PHASE", phase));
            // SIRF-26: an explicit model, never the CLI's silent global
            // default. ANTHROPIC_MODEL steers Claude Code (it wins over
            // settings.json — verified); SIRIUS_MODEL serves other agents.
            let ran_on_fallback = fleet.is_some_and(Fleet::on_fallback);
            let phase_model = crate::models::model_for(
                crate::models::active(&config.models, ran_on_fallback),
                &labels,
                if phase == "fix" { "fix" } else { "work" },
            );
            let phase_model = phase_model.as_deref();
            if let Some(m) = phase_model {
                env.push(kv("ANTHROPIC_MODEL", m));
                env.push(kv("SIRIUS_MODEL", m));
            }
            env.extend(extra_env.iter().cloned());
            if let Some(t) = &last_gate_tail {
                env.push(kv("SIRIUS_LAST_GATE_TAIL", t.as_str()));
            }
            let log_path = agent_log_path(
                fleet,
                &format!(
                    "{}{}",
                    if phase == "work" {
                        issue.clone()
                    } else {
                        format!("{issue}-{phase}-r{round}")
                    },
                    // A fallback retry runs within the same second as the
                    // failed primary attempt — keep both logs as evidence.
                    if ran_on_fallback { "-fb" } else { "" }
                ),
            );
            // SIRF-41: hard cap + idle watchdog + finish grace, per label.
            let limits = config.agent_timeouts(&labels);
            let opts = AgentRunOpts {
                timeout: Duration::from_secs(limits.hard_secs),
                heartbeat_interval: Duration::from_secs(config.heartbeat_interval_secs()),
                log_path: log_path.clone(),
                env,
            };
            let mut probe = || crate::shell::worktree_probe(runner);
            let mut watch = crate::shell::ProgressWatch {
                idle: Duration::from_secs(limits.idle_secs),
                probe_every: Duration::from_secs((limits.idle_secs / 3).clamp(1, 30)),
                grace: Duration::from_secs(crate::config::FINISH_GRACE_SECS),
                probe: &mut probe,
            };
            let cmd = template_cmd(agent_cmd, &issue, worker, phase_model);
            // SF-15: the agent command is the operator's own string (template_cmd
            // only fills {issue}/{worker}/{model}), so it gets the same explicit
            // shell resolution as gate.test_cmd. Spawning the literal program
            // `sh` inherited whoever launched sirius: from PowerShell on a box
            // with no `sh` on PATH, no agent could ever start. (The REVIEW spawn
            // below deliberately stays on `sh`: it runs a script sirius builds in
            // POSIX syntax, which cmd.exe could not run even if we handed it over.)
            let sh = crate::shell::resolve_shell();
            let work = runner.run_agent_watched(
                &sh.program,
                &[sh.flag.as_str(), &cmd],
                &opts,
                &mut watch,
                &mut heartbeat,
            );
            // SIRF-49: an agent that checked out a named branch would have
            // it parked here (blocking sibling worktrees) and wiped by the
            // next reset. Pin its commit and detach — first thing, before any
            // commit sirius makes could land on the agent's branch.
            if isolate_base.is_some() {
                let floor = work_floor.as_deref().or(isolate_base).unwrap_or_default();
                if let Some(g) = guard_branch(runner, &issue, floor) {
                    let ev = json!({"event": "branch_guard", "worker": worker, "issue": issue.as_str(),
                                    "phase": phase, "branch": g.branch, "sha": g.sha,
                                    "ref": g.keep_ref, "detached": g.detached});
                    let _ = out.write_all(format!("{ev}\n").as_bytes());
                }
            }
            let timed_out = work.as_ref().map(AgentOutcome::timed_out).unwrap_or(false);
            let timeout_kind = work.as_ref().ok().and_then(AgentOutcome::timeout_kind);
            work_ok = work.as_ref().map(AgentOutcome::success).unwrap_or(false);
            // SIRF-52: the CLI killed the agent's helper tasks on its way out —
            // an INCOMPLETE run, whatever its exit code says.
            let bg_killed = !timed_out && crate::shell::bg_tasks_killed_in(log_path.as_deref());
            work_ok &= !bg_killed;
            // Agent exit code, from the captured output (durably logged by the runner).
            let agent_code = work.as_ref().ok().and_then(|w| w.output().code);
            // A spawn-level failure (sh missing, fork failure) used to be dropped
            // entirely — the event now carries it so "the agent never ran" is
            // distinguishable from "the agent ran and failed".
            let spawn_err = work.as_ref().err().map(|e| e.to_string());
            last_exit = agent_code;
            last_log = log_path.clone();
            let mut ev = json!({"agent_ok": work_ok, "timed_out": timed_out, "exit": agent_code, "attempt": attempt + 1, "spawn_error": spawn_err, "model": phase_model, "tier": if ran_on_fallback { "fallback" } else { "primary" }});
            if phase != "work" {
                ev["round"] = json!(round);
            }
            if let Some(k) = timeout_kind {
                ev["timeout_kind"] = json!(k.as_str());
            }
            if bg_killed {
                ev["reason"] = json!("agent_bg_tasks_killed");
            }
            emit_event(out, worker, Some(&issue), phase, ev);

            // SIRF-26: a usage/plan limit is not a per-issue failure — every
            // later claim would fail the same way in seconds (observed: the
            // Lydgr fleet bounced its board after the limit hit). Pause the
            // whole fleet; leave this issue in `todo` untouched.
            if !work_ok {
                let hit = fleet.zip(
                    log_path
                        .as_ref()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .and_then(|log| crate::models::fleet_stop(&log)),
                );
                let limit = match hit {
                    Some((f, stop)) => match on_usage_limit(f, config, ran_on_fallback, &stop) {
                        // SIRF-27: retry THIS phase right away on the fallback
                        // tier — no release, no re-claim, leases kept. Bounded:
                        // a second hit runs on the fallback and pauses.
                        LimitAction::Fallback { first } => {
                            let line = &stop.line;
                            if first {
                                announce_fallback(out, worker, &issue, line, config);
                                emit_spine(
                                    "fleet.fallback",
                                    vec![crate::spine::issue_ref(&issue)],
                                    json!({"issue": issue.as_str(), "reason": line}),
                                );
                            }
                            continue;
                        }
                        LimitAction::Pause => Some(stop.line),
                    },
                    None => None,
                };
                if let Some(line) = limit {
                    if phase == "work" {
                        return WorkGate::Exit(usage_paused(out, &line));
                    }
                    // A fix round: the review stage reverts to the last
                    // reviewed state and escalates; the fleet still stops.
                    return WorkGate::Done {
                        work_ok: false,
                        gate_result: "skipped",
                        exit: agent_code,
                        log: log_path,
                        timed_out: false,
                    };
                }
            }

            // On a timeout the agent was killed. The iteration must FAIL immediately:
            // release the held entity claims (reverse), return the issue to `todo`
            // un-advanced, and file a deadend note so the next agent does not
            // re-derive the hang. A killed agent does NOT consume the retry budget
            // (SIRF-7 / SIRF-9): we return straight out of the loop rather than
            // looping back to WORK.
            if timed_out && phase != "work" {
                // A hung FIX agent: hand control back to the review stage, which
                // reverts to the last reviewed (gate-passing) checkpoint and
                // escalates. Releasing here would abandon that work — the next
                // iteration's reset would wipe it.
                return WorkGate::Done {
                    work_ok: false,
                    gate_result: "skipped",
                    exit: agent_code,
                    log: log_path,
                    timed_out: true,
                };
            }
            if timed_out {
                release_entities(hv, ledger, &issue, &claim_ids);
                // SIRF-41: a killed agent may have FINISHED — its work is
                // pinned at refs/sirius/wip/<issue> and resumed next claim.
                let wip = release_back(
                    &format!(
                        "sirius: released — agent timed out: {} (killed)",
                        timeout_kind
                            .unwrap_or(crate::shell::TimeoutKind::Hard)
                            .describe(limits.idle_secs, limits.hard_secs)
                    ),
                    WipKind::Resume,
                );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    "release",
                    with_wip(
                        json!({"reason": "agent_timeout", "advanced": false, "timeout_kind": timeout_kind.map(crate::shell::TimeoutKind::as_str)}),
                        &wip,
                    ),
                );
                ledger_warn(
                    "finish_iteration",
                    ledger.finish_iteration(
                        iter_id,
                        &entities,
                        "agent_timeout",
                        None,
                        &oracle_verdicts,
                        None,
                        Some(start.elapsed().as_millis() as i64),
                        None,
                    ),
                );
                ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));
                file_deadend(hv, &entities, &issue, "agent timed out");
                emit_job("job.blocked", &issue);
                return WorkGate::Exit(IterationOutcome::Deadend);
            }

            // GATE — select over the agent's ACTUAL changes, run the tests, and take
            //    the verdict from the test runner (never from the selector's exit
            //    code). `hayven affected-tests` only selects; on any doubt the gate
            //    runs the full suite. See gate.rs / SIRF-5 / D-3.
            let gate_once = || match baseline_err.clone().map_or_else(
                || crate::gitrange::changed_since(runner, pre_head.as_deref(), &pre_untracked),
                Err,
            ) {
                // A git failure — at baseline time or now — means the
                // changed-file set is UNKNOWN. That is doubt, not "nothing
                // changed": the old `unwrap_or_default()` folded it into an
                // empty list and skipped the gate (fail-open).
                Err(e) => Some(crate::gate::evaluate_doubt(
                    runner,
                    &config.gate,
                    &format!("cannot determine changed files: {e}"),
                )),
                // Genuinely nothing changed (committed, staged, unstaged, or
                // untracked) since the pre-work baseline → nothing to gate.
                Ok(files) if files.is_empty() => None,
                // Isolated worktrees are INVISIBLE to the code-graph daemon
                // (it watches the main checkout), so `affected-tests` would
                // select from a graph of the PRE-change code — a narrow
                // subset it blesses can miss tests the change newly affects.
                // Fleet mode therefore always takes the doubt path (full
                // suite under the default fallback); narrow selection remains
                // for the serial `sirius gate` CLI, which runs where the
                // daemon watches. (SIRF-48: the reason now reads as the
                // routine note it is, not as a cause of failure.)
                Ok(_) if isolate_base.is_some() => {
                    Some(crate::gate::evaluate_isolated(runner, &config.gate))
                }
                Ok(files) => Some(crate::gate::evaluate(hv, runner, &config.gate, &files)),
            };
            let mut verdict = gate_once();
            // SIRF-50: a failure whose output says a DEPENDENCY is missing
            // (`Cannot find module 'react'`) is the environment's fault, not
            // the agent's. Re-run worktree setup and gate again — ONCE, and
            // without consuming a work attempt. Still failing ⇒ an ordinary
            // gate failure below.
            let env_fault = verdict.as_ref().and_then(|v| v.env_fault.clone());
            let mut setup_rerun = false;
            let mut env_note: Option<String> = None;
            if let Some(line) = &env_fault {
                let line = line.replace('`', "'");
                env_note = Some(match setup_cmd {
                    Some(cmd) if !setup_rerun_used => {
                        setup_rerun_used = true;
                        setup_rerun = true;
                        eprintln!(
                            "sirius: {worker} {issue}: gate output looks like a missing dependency ({line}) — re-running worktree setup `{cmd}` and re-gating (no work attempt used)"
                        );
                        // Setup + a second full gate can outlast the lease
                        // (no agent supervision renews it here): renew now,
                        // and abort if the lease is no longer ours.
                        if let Err(reason) = renew_checked() {
                            return WorkGate::Exit(lease_lost(out, &reason, "env-fault re-gate"));
                        }
                        let sh = crate::shell::resolve_shell();
                        match crate::gate::run_setup(runner, &sh, cmd) {
                            Ok(()) => {
                                verdict = gate_once();
                                let after = if verdict.as_ref().is_some_and(|v| v.passed) {
                                    "the re-gate passed"
                                } else {
                                    "still failing, so this is an ordinary gate failure"
                                };
                                format!(
                                    "Environment fault suspected (`{line}`): re-ran worktree setup `{cmd}` and re-gated without using a work attempt — {after}."
                                )
                            }
                            Err(e) => format!(
                                "Environment fault suspected (`{line}`), but re-running worktree setup failed: {e}"
                            ),
                        }
                    }
                    Some(_) => format!(
                        "Environment fault suspected (`{line}`); worktree setup was already re-run once in this pass."
                    ),
                    None => format!(
                        "Environment fault suspected (`{line}`); no worktree.setup_cmd is configured to repair it."
                    ),
                });
            }
            // A pass does NOT move the issue by itself: the status changes once,
            // at RELEASE, after the review stage (SIRF-23) has had its say — a
            // gate pass is necessary, not sufficient.
            gate_result = match &verdict {
                Some(v) if v.passed => "pass",
                Some(v) => {
                    // SIRF-48: the comment leads with the failing output's
                    // tail; the selection note is demoted to a trailing line.
                    let _ = amt.comment_as(
                        &issue,
                        &crate::gate::loop_fail_comment(v, env_note.as_deref()),
                        worker,
                    );
                    "fail"
                }
                None => "skipped",
            };
            let error_tail = verdict
                .as_ref()
                .filter(|v| !v.passed)
                .map(crate::gate::error_tail)
                .filter(|t| !t.is_empty());
            if let Some(v) = verdict.as_ref().filter(|v| !v.passed) {
                last_gate_tail = Some(
                    error_tail
                        .clone()
                        .unwrap_or_else(|| format!("{}: {}", v.reason_code, v.reason)),
                );
            }
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
                    "reason_code": verdict.as_ref().map(|v| v.reason_code),
                    "error_tail": error_tail,
                    "env_fault": env_fault.is_some(),
                    "setup_rerun": setup_rerun,
                }),
            );
            // Durable: amt status advance (pass) / comment (fail) applied above. A
            // skipped gate (nothing to test) is not a gate verdict, so no event.
            if let Some(v) = &verdict {
                if gate_result != "skipped" {
                    emit_spine(
                        if gate_result == "pass" {
                            "gate.passed"
                        } else {
                            "gate.failed"
                        },
                        vec![crate::spine::issue_ref(&issue)],
                        json!({ "issue": issue.as_str(), "tests": v.test_ids.clone() }),
                    );
                }
            }

            // Retry decision (SIRF-9): only a FAIL is retryable, and only while the
            // budget has attempts left. Each retry is recorded as a policy event so
            // the ledger shows the honest attempt history.
            //
            // An `unconfigured` fail is STRUCTURAL — no test_cmd exists, so a
            // fresh agent attempt cannot change the verdict, and retrying re-runs
            // the whole (expensive) agent against a gate that can never pass
            // (observed in the field as a fleet burning 3× agent time per issue
            // and completing nothing). Everything else stays retryable: a blocked
            // selection or a test-runner spawn failure can be transient (daemon
            // restarting, fork pressure), and a fresh attempt produces a fresh
            // diff that may map cleanly.
            let gate_retryable = verdict.as_ref().map(|v| !v.structural).unwrap_or(false);
            if gate_result == "fail" && gate_retryable && attempt + 1 < max_attempts {
                ledger_warn(
                "log_policy_event",
                ledger.log_policy_event(
                    iter_ref,
                    "retry_budget",
                    &json!({"issue": issue, "attempt": attempt + 1, "max_attempts": max_attempts}),
                ),
            );
                emit_event(
                    out,
                    worker,
                    Some(&issue),
                    phase,
                    json!({"retrying": true, "attempt": attempt + 1, "max_attempts": max_attempts}),
                );
                attempt += 1;
                continue;
            }
            break;
        }
        WorkGate::Done {
            work_ok,
            gate_result,
            exit: last_exit,
            log: last_log,
            timed_out: false,
        }
    };

    let (work_ok, gate_result, work_exit, work_log) = match run_work_gate(out, "work", 0, &[]) {
        WorkGate::Done {
            work_ok,
            gate_result,
            exit,
            log,
            ..
        } => (work_ok, gate_result, exit, log),
        WorkGate::Exit(o) => return o,
    };

    // 6. REVIEW⇄FIX (SIRF-23): only over work that passed the gate. Off unless
    //    `review.cmd` is set, and fleet-only (it checkpoints commits and may
    //    hard-reset, which is only safe in a private worktree).
    // SIRF-32: on a red frontier (integration.on_fail = block) gated work is
    // HELD — not reviewed, not receipted, never advanced; it is preserved on
    // its branch and released to todo naming the red run.
    let integration_hold = crate::integrate::block_reason(config, ledger);
    let gated = work_ok && (gate_result == "pass" || gate_result == "skipped");
    let review = if gated && integration_hold.is_none() {
        match fleet {
            Some(f) if config.review.cmd.is_some() => {
                let cx = ReviewCtx {
                    amt,
                    ledger,
                    config,
                    runner,
                    worker,
                    issue: &issue,
                    fleet: f,
                    base: isolate_base.unwrap_or(f.base.as_str()),
                    iter_ref,
                    base_env: &base_env,
                    spine,
                    renew: &renew,
                    renew_checked: &renew_checked,
                };
                let fix_round = |out: &mut dyn Write, round: u32, env: &[(String, String)]| {
                    run_work_gate(out, "fix", round, env)
                };
                run_review_stage(&cx, out, &fix_round)
            }
            _ => ReviewStage::Off,
        }
    } else {
        ReviewStage::Off
    };
    if let ReviewStage::LeaseLost(reason) = &review {
        return lease_lost(out, reason, "the review");
    }
    // SIRF-32: red may have been recorded WHILE this work was in review —
    // re-check, and hold rather than advance onto a red frontier.
    let integration_hold = match (&review, integration_hold) {
        (ReviewStage::Held(why), _) => Some(why.clone()),
        (_, None) if gated => crate::integrate::block_reason(config, ledger),
        (_, h) => h,
    };
    if let ReviewStage::Exit(o) = review {
        return o;
    }

    // 7. RECEIPT: decide + two-way link (only on a passing/complete iteration).
    let mut receipt_id: Option<i64> = None;
    let review_released = matches!(review, ReviewStage::Release { .. });
    let review_summary: Option<&crate::review::ReviewSummary> = match &review {
        ReviewStage::Advance { summary, .. } => summary.as_ref(),
        ReviewStage::Release { summary, .. } => Some(summary),
        _ => None,
    };
    if gated && !review_released && integration_hold.is_none() {
        // The decision carries the WHY (SIRF-22 #6): the review history when
        // a review ran, attributed to the worker (SIRF-22 #5).
        let (title, body) = decision_text(&issue, &review);
        if let Ok(decision_ref) = amt.decide_as(&issue, &title, &body, worker) {
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
        // Durable: the two-way receipt was written inside bridge::link above.
        if let Some(rid) = receipt_id {
            emit_spine(
                "receipt.filed",
                vec![
                    crate::spine::receipt_ref(rid),
                    crate::spine::issue_ref(&issue),
                ],
                json!({ "issue": issue.as_str(), "symbols": entities.clone() }),
            );
        }
    }

    // 8. RELEASE: entities first (reverse), then close out the issue.
    //    A failed gate must NOT advance the issue — holding a failing change back
    //    is the gate's whole purpose (gate.rs contract: "fail files a comment and
    //    leaves status untouched"). Advancing requires BOTH a successful agent
    //    run and a non-failing gate: a passing suite over changes the agent
    //    didn't make (it failed or never spawned) is not completed work, and
    //    advancing on it let a broken agent command "complete" real issues.
    //    Otherwise the issue returns to `todo`: re-claimable, but un-promoted
    //    (matching the entity-overlap release path above). (SIRF-6)
    release_entities(hv, ledger, &issue, &claim_ids);
    let mut advanced = gated && !review_released && integration_hold.is_none();
    // PRESERVE the completed work before anything can reset the worktree
    // (isolated fleets hard-reset between issues and tear worktrees down at
    // exit). Non-committing agents are common — without this, a gated,
    // advanced, receipted issue's actual code could be deleted with the
    // worktree, completed-on-the-board but existing nowhere. Auto-commit
    // whatever is uncommitted, then stamp the per-issue branch (branch -f is
    // safe: detached worktrees never hold the branch checked out).
    let mut held_marked = false;
    if (advanced || (gated && integration_hold.is_some())) && isolate_base.is_some() {
        let _ = crate::gitrange::run_git(runner, &["add", "-A"]);
        // May legitimately fail with "nothing to commit" when the agent
        // committed its own work — that is fine, HEAD already has it. What is
        // NOT fine is a dirty tree after the attempt (e.g. commit refused for
        // a missing git identity): stamping HEAD then would silently drop the
        // uncommitted half, so verify cleanliness below.
        let _ = crate::gitrange::run_git(
            runner,
            &["commit", "-m", &format!("sirius: complete {issue}")],
        );
        let clean = crate::gitrange::run_git(runner, &["status", "--porcelain"])
            .map(|o| o.stdout.trim().is_empty())
            .unwrap_or(false);
        let branch = format!("sirius/{}", issue.to_lowercase());
        let stamp = if clean {
            crate::gitrange::run_git(runner, &["branch", "-f", &branch, "HEAD"]).map(|_| ())
        } else {
            Err("worktree still dirty after auto-commit (missing git identity?)".to_string())
        };
        let held_ref = held_ref(&issue);
        match &stamp {
            // SIRF-32: held work is resumed when the issue is re-claimed. The
            // ref both marks it and keeps its commits reachable, whatever
            // later happens to the issue branch.
            Ok(()) if !advanced => {
                // An older held commit this work does NOT contain (its resume
                // failed) is parked, never overwritten into oblivion.
                if let Ok(old) =
                    crate::gitrange::run_git(runner, &["show-ref", "--verify", "--hash", &held_ref])
                {
                    let old = old.stdout.trim().to_string();
                    let contained = crate::gitrange::run_git(
                        runner,
                        &["merge-base", "--is-ancestor", &old, "HEAD"],
                    )
                    .is_ok();
                    if !old.is_empty() && !contained {
                        let parked = format!(
                            "refs/sirius/held-superseded/{}/{}",
                            issue.to_lowercase(),
                            &old[..old.len().min(12)]
                        );
                        let _ = crate::gitrange::run_git(runner, &["update-ref", &parked, &old]);
                    }
                }
                held_marked =
                    crate::gitrange::run_git(runner, &["update-ref", &held_ref, "HEAD"]).is_ok();
            }
            // Completed work that resumed the held (or wip) commit contains
            // it: the marker has done its job. (Removed only now — an
            // iteration that ends earlier keeps it for the next claim.)
            Ok(()) => retire_resumed(runner, &issue, &resume),
            _ => {}
        }
        if let Err(e) = stamp {
            // Without the branch the commits die with the worktree — do NOT
            // report completion over work that is about to vanish.
            eprintln!(
                "sirius: FAILED to stamp {branch} for {issue}: {e} — work retained in the worktree; NOT advancing"
            );
            let _ = amt.comment_as(
                &issue,
                &format!("sirius: gate passed but stamping the work on its branch failed ({e}) — released without advancing"),
                worker,
            );
            advanced = false;
        }
    }
    // SIRF-41: anything not advanced and not held is preserved before the
    // release below hands the issue to the next claimer. Interrupted work
    // (agent failure, a stamp that could not be made) is resumed on the next
    // claim; work the gate or the review JUDGED and failed is only offered
    // to the next agent (SIRIUS_PRIOR_WORK) — merging it back automatically
    // would restart every attempt from the same failing state. An agent
    // that itself FAILED was interrupted, whatever the gate then said about
    // its partial tree (the ticket's "agent failure" → resume).
    let judged = review_released || (work_ok && gate_result == "fail");
    let wip: WipResult = if advanced || held_marked {
        Ok(None)
    } else if judged {
        // Preserved work this iteration resumed was part of what was judged:
        // it moves to wip-failed WITH the new work and stops being merged
        // back automatically.
        let resumed_wips: Vec<(String, String)> = resume
            .drop_on_stamp
            .iter()
            .filter(|(r, _)| *r == wip_ref(&issue, WipKind::Resume))
            .cloned()
            .collect();
        let mut w = preserve(WipKind::Failed);
        if let (Ok(None), false, Some(base)) = (&w, resumed_wips.is_empty(), isolate_base) {
            // Nothing new on top: the judged work IS the resumed work.
            w = preserve_wip(runner, &issue, base, base, WipKind::Failed);
        }
        if let Ok(Some(f)) = &w {
            for (r, sha) in &resumed_wips {
                if is_ancestor(runner, sha, &f.sha) {
                    let _ = crate::gitrange::run_git(runner, &["update-ref", "-d", r, sha]);
                }
            }
        }
        w
    } else {
        preserve(WipKind::Resume)
    };
    // Say what ACTUALLY held the issue back (SIRF-22 #8): "gate did not
    // pass" used to be posted even when the agent itself had exited 1.
    let held_back = if advanced {
        None
    } else if let (true, Some(why)) = (gated, &integration_hold) {
        Some(if held_marked {
            format!(
                "sirius: released without advancing — {why}; the gated work is held at {} and is resumed when the issue is claimed again",
                held_ref(&issue)
            )
        } else {
            format!("sirius: released without advancing — {why}; holding the gated work FAILED — it will be redone")
        })
    } else if let ReviewStage::Release { why, .. } = &review {
        Some(format!(
            "sirius: released without advancing — review: {why}"
        ))
    } else if !work_ok {
        Some(format!(
            "sirius: released without advancing — agent {}{}",
            match work_exit {
                // SIRF-52: exit 0, but its helper tasks were killed unfinished.
                Some(c) if crate::shell::bg_tasks_killed_in(work_log.as_deref()) => format!(
                    "exited {c} but its CLI killed its still-running background tasks (agent_bg_tasks_killed) — the work is incomplete"
                ),
                Some(c) => format!("exited {c}"),
                None => "did not run (spawn failure)".into(),
            },
            work_log
                .as_ref()
                .map(|p| format!(" (see {})", p.display()))
                .unwrap_or_default()
        ))
    } else if gate_result == "fail" {
        Some("sirius: released without advancing — gate did not pass".to_string())
    } else {
        Some(
            "sirius: released without advancing — the work could not be stamped on its branch"
                .to_string(),
        )
    };
    let held_back = held_back.map(|c| format!("{c}{}", wip_note(&wip)));
    let release_status: &str = if advanced {
        config.target_status.as_str()
    } else {
        "todo"
    };
    release_issue_checked(
        amt,
        ledger,
        &issue,
        worker,
        Some(release_status),
        held_back.as_deref(),
    );
    let mut release_ev = with_wip(
        json!({"status": release_status, "advanced": advanced}),
        &wip,
    );
    if let Some(sum) = review_summary {
        release_ev["review"] = json!(sum.line());
    }
    emit_event(out, worker, Some(&issue), "release", release_ev);

    let outcome = if gate_result == "fail" {
        "gate_failed"
    } else if review_released {
        // `deadend` (not a new outcome string): it is one — a deadend note
        // is filed below — and the console's outcome enum counts it.
        "deadend"
    } else if advanced {
        "completed"
    } else if gated && integration_hold.is_some() {
        // Good work held back by a red frontier (SIRF-32), not a failure.
        "released"
    } else {
        // The agent run failed (or never spawned) and nothing advanced —
        // record the honest failure rather than claiming completion.
        "error"
    };
    ledger_warn(
        "finish_iteration",
        ledger.finish_iteration(
            iter_id,
            &entities,
            outcome,
            Some(gate_result),
            &oracle_verdicts,
            None,
            Some(start.elapsed().as_millis() as i64),
            receipt_id,
        ),
    );
    ledger_warn("upsert_worker", ledger.upsert_worker(worker, "idle"));

    // Durable: the terminal finish_iteration above. "completed" ⇔ advanced;
    // gate_failed / error are non-completing → blocked.
    if outcome == "completed" {
        emit_job("job.completed", &issue);
    } else {
        emit_job("job.blocked", &issue);
    }

    if gate_result == "fail" {
        // Record the failure as a deadend note so the next agent does not
        // re-derive it (PRD §F3 retry-budget exhaustion behavior).
        file_deadend(hv, &entities, &issue, "gate failed (affected-tests)");
        IterationOutcome::Deadend
    } else if review_released {
        // A usage limit is not a dead end — the next attempt deserves a clean
        // slate, not a misleading "did not converge" note.
        if fleet.and_then(Fleet::paused).is_none() {
            file_deadend(hv, &entities, &issue, "review did not converge");
        }
        IterationOutcome::Deadend
    } else if outcome == "error" {
        // A failed agent with nothing advanced is an OPERATIONAL error: it
        // must reach the run loop's error budget/backoff. Returning Completed
        // here (the old behavior) made a broken `--agent-cmd` hot-loop the
        // whole board: claim → instant failure → release → re-claim, forever.
        IterationOutcome::Error("agent run failed and nothing advanced".into())
    } else {
        IterationOutcome::Completed
    }
}

/// Durable log path for one agent run (SIRF-7): `<.sirius>/logs/<issue>-<ts>.log`
/// — absolute under the fleet's `.sirius/` (review logs live there too), or
/// relative to the cwd when not isolated. Returns `None` only if the system
/// clock is before the epoch — the directory is created lazily by the writer.
fn agent_log_path(fleet: Option<&Fleet>, issue: &str) -> Option<std::path::PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    // Sanitize the issue ref for a filename (AMT-7 → AMT-7 is fine; guard slashes).
    let safe: String = issue
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = fleet
        .map(|f| f.sirius_dir.join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from(".sirius/logs"));
    Some(dir.join(format!("{safe}-{ts}.log")))
}

/// File a deadend fleet-memory note when a retry budget is exhausted (PRD §F3).
pub fn file_deadend(hv: &Hayven, entities: &[String], issue: &str, reason: &str) {
    let note = format!("deadend: {issue} exhausted retries — {reason}");
    let primary = entities.first().map(|s| s.as_str());
    let _ = hv.remember(&note, primary, "deadend", entities);
}

// ── 6. REVIEW⇄FIX (SIRF-23) ─────────────────────────────────────────────────

use crate::review::{Finding, ReviewSummary, RoundResult};

/// Everything the review stage needs from the iteration.
pub struct ReviewCtx<'a, 'r> {
    pub amt: &'a Amt<'r>,
    pub ledger: &'a Ledger,
    pub config: &'a Config,
    pub runner: &'a dyn Runner,
    pub worker: &'a str,
    pub issue: &'a str,
    pub fleet: &'a Fleet,
    /// The commit THIS iteration's worktree was reset to (SIRF-42: the base
    /// ref's tip at claim time, else `fleet.base`) — every review diff range
    /// starts here, never at the launch snapshot.
    pub base: &'a str,
    pub iter_ref: Option<i64>,
    pub base_env: &'a [(String, String)],
    pub spine: Option<&'a crate::spine::Spine>,
    /// Renews both leases; fired on the reviewer's heartbeat too (a review
    /// takes minutes — the lease must not lapse under it).
    pub renew: &'a dyn Fn(),
    /// Renew BEFORE spawning the reviewer and report a refused amt lease:
    /// the gap since the last beat (agent tail + full-suite gate + merge
    /// prep + the first heartbeat interval) can exceed the 900s lease.
    pub renew_checked: &'a dyn Fn() -> Result<(), String>,
}

/// How the review stage ended.
pub enum ReviewStage {
    /// `review.cmd` unset, not isolated, or the work never passed the gate —
    /// today's behavior.
    Off,
    /// Advance: clean, skipped (`skip_paths`), or escalated `advance-flagged`
    /// (then `open` lists the unresolved confirmed findings).
    Advance {
        summary: Option<ReviewSummary>,
        open: Vec<Finding>,
    },
    /// Escalated with `release`: back to `todo`, the findings already posted.
    Release { summary: ReviewSummary, why: String },
    /// A fix round hit a terminal path that already did its bookkeeping.
    Exit(IterationOutcome),
    /// The amt lease was refused before a reviewer spawned — the issue may
    /// now belong to someone else. The caller does the lease-lost unwind.
    LeaseLost(String),
    /// SIRF-32: integration went red mid-review (`on_fail: block`). The work
    /// is HELD — never escalated to an unreviewed advance.
    Held(String),
}

/// Did the fleet stop because integration went red (not a usage limit)?
fn integration_pause(fleet: &Fleet) -> Option<String> {
    fleet
        .paused()
        .filter(|why| why.starts_with("integration red"))
}

/// One reviewer attempt's result.
enum ReviewOnce {
    Report {
        report: crate::review::ReviewReport,
        head: String,
        /// Were the AUTO facts fully recomputed? If not, the previous
        /// round's facts stay open (fail closed — SIRF-31).
        facts_complete: bool,
    },
    /// The work does not merge onto the base: these findings, no review.
    Conflicts {
        findings: Vec<Finding>,
        head: String,
        facts_complete: bool,
    },
    Failed {
        result: RoundResult,
        detail: String,
    },
    LeaseLost(String),
    /// SIRF-27: the reviewer hit a usage limit on the primary tier and the
    /// fleet switched to its fallback — retry the review on the fallback.
    FellBack {
        line: String,
        first: bool,
    },
}

/// Suite-owned paths whose writers are daemons and hooks, never the reviewer:
/// a reviewer's Claude Code hooks append to `.suite/events/` (tracked in some
/// repos), and reading one as tampering discarded a clean review (SIRF-29).
const FINGERPRINT_EXCLUDES: [&str; 4] = [
    ":(exclude).suite",
    ":(exclude).hayven",
    ":(exclude).ametrite",
    ":(exclude).sirius",
];

/// What the reviewer must leave alone, as text: the HEAD commit, the porcelain
/// status (untracked included), and the full diff against HEAD. Any change
/// between before and after = the reviewer edited, staged, or committed.
fn tree_fingerprint(runner: &dyn Runner) -> String {
    let scoped = |args: &[&'static str]| -> Vec<&'static str> {
        let mut v = args.to_vec();
        v.extend(["--", "."]);
        v.extend(FINGERPRINT_EXCLUDES);
        v
    };
    [
        vec!["log", "-1", "--format=%H"],
        scoped(&["status", "--porcelain=v1", "--untracked-files=all"]),
        scoped(&["diff", "--no-ext-diff", "--binary", "HEAD"]),
    ]
    .iter()
    .map(|args| match crate::gitrange::run_git(runner, args) {
        Ok(o) => o.stdout,
        Err(e) => format!("ERR:{e}"),
    })
    .collect::<Vec<_>>()
    .join("\u{0}")
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Commit whatever the worker left uncommitted so the review sees exactly
/// `base..HEAD`, and a tampering reviewer can be undone with one reset.
fn checkpoint(cx: &ReviewCtx, round: u32) -> Result<String, String> {
    let _ = crate::gitrange::run_git(cx.runner, &["add", "-A"]);
    // "nothing to commit" is fine; a dirty tree afterwards is not.
    let _ = crate::gitrange::run_git(
        cx.runner,
        &[
            "commit",
            "-m",
            &format!(
                "sirius: checkpoint {} before review round {round}",
                cx.issue
            ),
        ],
    );
    let clean = crate::gitrange::run_git(cx.runner, &["status", "--porcelain"])
        .map(|o| o.stdout.trim().is_empty())
        .unwrap_or(false);
    if !clean {
        return Err("cannot checkpoint the work before review (missing git identity?)".into());
    }
    crate::gitrange::head_rev(cx.runner)
}

/// Discard everything since `head` in the worker's worktree.
fn revert_to(cx: &ReviewCtx, head: &str) {
    let _ = crate::gitrange::run_git(cx.runner, &["reset", "--hard", head]);
    let _ = crate::gitrange::run_git(cx.runner, &["clean", "-fd"]);
}

/// `git worktree add/remove/prune` contend on `.git` admin locks; v0.1.1
/// moved fleet worktree setup to a serial step for exactly that reason. The
/// review stage's throwaway merge trees are created from N worker threads,
/// so their admin operations are serialized process-wide.
static WORKTREE_ADMIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn worktree_admin<T>(f: impl FnOnce() -> T) -> T {
    // A poisoned lock only means another thread panicked mid-operation; the
    // guard protects git's lock files, not shared Rust state.
    let _g = WORKTREE_ADMIN.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

/// The ref holding the work a red frontier held back for `issue` (SIRF-32).
fn held_ref(issue: &str) -> String {
    format!("refs/sirius/held/{}", issue.to_lowercase())
}

/// Merge the work a red frontier held for `issue` into the freshly reset
/// worktree (a REAL merge in the issue's history: the repo's identity and
/// hooks). The held ref is NOT consumed here — it is removed only once the
/// resumed work is stamped, so an iteration that ends early resumes again
/// next time. Held work already on the base is dropped; work that no longer
/// merges is parked at `refs/sirius/held-conflicted/<issue>` (kept, not
/// retried) and the issue starts fresh.
fn resume_held(amt: &Amt, runner: &dyn Runner, issue: &str, worker: &str) -> Option<String> {
    let held = held_ref(issue);
    let sha = ref_sha(runner, &held)?;
    let conflicted = format!("refs/sirius/held-conflicted/{}", issue.to_lowercase());
    match resume_ref(
        amt,
        runner,
        issue,
        worker,
        &held,
        &sha,
        &conflicted,
        "held",
        None,
    ) {
        ResumeOutcome::Merged(sha) => Some(sha),
        _ => None,
    }
}

/// How one resume attempt ended ([`resume_ref`]).
#[derive(Debug, PartialEq)]
enum ResumeOutcome {
    /// Merged onto the worktree; carries the merged sha.
    Merged(String),
    /// Already on the base: the ref was removed, nothing to resume.
    Landed,
    /// Already in what an earlier resume merged this iteration: kept until
    /// the stamp (the stamped work contains it).
    Contained,
    /// Did not merge: parked at the conflicted ref (a conflict) or left in
    /// place for the next claim (any other merge failure).
    NotMerged,
}

/// Merge `sha` (what `live` points at) into the freshly reset worktree — a
/// REAL merge in the issue's history (the repo's identity and hooks). Shared
/// by held (SIRF-32) and preserved (SIRF-41) work; `label` names it on the
/// board ("held" / "preserved"). `base` is `Some(iteration base)` when an
/// earlier resume already moved HEAD off the base, so "already in HEAD" can
/// tell landed work (ref removed) from work merely contained in that resume.
#[allow(clippy::too_many_arguments)]
fn resume_ref(
    amt: &Amt,
    runner: &dyn Runner,
    issue: &str,
    worker: &str,
    live: &str,
    sha: &str,
    conflicted: &str,
    label: &str,
    base: Option<&str>,
) -> ResumeOutcome {
    if is_ancestor(runner, sha, "HEAD") {
        if base.is_some_and(|b| !is_ancestor(runner, sha, b)) {
            return ResumeOutcome::Contained;
        }
        let _ = crate::gitrange::run_git(runner, &["update-ref", "-d", live]);
        return ResumeOutcome::Landed; // already on the base — nothing to resume
    }
    if crate::gitrange::run_git(runner, &["merge", "--no-edit", sha]).is_ok() {
        let _ = amt.comment_as(
            issue,
            &format!("sirius: resuming the work {label} at {live} ({sha})"),
            worker,
        );
        return ResumeOutcome::Merged(sha.to_string());
    }
    let had_conflicts =
        crate::gitrange::run_git(runner, &["diff", "--name-only", "--diff-filter=U"])
            .map(|o| !o.stdout.trim().is_empty())
            .unwrap_or(false);
    let _ = crate::gitrange::run_git(runner, &["merge", "--abort"]);
    let _ = crate::gitrange::run_git(runner, &["reset", "-q", "--hard", "HEAD"]);
    if had_conflicts {
        let _ = crate::gitrange::run_git(runner, &["update-ref", conflicted, sha]);
        let _ = crate::gitrange::run_git(runner, &["update-ref", "-d", live]);
        let _ = amt.comment_as(
            issue,
            &format!("sirius: the {label} work ({sha}) no longer merges onto the base — starting fresh; the {label} commits are kept at {conflicted}"),
            worker,
        );
    } else {
        let _ = amt.comment_as(
            issue,
            &format!("sirius: could not merge the {label} work ({sha}) — starting fresh this time; it stays at {live} and is retried on the next claim"),
            worker,
        );
    }
    ResumeOutcome::NotMerged
}

// ── SIRF-41: never lose work; resume it ────────────────────────────────────

/// Where a non-advancing iteration's work is pinned — and so whether the
/// next claim merges it back by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WipKind {
    /// INTERRUPTED work (agent timeout/kill/failure, a usage limit, a lost
    /// lease, a stamp that failed): `refs/sirius/wip/<issue>`, merged back
    /// automatically on the next claim (`SIRIUS_RESUMED_FROM`).
    Resume,
    /// Work that was JUDGED and failed (gate deadend, review release):
    /// `refs/sirius/wip-failed/<issue>`, kept and offered to the next agent
    /// as `SIRIUS_PRIOR_WORK`, never merged automatically.
    Failed,
}

/// `refs/sirius/<kind>/<issue>`: the issue key is the 4th path segment,
/// which is how `sirius link --changed` (`gitrange::issue_refs`) knows these
/// commits are this issue's own work.
fn wip_ref(issue: &str, kind: WipKind) -> String {
    let ns = match kind {
        WipKind::Resume => "wip",
        WipKind::Failed => "wip-failed",
    };
    format!("refs/sirius/{ns}/{}", issue.to_lowercase())
}

/// Park an older preserved commit a new one does not contain — never
/// overwritten into oblivion (mirrors `held-superseded`).
fn park_superseded(runner: &dyn Runner, issue: &str, sha: &str) -> String {
    let parked = format!(
        "refs/sirius/wip-superseded/{}/{}",
        issue.to_lowercase(),
        short_sha(sha)
    );
    let _ = crate::gitrange::run_git(runner, &["update-ref", &parked, sha]);
    parked
}

/// Work pinned by [`preserve_wip`]: the release comment and the NDJSON
/// `release` event's `wip_ref` carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct WipRef {
    pub ref_name: String,
    pub sha: String,
    /// `git diff --shortstat` of the work against the iteration base.
    pub diffstat: String,
    /// The uncommitted rest could NOT be committed (why): only the
    /// committed work is pinned.
    pub partial: Option<String>,
}

impl WipRef {
    fn json(&self) -> Value {
        let mut v = json!({"ref": self.ref_name, "sha": self.sha, "diffstat": self.diffstat});
        if let Some(p) = &self.partial {
            v["partial"] = json!(p);
        }
        v
    }

    /// The release comment's tail.
    fn suffix(&self) -> String {
        format!(
            " — work preserved at {} ({}, {}){}",
            self.ref_name,
            short_sha(&self.sha),
            self.diffstat,
            match &self.partial {
                Some(p) =>
                    format!(" — committed work only: the uncommitted rest could NOT be saved ({p})"),
                None => String::new(),
            }
        )
    }
}

/// `Ok(None)`: nothing new to preserve. `Err`: there was work and pinning it
/// FAILED — said loudly, never silently dropped.
type WipResult = Result<Option<WipRef>, String>;

/// The release comment's tail for a preserve attempt.
fn wip_note(w: &WipResult) -> String {
    match w {
        Ok(Some(w)) => w.suffix(),
        Ok(None) => String::new(),
        Err(e) => {
            format!(" — preserving the work FAILED ({e}); it is lost at this worker's next reset")
        }
    }
}

/// Add `wip_ref` (or `wip_error`) to a release event — additive only.
fn with_wip(mut ev: Value, w: &WipResult) -> Value {
    match w {
        Ok(Some(w)) => ev["wip_ref"] = w.json(),
        Err(e) => ev["wip_error"] = json!(e),
        Ok(None) => {}
    }
    ev
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// What `r` points at, if it exists.
fn ref_sha(runner: &dyn Runner, r: &str) -> Option<String> {
    crate::gitrange::run_git(runner, &["show-ref", "--verify", "--hash", r])
        .ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_ancestor(runner: &dyn Runner, a: &str, b: &str) -> bool {
    crate::gitrange::run_git(runner, &["merge-base", "--is-ancestor", a, b]).is_ok()
}

/// Does the tree hold uncommitted work? Suite-owned telemetry (hook-written
/// `.suite/` events and the like) alone is not work.
fn worktree_dirty(runner: &dyn Runner) -> Result<bool, String> {
    let mut args = vec!["status", "--porcelain", "--", "."];
    args.extend(FINGERPRINT_EXCLUDES);
    crate::gitrange::run_git(runner, &args).map(|o| !o.stdout.trim().is_empty())
}

/// Does HEAD's tree differ from `floor`'s? (Commits that net to nothing are
/// not work.) Exit 1 = differs; anything but 0/1 is an error.
fn tree_differs(runner: &dyn Runner, floor: &str) -> Result<bool, String> {
    let out = runner
        .run(
            "git",
            &["diff", "--no-ext-diff", "--quiet", floor, "HEAD", "--"],
        )
        .map_err(|e| e.to_string())?;
    match out.code {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(format!(
            "git diff {floor} HEAD failed: {}",
            out.stderr.trim()
        )),
    }
}

/// Commit everything in the worktree (`add -A`) as sirius's own wip commit,
/// with NO repo hooks (`--no-verify` + an empty hooks path: a failing lint
/// hook, or a commit-msg rewrite, must not cost or alter the work). Retries
/// once under sirius's own identity with signing off (a machine without a
/// git identity), after clearing an `index.lock` left by the killed agent —
/// the worktree is this worker's alone and the agent is gone. `Err` when the
/// tree is still dirty.
fn commit_all(runner: &dyn Runner, msg: &str) -> Result<(), String> {
    let clean = || {
        crate::gitrange::run_git(runner, &["status", "--porcelain"])
            .map(|o| o.stdout.trim().is_empty())
            .unwrap_or(false)
    };
    let attempt = |extra: &[&str]| {
        let _ = crate::gitrange::run_git(runner, &["add", "-A"]);
        let mut args: Vec<&str> = vec!["-c", "core.hooksPath=/dev/null"];
        args.extend(extra);
        args.extend(["commit", "--no-verify", "-q", "-m", msg]);
        let _ = crate::gitrange::run_git(runner, &args);
    };
    attempt(&[]);
    if clean() {
        return Ok(());
    }
    if let Ok(o) = crate::gitrange::run_git(
        runner,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index.lock",
        ],
    ) {
        let lock = std::path::PathBuf::from(o.stdout.trim());
        if lock.is_absolute() && lock.exists() {
            eprintln!(
                "sirius: removing a stale {} left by the agent",
                lock.display()
            );
            let _ = std::fs::remove_file(&lock);
        }
    }
    attempt(&[
        "-c",
        "user.name=sirius",
        "-c",
        "user.email=sirius@localhost",
        "-c",
        "commit.gpgsign=false",
    ]);
    if clean() {
        Ok(())
    } else {
        Err("the worktree is still dirty after the wip commit".into())
    }
}

/// Point `target` at `sha` with a compare-and-swap, so two writers (a
/// worker that lost its lease and the issue's new holder) can never
/// silently overwrite each other: an older value `sha` does not contain is
/// parked at `refs/sirius/wip-superseded/<issue>/<sha12>` first, and a value
/// that changed under us is re-read and re-judged.
fn pin_ref(runner: &dyn Runner, issue: &str, target: &str, sha: &str) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..3 {
        let old = ref_sha(runner, target);
        if let Some(old) = &old {
            if old != sha && !is_ancestor(runner, old, sha) {
                park_superseded(runner, issue, old);
            }
        }
        // An empty old value = "must not exist yet".
        let expect = old.unwrap_or_default();
        match crate::gitrange::run_git(runner, &["update-ref", target, sha, &expect]) {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(format!("cannot point {target} at {sha}: {last}"))
}

/// Park work that is about to be DISCARDED on purpose (a fix round reverted
/// to its last reviewed checkpoint `head`): committed and pinned at
/// `refs/sirius/wip-superseded/<issue>/<sha12>` — never merged back, but
/// never unreferenced either. `None` when there is nothing past `head`.
fn park_attempt(runner: &dyn Runner, issue: &str, head: &str) -> Option<String> {
    let dirty = worktree_dirty(runner).unwrap_or(true);
    if !dirty && !tree_differs(runner, head).unwrap_or(true) {
        return None;
    }
    if dirty {
        let _ = commit_all(runner, &format!("sirius: discarded attempt {issue}"));
    }
    let sha = crate::gitrange::head_rev(runner).ok()?;
    if sha == head {
        return None;
    }
    let parked = park_superseded(runner, issue, &sha);
    eprintln!("sirius: {issue}: the discarded attempt is parked at {parked}");
    Some(parked)
}

/// SIRF-41: pin the worktree's work before a non-advancing release, so a
/// killed agent's finished commit is never left dangling on a detached HEAD
/// (MSX-80: a whole implementation recoverable only by `git fsck`).
///
/// `floor` is what the iteration started from (the base plus any resumed
/// work): with a clean tree and HEAD's content still at the floor there is
/// nothing new — `Ok(None)`. Otherwise everything is committed (`add -A`,
/// untracked non-ignored files included) as `sirius: wip <issue>` and the
/// result pinned at `refs/sirius/wip[-failed]/<issue>`. The wip commit skips
/// repo hooks (`--no-verify`): a failing lint hook must not cost the work.
/// A previous ref of the same kind that HEAD does not contain is parked at
/// `refs/sirius/wip-superseded/<issue>/<sha12>` first.
fn preserve_wip(
    runner: &dyn Runner,
    issue: &str,
    floor: &str,
    base: &str,
    kind: WipKind,
) -> WipResult {
    let r = (|| -> WipResult {
        let dirty = worktree_dirty(runner)?;
        if !dirty && !tree_differs(runner, floor)? {
            return Ok(None);
        }
        // Commit what is uncommitted. If that fails, the agent's COMMITTED
        // work is still pinned below — a stuck index must never cost the
        // commits too (the MSX-80 case this exists for).
        let partial = if dirty {
            commit_all(runner, &format!("sirius: wip {issue}")).err()
        } else {
            None
        };
        if partial.is_some() && !tree_differs(runner, floor)? {
            return Err(partial.unwrap_or_default());
        }
        let sha = crate::gitrange::head_rev(runner)?;
        let target = wip_ref(issue, kind);
        pin_ref(runner, issue, &target, &sha)?;
        let diffstat = crate::gitrange::run_git(runner, &["diff", "--shortstat", base, &sha])
            .map(|o| o.stdout.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "no change against the base".into());
        Ok(Some(WipRef {
            ref_name: target,
            sha,
            diffstat,
            partial,
        }))
    })();
    match &r {
        Ok(Some(w)) => eprintln!("sirius: {issue}:{}", w.suffix()),
        Err(e) => eprintln!("sirius: {issue}: FAILED to preserve the worktree's work: {e}"),
        Ok(None) => {}
    }
    r
}

/// What a fleet iteration resumed at claim time ([`resume_work`]).
#[derive(Debug, Default)]
struct Resume {
    /// The LAST sha merged in (`SIRIUS_RESUMED_FROM`).
    resumed: Option<String>,
    /// The ref it came from (`SIRIUS_RESUME_REF`).
    resume_ref: Option<String>,
    /// Refs (with the sha seen) the stamped work will contain: removed only
    /// once the work is stamped at completion.
    drop_on_stamp: Vec<(String, String)>,
    /// Judged-and-failed earlier work (ref, sha): `SIRIUS_PRIOR_WORK`.
    prior_work: Option<(String, String)>,
}

/// Resume an issue's held (SIRF-32) and preserved (SIRF-41) work onto the
/// freshly reset worktree, and find judged-and-failed earlier work.
///
/// Preserved work that already CONTAINS the held commit (held work resumed,
/// then interrupted) supersedes it: merging both would stack the wip merge
/// above the held one, and `sirius link --changed` follows only the resume
/// merge at the bottom of the line — the wip commits would never be stamped.
/// So the held merge is skipped then (and tried as a fallback if the wip
/// does not merge).
fn resume_work(amt: &Amt, runner: &dyn Runner, issue: &str, worker: &str, base: &str) -> Resume {
    let mut r = Resume::default();
    let held = held_ref(issue);
    let wip = wip_ref(issue, WipKind::Resume);
    let wip_sha = ref_sha(runner, &wip);
    let held_sha = wip_sha.as_ref().and_then(|_| ref_sha(runner, &held));
    let held_in_wip = match (&held_sha, &wip_sha) {
        (Some(h), Some(w)) => is_ancestor(runner, h, w),
        _ => false,
    };
    let resume_held_into = |r: &mut Resume| {
        if let Some(sha) = resume_held(amt, runner, issue, worker) {
            r.drop_on_stamp.push((held.clone(), sha.clone()));
            r.resumed = Some(sha);
            r.resume_ref = Some(held.clone());
        }
    };
    if !held_in_wip {
        resume_held_into(&mut r);
    }
    if let Some(sha) = &wip_sha {
        let conflicted = format!("refs/sirius/wip-conflicted/{}", issue.to_lowercase());
        let moved = r.resumed.is_some().then_some(base);
        match resume_ref(
            amt,
            runner,
            issue,
            worker,
            &wip,
            sha,
            &conflicted,
            "preserved",
            moved,
        ) {
            ResumeOutcome::Merged(sha) => {
                r.drop_on_stamp.push((wip.clone(), sha.clone()));
                if let (true, Some(h)) = (held_in_wip, &held_sha) {
                    r.drop_on_stamp.push((held.clone(), h.clone()));
                }
                r.resumed = Some(sha);
                r.resume_ref = Some(wip.clone());
            }
            ResumeOutcome::Contained => r.drop_on_stamp.push((wip.clone(), sha.clone())),
            ResumeOutcome::Landed | ResumeOutcome::NotMerged => {
                if held_in_wip {
                    resume_held_into(&mut r);
                }
            }
        }
    }
    let failed = wip_ref(issue, WipKind::Failed);
    if let Some(sha) = ref_sha(runner, &failed) {
        if is_ancestor(runner, &sha, base) {
            let _ = crate::gitrange::run_git(runner, &["update-ref", "-d", &failed, &sha]);
        } else if is_ancestor(runner, &sha, "HEAD") {
            r.drop_on_stamp.push((failed, sha));
        } else {
            let _ = amt.comment_as(
                issue,
                &format!("sirius: an earlier attempt's work failed its gate or review — kept at {failed} ({}), offered to the agent as SIRIUS_PRIOR_WORK (not merged)", short_sha(&sha)),
                worker,
            );
            r.prior_work = Some((failed, sha));
        }
    }
    r
}

/// The work is stamped (completion): retire the markers it contains. The
/// held ref keeps its SIRF-32 form; wip refs are removed only if they still
/// point where this iteration saw them (a lost-lease sibling may have pinned
/// newer work meanwhile). Prior failed work the completed work does not
/// contain is parked — the issue is done; it must not be offered again.
fn retire_resumed(runner: &dyn Runner, issue: &str, resume: &Resume) {
    let held = held_ref(issue);
    for (r, sha) in &resume.drop_on_stamp {
        if *r == held {
            let _ = crate::gitrange::run_git(runner, &["update-ref", "-d", r]);
        } else {
            let _ = crate::gitrange::run_git(runner, &["update-ref", "-d", r, sha]);
        }
    }
    if let Some((r, sha)) = &resume.prior_work {
        if !is_ancestor(runner, sha, "HEAD") {
            park_superseded(runner, issue, sha);
        }
        let _ = crate::gitrange::run_git(runner, &["update-ref", "-d", r, sha]);
    }
}

// ── SIRF-42: a fresh base per iteration ────────────────────────────────────

/// The commit this iteration's worktree is reset to: the base ref's tip
/// right now (`git rev-parse --verify <ref>^{commit}`), falling back to the
/// launch base when the fleet has no base ref or it does not resolve.
fn iteration_base(runner: &dyn Runner, base_ref: Option<&str>, launch: &str) -> String {
    static NOTED_DETACHED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let Some(r) = base_ref else {
        if !NOTED_DETACHED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "sirius: no base ref (detached launch, no review.base_ref) — every iteration builds on the launch base {}",
                short_sha(launch)
            );
        }
        return launch.to_string();
    };
    match crate::gitrange::run_git(
        runner,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{r}^{{commit}}"),
        ],
    ) {
        Ok(o) if !o.stdout.trim().is_empty() => o.stdout.trim().to_string(),
        _ => {
            eprintln!(
                "sirius: base ref `{r}` does not resolve to a commit — this iteration builds on the launch base {}",
                short_sha(launch)
            );
            launch.to_string()
        }
    }
}

// ── SIRF-49: the built-in branch guard ─────────────────────────────────────

/// What [`guard_branch`] found and did.
struct BranchGuard {
    branch: String,
    sha: Option<String>,
    /// Where the branch's commit is pinned (None: unborn, or the pin failed).
    keep_ref: Option<String>,
    detached: bool,
}

/// A ref path segment: lowercase (macOS refs are case-insensitive files —
/// `sirius/LYD-54` and `sirius/lyd-54` collide), `[a-z0-9_-]` only.
fn ref_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// SIRF-49: an agent left a named branch checked out in its worktree. Parked
/// there it blocks sibling worktrees from checking it out, and the next
/// iteration's reset would wipe it (Lydgr: on macOS `sirius/LYD-54` IS the
/// file `sirius/lyd-54` sirius stamps). Pin its commit at
/// `refs/sirius/keep/<issue>/<branch>` and detach HEAD in place (plumbing
/// `update-ref --no-deref`, which works with any index state and runs no
/// hooks — `checkout --detach` refuses mid-merge). The branch itself is
/// never touched or deleted. An UNBORN branch (`switch --orphan`) has no
/// commit to pin: HEAD is detached at `fallback` (the iteration's floor), so
/// sirius's own commits can never land as root commits on the orphan.
fn guard_branch(runner: &dyn Runner, issue: &str, fallback: &str) -> Option<BranchGuard> {
    let full = crate::gitrange::run_git(runner, &["symbolic-ref", "-q", "HEAD"])
        .ok()?
        .stdout
        .trim()
        .to_string();
    if full.is_empty() {
        return None; // detached — as it should be
    }
    let branch = full
        .strip_prefix("refs/heads/")
        .unwrap_or(&full)
        .to_string();
    let sha = crate::gitrange::head_rev(runner).ok();
    let mut keep_ref = None;
    if let Some(sha) = &sha {
        let mut keep = format!(
            "refs/sirius/keep/{}/{}",
            issue.to_lowercase(),
            ref_segment(&branch)
        );
        // An earlier pin of a same-named branch this commit does not contain
        // is kept, not overwritten.
        if let Some(old) = ref_sha(runner, &keep) {
            if old != *sha && !is_ancestor(runner, &old, sha) {
                keep = format!("{keep}-{}", short_sha(sha));
            }
        }
        if crate::gitrange::run_git(runner, &["update-ref", &keep, sha]).is_ok() {
            keep_ref = Some(keep);
        }
    }
    let at = sha.as_deref().unwrap_or(fallback);
    let detached =
        crate::gitrange::run_git(runner, &["update-ref", "--no-deref", "HEAD", at]).is_ok();
    eprintln!(
        "sirius: WARNING {issue}: the agent left branch `{branch}` checked out in its worktree — {}{}",
        match &keep_ref {
            Some(k) => format!("pinned at {k}"),
            None => "could NOT pin it".into(),
        },
        if detached {
            "; HEAD detached (the branch itself is untouched)"
        } else {
            "; detaching FAILED"
        }
    );
    Some(BranchGuard {
        branch,
        sha,
        keep_ref,
        detached,
    })
}

/// Blocking `conflict` findings: the work does not merge onto `r` at `cur`.
fn base_conflict_findings(round: u32, r: &str, cur: &str, files: &[String]) -> Vec<Finding> {
    let short = &cur[..cur.len().min(10)];
    files
        .iter()
        .enumerate()
        .map(|(i, f)| Finding {
            id: format!("R{round}-c{}", i + 1),
            kind: "conflict".into(),
            confidence: "confirmed".into(),
            file: Some(f.clone()),
            line: None,
            summary: format!("conflicts with the current {r} ({short})"),
            scenario: format!("merging this issue's work onto {r} at {short} conflicts in {f}"),
            fix: format!(
                "merge the current base into the issue work (`git merge {cur}`) and resolve the conflict"
            ),
            response: None,
        })
        .collect()
}

/// Create the throwaway review tree for this worker, detached at `at`. The
/// reviewer ALWAYS runs in one (SIRF-29): its cwd-relative side effects —
/// hooks, caches, test runs — land in a tree that is deleted afterwards, and
/// only a deliberate edit of the worker's tree still reads as tampering.
fn add_review_tree(cx: &ReviewCtx, at: &str) -> Result<std::path::PathBuf, String> {
    let t = cx
        .fleet
        .sirius_dir
        .join("worktrees")
        .join(format!("{}-review", safe_name(cx.worker)));
    let t_str = crate::gitrange::git_path(&t);
    remove_review_tree(cx, &t);
    worktree_admin(|| {
        crate::gitrange::run_git(cx.runner, &["worktree", "add", "--detach", &t_str, at])
    })
    .map_err(|e| format!("cannot create the review tree: {e}"))?;
    Ok(t)
}

/// Remove a throwaway review tree (always — even on error paths).
fn remove_review_tree(cx: &ReviewCtx, t: &std::path::Path) {
    let t_str = crate::gitrange::git_path(t);
    worktree_admin(|| {
        let _ = crate::gitrange::run_git(cx.runner, &["worktree", "remove", "--force", &t_str]);
    });
    let _ = std::fs::remove_dir_all(t);
}

/// Run ONE reviewer attempt for `round` (attempt `try_n` within the round).
fn review_once(
    cx: &ReviewCtx,
    review_cmd: &str,
    round: u32,
    try_n: u32,
    prev_findings: Option<&std::path::Path>,
) -> ReviewOnce {
    let rc = &cx.config.review;
    let fleet = cx.fleet;
    // SIRF-26: never spawn into a paused fleet (another worker hit a limit).
    if let Some(line) = fleet.paused() {
        return ReviewOnce::Failed {
            result: RoundResult::Error,
            detail: format!("fleet paused (\"{line}\") — review not started"),
        };
    }
    let head = match checkpoint(cx, round) {
        Ok(h) => h,
        Err(detail) => {
            return ReviewOnce::Failed {
                result: RoundResult::Error,
                detail,
            }
        }
    };
    let reviews_dir = fleet.sirius_dir.join("reviews");
    let _ = std::fs::create_dir_all(&reviews_dir);
    let stem = format!(
        "{}-r{round}-t{try_n}{}-{}",
        safe_name(cx.issue),
        if fleet.on_fallback() { "-fb" } else { "" },
        unix_secs()
    );

    // Which tree to review: the worker's own, or a throwaway merge of the
    // work onto the CURRENT base tip (catches clashes with work merged since
    // this iteration began). Falls back to the iteration base when there is
    // nothing newer.
    let mut diff_range = format!("{}..HEAD", cx.base);
    // The merge tree, when the review is against a moved base.
    let mut tmp: Option<std::path::PathBuf> = None;
    let base_ref = rc.base_ref.clone().or_else(|| fleet.base_ref.clone());
    let cur = base_ref.as_deref().and_then(|r| {
        crate::gitrange::run_git(
            cx.runner,
            &["rev-parse", "--verify", &format!("{r}^{{commit}}")],
        )
        .map(|o| o.stdout.trim().to_string())
        .ok()
        .filter(|c| !c.is_empty())
    });
    // SIRF-30/31: the in-flight siblings, and the facts no reviewer decides.
    let tip = cur.clone().unwrap_or_else(|| cx.base.to_string());
    let frontier_mode = rc.against == crate::config::ReviewAgainst::Frontier;
    fleet.register_inflight(cx.worker, cx.issue, &head);
    let (sibs, sibs_complete) = if frontier_mode || !rc.sequences.is_empty() {
        let awaiting = crate::frontier::awaiting(cx.amt, &cx.config.target_status);
        let d = crate::frontier::siblings(cx.runner, &awaiting, &tip, cx.issue);
        (d.sibs, d.complete)
    } else {
        (Vec::new(), true)
    };
    // Sequence slots are also contested by this fleet's peers under review
    // right now (no branch yet): the one that got here first owns the slot.
    let mut seq_sibs = sibs.clone();
    for (issue, peer_head) in fleet.peers_before(cx.worker) {
        if issue != cx.issue && !seq_sibs.iter().any(|s| s.issue == issue) {
            seq_sibs.push(crate::frontier::Sibling {
                branch: format!("in review in this fleet ({issue})"),
                issue,
                title: String::new(),
                tip: peer_head,
                files: Vec::new(),
            });
        }
    }
    let (mut auto, seq_complete) = crate::frontier::sequence_collisions(
        cx.runner,
        &rc.sequences,
        cx.base,
        &head,
        &tip,
        &seq_sibs,
    );
    let facts_complete = sibs_complete && seq_complete;
    let mut frontier_env = vec![
        kv("SIRIUS_FRONTIER", ""),
        kv("SIRIUS_SIBLING_BRANCHES", ""),
        kv("SIRIUS_SIBLINGS", "(none)"),
    ];
    if frontier_mode {
        let t = match add_review_tree(cx, &tip) {
            Ok(t) => t,
            Err(detail) => {
                return ReviewOnce::Failed {
                    result: RoundResult::Error,
                    detail,
                }
            }
        };
        let t_str = crate::gitrange::git_path(&t);
        // Merge only the oldest MAX_SIBLINGS (next in line); the sequence
        // check above already covered every sibling.
        let merge_sibs = &sibs[..sibs.len().min(crate::frontier::MAX_SIBLINGS)];
        match crate::frontier::prepare(cx.runner, &t_str, &tip, &head, merge_sibs) {
            crate::frontier::Prepared::Ready {
                frontier,
                merged,
                left_out,
                sibling_conflicts,
            } => {
                let ours = crate::gitrange::changed_files(
                    cx.runner,
                    Some(&format!("{}..{head}", cx.base)),
                )
                .unwrap_or_default();
                let branches: Vec<&str> = merged.iter().map(|s| s.branch.as_str()).collect();
                let mut summary =
                    crate::frontier::render_siblings(&merged, &left_out, &sibling_conflicts, &ours);
                if sibs.len() > merge_sibs.len() {
                    summary.push_str(&format!(
                        "\n- … and {} newer in-flight issue(s) not merged into the review tree",
                        sibs.len() - merge_sibs.len()
                    ));
                }
                frontier_env = vec![
                    kv("SIRIUS_FRONTIER", frontier.as_str()),
                    kv("SIRIUS_SIBLING_BRANCHES", branches.join(",")),
                    kv("SIRIUS_SIBLINGS", summary),
                ];
                diff_range = format!("{frontier}..HEAD");
                auto.extend(sibling_conflicts);
                tmp = Some(t);
            }
            crate::frontier::Prepared::BaseConflict {
                files,
                sibling_conflicts,
            } => {
                remove_review_tree(cx, &t);
                let label = base_ref.as_deref().unwrap_or("the iteration base");
                let mut findings = base_conflict_findings(round, label, &tip, &files);
                findings.extend(sibling_conflicts);
                findings.append(&mut auto);
                return ReviewOnce::Conflicts {
                    findings,
                    head,
                    facts_complete,
                };
            }
            crate::frontier::Prepared::Failed(detail) => {
                remove_review_tree(cx, &t);
                return ReviewOnce::Failed {
                    result: RoundResult::Error,
                    detail,
                };
            }
        }
    } else if rc.against == crate::config::ReviewAgainst::CurrentBaseMerge {
        match (base_ref, cur) {
            (Some(r), Some(cur)) if cur != cx.base => {
                let t = match add_review_tree(cx, &cur) {
                    Ok(t) => t,
                    Err(detail) => {
                        return ReviewOnce::Failed {
                            result: RoundResult::Error,
                            detail,
                        }
                    }
                };
                let t_str = crate::gitrange::git_path(&t);
                // A throwaway merge commit, never on any branch — so a fixed
                // identity is fine and the user's git config is not needed.
                let mut args = vec!["-C", t_str.as_str()];
                args.extend(crate::frontier::THROWAWAY_MERGE_CONFIG);
                args.extend(["merge", "--no-ff", "--no-edit", &head]);
                let merged = crate::gitrange::run_git(cx.runner, &args);
                if let Err(e) = merged {
                    let conflicts: Vec<String> = crate::gitrange::run_git(
                        cx.runner,
                        &["-C", &t_str, "diff", "--name-only", "--diff-filter=U"],
                    )
                    .map(|o| {
                        o.stdout
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                    remove_review_tree(cx, &t);
                    if conflicts.is_empty() {
                        return ReviewOnce::Failed {
                            result: RoundResult::Error,
                            detail: format!("merging onto {r} failed: {e}"),
                        };
                    }
                    let mut findings = base_conflict_findings(round, &r, &cur, &conflicts);
                    findings.append(&mut auto);
                    return ReviewOnce::Conflicts {
                        findings,
                        head,
                        facts_complete,
                    };
                }
                diff_range = format!("{cur}..HEAD");
                tmp = Some(t);
            }
            (None, _) => eprintln!(
                "sirius: review.against=current-base-merge but no base_ref (detached launch) — reviewing against the iteration base"
            ),
            (Some(r), None) => eprintln!(
                "sirius: review base_ref `{r}` does not resolve to a commit — reviewing against the iteration base instead"
            ),
            _ => {} // the base has not moved: the plain diff IS the merge
        }
    }
    // No merge tree: review the checkpoint itself — still in a throwaway
    // tree, never in the worker's (SIRF-29).
    let review_dir = match tmp {
        Some(t) => t,
        None => match add_review_tree(cx, &head) {
            Ok(t) => t,
            Err(detail) => {
                return ReviewOnce::Failed {
                    result: RoundResult::Error,
                    detail,
                }
            }
        },
    };

    let out_path = reviews_dir.join(format!("{stem}.json"));
    let _ = std::fs::remove_file(&out_path);
    let findings_path = prev_findings
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none — this is round 1)".into());
    let mut vars: Vec<(String, String)> = cx.base_env.to_vec();
    vars.extend([
        kv("SIRIUS_PHASE", "review"),
        kv("SIRIUS_REVIEW_DIR", review_dir.display().to_string()),
        kv("SIRIUS_DIFF_RANGE", diff_range),
        kv("SIRIUS_ROUND", round.to_string()),
        kv("SIRIUS_REVIEW_OUT", out_path.display().to_string()),
    ]);
    vars.extend(frontier_env);
    // SIRF-35: what got past review before, live kinds only.
    vars.push(kv(
        "SIRIUS_ESCAPES",
        crate::escape::patterns_section(
            &cx.ledger.escapes(None).unwrap_or_default(),
            &cx.ledger.automated_kinds().unwrap_or_default(),
            &|_| false,
            crate::escape::PROMPT_TOP,
        ),
    ));
    let mut rendered = crate::review::render_prompt(
        &fleet.review_prompt,
        &[
            vars.clone(),
            vec![kv("SIRIUS_REVIEW_FINDINGS", findings_path.clone())],
        ]
        .concat(),
    );
    // A custom template predating SIRF-30 still learns what is in flight.
    crate::review::append_missing_section(
        &mut rendered,
        &fleet.review_prompt,
        "SIRIUS_ESCAPES",
        env_value(&vars, "SIRIUS_ESCAPES"),
        crate::review::ESCAPES_HEADING,
    );
    crate::review::append_missing_section(
        &mut rendered,
        &fleet.review_prompt,
        "SIRIUS_SIBLINGS",
        env_value(&vars, "SIRIUS_SIBLINGS"),
        "Other in-flight changes (other issues' branches awaiting integration):",
    );
    let prompt_path = reviews_dir.join(format!("{stem}-prompt.md"));
    let _ = std::fs::write(&prompt_path, rendered);
    let mut env = vars;
    env.push(kv(
        "SIRIUS_REVIEW_PROMPT",
        prompt_path.display().to_string(),
    ));
    // SIRF-26: the reviewer's model, explicit (ideally not the workers').
    let review_on_fallback = fleet.on_fallback();
    let review_model =
        crate::models::review_model(crate::models::active(&cx.config.models, review_on_fallback));
    if let Some(m) = &review_model {
        env.push(kv("ANTHROPIC_MODEL", m.as_str()));
        env.push(kv("SIRIUS_REVIEW_MODEL", m.as_str()));
    }
    if let Some(p) = prev_findings {
        env.push(kv("SIRIUS_REVIEW_FINDINGS", p.display().to_string()));
    }

    if let Err(reason) = (cx.renew_checked)() {
        remove_review_tree(cx, &review_dir);
        return ReviewOnce::LeaseLost(reason);
    }
    let fp_before = tree_fingerprint(cx.runner);
    let opts = AgentRunOpts {
        timeout: Duration::from_secs(rc.timeout_secs),
        heartbeat_interval: Duration::from_secs(cx.config.heartbeat_interval_secs()),
        // Absolute, under the fleet's .sirius/: the reviewer's final message
        // is read back from here when it could not write $SIRIUS_REVIEW_OUT.
        log_path: Some(fleet.sirius_dir.join("logs").join(format!("{stem}.log"))),
        env,
    };
    // A FRESH process: the reviewer's only inputs are the issue spec (via
    // amt), the diff, and the repo — never the worker's session.
    let cmd = format!(
        "cd \"$SIRIUS_REVIEW_DIR\" || exit 1; {}",
        template_cmd(review_cmd, cx.issue, cx.worker, review_model.as_deref())
    );
    let ran = cx
        .runner
        .run_agent("sh", &["-c", &cmd], &opts, &mut || (cx.renew)());

    remove_review_tree(cx, &review_dir);
    // READ-ONLY enforcement: any change to the worker's tree is discarded.
    if tree_fingerprint(cx.runner) != fp_before {
        revert_to(cx, &head);
        return ReviewOnce::Failed {
            result: RoundResult::Tampered,
            detail: "the reviewer modified the worktree; its changes were discarded".into(),
        };
    }
    match &ran {
        Err(e) => {
            return ReviewOnce::Failed {
                result: RoundResult::Error,
                detail: format!("reviewer did not run: {e}"),
            }
        }
        Ok(o) if o.timed_out() => {
            return ReviewOnce::Failed {
                result: RoundResult::Error,
                detail: format!("reviewer timed out after {}s", rc.timeout_secs),
            }
        }
        Ok(_) => {}
    }
    let exit = ran.as_ref().ok().and_then(|o| o.output().code);
    // SIRF-26: a reviewer stopped by a usage limit pauses the fleet; the
    // round is a review error and is NOT retried (it would hit the same wall).
    if exit != Some(0) {
        let limit = opts
            .log_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|log| crate::models::fleet_stop(&log));
        if let Some(stop) = limit {
            let line = stop.line.clone();
            match on_usage_limit(cx.fleet, cx.config, review_on_fallback, &stop) {
                LimitAction::Fallback { first } => {
                    return ReviewOnce::FellBack { line, first };
                }
                LimitAction::Pause => {
                    return ReviewOnce::Failed {
                        result: RoundResult::Error,
                        detail: format!(
                            "the reviewer hit a fleet-wide stop — fleet paused (\"{line}\")"
                        ),
                    };
                }
            }
        }
    }
    // The findings file, OR the reviewer's final message: a headless reviewer
    // is usually NOT allowed to write files (and needs no write tools at all
    // — the strongest read-only guarantee), so it may print the JSON instead.
    // The file wins; prose or fences around it are tolerated. Then — only
    // for a reviewer that EXITED CLEANLY — its final message. A failed
    // reviewer's log may hold an echoed template or quoted findings; reading
    // those as its answer would fake a review.
    let raw = match crate::review::read_review_output(
        &out_path,
        opts.log_path.as_deref(),
        exit == Some(0),
        round,
    ) {
        Some(r) => r,
        None => {
            return ReviewOnce::Failed {
                result: RoundResult::Error,
                detail: format!(
                    "reviewer produced no findings JSON — neither $SIRIUS_REVIEW_OUT nor a final message that is one (exit {})",
                    exit.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                ),
            }
        }
    };
    match crate::review::parse_review(&raw, round) {
        Ok(mut report) => {
            // AUTO- findings are recomputed facts (SIRF-30/31): Sirius states
            // them, and a reviewer can neither restate nor resolve them.
            report.findings.retain(|f| !crate::review::is_auto(&f.id));
            report.previous.retain(|v| !crate::review::is_auto(&v.id));
            report.findings.append(&mut auto);
            ReviewOnce::Report {
                report,
                head,
                facts_complete,
            }
        }
        Err(detail) => ReviewOnce::Failed {
            result: RoundResult::Error,
            detail,
        },
    }
}

fn spine_emit(cx: &ReviewCtx, ty: &str, data: Value) {
    if let Some(sp) = cx.spine {
        sp.emit(
            ty,
            vec![
                crate::spine::issue_ref(cx.issue),
                crate::spine::worker_ref(cx.worker),
            ],
            data,
        );
    }
}

/// Apply `on_exhausted` / `on_review_error`.
fn escalate(
    cx: &ReviewCtx,
    policy: crate::config::ReviewEscalation,
    why: &str,
    open: Vec<Finding>,
    mut summary: ReviewSummary,
) -> ReviewStage {
    summary.open = open.len();
    let _ = cx.amt.comment_as(
        cx.issue,
        &crate::review::unresolved_comment(why, &open),
        cx.worker,
    );
    match policy {
        crate::config::ReviewEscalation::AdvanceFlagged => {
            // Advance anyway — the work is never hidden or thrown away — but
            // label it so the human sees exactly what is left.
            let _ = cx.amt.add_label(cx.issue, "review:open");
            summary.outcome = "flagged".into();
            spine_emit(
                cx,
                "review.flagged",
                json!({"issue": cx.issue, "action": "advance-flagged", "why": why, "open": open.len()}),
            );
            ReviewStage::Advance {
                summary: Some(summary),
                open,
            }
        }
        crate::config::ReviewEscalation::Release => {
            summary.outcome = "released".into();
            spine_emit(
                cx,
                "review.flagged",
                json!({"issue": cx.issue, "action": "release", "why": why, "open": open.len()}),
            );
            ReviewStage::Release {
                summary,
                why: why.to_string(),
            }
        }
    }
}

fn strip_responses(fs: &[Finding]) -> Vec<Finding> {
    fs.iter()
        .cloned()
        .map(|mut f| {
            f.response = None;
            f
        })
        .collect()
}

/// A review fix round: re-run the worker in fix mode with the given extra env
/// for the given round, then re-gate (the WORK⇄GATE loop, reused).
pub type FixRound<'a> = dyn Fn(&mut dyn Write, u32, &[(String, String)]) -> WorkGate + 'a;

/// The review stage: REVIEW ⇄ FIX until clean or `review.max_rounds` is spent.
/// `fix_round` re-runs the worker in fix mode and re-gates (WORK⇄GATE reuse).
pub fn run_review_stage(cx: &ReviewCtx, out: &mut dyn Write, fix_round: &FixRound) -> ReviewStage {
    let rc = &cx.config.review;
    let Some(review_cmd) = rc.cmd.as_deref().filter(|c| !c.trim().is_empty()) else {
        return ReviewStage::Off;
    };
    let issue = cx.issue;
    let worker = cx.worker;

    // A diff touching only `skip_paths` (docs, say) is not worth a review.
    if let Ok(files) = crate::gitrange::changed_since(cx.runner, Some(cx.base), &[]) {
        // Nothing changed (the gate was "skipped") or only skip_paths did.
        if files.is_empty() || crate::review::all_skippable(&files, &rc.skip_paths) {
            emit_event(
                out,
                worker,
                Some(issue),
                "review",
                json!({"round": 0, "result": "skipped", "confirmed": 0, "notes": 0}),
            );
            return ReviewStage::Advance {
                summary: Some(ReviewSummary {
                    outcome: "skipped".into(),
                    ..Default::default()
                }),
                open: vec![],
            };
        }
    }

    spine_emit(cx, "review.started", json!({"issue": issue}));
    let reviews_dir = cx.fleet.sirius_dir.join("reviews");
    let _ = std::fs::create_dir_all(&reviews_dir);
    let max_rounds = rc.max_rounds.max(1);
    let mut summary = ReviewSummary::default();
    // The last round's blocking findings, with the worker's responses.
    let mut previous: Vec<Finding> = Vec::new();
    let mut prev_path: Option<std::path::PathBuf> = None;
    let mut round: u32 = 0;
    loop {
        round += 1;
        summary.rounds = round;

        // Up to two attempts per round: a review error is retried once.
        let mut got: Option<ReviewOnce> = None;
        let mut last_err = String::new();
        let mut try_n: u32 = 0;
        while try_n < 2 {
            let this_try = try_n;
            try_n += 1;
            match review_once(cx, review_cmd, round, this_try, prev_path.as_deref()) {
                ReviewOnce::FellBack { line, first } => {
                    // SIRF-27: the same review again, on the fallback reviewer
                    // — not a failed try. Bounded: a second hit pauses. The
                    // primary attempt still leaves a trace (event + ledger).
                    emit_event(
                        out,
                        worker,
                        Some(issue),
                        "review",
                        json!({"round": round, "result": "fell_back", "confirmed": 0, "notes": 0, "detail": line}),
                    );
                    ledger_warn(
                        "insert_review_round",
                        cx.ledger.insert_review_round(
                            cx.iter_ref,
                            issue,
                            worker,
                            round,
                            "fell_back",
                            0,
                            0,
                            &json!({"fell_back": line}),
                        ),
                    );
                    if first {
                        announce_fallback(out, worker, issue, &line, cx.config);
                        spine_emit(
                            cx,
                            "fleet.fallback",
                            json!({"issue": issue, "phase": "review", "reason": line}),
                        );
                    }
                    try_n = this_try;
                }
                ReviewOnce::Failed { result, detail } => {
                    emit_event(
                        out,
                        worker,
                        Some(issue),
                        "review",
                        json!({"round": round, "result": result.as_str(), "confirmed": 0, "notes": 0, "detail": detail}),
                    );
                    ledger_warn(
                        "insert_review_round",
                        cx.ledger.insert_review_round(
                            cx.iter_ref,
                            issue,
                            worker,
                            round,
                            result.as_str(),
                            0,
                            0,
                            &json!({"error": detail}),
                        ),
                    );
                    if result == RoundResult::Tampered {
                        spine_emit(
                            cx,
                            "review.tampered",
                            json!({"issue": issue, "round": round}),
                        );
                    }
                    last_err = detail;
                    // A usage limit is not transient: never retry into it.
                    if let Some(line) = cx.fleet.paused() {
                        spine_emit(
                            cx,
                            "fleet.paused",
                            json!({"issue": issue, "phase": "review", "reason": line}),
                        );
                        break;
                    }
                }
                ReviewOnce::LeaseLost(reason) => return ReviewStage::LeaseLost(reason),
                ok => {
                    got = Some(ok);
                    break;
                }
            }
        }
        let (rec, head) = match got {
            Some(ReviewOnce::Report {
                mut report,
                head,
                facts_complete,
            }) => {
                if !facts_complete {
                    // A fact we could not recompute is not a fact that went
                    // away: keep the previous round's AUTO findings open.
                    for p in strip_responses(&previous) {
                        if crate::review::is_auto(&p.id)
                            && !report.findings.iter().any(|f| f.id == p.id)
                        {
                            report.findings.push(p);
                        }
                    }
                }
                (
                    crate::review::reconcile(&previous, &report, &rc.block_on),
                    head,
                )
            }
            Some(ReviewOnce::Conflicts {
                findings,
                head,
                facts_complete,
            }) => {
                // No review ran this round: the previous findings are still
                // unverified, so they stay open alongside the conflicts —
                // except AUTO- facts this round recomputed. A base conflict
                // ALWAYS blocks (whatever block_on says); only the AUTO facts
                // go through the blocking rule.
                let (mut blocking, notes): (Vec<Finding>, Vec<Finding>) =
                    findings.into_iter().partition(|f| {
                        !crate::review::is_auto(&f.id)
                            || crate::review::is_blocking(f, &rc.block_on)
                    });
                let fresh: Vec<String> = blocking
                    .iter()
                    .chain(notes.iter())
                    .map(|f| f.id.clone())
                    .collect();
                blocking.extend(strip_responses(&previous).into_iter().filter(|p| {
                    !fresh.contains(&p.id) && (!crate::review::is_auto(&p.id) || !facts_complete)
                }));
                (
                    crate::review::Reconciled {
                        blocking,
                        notes,
                        ..Default::default()
                    },
                    head,
                )
            }
            _ => {
                if let Some(why) = integration_pause(cx.fleet) {
                    return ReviewStage::Held(why);
                }
                let why = format!("the review could not complete in round {round} ({last_err})");
                return escalate(
                    cx,
                    rc.on_review_error,
                    &why,
                    strip_responses(&previous),
                    summary,
                );
            }
        };
        summary.fixed += rec.fixed;
        summary.rebuttals_accepted += rec.rebuttals_accepted;
        let result = if rec.blocking.is_empty() {
            RoundResult::Clean
        } else {
            RoundResult::Blocking
        };
        let round_json = json!({"findings": rec.blocking, "notes": rec.notes});
        ledger_warn(
            "insert_review_round",
            cx.ledger.insert_review_round(
                cx.iter_ref,
                issue,
                worker,
                round,
                result.as_str(),
                rec.blocking.len(),
                rec.notes.len(),
                &round_json,
            ),
        );
        emit_event(
            out,
            worker,
            Some(issue),
            "review",
            json!({"round": round, "result": result.as_str(), "confirmed": rec.blocking.len(), "notes": rec.notes.len()}),
        );
        let _ = cx.amt.comment_as(
            issue,
            &crate::review::round_comment(round, &rec, round > 1),
            worker,
        );
        for f in &rec.blocking {
            spine_emit(
                cx,
                "review.finding",
                json!({"issue": issue, "round": round, "id": f.id, "kind": f.kind,
                       "file": f.file, "line": f.line, "summary": f.summary}),
            );
        }
        if rec.blocking.is_empty() {
            // A re-claimed issue that was flagged on an earlier pass must not
            // keep a stale `review:open` label once a review comes back clean.
            let _ = cx.amt.remove_label(issue, "review:open");
            summary.outcome = "clean".into();
            spine_emit(
                cx,
                "review.passed",
                json!({"issue": issue, "rounds": round}),
            );
            return ReviewStage::Advance {
                summary: Some(summary),
                open: vec![],
            };
        }
        if round >= max_rounds {
            let why = format!("{round} review round(s) used up with confirmed findings open");
            return escalate(cx, rc.on_exhausted, &why, rec.blocking, summary);
        }

        // FIX: the worker gets the findings and answers each one.
        let findings_path =
            reviews_dir.join(format!("{}-r{round}-findings.json", safe_name(issue)));
        let fix_out = reviews_dir.join(format!("{}-r{round}-fix.json", safe_name(issue)));
        let _ = std::fs::write(&findings_path, round_json.to_string());
        let _ = std::fs::remove_file(&fix_out);
        let env = vec![
            kv("SIRIUS_ROUND", round.to_string()),
            kv("SIRIUS_REVIEW_DIR", cx.fleet.worktree.display().to_string()),
            kv("SIRIUS_DIFF_RANGE", format!("{}..HEAD", cx.base)),
            kv(
                "SIRIUS_REVIEW_FINDINGS",
                findings_path.display().to_string(),
            ),
            kv("SIRIUS_FIX_OUT", fix_out.display().to_string()),
        ];
        match fix_round(out, round, &env) {
            WorkGate::Exit(o) => return ReviewStage::Exit(o),
            WorkGate::Done {
                work_ok,
                gate_result,
                timed_out,
                ..
            } => {
                if !work_ok || gate_result == "fail" {
                    // Never trade gate-passing work for a broken fix: put the
                    // last reviewed (and gated) state back, then escalate.
                    // The discarded attempt is parked first (SIRF-41: a
                    // killed fix agent may have finished, too).
                    if let Some(parked) = park_attempt(cx.runner, issue, &head) {
                        let _ = cx.amt.comment_as(
                            issue,
                            &format!("sirius: fix round {round} did not hold — its work is parked at {parked}; reverted to the last reviewed state"),
                            worker,
                        );
                    }
                    revert_to(cx, &head);
                    let paused = cx.fleet.paused();
                    if let Some(line) = &paused {
                        spine_emit(
                            cx,
                            "fleet.paused",
                            json!({"issue": issue, "phase": "fix", "reason": line}),
                        );
                    }
                    let (policy, why) = if let Some(line) = paused {
                        (
                            rc.on_review_error,
                            format!("the fleet hit a fleet-wide stop during fix round {round} (\"{line}\") — fleet paused; reverted to the last reviewed state"),
                        )
                    } else if timed_out {
                        (
                            rc.on_review_error,
                            format!(
                                "the fix-mode worker timed out in round {round} (killed by the agent timeout — idle or hard cap; the fix event's timeout_kind says which); reverted to the last reviewed state"
                            ),
                        )
                    } else if !work_ok {
                        (
                            rc.on_review_error,
                            format!("the fix-mode worker failed in round {round}; reverted to the last reviewed state"),
                        )
                    } else {
                        (
                            rc.on_exhausted,
                            format!("fix round {round} broke the gate and could not repair it within retry_budget; reverted to the last passing state"),
                        )
                    };
                    // (The last reviewed checkpoint is already restored.)
                    if let Some(why) = integration_pause(cx.fleet) {
                        return ReviewStage::Held(why);
                    }
                    return escalate(cx, policy, &why, rec.blocking, summary);
                }
            }
        }
        let fix = crate::review::parse_fix(std::fs::read_to_string(&fix_out).ok().as_deref());
        let _ = cx
            .amt
            .comment_as(issue, &crate::review::fix_comment(round, &fix), worker);
        let mut answered = rec.blocking;
        crate::review::attach_responses(&mut answered, &fix);
        // The next reviewer sees each finding next to the worker's answer.
        let _ = std::fs::write(
            &findings_path,
            json!({"findings": answered, "notes": rec.notes}).to_string(),
        );
        prev_path = Some(findings_path);
        previous = answered;
    }
}

/// The receipt's decision title/body: the review history when a review ran.
fn decision_text(issue: &str, review: &ReviewStage) -> (String, String) {
    match review {
        ReviewStage::Advance {
            summary: Some(s),
            open,
        } => {
            let mut body = format!("See linked entities.\n\n{}", s.line());
            if !open.is_empty() {
                body.push_str("\n\nOpen (label review:open):");
                for f in open {
                    body.push_str(&format!("\n- [{}] {} — {}", f.id, f.location(), f.summary));
                }
            }
            (format!("Resolved {issue} via sirius · {}", s.line()), body)
        }
        _ => (
            format!("Resolved {issue} via sirius"),
            "See linked entities.".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};

    /// The recorded argv for a WORKER agent invocation, built from whatever
    /// shell `resolve_shell` picks on THIS machine.
    ///
    /// SF-15: worker spawns resolve their shell explicitly now, so asserting
    /// the literal `"sh -c …"` only held where an MSYS `sh` happened to be on
    /// PATH — the inherited-shell assumption the ticket is about, reproduced
    /// inside our own suite. `MockRunner`'s PREFIX matching is shell-
    /// canonicalized, but `recorded()` returns the raw argv joined by spaces,
    /// so exact-string expectations have to be built. Review spawns still run
    /// literal `sh` (their script is sirius-built POSIX), so those assertions
    /// deliberately stay literal.
    fn agent_call(cmd: &str) -> String {
        format!("{}{cmd}", shell_prefix())
    }

    /// `"<resolved shell> <flag> "` — the recorded prefix of any worker spawn.
    fn shell_prefix() -> String {
        let sh = crate::shell::resolve_shell();
        format!("{} {} ", sh.program, sh.flag)
    }

    /// A fleet context rooted in a private temp dir (review files land there,
    /// never in the repo's own `.sirius/`).
    fn test_fleet(base: &str) -> Fleet {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("sirius-fleet-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fleet {
            base: base.into(),
            base_ref: None,
            worktree: dir.join("wt"),
            sirius_dir: dir,
            review_prompt: crate::review::DEFAULT_PROMPT.into(),
            pause: Default::default(),
            fallback: Default::default(),
            inflight: Default::default(),
        }
    }

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
            LockResult::Locked {
                claim_ids,
                verdicts,
            } => {
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
            LockResult::Locked {
                claim_ids,
                verdicts,
            } => {
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"id":"AMT-11"}"#,
        );
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
            &amt,
            &hv,
            &led,
            &cfg(),
            &m,
            "sirius/oak",
            Some("todo"),
            "sleep 999",
            &mut out,
            None,
            None,
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
        assert!(
            !calls.iter().any(|c| c.contains("git diff")),
            "no gate on timeout"
        );
        // A deadend note was filed naming the timeout.
        assert!(calls
            .iter()
            .any(|c| c.contains("hayven remember") && c.contains("timed out")));

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
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"id":"AMT-12","title":"Long"}"#,
        );
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"id":"AMT-12"}"#,
        );
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
            &amt,
            &hv,
            &led,
            &cfg(),
            &m,
            "sirius/oak",
            Some("todo"),
            "long-cmd",
            &mut out,
            None,
            None,
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
        m.push(MockResponse::new(
            &["hayven", "release"],
            1,
            "",
            "daemon busy",
        ));
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
            m.push(MockResponse::new(
                &["hayven", "release"],
                1,
                "",
                "still down",
            ));
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
        // The failure was recorded, not swallowed — as an ANOMALY, not a 409:
        // a daemon protocol defect is not contention, and counting it as one
        // helped latch adaptive mode into pre-claiming.
        assert_eq!(led.count_policy_events("claim_anomaly", 100).unwrap(), 1);
        assert_eq!(led.count_policy_events("backoff_409", 100).unwrap(), 0);
    }

    // ---- SIRF-9: retry_budget reruns the WORK+GATE sequence -------------

    /// Program a full happy-path claim→map→lock→brief prefix, then leave the
    /// WORK/GATE/RELEASE calls for the caller to queue per scenario. The mock's
    /// longest-prefix matching + benign-default keeps incidental calls quiet.
    fn program_prefix(m: &MockRunner, issue: &str) {
        m.expect(
            &["amt", "--json", "claim"],
            0,
            &format!(r#"{{"id":"{issue}","title":"T"}}"#),
        );
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
    }

    // SIRF-11: the agent COMMITS its work, so a worktree-vs-HEAD diff is
    // empty. The gate must diff against the pre-work baseline and still run
    // tests — the old behavior skipped the gate and advanced untested.
    #[test]
    fn committed_agent_work_still_gates() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-30");
        m.expect(&["git", "rev-parse"], 0, "base123\n");
        m.expect(&["sh", "-c"], 0, ""); // agent (commits its work)
                                        // Diff vs the BASELINE sees the committed change; bare HEAD would not.
        m.expect(&["git", "diff", "--name-only", "base123"], 0, "src/x.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["x"],"note":"","tests":[]}"#,
        );
        m.expect(&["sh", "-c"], 0, "test result: ok"); // full suite (no runnables → doubt)
        m.expect(
            &["amt", "--json", "issue", "update"],
            0,
            r#"{"id":"AMT-30"}"#,
        );
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-9","resolves":"AMT-30"}"#,
        );
        m.expect(
            &["amt", "--json", "decision", "show"],
            0,
            r#"{"id":"D-9","resolves":"AMT-30"}"#,
        );
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-30"}"#);

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
            None,
            None,
        );
        assert_eq!(o, IterationOutcome::Completed);
        // The gate diffed against the pinned baseline, not bare HEAD.
        assert!(
            m.recorded()
                .iter()
                .any(|c| c == "git diff --name-only base123"),
            "calls: {:?}",
            m.recorded()
        );
        let (outcome, gate): (String, String) = led
            .conn
            .query_row("SELECT outcome, gate_result FROM iterations", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(outcome, "completed");
        assert_eq!(gate, "pass", "the gate must RUN, not skip");
    }

    // Isolated iterations (parallel fleet) reset the private worktree to the
    // fleet base DETACHED (a parked branch would block siblings re-claiming
    // the issue), diff against the base, take the doubt path (the daemon
    // cannot see the worktree), and PRESERVE completed work by auto-commit +
    // stamping the per-issue branch.
    #[test]
    fn isolated_iteration_detaches_gates_and_preserves() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-40");
        // Worktree prep: reset → clean → DETACHED checkout, all vs the base.
        m.expect(&["git", "reset"], 0, "");
        m.expect(&["git", "clean"], 0, "");
        m.expect(&["git", "checkout"], 0, "");
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n");
        m.expect(&["sh", "-c"], 0, "ok"); // full suite (doubt path) passes
        m.expect(
            &["amt", "--json", "issue", "update"],
            0,
            r#"{"id":"AMT-40"}"#,
        );
        m.expect(&["hayven", "release"], 0, "ok");
        // Preserve: add → commit → clean check → branch stamp.
        m.expect(&["git", "add"], 0, "");
        m.expect(&["git", "commit"], 0, "");
        m.expect(&["git", "status"], 0, "");
        m.expect(&["git", "branch"], 0, "");
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-2","resolves":"AMT-40"}"#,
        );
        m.expect(
            &["amt", "--json", "decision", "show"],
            0,
            r#"{"id":"D-2","resolves":"AMT-40"}"#,
        );
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-40"}"#);

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
            None,
            Some(&test_fleet("base999")),
        );
        assert_eq!(o, IterationOutcome::Completed);
        let calls = m.recorded();
        assert!(
            calls.iter().any(|c| c == "git reset --hard base999"),
            "{calls:?}"
        );
        assert!(calls.iter().any(|c| c == "git clean -fd"), "{calls:?}");
        // DETACHED — never a named branch at prep time.
        assert!(
            calls.iter().any(|c| c == "git checkout --detach base999"),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("git checkout -B")),
            "{calls:?}"
        );
        // The baseline is the fleet base — no rev-parse before the agent ran.
        assert!(
            !calls.iter().any(|c| c.starts_with("git rev-parse")),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c == "git diff --name-only base999"),
            "{calls:?}"
        );
        // Isolated mode NEVER consults the (stale-graph) selector.
        assert!(
            !calls.iter().any(|c| c.starts_with("hayven affected-tests")),
            "{calls:?}"
        );
        // Completed work is preserved on the per-issue branch.
        assert!(
            calls
                .iter()
                .any(|c| c == "git branch -f sirius/amt-40 HEAD"),
            "{calls:?}"
        );
    }

    // A lease REFUSAL at the pre-spawn check must abort before the expensive
    // agent runs — warning and spawning anyway put two agents on one issue.
    #[test]
    fn lost_lease_aborts_before_agent_spawn() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-42");
        // The pre-spawn heartbeat (amt claim --issue …) is REFUSED.
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"claimed":false,"reason":"lease held by sirius/rowan"}"#,
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
            None,
            None,
        );
        match o {
            IterationOutcome::Error(e) => assert!(e.contains("lease"), "{e}"),
            other => panic!("lost lease must abort as an error, got {other:?}"),
        }
        let calls = m.recorded();
        // The agent never spawned, and the issue was NOT amt-released (the
        // lease is not ours to release).
        assert!(!calls.iter().any(|c| *c == agent_call("true")), "{calls:?}");
        assert!(
            !calls.iter().any(|c| c.starts_with("amt --json release")),
            "{calls:?}"
        );
    }

    // A worktree that cannot be reset is a workspace that cannot be trusted:
    // the iteration must fail (releasing the issue), never gate over an
    // unknown baseline.
    #[test]
    fn isolated_iteration_fails_when_worktree_prep_fails() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-41");
        m.push(MockResponse::new(
            &["git", "reset"],
            128,
            "",
            "fatal: unable to write index",
        ));
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-41"}"#);
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
            None,
            Some(&test_fleet("base999")),
        );
        match o {
            IterationOutcome::Error(e) => assert!(e.contains("worktree prep"), "{e}"),
            other => panic!("prep failure must be an error, got {other:?}"),
        }
        // The agent never ran.
        assert!(
            !m.recorded().iter().any(|c| *c == agent_call("true")),
            "{:?}",
            m.recorded()
        );
    }

    // A STRUCTURAL gate failure (no tests ran: unconfigured test_cmd, blocked
    // policy) must not consume the retry budget — a fresh agent attempt cannot
    // change it, and the old behavior re-ran the whole agent retry_budget
    // times per issue against a gate that could never pass (field-observed as
    // a fleet burning 3× agent time and completing nothing).
    #[test]
    fn unconfigured_gate_does_not_burn_agent_retries() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-33");
        m.expect(&["git", "rev-parse"], 0, "base123\n");
        m.expect(&["sh", "-c"], 0, ""); // agent, ONCE
        m.expect(&["git", "diff", "--name-only", "base123"], 0, "src/x.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["x"],"note":"","tests":[]}"#,
        );
        // No sh -c test run is programmed: test_cmd is None (unconfigured).
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-33"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config {
            retry_budget: 3,
            gate: crate::config::GateConfig {
                test_cmd: None, // fail-closed, structurally unpassable
                fallback: crate::config::GateFallback::FullSuite,
            },
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
            None,
            None,
        );
        assert_eq!(o, IterationOutcome::Deadend);
        // ONE agent run, despite retry_budget=3.
        let agent_runs = m
            .recorded()
            .iter()
            .filter(|c| **c == agent_call("true"))
            .count();
        assert_eq!(
            agent_runs, 1,
            "a structurally-unpassable gate must not re-run the agent"
        );
        assert_eq!(led.count_policy_events("retry_budget", 100).unwrap(), 0);
    }

    // A failed agent must NEVER advance the issue — even if a leftover or
    // pre-existing change would pass the suite — and it must surface as an
    // ERROR so the run loop's budget/backoff applies. The old behavior
    // returned Completed, letting a broken --agent-cmd hot-loop the board
    // (and, with a pre-existing untracked file, silently "complete" issues).
    #[test]
    fn failed_agent_never_advances_and_surfaces_an_error() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-32");
        m.expect(&["git", "rev-parse"], 0, "base123\n");
        // Baseline untracked snapshot: TODO.txt existed before the agent ran.
        m.expect(&["git", "ls-files"], 0, "TODO.txt\n");
        m.push(MockResponse::new(&["sh", "-c"], 1, "", "boom")); // agent FAILS
        m.expect(&["git", "diff", "--name-only", "base123"], 0, "");
        // Post-work untracked list is identical → the agent changed NOTHING;
        // the pre-existing scratch file must not read as a change.
        m.expect(&["git", "ls-files"], 0, "TODO.txt\n");
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-32"}"#);

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
            "broken-agent",
            &mut out,
            None,
            None,
        );
        assert!(
            matches!(o, IterationOutcome::Error(_)),
            "failed agent must be an operational error, got {o:?}"
        );
        // No advance, no decision, no receipt.
        let calls = m.recorded();
        assert!(
            !calls.iter().any(|c| c.contains("issue update")),
            "must not advance: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("--json decide")),
            "must not file a decision: {calls:?}"
        );
        let outcome: String = led
            .conn
            .query_row("SELECT outcome FROM iterations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(outcome, "error");
    }

    // SIRF-11: a git failure means the changed-file set is UNKNOWN — doubt,
    // not "nothing changed". The gate must run (fallback policy), and its
    // verdict must rule; the old fold skipped the gate and advanced.
    #[test]
    fn git_failure_fails_closed_not_skipped() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-31");
        m.expect(&["git", "rev-parse"], 0, "base123\n");
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.push(MockResponse::new(
            &["git", "diff"],
            128,
            "",
            "fatal: bad object base123",
        ));
        // Doubt → full suite: make it FAIL to prove the verdict rules.
        m.push(MockResponse::new(&["sh", "-c"], 101, "FAILED", ""));
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-31"}"#);

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
            None,
            None,
        );
        // Gate FAILED (not skipped): deadend, released un-advanced.
        assert_eq!(o, IterationOutcome::Deadend);
        let ndjson = String::from_utf8(out).unwrap();
        assert!(ndjson.contains("\"result\":\"fail\""), "{ndjson}");
        assert!(!ndjson.contains("\"result\":\"skipped\""), "{ndjson}");
    }

    // ---- SIRF-23: the review stage ----------------------------------------

    fn review_cfg(max_rounds: u32) -> Config {
        let mut c = cfg();
        c.review.cmd = Some("reviewer".into());
        c.review.max_rounds = max_rounds;
        c.review.against = crate::config::ReviewAgainst::LaunchBase;
        c
    }

    /// claim→map→lock→brief, a passing WORK gate over a real diff, the review
    /// stage's skip_paths probe, and a decision for the receipt.
    fn program_review_iteration(m: &MockRunner, issue: &str) {
        program_prefix(m, issue);
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n"); // work gate
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n"); // skip probe
        m.expect(
            &["amt", "--json", "decide"],
            0,
            &format!(r#"{{"id":"D-1","resolves":"{issue}"}}"#),
        );
        m.expect(
            &["amt", "--json", "decision", "show"],
            0,
            &format!(r#"{{"id":"D-1","resolves":"{issue}"}}"#),
        );
    }

    /// One checkpoint HEAD per reviewer attempt.
    fn checkpoint_heads(m: &MockRunner, heads: &[&str]) {
        for h in heads {
            m.expect(&["git", "rev-parse", "HEAD"], 0, &format!("{h}\n"));
        }
    }

    const BUG: &str = r#"{"findings":[{"id":"R1-1","kind":"bug","confidence":"confirmed",
        "file":"src/x.rs","line":7,"summary":"off by one","scenario":"n=0 panics","fix":"guard"}]}"#;
    const CLEAN: &str = r#"{"findings":[],"checked":["callers of x"]}"#;

    fn run_fleet(m: &MockRunner, c: &Config) -> (IterationOutcome, Ledger, String) {
        let amt = Amt::new(m);
        let hv = Hayven::new(m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let fleet = test_fleet("base999");
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            c,
            m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        (o, led, String::from_utf8(out).unwrap())
    }

    fn phases(m: &MockRunner) -> Vec<String> {
        m.agent_envs()
            .iter()
            .map(|env| {
                env.iter()
                    .find(|(k, _)| k == "SIRIUS_PHASE")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn env_of(env: &[(String, String)], k: &str) -> Option<String> {
        env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone())
    }

    fn released_with(m: &MockRunner, status: &str) -> bool {
        m.recorded().iter().any(|c| {
            c.starts_with("amt --json release") && c.contains(&format!("--status {status}"))
        })
    }

    #[test]
    fn review_cmd_null_is_todays_loop() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-50");
        let (o, led, nd) = run_fleet(&m, &cfg());
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work"], "one agent run, no reviewer");
        assert!(!nd.contains("\"phase\":\"review\""), "{nd}");
        assert!(
            !m.recorded().iter().any(|c| c.contains("checkpoint")),
            "no checkpoint commit without a review stage"
        );
        assert!(led.review_rounds_for_issue("AMT-50").unwrap().is_empty());
        assert!(released_with(&m, "in_review"));
    }

    #[test]
    fn agent_gets_the_issue_contract_and_templated_cmd() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-51");
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        run_iteration(
            &amt,
            &hv,
            &led,
            &cfg(),
            &m,
            "sirius/oak",
            None,
            "agent --issue {issue} --as {worker}",
            &mut out,
            None,
            Some(&fleet),
        );
        assert!(m
            .recorded()
            .iter()
            .any(|c| *c == agent_call("agent --issue AMT-51 --as sirius/oak")));
        let env = &m.agent_envs()[0];
        assert_eq!(env_of(env, "SIRIUS_ISSUE").as_deref(), Some("AMT-51"));
        assert_eq!(env_of(env, "SIRIUS_WORKER").as_deref(), Some("sirius/oak"));
        // Board writes by the agent are attributed to the worker, not $USER.
        assert_eq!(env_of(env, "AMT_AGENT").as_deref(), Some("sirius/oak"));
        assert_eq!(env_of(env, "SIRIUS_PHASE").as_deref(), Some("work"));
        assert_eq!(env_of(env, "SIRIUS_BASE").as_deref(), Some("base999"));
        assert_eq!(
            env_of(env, "SIRIUS_WORKTREE").as_deref(),
            Some(fleet.worktree.display().to_string().as_str())
        );
    }

    #[test]
    fn clean_review_advances_with_review_in_the_receipt() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-52");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let (o, led, nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work", "review"]);
        assert_eq!(nd.matches("\"phase\":\"review\"").count(), 1, "{nd}");
        assert!(nd.contains("\"result\":\"clean\""), "{nd}");
        // The status moves ONCE, at release — not at gate time.
        assert!(!m
            .recorded()
            .iter()
            .any(|c| c.contains("issue update AMT-52 --status")));
        assert!(released_with(&m, "in_review"));
        let decide = m
            .recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json decide"))
            .unwrap();
        assert!(decide.contains("review: 1 round, 0 bugs fixed"), "{decide}");
        assert!(decide.ends_with("--author sirius/oak"), "{decide}");
        // The reviewer was told what to review, read-only, in a fresh process.
        let env = &m.agent_envs()[1];
        assert_eq!(
            env_of(env, "SIRIUS_DIFF_RANGE").as_deref(),
            Some("base999..HEAD")
        );
        assert_eq!(env_of(env, "SIRIUS_ROUND").as_deref(), Some("1"));
        let prompt = std::fs::read_to_string(env_of(env, "SIRIUS_REVIEW_PROMPT").unwrap()).unwrap();
        assert!(prompt.contains("issue AMT-52") && !prompt.contains("$SIRIUS_ISSUE"));
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("sh -c cd \"$SIRIUS_REVIEW_DIR\" || exit 1; reviewer")));
        let rounds = led.review_rounds_for_issue("AMT-52").unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].result, "clean");
        // A clean review clears any stale flag from an earlier pass.
        assert!(m
            .recorded()
            .iter()
            .any(|c| c == "amt --json issue update AMT-52 --remove-label review:open"));
    }

    // ---- SIRF-26: model selection + the usage-limit pause ------------------

    fn models_cfg() -> crate::models::ModelsConfig {
        crate::models::ModelsConfig {
            default: Some("claude-sonnet-5-5".into()),
            routes: vec![crate::models::ModelRoute {
                labels: vec!["security".into()],
                model: "claude-opus-5-5".into(),
            }],
            fix_floor: Some("claude-opus-5-5".into()),
            review: Some("claude-fable-5-1".into()),
            allow_default: false,
            fallback: None,
        }
    }

    #[test]
    fn worker_and_reviewer_get_their_own_explicit_models() {
        // The Done-when case: workers on sonnet, the reviewer on another model,
        // both visible to the agent as ANTHROPIC_MODEL (+ SIRIUS_* twins).
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-80");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n");
        checkpoint_heads(&m, &["ck1", "ck2"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.models = models_cfg();
        let (o, _led, nd) = run_fleet(&m, &c);
        assert_eq!(o, IterationOutcome::Completed);
        let envs = m.agent_envs();
        let model_of = |i: usize| env_of(&envs[i], "ANTHROPIC_MODEL");
        assert_eq!(phases(&m), vec!["work", "review", "fix", "review"]);
        assert_eq!(model_of(0).as_deref(), Some("claude-sonnet-5-5"), "worker");
        assert_eq!(
            env_of(&envs[0], "SIRIUS_MODEL").as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(model_of(1).as_deref(), Some("claude-fable-5-1"), "reviewer");
        assert_eq!(
            env_of(&envs[1], "SIRIUS_REVIEW_MODEL").as_deref(),
            Some("claude-fable-5-1")
        );
        // An un-routed ticket's FIX round rises to the floor.
        assert_eq!(model_of(2).as_deref(), Some("claude-opus-5-5"), "fix floor");
        // Visible in the claim event.
        assert!(nd.contains("\"model\":\"claude-sonnet-5-5\""), "{nd}");
        assert!(nd.contains("\"review_model\":\"claude-fable-5-1\""), "{nd}");
    }

    #[test]
    fn ticket_labels_route_the_worker_model_and_fill_the_placeholder() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-81");
        // Re-program the claim with a routed label (most-specific prefix wins).
        m.expect(
            &["amt", "--json", "claim", "--agent"],
            0,
            r#"{"id":"AMT-81","title":"T","labels":["Security","bug"]}"#,
        );
        let mut c = cfg();
        c.models = models_cfg();
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            None,
            "agent --model {model} {issue}",
            &mut out,
            None,
            Some(&fleet),
        );
        assert_eq!(
            env_of(&m.agent_envs()[0], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-opus-5-5")
        );
        assert!(m
            .recorded()
            .iter()
            .any(|c| *c == agent_call("agent --model claude-opus-5-5 AMT-81")));
    }

    #[test]
    fn no_models_configured_sets_no_model_env() {
        // With nothing configured (and the launch allowed), Sirius injects
        // nothing — it never invents a model.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-82");
        let (_o, _led, nd) = run_fleet(&m, &cfg());
        assert!(env_of(&m.agent_envs()[0], "ANTHROPIC_MODEL").is_none());
        assert!(nd.contains("\"model\":null"), "{nd}");
    }

    const LIMIT: &str = "You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.\n";

    #[test]
    fn a_usage_limit_pauses_the_fleet_and_leaves_the_issue_untouched() {
        // The Lydgr failure mode: after the limit hit, the loop claimed and
        // bounced tickets in seconds. Now: one hit pauses everything.
        let m = MockRunner::new();
        program_prefix(&m, "AMT-83");
        m.push(MockResponse::new(&["sh", "-c", "true"], 1, "", ""));
        m.on_phase_stdout("work", LIMIT);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &cfg(),
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert!(
            matches!(&o, IterationOutcome::Paused(l) if l.contains("Fable 5 limit")),
            "{o:?}"
        );
        assert!(fleet.paused().is_some(), "the shared pause flag is set");
        let rec = m.recorded();
        let rel = rec
            .iter()
            .find(|c| c.starts_with("amt --json release"))
            .unwrap();
        assert!(
            rel.contains("--status todo") && rel.contains("fleet paused"),
            "{rel}"
        );
        // Not a deadend: the issue never got a real attempt.
        assert!(!rec
            .iter()
            .any(|c| c.contains("hayven remember") && c.contains("deadend")));
        let nd = String::from_utf8(out).unwrap();
        assert!(nd.contains("\"reason\":\"usage_limit\""), "{nd}");
    }

    #[test]
    fn a_fix_round_usage_limit_reverts_escalates_and_pauses() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-86");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        // The fix-mode agent exits 1 at a limit.
        m.push(MockResponse::new(&["sh", "-c", "true"], 0, "", "")); // work ok
        m.push(MockResponse::new(&["sh", "-c", "true"], 1, "", "")); // fix fails
        m.on_phase_stdout("fix", LIMIT);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &review_cfg(3),
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert!(fleet.paused().is_some(), "fleet paused");
        let rec = m.recorded();
        assert!(
            rec.iter().any(|c| c == "git reset --hard ck1"),
            "reverted to the reviewed state"
        );
        assert!(
            rec.iter()
                .any(|c| c.contains("hit a fleet-wide stop during fix round 1")),
            "the escalation names the limit: {rec:?}"
        );
        // The reviewed, gate-passing work is kept (advance-flagged default).
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(
            phases(&m),
            vec!["work", "review", "fix"],
            "no review after the pause"
        );
    }

    #[test]
    fn a_paused_fleet_spawns_nothing_more() {
        // Another worker already hit the limit: this one must not spawn.
        let m = MockRunner::new();
        program_prefix(&m, "AMT-87");
        let fleet = test_fleet("base999");
        fleet.pause_with("You've reached your limit");
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
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert!(matches!(o, IterationOutcome::Paused(_)), "{o:?}");
        assert!(phases(&m).is_empty(), "no agent spawned: {:?}", phases(&m));
        assert!(released_with(&m, "todo"));
    }

    #[test]
    fn review_cmd_model_placeholder_gets_the_review_model() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-88");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.models = models_cfg();
        c.review.cmd = Some("claude -p x --model {model}".into());
        let (_o, _led, _nd) = run_fleet(&m, &c);
        assert!(
            m.recorded()
                .iter()
                .any(|c| c.ends_with("claude -p x --model claude-fable-5-1")),
            "{:?}",
            m.recorded()
        );
    }

    /// The owner's two-tier policy (SIRF-27).
    fn tiered() -> crate::models::ModelsConfig {
        serde_json::from_str(
            r#"{"default":"claude-opus-5-5",
                "routes":[{"labels":["simple"],"model":"claude-sonnet-5-5"}],
                "review":"claude-fable-5-1",
                "fallback":{"default":"claude-sonnet-5-5","review":"claude-opus-5-5"}}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_primary_tier_limit_retries_on_the_fallback_models_without_pausing() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-90");
        m.push(MockResponse::new(&["sh", "-c", "true"], 1, "", "")); // opus: limit
        m.on_phase_stdout("work", LIMIT);
        // (the retry on the fallback tier succeeds — benign default)
        let mut c = cfg();
        c.models = tiered();
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert_eq!(o, IterationOutcome::Completed, "no pause, no bounce");
        assert!(fleet.paused().is_none());
        assert!(fleet.on_fallback());
        let envs = m.agent_envs();
        assert_eq!(
            phases(&m),
            vec!["work", "work"],
            "the SAME phase, retried in place"
        );
        assert_eq!(
            env_of(&envs[0], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            env_of(&envs[1], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-sonnet-5-5")
        );
        let nd = String::from_utf8(out).unwrap();
        assert!(nd.contains("\"phase\":\"fallback\""), "{nd}");
        assert!(released_with(&m, "in_review"));
        // Not released to todo first: retried in place, leases kept.
        assert_eq!(
            m.recorded()
                .iter()
                .filter(|c| c.starts_with("amt --json release"))
                .count(),
            1
        );
    }

    #[test]
    fn a_fable_reviewer_limit_retries_the_review_on_the_fallback_reviewer() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-91");
        checkpoint_heads(&m, &["ck1", "ck1"]);
        // Fable reviewer exits 1 at its limit; the Opus retry reviews clean.
        m.push(MockResponse::new(
            &["sh", "-c", "cd \"$SIRIUS_REVIEW_DIR\" || exit 1; reviewer"],
            1,
            "",
            "",
        ));
        m.on_phase_stdout(
            "review",
            "You've reached your Fable 5.1 limit. Run /usage-credits to continue.\n",
        );
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN); // consumed by the failed try
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.models = tiered();
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert_eq!(o, IterationOutcome::Completed);
        assert!(fleet.paused().is_none() && fleet.on_fallback());
        let envs = m.agent_envs();
        assert_eq!(phases(&m), vec!["work", "review", "review"]);
        assert_eq!(
            env_of(&envs[1], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-fable-5-1")
        );
        assert_eq!(
            env_of(&envs[2], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-opus-5-5")
        );
        // The primary attempt leaves a trace.
        assert_eq!(
            led.review_rounds_for_issue("AMT-91").unwrap()[0].result,
            "fell_back"
        );
        // A REAL review happened: not flagged, not "did not complete".
        assert!(!m
            .recorded()
            .iter()
            .any(|c| c.contains("--add-label review:open")));
        let decide = m
            .recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json decide"))
            .unwrap();
        assert!(decide.contains("review: 1 round"), "{decide}");
    }

    #[test]
    fn a_fix_round_limit_on_the_primary_tier_retries_the_fix_on_the_fallback() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-93");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n"); // fix gate
        checkpoint_heads(&m, &["ck1", "ck2"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        m.push(MockResponse::new(&["sh", "-c", "true"], 0, "", "")); // work ok
        m.push(MockResponse::new(&["sh", "-c", "true"], 1, "", "")); // fix: limit
        m.on_phase_stdout("fix", LIMIT);
        let mut c = review_cfg(3);
        c.models = tiered();
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert_eq!(o, IterationOutcome::Completed);
        assert!(fleet.on_fallback() && fleet.paused().is_none());
        assert_eq!(phases(&m), vec!["work", "review", "fix", "fix", "review"]);
        let envs = m.agent_envs();
        // The fix ran first on the primary floor (opus default), then on the
        // fallback tier (sonnet); the re-review used the fallback reviewer.
        assert_eq!(
            env_of(&envs[2], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            env_of(&envs[3], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(
            env_of(&envs[4], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-opus-5-5")
        );
        let nd = String::from_utf8(out).unwrap();
        assert!(
            nd.contains("\"tier\":\"primary\"") && nd.contains("\"tier\":\"fallback\""),
            "{nd}"
        );
    }

    #[test]
    fn a_logged_out_cli_pauses_straight_away_even_with_a_fallback() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-94");
        m.push(MockResponse::new(&["sh", "-c", "true"], 1, "", ""));
        m.on_phase_stdout("work", "Not logged in · Please run /login\n");
        let mut c = cfg();
        c.models = tiered();
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert!(matches!(o, IterationOutcome::Paused(_)), "{o:?}");
        assert!(
            !fleet.on_fallback(),
            "falling back cannot fix a logged-out CLI"
        );
        assert_eq!(phases(&m), vec!["work"]);
    }

    #[test]
    fn a_limit_on_the_fallback_tier_pauses() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-92");
        m.push(MockResponse::new(&["sh", "-c", "true"], 1, "", ""));
        m.on_phase_stdout("work", LIMIT);
        let mut c = cfg();
        c.models = tiered();
        let fleet = test_fleet("base999");
        *fleet.fallback.lock().unwrap() = Some("earlier".into()); // already switched
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert!(matches!(o, IterationOutcome::Paused(_)), "{o:?}");
        assert_eq!(phases(&m), vec!["work"], "nowhere left to fall back to");
        assert_eq!(
            env_of(&m.agent_envs()[0], "ANTHROPIC_MODEL").as_deref(),
            Some("claude-sonnet-5-5"),
            "ran on the fallback tier"
        );
    }

    #[test]
    fn a_failure_without_limit_text_is_an_ordinary_error() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-84");
        m.push(MockResponse::new(&["sh", "-c", "true"], 1, "", ""));
        m.on_phase_stdout("work", "error: tests failed in the rate limit middleware\n");
        let fleet = test_fleet("base999");
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
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert!(matches!(o, IterationOutcome::Error(_)), "{o:?}");
        assert!(fleet.paused().is_none());
    }

    #[test]
    fn a_reviewer_usage_limit_pauses_without_retrying() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-85");
        checkpoint_heads(&m, &["ck1", "ck1"]);
        // The reviewer exits 1 (as `claude -p` does at a limit); its whole
        // command is ONE argv element.
        m.push(MockResponse::new(
            &["sh", "-c", "cd \"$SIRIUS_REVIEW_DIR\" || exit 1; reviewer"],
            1,
            "",
            "",
        ));
        m.on_phase_stdout("review", LIMIT);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &review_cfg(3),
            &m,
            "sirius/oak",
            None,
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert_eq!(phases(&m), vec!["work", "review"], "no retry into the wall");
        assert!(fleet.paused().is_some());
        // The gate-passing work is kept (advance-flagged), never discarded.
        assert_eq!(o, IterationOutcome::Completed);
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("--add-label review:open")));
    }

    #[test]
    fn reviewer_that_cannot_write_files_answers_in_its_final_message() {
        // Headless reviewers are usually denied file writes (verified with a
        // real `claude -p`). Its printed JSON must count as the review.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-69");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_stdout(
            "review",
            &format!(
                "Permission to write the file was denied, so here it is:\n```json\n{BUG}\n```\n"
            ),
        );
        let mut c = review_cfg(1);
        c.review.on_exhausted = crate::config::ReviewEscalation::Release;
        let (o, led, _nd) = run_fleet(&m, &c);
        let rounds = led.review_rounds_for_issue("AMT-69").unwrap();
        assert_eq!(rounds[0].result, "blocking", "the printed bug was read");
        assert_eq!(o, IterationOutcome::Deadend);
    }

    #[test]
    fn confirmed_bug_runs_fix_regate_and_clean_rereview() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-53");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n"); // fix gate
        checkpoint_heads(&m, &["ck1", "ck2"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        m.on_phase_write(
            "fix",
            "SIRIUS_FIX_OUT",
            r#"{"responses":[{"id":"R1-1","status":"fixed","note":"guarded n=0"}]}"#,
        );
        m.on_phase_write(
            "review",
            "SIRIUS_REVIEW_OUT",
            r#"{"findings":[],"previous":[{"id":"R1-1","verdict":"resolved"}]}"#,
        );
        let (o, led, nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work", "review", "fix", "review"]);
        // The fix round got the findings; the re-review got the answers.
        let envs = m.agent_envs();
        let findings =
            std::fs::read_to_string(env_of(&envs[2], "SIRIUS_REVIEW_FINDINGS").unwrap()).unwrap();
        assert!(
            findings.contains("off by one") && findings.contains("guarded n=0"),
            "{findings}"
        );
        assert_eq!(env_of(&envs[3], "SIRIUS_ROUND").as_deref(), Some("2"));
        // The fix worker also gets what to look at (spec env table).
        assert_eq!(
            env_of(&envs[2], "SIRIUS_DIFF_RANGE").as_deref(),
            Some("base999..HEAD")
        );
        assert!(env_of(&envs[2], "SIRIUS_REVIEW_DIR").is_some());
        assert!(env_of(&envs[2], "SIRIUS_FIX_OUT").is_some());
        // Re-gated after the fix.
        assert!(nd.contains("\"phase\":\"fix\""), "{nd}");
        assert_eq!(nd.matches("\"phase\":\"gate\"").count(), 2, "{nd}");
        let rounds = led.review_rounds_for_issue("AMT-53").unwrap();
        let results: Vec<&str> = rounds.iter().map(|r| r.result.as_str()).collect();
        assert_eq!(results, vec!["blocking", "clean"]);
        let decide = m
            .recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json decide"))
            .unwrap();
        assert!(
            decide.contains("review: 2 rounds, 1 bug fixed, 0 rebuttals accepted"),
            "{decide}"
        );
        // One comment per round, plus the fix responses, as the worker.
        let comments: Vec<String> = m
            .recorded()
            .into_iter()
            .filter(|c| c.contains("issue comment AMT-53"))
            .collect();
        assert!(comments
            .iter()
            .any(|c| c.contains("Review round 1: 1 confirmed")));
        assert!(comments
            .iter()
            .any(|c| c.contains("Fix round 1") && c.contains("guarded n=0")));
        assert!(
            comments.iter().all(|c| c.ends_with("--author sirius/oak")),
            "{comments:?}"
        );
        assert!(released_with(&m, "in_review"));
    }

    #[test]
    fn uncertain_and_minor_findings_only_advance_as_notes() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-54");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write(
            "review",
            "SIRIUS_REVIEW_OUT",
            r#"{"findings":[{"kind":"bug","confidence":"uncertain","summary":"maybe racy"},
                            {"kind":"minor","confidence":"confirmed","summary":"typo"}]}"#,
        );
        let (o, _led, nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work", "review"], "no fix round for notes");
        assert!(
            nd.contains("\"confirmed\":0") && nd.contains("\"notes\":2"),
            "{nd}"
        );
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("Notes (non-blocking)") && c.contains("maybe racy")));
        assert!(released_with(&m, "in_review"));
    }

    #[test]
    fn accepted_rebuttal_closes_the_finding() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-55");
        checkpoint_heads(&m, &["ck1", "ck2"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        m.on_phase_write(
            "fix",
            "SIRIUS_FIX_OUT",
            r#"{"responses":[{"id":"R1-1","status":"rebutted","note":"n is never 0: guarded by caller"}]}"#,
        );
        m.on_phase_write(
            "review",
            "SIRIUS_REVIEW_OUT",
            r#"{"findings":[],"previous":[{"id":"R1-1","verdict":"accepted","note":"right"}]}"#,
        );
        let (o, _led, _nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        let decide = m
            .recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json decide"))
            .unwrap();
        assert!(
            decide.contains("0 bugs fixed, 1 rebuttal accepted"),
            "{decide}"
        );
        assert!(released_with(&m, "in_review"));
    }

    #[test]
    fn exhausted_rounds_advance_flagged_with_label_and_comment() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-56");
        checkpoint_heads(&m, &["ck1", "ck2"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG); // not fixed
        let (o, _led, _nd) = run_fleet(&m, &review_cfg(2));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work", "review", "fix", "review"]);
        let rec = m.recorded();
        assert!(rec
            .iter()
            .any(|c| c == "amt --json issue update AMT-56 --add-label review:open"));
        assert!(rec
            .iter()
            .any(|c| c.contains("2 review round(s) used up") && c.contains("[R1-1] src/x.rs:7")));
        assert!(
            released_with(&m, "in_review"),
            "advance-flagged still advances"
        );
        let decide = rec
            .iter()
            .find(|c| c.starts_with("amt --json decide"))
            .unwrap();
        assert!(decide.contains("1 still open (flagged)"), "{decide}");
    }

    #[test]
    fn exhausted_rounds_release_variant_goes_back_to_todo() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-57");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        let mut c = review_cfg(1);
        c.review.on_exhausted = crate::config::ReviewEscalation::Release;
        let (o, led, _nd) = run_fleet(&m, &c);
        assert_eq!(o, IterationOutcome::Deadend);
        assert_eq!(
            phases(&m),
            vec!["work", "review"],
            "max_rounds 1 ⇒ no fix round"
        );
        let rec = m.recorded();
        assert!(released_with(&m, "todo"));
        assert!(rec.iter().any(|c| c.starts_with("amt --json release")
            && c.contains("released without advancing — review:")));
        assert!(
            !rec.iter().any(|c| c.starts_with("amt --json decide")),
            "no receipt"
        );
        assert!(!rec.iter().any(|c| c.contains("--add-label")));
        let outcome: String = led
            .conn
            .query_row("SELECT outcome FROM iterations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(outcome, "deadend");
    }

    #[test]
    fn tampering_reviewer_is_discarded_and_counts_as_an_error() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-58");
        checkpoint_heads(&m, &["ck1", "ck1"]);
        // Attempt 1: the tree changes under the reviewer. Attempt 2: clean.
        m.expect(&["git", "status", "--porcelain=v1"], 0, "");
        m.expect(&["git", "status", "--porcelain=v1"], 0, " M src/x.rs\n");
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let (o, led, nd) = run_fleet(&m, &review_cfg(3));
        assert!(
            m.recorded().iter().any(|c| c == "git reset --hard ck1"),
            "changes discarded"
        );
        assert!(nd.contains("\"result\":\"tampered\""), "{nd}");
        let results: Vec<String> = led
            .review_rounds_for_issue("AMT-58")
            .unwrap()
            .into_iter()
            .map(|r| r.result)
            .collect();
        assert_eq!(
            results,
            vec!["tampered", "clean"],
            "retried once, then clean"
        );
        assert_eq!(o, IterationOutcome::Completed);
    }

    #[test]
    fn reviewer_never_runs_in_the_workers_tree() {
        // SIRF-29: even reviewing against the launch base (no merge tree),
        // the reviewer gets a throwaway tree at the checkpoint — its hooks'
        // cwd-relative writes must never land in the worker's tree.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-80");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let (o, _led, _nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        let env = &m.agent_envs()[1];
        let dir = env_of(env, "SIRIUS_REVIEW_DIR").unwrap();
        let worktree = env_of(env, "SIRIUS_WORKTREE").unwrap();
        assert_ne!(dir, worktree, "the reviewer's cwd is not the worker tree");
        assert!(dir.ends_with("sirius_oak-review"), "{dir}");
        let rec = m.recorded();
        assert!(
            rec.iter()
                .any(|c| c == &format!("git worktree add --detach {dir} ck1")),
            "{rec:?}"
        );
        assert!(
            rec.iter()
                .any(|c| c == &format!("git worktree remove --force {dir}")),
            "the throwaway tree is removed"
        );
    }

    #[test]
    fn fingerprint_ignores_suite_telemetry_but_not_source() {
        // SIRF-29, against real git: a hook appending to a TRACKED
        // `.suite/` file (Lydgr commits it) is not tampering; an edit to
        // source, or a new stray file, still is.
        let dir = std::env::temp_dir().join(format!("sirius-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".suite/events")).unwrap();
        std::fs::write(dir.join("src/x.rs"), "fn x() {}\n").unwrap();
        std::fs::write(dir.join(".suite/events/a.jsonl"), "{}\n").unwrap();
        let r = crate::shell::RealRunner {
            cwd: Some(dir.clone()),
        };
        for args in [
            &["init", "-q"][..],
            &["config", "core.autocrlf", "false"][..],
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ][..],
        ] {
            crate::gitrange::run_git(&r, args).unwrap();
        }
        let before = tree_fingerprint(&r);
        std::fs::write(dir.join(".suite/events/a.jsonl"), "{}\n{\"hook\":1}\n").unwrap();
        std::fs::create_dir_all(dir.join(".hayven")).unwrap();
        std::fs::write(dir.join(".hayven/cache"), "x").unwrap();
        assert_eq!(
            tree_fingerprint(&r),
            before,
            "suite telemetry is not tampering"
        );
        std::fs::write(dir.join("stray.txt"), "x").unwrap();
        assert_ne!(tree_fingerprint(&r), before, "a new file is");
        std::fs::remove_file(dir.join("stray.txt")).unwrap();
        std::fs::write(dir.join("src/x.rs"), "fn y() {}\n").unwrap();
        assert_ne!(tree_fingerprint(&r), before, "a source edit is");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- SIRF-30/31: the frontier and sequence collisions -----------------

    fn run_fleet_at(
        m: &MockRunner,
        c: &Config,
        fleet: &Fleet,
    ) -> (IterationOutcome, Ledger, String) {
        let amt = Amt::new(m);
        let hv = Hayven::new(m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            c,
            m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(fleet),
        );
        (o, led, String::from_utf8(out).unwrap())
    }

    /// One in-flight sibling, AMT-7, awaiting integration and touching src/x.rs.
    fn program_sibling(m: &MockRunner) {
        m.expect(&["git", "for-each-ref"], 0, "sirius/amt-7\tsib7\n");
        m.expect(
            &["amt", "--json", "issue", "list", "--status", "in_review"],
            0,
            r#"[{"id":"AMT-7","status":"in_review","title":"Other work"}]"#,
        );
        m.expect(
            &["git", "diff", "--name-only", "base999...sib7"],
            0,
            "src/x.rs\n",
        );
    }

    #[test]
    fn frontier_review_merges_siblings_and_names_them_in_the_prompt() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-81");
        checkpoint_heads(&m, &["ck1"]);
        program_sibling(&m);
        let fleet = test_fleet("base999");
        let t = fleet.sirius_dir.join("worktrees").join("sirius_oak-review");
        let t = t.to_string_lossy().to_string();
        m.expect(&["git", "-C", &t, "rev-parse", "HEAD"], 0, "front1\n");
        m.expect(
            &["git", "diff", "--name-only", "base999..ck1"],
            0,
            "src/x.rs\n",
        );
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.review.against = crate::config::ReviewAgainst::Frontier;
        let (o, _led, _nd) = run_fleet_at(&m, &c, &fleet);
        assert_eq!(o, IterationOutcome::Completed);
        let env = &m.agent_envs()[1];
        assert_eq!(env_of(env, "SIRIUS_FRONTIER").as_deref(), Some("front1"));
        assert_eq!(
            env_of(env, "SIRIUS_DIFF_RANGE").as_deref(),
            Some("front1..HEAD")
        );
        assert_eq!(
            env_of(env, "SIRIUS_SIBLING_BRANCHES").as_deref(),
            Some("sirius/amt-7")
        );
        assert_eq!(
            env_of(env, "SIRIUS_REVIEW_DIR").as_deref(),
            Some(t.as_str())
        );
        let prompt = std::fs::read_to_string(env_of(env, "SIRIUS_REVIEW_PROMPT").unwrap()).unwrap();
        assert!(
            prompt.contains("AMT-7 \"Other work\" (sirius/amt-7)"),
            "{prompt}"
        );
        assert!(prompt.contains("OVERLAPS this diff: src/x.rs"), "{prompt}");
        let rec = m.recorded();
        let merged = |rev: &str| {
            rec.iter().any(|c| {
                c.starts_with(&format!("git -C {t} ")) && c.contains(" merge ") && c.ends_with(rev)
            })
        };
        assert!(
            merged("sib7"),
            "the sibling is merged into the frontier: {rec:?}"
        );
        assert!(merged("ck1"), "then the work");
        assert!(rec
            .iter()
            .any(|c| c == &format!("git worktree remove --force {t}")));
    }

    #[test]
    fn a_custom_prompt_without_the_placeholder_still_learns_the_siblings() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-82");
        checkpoint_heads(&m, &["ck1"]);
        program_sibling(&m);
        let mut fleet = test_fleet("base999");
        fleet.review_prompt = "Review $SIRIUS_ISSUE.".into();
        let t = fleet.sirius_dir.join("worktrees").join("sirius_oak-review");
        m.expect(
            &["git", "-C", &t.to_string_lossy(), "rev-parse", "HEAD"],
            0,
            "front1\n",
        );
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.review.against = crate::config::ReviewAgainst::Frontier;
        run_fleet_at(&m, &c, &fleet);
        let env = &m.agent_envs()[1];
        let prompt = std::fs::read_to_string(env_of(env, "SIRIUS_REVIEW_PROMPT").unwrap()).unwrap();
        assert!(prompt.starts_with("Review AMT-82."), "{prompt}");
        assert!(
            prompt.contains("Other in-flight changes") && prompt.contains("AMT-7"),
            "{prompt}"
        );
    }

    #[test]
    fn a_braced_siblings_placeholder_is_not_appended_twice() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-89");
        checkpoint_heads(&m, &["ck1"]);
        program_sibling(&m);
        let mut fleet = test_fleet("base999");
        fleet.review_prompt = "In flight:\n${SIRIUS_SIBLINGS}".into();
        let t = fleet.sirius_dir.join("worktrees").join("sirius_oak-review");
        m.expect(
            &["git", "-C", &t.to_string_lossy(), "rev-parse", "HEAD"],
            0,
            "front1\n",
        );
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.review.against = crate::config::ReviewAgainst::Frontier;
        run_fleet_at(&m, &c, &fleet);
        let env = &m.agent_envs()[1];
        let prompt = std::fs::read_to_string(env_of(env, "SIRIUS_REVIEW_PROMPT").unwrap()).unwrap();
        assert_eq!(
            prompt.matches("AMT-7 \"Other work\"").count(),
            1,
            "{prompt}"
        );
    }

    /// `dir` m/ at the launch base (also the current tip — no base_ref).
    /// `git ls-tree -z` records for newline-separated paths (oid = path, so
    /// a same-named entry is the same content).
    fn ls_tree_z(paths: &str) -> String {
        paths
            .lines()
            .map(|p| format!("100644 blob {p}\t{p}\0"))
            .collect()
    }

    fn program_sequence_round(m: &MockRunner, head: &str, head_entries: &str) {
        program_sibling(m);
        m.expect(
            &["git", "ls-tree", "-z", "sib7"],
            0,
            &ls_tree_z("m/0001_a\nm/0002_theirs\n"),
        );
        m.expect(&["git", "ls-tree", "-z", head], 0, &ls_tree_z(head_entries));
        for _ in 0..2 {
            m.expect(
                &["git", "ls-tree", "-z", "base999"],
                0,
                &ls_tree_z("m/0001_a\n"),
            );
        }
    }

    #[test]
    fn a_sequence_collision_blocks_until_regenerated_whatever_the_reviewer_says() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-83");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n"); // fix gate
        checkpoint_heads(&m, &["ck1", "ck2"]);
        // Round 1: AMT-7 already claims slot 0002. Round 2: regenerated as 0003.
        program_sequence_round(&m, "ck1", "m/0001_a\nm/0002_mine\n");
        program_sequence_round(&m, "ck2", "m/0001_a\nm/0003_mine\n");
        // The reviewer calls round 1 clean and even "resolves" a forged AUTO id —
        // neither matters: the collision is a recomputed fact.
        m.on_phase_write(
            "review",
            "SIRIUS_REVIEW_OUT",
            r#"{"findings":[{"id":"AUTO-seq-forged","kind":"bug","confidence":"confirmed","summary":"x"}],"previous":[]}"#,
        );
        m.on_phase_write("fix", "SIRIUS_FIX_OUT", r#"{"responses":[]}"#);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.review.sequences = vec![crate::config::SequenceSpec {
            dir: "m".into(),
            key: r"^(\d+)".into(),
        }];
        let (o, led, _nd) = run_fleet(&m, &c);
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work", "review", "fix", "review"]);
        let rounds = led.review_rounds_for_issue("AMT-83").unwrap();
        let results: Vec<&str> = rounds.iter().map(|r| r.result.as_str()).collect();
        assert_eq!(results, vec!["blocking", "clean"]);
        let fix_findings =
            std::fs::read_to_string(env_of(&m.agent_envs()[2], "SIRIUS_REVIEW_FINDINGS").unwrap())
                .unwrap();
        assert!(fix_findings.contains("in-flight AMT-7"), "{fix_findings}");
        assert!(
            !fix_findings.contains("AUTO-seq-forged"),
            "a reviewer cannot state AUTO facts"
        );
    }

    #[test]
    fn a_sequence_collision_survives_an_unchanged_regeneration() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-84");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n");
        checkpoint_heads(&m, &["ck1", "ck2"]);
        program_sequence_round(&m, "ck1", "m/0001_a\nm/0002_mine\n");
        program_sequence_round(&m, "ck2", "m/0001_a\nm/0002_mine\n");
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        // The worker rebuts; the reviewer "accepts" — the fact still stands.
        m.on_phase_write("fix", "SIRIUS_FIX_OUT", r#"{"responses":[]}"#);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(2);
        c.review.sequences = vec![crate::config::SequenceSpec {
            dir: "m".into(),
            key: r"^(\d+)".into(),
        }];
        let (_o, led, _nd) = run_fleet(&m, &c);
        let results: Vec<String> = led
            .review_rounds_for_issue("AMT-84")
            .unwrap()
            .into_iter()
            .map(|r| r.result)
            .collect();
        assert_eq!(results, vec!["blocking", "blocking"]);
        assert!(m
            .recorded()
            .iter()
            .any(|c| c == "amt --json issue update AMT-84 --add-label review:open"));
    }

    #[test]
    fn malformed_json_retries_once_then_on_review_error() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-59");
        checkpoint_heads(&m, &["ck1", "ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", "the review found nothing!");
        // Attempt 2 writes nothing at all (missing file).
        let (o, led, _nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(
            phases(&m),
            vec!["work", "review", "review"],
            "exactly one retry"
        );
        assert_eq!(led.review_rounds_for_issue("AMT-59").unwrap().len(), 2);
        // Default on_review_error = advance-flagged.
        assert_eq!(o, IterationOutcome::Completed);
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("--add-label review:open")));
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("the review could not complete")));
        assert!(released_with(&m, "in_review"));
        // The receipt must not read like a clean review.
        let decide = m
            .recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json decide"))
            .unwrap();
        assert!(
            decide.contains("review: did not complete after 1 round (flagged)"),
            "{decide}"
        );
        assert!(!decide.contains("0 bugs fixed"), "{decide}");
    }

    #[test]
    fn review_error_release_variant() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-60");
        checkpoint_heads(&m, &["ck1", "ck1"]);
        let mut c = review_cfg(3);
        c.review.on_review_error = crate::config::ReviewEscalation::Release;
        let (o, _led, _nd) = run_fleet(&m, &c);
        assert_eq!(o, IterationOutcome::Deadend);
        assert!(released_with(&m, "todo"));
    }

    #[test]
    fn fix_that_breaks_the_gate_goes_back_to_fixing_within_budget() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-61");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n"); // fix gate 1
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n"); // fix gate 2
        m.expect(&["sh", "-c", "run-suite"], 0, "ok"); // work gate
        m.push(MockResponse::new(
            &["sh", "-c", "run-suite"],
            1,
            "FAILED",
            "",
        )); // fix breaks it
        m.expect(&["sh", "-c", "run-suite"], 0, "ok"); // fix retry repairs it
        checkpoint_heads(&m, &["ck1", "ck2"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let (o, _led, _nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work", "review", "fix", "fix", "review"]);
        assert!(released_with(&m, "in_review"));
    }

    #[test]
    fn fix_that_cannot_repair_the_gate_reverts_and_escalates() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-62");
        for _ in 0..3 {
            m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n");
        }
        m.expect(&["sh", "-c", "run-suite"], 0, "ok"); // work gate passes
        for _ in 0..3 {
            m.push(MockResponse::new(
                &["sh", "-c", "run-suite"],
                1,
                "FAILED",
                "",
            ));
        }
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        let (o, _led, _nd) = run_fleet(&m, &review_cfg(3));
        // The gate-passing checkpoint is restored, never the broken fix.
        assert!(m.recorded().iter().any(|c| c == "git reset --hard ck1"));
        assert!(m.recorded().iter().any(|c| c.contains("broke the gate")));
        assert_eq!(
            o,
            IterationOutcome::Completed,
            "advance-flagged keeps the passing work"
        );
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("--add-label review:open")));
    }

    #[test]
    fn fix_round_timeout_reverts_and_escalates_instead_of_abandoning_work() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-66");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", BUG);
        m.arm_phase_timeout("fix");
        let (o, _led, _nd) = run_fleet(&m, &review_cfg(3));
        let rec = m.recorded();
        // The reviewed, gate-passing checkpoint is restored and kept...
        assert!(rec.iter().any(|c| c == "git reset --hard ck1"), "{rec:?}");
        assert!(rec.iter().any(|c| c.contains("timed out in round 1")));
        // ...and it advances flagged (on_review_error) — NOT released to todo
        // with a deadend, which would let the next reset wipe it.
        assert_eq!(o, IterationOutcome::Completed);
        assert!(released_with(&m, "in_review"));
        assert!(rec.iter().any(|c| c.contains("--add-label review:open")));
        assert!(!rec.iter().any(|c| c.contains("agent timed out:")));
    }

    #[test]
    fn lease_lost_before_the_reviewer_aborts_without_releasing_the_issue() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-67");
        checkpoint_heads(&m, &["ck1"]);
        // Pre-spawn (work) renewal OK; the pre-review renewal is REFUSED.
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"id":"AMT-67"}"#,
        );
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"claimed":false,"reason":"held by sirius/rowan"}"#,
        );
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let (o, _led, nd) = run_fleet(&m, &review_cfg(3));
        match o {
            IterationOutcome::Error(e) => assert!(e.contains("before the review"), "{e}"),
            other => panic!("expected a lease-lost error, got {other:?}"),
        }
        assert_eq!(phases(&m), vec!["work"], "the reviewer never spawned");
        let rec = m.recorded();
        assert!(
            !rec.iter().any(|c| c.starts_with("amt --json release")),
            "not ours to release"
        );
        assert!(
            !rec.iter().any(|c| c.starts_with("amt --json decide")),
            "no receipt"
        );
        assert!(nd.contains("\"reason\":\"lease_lost\""), "{nd}");
    }

    #[test]
    fn nothing_changed_means_nothing_to_review() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-68");
        // Both the gate and the skip probe see an empty diff.
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-1","resolves":"AMT-68"}"#,
        );
        let (o, _led, nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work"], "no reviewer over an empty diff");
        assert!(nd.contains("\"result\":\"skipped\""), "{nd}");
    }

    #[test]
    fn a_base_conflict_blocks_even_when_block_on_omits_conflict() {
        // Review F1: no reviewer ran — the conflict must never read as clean.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-85");
        checkpoint_heads(&m, &["ck1"]);
        // SIRF-42: the iteration base, resolved at claim (main had not moved yet).
        m.expect(
            &["git", "rev-parse", "--verify", "--quiet", "main^{commit}"],
            0,
            "base999\n",
        );
        m.expect(&["git", "rev-parse", "--verify"], 0, "cur777\n");
        m.push(MockResponse::new(
            &["git", "-C"],
            1,
            "",
            "CONFLICT (content)",
        ));
        m.expect(&["git", "-C"], 0, "src/x.rs\n");
        let mut c = review_cfg(1);
        c.review.against = crate::config::ReviewAgainst::CurrentBaseMerge;
        c.review.base_ref = Some("main".into());
        c.review.block_on = vec!["bug".into()];
        let (_o, led, _nd) = run_fleet(&m, &c);
        let rounds = led.review_rounds_for_issue("AMT-85").unwrap();
        assert_eq!(rounds[0].result, "blocking");
        assert!(m
            .recorded()
            .iter()
            .any(|c| c == "amt --json issue update AMT-85 --add-label review:open"));
    }

    #[test]
    fn a_fleet_peer_under_review_first_owns_the_sequence_slot() {
        // Review F3: rowan reached review first with its own 0002; it has no
        // branch yet, so only the fleet's in-flight registry can tell oak.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-86");
        checkpoint_heads(&m, &["ck1"]);
        m.expect(
            &["git", "ls-tree", "-z", "peer1"],
            0,
            &ls_tree_z("m/0001_a\nm/0002_rowan\n"),
        );
        m.expect(
            &["git", "ls-tree", "-z", "ck1"],
            0,
            &ls_tree_z("m/0001_a\nm/0002_oak\n"),
        );
        for _ in 0..2 {
            m.expect(
                &["git", "ls-tree", "-z", "base999"],
                0,
                &ls_tree_z("m/0001_a\n"),
            );
        }
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let fleet = test_fleet("base999");
        fleet.register_inflight("sirius/rowan", "AMT-70", "peer1");
        let mut c = review_cfg(1);
        c.review.sequences = vec![crate::config::SequenceSpec {
            dir: "m".into(),
            key: r"^(\d+)".into(),
        }];
        let (_o, led, _nd) = run_fleet_at(&m, &c, &fleet);
        let rounds = led.review_rounds_for_issue("AMT-86").unwrap();
        assert_eq!(rounds[0].result, "blocking", "{rounds:?}");
        assert!(
            rounds[0].findings.contains("in-flight AMT-70"),
            "{}",
            rounds[0].findings
        );
        // rowan, registered FIRST, does not see oak (first come owns the slot).
        assert!(fleet.peers_before("sirius/rowan").is_empty());
        // oak's iteration is over: its registration is gone (drop guard),
        // and only rowan — still under review — remains.
        assert!(fleet.peers_before("sirius/oak").is_empty());
        fleet.register_inflight("sirius/elm", "AMT-71", "elm1");
        assert_eq!(
            fleet.peers_before("sirius/elm"),
            vec![("AMT-70".to_string(), "peer1".to_string())]
        );
    }

    #[test]
    fn an_incomplete_recheck_keeps_the_previous_fact_open() {
        // Review F4: round 2 cannot read the sibling (amt fails) — the
        // collision must not count as "fixed".
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-87");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n");
        checkpoint_heads(&m, &["ck1", "ck2"]);
        program_sequence_round(&m, "ck1", "m/0001_a\nm/0002_mine\n");
        // Round 2: the sibling's status cannot be read.
        m.expect(&["git", "for-each-ref"], 0, "sirius/amt-7\tsib7\n");
        m.expect(&["amt", "--json", "issue", "list"], 1, "database is locked");
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        m.on_phase_write("fix", "SIRIUS_FIX_OUT", r#"{"responses":[]}"#);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(2);
        c.review.sequences = vec![crate::config::SequenceSpec {
            dir: "m".into(),
            key: r"^(\d+)".into(),
        }];
        let (_o, led, _nd) = run_fleet(&m, &c);
        let results: Vec<String> = led
            .review_rounds_for_issue("AMT-87")
            .unwrap()
            .into_iter()
            .map(|r| r.result)
            .collect();
        assert_eq!(results, vec!["blocking", "blocking"]);
    }

    #[test]
    fn a_red_integration_holds_gated_work_without_review() {
        // SIRF-32 review F1: no review, no receipt, no advance — preserved on
        // its branch and released to todo naming the red run.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-88");
        let mut c = review_cfg(3);
        c.integration.on_fail = crate::config::IntegrationOnFail::Block;
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        led.set_meta(
            crate::integrate::RED_KEY,
            Some(r#"{"at":"t","frontier":"f1","issue":"AMT-90"}"#),
        )
        .unwrap();
        let fleet = test_fleet("base999");
        let mut out = Vec::new();
        run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        assert_eq!(phases(&m), vec!["work"], "no reviewer while red");
        let rec = m.recorded();
        assert!(
            !rec.iter().any(|c| c.starts_with("amt --json decide")),
            "no receipt"
        );
        assert!(
            rec.iter().any(|c| c == "git branch -f sirius/amt-88 HEAD"),
            "work preserved: {rec:?}"
        );
        let release = rec
            .iter()
            .find(|c| c.starts_with("amt --json release"))
            .unwrap();
        assert!(
            release.contains("--status todo") && release.contains("integration red at f1 (AMT-90)"),
            "{release}"
        );
        assert!(
            rec.iter()
                .any(|c| c == "git update-ref refs/sirius/held/amt-88 HEAD"),
            "the held commit is kept and marked for the re-claim: {rec:?}"
        );
        assert!(
            release.contains("resumed when the issue is claimed again"),
            "{release}"
        );
    }

    /// A held commit for `issue` (lowercase ref) that is NOT on the base yet.
    fn program_held(m: &MockRunner, key: &str) {
        m.expect(
            &[
                "git",
                "show-ref",
                "--verify",
                "--hash",
                &format!("refs/sirius/held/{key}"),
            ],
            0,
            "heldsha\n",
        );
        m.expect(&["git", "merge-base", "--is-ancestor", "heldsha"], 1, "");
    }

    fn iterate(m: &MockRunner, c: &Config) -> Vec<String> {
        let (amt, hv, fleet) = (Amt::new(m), Hayven::new(m), test_fleet("base999"));
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        run_iteration(
            &amt,
            &hv,
            &led,
            c,
            m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        m.recorded()
    }

    #[test]
    fn held_work_is_resumed_on_reclaim_and_released_once_stamped() {
        // SIRF-32 review N4: not redone from scratch, and not clobbered.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-91");
        program_held(&m, "amt-91");
        let rec = iterate(&m, &cfg());
        let merge = rec.iter().position(|c| c == "git merge --no-edit heldsha");
        let agent = rec.iter().position(|c| c.starts_with(&shell_prefix()));
        assert!(
            merge.is_some() && merge < agent,
            "held work merged before the agent runs: {rec:?}"
        );
        assert_eq!(
            env_of(&m.agent_envs()[0], "SIRIUS_RESUMED_FROM").as_deref(),
            Some("heldsha")
        );
        assert!(
            rec.iter()
                .any(|c| c == "git update-ref -d refs/sirius/held/amt-91"),
            "stamped work contains the held commit: the marker is done"
        );
    }

    #[test]
    fn a_resumed_iteration_that_ends_early_keeps_the_held_marker() {
        // Verification V1: the marker is consumed only by a successful stamp.
        let m = MockRunner::new();
        program_prefix(&m, "AMT-95");
        program_held(&m, "amt-95");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/x.rs\n");
        m.expect(&["sh", "-c", "run-suite"], 1, "test result: FAILED");
        let mut c = cfg();
        c.retry_budget = 1;
        let rec = iterate(&m, &c);
        assert!(
            rec.iter().any(|c| c == "git merge --no-edit heldsha"),
            "{rec:?}"
        );
        assert!(
            !rec.iter()
                .any(|c| c.starts_with("git update-ref -d refs/sirius/held/")),
            "a failed gate must not drop the held work: {rec:?}"
        );
    }

    #[test]
    fn held_again_parks_the_older_held_work_it_does_not_contain() {
        // Branch review X8: a resume that failed without conflict, then a
        // second hold — the first held commit must not become unreachable.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-93");
        m.expect(
            &[
                "git",
                "show-ref",
                "--verify",
                "--hash",
                "refs/sirius/held/amt-93",
            ],
            1,
            "",
        ); // nothing to resume at claim time…
        m.expect(
            &[
                "git",
                "show-ref",
                "--verify",
                "--hash",
                "refs/sirius/held/amt-93",
            ],
            0,
            "oldheld000001\n",
        ); // …but one is there when holding again
        m.expect(
            &["git", "merge-base", "--is-ancestor", "oldheld000001"],
            1,
            "",
        );
        let mut c = review_cfg(3);
        c.integration.on_fail = crate::config::IntegrationOnFail::Block;
        let led = Ledger::open_in_memory().unwrap();
        led.set_meta(
            crate::integrate::RED_KEY,
            Some(r#"{"at":"t","frontier":"f1","issue":"AMT-90"}"#),
        )
        .unwrap();
        let (amt, hv, fleet) = (Amt::new(&m), Hayven::new(&m), test_fleet("base999"));
        let mut out = Vec::new();
        run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        let rec = m.recorded();
        assert!(
            rec.iter().any(|c| c
                == "git update-ref refs/sirius/held-superseded/amt-93/oldheld00000 oldheld000001"),
            "{rec:?}"
        );
        assert!(rec
            .iter()
            .any(|c| c == "git update-ref refs/sirius/held/amt-93 HEAD"));
    }

    #[test]
    fn held_work_already_on_the_base_is_dropped_not_merged() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-92");
        m.expect(
            &[
                "git",
                "show-ref",
                "--verify",
                "--hash",
                "refs/sirius/held/amt-92",
            ],
            0,
            "heldsha\n",
        );
        let rec = iterate(&m, &cfg());
        assert!(!rec.iter().any(|c| c == "git merge --no-edit heldsha"));
        assert!(rec
            .iter()
            .any(|c| c == "git update-ref -d refs/sirius/held/amt-92"));
        assert_eq!(env_of(&m.agent_envs()[0], "SIRIUS_RESUMED_FROM"), None);
    }

    #[test]
    fn sequence_checks_cover_siblings_past_the_merge_cap() {
        // Verification N3/V9: 13 in-flight siblings; the 13th (newest, NOT
        // merged into the review tree) holds the colliding slot.
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-96");
        checkpoint_heads(&m, &["ck1"]);
        let refs: String = (1..=13)
            .map(|n| format!("sirius/amt-{n}\tsib{n}\n"))
            .collect();
        let list: Vec<String> = (1..=13)
            .map(|n| format!(r#"{{"id":"AMT-{n}","title":"t{n}"}}"#))
            .collect();
        m.expect(&["git", "for-each-ref"], 0, &refs);
        m.expect(
            &["amt", "--json", "issue", "list", "--status", "in_review"],
            0,
            &format!("[{}]", list.join(",")),
        );
        m.expect(
            &["git", "ls-tree", "-z", "sib13"],
            0,
            &ls_tree_z("m/0001_a\nm/0002_theirs\n"),
        );
        m.expect(
            &["git", "ls-tree", "-z", "ck1"],
            0,
            &ls_tree_z("m/0001_a\nm/0002_mine\n"),
        );
        for _ in 0..2 {
            m.expect(
                &["git", "ls-tree", "-z", "base999"],
                0,
                &ls_tree_z("m/0001_a\n"),
            );
        }
        let fleet = test_fleet("base999");
        let t = fleet.sirius_dir.join("worktrees").join("sirius_oak-review");
        m.expect(
            &["git", "-C", &t.to_string_lossy(), "rev-parse", "HEAD"],
            0,
            "front1\n",
        );
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(1);
        c.review.against = crate::config::ReviewAgainst::Frontier;
        c.review.sequences = vec![crate::config::SequenceSpec {
            dir: "m".into(),
            key: r"^(\d+)".into(),
        }];
        let (_o, led, _nd) = run_fleet_at(&m, &c, &fleet);
        let rounds = led.review_rounds_for_issue("AMT-96").unwrap();
        assert_eq!(rounds[0].result, "blocking");
        assert!(
            rounds[0].findings.contains("in-flight AMT-13"),
            "{}",
            rounds[0].findings
        );
        assert!(
            !m.recorded()
                .iter()
                .any(|c| c.contains(" merge ") && c.ends_with("sib13")),
            "13th not merged"
        );
        let prompt =
            std::fs::read_to_string(env_of(&m.agent_envs()[1], "SIRIUS_REVIEW_PROMPT").unwrap())
                .unwrap();
        assert!(
            prompt.contains("1 newer in-flight issue(s) not merged"),
            "{prompt}"
        );
    }

    #[test]
    fn escapes_reach_the_next_review_prompt_until_automated() {
        // SIRF-35 Done-when: two `migration-fork` escapes appear in the next
        // rendered review prompt; marking the kind automated removes them.
        let led = Ledger::open_in_memory().unwrap();
        led.insert_escape(
            "LYD-13",
            "migration-fork",
            "snapshot chain forked",
            None,
            None,
        )
        .unwrap();
        led.insert_escape("LYD-52", "migration-fork", "two 0042s", None, None)
            .unwrap();
        let review_prompt = |led: &Ledger| {
            let m = MockRunner::new();
            program_review_iteration(&m, "AMT-97");
            checkpoint_heads(&m, &["ck1"]);
            m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
            let (amt, hv, fleet) = (Amt::new(&m), Hayven::new(&m), test_fleet("base999"));
            let mut out = Vec::new();
            run_iteration(
                &amt,
                &hv,
                led,
                &review_cfg(3),
                &m,
                "sirius/oak",
                Some("todo"),
                "true",
                &mut out,
                None,
                Some(&fleet),
            );
            let env = &m.agent_envs()[1];
            std::fs::read_to_string(env_of(env, "SIRIUS_REVIEW_PROMPT").unwrap()).unwrap()
        };
        let p = review_prompt(&led);
        assert!(p.contains("- migration-fork (2×"), "{p}");
        assert!(p.contains("two 0042s"), "the latest summary: {p}");
        led.set_kind_automated("migration-fork", "tests/migrations.rs")
            .unwrap();
        let p = review_prompt(&led);
        assert!(!p.contains("migration-fork"), "{p}");
    }

    /// Turns integration red WHILE the reviewer runs (what a concurrent
    /// `sirius integrate` does): records the red state in the ledger file and
    /// pauses the fleet the way a sibling worker's pre-claim check would.
    struct RedDuringReview<'a> {
        inner: &'a MockRunner,
        ledger_path: std::path::PathBuf,
        pause: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    impl Runner for RedDuringReview<'_> {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<crate::shell::CmdOutput> {
            self.inner.run(program, args)
        }
        fn run_agent(
            &self,
            program: &str,
            args: &[&str],
            opts: &AgentRunOpts,
            hb: &mut dyn FnMut(),
        ) -> std::io::Result<crate::shell::AgentOutcome> {
            if opts
                .env
                .iter()
                .any(|(k, v)| k == "SIRIUS_PHASE" && v == "review")
            {
                let l = Ledger::open(&self.ledger_path).unwrap();
                l.set_meta(
                    crate::integrate::RED_KEY,
                    Some(r#"{"at":"t","frontier":"f9","issue":"AMT-90"}"#),
                )
                .unwrap();
                *self.pause.lock().unwrap() =
                    Some("integration red at f9 (AMT-90) — fix it".into());
            }
            self.inner.run_agent(program, args, opts, hb)
        }
    }

    fn red_mid_review(issue: &str, reviewer_answers: bool) -> (Vec<String>, IterationOutcome) {
        let m = MockRunner::new();
        program_review_iteration(&m, issue);
        checkpoint_heads(&m, &["ck1", "ck1"]);
        if reviewer_answers {
            m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        }
        let fleet = test_fleet("base999");
        let path = fleet.sirius_dir.join("red.db");
        let led = Ledger::create(&path, "test").unwrap();
        let r = RedDuringReview {
            inner: &m,
            ledger_path: path,
            pause: fleet.pause.clone(),
        };
        let mut c = review_cfg(3);
        c.integration.on_fail = crate::config::IntegrationOnFail::Block;
        let (amt, hv) = (Amt::new(&m), Hayven::new(&m));
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &c,
            &r,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&fleet),
        );
        (m.recorded(), o)
    }

    fn assert_held(rec: &[String], issue: &str) {
        let key = issue.to_lowercase();
        let release = rec
            .iter()
            .find(|c| c.starts_with("amt --json release"))
            .unwrap();
        assert!(
            release.contains("--status todo"),
            "held, not advanced: {release}"
        );
        assert!(release.contains("integration red at f9"), "{release}");
        assert!(
            !rec.iter().any(|c| c.contains("--add-label review:open")),
            "never flagged-advanced"
        );
        assert!(
            !rec.iter().any(|c| c.starts_with("amt --json decide")),
            "no receipt"
        );
        assert!(rec
            .iter()
            .any(|c| c == &format!("git update-ref refs/sirius/held/{key} HEAD")));
    }

    #[test]
    fn red_during_a_review_that_cannot_finish_holds_instead_of_escalating() {
        // Branch review X1: the pause made the reviewer error out, and
        // on_review_error (advance-flagged) advanced UNREVIEWED work.
        let (rec, _) = red_mid_review("AMT-98", false);
        assert_held(&rec, "AMT-98");
    }

    #[test]
    fn red_recorded_while_a_review_finishes_clean_still_holds() {
        let (rec, _) = red_mid_review("AMT-99", true);
        assert_held(&rec, "AMT-99");
    }

    #[test]
    fn current_base_merge_conflict_is_a_blocking_finding_and_cleans_up() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-63");
        checkpoint_heads(&m, &["ck1", "ck2"]);
        // SIRF-42: the iteration base, resolved at claim (main had not moved yet).
        m.expect(
            &["git", "rev-parse", "--verify", "--quiet", "main^{commit}"],
            0,
            "base999\n",
        );
        m.expect(&["git", "rev-parse", "--verify"], 0, "cur777\n"); // round 1
        m.expect(&["git", "rev-parse", "--verify"], 0, "cur777\n"); // round 2
        m.push(MockResponse::new(
            &["git", "-C"],
            1,
            "",
            "CONFLICT (content)",
        )); // merge
        m.expect(&["git", "-C"], 0, "src/x.rs\n"); // diff --diff-filter=U
                                                   // Round 2: the merge succeeds (benign), the reviewer reviews it clean.
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let mut c = review_cfg(3);
        c.review.against = crate::config::ReviewAgainst::CurrentBaseMerge;
        c.review.base_ref = Some("main".into());
        let (o, led, _nd) = run_fleet(&m, &c);
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(
            phases(&m),
            vec!["work", "fix", "review"],
            "no reviewer on a conflict round"
        );
        let envs = m.agent_envs();
        let findings =
            std::fs::read_to_string(env_of(&envs[1], "SIRIUS_REVIEW_FINDINGS").unwrap()).unwrap();
        assert!(
            findings.contains("\"kind\":\"conflict\"") && findings.contains("git merge cur777"),
            "{findings}"
        );
        // Round 2 reviews the throwaway merge tree, against the CURRENT base.
        let review_dir = env_of(&envs[2], "SIRIUS_REVIEW_DIR").unwrap();
        assert!(review_dir.ends_with("sirius_oak-review"), "{review_dir}");
        assert_eq!(
            env_of(&envs[2], "SIRIUS_DIFF_RANGE").as_deref(),
            Some("cur777..HEAD")
        );
        // The temporary worktree is removed after EVERY round.
        let removes = m
            .recorded()
            .iter()
            .filter(|c| {
                c.starts_with("git worktree remove --force") && c.ends_with("sirius_oak-review")
            })
            .count();
        assert!(
            removes >= 4,
            "pre-clean + post-remove in both rounds, got {removes}"
        );
        let rounds = led.review_rounds_for_issue("AMT-63").unwrap();
        assert_eq!(rounds[0].result, "blocking");
        assert_eq!(rounds[1].result, "clean");
    }

    #[test]
    fn doc_only_diff_skips_review() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-64");
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "README.md\n");
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "docs/guide.md\n",
        );
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-1","resolves":"AMT-64"}"#,
        );
        let (o, _led, nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work"], "no reviewer for docs");
        assert!(nd.contains("\"result\":\"skipped\""), "{nd}");
    }

    #[test]
    fn failed_agent_release_comment_names_the_agent_not_the_gate() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-65");
        m.push(MockResponse::new(
            &["sh", "-c", "true"],
            1,
            "",
            "Not logged in",
        ));
        let (o, _led, _nd) = run_fleet(&m, &cfg());
        assert!(matches!(o, IterationOutcome::Error(_)));
        let rel = m
            .recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json release"))
            .unwrap();
        assert!(rel.contains("agent exited 1"), "{rel}");
        assert!(!rel.contains("gate did not pass"), "{rel}");
    }

    // ---- SIRF-41 (progress-aware timeout) / SIRF-52 (helper agents) ---------

    fn release_comment(m: &MockRunner) -> String {
        m.recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json release"))
            .expect("released")
    }

    // SIRF-52: sirius owns the timeout, so the CLI's own 600s background-wait
    // ceiling is lifted for the worker AND the reviewer.
    #[test]
    fn work_and_review_agents_get_an_unlimited_bg_wait_ceiling() {
        let m = MockRunner::new();
        program_review_iteration(&m, "AMT-140");
        checkpoint_heads(&m, &["ck1"]);
        m.on_phase_write("review", "SIRIUS_REVIEW_OUT", CLEAN);
        let (o, _led, _nd) = run_fleet(&m, &review_cfg(3));
        assert_eq!(o, IterationOutcome::Completed);
        assert_eq!(phases(&m), vec!["work", "review"]);
        for env in m.agent_envs() {
            assert_eq!(
                env_of(&env, "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS").as_deref(),
                Some("0")
            );
        }
    }

    // SIRF-41 #3: a ticket's labels pick its limits (first matching route,
    // per-field), and they reach the supervisor with the worktree probe.
    #[test]
    fn timeouts_route_by_label_and_reach_the_supervisor() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-141");
        m.expect(
            &["amt", "--json", "claim", "--agent"],
            0,
            r#"{"id":"AMT-141","title":"T","labels":["UI"]}"#,
        );
        let mut c = cfg();
        c.timeouts = serde_json::from_str(
            r#"{"idle_secs":600,"hard_secs":7200,"routes":[{"labels":["ui"],"hard_secs":14400}]}"#,
        )
        .unwrap();
        m.arm_probe_calls(1);
        let _ = run_fleet(&m, &c);
        assert_eq!(
            m.watches()[0],
            (
                Duration::from_secs(14_400),
                Duration::from_secs(600),
                Duration::from_secs(60)
            )
        );
        // The probe run.rs hands the supervisor asks git about the worktree,
        // without ever taking the index lock.
        let rec = m.recorded();
        assert!(
            rec.iter().any(|c| c == "git rev-parse --show-toplevel"),
            "{rec:?}"
        );
        assert!(
            rec.iter()
                .any(|c| c.starts_with("git --no-optional-locks") && c.contains("status")),
            "{rec:?}"
        );
    }

    // SIRF-41 #3: the release comment and NDJSON say WHICH limit fired.
    #[test]
    fn an_idle_kill_and_a_hard_kill_are_told_apart() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-142");
        m.arm_agent_idle_timeout(0);
        let (o, _led, nd) = run_fleet(&m, &cfg());
        assert_eq!(o, IterationOutcome::Deadend);
        let rel = release_comment(&m);
        assert!(
            rel.contains("agent idle for 1800s — no output, no file changes"),
            "{rel}"
        );
        assert!(nd.contains("\"timeout_kind\":\"idle\""), "{nd}");
        assert!(nd.contains("\"timed_out\":true"), "{nd}");

        let m = MockRunner::new();
        program_prefix(&m, "AMT-143");
        m.arm_agent_timeout(0);
        let (o, _led, nd) = run_fleet(&m, &cfg());
        assert_eq!(o, IterationOutcome::Deadend);
        let rel = release_comment(&m);
        assert!(rel.contains("hit the hard cap of 10800s"), "{rel}");
        assert!(nd.contains("\"timeout_kind\":\"hard\""), "{nd}");
    }

    // SIRF-52: the MSX-60 shape — exit 0, helpers killed, deliverable missing.
    // That is an INCOMPLETE run: never advanced, and it says why.
    #[test]
    fn helpers_killed_by_the_cli_make_an_exit_0_run_incomplete() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-144");
        m.on_phase_stdout(
            "work",
            "converted 3 of 6\nBackground tasks still running after 600s; terminating. \
             Set CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0 to wait indefinitely.\n",
        );
        let (o, _led, nd) = run_fleet(&m, &cfg());
        assert!(matches!(o, IterationOutcome::Error(_)), "{o:?}");
        assert!(released_with(&m, "todo"));
        assert!(nd.contains("\"reason\":\"agent_bg_tasks_killed\""), "{nd}");
        assert!(nd.contains("\"agent_ok\":false"), "{nd}");
        let rel = release_comment(&m);
        assert!(rel.contains("exited 0"), "{rel}");
        assert!(rel.contains("agent_bg_tasks_killed"), "{rel}");
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
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"doubt","tests":[]}"#,
        );
        m.push(MockResponse::new(
            &["sh", "-c"],
            101,
            "test result: FAILED. 1 failed",
            "",
        ));
        // Attempt 2: work ok, gate runs the suite and PASSES.
        m.expect(&["sh", "-c"], 0, ""); // agent (2nd attempt)
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"doubt","tests":[]}"#,
        );
        m.expect(&["sh", "-c"], 0, "test result: ok");
        // advance + receipt + release
        m.expect(
            &["amt", "--json", "issue", "update"],
            0,
            r#"{"id":"AMT-20"}"#,
        );
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-1","resolves":"AMT-20"}"#,
        );
        m.expect(
            &["amt", "--json", "decision", "show"],
            0,
            r#"{"id":"D-1","resolves":"AMT-20"}"#,
        );
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-20"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config {
            retry_budget: 2,
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
            None,
            None,
        );
        assert_eq!(o, IterationOutcome::Completed);

        // The agent ran twice (two attempts).
        let agent_runs = m
            .recorded()
            .iter()
            .filter(|c| **c == agent_call("true"))
            .count();
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
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("\"retrying\":true"));
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
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"doubt","tests":[]}"#,
        );
        m.push(MockResponse::new(
            &["sh", "-c"],
            101,
            "test result: FAILED. 1 failed",
            "",
        ));
        // Attempt 2: fail again.
        m.expect(&["sh", "-c"], 0, ""); // agent (2nd)
        m.expect(&["git", "diff"], 0, "src/run.rs\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["run"],"note":"doubt","tests":[]}"#,
        );
        m.push(MockResponse::new(
            &["sh", "-c"],
            101,
            "test result: FAILED. 1 failed",
            "",
        ));
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-21"}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config {
            retry_budget: 2,
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
            None,
            None,
        );
        assert_eq!(o, IterationOutcome::Deadend);

        let calls = m.recorded();
        // Two agent runs (attempt 1 + one retry).
        let agent_runs = calls.iter().filter(|c| **c == agent_call("true")).count();
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
        let c = Config {
            retry_budget: 3,
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
            "sleep 999",
            &mut out,
            None,
            None,
        );
        assert_eq!(o, IterationOutcome::Deadend);
        // Exactly one agent run — the timeout did NOT loop back.
        let agent_runs = m
            .recorded()
            .iter()
            .filter(|c| **c == agent_call("sleep 999"))
            .count();
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
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"id":"AMT-23","title":"T"}"#,
        );
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        // Lock: e1 → oracle conflict (exit 3), then forced (exit 0 with id).
        m.push(MockResponse::new(&["hayven", "claim"], 3, "", "adjacency"));
        m.expect(&["hayven", "claim"], 0, r#"{"id":"c1"}"#);
        m.expect(&["hayven", "context"], 0, r#"{"pack":true}"#);
        m.expect(&["hayven", "recall"], 0, r#"{"notes":[]}"#);
        // work ok, no changes → gate skipped → completes.
        m.expect(&["sh", "-c"], 0, "");
        m.expect(&["git", "diff"], 0, "");
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-1","resolves":"AMT-23"}"#,
        );
        m.expect(
            &["amt", "--json", "decision", "show"],
            0,
            r#"{"id":"D-1","resolves":"AMT-23"}"#,
        );
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem"}"#);
        m.expect(&["hayven", "release"], 0, "ok");
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-23"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config {
            oracle_202: Oracle202::ForceWithBudget,
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
            None,
            None,
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
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"id":"AMT-24","title":"T"}"#,
        );
        m.expect(&["hayven", "query"], 0, r#"{"hits":[{"id":"e1"}]}"#);
        // Lock: e1 → oracle conflict (exit 3), policy is BackOff → back off.
        m.push(MockResponse::new(&["hayven", "claim"], 3, "", "adjacency"));
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-24"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let c = Config {
            oracle_202: Oracle202::BackOff,
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
            None,
            None,
        );
        assert_eq!(o, IterationOutcome::ReleasedOverlap);
        let verdicts: String = led
            .conn
            .query_row("SELECT oracle_verdicts FROM iterations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(verdicts, r#"["backoff"]"#);
    }

    // ---- SIRF-50 / SIRF-48: env-fault re-gate and the failure tail -------

    const SETUP: &str = "bun install --frozen-lockfile";
    /// The MainSpanX output: the suite's own tests pass, a dependency is gone.
    const MISSING_DEP: &str =
        "error: Cannot find module \"react\" from \"/wt/src/ui.tsx\"\n 270 pass\n 3 fail";

    fn setup_cfg(retry_budget: u32, setup: &str) -> Config {
        Config {
            retry_budget,
            worktree: crate::config::WorktreeConfig {
                setup_cmd: Some(setup.into()),
            },
            ..cfg()
        }
    }

    /// Fleet worktree prep (reset → clean → detached checkout).
    fn program_fleet_prep(m: &MockRunner) {
        m.expect(&["git", "reset"], 0, "");
        m.expect(&["git", "clean"], 0, "");
        m.expect(&["git", "checkout"], 0, "");
    }

    fn gate_events(out: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(out)
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["phase"] == "gate")
            .collect()
    }

    fn setup_runs(m: &MockRunner) -> usize {
        let want = format!("{}{SETUP}", shell_prefix());
        m.recorded().iter().filter(|c| **c == want).count()
    }

    fn agent_runs(m: &MockRunner) -> usize {
        m.recorded()
            .iter()
            .filter(|c| **c == agent_call("true"))
            .count()
    }

    // SIRF-50: a gate that failed on a MISSING DEPENDENCY re-runs worktree
    // setup and gates again without consuming a work attempt — with
    // retry_budget 1, a consumed attempt would have been a deadend.
    #[test]
    fn env_fault_regate_reruns_setup_without_consuming_an_attempt() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-60");
        program_fleet_prep(&m);
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "src/ui.tsx\n",
        );
        m.push(MockResponse::new(&["sh", "-c"], 1, "", MISSING_DEP)); // gate
        m.expect(&["sh", "-c", SETUP], 0, "installed 312 packages");
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "src/ui.tsx\n",
        );
        m.expect(&["sh", "-c"], 0, "302 pass"); // re-gate
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-60"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &setup_cfg(1, SETUP),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&test_fleet("base999")),
        );
        assert_eq!(o, IterationOutcome::Completed, "{:?}", m.recorded());
        assert_eq!(agent_runs(&m), 1, "no fresh work attempt");
        assert_eq!(setup_runs(&m), 1);
        assert_eq!(led.count_policy_events("retry_budget", 100).unwrap(), 0);
        let gates = gate_events(&out);
        assert_eq!(gates.len(), 1, "{gates:?}");
        assert_eq!(gates[0]["result"], "pass");
        assert_eq!(gates[0]["env_fault"], true);
        assert_eq!(gates[0]["setup_rerun"], true);
        assert_eq!(gates[0]["attempt"], 1);
        assert!(gates[0]["error_tail"].is_null(), "a pass carries no tail");
        // A recovered env fault files no failure comment.
        assert!(
            !m.recorded().iter().any(|c| c.contains("gate failed")),
            "{:?}",
            m.recorded()
        );
    }

    // The re-gate happens ONCE per WORK⇄GATE pass: still failing is an
    // ordinary failure (an attempt IS used), and the next attempt's env fault
    // does not re-run setup again. The next attempt is told why it failed.
    #[test]
    fn env_fault_regate_happens_once_then_counts_as_an_ordinary_failure() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-61");
        program_fleet_prep(&m);
        // Attempt 1: env fault → setup → re-gate, still missing.
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "src/ui.tsx\n",
        );
        m.push(MockResponse::new(&["sh", "-c"], 1, "", MISSING_DEP));
        m.expect(&["sh", "-c", SETUP], 0, "");
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "src/ui.tsx\n",
        );
        m.push(MockResponse::new(&["sh", "-c"], 1, "", MISSING_DEP));
        // Attempt 2: the same fault — no second setup run.
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "src/ui.tsx\n",
        );
        m.push(MockResponse::new(&["sh", "-c"], 1, "", MISSING_DEP));
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-61"}"#);

        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &setup_cfg(2, SETUP),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&test_fleet("base999")),
        );
        assert_eq!(o, IterationOutcome::Deadend);
        assert_eq!(setup_runs(&m), 1, "setup re-runs at most once");
        assert_eq!(agent_runs(&m), 2, "the failed re-gate used attempt 1");
        let gates = gate_events(&out);
        assert_eq!(gates.len(), 2, "{gates:?}");
        assert_eq!(
            (&gates[0]["env_fault"], &gates[0]["setup_rerun"]),
            (&json!(true), &json!(true))
        );
        assert_eq!(
            (&gates[1]["env_fault"], &gates[1]["setup_rerun"]),
            (&json!(true), &json!(false))
        );
        assert!(gates[0]["error_tail"]
            .as_str()
            .unwrap()
            .contains("Cannot find module"));
        // The failure comment says what was tried.
        let comments: Vec<String> = m
            .recorded()
            .into_iter()
            .filter(|c| c.starts_with("amt --json issue comment") && c.contains("gate failed"))
            .collect();
        assert_eq!(comments.len(), 2, "{comments:?}");
        assert!(
            comments[0].contains("re-ran worktree setup") && comments[0].contains("still failing"),
            "{}",
            comments[0]
        );
        assert!(
            comments[1].contains("already re-run once"),
            "{}",
            comments[1]
        );
        // Attempt 2's agent was handed attempt 1's failing tail.
        let envs = m.agent_envs();
        assert_eq!(env_of(&envs[0], "SIRIUS_LAST_GATE_TAIL"), None);
        let tail = env_of(&envs[1], "SIRIUS_LAST_GATE_TAIL").expect("tail handed over");
        assert!(tail.contains("Cannot find module \"react\""), "{tail}");
    }

    // Setup + a second full gate can outlast the lease, so the lease is
    // renewed first — and a REFUSED renewal aborts before setup runs.
    #[test]
    fn env_fault_regate_renews_the_lease_and_aborts_when_it_is_lost() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-64");
        program_fleet_prep(&m);
        // Pre-spawn heartbeat: ours. Re-gate renewal: refused.
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"claimed":true}"#,
        );
        m.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"claimed":false,"reason":"lease held by sirius/rowan"}"#,
        );
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "src/ui.tsx\n",
        );
        m.push(MockResponse::new(&["sh", "-c"], 1, "", MISSING_DEP));
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &setup_cfg(1, SETUP),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&test_fleet("base999")),
        );
        match o {
            IterationOutcome::Error(e) => assert!(e.contains("lease"), "{e}"),
            other => panic!("a lost lease must abort, got {other:?}"),
        }
        assert_eq!(
            agent_runs(&m),
            1,
            "the abort came AFTER the work, at the re-gate"
        );
        assert_eq!(setup_runs(&m), 0, "setup never ran on a lost lease");
    }

    // No setup command (disabled) ⇒ an env fault is flagged but nothing is
    // re-run; it is an ordinary failure.
    #[test]
    fn env_fault_without_a_setup_cmd_is_an_ordinary_failure() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-62");
        program_fleet_prep(&m);
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(
            &["git", "diff", "--name-only", "base999"],
            0,
            "src/ui.tsx\n",
        );
        m.push(MockResponse::new(&["sh", "-c"], 1, "", MISSING_DEP));
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-62"}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &setup_cfg(1, ""),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&test_fleet("base999")),
        );
        assert_eq!(o, IterationOutcome::Deadend);
        let gates = gate_events(&out);
        assert_eq!(gates[0]["env_fault"], true);
        assert_eq!(gates[0]["setup_rerun"], false);
        assert!(m
            .recorded()
            .iter()
            .any(|c| c.contains("no worktree.setup_cmd is configured")));
    }

    // SIRF-48: a failing gate's comment leads with the OUTPUT TAIL (not the
    // first kept line, not the selection note), the selection note reads as
    // the routine note it is, and the gate event carries `error_tail`.
    #[test]
    fn gate_failure_comment_and_event_carry_the_output_tail() {
        let m = MockRunner::new();
        program_prefix(&m, "AMT-63");
        program_fleet_prep(&m);
        m.expect(&["sh", "-c"], 0, ""); // agent
        m.expect(&["git", "diff", "--name-only", "base999"], 0, "src/db.rs\n");
        let mut output: String = (1..=24).map(|i| format!("progress line {i}\n")).collect();
        output.push_str("FAILED db::trigger_fires — relation \"audit\" does not exist\n");
        m.push(MockResponse::new(&["sh", "-c"], 101, &output, ""));
        m.expect(&["amt", "--json", "release"], 0, r#"{"id":"AMT-63"}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let mut out = Vec::new();
        let o = run_iteration(
            &amt,
            &hv,
            &led,
            &setup_cfg(1, ""),
            &m,
            "sirius/oak",
            Some("todo"),
            "true",
            &mut out,
            None,
            Some(&test_fleet("base999")),
        );
        assert_eq!(o, IterationOutcome::Deadend);
        let comment = m
            .recorded()
            .into_iter()
            .find(|c| c.starts_with("amt --json issue comment") && c.contains("gate failed"))
            .expect("a failure comment");
        assert!(comment.contains("```text"), "{comment}");
        assert!(comment.contains("FAILED db::trigger_fires"), "{comment}");
        assert!(comment.contains("[full-suite, tests_failed]"), "{comment}");
        // The last 15 lines only (14 progress lines + the failure): line 11
        // is in, line 10 is out.
        assert!(comment.contains("progress line 11\n"), "{comment}");
        assert!(!comment.contains("progress line 10\n"), "{comment}");
        // The selection note is demoted and reworded.
        assert!(
            comment.contains("Plan: selection skipped in an isolated worktree"),
            "{comment}"
        );
        assert!(!comment.contains("not visible to the code-graph daemon"));
        let gates = gate_events(&out);
        assert_eq!(gates[0]["reason_code"], "tests_failed");
        assert_eq!(gates[0]["env_fault"], false);
        assert_eq!(gates[0]["setup_rerun"], false);
        let tail = gates[0]["error_tail"].as_str().unwrap();
        assert!(tail.ends_with("does not exist"), "{tail}");
        assert_eq!(tail.lines().count(), crate::gate::ERROR_TAIL_LINES);
    }

    // ---- SIRF-41 / SIRF-42 / SIRF-49 against REAL git ---------------------

    /// One git call in `at` with a fixed identity, panicking with context.
    fn git_at(at: &std::path::Path, args: &[&str]) -> String {
        let r = crate::shell::RealRunner {
            cwd: Some(at.to_path_buf()),
        };
        let mut full = vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ];
        full.extend(args);
        crate::gitrange::run_git(&r, &full)
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
            .stdout
            .trim()
            .to_string()
    }

    /// Write `name` in `at` and commit it there; returns the new HEAD.
    fn commit_file(at: &std::path::Path, name: &str) -> String {
        std::fs::write(at.join(name), name).unwrap();
        git_at(at, &["add", "-A"]);
        git_at(at, &["commit", "-q", "-m", name]);
        git_at(at, &["rev-parse", "HEAD"])
    }

    /// The files in commit `sha`'s tree.
    fn tree_files(at: &std::path::Path, sha: &str) -> Vec<String> {
        let mut v: Vec<String> = git_at(at, &["ls-tree", "-r", "--name-only", sha])
            .lines()
            .map(String::from)
            .collect();
        v.sort();
        v
    }

    type AgentAction = Box<dyn Fn(&std::path::Path) + Send>;

    /// A fleet worker over a REAL repo: git runs for real in the worker's
    /// worktree; amt, hayven and the gate's test command go to the mock; an
    /// agent spawn first plays its scripted action in the worktree, then the
    /// mock decides how it ends (exit code / timeout). Every call is logged
    /// in order, git included.
    struct GitWorld {
        dir: std::path::PathBuf,
        repo: std::path::PathBuf,
        wt: std::path::PathBuf,
        base: String,
        fleet: Fleet,
        mock: MockRunner,
        git: crate::shell::RealRunner,
        log: std::sync::Mutex<Vec<String>>,
        agents: std::sync::Mutex<std::collections::VecDeque<AgentAction>>,
    }

    impl crate::shell::Runner for GitWorld {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<crate::shell::CmdOutput> {
            self.log
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            if program == "git" {
                self.git.run(program, args)
            } else {
                self.mock.run(program, args)
            }
        }

        fn run_agent(
            &self,
            program: &str,
            args: &[&str],
            opts: &AgentRunOpts,
            heartbeat: &mut dyn FnMut(),
        ) -> std::io::Result<AgentOutcome> {
            self.log
                .lock()
                .unwrap()
                .push(format!("AGENT {program} {}", args.join(" ")));
            let act = self.agents.lock().unwrap().pop_front();
            if let Some(act) = act {
                act(&self.wt);
            }
            self.mock.run_agent(program, args, opts, heartbeat)
        }
    }

    impl GitWorld {
        /// A repo with `base.txt` on `main`, and a worker worktree detached at
        /// it — what `sirius run` builds at launch.
        fn new(tag: &str) -> GitWorld {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("sirius-wip-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let repo = dir.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            git_at(&repo, &["init", "-q", "-b", "main"]);
            // Repo-local identity: the worker's own commits (completion,
            // resume merges) must not depend on the machine's git config.
            git_at(&repo, &["config", "user.name", "t"]);
            git_at(&repo, &["config", "user.email", "t@t"]);
            git_at(&repo, &["config", "commit.gpgsign", "false"]);
            let base = commit_file(&repo, "base.txt");
            let wt = dir.join("wt");
            let wt_str = crate::gitrange::git_path(&wt);
            git_at(
                &repo,
                &["worktree", "add", "-q", "--detach", &wt_str, &base],
            );
            let fleet = Fleet {
                base_ref: Some("main".into()),
                worktree: wt.clone(),
                ..test_fleet(&base)
            };
            GitWorld {
                git: crate::shell::RealRunner {
                    cwd: Some(wt.clone()),
                },
                dir,
                repo,
                wt,
                base,
                fleet,
                mock: MockRunner::new(),
                log: Default::default(),
                agents: Default::default(),
            }
        }

        fn agent(&self, f: impl Fn(&std::path::Path) + Send + 'static) {
            self.agents.lock().unwrap().push_back(Box::new(f));
        }

        /// One fleet iteration claiming `issue`; returns the outcome and the
        /// NDJSON events.
        fn iterate(&self, issue: &str, c: &Config) -> (IterationOutcome, Vec<Value>) {
            self.mock.expect(
                &["amt", "--json", "claim", "--agent"],
                0,
                &format!(r#"{{"id":"{issue}","title":"T"}}"#),
            );
            let (amt, hv) = (Amt::new(self), Hayven::new(self));
            let led = Ledger::open_in_memory().unwrap();
            let mut out = Vec::new();
            let o = run_iteration(
                &amt,
                &hv,
                &led,
                c,
                self,
                "sirius/oak",
                Some("todo"),
                "true",
                &mut out,
                None,
                Some(&self.fleet),
            );
            let evs = String::from_utf8(out)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            (o, evs)
        }

        fn calls(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn ref_at(&self, r: &str) -> Option<String> {
            ref_sha(&self.git, r)
        }
    }

    impl Drop for GitWorld {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            let _ = std::fs::remove_dir_all(&self.fleet.sirius_dir);
        }
    }

    fn event<'v>(evs: &'v [Value], phase: &str) -> &'v Value {
        evs.iter()
            .rev()
            .find(|e| e["phase"] == phase)
            .unwrap_or_else(|| panic!("no {phase} event in {evs:?}"))
    }

    fn link_files(w: &GitWorld, base: &str, resumed: &str, issue: &str) -> Vec<String> {
        let hm = MockRunner::new();
        hm.expect(&["hayven", "affected-tests"], 0, r#"{"roots":[]}"#);
        let got = crate::gitrange::changed_symbols(
            &w.git,
            &Hayven::new(&hm),
            None,
            Some(crate::gitrange::FleetBase {
                base,
                resumed_from: Some(resumed),
                issue: Some(issue),
                base_ref: Some("main"),
            }),
        )
        .unwrap();
        let mut files = got.files;
        files.sort();
        files
    }

    /// SIRF-41 "done when" + SIRF-42, end to end: a killed agent's commit
    /// (and its uncommitted file) end up at refs/sirius/wip/<issue> BEFORE
    /// the release; the re-claim builds on the base ref's CURRENT tip, merges
    /// the preserved work back, and `sirius link --changed`'s resolution of
    /// the completed line includes the resumed work — never main's commit.
    #[test]
    fn timed_out_work_is_preserved_then_resumed_on_the_fresh_base_and_stamped() {
        let w = GitWorld::new("timeout");
        let base = w.base.clone();
        // Iteration 1: the agent commits a.txt, leaves b.txt uncommitted, and
        // is killed by the timeout.
        w.agent(|wt| {
            commit_file(wt, "a.txt");
            std::fs::write(wt.join("b.txt"), "b").unwrap();
        });
        w.mock.arm_agent_timeout(0);
        let (o, evs) = w.iterate("AMT-7", &cfg());
        assert_eq!(o, IterationOutcome::Deadend);
        assert_eq!(event(&evs, "claim")["base_sha"], json!(base));
        let wip = w
            .ref_at("refs/sirius/wip/amt-7")
            .expect("the killed agent's work is pinned");
        assert_eq!(
            tree_files(&w.repo, &wip),
            vec!["a.txt", "b.txt", "base.txt"]
        );
        let rel = event(&evs, "release");
        assert_eq!(rel["reason"], "agent_timeout");
        assert_eq!(rel["wip_ref"]["ref"], "refs/sirius/wip/amt-7");
        assert_eq!(rel["wip_ref"]["sha"], json!(wip));
        assert!(
            rel["wip_ref"]["diffstat"]
                .as_str()
                .unwrap()
                .contains("2 files changed"),
            "{rel}"
        );
        let calls = w.calls();
        let pinned = calls
            .iter()
            .position(|c| c.starts_with("git update-ref refs/sirius/wip/amt-7"))
            .expect("pinned");
        let released = calls
            .iter()
            .position(|c| c.starts_with("amt --json release AMT-7"))
            .expect("released");
        assert!(pinned < released, "preserved BEFORE the release: {calls:?}");
        assert!(
            calls[released].contains("work preserved at refs/sirius/wip/amt-7"),
            "{}",
            calls[released]
        );

        // A predecessor lands on main meanwhile.
        let main = commit_file(&w.repo, "landed.txt");
        assert_ne!(main, base);

        // Iteration 2: re-claim — fresh base, resumed work, agent finishes.
        w.agent(|wt| {
            assert!(wt.join("a.txt").exists() && wt.join("b.txt").exists());
            assert!(wt.join("landed.txt").exists(), "built on the fresh main");
            commit_file(wt, "c.txt");
        });
        w.mock
            .expect(&["sh", "-c", "run-suite"], 0, "test result: ok");
        let (o, evs) = w.iterate("AMT-7", &cfg());
        assert_eq!(o, IterationOutcome::Completed, "{evs:?}");
        assert_eq!(event(&evs, "claim")["base_sha"], json!(main));
        let env = &w.mock.agent_envs()[1];
        assert_eq!(env_of(env, "SIRIUS_BASE"), Some(main.clone()));
        assert_eq!(env_of(env, "SIRIUS_BASE_REF").as_deref(), Some("main"));
        assert_eq!(env_of(env, "SIRIUS_RESUMED_FROM"), Some(wip.clone()));
        assert_eq!(
            env_of(env, "SIRIUS_RESUME_REF").as_deref(),
            Some("refs/sirius/wip/amt-7")
        );
        let stamped = w.ref_at("refs/heads/sirius/amt-7").expect("stamped");
        assert_eq!(
            tree_files(&w.repo, &stamped),
            vec!["a.txt", "b.txt", "base.txt", "c.txt", "landed.txt"]
        );
        assert_eq!(
            w.ref_at("refs/sirius/wip/amt-7"),
            None,
            "retired once stamped"
        );
        // What `sirius link --changed` stamps for the completed line.
        assert_eq!(
            link_files(&w, &main, &wip, "AMT-7"),
            vec!["a.txt", "b.txt", "c.txt"]
        );
    }

    /// SIRF-42: every iteration resets to the base ref's tip at ITS claim,
    /// and falls back to the launch base when the ref does not resolve.
    #[test]
    fn each_iteration_resets_to_the_base_refs_current_tip() {
        let w = GitWorld::new("freshbase");
        let launch = w.base.clone();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let main = commit_file(&w.repo, "landed.txt");
        for _ in 0..2 {
            let s = seen.clone();
            w.agent(move |wt| s.lock().unwrap().push(git_at(wt, &["rev-parse", "HEAD"])));
        }
        // An agent that does nothing and fails: nothing to preserve.
        w.mock
            .push(MockResponse::new(&["sh", "-c", "true"], 1, "", "no"));
        let (o, evs) = w.iterate("AMT-12", &cfg());
        assert!(matches!(o, IterationOutcome::Error(_)), "{o:?}");
        assert_eq!(event(&evs, "claim")["base_sha"], json!(main));
        assert!(event(&evs, "release").get("wip_ref").is_none());
        assert_eq!(w.ref_at("refs/sirius/wip/amt-12"), None);
        // A base ref that does not resolve: the launch base.
        let mut c = cfg();
        c.review.base_ref = Some("no-such-branch".into());
        w.mock
            .push(MockResponse::new(&["sh", "-c", "true"], 1, "", "no"));
        let (_o, evs) = w.iterate("AMT-12", &c);
        assert_eq!(event(&evs, "claim")["base_sha"], json!(launch));
        assert_eq!(*seen.lock().unwrap(), vec![main, launch]);
    }

    /// SIRF-41 policy: work a failing gate JUDGED is kept at wip-failed and
    /// offered as SIRIUS_PRIOR_WORK — never merged back by itself. Once the
    /// issue completes without it, it is parked, not offered again.
    #[test]
    fn gate_failed_work_is_offered_not_resumed_and_parked_at_completion() {
        let w = GitWorld::new("failed");
        w.agent(|wt| {
            commit_file(wt, "bad.txt");
        });
        w.mock
            .expect(&["sh", "-c", "run-suite"], 1, "test result: FAILED");
        let mut c = cfg();
        c.retry_budget = 1;
        let (o, evs) = w.iterate("AMT-9", &c);
        assert_eq!(o, IterationOutcome::Deadend);
        let failed = w.ref_at("refs/sirius/wip-failed/amt-9").expect("kept");
        assert_eq!(w.ref_at("refs/sirius/wip/amt-9"), None);
        assert_eq!(event(&evs, "release")["wip_ref"]["sha"], json!(failed));

        w.agent(|wt| {
            assert!(!wt.join("bad.txt").exists(), "never merged back by itself");
            commit_file(wt, "good.txt");
        });
        w.mock
            .expect(&["sh", "-c", "run-suite"], 0, "test result: ok");
        let (o, _evs) = w.iterate("AMT-9", &c);
        assert_eq!(o, IterationOutcome::Completed);
        let env = &w.mock.agent_envs()[1];
        assert_eq!(
            env_of(env, "SIRIUS_PRIOR_WORK").as_deref(),
            Some("refs/sirius/wip-failed/amt-9")
        );
        assert_eq!(env_of(env, "SIRIUS_RESUMED_FROM"), None);
        assert_eq!(w.ref_at("refs/sirius/wip-failed/amt-9"), None);
        assert_eq!(
            w.ref_at(&format!(
                "refs/sirius/wip-superseded/amt-9/{}",
                short_sha(&failed)
            )),
            Some(failed),
            "the unused attempt stays reachable"
        );
    }

    /// SIRF-41: a lease refused at a RETRY's pre-spawn check — after the
    /// first attempt worked — must not take the work with it. The issue is
    /// not released (not ours), but the work is pinned and named.
    #[test]
    fn a_lost_lease_after_work_started_preserves_the_work() {
        let w = GitWorld::new("lease");
        w.agent(|wt| {
            commit_file(wt, "try1.txt");
        });
        w.mock.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"id":"AMT-10"}"#,
        );
        w.mock.expect(
            &["amt", "--json", "claim", "--issue"],
            0,
            r#"{"claimed":false,"reason":"lease held by sirius/rowan"}"#,
        );
        w.mock
            .expect(&["sh", "-c", "run-suite"], 1, "test result: FAILED");
        let mut c = cfg();
        c.retry_budget = 2;
        let (o, evs) = w.iterate("AMT-10", &c);
        assert!(
            matches!(o, IterationOutcome::Error(ref e) if e.contains("lease")),
            "{o:?}"
        );
        let wip = w.ref_at("refs/sirius/wip/amt-10").expect("pinned");
        assert!(tree_files(&w.repo, &wip).contains(&"try1.txt".to_string()));
        let rel = event(&evs, "release");
        assert_eq!(rel["reason"], "lease_lost");
        assert_eq!(rel["wip_ref"]["sha"], json!(wip));
        let calls = w.calls();
        assert!(!calls.iter().any(|c| c.starts_with("amt --json release")));
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("amt --json issue comment AMT-10")
                    && c.contains("lost the lease")
                    && c.contains("refs/sirius/wip/amt-10")),
            "{calls:?}"
        );
    }

    /// Preserved work that already contains the held commit supersedes it:
    /// ONE resume merge (the wip), both markers retired at the stamp, and
    /// the link resolution still sees the held, wip and new work.
    #[test]
    fn wip_that_contains_the_held_work_is_resumed_alone() {
        let w = GitWorld::new("heldwip");
        let base = w.base.clone();
        let held = commit_file(&w.wt, "h.txt");
        let wip = commit_file(&w.wt, "w.txt");
        git_at(&w.repo, &["update-ref", "refs/sirius/held/amt-11", &held]);
        git_at(&w.repo, &["update-ref", "refs/sirius/wip/amt-11", &wip]);
        git_at(&w.wt, &["checkout", "-q", "--detach", &base]);
        let main = commit_file(&w.repo, "landed.txt");
        w.agent(|wt| {
            commit_file(wt, "mine.txt");
        });
        w.mock
            .expect(&["sh", "-c", "run-suite"], 0, "test result: ok");
        let (o, _evs) = w.iterate("AMT-11", &cfg());
        assert_eq!(o, IterationOutcome::Completed);
        let calls = w.calls();
        let merges: Vec<&String> = calls
            .iter()
            .filter(|c| c.starts_with("git merge --no-edit"))
            .collect();
        assert_eq!(merges, vec![&format!("git merge --no-edit {wip}")]);
        let env = &w.mock.agent_envs()[0];
        assert_eq!(env_of(env, "SIRIUS_RESUMED_FROM"), Some(wip.clone()));
        assert_eq!(w.ref_at("refs/sirius/wip/amt-11"), None);
        assert_eq!(w.ref_at("refs/sirius/held/amt-11"), None);
        assert_eq!(
            link_files(&w, &main, &wip, "AMT-11"),
            vec!["h.txt", "mine.txt", "w.txt"]
        );
    }

    /// preserve_wip in isolation: nothing new is a no-op (suite telemetry
    /// is not work); untracked files are work; an older pin the new work does
    /// not contain is parked, never overwritten into oblivion.
    #[test]
    fn preserve_wip_pins_only_new_work_and_parks_what_it_supersedes() {
        let w = GitWorld::new("unit");
        let base = w.base.clone();
        let r = &w.git;
        assert_eq!(
            preserve_wip(r, "AMT-5", &base, &base, WipKind::Resume),
            Ok(None)
        );
        std::fs::create_dir_all(w.wt.join(".suite")).unwrap();
        std::fs::write(w.wt.join(".suite/events.ndjson"), "{}").unwrap();
        assert_eq!(
            preserve_wip(r, "AMT-5", &base, &base, WipKind::Resume),
            Ok(None)
        );
        std::fs::write(w.wt.join("new.txt"), "n").unwrap();
        let first = preserve_wip(r, "AMT-5", &base, &base, WipKind::Resume)
            .unwrap()
            .expect("untracked work is work");
        assert_eq!(first.ref_name, "refs/sirius/wip/amt-5");
        assert!(tree_files(&w.repo, &first.sha).contains(&"new.txt".to_string()));
        assert_eq!(git_at(&w.wt, &["status", "--porcelain"]), "");
        // A different line of work for the same issue, not containing it.
        git_at(&w.wt, &["checkout", "-q", "--detach", &base]);
        std::fs::write(w.wt.join("other.txt"), "o").unwrap();
        let second = preserve_wip(r, "AMT-5", &base, &base, WipKind::Resume)
            .unwrap()
            .unwrap();
        assert_eq!(w.ref_at("refs/sirius/wip/amt-5"), Some(second.sha.clone()));
        assert_eq!(
            w.ref_at(&format!(
                "refs/sirius/wip-superseded/amt-5/{}",
                short_sha(&first.sha)
            )),
            Some(first.sha)
        );
        // Nothing past the floor any more.
        assert_eq!(
            preserve_wip(r, "AMT-5", &second.sha, &base, WipKind::Resume),
            Ok(None)
        );
    }

    /// SIRF-49: an agent that checks out (and commits on) a named branch is
    /// pinned at refs/sirius/keep/<issue>/<branch> and detached; its branch
    /// is left alone; completion still stamps sirius/<issue>.
    #[test]
    fn a_branch_the_agent_checked_out_is_pinned_and_detached() {
        let w = GitWorld::new("guard");
        w.agent(|wt| {
            git_at(wt, &["switch", "-q", "-c", "Feature/X"]);
            commit_file(wt, "x.txt");
        });
        w.mock
            .expect(&["sh", "-c", "run-suite"], 0, "test result: ok");
        let (o, evs) = w.iterate("AMT-8", &cfg());
        assert_eq!(o, IterationOutcome::Completed, "{evs:?}");
        let tip = w
            .ref_at("refs/heads/Feature/X")
            .expect("the branch is untouched");
        assert_eq!(
            w.ref_at("refs/sirius/keep/amt-8/feature-x"),
            Some(tip.clone())
        );
        let g = evs
            .iter()
            .find(|e| e["event"] == "branch_guard")
            .expect("branch_guard event");
        assert_eq!(g["branch"], "Feature/X");
        assert_eq!(g["sha"], json!(tip));
        assert_eq!(g["detached"], true);
        assert!(
            crate::gitrange::run_git(&w.git, &["symbolic-ref", "-q", "HEAD"]).is_err(),
            "the worktree is detached again"
        );
        assert_eq!(w.ref_at("refs/heads/sirius/amt-8"), Some(tip));
    }

    fn index_lock(w: &GitWorld) -> std::path::PathBuf {
        std::path::PathBuf::from(git_at(
            &w.wt,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "index.lock",
            ],
        ))
    }

    /// Review B1: an uncommittable rest must never cost the agent's COMMITTED
    /// work — HEAD is pinned anyway, and the partial loss is said. A plain
    /// stale `index.lock` left by the killed agent is cleared and costs
    /// nothing at all.
    #[test]
    fn preserve_wip_pins_committed_work_when_the_rest_cannot_be_committed() {
        let w = GitWorld::new("partial");
        let base = w.base.clone();
        // A stale lock FILE: cleared, everything committed.
        commit_file(&w.wt, "a.txt");
        std::fs::write(w.wt.join("b.txt"), "b").unwrap();
        std::fs::write(index_lock(&w), "").unwrap();
        let got = preserve_wip(&w.git, "AMT-6", &base, &base, WipKind::Resume)
            .unwrap()
            .unwrap();
        assert_eq!(got.partial, None);
        assert_eq!(
            tree_files(&w.repo, &got.sha),
            vec!["a.txt", "b.txt", "base.txt"]
        );
        // An index that cannot be locked at all (a DIRECTORY in the way).
        let committed = commit_file(&w.wt, "c.txt");
        std::fs::write(w.wt.join("d.txt"), "d").unwrap();
        std::fs::create_dir_all(index_lock(&w)).unwrap();
        let got = preserve_wip(&w.git, "AMT-6", &base, &base, WipKind::Resume);
        std::fs::remove_dir_all(index_lock(&w)).unwrap();
        let got = got.unwrap().expect("the committed work is pinned anyway");
        assert_eq!(got.sha, committed);
        assert!(got.partial.is_some());
        assert!(
            got.suffix().contains("could NOT be saved"),
            "{}",
            got.suffix()
        );
        assert!(got.json()["partial"].is_string());
        assert_eq!(w.ref_at("refs/sirius/wip/amt-6"), Some(committed));
    }

    /// Review B2: a worktree left on an UNBORN branch (or an unmerged index)
    /// must not wedge the worker at the next prep; and the guard detaches an
    /// agent's orphan checkout.
    #[test]
    fn an_orphan_checkout_never_wedges_the_worker() {
        let w = GitWorld::new("orphan");
        git_at(&w.wt, &["switch", "-q", "--orphan", "left-behind"]);
        w.agent(|wt| {
            git_at(wt, &["switch", "-q", "--orphan", "agent-orphan"]);
        });
        w.mock
            .push(MockResponse::new(&["sh", "-c", "true"], 1, "", "no"));
        let (o, evs) = w.iterate("AMT-13", &cfg());
        assert!(
            !evs.iter().any(|e| e["reason"] == "worktree_prep_failed"),
            "{evs:?}"
        );
        assert!(
            matches!(o, IterationOutcome::Error(ref e) if e.contains("agent")),
            "{o:?}"
        );
        let g = evs
            .iter()
            .find(|e| e["event"] == "branch_guard")
            .expect("guarded");
        assert_eq!(g["branch"], "agent-orphan");
        assert_eq!(g["detached"], true);
        assert!(g["sha"].is_null());
        assert!(crate::gitrange::run_git(&w.git, &["symbolic-ref", "-q", "HEAD"]).is_err());
    }

    /// Review B3: preserved work that was resumed and then JUDGED (here: the
    /// gate fails with nothing new on top) moves to wip-failed and stops
    /// being merged back automatically.
    #[test]
    fn resumed_work_that_then_fails_the_gate_is_no_longer_auto_resumed() {
        let w = GitWorld::new("rejudged");
        w.agent(|wt| {
            commit_file(wt, "a.txt");
        });
        w.mock.arm_agent_timeout(0);
        w.iterate("AMT-14", &cfg());
        let wip = w.ref_at("refs/sirius/wip/amt-14").expect("pinned");
        // Iteration 2 resumes it; the agent adds nothing; the gate fails.
        w.mock
            .expect(&["sh", "-c", "run-suite"], 1, "test result: FAILED");
        let mut c = cfg();
        c.retry_budget = 1;
        let (o, evs) = w.iterate("AMT-14", &c);
        assert_eq!(o, IterationOutcome::Deadend);
        assert_eq!(w.ref_at("refs/sirius/wip/amt-14"), None, "retired");
        let failed = w.ref_at("refs/sirius/wip-failed/amt-14").expect("kept");
        assert!(is_ancestor(&w.git, &wip, &failed));
        assert_eq!(event(&evs, "release")["wip_ref"]["sha"], json!(failed));
        // Iteration 3: offered, not merged.
        w.mock
            .push(MockResponse::new(&["sh", "-c", "true"], 1, "", "no"));
        w.iterate("AMT-14", &c);
        let env = &w.mock.agent_envs()[2];
        assert_eq!(env_of(env, "SIRIUS_RESUMED_FROM"), None);
        assert_eq!(
            env_of(env, "SIRIUS_PRIOR_WORK").as_deref(),
            Some("refs/sirius/wip-failed/amt-14")
        );
    }

    /// An agent that itself FAILED was interrupted — its partial tree is
    /// resumed next claim even though the gate then failed over it.
    #[test]
    fn a_failed_agents_partial_work_is_resumable_even_if_the_gate_failed() {
        let w = GitWorld::new("agentfail");
        w.agent(|wt| std::fs::write(wt.join("p.txt"), "p").unwrap());
        w.mock
            .push(MockResponse::new(&["sh", "-c", "true"], 1, "", "crash"));
        w.mock
            .expect(&["sh", "-c", "run-suite"], 1, "test result: FAILED");
        let mut c = cfg();
        c.retry_budget = 1;
        let (_o, evs) = w.iterate("AMT-15", &c);
        let wip = w.ref_at("refs/sirius/wip/amt-15").expect("resumable");
        assert_eq!(w.ref_at("refs/sirius/wip-failed/amt-15"), None);
        assert_eq!(event(&evs, "release")["wip_ref"]["sha"], json!(wip));
    }

    /// A fix round's discarded attempt is parked before the revert.
    #[test]
    fn park_attempt_keeps_a_discarded_attempt_reachable() {
        let w = GitWorld::new("park");
        let head = w.base.clone();
        assert_eq!(park_attempt(&w.git, "AMT-16", &head), None);
        commit_file(&w.wt, "fix1.txt");
        std::fs::write(w.wt.join("fix2.txt"), "f").unwrap();
        let parked = park_attempt(&w.git, "AMT-16", &head).expect("parked");
        assert!(parked.starts_with("refs/sirius/wip-superseded/amt-16/"));
        let sha = w.ref_at(&parked).unwrap();
        assert_eq!(
            tree_files(&w.repo, &sha),
            vec!["base.txt", "fix1.txt", "fix2.txt"]
        );
    }

    #[test]
    fn ref_segments_are_lowercase_and_ref_safe() {
        assert_eq!(ref_segment("sirius/LYD-54"), "sirius-lyd-54");
        assert_eq!(ref_segment("a.b..lock"), "a-b--lock");
    }

    /// Mock-level: the iteration base comes from the given ref, and is
    /// never queried in a detached launch.
    #[test]
    fn iteration_base_resolution_order() {
        let m = MockRunner::new();
        assert_eq!(iteration_base(&m, None, "launch"), "launch");
        assert!(m.recorded().is_empty());
        m.expect(&["git", "rev-parse"], 0, "tip1\n");
        assert_eq!(iteration_base(&m, Some("main"), "launch"), "tip1");
        assert_eq!(
            m.recorded(),
            vec!["git rev-parse --verify --quiet main^{commit}"]
        );
        m.push(MockResponse::new(&["git", "rev-parse"], 1, "", ""));
        assert_eq!(iteration_base(&m, Some("gone"), "launch"), "launch");
    }
}
