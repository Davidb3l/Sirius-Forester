# Sirius Forester — Build Contracts (v1)

This is the **single coordination artifact** for parallel implementation. Three agents
build against it concurrently. If any interface below must change, change it *here first*
and note it in your final report so the other agents can reconcile.

Authoritative sources: [PRD.md](PRD.md) and [ROADMAP.md](ROADMAP.md). This file only
pins the concrete interfaces where the three workstreams meet.

---

## 0. File ownership (collision avoidance — do NOT edit outside your tree)

| Agent | Owns (writes) | Reads only |
|---|---|---|
| **sirius-core** | `Cargo.toml`, `Cargo.lock`, `src/**`, `.sirius/` schema (created by `sirius init`) | `CONTRACTS.md`, `PRD.md`, `ROADMAP.md` |
| **sirius-console** | `web/**` | `CONTRACTS.md`, the ledger schema below, `sirius --json` shapes below |
| **sirius-bench-docs** | `bench/**`, `.github/**`, `README.md`, `AGENTS.md`, `.claude/skills/sirius/**`, `docs/**` | everything |

Nobody but **sirius-core** touches `Cargo.toml` or `src/`. Nobody but **sirius-console**
touches `web/`. The top-level `.gitignore`, `LICENSE`, `PRD.md`, `ROADMAP.md`,
`CONTRACTS.md` already exist — do not overwrite them.

Commit your own work on a branch named `agent/<your-area>` (e.g. `agent/core`,
`agent/console`, `agent/bench-docs`) so merges are clean. Do not commit to a shared branch.

---

## 1. The ledger — `.sirius/sirius.db` (SQLite, WAL mode)

Sirius's ONLY write target. Created by `sirius init`. Read-only (WAL) by the Console.
`sirius init` sets `PRAGMA journal_mode=WAL` and `PRAGMA user_version` to the schema version.

```sql
-- schema_version = 1
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);  -- rows: schema_version, created_at, sirius_version

CREATE TABLE workers (
  id           TEXT PRIMARY KEY,          -- 'sirius/oak'
  created_at   TEXT NOT NULL,             -- ISO-8601 UTC
  last_seen_at TEXT,
  status       TEXT NOT NULL DEFAULT 'idle' -- idle|working|blocked|stopped
);

CREATE TABLE iterations (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  worker_id     TEXT NOT NULL REFERENCES workers(id),
  issue_ref     TEXT,                     -- 'AMT-7'
  entities      TEXT,                     -- JSON array of hayven entity ids
  started_at    TEXT NOT NULL,
  ended_at      TEXT,
  outcome       TEXT,                     -- completed|released|deadend|gate_failed|error
  gate_result   TEXT,                     -- pass|fail|skipped|null
  oracle_verdicts TEXT,                   -- JSON array (per-entity: registered|blocked|forced)
  tokens        INTEGER,                  -- nullable
  duration_ms   INTEGER,
  receipt_id    INTEGER REFERENCES receipts(id)
);

CREATE TABLE receipts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  kind          TEXT NOT NULL,            -- 'issue' | 'decision'
  ref           TEXT NOT NULL,            -- 'AMT-7' or 'D-3'
  symbols       TEXT NOT NULL,            -- JSON array of entity ids stamped
  forward_ok    INTEGER NOT NULL DEFAULT 0, -- amt comment landed (0/1)
  reverse_ok    INTEGER NOT NULL DEFAULT 0, -- hayven remember landed (0/1)
  created_at    TEXT NOT NULL,
  worker_id     TEXT
);

CREATE TABLE policy_events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  iteration_id INTEGER REFERENCES iterations(id),
  kind        TEXT NOT NULL,             -- claim_order|backoff_409|oracle_202|gate_tier|retry_budget|concurrency
  detail      TEXT,                      -- JSON
  created_at  TEXT NOT NULL
);
```

The Console reads these tables directly (read-only) for the fleet board and history views.
`data_version` for SSE polling = `PRAGMA data_version` on the ledger connection.

---

## 2. `sirius` CLI surface + `--json` output shapes

Every mutating command accepts `--json` and prints ONE JSON object to stdout (nothing else
on stdout; logs go to stderr). Exit codes: `0` ok, `1` operational failure, `2` usage error,
`3` gate/oracle "blocked" (soft), matching Hayvenhurst conventions.

