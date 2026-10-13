//! The Hayvenhurst (`hayven`) boundary.
//!
//! All Hayvenhurst reads/writes go through the `hayven` CLI (PRD §2.2). Flags
//! here are the ground-truth `hayven 0.0.5`..`0.0.8` forms (see CONTRACTS §6).
//! Notable deltas from the PRD/CONTRACTS §4 intent:
//!   * `hayven claim <ids...> --intent "..." --agent <worker> --json [--force]`
//!     — ids are POSITIONAL. `--agent` (default `cli`!) and `--json` exist and
//!     are REQUIRED here: without `--agent` every worker shares one identity,
//!     and without `--json` hayven prints markdown with no parseable claim id.
//!     The outcome is read from the JSON body, not the exit code alone: 0.0.8
//!     exits 1 for an idempotent re-claim (HTTP 200) as well as for a 409
//!     overlap and a daemon error (see `classify_claim`).
//!   * `hayven remember "<note>" [--node <id>] [--kind K] [--scope a,b]` — the
//!     note is the FIRST positional arg.
//!   * `hayven affected-tests <symbol> [--changed a,b] [--trace-only] [--json]`
//!     has NO `--gate`/`--gate-tier` flags. Sirius synthesizes the gate verdict
//!     from the command's exit code (0 = pass, 1 = fail), matching the exit-code
//!     contract in PRD §6 fact 4.
//!   * The daemon on :7777 is single-project-bound: a call against a workspace
//!     whose daemon isn't the one on :7777 fails (exit 1, "serves a DIFFERENT
//!     project"). `hayven claim` exit codes: 0 registered (201), 1 = 409 overlap OR
//!     200 idempotent re-claim OR daemon error (decide from the body), 3 = oracle
//!     adjacency (202).

use crate::shell::{CmdOutput, Runner};
use serde_json::Value;

/// Result of a `hayven claim`, decided from the JSON body then the exit code.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimVerdict {
    /// Claim held by us: freshly registered (201) or an idempotent re-claim by the same agent (200).
    Registered { claim_id: Option<String> },
    /// Hard overlap (409). `detail` names the blocking claim if known.
    Overlap { detail: String },
    /// Soft oracle adjacency conflict (202); force-able.
    OracleConflict { detail: String },
    /// Any other failure (daemon down, wrong project, etc).
    Error { detail: String },
}

pub struct Hayven<'r> {
    runner: &'r dyn Runner,
}

impl<'r> Hayven<'r> {
    pub fn new(runner: &'r dyn Runner) -> Self {
        Hayven { runner }
    }

    /// `hayven --version`
    pub fn version(&self) -> Result<String, String> {
        let out = self
            .runner
            .run("hayven", &["--version"])
            .map_err(|e| e.to_string())?;
        if out.success() {
            Ok(out.stdout.trim().to_string())
        } else {
            Err(out.stderr.trim().to_string())
        }
    }

    /// `hayven daemon status` — returns the trimmed status line ("running" /
    /// "stopped" / a project-mismatch message).
    pub fn daemon_status(&self) -> Result<String, String> {
        let out = self
            .runner
            .run("hayven", &["daemon", "status"])
            .map_err(|e| e.to_string())?;
        Ok(combined(&out).trim().to_string())
    }

    /// `hayven query <terms> --json`
    pub fn query(&self, terms: &str) -> Result<Value, String> {
        self.json(&["query", terms, "--json"])
    }

    /// `hayven impact <symbol> --json`
    pub fn impact(&self, symbol: &str) -> Result<Value, String> {
        self.json(&["impact", symbol, "--json"])
    }

    /// `hayven context <symbol> --json`
    pub fn context(&self, symbol: &str) -> Result<Value, String> {
        self.json(&["context", symbol, "--json"])
    }

    /// `hayven recall --node <id> --json`
    pub fn recall_node(&self, node: &str) -> Result<Value, String> {
        self.json(&["recall", "--node", node, "--json"])
    }

