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

/// What the gate does when it cannot trust the selector to be complete
/// (empty/stale selection, unparseable output, a hub/config change). The
/// governing rule is "ran too much, never missed a test" — hence the default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateFallback {
    /// Run the entire suite (never miss a test). The default and only safe posture.
    #[default]
    FullSuite,
    /// Block the gate (fail) rather than run everything — for suites too slow to
    /// run in full, accepting that an un-selectable change cannot pass.
    Fail,
    /// Advance with a warning comment — explicitly opting into the miss risk.
    PassWithWarning,
}

/// The Gate's runner config (SIRF-5 / D-3). `hayven affected-tests` only
/// *selects* tests; Sirius runs them. `test_cmd` is the full-suite command;
/// when a trustworthy narrow selection exists its ids are appended.
///
/// SF-11 naming note: `fallback` is NOT renamed and `full-suite` keeps its
/// spelling. The ticket is right that `"fallback": "full-suite"` beside
/// `"test_cmd": null` reads as a promise to run everything when there is
/// nothing to run — but the two fields are orthogonal (`fallback` picks the
/// SELECTION policy, `test_cmd` supplies the RUNNER), and every
/// `.sirius/config.json` in the field spells them this way. Renaming the key,
/// or accepting a second spelling for it, buys clarity in one file at the cost
/// of two valid names for one knob forever. The nonsense *state* is what got
/// fixed instead: `sirius init` now writes a detected `test_cmd` (see
/// [`Config::default_json_for_root`]) so the pair is coherent from the first
/// run, and `sirius doctor`'s `gate_configured` fact fails loudly when it is
/// not. A rename remains available as a deliberate breaking change.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateConfig {
    /// Full-suite test command, run verbatim through a shell (e.g.
    /// `cargo test`, `bun test`, `pytest -q`) — see `shell::resolve_shell` for
    /// WHICH shell. Unset ⇒ the gate cannot run tests and refuses to pass
    /// (fail-closed). Backward compatible: `"test_cmd": null` still parses,
    /// which is the shape every workspace initialised before SF-11 has on disk.
    #[serde(default)]
    pub test_cmd: Option<String>,
    /// Behavior when the selection cannot be trusted to be complete.
    #[serde(default)]
    pub fallback: GateFallback,
}

/// What to do when the review stage cannot reach a clean verdict (SIRF-23).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewEscalation {
    /// Advance to `target_status` anyway, label the issue `review:open`, and
    /// comment every unresolved confirmed finding. The work is never hidden or
    /// thrown away, and the human sees exactly what is left.
    #[default]
    AdvanceFlagged,
    /// Release back to `todo` with the findings attached.
    Release,
}

/// Which tree the reviewer looks at (SIRF-23).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewAgainst {
    /// The plain diff against the fleet's launch base.
    LaunchBase,
    /// The issue's work merged onto the CURRENT tip of `base_ref` in a
    /// throwaway worktree — catches bugs that only appear when the branch
    /// meets work merged later in the same run. A merge conflict becomes a
    /// blocking `conflict` finding.
    #[default]
    CurrentBaseMerge,
    /// SIRF-30: the work merged onto the integration frontier — the current
    /// base tip plus every in-flight sibling branch awaiting integration — so
    /// a change is reviewed as it will actually land. A conflict with a
    /// sibling is a `sibling-conflict` finding; one with the base, `conflict`.
    Frontier,
}

/// SIRF-31: a directory whose entries form an ordered sequence (migrations).
/// Two branches claiming the same slot fork the chain — a fact no per-branch
/// reviewer can see, so Sirius checks it mechanically.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SequenceSpec {
    /// Repo-relative directory whose DIRECT children are the sequence.
    pub dir: String,
    /// Regex whose capture group 1 is an entry's key; non-matching entries
    /// (e.g. a `meta/` folder) are not part of the sequence.
    #[serde(default = "default_sequence_key")]
    pub key: String,
}

fn default_sequence_key() -> String {
    r"^(\d+)".into()
}

