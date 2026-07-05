# Sirius Forester

> *A serious foreman who plants forests. Ametrite keeps the whiteboard,
> Hayvenhurst knows the building, and Sirius Forester hands out the jobs, checks
> the work, and files the paperwork. Every finished job plants a tree: a
> permanent, linked record of what was done, why, and what it touched.*

**Sirius Forester is a local-first fleet foreman for AI coding agents.** It runs
the loop the other two apps make possible but neither owns: **claim a task from
Ametrite, translate it into code with Hayvenhurst, lock the code, brief the
agent, gate the finish with affected-tests, and file a two-way receipt linking
the decision to the exact symbols it changed.**

Separately, [Ametrite](https://github.com) is a local issue tracker and
[Hayvenhurst](https://github.com) is a fast code index. Under Sirius Forester
they become a no-cloud fleet manager where every task is claimed exactly once,
every edit is collision-checked, every completion is test-gated, and every
change traces back to a written decision. On one machine. No accounts, no
embeddings, nothing leaving the box.

Status: **v1 specification, implementation in progress.** This is the third repo
in the suite — a standalone app, never a fork or plugin of the other two.

---

## What it is (and is not)

Sirius is exactly four things the suite was missing, and deliberately nothing
more:

- **A dispatcher** — pulls the next issue and turns it into claimed code plus a
  briefing pack.
- **A gate on the board** — an issue cannot move to done with failing
  blast-radius tests, because Sirius runs `hayven affected-tests` for the tracker.
- **Receipts** — two-way provenance: the decision lists the symbols it touched,
  and each touched symbol records the decision that governs it.
- **A fleet view** — who is working what, holding which code, blocked on what.

It is **not** an issue tracker, a code graph, CI, a merge tool, or a cloud
service. It never writes Ametrite's or Hayvenhurst's databases — only its own
audit ledger. It makes no LLM calls of its own; agents bring their own model.

---

## Requirements

- **[Ametrite](https://github.com)** — `amt` CLI, schema ≥ v3.
- **[Hayvenhurst](https://github.com)** — `hayven` CLI ≥ 0.0.5, daemon on `:7777`.
- **Rust** (stable) to build the `sirius` binary.
- **[Bun](https://bun.sh)** to run the console (`:1777`) and the `bench/` harnesses.

`sirius doctor` verifies all of these at runtime (see below).

---

## Install / build

Prebuilt binaries ship with GitHub releases (M6). To build from source:

```bash
cargo build --release
# binary at target/release/sirius
```

The console needs no build step and no npm install — it is vanilla TypeScript
run directly by Bun:

```bash
cd web && bun run start        # serves the console on http://localhost:1777
```

---

## Commands

Every mutating command accepts `--json` and prints exactly one JSON object to
stdout (logs go to stderr). Exit codes follow the Hayvenhurst convention: `0` ok,
`1` operational failure, `2` usage error, `3` soft-blocked (gate/oracle).

### `sirius init`
Creates the ledger at `.sirius/sirius.db` beside an existing `.ametrite/`. The
ledger is Sirius's only write target: run history, receipts, the worker roster,
and policy outcomes. Delete it and no work is lost — only the audit trail of how
it happened.

### `sirius doctor`
Checks the five integration-contract facts Sirius is built on, live: `amt`
present and schema ≥ v3, the Hayvenhurst daemon healthy, claim exit-code
semantics, gate exit codes, and the fleet-memory write path. Any parent release
that breaks one of these is a Sirius-blocking regression, and `sirius doctor`
is how you find out.

### `sirius link` — file a receipt (the bridge, usable with zero agents)
```bash
sirius link AMT-7 --symbols auth::verify,auth::mint
sirius link AMT-7 --changed          # resolve changed symbols from a git range
sirius link --decision D-3 --symbols ...
```
Stamps both directions: entity IDs into the issue's activity (via `amt comment`)
and a fleet-memory decision note onto each Hayvenhurst node (via
`hayven remember`). This is the one-week MVP — useful before any loop exists.

### `sirius why` — read a receipt, either direction
```bash
sirius why auth::verify     # which issues and decisions explain this function
sirius why AMT-7            # which symbols and decisions this issue touched
```

### `sirius gate` — test-gate a completion (usable by humans and CI)
```bash
sirius gate AMT-7                          # SAFE tier, advances to in_review on pass
sirius gate AMT-7 --tier safe --target-status in_review
```
Runs `hayven affected-tests --changed --gate --gate-tier safe` over the issue's
mapped entities. Pass advances the issue's status via `amt`; fail files the
failure as an issue comment and leaves the status untouched.

### `sirius run` — the loop
```bash
sirius run --workers 3 --agent-cmd "claude -p ..." --from todo
```
Runs N workers, each looping: `amt claim` → map the issue to symbols → `hayven
claim` per entity → assemble a briefing pack → spawn the agent → heartbeat both
leases → gate → file the receipt → release. **Claim order is law:** issue first,
entities second, release in reverse; a 409 on an entity releases the issue back
with a comment naming the blocker. Streams NDJSON iteration events to stdout.

---

## The Console (`:1777`)

`Bun.serve`, vanilla TypeScript, zero npm runtime dependencies. It reads the
ledger and both parent stores read-only (WAL), and mutates only by shelling to
`sirius --json`. Live updates come from `data_version` polling over SSE — the
proven Ametrite pattern. (Ametrite's own console holds `:1776`; Sirius takes
`:1777`.)

```bash
cd web && bun run start
# open http://localhost:1777
```

It shows the fleet board (workers, current issue, entities held, oracle
verdicts, gate outcomes, receipts filed) and history views from the ledger
(throughput, cycle time, gate-escape attempts, collision near-misses).

---

## Measured, not marketed

Every quantitative claim about Sirius traces to a committed harness in
[`bench/`](bench/) (PRD §2.4). Run any of them — they work **offline in fixture
mode** today, before the binary exists, and become live measurements once it
lands:

```bash
bun run bench/receipts.ts       # provenance coverage
bun run bench/gate-escape.ts    # gate-escape rate
bun run bench/soak.ts           # claim integrity (double-claims)
bun run bench/wasted-work.ts    # wasted-work ratio
bun run bench/claim-mode.ts     # always vs never vs adaptive claim modes
```

The targets these harnesses measure against (PRD §8):

| Claim | Target | Harness |
|---|---|---|
| Claim integrity | 0 double-assignments in a 30-min, 4-worker soak | `bench/soak.ts` |
| Gate escape rate | < 2% of gated completions were undetected regressions | `bench/gate-escape.ts` |
| Provenance coverage | 100% of done issues carry a two-way receipt | `bench/receipts.ts` |
| Wasted-work ceiling | < 15% of tokens on release-without-completion | `bench/wasted-work.ts` |
| Adaptive claiming | adaptive ≤ best static mode on tokens-per-completion | `bench/claim-mode.ts` |

The numbers printed today are from faithful fixture simulations of the ledger
and the parents' hard-lock/gate semantics; they show the harnesses are sound and
the targets are met **in simulation**. Live numbers on real workspaces land with
the binary. See [`bench/README.md`](bench/README.md) for exactly what each
harness measures and how.

---

## How it fits together

```
        amt --json (all writes)          HTTP :7777 + hayven CLI
                 │                                 │
   .ametrite/ametrite.db   ┌──────────────┐   Hayvenhurst daemon + index
   (Ametrite owns writes)  │    sirius    │   (read-only WAL / GET from Sirius)
                 ▲          │  one binary  │
                 │ read-only└──────┬───────┘
                 │                 │ writes only
                 │          .sirius/sirius.db  ← the ledger (Sirius's ONLY write target)
                 │                 ▲
        ┌────────┴─────────────────┴────────┐
        │   Sirius Console (Bun, :1777)      │  reads all three stores read-only;
        │   mutates by shelling sirius --json │  live via data_version SSE polling
        └────────────────────────────────────┘
```

One write path per store, and Sirius owns only its own.

---

## For agents

If you are an AI agent working in this repo, read [AGENTS.md](AGENTS.md) for
repo etiquette and — if you are the *worker* running the loop by hand — the
`.claude/skills/sirius/SKILL.md` loop-etiquette skill.

## License

MIT. See [LICENSE](LICENSE).
