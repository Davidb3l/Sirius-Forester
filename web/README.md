# Sirius Console (`web/`)

The Sirius Forester web console — Milestone **M4**. `Bun.serve`, vanilla
TypeScript, **zero npm runtime dependencies** (Bun built-ins only). Port **:1777**.

A bystander should answer *"who is doing what, and is anything blocked"* in one
glance, without a terminal.

## Run it

Against a seeded fixture ledger (no `sirius` binary required):

```sh
bun run demo          # seeds fixtures/.sirius/sirius.db and serves :1777
```

Or step by step:

```sh
bun run seed                                   # -> fixtures/.sirius/sirius.db
SIRIUS_LEDGER="$PWD/fixtures/.sirius/sirius.db" bun run src/server.ts
```

Against a real workspace (once `sirius init` has created `.sirius/sirius.db`):

```sh
cd /path/to/repo && bun run /path/to/web/src/server.ts   # walks up for .sirius/
```

Quality gates:

```sh
bun test            # 29 tests
bun run typecheck   # tsc --noEmit, clean
```

## What it shows

- **Fleet board** — one card per worker: current issue (+ title if the Ametrite
  store is readable), code entities held, per-entity oracle verdicts
  (`registered`/`blocked`/`forced`), gate status, and the filed receipt with a
  two-way ✓✓ / partial ◐ flag. Live via SSE.
- **History** — throughput/hr, median & avg cycle time, gate escape attempts
  (`gate_result='fail'`), collision near-misses (`blocked` verdicts +
  `backoff_409` policy events), two-way receipt coverage, tokens; plus a recent
  iterations table and a policy-events table.
- **Receipts** — click any receipt (in the board or the table) to open a drawer
  with its issue/decision, the symbols stamped, forward/reverse provenance, the
  iterations that filed it, and `sirius why <ref>` enrichment when the binary is
  present.
- **Config** — read-only view of `.sirius/config.json`, marking which keys come
  from the file vs. the committed defaults. Falls back to defaults when absent.

## Architecture (mirrors Ametrite's web app)

```
src/db.ts       workspace discovery + read-only WAL SQLite opens; PRAGMA data_version
src/schema.ts   CONTRACTS §1 ledger DDL (used only by the fixture seeder + tests)
src/ledger.ts   typed read queries over the ledger (fleet, history, receipts)
src/stores.ts   best-effort read-only enrichment from the parent Ametrite store
src/config.ts   .sirius/config.json reader with §3 committed defaults
src/sirius.ts   THE `sirius --json` shell-out boundary (swap-in point for the real binary)
src/sse.ts      data_version poller -> text/event-stream
src/api.ts      pure JSON payload assembly for the frontend
src/server.ts   Bun.serve router
public/         index.html + app.css + app.js (no external assets, CSP-safe)
fixtures/seed.ts  writes a sample ledger matching CONTRACTS §1
```

**Write discipline (PRD §2.2):** the console opens all three stores **read-only**
and never writes any SQLite. The *only* mutation path is shelling to
`sirius <cmd> --json` via `src/sirius.ts`.

## HTTP endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/` , `/app.css`, `/app.js` | static console |
| GET | `/events` | SSE — `version` events on `data_version` change |
| GET | `/api/fleet` | fleet board JSON |
| GET | `/api/history` | history stats + recent iterations + policy events |
| GET | `/api/receipts` | receipt list |
| GET | `/api/receipt/:id` | receipt detail + filing iterations + `sirius why` |
| GET | `/api/config` | `.sirius/config.json` (or defaults) |
| GET | `/api/doctor` | shells `sirius doctor --json` |
| GET | `/api/health` | liveness + ledger availability |
| POST | `/api/gate` | shells `sirius gate <issue> --json` |
| POST | `/api/link` | shells `sirius link … --json` |

## Integration once the real `sirius` binary lands

1. Delete/ignore `fixtures/.sirius/`; run the server with no `SIRIUS_LEDGER` env
   from inside a repo that has `.sirius/sirius.db` — `discoverWorkspace()` walks
   up to find it (and `.ametrite/`, `.hayven/`).
2. `src/sirius.ts` already resolves the binary via `$SIRIUS_BIN` or `sirius` on
   `PATH`. No other file shells out. The `why` enrichment and the `POST`
   mutation routes light up automatically once the binary exists.
3. The ledger schema this console reads is `src/schema.ts`, pinned to
   CONTRACTS §1 `schema_version = 1`. If `sirius init` bumps the schema, update
   the queries in `src/ledger.ts` and the DDL/seed together.