    /// `hayven claim <ids...> --intent "<intent>" --agent <agent> --json [--force]`.
    ///
    /// `agent` is the worker identity (the same one used for `amt`). It is
    /// REQUIRED in practice: hayven derives the claim id deterministically from
    /// `(agent, sorted scope)` and defaults the agent to `cli`, so without a
    /// per-worker agent every worker shares one identity — a retry by a
    /// different worker would look like an idempotent re-claim by the same
    /// agent (and silently "hold" someone else's lock).
    ///
    /// The outcome is decided from the `--json` BODY first (see
    /// [`classify_claim`]), then the exit code: hayven 0.0.8 exits 1 for an
    /// idempotent re-claim (HTTP 200) *and* for a 409 overlap *and* for a
    /// daemon error. An idempotent re-claim by the same agent is success; if
    /// that re-claim returns an EXPIRED claim (past its TTL but not yet
    /// removed, so it no longer blocks anyone and no longer protects us) it is
    /// released and re-registered once so the lease is real again.
    pub fn claim(&self, ids: &[String], intent: &str, force: bool, agent: &str) -> ClaimVerdict {
        for attempt in 0..2 {
            let mut args: Vec<&str> = Vec::new();
            args.push("claim");
            for id in ids {
                args.push(id.as_str());
            }
            args.push("--intent");
            args.push(intent);
            args.push("--agent");
            args.push(agent);
            args.push("--json");
            if force {
                args.push("--force");
            }
            let out = match self.runner.run("hayven", &args) {
                Ok(o) => o,
                Err(e) => {
                    return ClaimVerdict::Error {
                        detail: e.to_string(),
                    }
                }
            };
            match classify_claim(&out, agent) {
                Classified::Verdict(v) => return v,
                Classified::Expired { id } => {
                    // The only way to refresh an expired claim is to release it
                    // and register again (a re-claim just echoes the old row).
                    if attempt == 0 && self.release(&id).is_ok() {
                        continue;
                    }
                    return ClaimVerdict::Error {
                        detail: format!("claim {id} has expired and could not be re-registered"),
                    };
                }
            }
        }
        ClaimVerdict::Error {
            detail: "claim re-registration did not converge".into(),
        }
    }

    /// `hayven release <claim_id> --json`. Releasing a claim that is already
    /// gone (HTTP 404 — released earlier or reaped) is success: the goal state
    /// holds.
    pub fn release(&self, claim_id: &str) -> Result<(), String> {
        let out = self
            .runner
            .run("hayven", &["release", claim_id, "--json"])
            .map_err(|e| e.to_string())?;
        if out.success() {
            return Ok(());
        }
        if parse_json_object(&out.stdout)
            .as_ref()
            .and_then(http_status)
            == Some(404)
        {
            return Ok(());
        }
        Err(combined(&out).trim().to_string())
    }

    /// `hayven remember "<note>" [--node <id>] --kind <kind> [--scope a,b]`.
    /// This is the reverse-provenance write (PRD §6 fact 3).
    pub fn remember(
        &self,
        note: &str,
        node: Option<&str>,
        kind: &str,
        scope: &[String],
    ) -> Result<(), String> {
        let mut args: Vec<String> = vec!["remember".into(), note.into()];
        if let Some(n) = node {
            args.push("--node".into());
            args.push(n.into());
        }
        args.push("--kind".into());
        args.push(kind.into());
        if !scope.is_empty() {
            args.push("--scope".into());
            args.push(scope.join(","));
        }
        args.push("--json".into());
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = self
            .runner
            .run("hayven", &argv)
            .map_err(|e| e.to_string())?;
        if out.success() {
            Ok(())
        } else {
            Err(combined(&out).trim().to_string())
        }
    }

    /// `hayven affected-tests --changed <csv> --json` — the reference-recipe
    /// invocation that selects tests from *changed files* (not a symbol). This
    /// is a SELECTOR ONLY: exit 0 means "selection computed," never "tests
    /// pass." The gate parses the returned JSON (`roots`, `note`, `tests`) to
    /// decide whether to trust a narrow selection or fall back to the full
    /// suite, and then runs the tests itself (SIRF-5 / D-3).
    ///
    /// Returns `(command_ok, parsed_json, detail)`. `command_ok` is false on a
    /// non-zero exit, an execution error, or empty input — every one of which
    /// the gate treats as doubt.
    pub fn affected_tests_changed(&self, files: &[String]) -> (bool, Option<Value>, String) {
        if files.is_empty() {
            return (false, None, "no changed files".into());
        }
        let csv = files.join(",");
        let argv = ["affected-tests", "--changed", csv.as_str(), "--json"];
        match self.runner.run("hayven", &argv) {
            Ok(out) => {
                let ok = out.success();
                let parsed = serde_json::from_str::<Value>(&out.stdout).ok();
                (ok, parsed, combined(&out).trim().to_string())
            }
            Err(e) => (false, None, e.to_string()),
        }
    }

