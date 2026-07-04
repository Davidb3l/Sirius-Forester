# `bench/` — Measurement harnesses

Every quantitative claim about Sirius Forester traces to a harness here (PRD
§2.4: *measured, not marketed*). Each harness measures **one** number tied to a
PRD §8 success metric, prints it, and exits.

All harnesses run **offline** in **fixture mode** — the `sirius` binary, the
`amt`/`hayven` parents, and a live ledger are not required. Fixture mode seeds a
faithful in-memory model of the ledger (`lib/ledger.ts`, mirroring CONTRACTS.md
§1) and of the parents' hard-lock / gate semantics (CONTRACTS.md §6), so the
harnesses are runnable today and become live measurements once the binary lands.

```
bun run bench/receipts.ts
bun run bench/gate-escape.ts
bun run bench/soak.ts
bun run bench/wasted-work.ts
bun run bench/claim-mode.ts
```

Add `--enforce` to any harness to make it exit non-zero when the metric misses
its target (used by CI's optional gating; the default is report-only so a smoke
run never fails on a simulated number).

---

## Harness → PRD §8 metric map

| Harness | PRD §8 metric | Target | Prints (fixture mode) |
|---|---|---|---|
| `receipts.ts` | **Provenance coverage** | 100% of done issues carry a two-way receipt | `provenance_coverage` = 100% |
| `gate-escape.ts` | **Gate escape rate** | < 2% of gated completions were undetected regressions | `gate_escape_rate` ≈ 1.0–1.8% |
| `soak.ts` | **Claim integrity** | 0 double-assignments, 30-min / 4-worker soak | `double_claims` = 0 |
| `wasted-work.ts` | **Wasted-work ceiling** | < 15% of tokens on release-without-completion | `wasted_work_ratio` ≈ 7–13% |
| `claim-mode.ts` | **Policy engine / R5 lesson** (M5) | adaptive ≤ best static mode on tokens-per-completion | per-mode table + `adaptive_tokens_per_completion_savings` |

> **Loop overhead** (PRD §8: < 1 s Sirius-added latency per iteration) is the one
> §8 metric with no standalone harness here: it is a wall-clock measurement of
> the real `sirius run` loop and belongs to the core binary's own timing test
> once `sirius run` exists. The soak harness reports wall-clock per run as a
> placeholder signal.

---

## What each harness actually does

### `receipts.ts` — provenance coverage
Seeds a ledger of completed iterations, every one filing a two-way receipt
(`forward_ok=1` **and** `reverse_ok=1`, CONTRACTS.md §1 `receipts`). Also seeds
non-done outcomes (`released`, `gate_failed`) that must **not** count against
coverage. Metric = done iterations with a two-way receipt ÷ done iterations.
`--broken` drops one reverse stamp to prove the metric detects sub-100%.

### `gate-escape.ts` — gate-escape rate
Replays a corpus of known regressions (default 95, across 4 repos — mirroring
Hayvenhurst's published SAFE-tier evaluation) through a simulated
`hayven affected-tests --changed --gate --gate-tier safe`. An **escape** is a
regression the SAFE tier fails to select a test for (gate exit 0). The simulated
miss rate is tuned to Hayvenhurst's ~1.8% observed floor, so the number sits
realistically just under the 2% target rather than a suspicious 0%. `--n=<k>`
grows the corpus.

### `soak.ts` — claim integrity
Drives 4 workers through the full claim → … → release loop against a deliberately
**contentious** shared pool (12 hot issues, 20 hot entities), interleaved tick by
tick so workers genuinely hold resources while others try to claim them. An
independent shadow auditor (separate from the lock under test) counts any moment
two distinct workers hold the same resource. Enforces the claim-order law (issue
first, entities second; release in reverse; 409 → release issue). Reports
`double_claims` (target 0) alongside the number of correctly-rejected contended
claims, proving contention was real. Duration is parameterizable:

```
bun run bench/soak.ts                 # short, CI default (~ms, 800 sim iterations)
bun run bench/soak.ts --duration=30m  # full 30-minute-equivalent workload
bun run bench/soak.ts --duration=30m --realtime   # actually pace to wall-clock
bun run bench/soak.ts --workers=4 --duration=90s
```

### `wasted-work.ts` — wasted-work ceiling
Seeds a ledger under a chosen contention profile and sums tokens on iterations
whose outcome is **not** `completed` (released / deadend / gate_failed / error).
The token cost model reflects the claim-order payoff: a 409 release costs almost
nothing (backed off before the agent ran) while post-work failures cost a full
agent pass. Metric = wasted tokens ÷ total tokens.

```
bun run bench/wasted-work.ts
bun run bench/wasted-work.ts --contention=high   # more 409s + deadends
```

### `claim-mode.ts` — policy comparison (M5)
Replays **one** seeded contentious workload under all three
`claim_mode` values (`always` / `never` / `adaptive`, CONTRACTS.md §3) and
compares **tokens per completed issue** — the honest efficiency lens, since
completion counts are held equal across modes. Cost model: `always` pays
per-entity claim overhead over the whole blast radius (cheap on collisions, but
constant coordination tax on cold code); `never` skips claims but pays a full
wasted agent pass on every collision redo; `adaptive` claims only entities the
ledger has seen contended and skips the rest.

Result (this is the R5 lesson made measurable): at **low** contention `never`
wins and blanket claim-first is a needless tax; from **moderate** contention up,
`adaptive` is cheaper per completion than **both** static modes. Adaptive is
deliberately *not* a universal winner — at very low contention it trails
`never` slightly while it learns, which is the truthful outcome.

```
bun run bench/claim-mode.ts                 # default contention 0.35 (adaptive wins)
bun run bench/claim-mode.ts --contention=0.1   # low: never-claim wins
bun run bench/claim-mode.ts --contention=0.6   # high: adaptive wins clearly
```

---

## Shared library (`bench/lib/`)

| File | Purpose |
|---|---|
| `ledger.ts` | In-memory `FixtureLedger` mirroring CONTRACTS.md §1 tables 1:1, plus a seeded PRNG so every workload is reproducible. |
| `sirius.ts` | `siriusAvailable()` + `runSiriusJson()` — the bridge to the real binary (CONTRACTS.md §2 shapes) for a future `--live` mode. Harnesses fall back to fixture mode when the binary is absent. |
| `report.ts` | Uniform metric reporting. Every harness ends with a machine-parseable `METRIC <name> value=… pass=… mode=…` line that CI's bench-smoke step asserts on. |

## Fixtures on disk

Harnesses that need an on-disk sample ledger write it under `bench/fixtures/`
or a scratch tmpdir — **never** to the repo-root `.sirius/`, which the core
agent owns. The current harnesses use the in-memory model, so `bench/fixtures/`
is usually empty.

## Assumptions the core binary must honor

For `--live` mode (not exercised offline) these `sirius --json` guarantees from
CONTRACTS.md §2 must hold:

- Every mutating subcommand accepts `--json` and prints exactly **one** JSON
  object to stdout; logs go to stderr.
- `sirius run … --json` streams NDJSON, one iteration event per line.
- Exit codes: `0` ok, `1` operational failure, `2` usage error, `3` soft-blocked.
- The ledger schema matches CONTRACTS.md §1 (harnesses query `iterations`,
  `receipts`, `outcome`, `forward_ok`, `reverse_ok`, `tokens`).
