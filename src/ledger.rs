//! The ledger — `.sirius/sirius.db`.
//!
//! Sirius's ONLY write target (PRD §2.2). SQLite in WAL mode, `user_version` =
//! [`SCHEMA_VERSION`]. Schema is exactly CONTRACTS §1. The Console reads these
//! tables read-only for the fleet board and history.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

/// The current wall-clock time as an ISO-8601 UTC string.
///
/// Kept dependency-free: we format the Unix epoch ourselves so we don't pull in
/// `chrono` (outside the allowed crate set).
pub fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_iso8601(dur.as_secs(), dur.subsec_millis())
}

/// Format seconds-since-epoch (+ millis) as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn format_iso8601(secs: u64, millis: u32) -> String {
    // Civil-from-days algorithm (Howard Hinnant), no external deps.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, hour, min, sec, millis
    )
}

/// A handle to the ledger connection.
pub struct Ledger {
    pub conn: Connection,
}

impl Ledger {
    /// Open (must already exist). Enables WAL and foreign keys.
    pub fn open(path: &Path) -> rusqlite::Result<Ledger> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Ledger { conn })
    }

    /// Open an in-memory ledger with the schema applied (for tests).
    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Ledger> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        apply_schema(&conn, "test")?;
        Ok(Ledger { conn })
    }

    /// Create the ledger at `path`, applying the schema and meta rows.
    /// Idempotent-safe to call once at `sirius init`.
    pub fn create(path: &Path, sirius_version: &str) -> rusqlite::Result<Ledger> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        apply_schema(&conn, sirius_version)?;
        Ok(Ledger { conn })
    }

    #[allow(dead_code)] // read API used by the Console + tests
    pub fn schema_version(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))
    }

    /// The SQLite `data_version` — bumps on any external write; used by the
    /// Console for SSE polling (CONTRACTS §1). Part of the read API the Console
    /// consumes; not called by the CLI itself.
    #[allow(dead_code)]
    pub fn data_version(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("PRAGMA data_version", [], |r| r.get(0))
    }

    #[allow(dead_code)] // read API used by the Console + tests
    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    // ---- workers -------------------------------------------------------

    /// Insert a worker if absent; refresh `last_seen_at`/`status` if present.
    pub fn upsert_worker(&self, id: &str, status: &str) -> rusqlite::Result<()> {
        let now = now_iso8601();
        self.conn.execute(
            "INSERT INTO workers (id, created_at, last_seen_at, status)
             VALUES (?1, ?2, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET last_seen_at = ?2, status = ?3",
            params![id, now, status],
        )?;
        Ok(())
    }

    // ---- receipts ------------------------------------------------------

    /// Insert a receipt row and return its id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_receipt(
        &self,
        kind: &str,
        r#ref: &str,
        symbols: &[String],
        forward_ok: bool,
        reverse_ok: bool,
        worker_id: Option<&str>,
    ) -> rusqlite::Result<i64> {
        let symbols_json = serde_json::to_string(symbols).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO receipts (kind, ref, symbols, forward_ok, reverse_ok, created_at, worker_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                kind,
                r#ref,
                symbols_json,
                forward_ok as i64,
                reverse_ok as i64,
                now_iso8601(),
                worker_id
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // ---- iterations ----------------------------------------------------

    /// Start an iteration row; returns its id. Fields are filled in as the
    /// iteration progresses via [`Ledger::finish_iteration`].
    pub fn start_iteration(
        &self,
        worker_id: &str,
        issue_ref: Option<&str>,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO iterations (worker_id, issue_ref, started_at)
             VALUES (?1, ?2, ?3)",
            params![worker_id, issue_ref, now_iso8601()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Finalize an iteration row with its outcome and metrics.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_iteration(
        &self,
        id: i64,
        entities: &[String],
        outcome: &str,
        gate_result: Option<&str>,
        oracle_verdicts: &[String],
        tokens: Option<i64>,
        duration_ms: Option<i64>,
        receipt_id: Option<i64>,
    ) -> rusqlite::Result<()> {
        let entities_json = serde_json::to_string(entities).unwrap_or_else(|_| "[]".into());
        let verdicts_json = serde_json::to_string(oracle_verdicts).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "UPDATE iterations SET
               entities = ?2, ended_at = ?3, outcome = ?4, gate_result = ?5,
               oracle_verdicts = ?6, tokens = ?7, duration_ms = ?8, receipt_id = ?9
             WHERE id = ?1",
            params![
                id,
                entities_json,
                now_iso8601(),
                outcome,
                gate_result,
                verdicts_json,
                tokens,
                duration_ms,
                receipt_id
            ],
        )?;
        Ok(())
    }

    // ---- policy events -------------------------------------------------

    pub fn log_policy_event(
        &self,
        iteration_id: Option<i64>,
        kind: &str,
        detail: &serde_json::Value,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO policy_events (iteration_id, kind, detail, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![iteration_id, kind, detail.to_string(), now_iso8601()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Count recent policy events of a kind — used by adaptive claiming (M5)
    /// to read contention from history.
    pub fn count_policy_events(&self, kind: &str, limit: i64) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM (SELECT id FROM policy_events
             WHERE kind = ?1 ORDER BY id DESC LIMIT ?2)",
            params![kind, limit],
            |r| r.get(0),
        )
    }
}

/// Apply the full CONTRACTS §1 schema and seed the meta rows.
fn apply_schema(conn: &Connection, sirius_version: &str) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
          key   TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS workers (
          id           TEXT PRIMARY KEY,
          created_at   TEXT NOT NULL,
          last_seen_at TEXT,
          status       TEXT NOT NULL DEFAULT 'idle'
        );

        CREATE TABLE IF NOT EXISTS receipts (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          kind          TEXT NOT NULL,
          ref           TEXT NOT NULL,
          symbols       TEXT NOT NULL,
          forward_ok    INTEGER NOT NULL DEFAULT 0,
          reverse_ok    INTEGER NOT NULL DEFAULT 0,
          created_at    TEXT NOT NULL,
          worker_id     TEXT
        );

        CREATE TABLE IF NOT EXISTS iterations (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          worker_id     TEXT NOT NULL REFERENCES workers(id),
          issue_ref     TEXT,
          entities      TEXT,
          started_at    TEXT NOT NULL,
          ended_at      TEXT,
          outcome       TEXT,
          gate_result   TEXT,
          oracle_verdicts TEXT,
          tokens        INTEGER,
          duration_ms   INTEGER,
          receipt_id    INTEGER REFERENCES receipts(id)
        );

        CREATE TABLE IF NOT EXISTS policy_events (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          iteration_id INTEGER REFERENCES iterations(id),
          kind        TEXT NOT NULL,
          detail      TEXT,
          created_at  TEXT NOT NULL
        );
        "#,
    )?;

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    let now = now_iso8601();
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('created_at', ?1)",
        params![now],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('sirius_version', ?1)",
        params![sirius_version],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_epoch_zero() {
        assert_eq!(format_iso8601(0, 0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn iso8601_known_timestamp() {
        // 2026-07-04T00:00:00Z == 1783123200
        assert_eq!(format_iso8601(1_783_123_200, 0), "2026-07-04T00:00:00.000Z");
    }

    #[test]
    fn schema_has_version_and_meta_rows() {
        let led = Ledger::open_in_memory().unwrap();
        assert_eq!(led.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(led.meta("schema_version").unwrap().as_deref(), Some("1"));
        assert!(led.meta("created_at").unwrap().is_some());
        assert!(led.meta("sirius_version").unwrap().is_some());
    }

    #[test]
    fn receipt_roundtrip() {
        let led = Ledger::open_in_memory().unwrap();
        let id = led
            .insert_receipt(
                "issue",
                "AMT-7",
                &["a".into(), "b".into()],
                true,
                false,
                Some("sirius/oak"),
            )
            .unwrap();
        assert_eq!(id, 1);
        let (kind, symbols, fwd, rev): (String, String, i64, i64) = led
            .conn
            .query_row(
                "SELECT kind, symbols, forward_ok, reverse_ok FROM receipts WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(kind, "issue");
        assert_eq!(symbols, "[\"a\",\"b\"]");
        assert_eq!(fwd, 1);
        assert_eq!(rev, 0);
    }

    #[test]
    fn iteration_lifecycle_writes_one_row() {
        let led = Ledger::open_in_memory().unwrap();
        led.upsert_worker("sirius/oak", "working").unwrap();
        let it = led.start_iteration("sirius/oak", Some("AMT-7")).unwrap();
        led.finish_iteration(
            it,
            &["e1".into()],
            "completed",
            Some("pass"),
            &["registered".into()],
            Some(1234),
            Some(42),
            None,
        )
        .unwrap();
        let (outcome, gate, entities): (String, String, String) = led
            .conn
            .query_row(
                "SELECT outcome, gate_result, entities FROM iterations WHERE id = ?1",
                [it],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(outcome, "completed");
        assert_eq!(gate, "pass");
        assert_eq!(entities, "[\"e1\"]");
        let count: i64 = led
            .conn
            .query_row("SELECT COUNT(*) FROM iterations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn policy_events_count() {
        let led = Ledger::open_in_memory().unwrap();
        for _ in 0..5 {
            led.log_policy_event(None, "backoff_409", &serde_json::json!({"x":1}))
                .unwrap();
        }
        assert_eq!(led.count_policy_events("backoff_409", 100).unwrap(), 5);
        assert_eq!(led.count_policy_events("backoff_409", 3).unwrap(), 3);
        assert_eq!(led.count_policy_events("gate_tier", 100).unwrap(), 0);
    }
}
