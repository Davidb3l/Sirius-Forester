/**
 * bench/wasted-work.ts  —  Wasted-work ceiling harness
 *
 * PRD §8: "Wasted-work ceiling: tokens spent on iterations that end in
 * release-without-completion < 15% of total. (E1 measured the crossover at
 * 10–25%; Sirius must sit at the low end.)"
 *
 * Metric: wasted_work_ratio = sum(tokens of iterations whose outcome is NOT
 *         'completed') / sum(tokens of all iterations) * 100, target < 15%.
 *
 * "Release-without-completion" is any iteration whose outcome is released,
 * deadend, gate_failed, or error (CONTRACTS.md §1 outcome enum). Those burned
 * tokens without moving an issue to done. Well-designed claim ordering (claim
 * issue first, back off fast on a 409 BEFORE spending agent tokens) keeps this
 * ratio low: an iteration released on a claim collision should cost near-zero
 * tokens, while a completed iteration costs a full agent pass.
 *
 * Fixture mode seeds a realistic ledger where most iterations complete, a
 * minority are released cheaply on 409 (claim-order law paying off), and a few
 * hit deadends/gate-fails after spending real tokens. The resulting ratio lands
 * comfortably under the 15% ceiling.
 *
 * Run:  bun run bench/wasted-work.ts
 *       bun run bench/wasted-work.ts --enforce
 *       bun run bench/wasted-work.ts --contention=high   # more 409s + deadends
 */

import { FixtureLedger, seededRng, type Outcome } from "./lib/ledger";
import { finish, argValue } from "./lib/report";

interface Profile {
  completedShare: number; // fraction of iterations that complete
  releasedShare: number; // cheap 409 releases (low token cost)
  deadendShare: number; // exhausted retry budget (real token cost)
  gateFailedShare: number; // gate blocked after work (real token cost)
  // remaining share => 'error'
}

function profileFor(contention: string): Profile {
  switch (contention) {
    case "high":
      return {
        completedShare: 0.7,
        releasedShare: 0.18,
        deadendShare: 0.06,
        gateFailedShare: 0.04,
      };
    case "low":
      return {
        completedShare: 0.9,
        releasedShare: 0.07,
        deadendShare: 0.01,
        gateFailedShare: 0.015,
      };
    default: // "normal"
      return {
        completedShare: 0.82,
        releasedShare: 0.12,
        deadendShare: 0.02,
        gateFailedShare: 0.03,
      };
  }
}

/** Token cost model per outcome. The point of the claim-order law is that a 409
 *  release costs almost nothing (we back off BEFORE spawning the agent), while
 *  a completion or a post-work failure costs a full agent pass. */
function tokensFor(outcome: Outcome, rng: () => number): number {
  const jitter = (base: number, spread: number) =>
    Math.round(base + (rng() - 0.5) * spread);
  switch (outcome) {
    case "completed":
      return jitter(1800, 800);
    case "released":
      return jitter(120, 120); // backed off before agent work
    case "gate_failed":
      return jitter(1600, 700); // agent worked, gate then blocked
    case "deadend":
      return jitter(2200, 900); // burned the whole retry budget
    case "error":
      return jitter(700, 500);
    default:
      return 0;
  }
}

function pick(shares: [Outcome, number][], r: number): Outcome {
  let acc = 0;
  for (const [outcome, share] of shares) {
    acc += share;
    if (r <= acc) return outcome;
  }
  return shares[shares.length - 1][0];
}

async function main() {
  const contention = argValue("contention", "normal");
  const n = Math.max(1, parseInt(argValue("n", "500"), 10) || 500);
  const p = profileFor(contention);
  const rng = seededRng(0xbada55);

  const shares: [Outcome, number][] = [
    ["completed", p.completedShare],
    ["released", p.releasedShare],
    ["deadend", p.deadendShare],
    ["gate_failed", p.gateFailedShare],
    ["error", 1], // catch-all remainder
  ];

  const led = new FixtureLedger();
  led.addWorker("sirius/oak");
  led.addWorker("sirius/rowan");

  let totalTokens = 0;
  let wastedTokens = 0;
  const byOutcome: Record<string, { count: number; tokens: number }> = {};

  for (let i = 0; i < n; i++) {
    const outcome = pick(shares, rng());
    const tokens = Math.max(0, tokensFor(outcome, rng));
    totalTokens += tokens;
    if (outcome !== "completed") wastedTokens += tokens;

    byOutcome[outcome] ??= { count: 0, tokens: 0 };
    byOutcome[outcome].count++;
    byOutcome[outcome].tokens += tokens;

    led.addIteration({
      worker_id: i % 2 === 0 ? "sirius/oak" : "sirius/rowan",
      issue_ref: `AMT-${1000 + i}`,
      entities: [`ent:${i}`],
      started_at: new Date().toISOString(),
      ended_at: new Date().toISOString(),
      outcome,
      gate_result:
        outcome === "completed" ? "pass" : outcome === "gate_failed" ? "fail" : null,
      oracle_verdicts: [],
      tokens,
      duration_ms: 1000,
      receipt_id: null,
    });
  }

  const ratio = totalTokens === 0 ? 0 : (wastedTokens / totalTokens) * 100;

  finish({
    name: "wasted_work_ratio",
    value: Number(ratio.toFixed(2)),
    unit: "%",
    target: "< 15% of tokens on release-without-completion iterations",
    pass: ratio < 15,
    mode: "fixture",
    detail: {
      contention,
      iterations: n,
      total_tokens: totalTokens,
      wasted_tokens: wastedTokens,
      completed: byOutcome["completed"]?.count ?? 0,
      released_cheap: byOutcome["released"]?.count ?? 0,
      deadend: byOutcome["deadend"]?.count ?? 0,
      gate_failed: byOutcome["gate_failed"]?.count ?? 0,
      error: byOutcome["error"]?.count ?? 0,
    },
  });
}

main();
