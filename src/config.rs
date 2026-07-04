//! `.sirius/config.json` — the policy engine (CONTRACTS §3, PRD §F5).
//!
//! Absent file ⇒ committed defaults. Every enforcement point is opt-out-able
//! (PRD §2.5). Sirius reads it; the Console displays it read-only.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Backoff409 {
    /// Currently only "release_and_comment" is honored by the loop.
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_base_ms")]
    pub base_ms: u64,
    #[serde(default = "default_max_ms")]
    pub max_ms: u64,
}

fn default_strategy() -> String {
    "release_and_comment".into()
}
fn default_base_ms() -> u64 {
    500
}
fn default_max_ms() -> u64 {
    8000
}

impl Default for Backoff409 {
    fn default() -> Self {
        Backoff409 {
            strategy: default_strategy(),
            base_ms: default_base_ms(),
            max_ms: default_max_ms(),
        }
    }
}

/// Oracle-202 (soft adjacency conflict) handling.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Oracle202 {
    /// Back off: release entity, do not force.
    #[default]
    BackOff,
    /// Force the claim, spending from the force budget.
    ForceWithBudget,
}

/// Contention-adaptive claiming mode (M5).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimMode {
    /// Always pre-emptively claim entities before work.
    Always,
    /// Never pre-claim; rely on the gate to catch collisions.
    Never,
    /// Decide per-iteration from ledger contention history.
    #[default]
    Adaptive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_true")]
    pub claim_order_enforced: bool,
    #[serde(default)]
    pub backoff_409: Backoff409,
    #[serde(default)]
    pub oracle_202: Oracle202,
    #[serde(default)]
    pub force_budget_tokens: u64,
    #[serde(default = "default_gate_tier")]
    pub gate_tier: String,
    #[serde(default = "default_target_status")]
    pub target_status: String,
    #[serde(default = "default_retry_budget")]
    pub retry_budget: u32,
    #[serde(default = "default_worker_concurrency")]
    pub worker_concurrency: u32,
    #[serde(default)]
    pub claim_mode: ClaimMode,
}

fn default_true() -> bool {
    true
}
fn default_gate_tier() -> String {
    "safe".into()
}
fn default_target_status() -> String {
    "in_review".into()
}
fn default_retry_budget() -> u32 {
    3
}
fn default_worker_concurrency() -> u32 {
    3
}

impl Default for Config {
    fn default() -> Self {
        Config {
            claim_order_enforced: true,
            backoff_409: Backoff409::default(),
            oracle_202: Oracle202::default(),
            force_budget_tokens: 0,
            gate_tier: default_gate_tier(),
            target_status: default_target_status(),
            retry_budget: default_retry_budget(),
            worker_concurrency: default_worker_concurrency(),
            claim_mode: ClaimMode::default(),
        }
    }
}

impl Config {
    /// Load from a path, falling back to defaults if the file is absent.
    /// A malformed file is a hard error (returned as a message).
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                serde_json::from_str(&s).map_err(|e| format!("invalid {}: {e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// The committed-defaults JSON, pretty-printed. Used by `sirius init` to
    /// write a starter config, and as documentation.
    pub fn default_json() -> String {
        serde_json::to_string_pretty(&Config::default()).unwrap()
    }

    /// Compute exponential backoff for the Nth consecutive 409, clamped to
    /// `[base_ms, max_ms]`.
    pub fn backoff_delay_ms(&self, attempt: u32) -> u64 {
        let base = self.backoff_409.base_ms;
        let factor = 1u64 << attempt.min(20);
        (base.saturating_mul(factor)).min(self.backoff_409.max_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_contracts_section_3() {
        let c = Config::default();
        assert!(c.claim_order_enforced);
        assert_eq!(c.backoff_409.strategy, "release_and_comment");
        assert_eq!(c.backoff_409.base_ms, 500);
        assert_eq!(c.backoff_409.max_ms, 8000);
        assert_eq!(c.oracle_202, Oracle202::BackOff);
        assert_eq!(c.force_budget_tokens, 0);
        assert_eq!(c.gate_tier, "safe");
        assert_eq!(c.target_status, "in_review");
        assert_eq!(c.retry_budget, 3);
        assert_eq!(c.worker_concurrency, 3);
        assert_eq!(c.claim_mode, ClaimMode::Adaptive);
    }

    #[test]
    fn absent_file_yields_defaults() {
        let p = std::env::temp_dir().join("sirius-nonexistent-config-xyz.json");
        let _ = std::fs::remove_file(&p);
        let c = Config::load(&p).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn partial_file_fills_defaults() {
        let json = r#"{ "gate_tier": "observed", "claim_mode": "never" }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.gate_tier, "observed");
        assert_eq!(c.claim_mode, ClaimMode::Never);
        // Unspecified fields keep defaults.
        assert_eq!(c.retry_budget, 3);
        assert!(c.claim_order_enforced);
    }

    #[test]
    fn default_json_roundtrips() {
        let json = Config::default_json();
        let c: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn backoff_is_exponential_and_clamped() {
        let c = Config::default();
        assert_eq!(c.backoff_delay_ms(0), 500);
        assert_eq!(c.backoff_delay_ms(1), 1000);
        assert_eq!(c.backoff_delay_ms(2), 2000);
        assert_eq!(c.backoff_delay_ms(3), 4000);
        assert_eq!(c.backoff_delay_ms(4), 8000);
        assert_eq!(c.backoff_delay_ms(10), 8000); // clamped to max
    }
}