```
sirius init                 -> {"ok":true,"ledger":".sirius/sirius.db","schema_version":1}
sirius doctor --json        -> {"ok":bool,"checks":[{"name":str,"pass":bool,"detail":str}, ...]}
                               # the five §6 contract facts: amt present+schema, hayven daemon,
                               # claim exit-code semantics, gate exit codes, fleet-memory write path

sirius link AMT-7 --symbols a,b,c [--changed] --json
   -> {"ok":true,"receipt_id":12,"kind":"issue","ref":"AMT-7",
       "symbols":["a","b","c"],"forward_ok":true,"reverse_ok":true}
sirius link --decision D-3 --symbols ... --json    # same shape, kind:"decision"

sirius why <symbol> --json  -> {"symbol":str,"issues":[{"ref":"AMT-7","title":str}],
                                "decisions":[{"ref":"D-3","summary":str}]}
sirius why AMT-7 --json     -> {"ref":"AMT-7","symbols":[str],"decisions":[str]}

sirius gate AMT-7 [--tier safe] [--target-status in_review] --json
   -> {"ok":bool,"issue":"AMT-7","tier":"safe","gate":"pass|fail",
       "advanced_to":"in_review"|null,"tests_selected":int,"comment_filed":bool}

sirius run --workers N --agent-cmd "<cmd>" [--from todo] --json
   # streams NDJSON iteration events to stdout, one object per line:
   -> {"event":"iteration","worker":"sirius/oak","issue":"AMT-7","phase":"claim|map|lock|brief|work|gate|receipt|release","...":...}
```

Console mutations shell out to `sirius <cmd> --json` and parse these shapes. Do not invent
other stdout formats.

---

## 3. `.sirius/config.json` (Policy engine, M5) — committed defaults

```json
{
  "claim_order_enforced": true,
  "backoff_409": {"strategy": "release_and_comment", "base_ms": 500, "max_ms": 8000},
  "oracle_202": "back-off",                // "back-off" | "force-with-budget"
  "force_budget_tokens": 0,
  "gate_tier": "safe",
  "target_status": "in_review",
  "retry_budget": 3,
  "worker_concurrency": 3,
  "claim_mode": "adaptive"                 // "always" | "never" | "adaptive"
}
```

Absent file ⇒ these defaults. `sirius` reads it; Console displays it read-only.

---

## 4. External parent CLIs (Sirius NEVER writes their DBs — §2.2)

All Ametrite writes via `amt ... --json`. All Hayvenhurst reads/writes via `hayven` CLI or
`http://localhost:7777`. Key calls (see PRD §9 for the full reference iteration):

```
amt claim --from todo --agent sirius/oak --json      # {claimed:bool, issue?, retry_after?}
amt issue update AMT-7 --status in_review --json
amt comment AMT-7 "<text>" --json
amt decide AMT-7 "<why>" --json                      # -> {decision:"D-n"}
amt release AMT-7 --json

hayven query "<terms>" --json
hayven impact <symbol> --json
hayven claim <symbol> --intent "AMT-7: ..." --agent sirius/oak   # exit 0 ok, 1 overlap, 3 oracle
hayven context <symbol> --json
hayven recall --node <id> --json
hayven remember --kind decision --node <id> --scope <ids> "<text>"
hayven affected-tests --changed --gate --gate-tier safe          # exit 0 pass, 1 fail
hayven release <claimId>
```

Verify exact flag names against the installed CLIs (`amt --help`, `hayven --help`,
`hayven <sub> --help`) at build time — the PRD's forms are the intent; the installed binaries
(`amt 0.1.0`, `hayven 0.0.5`) are ground truth. If a flag differs, adapt and note it here.

---

## 5. Build / test conventions

- Rust: `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
  Deps limited to the Ametrite set: `rusqlite` (bundled feature), `serde`, `serde_json`,
  `clap` (derive), `regex`. No others without a note here.
- Console: Bun only, ZERO npm runtime deps. `bun test`. Vanilla TS.
- Bench: harnesses under `bench/`, runnable with `bun run bench/<name>.ts`, each emits a
  measured number tied to a PRD §8 metric.
- CI (`.github/workflows/ci.yml`): cargo test/clippy/fmt + web bundle check + bench smoke,
  matrix macOS/Ubuntu/Windows (mirror the Ametrite workflow).
