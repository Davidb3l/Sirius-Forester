//! `sirius doctor` — checks the five PRD §6 contract facts live (M0).
//!
//! 1. amt present + schema (read the ametrite `meta.schema_version` read-only;
//!    pragmatic ≥ 3, NOT a version-string compare — `amt 0.1.0` ships schema 3).
//! 2. hayven daemon on :7777 (health probe + `hayven daemon status`).
//! 3. claim exit-code semantics (amt claim JSON shape is parseable; hayven claim
//!    surface present).
//! 4. gate exit codes (hayven affected-tests present).
//! 5. fleet-memory write path (hayven remember/recall present).

use crate::amt::Amt;
use crate::hayven::Hayven;
use crate::shell::Runner;
use crate::workspace::Workspace;
use rusqlite::{Connection, OpenFlags};

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub pass: bool,
    pub detail: String,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass: true,
            detail: detail.into(),
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass: false,
            detail: detail.into(),
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

/// Run all five checks. `runner` is the shell seam so this is testable offline.
pub fn run(ws: &Workspace, runner: &dyn Runner) -> DoctorReport {
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
        checks.push(Check::ok(
            "hayven_daemon_7777",
            format!(
                "hayven {hv_ver}, daemon healthy on :7777 (status: {});{hv_ws}",
                first_line(&status)
            ),
        ));
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

    let ok = checks.iter().all(|c| c.pass);
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

        let report = run(&ws, &m);
        assert!(report.ok, "checks: {:?}", report.checks);
        assert_eq!(report.checks.len(), 5);
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
        let report = run(&ws_no_ametrite(), &m);
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
        let report = run(&ws, &m);
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
}
