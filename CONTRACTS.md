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

sirius gate AMT-7 [--tier safe] [--target-status in_review] [--range <git-range>] --json
   -> {"ok":bool,"issue":"AMT-7","tier":"safe","gate":"pass|fail",
       "plan":"subset(n)|full-suite|blocked|pass-with-warning|unconfigured",
       "ran_tests":bool,"advanced_to":"in_review"|null,
       "tests_selected":int,"comment_filed":bool}
   # Selects affected tests over the changed files, then RUNS them via
   # gate.test_cmd (full suite on any doubt); verdict = the runner's exit code.

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
hayven affected-tests --changed <files> --json  # SELECTS tests (exit 0 = selected, NOT passed); sirius runs them
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

---

## 6. Ground-truth CLI deltas

Verified against the installed `amt 0.1.0` and `hayven 0.0.5` at build time. Where a
real flag differs from §4's intent, `sirius-core` adapted its code to the form below.
These deltas are what the binary actually shells; the Console must mirror them if it
ever shells the parents directly (it should only shell `sirius --json`).

### Ametrite (`amt 0.1.0`)

- **`--json` is a global flag before the subcommand** (`amt --json claim ...`), not a
  per-subcommand suffix. (It is also accepted after, but Sirius emits it globally.)
- **Comments:** `amt issue comment <ID> -m/--body <text>` (returns `{"ok":true}`), NOT
  the §4 `amt comment <ID> "<text>"`.
- **Status updates:** `amt issue update <ID> --status <s>` (returns the updated issue
  object), NOT `amt issue update <ID> --status`.
- **`amt claim` output is NOT `{claimed:bool, issue?}`.** On success it returns the full
  issue object (`{"id":"AMT-7","title":...,"activity":[...]}`). On no-work it returns
  `{"claimed":false,"retry_after":N,"counts":{...},"reason":"..."}`. Sirius keys success
  off the presence of `id`, and no-work off `claimed:false`. `--peek` gives the no-work
  shape without taking a lease (used by `sirius doctor`).
- **`amt claim` flags:** `--from` accepts a comma-list/repeat of `backlog,todo`; `--ttl`
  (default 900), `--cooldown` (default 3600), `--agent`, `--issue <id>` (specific claim /
  heartbeat), `--peek`, `--all-workspaces` all present as in §4's intent.
- **`amt release <ID>`** requires `--agent` matching the claimant; takes `--status`
  (default `in_review`) and `-m/--comment`.
- **`amt decide` is `amt decide --issue <ID> --title <T> [-b <body>]`** (title required),
  returns `{"id":"D-n","resolves":"AMT-7",...}`, NOT `amt decide AMT-7 "<why>"`.
- **Ametrite schema version lives in the `meta` table** (`SELECT value FROM meta WHERE
  key='schema_version'`), NOT in `PRAGMA user_version` (which reads 0). Observed value on
  a freshly-`amt init`ed workspace is **v4** (≥ the PRD's "v3" floor). `sirius doctor`
  reads this read-only and checks `>= 3` pragmatically — a hardcoded `== 3` compare would
  already be wrong.

### Hayvenhurst (`hayven 0.0.5`)

- **No per-subcommand `--help`:** every `hayven <sub> --help` prints the same global help.
- **`hayven claim <ids...> --intent "..." [--force]`** — ids are **positional** (multiple
  allowed), there is **NO `--agent` flag** (agent is derived by the daemon) and **NO
  `--node`/`--scope`** on claim. Exit codes as in §4: **0 registered, 1 hard overlap
  (409), 3 oracle adjacency (202)**.
- **`hayven remember "<note>" [--node <id>] [--kind K] [--scope a,b] [--ttl S]`** — the
  note is the **first positional arg**; `--scope` is a comma list. Returns
  `{"id":"mem_...","nodeId":...,"kind":...,"scope":[...]}`. There is no `--agent` (agent
  is null in the record). This is the reverse-provenance write path (PRD §6 fact 3).
- **`hayven recall [<term>] [--node <id>] [--kind K] [--json]`** returns
  `{"count":N,"notes":[{...}]}`.
- **`hayven affected-tests` is a test SELECTOR, not a runner** (SIRF-5 / D-3). Its exit
  code means "selection computed," **never** "the tests pass" — `affected-tests --changed
  <files> --json` returns exit 0 with `{"roots":[...],"note":...,"tests":[...]}` even when
  `tests` is empty, and there are no `--gate` / `--gate-tier` flags in 0.0.5. **Sirius owns
  the run-the-tests half itself.** The gate (`src/gate.rs`): (1) resolves changed files from
  a git range, (2) calls `hayven affected-tests --changed <csv> --json` to *select*, (3)
  trusts a narrow selection **only** when the command succeeded, `roots > 0`, there are
  runnable ids, the `note` raises no under-report/stale flag, and no global-impact file
  (Cargo.toml, package.json, `.github/`, …) changed — otherwise it **falls back to the full
  suite**, (4) runs the chosen tests via the configurable **`gate.test_cmd`** (e.g.
  `cargo test`), and (5) takes the verdict from the **test runner's** exit code. The
  governing rule is *"ran too much, never missed a test."* `gate.fallback` (`full-suite`
  default | `fail` | `pass-with-warning`) governs behavior under doubt; with no `test_cmd`
  the gate is **fail-closed** (refuses to pass). The requested tier is recorded in the
  ledger/`--json` output; `--gate-tier` is wired through only when a future hayven exposes
  it. This mirrors the ported reference recipe `ci/hayven-affected-tests.sh` in public
  Hayvenhurst (SAFE tier: 0 misses across ~62 replayed bugs).
- **Daemon is single-project-bound.** The daemon on `:7777` serves ONE project; a
  read/write against a workspace whose daemon is not the one on `:7777` fails with exit 1
  and `"daemon at ... serves a DIFFERENT project — refusing to mutate it"`. Consequence:
  `sirius` hayven calls only succeed when the `:7777` daemon matches the current workspace
  (start it with `hayven daemon start` in the repo). `sirius doctor`'s daemon check probes
  `GET http://localhost:7777/` for a 200 and reports the `hayven daemon status` line;
  when the workspace mismatches, forward stamping (`amt`) still lands while reverse
  stamping (`hayven remember`) reports `reverse_ok:false` — verified live.
- **`hayven --version` prints just `0.0.5`** (no `hayven ` prefix), unlike `amt --version`
  which prints `amt 0.1.0`.
