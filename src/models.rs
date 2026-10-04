//! Model selection for fleet agents (SIRF-26) — the pure half.
//!
//! Incident (2026-10-03, Lydgr): neither `--agent-cmd` nor `review.cmd`
//! named a model, so every headless `claude -p` silently used the GLOBAL
//! default in `~/.claude/settings.json` (`fable[1m]` → claude-fable-5 with 1M
//! context) — ~100 sessions for hours, until the weekly limit hit, after which
//! the loop bounced tickets back to `todo` in seconds.
//!
//! So the fleet's model is EXPLICIT and VISIBLE:
//!   * resolved once at launch: `--model` > `models.default` >
//!     `$SIRIUS_PARENT_MODEL` > none — and "none" is refused unless
//!     `--allow-default-model`, with the model workers WOULD get named;
//!   * routed per TICKET by its Ametrite labels (first matching route wins);
//!     fix rounds of un-routed tickets never drop below `models.fix_floor`;
//!   * injected as `ANTHROPIC_MODEL` (Claude Code honors it over settings.json
//!     — verified) plus `SIRIUS_MODEL` / `SIRIUS_REVIEW_MODEL` and a `{model}`
//!     placeholder for non-Claude agents;
//!   * a usage-limit message in a failed agent's output pauses the fleet.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Route a ticket to a model by its labels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRoute {
    /// Any of these labels (case-insensitive) selects `model`.
    pub labels: Vec<String>,
    pub model: String,
}

/// `.sirius/config.json` → `models`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsConfig {
    /// The worker model when no route matches. `--model` overrides it.
    #[serde(default)]
    pub default: Option<String>,
    /// Per-ticket routing by Ametrite label; first match wins.
    #[serde(default)]
    pub routes: Vec<ModelRoute>,
    /// Fix rounds answer a reviewer's CONFIRMED findings — the subtle cases —
    /// so an un-routed ticket's fix round uses at least this model.
    #[serde(default)]
    pub fix_floor: Option<String>,
    /// The reviewer model (ideally NOT the workers' — a different model does
    /// not share the author's blind spots). Falls back to `default`.
    /// `--review-model` overrides it.
    #[serde(default)]
    pub review: Option<String>,
    /// Allow launching with NO resolved worker model (agents then use
    /// whatever their CLI defaults to). Same as `--allow-default-model`.
    #[serde(default)]
    pub allow_default: bool,
}

/// What `--model` / `--review-model` may say: an explicit id, or `inherit`
/// (read `$SIRIUS_PARENT_MODEL`, set by a launching session).
fn flag_value(
    flag: Option<&str>,
    parent: Option<&str>,
    name: &str,
) -> Result<Option<String>, String> {
    match flag.map(str::trim) {
        None | Some("") => Ok(None),
        Some("inherit") => parent.map(|p| Some(p.to_string())).ok_or_else(|| {
            format!(
                "{name} inherit: $SIRIUS_PARENT_MODEL is not set — pass the exact model id instead"
            )
        }),
        Some(m) => Ok(Some(m.to_string())),
    }
}

/// Resolve the launch-time models into `cfg` (precedence: flag > config >
/// `$SIRIUS_PARENT_MODEL` > none). Returns where the worker default came from.
pub fn resolve(
    cfg: &mut ModelsConfig,
    flag_model: Option<&str>,
    flag_review: Option<&str>,
    allow_default_flag: bool,
    parent_model: Option<&str>,
) -> Result<&'static str, String> {
    let parent = parent_model.map(str::trim).filter(|p| !p.is_empty());
    let source;
    if let Some(m) = flag_value(flag_model, parent, "--model")? {
        cfg.default = Some(m);
        source = "--model";
    } else if cfg.default.as_deref().is_some_and(|d| !d.trim().is_empty()) {
        source = "config models.default";
    } else if let Some(p) = parent {
        cfg.default = Some(p.to_string());
        source = "$SIRIUS_PARENT_MODEL";
    } else {
        cfg.default = None;
        source = "none";
    }
    if let Some(r) = flag_value(flag_review, parent, "--review-model")? {
        cfg.review = Some(r);
    }
    // An empty string is "unset", everywhere — `ANTHROPIC_MODEL=""` would
    // silently fall back to the CLI default (the incident, by another door).
    let blank = |m: &Option<String>| m.as_deref().is_some_and(|x| x.trim().is_empty());
    if blank(&cfg.review) {
        cfg.review = None;
    }
    if blank(&cfg.fix_floor) {
        cfg.fix_floor = None;
    }
    cfg.routes.retain(|r| !r.model.trim().is_empty());
    cfg.allow_default |= allow_default_flag;
    Ok(source)
}

/// The model a ticket's agent runs on in `phase` (`work` | `fix`).
pub fn model_for(cfg: &ModelsConfig, labels: &[String], phase: &str) -> Option<String> {
    let routed = cfg.routes.iter().find(|r| {
        r.labels.iter().any(|want| {
            labels
                .iter()
                .any(|have| have.trim().eq_ignore_ascii_case(want.trim()))
        })
    });
    match routed {
        Some(r) => Some(r.model.clone()),
        None if phase == "fix" => cfg.fix_floor.clone().or_else(|| cfg.default.clone()),
        None => cfg.default.clone(),
    }
}

