//! `sirius doctor` — checks the five PRD §6 contract facts live (M0), plus one
//! ADVISORY check on the Claude Code plugin half of the install.
//!
//! 1. amt present + schema (read the ametrite `meta.schema_version` read-only;
//!    pragmatic ≥ 3, NOT a version-string compare — `amt 0.1.0` ships schema 3).
//! 2. hayven daemon on :7777 (health probe + `hayven daemon status`).
//! 3. claim exit-code semantics (amt claim JSON shape is parseable; hayven claim
//!    surface present).
//! 4. gate exit codes (hayven affected-tests present).
//! 5. fleet-memory write path (hayven remember/recall present).
//! 6. plugin handoff (ADVISORY, never gates `ok`): is the Sothis bundle
//!    marketplace added and are the sirius/hayvenhurst/catryna plugins
//!    installed in Claude Code? This exists because the CLI half and the
//!    plugin half install separately, and a real audit found machines with
//!    every BINARY present but the `sirius` plugin never installed — the
//!    printed `/plugin` handoff is a silent drop-off unless something checks.

use crate::amt::Amt;
use crate::hayven::Hayven;
use crate::shell::Runner;
use crate::workspace::Workspace;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub pass: bool,
    pub detail: String,
    /// A gating check flips the report's `ok` when it fails. An advisory
    /// (non-gating) check reports and recommends but never fails doctor —
    /// sirius is fully functional without the Claude Code plugin layer
    /// (CI boxes, plain-terminal users), so incompleteness there is a WARN.
    pub gating: bool,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass: true,
            detail: detail.into(),
            gating: true,
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass: false,
            detail: detail.into(),
            gating: true,
        }
    }
    fn advisory(name: &str, pass: bool, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass,
            detail: detail.into(),
            gating: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<Check>,
}

/// Minimum ametrite schema version Sirius depends on (PRD "schema >= v3").
pub const MIN_AMETRITE_SCHEMA: i64 = 3;

/// Read the ametrite schema version from its `meta` table, read-only. Sirius
/// never writes the parent DB; here it only reads (§2.2 allows read-only).
pub fn ametrite_schema_version(ws: &Workspace) -> Result<i64, String> {
    let db = ws
        .ametrite_db
        .as_ref()
        .ok_or_else(|| "no .ametrite/ametrite.db found (run `amt init`)".to_string())?;
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open ametrite db read-only: {e}"))?;
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("ametrite meta.schema_version unreadable: {e}"))?;
    v.trim()
        .parse::<i64>()
        .map_err(|e| format!("ametrite schema_version not an integer: {e}"))
}

/// Probe the hayven daemon health on :7777 via a plain HTTP GET (no deps —
/// we shell `curl`, present on macOS/Linux; a failure is reported, not fatal).
fn daemon_http_ok(runner: &dyn Runner) -> bool {
    match runner.run(
        "curl",
        &[
            "-s",
            "-o",
            "/dev/null",
            "-m",
            "3",
            "-w",
            "%{http_code}",
            "http://localhost:7777/",
        ],
    ) {
        Ok(o) => o.stdout.trim() == "200",
        Err(_) => false,
    }
}

/// The Claude Code plugin directory for this user, if resolvable.
fn default_plugins_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("plugins"))
}

