/**
 * bench/receipts.ts  —  Provenance coverage harness
 *
 * PRD §8: "Provenance coverage: 100% of issues moved to done through Sirius
 * carry a two-way receipt."
 *
 * Metric: provenance_coverage = (done iterations with a two-way receipt)
 *                               / (done iterations) * 100, target 100%.
 *
 * A "two-way receipt" is a receipts row with forward_ok=1 (amt comment landed)
 * AND reverse_ok=1 (hayven remember landed), referenced by the iteration's
 * receipt_id. See CONTRACTS.md §1 (receipts table) and PRD §4 (Receipt).
 *
 * Fixture mode (default, offline): seeds a ledger of N completed iterations
 * where every completion files a two-way receipt, plus a couple of NON-done
 * outcomes (released/gate_failed) that must NOT count against coverage.
 *
 * Live mode (SIRIUS_BIN present + --live): reads the real ledger instead.
 * Not exercised offline; the query shape is documented for the core agent.
 *
 * Run:  bun run bench/receipts.ts
 *       bun run bench/receipts.ts --enforce      # non-zero exit if < 100%
 *       bun run bench/receipts.ts --broken        # inject a gap to prove the
 *                                                   metric can detect failure
 */

import { FixtureLedger } from "./lib/ledger";
import { finish, argFlag } from "./lib/report";
import { siriusAvailable } from "./lib/sirius";

function seedLedger(injectGap: boolean): FixtureLedger {
  const led = new FixtureLedger();
  const oak = led.addWorker("sirius/oak").id;
  const rowan = led.addWorker("sirius/rowan").id;

  const workers = [oak, rowan];
  const DONE = 40; // issues driven to done through Sirius

  for (let i = 0; i < DONE; i++) {
    const worker = workers[i % workers.length];
    const ref = `AMT-${100 + i}`;
    const symbols = [`ent:${i}:a`, `ent:${i}:b`];

    // Model the completion filing a two-way receipt. In fixture mode the happy
    // path always stamps both directions. The --broken flag drops the reverse
    // stamp on one issue to prove the harness detects sub-100% coverage.
    const brokenHere = injectGap && i === 7;
    const receipt = led.addReceipt({
      kind: "issue",
      ref,
      symbols,
      forward_ok: 1,
      reverse_ok: brokenHere ? 0 : 1,
      created_at: new Date().toISOString(),
      worker_id: worker,
    });

    led.addIteration({
      worker_id: worker,
      issue_ref: ref,
      entities: symbols,
      started_at: new Date().toISOString(),
      ended_at: new Date().toISOString(),
      outcome: "completed",
      gate_result: "pass",
      oracle_verdicts: symbols.map(() => "registered"),
      tokens: 1800,
      duration_ms: 42000,
      receipt_id: receipt.id,
    });
  }

  // Non-done outcomes: these did NOT move an issue to done, so they are outside
  // the coverage denominator and must not drag the number down.
  led.addIteration({
    worker_id: oak,
    issue_ref: "AMT-900",
    entities: ["ent:x"],
    started_at: new Date().toISOString(),
    ended_at: new Date().toISOString(),
    outcome: "released", // 409 on an entity claim → released the issue
    gate_result: null,
    oracle_verdicts: ["blocked"],
    tokens: 300,
    duration_ms: 4000,
    receipt_id: null,
  });
  led.addIteration({
    worker_id: rowan,
    issue_ref: "AMT-901",
    entities: ["ent:y"],
    started_at: new Date().toISOString(),
    ended_at: new Date().toISOString(),
    outcome: "gate_failed", // gate blocked it; status untouched
    gate_result: "fail",
    oracle_verdicts: ["registered"],
    tokens: 900,
    duration_ms: 15000,
    receipt_id: null,
  });

  return led;
}

async function main() {
  const broken = argFlag("broken");
  const live = argFlag("live") && (await siriusAvailable());

  if (live) {
    // Documented intent for the core agent. The real query is:
    //   SELECT count(*) FROM iterations i WHERE i.outcome='completed';
    //   SELECT count(*) FROM iterations i JOIN receipts r ON r.id=i.receipt_id
    //     WHERE i.outcome='completed' AND r.forward_ok=1 AND r.reverse_ok=1;
    // Until the binary + ledger exist we fall through to fixture mode.
    console.error(
      "[receipts] live mode requested but reading the real ledger is not " +
        "wired yet; falling back to fixture mode.",
    );
  }

  const led = seedLedger(broken);
  const done = led.doneIterations();
  const withTwoWay = done.filter((it) => {
    const r = led.receiptFor(it);
    return r != null && r.forward_ok === 1 && r.reverse_ok === 1;
  });

  const coverage = done.length === 0 ? 100 : (withTwoWay.length / done.length) * 100;

  finish({
    name: "provenance_coverage",
    value: Number(coverage.toFixed(2)),
    unit: "%",
    target: "100% of done issues carry a two-way receipt",
    pass: coverage >= 100,
    mode: "fixture",
    detail: {
      done_iterations: done.length,
      with_two_way_receipt: withTwoWay.length,
      missing_forward: done.filter((i) => led.receiptFor(i)?.forward_ok !== 1).length,
      missing_reverse: done.filter((i) => led.receiptFor(i)?.reverse_ok !== 1).length,
      non_done_iterations: led.iterations.length - done.length,
    },
  });
}

main();
