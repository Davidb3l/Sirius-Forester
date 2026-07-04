//! The Hayvenhurst (`hayven`) boundary.
//!
//! All Hayvenhurst reads/writes go through the `hayven` CLI (PRD §2.2). Flags
//! here are the ground-truth `hayven 0.0.5` forms (see CONTRACTS §6). Notable
//! deltas from the PRD/CONTRACTS §4 intent:
//!   * `hayven claim <ids...> --intent "..." [--force]` — ids are POSITIONAL,
//!     there is NO `--agent` flag (agent is derived by the daemon).
//!   * `hayven remember "<note>" [--node <id>] [--kind K] [--scope a,b]` — the
//!     note is the FIRST positional arg.
//!   * `hayven affected-tests <symbol> [--changed a,b] [--trace-only] [--json]`
//!     has NO `--gate`/`--gate-tier` flags. Sirius synthesizes the gate verdict
//!     from the command's exit code (0 = pass, 1 = fail), matching the exit-code
//!     contract in PRD §6 fact 4.
//!   * The daemon on :7777 is single-project-bound: a call against a workspace
//!     whose daemon isn't the one on :7777 fails (exit 1, "serves a DIFFERENT
//!     project"). `hayven claim` exit codes: 0 registered, 1 hard overlap (409),
//!     3 soft oracle adjacency (202).

use crate::shell::{CmdOutput, Runner};
use serde_json::Value;

/// Result of a `hayven claim`, mapped from exit codes.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimVerdict {
    /// exit 0 — claim registered.
    Registered { claim_id: Option<String> },
    /// exit 1 — hard overlap (409). `detail` names the blocking claim if known.
    Overlap { detail: String },
    /// exit 3 — soft oracle adjacency conflict (202); force-able.
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

    /// `hayven claim <ids...> --intent "<intent>" [--force]`.
    ///
    /// Exit-code → verdict mapping is the PRD §9 contract:
    /// 0 registered · 1 hard overlap (409) · 3 oracle adjacency (202).
    pub fn claim(&self, ids: &[String], intent: &str, force: bool) -> ClaimVerdict {
        let mut args: Vec<&str> = Vec::new();
        args.push("claim");
        for id in ids {
            args.push(id.as_str());
        }
        args.push("--intent");
        args.push(intent);
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
        let detail = combined(&out).trim().to_string();
        match out.code_or_err() {
            0 => {
                // Try to lift a claim id out of any JSON on stdout.
                let claim_id = serde_json::from_str::<Value>(&out.stdout)
                    .ok()
                    .and_then(|v| {
                        v.get("id")
                            .or_else(|| v.get("claim_id"))
                            .or_else(|| v.get("claimId"))
                            .and_then(Value::as_str)
                            .map(|s| s.to_string())
                    });
                ClaimVerdict::Registered { claim_id }
            }
            1 => ClaimVerdict::Overlap { detail },
            3 => ClaimVerdict::OracleConflict { detail },
            _ => ClaimVerdict::Error { detail },
        }
    }

    /// `hayven release <claim_id>`
    pub fn release(&self, claim_id: &str) -> Result<(), String> {
        let out = self
            .runner
            .run("hayven", &["release", claim_id])
            .map_err(|e| e.to_string())?;
        if out.success() {
            Ok(())
        } else {
            Err(combined(&out).trim().to_string())
        }
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

    /// `hayven affected-tests <symbol> [--changed a,b] --json`.
    ///
    /// Returns `(selected_test_count, passed)`. Because the real CLI has no
    /// `--gate` flag, the gate verdict is the command's exit code: 0 = pass,
    /// non-zero = fail. When it succeeds we also count selected tests from JSON.
    pub fn affected_tests(
        &self,
        symbol: &str,
        changed: Option<&[String]>,
    ) -> Result<AffectedTests, String> {
        let mut args: Vec<String> = vec!["affected-tests".into(), symbol.into()];
        if let Some(c) = changed {
            if !c.is_empty() {
                args.push("--changed".into());
                args.push(c.join(","));
            }
        }
        args.push("--json".into());
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = self
            .runner
            .run("hayven", &argv)
            .map_err(|e| e.to_string())?;
        let passed = out.success();
        let selected = serde_json::from_str::<Value>(&out.stdout)
            .ok()
            .map(|v| count_tests(&v))
            .unwrap_or(0);
        Ok(AffectedTests {
            selected,
            passed,
            detail: combined(&out).trim().to_string(),
        })
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

/// The parsed result of `affected-tests`.
#[derive(Debug, Clone)]
pub struct AffectedTests {
    pub selected: usize,
    pub passed: bool,
    pub detail: String,
}

/// Best-effort count of selected tests from an affected-tests JSON payload.
fn count_tests(v: &Value) -> usize {
    for key in ["tests", "selected", "affected", "hits"] {
        if let Some(arr) = v.get(key).and_then(Value::as_array) {
            return arr.len();
        }
    }
    if let Some(n) = v.get("count").and_then(Value::as_u64) {
        return n as usize;
    }
    if let Some(arr) = v.as_array() {
        return arr.len();
    }
    0
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

    #[test]
    fn claim_exit0_is_registered_with_id() {
        let m = MockRunner::new();
        m.expect(&["hayven", "claim"], 0, r#"{"id":"clm_123"}"#);
        let h = Hayven::new(&m);
        let v = h.claim(&["a".into(), "b".into()], "AMT-7: t", false);
        assert_eq!(
            v,
            ClaimVerdict::Registered {
                claim_id: Some("clm_123".into())
            }
        );
        // Positional ids, --intent, no --agent.
        assert_eq!(m.recorded()[0], "hayven claim a b --intent AMT-7: t");
    }

    #[test]
    fn claim_exit1_is_overlap() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "claim"],
            1,
            "",
            "409 overlap: held by other/agent",
        ));
        let h = Hayven::new(&m);
        match h.claim(&["a".into()], "i", false) {
            ClaimVerdict::Overlap { detail } => assert!(detail.contains("409")),
            other => panic!("expected Overlap, got {other:?}"),
        }
    }

    #[test]
    fn claim_exit3_is_oracle_conflict() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "claim"],
            3,
            "",
            "202 adjacency verdict",
        ));
        let h = Hayven::new(&m);
        assert!(matches!(
            h.claim(&["a".into()], "i", false),
            ClaimVerdict::OracleConflict { .. }
        ));
    }

    #[test]
    fn affected_tests_pass_counts_selected() {
        let m = MockRunner::new();
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"tests":["t1","t2","t3"]}"#,
        );
        let h = Hayven::new(&m);
        let r = h.affected_tests("sym", Some(&["a".into()])).unwrap();
        assert!(r.passed);
        assert_eq!(r.selected, 3);
    }

    #[test]
    fn affected_tests_fail_on_nonzero_exit() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "affected-tests"],
            1,
            "",
            "fail",
        ));
        let h = Hayven::new(&m);
        let r = h.affected_tests("sym", None).unwrap();
        assert!(!r.passed);
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