/// The review stage (SIRF-23): an unbiased fresh-eyes review after the gate,
/// with an automatic fix loop. `cmd: None` turns the stage off entirely —
/// today's behavior, byte for byte.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewConfig {
    /// The reviewer command, run via `sh -c` in a fresh process (e.g.
    /// `claude -p "$(cat "$SIRIUS_REVIEW_PROMPT")" --model <other>`).
    /// Overridden by `sirius run --review-cmd`. `None` = stage off.
    #[serde(default)]
    pub cmd: Option<String>,
    /// An OVERRIDE for the reviewer prompt template; when the file is absent
    /// the built-in default is used (and improves with each release). Sirius
    /// renders `$SIRIUS_*` placeholders per round and hands the rendered file
    /// over as `SIRIUS_REVIEW_PROMPT`.
    #[serde(default = "default_review_prompt_file")]
    pub prompt_file: String,
    /// Max review runs per issue (fix rounds = max_rounds - 1).
    #[serde(default = "default_review_max_rounds")]
    pub max_rounds: u32,
    /// Finding kinds that block when `confidence == "confirmed"`.
    #[serde(default = "default_review_block_on")]
    pub block_on: Vec<String>,
    #[serde(default)]
    pub against: ReviewAgainst,
    /// The ref whose CURRENT tip `current-base-merge` merges onto. `None` =
    /// the branch HEAD pointed to when the fleet launched.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Hard wall-clock cap on one reviewer run.
    #[serde(default = "default_review_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub on_exhausted: ReviewEscalation,
    #[serde(default)]
    pub on_review_error: ReviewEscalation,
    /// A diff touching ONLY files matching these globs skips review.
    #[serde(default = "default_review_skip_paths")]
    pub skip_paths: Vec<String>,
    /// SIRF-31: sequence directories checked for collisions. Empty = off.
    #[serde(default)]
    pub sequences: Vec<SequenceSpec>,
}

fn default_review_prompt_file() -> String {
    ".sirius/review-prompt.md".into()
}
fn default_review_max_rounds() -> u32 {
    3
}
fn default_review_block_on() -> Vec<String> {
    vec!["bug".into(), "conflict".into()]
}
fn default_review_timeout_secs() -> u64 {
    1500
}
fn default_review_skip_paths() -> Vec<String> {
    vec!["**/*.md".into(), "docs/**".into()]
}

impl Default for ReviewConfig {
    fn default() -> Self {
        ReviewConfig {
            cmd: None,
            prompt_file: default_review_prompt_file(),
            max_rounds: default_review_max_rounds(),
            block_on: default_review_block_on(),
            against: ReviewAgainst::default(),
            base_ref: None,
            timeout_secs: default_review_timeout_secs(),
            on_exhausted: ReviewEscalation::default(),
            on_review_error: ReviewEscalation::default(),
            skip_paths: default_review_skip_paths(),
            sequences: Vec::new(),
        }
    }
}

// ── Ecosystem detection (SF-11) ─────────────────────────────────────────────

/// A `gate.test_cmd` suggestion, with the file it was inferred from so the
/// operator can check our reasoning rather than trust it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedTestCmd {
    /// The suggested command, e.g. `cargo test --workspace`.
    pub cmd: String,
    /// The workspace-root file that produced it, e.g. `Cargo.toml`.
    pub from: String,
}

/// Infer a full-suite test command from what is present at a workspace root.
///
/// Pure over `root` — it reads only `root/<marker>`, never walks up and never
/// consults the process cwd — so it is unit-testable against a temp fixture
/// instead of against whatever repo the test binary happens to be built in.
///
/// Order is deliberate: Rust first (a `Cargo.toml` at the root means the crate
/// IS the project), then JS, then Python, then Go. A polyglot repo gets the
/// first match; the suggestion is a starting point a human edits, never a
/// silently-applied default.
pub fn detect_test_cmd(root: &Path) -> Option<DetectedTestCmd> {
    let has = |name: &str| root.join(name).exists();
    let detected = |cmd: &str, from: &str| {
        Some(DetectedTestCmd {
            cmd: cmd.to_string(),
            from: from.to_string(),
        })
    };

    if has("Cargo.toml") {
        return detected("cargo test --workspace", "Cargo.toml");
    }
    // package.json only counts when it actually declares a test script —
    // a manifest with no `scripts.test` makes `npm test` exit 1 ("missing
    // script: test"), which would hand the operator a test_cmd that fails
    // every gate closed for a reason unrelated to their code.
    if has("package.json") && package_json_has_test_script(&root.join("package.json")) {
        // Prefer bun when the lockfile proves the repo is a bun repo: `npm
        // test` in a bun workspace re-resolves against a different lockfile.
        let bun = has("bun.lock") || has("bun.lockb");
        return if bun {
            detected("bun test", "package.json + bun.lock")
        } else {
            detected("npm test", "package.json")
        };
    }
    if has("pyproject.toml") {
        return detected("pytest", "pyproject.toml");
    }
    if has("pytest.ini") {
        return detected("pytest", "pytest.ini");
    }
    if has("go.mod") {
        return detected("go test ./...", "go.mod");
    }
    None
}

