# Architecture & Integration Contract

This is the reader's companion to [PRD.md](../PRD.md) and
[CONTRACTS.md](../CONTRACTS.md): how the three stores fit together, what Sirius
is allowed to write, and the five facts the whole design rests on. Every
quantitative claim here points at a harness in [`bench/`](../bench/).

## Three stores, one write path each

```
        amt --json (all writes)              HTTP :7777 + hayven CLI
                 │                                     │
   .ametrite/ametrite.db   ┌──────────────────┐   Hayvenhurst daemon + index
   Ametrite owns writes    │   sirius (Rust)   │   Sirius reads read-only (WAL/GET)
                 ▲          │  CLI · loop ·     │
      read-only  │          │  policy · bridge  │
                 │          └─────────┬─────────┘
                 │                    │ writes ONLY here
                 │             .sirius/sirius.db   ← the ledger
                 │                    ▲
        ┌────────┴────────────────────┴─────────┐
        │   Sirius Console  (Bun.serve, :1777)   │
        │   reads all three read-only;           │
        │   mutates by shelling `sirius --json`  │
        └────────────────────────────────────────┘
```

- **Ametrite** owns issues, claims, decisions, the activity log. Sirius writes it
  only through `amt … --json`.
- **Hayvenhurst** owns the code graph, entity claims, context packs, the
  affected-tests gate, and fleet memory. Sirius uses the `hayven` CLI or
  `http://localhost:7777`.
- **The ledger** (`.sirius/sirius.db`) is Sirius's **only** write target: run
  history, receipts, the worker roster, policy outcomes. It is an audit log, not
  a second brain — delete it and no work is lost, only the trail of how it
  happened. It is git-ignored (self-ignoring `.gitignore` containing `*`) and
  branch-invariant, because the ledger is a fact about work, not a code snapshot.

The design invariant (PRD §2.2): **one write path per store, and Sirius owns only
its own.** Sirius never opens either parent's SQLite for writing.

## The ledger schema

Five tables (`meta`, `workers`, `iterations`, `receipts`, `policy_events`),
defined authoritatively in [CONTRACTS.md §1](../CONTRACTS.md). The bench
harnesses model these tables 1:1 (`bench/lib/ledger.ts`) so their measurements
match what the real ledger will report. The console reads the same tables
read-only and polls `PRAGMA data_version` for live SSE updates.

## The five contract facts (`sirius doctor` checks these live)

Sirius is built on facts verified in both parent codebases (PRD §6). Any parent
release that breaks one is a Sirius-blocking regression:

1. **Ametrite claims are hard locks.** `BEGIN IMMEDIATE`; zero double-claims
   under a 4-claimer race. Leases default 900 s; re-claiming your own id is a
   heartbeat. → measured by [`bench/soak.ts`](../bench/soak.ts).
2. **Hayvenhurst claims are hard locally.** Overlap returns a synchronous 409 at
   the daemon. Only Layer C oracle adjacency verdicts are soft (202, force-able).
3. **Fleet memory is a plain supported write, not CRDT-synced.** So reverse
   provenance needs no CRDT surgery and stays machine-local.
4. **The gate exists and is measured.** `hayven affected-tests --changed --gate
   --gate-tier safe`: exit 0 pass, exit 1 fail; SAFE tier measured at 0 missed
   regressions across ~95 replayed bugs on 4 repos. → tracked by
   [`bench/gate-escape.ts`](../bench/gate-escape.ts).
5. **Every Ametrite mutation has a `--json` CLI form; every Hayvenhurst
   read/write has a CLI or `:7777` form.** The old MCP/proxy surfaces are NOT
   depended on.

## One iteration, exactly

The loop is claim → map → lock → brief → work → gate → receipt → release. The
full reference sequence is [PRD §9](../PRD.md); the by-hand version for a single
worker agent is
[`.claude/skills/sirius/SKILL.md`](../.claude/skills/sirius/SKILL.md). Two rules
carry the coordination guarantees:

- **Claim order is law:** issue first, entities second, release in reverse. A 409
  on an entity releases the issue back with a comment naming the blocker — never
  hold an issue while spinning on a code lock.
- **The gate is on the board:** an issue cannot reach `in_review` through Sirius
  with a failing SAFE tier.

## What we measure (and where)

| Metric (PRD §8) | Target | Harness |
|---|---|---|
| Claim integrity | 0 double-assignments, 30-min / 4-worker soak | [`bench/soak.ts`](../bench/soak.ts) |
| Gate escape rate | < 2% undetected regressions | [`bench/gate-escape.ts`](../bench/gate-escape.ts) |
| Provenance coverage | 100% of done issues carry a two-way receipt | [`bench/receipts.ts`](../bench/receipts.ts) |
| Wasted-work ceiling | < 15% tokens on release-without-completion | [`bench/wasted-work.ts`](../bench/wasted-work.ts) |
| Adaptive claiming (M5, R5 lesson) | adaptive ≤ best static mode / completion | [`bench/claim-mode.ts`](../bench/claim-mode.ts) |
| Loop overhead | < 1 s Sirius-added latency / iteration | timed in the live `sirius run` (no fixture harness) |

Today the harnesses report **fixture-mode** numbers — faithful simulations of the
ledger and the parents' hard-lock/gate semantics — so the targets are met in
simulation and the harnesses are proven sound. Live numbers on real workspaces
arrive with the binary. See [`bench/README.md`](../bench/README.md) for the full
methodology of each.
