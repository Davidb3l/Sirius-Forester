//! The Ametrite (`amt`) boundary.
//!
//! Sirius NEVER writes Ametrite's SQLite (PRD §2.2); every mutation goes through
//! `amt ... --json`. Flags here are the ground-truth `amt 0.1.0` forms verified
//! at build time (see CONTRACTS §6 "Ground-truth CLI deltas"), NOT the PRD's
//! intent forms. Notably:
//!   * `--json` is a global flag placed before the subcommand.
//!   * comments are `amt issue comment <ID> -m <body>` (not `amt comment`).
//!   * status updates are `amt issue update <ID> --status <s>`.
//!   * `amt claim` on success returns the full issue object; on no-work it
//!     returns `{claimed:false, retry_after, counts, reason}`.
//!   * `amt decide --issue <ID> --title <T> -b <body>` returns `{id:"D-n",...}`.

use crate::shell::Runner;
use serde_json::Value;

/// Outcome of an `amt claim`.
#[derive(Debug, Clone)]
pub enum ClaimResult {
    /// Claimed: the full issue object.
    Claimed(Value),
    /// No work available; optional seconds to wait before retrying.
    NoWork {
        retry_after: Option<u64>,
        reason: String,
    },
    /// The command itself failed (bad workspace, etc).
    Error(String),
}

/// How a lease refresh failed — see [`Amt::heartbeat`].
#[derive(Debug, Clone, PartialEq)]
pub enum HeartbeatError {
    /// amt answered `claimed:false`: the lease is NOT held by this agent.
    Refused(String),
    /// amt could not be asked (unreachable, non-JSON) — possibly transient.
    Failed(String),
}

impl std::fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeartbeatError::Refused(r) => write!(f, "lease refresh refused: {r}"),
            HeartbeatError::Failed(e) => write!(f, "lease refresh failed: {e}"),
        }
    }
}

/// Decode amt's refusal shape (`{"claimed": false, "reason": ...}`) — the ONE
/// decoder for this upstream contract, shared by `claim` and `heartbeat` so a
/// reshape upstream cannot be fixed in one and silently missed in the other.
fn claim_refused(v: &Value, default_reason: &str) -> Option<String> {
    if v.get("claimed").and_then(Value::as_bool) == Some(false) {
        Some(
            v.get("reason")
                .and_then(Value::as_str)
                .unwrap_or(default_reason)
                .to_string(),
        )
    } else {
        None
    }
}

pub struct Amt<'r> {
    runner: &'r dyn Runner,
}

impl<'r> Amt<'r> {
    pub fn new(runner: &'r dyn Runner) -> Self {
        Amt { runner }
    }

    /// `amt --version`
    pub fn version(&self) -> Result<String, String> {
        let out = self
            .runner
            .run("amt", &["--version"])
            .map_err(|e| e.to_string())?;
        if out.success() {
            Ok(out.stdout.trim().to_string())
        } else {
            Err(out.stderr.trim().to_string())
        }
    }

    /// `amt --json claim [--from <stages>] --agent <agent>`.
    pub fn claim(&self, agent: &str, from: Option<&str>) -> ClaimResult {
        let mut args: Vec<String> = vec![
            "--json".into(),
            "claim".into(),
            "--agent".into(),
            agent.into(),
        ];
        if let Some(f) = from {
            args.push("--from".into());
            args.push(f.into());
        }
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = match self.runner.run("amt", &argv) {
            Ok(o) => o,
            Err(e) => return ClaimResult::Error(e.to_string()),
        };
        let v: Value = match serde_json::from_str(&out.stdout) {
            Ok(v) => v,
            Err(_) => {
                return ClaimResult::Error(if out.stderr.trim().is_empty() {
                    "amt claim produced no JSON".into()
                } else {
                    out.stderr.trim().to_string()
                })
            }
        };
        // No-work shape: {"claimed": false, "retry_after": N, ...}
        if let Some(reason) = claim_refused(&v, "no claimable work") {
            let retry_after = v.get("retry_after").and_then(Value::as_u64);
            return ClaimResult::NoWork {
                retry_after,
                reason,
            };
        }
        // Success: the issue object carries an `id`.
        if v.get("id").and_then(Value::as_str).is_some() {
            ClaimResult::Claimed(v)
        } else {
            ClaimResult::Error(format!("unexpected amt claim JSON: {v}"))
        }
    }

    /// `amt --json claim --issue <id> --agent <agent>` — re-claim your own
    /// issue id to renew its lease (heartbeat). Same agent + id is a refresh.
    ///
    /// The two failure shapes are TYPED because callers must react
    /// differently: `Refused` (exit 0 + `claimed:false` — the lease lapsed
    /// and someone else may hold the issue) means STOP working the issue;
    /// `Failed` (amt unreachable / non-JSON) is a transient plumbing error a
    /// long-running agent should survive. Checking only the exit code used to
    /// read a refusal as success, so a lost lease looked renewed and two
    /// agents could share an issue.
    pub fn heartbeat(&self, issue: &str, agent: &str) -> Result<(), HeartbeatError> {
        let v = self
            .json(&["--json", "claim", "--issue", issue, "--agent", agent])
            .map_err(HeartbeatError::Failed)?;
        if let Some(reason) = claim_refused(&v, "lease not held by this agent") {
            return Err(HeartbeatError::Refused(reason));
        }
        Ok(())
    }

    /// `amt --json issue show <id>`
    pub fn issue_show(&self, issue: &str) -> Result<Value, String> {
        self.json(&["--json", "issue", "show", issue])
    }