    fn json(&self, args: &[&str]) -> Result<Value, String> {
        let out = self.runner.run("hayven", args).map_err(|e| e.to_string())?;
        if !out.success() {
            return Err(combined(&out).trim().to_string());
        }
        serde_json::from_str(&out.stdout).map_err(|e| {
            format!(
                "hayven returned non-JSON: {e}; stdout={}",
                out.stdout.trim()
            )
        })
    }
}

/// What one `hayven claim` invocation resolved to, before the expired-claim
/// re-registration step.
enum Classified {
    Verdict(ClaimVerdict),
    /// An idempotent re-claim returned our OWN claim, but past its TTL.
    Expired {
        id: String,
    },
}

/// Parse stdout as a single JSON object (hayven `--json` prints exactly one).
fn parse_json_object(stdout: &str) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// The HTTP status of a hayven `--json` body. Newer hayven adds an explicit
/// `httpStatus`; 0.0.8 prints `{"status": <http>, ...payload}` where the
/// payload's own `status` ("active"/"expired"/"potential-conflict") wins on
/// success paths — so a NUMERIC `status` only survives on errors (409/404/5xx).
fn http_status(body: &serde_json::Map<String, Value>) -> Option<u64> {
    body.get("httpStatus")
        .and_then(Value::as_u64)
        .or_else(|| body.get("status").and_then(Value::as_u64))
}

/// Lift a claim id out of hayven's human (markdown) output:
/// "# Claim registered\n\n- id:    `claim_...`". Only a fallback for a CLI that
/// ignored `--json`.
fn markdown_claim_id(stdout: &str) -> Option<String> {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("- id:"))?;
    let start = line.find('`')? + 1;
    let end = line[start..].find('`')? + start;
    let id = &line[start..end];
    (!id.is_empty()).then(|| id.to_string())
}