/// The reviewer's model: `models.review`, else the worker default.
pub fn review_model(cfg: &ModelsConfig) -> Option<String> {
    cfg.review.clone().or_else(|| cfg.default.clone())
}

/// Every model id the config names (for alias checks / display).
pub fn named_models(cfg: &ModelsConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |m: &Option<String>| {
        if let Some(m) = m {
            if !out.contains(m) {
                out.push(m.clone());
            }
        }
    };
    push(&cfg.default);
    push(&cfg.fix_floor);
    push(&cfg.review);
    for r in &cfg.routes {
        push(&Some(r.model.clone()));
    }
    out
}

/// Does this look like an ALIAS rather than an explicit id? Aliases resolve
/// silently (`fable[1m]` became claude-fable-5 in the incident), so they are
/// worth a warning. `claude-…` ids and any id with a version-ish `-` pass.
pub fn looks_like_alias(model: &str) -> bool {
    let m = model.trim();
    m.contains('[') || !m.contains('-')
}

/// The model a Claude agent with NO override would use: the `model` key of
/// `.claude/settings.local.json`, then `.claude/settings.json` (project),
/// then `~/.claude/settings.json` (user) — Claude Code's precedence.
pub fn claude_default_model(root: &Path, home: Option<&Path>) -> Option<(String, String)> {
    let mut candidates = vec![
        root.join(".claude").join("settings.local.json"),
        root.join(".claude").join("settings.json"),
    ];
    if let Some(h) = home {
        candidates.push(h.join(".claude").join("settings.json"));
    }
    for p in candidates {
        let model = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
        if let Some(m) = model {
            return Some((m, p.display().to_string()));
        }
    }
    None
}