/// Read a JSON file into a Value; None if absent or unparseable.
fn read_json(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Check #6 — the plugin handoff (ADVISORY). Pure over `plugins_dir` so tests
/// drive it with fixture directories, no env mutation.
///
/// Detection rules (the gotcha that produces false negatives if ignored):
/// the same plugin can be installed from DIFFERENT marketplaces — on a real
/// machine Hayvenhurst is `hayvenhurst@hayvenhurst`, via the bundle it would be
/// `hayvenhurst@sirius-forester`. So plugins match on the `<name>@` KEY PREFIX,
/// never one full key. Only the bundle MARKETPLACE is matched by its exact
/// name, because that is the thing only this repo provides.
pub fn plugin_handoff_check(plugins_dir: Option<&Path>) -> Check {
    const NAME: &str = "plugin_handoff";
    let dir = match plugins_dir {
        // No resolvable plugin dir at all: not a Claude Code environment —
        // nothing to hand off to. Advisory pass, clearly labeled as skipped.
        Some(d) if d.is_dir() => d,
        _ => {
            return Check::advisory(
                NAME,
                true,
                "no Claude Code plugin dir (~/.claude/plugins) — not a Claude Code \
                 environment, check skipped",
            );
        }
    };

    // Marketplace: known_marketplaces.json is an object keyed by marketplace
    // name. Absent/unparseable counts as "not added" — that IS the cold state.
    let marketplace_ok = read_json(&dir.join("known_marketplaces.json"))
        .and_then(|v| v.as_object().map(|o| o.contains_key("sirius-forester")))
        .unwrap_or(false);

    // Plugins: installed_plugins.json v2 is {version, plugins: {"name@mkt": …}}.
    // PREFIX match per the rule above; any marketplace satisfies a plugin.
    let plugin_keys: Vec<String> = read_json(&dir.join("installed_plugins.json"))
        .and_then(|v| {
            v.get("plugins")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect())
        })
        .unwrap_or_default();
    let has_plugin = |name: &str| {
        let prefix = format!("{name}@");
        plugin_keys.iter().any(|k| k.starts_with(&prefix))
    };

    let mut fixes: Vec<String> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    if !marketplace_ok {
        missing.push("sirius-forester marketplace");
        fixes.push("/plugin marketplace add Davidb3l/Sirius-Forester".into());
    }
    for name in ["sirius", "hayvenhurst", "catryna"] {
        if !has_plugin(name) {
            missing.push(match name {
                "sirius" => "sirius plugin",
                "hayvenhurst" => "hayvenhurst plugin",
                _ => "catryna plugin",
            });
            fixes.push(format!("/plugin install {name}@sirius-forester"));
        }
    }

    if missing.is_empty() {
        Check::advisory(
            NAME,
            true,
            "bundle marketplace added; sirius/hayvenhurst/catryna plugins installed",
        )
    } else {
        Check::advisory(
            NAME,
            false,
            format!(
                "CLIs alone are half the install — missing: {}. Fix in Claude Code: {}",
                missing.join(", "),
                fixes.join("  then  ")
            ),
        )
    }
}

/// Run all checks. `runner` is the shell seam so this is testable offline.
pub fn run(ws: &Workspace, runner: &dyn Runner) -> DoctorReport {
    run_with_plugins_dir(ws, runner, default_plugins_dir())
}