/// A readable one-line description of a 409/202 JSON body.
fn describe_conflict(body: &serde_json::Map<String, Value>, fallback: &str) -> String {
    if let Some(verdicts) = body.get("verdicts").and_then(Value::as_array) {
        let reasons: Vec<&str> = verdicts
            .iter()
            .filter_map(|v| v.get("reason").and_then(Value::as_str))
            .collect();
        if !reasons.is_empty() {
            return reasons.join("; ");
        }
    }
    let mut s = String::new();
    if let Some(e) = body.get("error").and_then(Value::as_str) {
        s.push_str("409 ");
        s.push_str(e);
    }
    if let Some(c) = body.get("conflictingClaimId").and_then(Value::as_str) {
        s.push_str(&format!(" (held by claim {c}"));
        if let Some(ents) = body.get("overlappingEntities").and_then(Value::as_array) {
            let names: Vec<&str> = ents.iter().filter_map(Value::as_str).collect();
            if !names.is_empty() {
                s.push_str(&format!(" on {}", names.join(", ")));
            }
        }
        s.push(')');
    }
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

/// Decide what a `hayven claim --json` run means. Order of evidence:
///
/// 1. the BODY's explicit `outcome` ("granted" | "held" | "conflict" |
///    "potential-conflict" | "error") / `httpStatus` — present in newer hayven;
/// 2. 0.0.8 heuristics on the body: a claim `id` whose `agent` is OURS means
///    the claim is held (201 fresh OR 200 idempotent — the 200 exits 1 on
///    0.0.8); a numeric `status` 409 is an overlap; `potential-conflict` is the
///    202 oracle verdict;
/// 3. no parseable body (old CLI printing markdown, a daemon-unreachable
///    message on stderr): the exit code, with exit 1 mapped to Overlap only
///    when the text positively describes one.
///
/// Exit 1 alone never means overlap: it is also the code for a daemon error.
fn classify_claim(out: &CmdOutput, agent: &str) -> Classified {
    let detail = combined(out).trim().to_string();
    let granted = |body: &serde_json::Map<String, Value>| -> Classified {
        let id = ["id", "claim_id", "claimId"]
            .iter()
            .find_map(|k| body.get(*k).and_then(Value::as_str))
            .map(|s| s.to_string());
        match (body.get("status").and_then(Value::as_str), id) {
            (Some("expired"), Some(id)) => Classified::Expired { id },
            (_, claim_id) => Classified::Verdict(ClaimVerdict::Registered { claim_id }),
        }
    };
    let verdict = |v: ClaimVerdict| Classified::Verdict(v);

    if let Some(body) = parse_json_object(&out.stdout) {
        let http = http_status(&body);
        match body.get("outcome").and_then(Value::as_str) {
            Some("granted") | Some("held") => return granted(&body),
            // 202: an oracle flagged a potential conflict; the claim is NOT
            // registered, so it must never read as held.
            Some("potential-conflict") => {
                return verdict(ClaimVerdict::OracleConflict {
                    detail: describe_conflict(&body, &detail),
                })
            }
            Some("conflict") => {
                let d = describe_conflict(&body, &detail);
                return verdict(if http == Some(202) || out.code == Some(3) {
                    ClaimVerdict::OracleConflict { detail: d }
                } else {
                    ClaimVerdict::Overlap { detail: d }
                });
            }
            Some("error") => return verdict(ClaimVerdict::Error { detail }),
            _ => {}
        }
        match http {
            Some(200) | Some(201) => return granted(&body),
            Some(202) => {
                return verdict(ClaimVerdict::OracleConflict {
                    detail: describe_conflict(&body, &detail),
                })
            }
            Some(409) => {
                return verdict(ClaimVerdict::Overlap {
                    detail: describe_conflict(&body, &detail),
                })
            }
            Some(_) => return verdict(ClaimVerdict::Error { detail }),
            None => {}
        }
        if out.code == Some(3)
            || body.get("status").and_then(Value::as_str) == Some("potential-conflict")
        {
            return verdict(ClaimVerdict::OracleConflict {
                detail: describe_conflict(&body, &detail),
            });
        }
        let ours = body.get("agent").and_then(Value::as_str) == Some(agent);
        let has_id = ["id", "claim_id", "claimId"]
            .iter()
            .any(|k| body.get(*k).and_then(Value::as_str).is_some());
        if has_id && (ours || out.code == Some(0)) {
            return granted(&body);
        }
        if out.code == Some(0) {
            // Exit 0 with a JSON body that names no claim id: still a
            // registration; the caller refuses to manage an id-less lease.
            return verdict(ClaimVerdict::Registered { claim_id: None });
        }
    }

    match out.code {
        Some(0) => verdict(ClaimVerdict::Registered {
            claim_id: markdown_claim_id(&out.stdout),
        }),
        Some(3) => verdict(ClaimVerdict::OracleConflict { detail }),
        // Exit 1 is OVERLOADED upstream: a genuine 409 overlap, but also
        // "daemon serves a DIFFERENT project", daemon-unreachable and 5xx.
        // Treating those as Overlap sent the loop into endless 409 backoff
        // against a daemon that would never answer (the shared-daemon-on-:7777
        // topology is the NORM in the field). POSITIVE-match the overlap
        // vocabulary and default everything else to Error: an unrecognized
        // operational failure then hits the loop's error budget (visible,
        // bounded) instead of backing off forever (silent, unbounded).
        Some(1) if looks_like_overlap(&detail) => verdict(ClaimVerdict::Overlap { detail }),
        // Signal-killed (code None) or any other exit: an ERROR, not an
        // overlap.
        _ => verdict(ClaimVerdict::Error { detail }),
    }
}

/// Does an exit-1 `hayven claim` message describe a genuine 409 overlap?
/// Upstream overloads exit 1 (see the module header: "serves a DIFFERENT
/// project" is also exit 1, as is a daemon-unreachable CLI). We POSITIVE-match
/// the overlap vocabulary rather than blocklisting failures: a blocklist
/// missed any unlisted operational message and silently spun the 409 backoff
/// forever, and a genuine overlap's detail echoes the HOLDER'S intent text
/// (an issue title containing "connection refused" would have misread as an
/// error). Positive matching keys on the daemon's own framing instead.
fn looks_like_overlap(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    ["overlap", "held by", "409", "already claimed", "claimed by"]
        .iter()
        .any(|k| d.contains(k))
}

/// Does a hayven failure message describe a CONNECTIVITY/scope problem (daemon
/// down or bound to another repo) rather than a normal "nothing found" answer?
/// Used by `sirius why` to distinguish "no provenance recorded" (a valid empty
/// answer) from "the daemon could not be asked" (an error).
pub(crate) fn looks_like_connectivity_failure(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    [
        "different project",
        "not serving",
        "connection refused",
        "econnrefused",
        "daemon not running",
        "no daemon",
        "failed to connect",
        "connect error",
    ]
    .iter()
    .any(|k| d.contains(k))
}

/// Combine stdout+stderr for human-readable error/detail text (hayven prints
/// errors on either stream depending on the path).
fn combined(out: &CmdOutput) -> String {
    let mut s = out.stdout.clone();
    if !out.stderr.trim().is_empty() {
        if !s.trim().is_empty() {
            s.push('\n');
        }
        s.push_str(&out.stderr);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};
    // ---- Fixtures: REAL `hayven 0.0.8 claim --json` output (captured live
    // against a scratch daemon; see CONTRACTS §6). Quirks the parser must
    // survive: a 200 idempotent re-claim EXITS 1; the payload's own
    // `status: "active"` overwrites the CLI's numeric `status` on success; only
    // errors keep a numeric `status`.
    const AGENT: &str = "sirius/oak";
    /// 201, exit 0.
    const GRANTED_008: &str = r#"{
  "status": "active",
  "id": "claim_08fbb6d2fd54",
  "agent": "sirius/oak",
  "intent": "AMT-7: t",
  "scope": ["a/alpha"],
  "fingerprint": "cli:unfingerprinted",
  "created": 1791782766349,
  "ttl": 1791786366349,
  "suggestedScope": ["a/beta"]
}"#;
    /// 200 idempotent re-claim by the same agent: EXIT 1, same body shape.
    const HELD_008: &str = r#"{
  "status": "active",
  "id": "claim_08fbb6d2fd54",
  "agent": "sirius/oak",
  "intent": "AMT-7: t",
  "scope": ["a/alpha"],
  "fingerprint": "cli:unfingerprinted",
  "created": 1791782766349,
  "ttl": 1791786366349
}"#;
    /// 200 re-claim of our own claim past its TTL: EXIT 1, `status: "expired"`.
    const EXPIRED_008: &str = r#"{
  "status": "expired",
  "id": "claim_9cd7e5fb7537",
  "agent": "sirius/oak",
  "intent": "z",
  "scope": ["b/gamma"],
  "fingerprint": "cli:unfingerprinted",
  "created": 1791782795582,
  "ttl": 1791782796582
}"#;
    /// 409 overlap with another agent's claim: exit 1.
    const OVERLAP_008: &str = r#"{
  "status": 409,
  "error": "scope overlaps an active claim",
  "conflictingClaimId": "claim_08fbb6d2fd54",
  "overlappingEntities": ["a/alpha"]
}"#;
    /// 202 oracle adjacency: exit 3 (`status` is the payload's string).
    const ORACLE_008: &str = r#"{
  "status": "potential-conflict",
  "message": "Adjacent active claim(s) may break each other's assumptions; coordinate or re-submit with force:true.",
  "verdicts": [{"conflict": true, "reason": "Connected by a call/import edge to active claim `claim_08fbb6d2fd54`. A contract change on beta breaks a real dependent (beta -> alpha).", "confidence": 0.85, "oracle": "contract-diff"}]
}"#;
    /// 409 because the id exists under a DIFFERENT agent.
    const ID_COLLISION_008: &str = r#"{
  "status": 409,
  "error": "claim id already exists (held by a different agent)",
  "existing": {"id": "claim_x", "agent": "someone/else", "status": "active"}
}"#;

    fn claim_with(code: i32, stdout: &str, stderr: &str) -> ClaimVerdict {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "claim"],
            code,
            stdout,
            stderr,
        ));
        Hayven::new(&m).claim(&["a/alpha".into()], "AMT-7: t", false, AGENT)
    }

    #[test]
    fn claim_passes_agent_and_json_flags() {
        let m = MockRunner::new();
        m.expect(&["hayven", "claim"], 0, GRANTED_008);
        let h = Hayven::new(&m);
        h.claim(&["a".into(), "b".into()], "AMT-7: t", false, AGENT);
        assert_eq!(
            m.recorded()[0],
            "hayven claim a b --intent AMT-7: t --agent sirius/oak --json"
        );
        let m = MockRunner::new();
        m.expect(&["hayven", "claim"], 0, GRANTED_008);
        Hayven::new(&m).claim(&["a".into()], "i", true, AGENT);
        assert_eq!(
            m.recorded()[0],
            "hayven claim a --intent i --agent sirius/oak --json --force"
        );
    }

    #[test]
    fn claim_201_is_registered_with_id() {
        assert_eq!(
            claim_with(0, GRANTED_008, ""),
            ClaimVerdict::Registered {
                claim_id: Some("claim_08fbb6d2fd54".into())
            }
        );
    }

    #[test]
    fn claim_200_idempotent_reclaim_exit1_is_success_not_error() {
        // The bug: 0.0.8 exits 1 here, which used to surface as
        // "daemon returned 200" -> ClaimVerdict::Error.
        assert_eq!(
            claim_with(1, HELD_008, ""),
            ClaimVerdict::Registered {
                claim_id: Some("claim_08fbb6d2fd54".into())
            }
        );
    }

    #[test]
    fn claim_200_for_a_different_agent_is_not_success() {
        // A body naming ANOTHER agent's claim, exit 1, no explicit status:
        // not ours -> falls through to the exit-code heuristics (an Error).
        let other = HELD_008.replace("sirius/oak", "sirius/elm");
        assert!(matches!(
            claim_with(1, &other, ""),
            ClaimVerdict::Error { .. }
        ));
    }

    #[test]
    fn claim_409_json_is_overlap_naming_the_blocker() {
        match claim_with(1, OVERLAP_008, "") {
            ClaimVerdict::Overlap { detail } => {
                assert!(detail.contains("claim_08fbb6d2fd54"), "{detail}");
                assert!(detail.contains("a/alpha"), "{detail}");
            }
            other => panic!("expected Overlap, got {other:?}"),
        }
        assert!(matches!(
            claim_with(1, ID_COLLISION_008, ""),
            ClaimVerdict::Overlap { .. }
        ));
    }

    #[test]
    fn claim_202_json_is_oracle_conflict() {
        match claim_with(3, ORACLE_008, "") {
            ClaimVerdict::OracleConflict { detail } => assert!(detail.contains("beta")),
            other => panic!("expected OracleConflict, got {other:?}"),
        }
    }

    #[test]
    fn claim_forced_201_with_overridden_verdicts_is_registered() {
        let forced = r#"{"status":"active","id":"claim_bcb3ba881a01","agent":"sirius/oak","intent":"AMT-9","scope":["a/beta"],"fingerprint":"f","created":1,"ttl":2,"overriddenVerdicts":[{"conflict":true,"reason":"r","confidence":0.85,"oracle":"contract-diff"}]}"#;
        assert_eq!(
            claim_with(0, forced, ""),
            ClaimVerdict::Registered {
                claim_id: Some("claim_bcb3ba881a01".into())
            }
        );
    }

    #[test]
    fn claim_5xx_json_body_is_error_not_overlap() {
        let body = r#"{"status":500,"error":"boom"}"#;
        assert!(matches!(
            claim_with(1, body, ""),
            ClaimVerdict::Error { .. }
        ));
    }

    // The parallel Hayvenhurst fix adds `httpStatus` + `outcome` WITHOUT
    // removing existing keys, and makes 200 exit 0. Prefer them when present.
    #[test]
    fn claim_prefers_outcome_and_http_status_when_present() {
        let held = HELD_008.replacen("{", r#"{"httpStatus":200,"outcome":"held","#, 1);
        assert_eq!(
            claim_with(0, &held, ""),
            ClaimVerdict::Registered {
                claim_id: Some("claim_08fbb6d2fd54".into())
            }
        );
        let granted = GRANTED_008.replacen("{", r#"{"httpStatus":201,"outcome":"granted","#, 1);
        assert!(matches!(
            claim_with(0, &granted, ""),
            ClaimVerdict::Registered { claim_id: Some(_) }
        ));
        let conflict = OVERLAP_008.replacen("{", r#"{"httpStatus":409,"outcome":"conflict","#, 1);
        assert!(matches!(
            claim_with(1, &conflict, ""),
            ClaimVerdict::Overlap { .. }
        ));
        let oracle = ORACLE_008.replacen(
            "{",
            r#"{"httpStatus":202,"outcome":"potential-conflict","#,
            1,
        );
        assert!(matches!(
            claim_with(3, &oracle, ""),
            ClaimVerdict::OracleConflict { .. }
        ));
        // Even with a success exit code, a potential conflict is never held.
        assert!(matches!(
            claim_with(0, &oracle, ""),
            ClaimVerdict::OracleConflict { .. }
        ));
        let err = r#"{"httpStatus":500,"outcome":"error","error":"boom"}"#;
        assert!(matches!(claim_with(1, err, ""), ClaimVerdict::Error { .. }));
        // `outcome` wins over a contradicting exit code.
        assert!(matches!(
            claim_with(1, &held, ""),
            ClaimVerdict::Registered { .. }
        ));
    }

    #[test]
    fn claim_markdown_output_still_yields_the_id() {
        // An older hayven that ignores --json prints markdown.
        let md = "# Claim registered\n\n- id:    `claim_f4b4c2e04e12`\n- scope: `b/gamma`\n";
        assert_eq!(
            claim_with(0, md, ""),
            ClaimVerdict::Registered {
                claim_id: Some("claim_f4b4c2e04e12".into())
            }
        );
    }

    #[test]
    fn claim_expired_own_claim_is_released_and_reregistered() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["hayven", "claim"], 1, EXPIRED_008, ""));
        m.push(MockResponse::new(
            &["hayven", "release"],
            0,
            r#"{"status":200,"ok":true,"id":"claim_9cd7e5fb7537"}"#,
            "",
        ));
        m.push(MockResponse::new(&["hayven", "claim"], 0, GRANTED_008, ""));
        let v = Hayven::new(&m).claim(&["b/gamma".into()], "z", false, AGENT);
        assert_eq!(
            v,
            ClaimVerdict::Registered {
                claim_id: Some("claim_08fbb6d2fd54".into())
            }
        );
        let calls = m.recorded();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[1].starts_with("hayven release claim_9cd7e5fb7537"));
    }

    #[test]
    fn claim_expired_that_cannot_be_released_is_an_error() {
        let m = MockRunner::new();
        m.push(MockResponse::new(&["hayven", "claim"], 1, EXPIRED_008, ""));
        m.push(MockResponse::new(&["hayven", "release"], 1, "", "boom"));
        assert!(matches!(
            Hayven::new(&m).claim(&["b/gamma".into()], "z", false, AGENT),
            ClaimVerdict::Error { .. }
        ));
    }

    #[test]
    fn release_passes_json_and_treats_404_as_released() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "release"],
            0,
            r#"{"status":200,"ok":true,"id":"c"}"#,
            "",
        ));
        assert!(Hayven::new(&m).release("c").is_ok());
        assert_eq!(m.recorded()[0], "hayven release c --json");
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "release"],
            1,
            r#"{"status":404,"error":"claim not found"}"#,
            "",
        ));
        assert!(Hayven::new(&m).release("c").is_ok());
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "release"],
            1,
            "",
            "error: daemon returned 500",
        ));
        assert!(Hayven::new(&m).release("c").is_err());
    }

    #[test]
    fn claim_exit1_is_overlap() {
        match claim_with(1, "", "409 overlap: held by other/agent") {
            ClaimVerdict::Overlap { detail } => assert!(detail.contains("409")),
            other => panic!("expected Overlap, got {other:?}"),
        }
    }

    // Exit 1 is overloaded upstream: also "serves a DIFFERENT project" and
    // daemon-unreachable. Those are OPERATIONAL errors — mapping them to
    // Overlap sent the loop into endless 409 backoff against a daemon that
    // would never answer (the shared-daemon topology is the field norm).
    #[test]
    fn claim_exit1_wrong_project_is_error_not_overlap() {
        match claim_with(
            1,
            "",
            "workspace error: daemon on :7777 serves a DIFFERENT project (/other/repo)",
        ) {
            ClaimVerdict::Error { detail } => assert!(detail.contains("DIFFERENT")),
            other => panic!("wrong-project exit 1 must be an Error, got {other:?}"),
        }
    }

    #[test]
    fn claim_exit1_connection_refused_is_error_not_overlap() {
        assert!(matches!(
            claim_with(
                1,
                "",
                "error: connection refused (is the daemon running on :7777?)"
            ),
            ClaimVerdict::Error { .. }
        ));
    }

    #[test]
    fn claim_exit3_is_oracle_conflict() {
        assert!(matches!(
            claim_with(3, "", "202 adjacency verdict"),
            ClaimVerdict::OracleConflict { .. }
        ));
    }

    #[test]
    fn affected_tests_changed_reports_ok_and_parses_json() {
        let m = MockRunner::new();
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"roots":["r"],"note":"clean","tests":[]}"#,
        );
        let h = Hayven::new(&m);
        let (ok, parsed, _) = h.affected_tests_changed(&["src/a.rs".into()]);
        assert!(ok);
        assert!(parsed.is_some());
        // File-based invocation: --changed <csv>, no positional symbol.
        assert_eq!(
            m.recorded()[0],
            "hayven affected-tests --changed src/a.rs --json"
        );
    }

    #[test]
    fn affected_tests_changed_marks_nonzero_exit_not_ok() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "affected-tests"],
            1,
            "",
            "index error",
        ));
        let h = Hayven::new(&m);
        let (ok, _, detail) = h.affected_tests_changed(&["src/a.rs".into()]);
        assert!(!ok);
        assert!(detail.contains("index error"));
    }

    #[test]
    fn affected_tests_changed_empty_files_is_not_ok() {
        let m = MockRunner::new();
        let h = Hayven::new(&m);
        let (ok, _, _) = h.affected_tests_changed(&[]);
        assert!(!ok);
        assert_eq!(m.call_count(), 0);
    }

    #[test]
    fn remember_uses_positional_note_and_scope_csv() {
        let m = MockRunner::new();
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem_1"}"#);
        let h = Hayven::new(&m);
        h.remember(
            "AMT-7 governs this",
            Some("nodeA"),
            "decision",
            &["nodeA".into(), "nodeB".into()],
        )
        .unwrap();
        assert_eq!(
            m.recorded()[0],
            "hayven remember AMT-7 governs this --node nodeA --kind decision --scope nodeA,nodeB --json"
        );
    }
}

