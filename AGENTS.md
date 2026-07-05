# AGENTS.md — working in the Sirius Forester repo

This file is for AI agents (and humans acting like them) operating **on** this
codebase. If you are an agent being driven **by** Sirius as a worker in the loop,
read [`.claude/skills/sirius/SKILL.md`](.claude/skills/sirius/SKILL.md) instead —
that covers loop etiquette (claim → map → lock → brief → work → gate → receipt →
release).

---

## The one rule: one write path per store

Sirius's entire design rests on this (PRD §2.2). It applies to you too.

- **Never** open `.ametrite/ametrite.db` or Hayvenhurst's databases for writing.
  All Ametrite writes go through `amt … --json`. All Hayvenhurst reads/writes go
  through the `hayven` CLI or `http://localhost:7777`.
- The **only** database Sirius (and anything acting as Sirius) writes is
  `.sirius/sirius.db`, and only via `sirius` subcommands — never by hand.
- Read both parent stores **read-only** (WAL) if you need to; do not mutate them.

Breaking this corrupts other agents' coordination. There is no exception.

---

## Repository layout and ownership

The v1 build is done by three agents against a single coordination artifact,
[`CONTRACTS.md`](CONTRACTS.md). Respect the ownership table — do not edit outside
your tree:

| Area | Owns (writes) |
|---|---|
| **sirius-core** | `Cargo.toml`, `Cargo.lock`, `src/**`, the `.sirius/` ledger schema |
| **sirius-console** | `web/**` |
| **sirius-bench-docs** | `bench/**`, `.github/**`, `README.md`, `AGENTS.md`, `.claude/skills/sirius/**`, `docs/**` |

`CONTRACTS.md`, `LICENSE`, and `.gitignore` already exist
— read them, do not overwrite them. If an interface at a workstream boundary must
change, **change it in `CONTRACTS.md` first** and note it, so the others can
reconcile.

Commit your own work on a branch named `agent/<area>` (`agent/core`,
`agent/console`, `agent/bench-docs`). Do not commit to a shared branch, and do
not run git operations that touch another agent's tree while they are working.

---

## Ground truth is the installed binaries

`CONTRACTS.md` §4 and the PRD list the intended `amt`/`hayven` calls, but the
**installed CLIs are authoritative**. Before depending on a flag, check it:

```bash
amt --help
hayven --help
hayven <sub> --help
```

Pinned minimums: `amt` schema ≥ v3, `hayven` ≥ 0.0.5 with the daemon on `:7777`.
If an installed flag differs from the PRD's form, adapt to the binary and note
the drift in `CONTRACTS.md §4`. `sirius doctor` is the runtime check that the
five integration-contract facts (PRD §6) still hold.

---

## Build, test, and conventions

Follow `CONTRACTS.md §5`.

**Rust (`sirius` binary):**
```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```
Dependencies are limited to the Ametrite set: `rusqlite` (bundled), `serde`,
`serde_json`, `clap` (derive), `regex`. Adding a crate needs a note in
`CONTRACTS.md`.

**Console (`web/`):** Bun only, **zero npm runtime dependencies**, vanilla TS.
```bash
cd web && bun test && bunx tsc --noEmit
```

**Bench (`bench/`):** each harness is `bun run bench/<name>.ts`, runs offline in
fixture mode, and prints a machine-parseable `METRIC …` line. Never seed a
fixture ledger into the repo-root `.sirius/` — write it under `bench/fixtures/`
or a scratch tmpdir.

CI (`.github/workflows/ci.yml`) runs all of the above on macOS, Ubuntu, and
Windows, plus a bench smoke that asserts every harness produced a metric.

---

## `--json` is a contract, not a convenience

If you add or change a `sirius` subcommand: it must accept `--json`, print
**exactly one** JSON object to stdout (nothing else — logs go to stderr), and use
the shapes in `CONTRACTS.md §2`. `sirius run … --json` streams NDJSON, one
iteration event per line. The console parses these shapes; inventing another
stdout format breaks it silently.

---

## Provenance is not optional

Sirius exists to make work traceable. When you close a unit of work here, leave
the same trail Sirius would: link the decision to the symbols it touched (forward
and reverse), so `sirius why <symbol>` and `sirius why <issue>` both answer.
Every issue driven to done should carry a two-way receipt — that is a measured
success metric (PRD §8; `bench/receipts.ts`), not a nicety.

<!-- hayvenhurst:reflex -->
## Code navigation: prefer `hayven` over grep

This repo is indexed by Hayvenhurst. To find code, reach for `hayven` FIRST:
- `hayven query "<natural language or identifier>"` — semantic/identifier search over the code graph (faster and higher-signal than grep; never returns empty on a real query).
- `hayven neighbors <id>` — callers/callees of a node (follow the call graph instead of guessing).
- `hayven view` — open the browser graph.
Fall back to grep only when hayven has no answer. Run `hayven reindex` after large changes if results look stale.
<!-- /hayvenhurst:reflex -->