/// Like `run`, with the Claude Code plugins dir injected (test seam).
pub fn run_with_plugins_dir(
    ws: &Workspace,
    runner: &dyn Runner,
    plugins_dir: Option<PathBuf>,
) -> DoctorReport {
    let amt = Amt::new(runner);
    let hv = Hayven::new(runner);
    let mut checks = Vec::new();

    // 1. amt present + schema.
    match amt.version() {
        Ok(ver) => match ametrite_schema_version(ws) {
            Ok(v) if v >= MIN_AMETRITE_SCHEMA => checks.push(Check::ok(
                "amt_present_and_schema",
                format!("{ver}, ametrite schema v{v} (>= v{MIN_AMETRITE_SCHEMA})"),
            )),
            Ok(v) => checks.push(Check::fail(
                "amt_present_and_schema",
                format!("{ver} but ametrite schema v{v} < v{MIN_AMETRITE_SCHEMA}"),
            )),
            Err(e) => checks.push(Check::fail("amt_present_and_schema", format!("{ver}; {e}"))),
        },
        Err(e) => checks.push(Check::fail(
            "amt_present_and_schema",
            format!("amt not runnable: {e}"),
        )),
    }

    // 2. hayven daemon on :7777.
    let http = daemon_http_ok(runner);
    let status = hv.daemon_status().unwrap_or_default();
    let hv_ver = hv.version().unwrap_or_else(|_| "unknown".into());
    let hv_ws = ws
        .hayven_dir
        .as_ref()
        .map(|_| " .hayven/ present")
        .unwrap_or(" .hayven/ not found (run `hayven init`)");
    if http {
        // The daemon is single-project-bound: a 200 on :7777 means *a* daemon is
        // up, not that it serves this workspace. Only call it "healthy" when the
        // status line isn't an error and this workspace has a .hayven/ to serve.
        let status_line = first_line(&status);
        let serving = ws.hayven_dir.is_some()
            && !status_line.to_ascii_lowercase().contains("error")
            && !status_line.contains("No .hayven");
        if serving {
            checks.push(Check::ok(
                "hayven_daemon_7777",
                format!("hayven {hv_ver}, daemon healthy on :7777 (status: {status_line});{hv_ws}"),
            ));
        } else {
            // SIRF-10: the daemon is single-project-bound — a 200 on :7777 means
            // *a* daemon is up, not that it serves THIS workspace. This is the one
            // silent-degradation state CONTRACTS documents: forward stamps (amt)
            // still land but reverse stamps (hayven remember) quietly go one-way
            // (reverse_ok:false). So a different-project daemon must FAIL, not pass.
            checks.push(Check::fail(
                "hayven_daemon_7777",
                format!(
                    "hayven {hv_ver}, daemon up on :7777 but not serving this workspace (status: {status_line});{hv_ws} — run `hayven daemon start` in this repo"
                ),
            ));
        }
    } else {
        checks.push(Check::fail(
            "hayven_daemon_7777",
            format!(
                "no 200 from http://localhost:7777 (status: {});{hv_ws}",
                first_line(&status)
            ),
        ));
    }

    // 3. claim exit-code semantics — verify amt claim --peek returns parseable
    //    JSON (does not take a lease) and the hayven claim surface exists.
    match runner.run(
        "amt",
        &["--json", "claim", "--peek", "--agent", "sirius/doctor"],
    ) {
        Ok(o) if serde_json::from_str::<serde_json::Value>(&o.stdout).is_ok() => {
            checks.push(Check::ok(
                "claim_exit_codes",
                "amt claim JSON shape parseable; hayven claim: 0/1/3",
            ))
        }
        Ok(o) => checks.push(Check::fail(
            "claim_exit_codes",
            format!("amt claim --peek non-JSON: {}", first_line(&o.stdout)),
        )),
        Err(e) => checks.push(Check::fail(
            "claim_exit_codes",
            format!("amt claim --peek failed: {e}"),
        )),
    }

    // Fetch hayven's command surface once and reuse for checks 4 and 5.
    let hayven_help = runner.run("hayven", &["--help"]);

    // 4. gate exit codes — hayven affected-tests present (its --help mentions it).
    match &hayven_help {
        Ok(o) if o.stdout.contains("affected-tests") => checks.push(Check::ok(
            "gate_exit_codes",
            "hayven affected-tests present (exit 0 pass / non-0 fail)",
        )),
        Ok(_) => checks.push(Check::fail(
            "gate_exit_codes",
            "hayven affected-tests not found in help",
        )),
        Err(e) => checks.push(Check::fail(
            "gate_exit_codes",
            format!("hayven not runnable: {e}"),
        )),
    }

    // 5. fleet-memory write path — hayven remember/recall present.
    match &hayven_help {
        Ok(o) if o.stdout.contains("remember") && o.stdout.contains("recall") => checks.push(
            Check::ok("fleet_memory_write_path", "hayven remember/recall present"),
        ),
        Ok(_) => checks.push(Check::fail(
            "fleet_memory_write_path",
            "remember/recall not found in help",
        )),
        Err(e) => checks.push(Check::fail(
            "fleet_memory_write_path",
            format!("hayven not runnable: {e}"),
        )),
    }

    // 6. plugin handoff — advisory; reports and recommends, never gates.
    checks.push(plugin_handoff_check(plugins_dir.as_deref()));

    // Only GATING checks decide overall health; an advisory failure is a WARN.
    let ok = checks.iter().all(|c| !c.gating || c.pass);
    DoctorReport { ok, checks }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};
    use std::path::PathBuf;

    fn ws_no_ametrite() -> Workspace {
        Workspace {
            root: PathBuf::from("/nonexistent"),
            ametrite_db: None,
            hayven_dir: None,
        }
    }

    #[test]
    fn all_green_when_everything_healthy() {
        // Build a workspace with a real read-only ametrite-like db in a temp dir.
        let dir = std::env::temp_dir().join(format!("sirius-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        std::fs::create_dir_all(dir.join(".hayven")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        // SIRF-10: a genuinely-healthy workspace must have a .hayven/ so the daemon
        // on :7777 is serving THIS repo (serving == true).
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: Some(dir.join(".hayven")),
        };

        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.expect(&["curl"], 0, "200");
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "running");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "commands: affected-tests remember recall claim",
            "",
        ));

        let report = run_with_plugins_dir(&ws, &m, None);
        assert!(report.ok, "checks: {:?}", report.checks);
        assert_eq!(report.checks.len(), 6);
        // With no plugins dir the handoff check is an advisory PASS (skipped),
        // clearly labeled — a CI box is not an incomplete install.
        let ph = report
            .checks
            .iter()
            .find(|c| c.name == "plugin_handoff")
            .unwrap();
        assert!(ph.pass && !ph.gating);
        assert!(ph.detail.contains("skipped"), "detail: {}", ph.detail);
    }

    // The central invariant of the advisory design, pinned END-TO-END: a
    // failing plugin_handoff must NOT flip report.ok (and therefore never
    // changes doctor's exit code) even when every gating check passes.
    #[test]
    fn advisory_failure_never_flips_overall_ok() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor-adv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        std::fs::create_dir_all(dir.join(".hayven")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: Some(dir.join(".hayven")),
        };
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.expect(&["curl"], 0, "200");
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "running");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        // An EXISTING but empty plugins dir = the true cold state → advisory FAILS.
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let report = run_with_plugins_dir(&ws, &m, Some(plugins));
        let ph = report
            .checks
            .iter()
            .find(|c| c.name == "plugin_handoff")
            .unwrap();
        assert!(!ph.pass && !ph.gating, "advisory must fail here");
        assert!(report.ok, "advisory failure flipped ok — the design broke");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- plugin handoff (check #6) -----------------------------------------

    fn plugins_fixture(name: &str, marketplaces: &str, plugins: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sirius-ph-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("known_marketplaces.json"), marketplaces).unwrap();
        std::fs::write(dir.join("installed_plugins.json"), plugins).unwrap();
        dir
    }

    // The exact state found on the audited machine (2026-08-05): every CLI
    // installed, hayvenhurst + catryna plugins from their OWN marketplaces,
    // and sirius — the only tool without a standalone marketplace — missing.
    #[test]
    fn plugin_handoff_flags_the_audited_dropoff_state() {
        let dir = plugins_fixture(
            "audit",
            r#"{"claude-plugins-official":{},"rlm-claude-code":{},"hayvenhurst":{},"catryna-wikinelli":{}}"#,
            r#"{"version":2,"plugins":{"rlm-claude-code@rlm-claude-code":{},"frontend-design@claude-plugins-official":{},"hayvenhurst@hayvenhurst":{},"catryna@catryna-wikinelli":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(!c.pass && !c.gating);
        // Hayvenhurst and catryna are present via their standalone marketplaces
        // (prefix rule) — ONLY the marketplace and the sirius plugin may be named.
        assert!(c.detail.contains("sirius-forester marketplace"));
        assert!(c.detail.contains("sirius plugin"));
        assert!(!c.detail.contains("hayvenhurst plugin"), "{}", c.detail);
        assert!(!c.detail.contains("catryna plugin"), "{}", c.detail);
        // The fix commands are exact and in cold-start order.
        assert!(c
            .detail
            .contains("/plugin marketplace add Davidb3l/Sirius-Forester"));
        assert!(c.detail.contains("/plugin install sirius@sirius-forester"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_handoff_passes_when_bundle_complete() {
        let dir = plugins_fixture(
            "full",
            r#"{"sirius-forester":{},"hayvenhurst":{}}"#,
            r#"{"version":2,"plugins":{"sirius@sirius-forester":{},"hayvenhurst@sirius-forester":{},"catryna@sirius-forester":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(c.pass && !c.gating, "detail: {}", c.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Mixed sources must satisfy the check: bundle marketplace added, sirius
    // from the bundle, hayvenhurst/catryna from their standalone marketplaces.
    #[test]
    fn plugin_handoff_accepts_any_marketplace_per_plugin() {
        let dir = plugins_fixture(
            "mixed",
            r#"{"sirius-forester":{},"hayvenhurst":{},"catryna-wikinelli":{}}"#,
            r#"{"version":2,"plugins":{"sirius@sirius-forester":{},"hayvenhurst@hayvenhurst":{},"catryna@catryna-wikinelli":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(c.pass, "mixed sources must pass, detail: {}", c.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Absent files inside an existing plugins dir = the true cold state.
    #[test]
    fn plugin_handoff_cold_state_names_everything() {
        let dir = std::env::temp_dir().join(format!("sirius-ph-cold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = plugin_handoff_check(Some(&dir));
        assert!(!c.pass && !c.gating);
        for needle in [
            "sirius-forester marketplace",
            "sirius plugin",
            "hayvenhurst plugin",
            "catryna plugin",
        ] {
            assert!(c.detail.contains(needle), "missing {needle}: {}", c.detail);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A prefix must not match a plugin whose name merely STARTS with another
    // tool's name — "sirius-console@x" is not the sirius plugin.
    #[test]
    fn plugin_handoff_prefix_requires_the_at_sign() {
        let dir = plugins_fixture(
            "prefix",
            r#"{"sirius-forester":{}}"#,
            r#"{"version":2,"plugins":{"sirius-console@somewhere":{},"hayvenhurst@hayvenhurst":{},"catryna@catryna-wikinelli":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(!c.pass);
        assert!(c.detail.contains("sirius plugin"), "{}", c.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fails_without_ametrite_schema() {
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.expect(&["curl"], 0, "200");
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"ok":true}"#,
            "",
        ));
        let report = run_with_plugins_dir(&ws_no_ametrite(), &m, None);
        assert!(!report.ok);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "amt_present_and_schema")
            .unwrap();
        assert!(!c.pass);
    }

    #[test]
    fn fails_when_daemon_down() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: None,
        };
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.push(MockResponse::new(&["curl"], 0, "000", "")); // no 200
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "stopped");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        let report = run_with_plugins_dir(&ws, &m, None);
        assert!(!report.ok);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name == "hayven_daemon_7777")
                .unwrap()
                .pass
        );
    }

    // SIRF-10: daemon up (http 200) but serving a DIFFERENT project must FAIL,
    // because reverse stamps (hayven remember) silently go one-way in that state.
    #[test]
    fn fails_when_daemon_serves_different_workspace() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        // This workspace has NO .hayven/, so a running daemon on :7777 is serving
        // some other project — `serving` is false and the check must fail.
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: None,
        };
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.push(MockResponse::new(&["curl"], 0, "200", "")); // daemon IS up
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "running (other-repo)");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        let report = run_with_plugins_dir(&ws, &m, None);
        assert!(!report.ok, "checks: {:?}", report.checks);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "hayven_daemon_7777")
            .unwrap();
        assert!(!c.pass, "expected mismatch to fail, got: {}", c.detail);
        assert!(
            c.detail.contains("not serving this workspace"),
            "detail: {}",
            c.detail
        );
        assert!(
            c.detail.contains("hayven daemon start"),
            "expected fix-it hint, detail: {}",
            c.detail
        );
    }
}