/// Real-CLI contract test (`cargo test --features integration`): drives the
/// installed `hayven` against a THROWAWAY project, `$HAYVEN_HOME` and daemon
/// port, so it never touches a developer's registry or the daemon on :7777.
/// Pins the claim/release behavior `classify_claim` depends on (hayven >= 0.0.8).
/// NOTE (Windows): `hayven init` fails with ENOTCONN (Bun) when launched from
/// inside `cargo test`; build with `cargo test --features integration --no-run`
/// and run the produced test binary directly.
#[cfg(all(test, feature = "integration"))]
mod integration_tests {
    use super::*;
    use crate::shell::RealRunner;
    use std::process::Command;

    fn sh(dir: &std::path::Path, prog: &str, args: &[&str]) {
        // Output goes to a file, not a pipe: hayven's ingest (Bun) fails to
        // spawn its native parser when its stdio are Rust-created pipes, and a
        // detached daemon must not hold a pipe the test is waiting on.
        let log = dir.join("..").join("cmd.log");
        let f = std::fs::File::create(&log).unwrap();
        let st = Command::new(prog)
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(f.try_clone().unwrap())
            .stderr(f)
            .status()
            .unwrap_or_else(|e| panic!("{prog}: {e}"));
        assert!(
            st.success(),
            "{prog} {args:?}: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }

    #[test]
    fn real_hayven_claim_contract() {
        let root = std::env::temp_dir().join(format!("sirius-hv-int-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("a.ts"),
            "export function alpha(){return 1}\nexport function beta(){return alpha()}\n",
        )
        .unwrap();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        // Sandbox: global state + daemon port are private to this test.
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::env::set_var("HAYVEN_HOME", root.join("home"));
        std::env::set_var("HAYVEN_PORT", port.to_string());
        sh(&proj, "git", &["init", "-q", "."]);
        sh(&proj, "git", &["add", "-A"]);
        sh(
            &proj,
            "git",
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "i",
            ],
        );
        sh(&proj, "hayven", &["init"]);
        sh(&proj, "hayven", &["daemon", "start"]);

        let runner = RealRunner {
            cwd: Some(proj.clone()),
        };
        let hv = Hayven::new(&runner);
        let ids = |s: &str| vec![s.to_string()];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Fresh claim -> Registered with a real id.
            let id = match hv.claim(&ids("a/alpha"), "AMT-1: t", false, "sirius/oak") {
                ClaimVerdict::Registered { claim_id: Some(id) } => id,
                other => panic!("fresh claim: {other:?}"),
            };
            // Same worker again -> idempotent, SAME id, still success.
            assert_eq!(
                hv.claim(&ids("a/alpha"), "AMT-1: t", false, "sirius/oak"),
                ClaimVerdict::Registered {
                    claim_id: Some(id.clone())
                }
            );
            // A different worker -> hard overlap naming our claim.
            match hv.claim(&ids("a/alpha"), "AMT-2: t", false, "sirius/elm") {
                ClaimVerdict::Overlap { detail } => assert!(detail.contains(&id), "{detail}"),
                other => panic!("overlap: {other:?}"),
            }
            // Release, then release again (404 = already gone = ok).
            hv.release(&id).unwrap();
            hv.release(&id).unwrap();
            // Now the other worker gets it.
            assert!(matches!(
                hv.claim(&ids("a/alpha"), "AMT-2: t", false, "sirius/elm"),
                ClaimVerdict::Registered { claim_id: Some(_) }
            ));
        }));
        let _ = Command::new("hayven")
            .args(["daemon", "stop"])
            .current_dir(&proj)
            .output();
        let _ = std::fs::remove_dir_all(&root);
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
