// Ledger schema (CONTRACTS.md §1) — schema_version = 1.
// The Console only READS these tables; this DDL exists so the fixture seeder
// can produce a ledger byte-shaped like the one `sirius init` creates. When the
// real binary lands, the Console reads its DB and this DDL is only used by tests.

export const SCHEMA_VERSION = 1;

export const LEDGER_DDL = /* sql */ `
PRAGMA journal_mode=WAL;
PRAGMA user_version=${SCHEMA_VERSION};

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
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  iteration_id INTEGER REFERENCES iterations(id),
  kind         TEXT NOT NULL,
  detail       TEXT,
  created_at   TEXT NOT NULL
);
`;

// Enumerations from the schema comments, used for typing + display.
export type WorkerStatus = "idle" | "working" | "blocked" | "stopped";
export type IterationOutcome =
  | "completed"
  | "released"
  | "deadend"
  | "gate_failed"
  | "error";
export type GateResult = "pass" | "fail" | "skipped" | null;
export type ReceiptKind = "issue" | "decision";
// per-entity oracle verdict inside iterations.oracle_verdicts JSON array
export type OracleVerdict = "registered" | "blocked" | "forced";
export type PolicyEventKind =
  | "claim_order"
  | "backoff_409"
  | "oracle_202"
  | "gate_tier"
  | "retry_budget"
  | "concurrency";