/// Find a usage/plan-limit message in a FAILED agent's output. Only the LAST
/// few non-empty lines count (Sirius's `[sirius] agent exit` trailer
/// skipped): a CLI stopped by a limit prints that and exits, while the same
/// words EARLIER in a log are just content — a wrapper running tests after
/// the agent, quota errors in the code under work. Deliberately narrow
/// phrasing too: an agent building a rate-limiter says "rate limit" all day;
/// these are the CLI telling US to stop. Returns the matching line.
pub fn usage_limit_in(log: &str) -> Option<String> {
    let re = regex::Regex::new(
        r"(?i)(you(?:'|’)?ve (?:reached|hit) your .{0,60}limit|(?:usage|session|weekly|\d+[- ]hour) limit (?:reached|exceeded)|/usage-credits|credit balance is too low|out of usage)",
    )
    .ok()?;
    log.lines()
        .rev()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("[sirius] agent exit"))
        .take(5)
        .find(|l| re.is_match(l))
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ModelsConfig {
        ModelsConfig {
            default: Some("claude-sonnet-5-5".into()),
            routes: vec![
                ModelRoute {
                    labels: vec!["architecture".into(), "research".into(), "epic".into()],
                    model: "claude-fable-5-1".into(),
                },
                ModelRoute {
                    labels: vec!["accounting".into(), "security".into(), "auth".into()],
                    model: "claude-opus-5-5".into(),
                },
            ],
            fix_floor: Some("claude-opus-5-5".into()),
            review: Some("claude-fable-5-1".into()),
            allow_default: false,
        }
    }

    #[test]
    fn routes_by_label_first_match_wins_and_fix_rounds_have_a_floor() {
        let c = cfg();
        let l = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            model_for(&c, &l(&["bug"]), "work").as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(
            model_for(&c, &l(&["Security"]), "work").as_deref(),
            Some("claude-opus-5-5")
        );
        // First matching ROUTE wins, even if a later route matches too.
        assert_eq!(
            model_for(&c, &l(&["auth", "epic"]), "work").as_deref(),
            Some("claude-fable-5-1")
        );
        // Fix rounds of un-routed tickets rise to the floor...
        assert_eq!(
            model_for(&c, &l(&["bug"]), "fix").as_deref(),
            Some("claude-opus-5-5")
        );
        // ...routed tickets keep their route model.
        assert_eq!(
            model_for(&c, &l(&["research"]), "fix").as_deref(),
            Some("claude-fable-5-1")
        );
        assert_eq!(review_model(&c).as_deref(), Some("claude-fable-5-1"));
    }

    #[test]
    fn resolution_precedence_flag_config_parent_none() {
        // flag beats config
        let mut c = cfg();
        assert_eq!(
            resolve(&mut c, Some("claude-haiku-4-5"), None, false, Some("p")).unwrap(),
            "--model"
        );
        assert_eq!(c.default.as_deref(), Some("claude-haiku-4-5"));
        // config beats parent
        let mut c = cfg();
        assert_eq!(
            resolve(&mut c, None, None, false, Some("claude-x-1")).unwrap(),
            "config models.default"
        );
        assert_eq!(c.default.as_deref(), Some("claude-sonnet-5-5"));
        // parent when nothing else
        let mut c = ModelsConfig::default();
        assert_eq!(
            resolve(&mut c, None, None, false, Some("claude-opus-5-5")).unwrap(),
            "$SIRIUS_PARENT_MODEL"
        );
        assert_eq!(c.default.as_deref(), Some("claude-opus-5-5"));
        // none
        let mut c = ModelsConfig::default();
        assert_eq!(resolve(&mut c, None, None, false, None).unwrap(), "none");
        assert_eq!(c.default, None);
        // review: flag > config > (falls back to the worker default)
        let mut c = ModelsConfig::default();
        resolve(
            &mut c,
            Some("claude-sonnet-5-5"),
            Some("claude-opus-5-5"),
            false,
            None,
        )
        .unwrap();
        assert_eq!(review_model(&c).as_deref(), Some("claude-opus-5-5"));
        let mut c = ModelsConfig::default();
        resolve(&mut c, Some("claude-sonnet-5-5"), None, false, None).unwrap();
        assert_eq!(review_model(&c).as_deref(), Some("claude-sonnet-5-5"));
    }

    #[test]
    fn empty_strings_are_unset_everywhere() {
        let mut c = ModelsConfig {
            default: Some("claude-sonnet-5-5".into()),
            routes: vec![ModelRoute {
                labels: vec!["x".into()],
                model: " ".into(),
            }],
            fix_floor: Some("".into()),
            review: Some("".into()),
            allow_default: false,
        };
        resolve(&mut c, None, None, false, None).unwrap();
        assert!(c.routes.is_empty() && c.fix_floor.is_none() && c.review.is_none());
        // Fix rounds then fall back to the default — never to "".
        assert_eq!(
            model_for(&c, &["x".into()], "fix").as_deref(),
            Some("claude-sonnet-5-5")
        );
    }

    #[test]
    fn inherit_reads_the_parent_model_or_fails_loudly() {
        let mut c = ModelsConfig::default();
        resolve(
            &mut c,
            Some("inherit"),
            Some("inherit"),
            false,
            Some("claude-opus-5-5"),
        )
        .unwrap();
        assert_eq!(c.default.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(c.review.as_deref(), Some("claude-opus-5-5"));
        let mut c = ModelsConfig::default();
        let e = resolve(&mut c, Some("inherit"), None, false, None).unwrap_err();
        assert!(e.contains("SIRIUS_PARENT_MODEL"), "{e}");
    }

    #[test]
    fn aliases_are_flagged_explicit_ids_are_not() {
        assert!(looks_like_alias("fable[1m]"));
        assert!(looks_like_alias("opus"));
        assert!(looks_like_alias("sonnet"));
        assert!(!looks_like_alias("claude-opus-5-5"));
        assert!(!looks_like_alias("claude-haiku-4-5-20251001"));
        assert!(!looks_like_alias("gpt-5"));
    }

    #[test]
    fn usage_limit_text_is_found_only_when_the_cli_says_stop() {
        let lydgr = "Ignoring 15 permissions.allow entries...\nYou've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.\n\n[sirius] agent exit: 1\n";
        assert_eq!(
            usage_limit_in(lydgr).as_deref(),
            Some("You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.")
        );
        assert!(usage_limit_in("Credit balance is too low").is_some());
        // An agent WORKING on rate limiting is not a usage limit.
        assert!(usage_limit_in("Implemented the rate limit middleware (429 on burst).").is_none());
        assert!(usage_limit_in("").is_none());
        // Curly apostrophe and other CLI phrasings.
        assert!(usage_limit_in("You’ve hit your limit · resets 5pm").is_some());
        assert!(usage_limit_in("5-hour limit reached ∙ resets 3am").is_some());
        // The same words EARLY in a long failed log are content, not a stop.
        let early = format!(
            "usage limit exceeded in fixture\n{}",
            "test line\n".repeat(20)
        );
        assert!(usage_limit_in(&early).is_none());
    }

    #[test]
    fn claude_default_model_follows_settings_precedence() {
        let dir = std::env::temp_dir().join(format!("sirius-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let root = dir.join("repo");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"model":"fable[1m]"}"#,
        )
        .unwrap();
        assert_eq!(
            claude_default_model(&root, Some(&home)).unwrap().0,
            "fable[1m]"
        );
        std::fs::write(
            root.join(".claude/settings.json"),
            r#"{"model":"claude-sonnet-5-5"}"#,
        )
        .unwrap();
        assert_eq!(
            claude_default_model(&root, Some(&home)).unwrap().0,
            "claude-sonnet-5-5"
        );
        std::fs::write(
            root.join(".claude/settings.local.json"),
            r#"{"model":"claude-opus-5-5"}"#,
        )
        .unwrap();
        assert_eq!(
            claude_default_model(&root, Some(&home)).unwrap().0,
            "claude-opus-5-5"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