/// True when `package.json` declares a `scripts.test`. Unreadable/unparseable
/// counts as false — we will not suggest a command we cannot see evidence for.
fn package_json_has_test_script(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("scripts")
                .and_then(|s| s.get("test"))
                .and_then(|t| t.as_str())
                .map(|t| !t.trim().is_empty())
        })
        .unwrap_or(false)
}

/// One sentence naming the detected command AND the file it came from, for
/// `sirius doctor`. Naming the file matters: an operator whose repo was
/// detected wrongly can see why in the same line.
pub fn test_cmd_suggestion(root: &Path) -> String {
    match detect_test_cmd(root) {
        Some(d) => format!(
            "set \"gate\": {{\"test_cmd\": \"{}\"}} in .sirius/config.json (detected from {})",
            d.cmd, d.from
        ),
        None => "set \"gate\": {\"test_cmd\": \"<your full-suite test command>\"} in \
                 .sirius/config.json (no Cargo.toml / package.json / pyproject.toml / \
                 pytest.ini / go.mod at the workspace root to infer one from)"
            .into(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_true")]
    pub claim_order_enforced: bool,
    #[serde(default)]
    pub backoff_409: Backoff409,
    #[serde(default)]
    pub oracle_202: Oracle202,
    // SIRF-9: `force_budget_tokens` was removed — the loop cannot meter an
    // agent's token spend, so the knob was never read (the `ForceWithBudget`
    // path forces unconditionally). Kept out of the struct entirely; the
    // `Oracle202::ForceWithBudget` variant and its behavior are unchanged.
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
    #[serde(default)]
    pub gate: GateConfig,
    /// SIRF-7: hard wall-clock cap on a single agent run, in seconds. On expiry
    /// the agent process is killed and the iteration fails (release without
    /// advancing + deadend note). This may safely EXCEED `lease_ttl_secs`: the
    /// heartbeat renews both leases every `heartbeat_interval_secs`
    /// (= `lease_ttl_secs / 3`) while the agent runs, so the lease never lapses
    /// mid-run however long the cap is. The invariant that actually matters is
    /// `heartbeat_interval_secs < lease_ttl_secs`, not `timeout < lease_ttl`.
    #[serde(default = "default_agent_timeout_secs")]
    pub agent_timeout_secs: u64,
    /// SIRF-7: the amt/hayven lease TTL, in seconds. The heartbeat that renews
    /// both leases fires every `lease_ttl_secs / 3` while the agent runs, so a
    /// lease can never lapse mid-run (amt's lease is 900s by contract).
    #[serde(default = "default_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    /// SIRF-23: the fresh-eyes review stage. Off unless `review.cmd` is set.
    #[serde(default)]
    pub review: ReviewConfig,
    /// SIRF-26: which model each agent / reviewer runs on. `sirius run`
    /// refuses to launch with no worker model unless explicitly allowed.
    #[serde(default)]
    pub models: crate::models::ModelsConfig,
    /// SIRF-32: `sirius integrate` — the integration command run on the
    /// frontier, and whether a red run stops the fleet.
    #[serde(default)]
    pub integration: IntegrationConfig,
}

/// What a red integration run does to the fleet (SIRF-32).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationOnFail {
    /// File the issue; the fleet keeps working.
    #[default]
    Warn,
    /// Stop the line: `sirius run` refuses to start and a running fleet stops
    /// claiming until a later `sirius integrate` is green.
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntegrationConfig {
    /// Run in the frontier tree via `sh -c`. `None` = build + report only.
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub on_fail: IntegrationOnFail,
    #[serde(default = "default_integration_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_integration_timeout_secs() -> u64 {
    1800
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        IntegrationConfig {
            cmd: None,
            on_fail: IntegrationOnFail::default(),
            timeout_secs: default_integration_timeout_secs(),
        }
    }
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
fn default_agent_timeout_secs() -> u64 {
    // 30 min: generous for a real agent run. It intentionally exceeds the 900s
    // lease — the heartbeat (not this cap) keeps the lease alive by renewing it
    // every lease_ttl/3; this cap only bounds a hung/runaway agent. (SIRF-7)
    1800
}
fn default_lease_ttl_secs() -> u64 {
    // amt's claim lease is 900s by contract (see amt.rs claim/heartbeat).
    900
}

impl Default for Config {
    fn default() -> Self {
        Config {
            claim_order_enforced: true,
            backoff_409: Backoff409::default(),
            oracle_202: Oracle202::default(),
            gate_tier: default_gate_tier(),
            target_status: default_target_status(),
            retry_budget: default_retry_budget(),
            worker_concurrency: default_worker_concurrency(),
            claim_mode: ClaimMode::default(),
            gate: GateConfig::default(),
            agent_timeout_secs: default_agent_timeout_secs(),
            lease_ttl_secs: default_lease_ttl_secs(),
            review: ReviewConfig::default(),
            integration: IntegrationConfig::default(),
            models: crate::models::ModelsConfig::default(),
        }
    }
}

impl Config {
    /// SIRF-7: the interval at which the loop renews both leases while the agent
    /// runs — `lease_ttl_secs / 3`, floored at 1s so it is never zero.
    pub fn heartbeat_interval_secs(&self) -> u64 {
        (self.lease_ttl_secs / 3).max(1)
    }
}

impl Config {
    /// Load from a path, falling back to defaults if the file is absent.
    /// A malformed file is a hard error (returned as a message).
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let c: Config = serde_json::from_str(&s)
                    .map_err(|e| format!("invalid {}: {e}", path.display()))?;
                c.validate()
                    .map_err(|e| format!("invalid {}: {e}", path.display()))?;
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Reject configs that would silently disable a check (SIRF-31: a
    /// sequence key without a capture group never matches anything).
    pub fn validate(&self) -> Result<(), String> {
        for s in &self.review.sequences {
            let re = regex::Regex::new(&s.key)
                .map_err(|e| format!("review.sequences key `{}`: {e}", s.key))?;
            if re.captures_len() < 2 {
                return Err(format!(
                    "review.sequences key `{}` has no capture group — wrap the key part in ( )",
                    s.key
                ));
            }
        }
        Ok(())
    }

    /// SF-11: the starter config for `sirius init`, with `gate.test_cmd`
    /// PRE-FILLED from whatever ecosystem `root` looks like.
    ///
    /// A null `test_cmd` makes the very first gate on a fresh workspace fail
    /// closed — correct, but the operator only learns it after an agent has
    /// already done real work and the issue is stranded in `in_progress`.
    /// Writing a detected command turns that into a config an operator
    /// *corrects* rather than one they must discover. When nothing is
    /// detectable the field stays null and `sirius doctor` says so.
    ///
    /// Called by `cmd_init` in src/main.rs, which is what makes a fresh
    /// workspace gateable instead of inert.
    pub fn default_json_for_root(root: &Path) -> String {
        let mut c = Config::default();
        c.gate.test_cmd = detect_test_cmd(root).map(|d| d.cmd);
        serde_json::to_string_pretty(&c).unwrap()
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
        assert_eq!(c.gate_tier, "safe");
        assert_eq!(c.target_status, "in_review");
        assert_eq!(c.retry_budget, 3);
        assert_eq!(c.worker_concurrency, 3);
        assert_eq!(c.claim_mode, ClaimMode::Adaptive);
        // Gate: no test command by default (fail-closed), full-suite fallback.
        assert_eq!(c.gate.test_cmd, None);
        assert_eq!(c.gate.fallback, GateFallback::FullSuite);
        // SIRF-7: agent timeout well below the lease, heartbeat at lease/3.
        assert_eq!(c.agent_timeout_secs, 1800);
        assert_eq!(c.lease_ttl_secs, 900);
        assert_eq!(c.heartbeat_interval_secs(), 300);
        // SIRF-23: the review stage is OFF by default (identical to the
        // pre-review loop) with the spec's defaults ready when it is enabled.
        assert_eq!(c.review.cmd, None);
        assert_eq!(c.review.max_rounds, 3);
        assert_eq!(c.review.block_on, vec!["bug", "conflict"]);
        assert_eq!(c.review.against, ReviewAgainst::CurrentBaseMerge);
        assert_eq!(c.review.on_exhausted, ReviewEscalation::AdvanceFlagged);
        assert_eq!(c.review.on_review_error, ReviewEscalation::AdvanceFlagged);
        assert_eq!(c.review.timeout_secs, 1500);
    }

    #[test]
    fn frontier_and_sequences_parse() {
        let c: Config = serde_json::from_str(
            r#"{"review":{"against":"frontier","sequences":[{"dir":"drizzle"},{"dir":"db/m","key":"^V(\\d+)__"}]}}"#,
        )
        .unwrap();
        assert_eq!(c.review.against, ReviewAgainst::Frontier);
        assert_eq!(c.review.sequences[0].key, r"^(\d+)");
        assert_eq!(c.review.sequences[1].key, r"^V(\d+)__");
        assert!(
            Config::default().review.sequences.is_empty(),
            "off by default"
        );
        let bad: Config =
            serde_json::from_str(r#"{"review":{"sequences":[{"dir":"m","key":"^\\d+"}]}}"#)
                .unwrap();
        assert!(bad.validate().unwrap_err().contains("capture group"));
    }

    #[test]
    fn review_block_parses_kebab_case_and_fills_defaults() {
        let c: Config = serde_json::from_str(
            r#"{"review":{"cmd":"rev","against":"launch-base","on_exhausted":"release"}}"#,
        )
        .unwrap();
        assert_eq!(c.review.cmd.as_deref(), Some("rev"));
        assert_eq!(c.review.against, ReviewAgainst::LaunchBase);
        assert_eq!(c.review.on_exhausted, ReviewEscalation::Release);
        // Unspecified fields fall back to defaults.
        assert_eq!(c.review.max_rounds, 3);
        assert_eq!(c.review.on_review_error, ReviewEscalation::AdvanceFlagged);
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

    /// A root with no recognisable ecosystem yields exactly the committed
    /// defaults — so `sirius init` never invents a test command it could not
    /// detect, and the starter config still round-trips.
    #[test]
    fn default_json_roundtrips() {
        let root = fixture("roundtrip");
        let json = Config::default_json_for_root(&root);
        let c: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c, Config::default());
    }

    // ---- SF-11 ecosystem detection ----------------------------------------

    /// Unique temp dir; every detection test builds its own fixture so nothing
    /// depends on the layout of the repo the test binary was built in.
    fn fixture(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("sirius-detect-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn detects_cargo_workspace() {
        let d = fixture("cargo");
        touch(&d, "Cargo.toml", "[package]\nname=\"x\"\n");
        let got = detect_test_cmd(&d).unwrap();
        assert_eq!(got.cmd, "cargo test --workspace");
        assert_eq!(got.from, "Cargo.toml");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detects_npm_from_a_declared_test_script() {
        let d = fixture("npm");
        touch(&d, "package.json", r#"{"scripts":{"test":"vitest run"}}"#);
        let got = detect_test_cmd(&d).unwrap();
        assert_eq!(got.cmd, "npm test");
        assert_eq!(got.from, "package.json");
        let _ = std::fs::remove_dir_all(&d);
    }

    // A bun lockfile means the repo resolves with bun; `npm test` there would
    // re-resolve against a different lockfile.
    #[test]
    fn prefers_bun_when_a_bun_lockfile_is_present() {
        for lock in ["bun.lock", "bun.lockb"] {
            let d = fixture("bun");
            touch(&d, "package.json", r#"{"scripts":{"test":"bun test"}}"#);
            touch(&d, lock, "");
            let got = detect_test_cmd(&d).unwrap();
            assert_eq!(got.cmd, "bun test", "lockfile {lock}");
            assert!(got.from.contains("bun.lock"), "{}", got.from);
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    // `npm test` with no `scripts.test` exits 1 ("missing script: test"), which
    // would fail every gate closed for a reason that has nothing to do with the
    // agent's code. Suggest nothing rather than something broken.
    #[test]
    fn package_json_without_a_test_script_is_not_detected() {
        let d = fixture("noscript");
        touch(
            &d,
            "package.json",
            r#"{"name":"x","scripts":{"build":"tsc"}}"#,
        );
        assert_eq!(detect_test_cmd(&d), None);
        // Same for an empty test script and for unparseable JSON.
        touch(&d, "package.json", r#"{"scripts":{"test":"  "}}"#);
        assert_eq!(detect_test_cmd(&d), None);
        touch(&d, "package.json", "{ not json");
        assert_eq!(detect_test_cmd(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detects_python_and_go() {
        let d = fixture("py");
        touch(&d, "pyproject.toml", "[project]\nname=\"x\"\n");
        assert_eq!(detect_test_cmd(&d).unwrap().cmd, "pytest");
        let _ = std::fs::remove_dir_all(&d);

        let d = fixture("pyini");
        touch(&d, "pytest.ini", "[pytest]\n");
        let got = detect_test_cmd(&d).unwrap();
        assert_eq!(
            (got.cmd.as_str(), got.from.as_str()),
            ("pytest", "pytest.ini")
        );
        let _ = std::fs::remove_dir_all(&d);

        let d = fixture("go");
        touch(&d, "go.mod", "module x\n");
        assert_eq!(detect_test_cmd(&d).unwrap().cmd, "go test ./...");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn undetectable_root_yields_no_suggestion_but_still_explains() {
        let d = fixture("bare");
        assert_eq!(detect_test_cmd(&d), None);
        let s = test_cmd_suggestion(&d);
        assert!(s.contains("gate"), "{s}");
        assert!(s.contains("no Cargo.toml"), "{s}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn suggestion_names_the_file_it_was_detected_from() {
        let d = fixture("sugg");
        touch(&d, "Cargo.toml", "[package]\nname=\"x\"\n");
        let s = test_cmd_suggestion(&d);
        assert!(s.contains("cargo test --workspace"), "{s}");
        assert!(s.contains("detected from Cargo.toml"), "{s}");
        let _ = std::fs::remove_dir_all(&d);
    }

    // SF-11: `sirius init` must not mint the inert `test_cmd: null` config in a
    // repo whose ecosystem is obvious.
    #[test]
    fn init_json_prefills_a_detected_test_cmd() {
        let d = fixture("init");
        touch(&d, "Cargo.toml", "[package]\nname=\"x\"\n");
        let c: Config = serde_json::from_str(&Config::default_json_for_root(&d)).unwrap();
        assert_eq!(c.gate.test_cmd.as_deref(), Some("cargo test --workspace"));
        // Everything else stays at the committed defaults.
        assert_eq!(c.gate.fallback, GateFallback::FullSuite);
        assert_eq!(
            Config {
                gate: GateConfig::default(),
                ..c.clone()
            },
            Config::default()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn init_json_leaves_test_cmd_null_when_nothing_is_detectable() {
        let d = fixture("initbare");
        let c: Config = serde_json::from_str(&Config::default_json_for_root(&d)).unwrap();
        assert_eq!(c.gate.test_cmd, None);
        let _ = std::fs::remove_dir_all(&d);
    }

    // Backward compatibility is load-bearing: every workspace initialised
    // before SF-11 has `"test_cmd": null` on disk and must keep parsing.
    #[test]
    fn legacy_null_test_cmd_still_parses() {
        let json = r#"{"gate":{"test_cmd":null,"fallback":"full-suite"}}"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.gate.test_cmd, None);
        assert_eq!(c.gate.fallback, GateFallback::FullSuite);
        // …as does a gate object that omits the key entirely.
        let c2: Config = serde_json::from_str(r#"{"gate":{}}"#).unwrap();
        assert_eq!(c2.gate, GateConfig::default());
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