    /// `amt --json issue comment <id> -m <body>`
    pub fn comment(&self, issue: &str, body: &str) -> Result<(), String> {
        self.json_ok(&["--json", "issue", "comment", issue, "-m", body])
    }

    /// `amt --json issue update <id> --status <status>`
    pub fn update_status(&self, issue: &str, status: &str) -> Result<Value, String> {
        self.json(&["--json", "issue", "update", issue, "--status", status])
    }

    /// `amt --json release <id> --agent <agent> [--status <s>] [-m <comment>]`
    pub fn release(
        &self,
        issue: &str,
        agent: &str,
        status: Option<&str>,
        comment: Option<&str>,
    ) -> Result<(), String> {
        let mut args: Vec<&str> = vec!["--json", "release", issue, "--agent", agent];
        if let Some(s) = status {
            args.push("--status");
            args.push(s);
        }
        if let Some(c) = comment {
            args.push("-m");
            args.push(c);
        }
        self.json_ok(&args)
    }

    /// `amt --json decide --issue <id> --title <title> -b <body>` → `D-n`.
    pub fn decide(&self, issue: &str, title: &str, body: &str) -> Result<String, String> {
        let v = self.json(&[
            "--json", "decide", "--issue", issue, "--title", title, "-b", body,
        ])?;
        v.get("id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| format!("amt decide returned no decision id: {v}"))
    }

    /// `amt --json decision show <id>`
    pub fn decision_show(&self, decision: &str) -> Result<Value, String> {
        self.json(&["--json", "decision", "show", decision])
    }

    /// `amt --json search <terms>` — available for symbol mapping fallbacks.
    #[allow(dead_code)]
    pub fn search(&self, terms: &str) -> Result<Value, String> {
        self.json(&["--json", "search", terms])
    }

    // ---- helpers -------------------------------------------------------

    fn json(&self, args: &[&str]) -> Result<Value, String> {
        let out = self.runner.run("amt", args).map_err(|e| e.to_string())?;
        if !out.success() {
            return Err(err_text(&out.stderr, &out.stdout));
        }
        serde_json::from_str(&out.stdout)
            .map_err(|e| format!("amt returned non-JSON: {e}; stdout={}", out.stdout.trim()))
    }

    fn json_ok(&self, args: &[&str]) -> Result<(), String> {
        let out = self.runner.run("amt", args).map_err(|e| e.to_string())?;
        if out.success() {
            Ok(())
        } else {
            Err(err_text(&out.stderr, &out.stdout))
        }
    }
}

fn err_text(stderr: &str, stdout: &str) -> String {
    let s = stderr.trim();
    if s.is_empty() {
        stdout.trim().to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::MockRunner;

    #[test]
    fn claim_success_parses_issue() {
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"id":"AMT-7","title":"Do the thing","status":"in_progress"}"#,
        );
        let amt = Amt::new(&m);
        match amt.claim("sirius/oak", Some("todo")) {
            ClaimResult::Claimed(v) => {
                assert_eq!(v["id"], "AMT-7");
            }
            other => panic!("expected Claimed, got {other:?}"),
        }
        // Verify the real flag form was used.
        assert_eq!(
            m.recorded()[0],
            "amt --json claim --agent sirius/oak --from todo"
        );
    }

    #[test]
    fn claim_no_work_parses_retry_after() {
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"claimed":false,"retry_after":900,"reason":"held by leases"}"#,
        );
        let amt = Amt::new(&m);
        match amt.claim("sirius/oak", None) {
            ClaimResult::NoWork {
                retry_after,
                reason,
            } => {
                assert_eq!(retry_after, Some(900));
                assert_eq!(reason, "held by leases");
            }
            other => panic!("expected NoWork, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_detects_a_refused_refresh() {
        // amt signals a refused refresh as exit 0 + claimed:false — the exit
        // code alone read that as success, so a lapsed lease looked renewed
        // and two agents could end up working the same issue.
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "claim"],
            0,
            r#"{"claimed":false,"reason":"lease held by sirius/rowan"}"#,
        );
        let amt = Amt::new(&m);
        let err = amt.heartbeat("AMT-7", "sirius/oak").unwrap_err();
        assert!(
            matches!(&err, HeartbeatError::Refused(r) if r.contains("sirius/rowan")),
            "{err}"
        );
        assert!(err.to_string().contains("refused"), "{err}");
        // A healthy refresh (the issue object back) is Ok.
        m.expect(&["amt", "--json", "claim"], 0, r#"{"id":"AMT-7"}"#);
        assert!(amt.heartbeat("AMT-7", "sirius/oak").is_ok());
    }

    #[test]
    fn decide_extracts_decision_id() {
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "decide"],
            0,
            r#"{"id":"D-3","resolves":"AMT-7"}"#,
        );
        let amt = Amt::new(&m);
        assert_eq!(amt.decide("AMT-7", "why", "body").unwrap(), "D-3");
    }

    #[test]
    fn comment_uses_issue_comment_form() {
        let m = MockRunner::new();
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        let amt = Amt::new(&m);
        amt.comment("AMT-7", "linked a,b").unwrap();
        assert_eq!(
            m.recorded()[0],
            "amt --json issue comment AMT-7 -m linked a,b"
        );
    }

    #[test]
    fn error_exit_is_surfaced() {
        let m = MockRunner::new();
        m.push(crate::shell::MockResponse::new(
            &["amt", "--json", "issue", "show"],
            1,
            "",
            "no such issue",
        ));
        let amt = Amt::new(&m);
        assert!(amt
            .issue_show("AMT-99")
            .unwrap_err()
            .contains("no such issue"));
    }
}
