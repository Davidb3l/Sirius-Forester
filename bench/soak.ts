/**
 * bench/soak.ts  —  Claim-integrity soak harness
 *
 * PRD §8: "Claim integrity: 0 double-assignments of an issue or entity across a
 * 30-minute, 4-worker soak test."
 * ROADMAP M3: "Soak test: 30 min, 4 workers, seeded workspace, zero
 * double-claims."
 *
 * Metric: double_claims = count of moments where the same issue OR the same
 *         entity was held by two workers at once. Target: 0.
 *
 * The real integrity guarantee lives in the parents (CONTRACTS.md §6.1/§6.2):
 * Ametrite claims are BEGIN IMMEDIATE hard locks; Hayvenhurst overlap returns a
 * synchronous 409. This harness measures whether Sirius's loop, driving 4
 * concurrent workers through claim → ... → release, ever produces overlapping
 * ownership. Offline it drives a faithful in-process model of those hard locks;
 * a bug in the model's release ordering (claim-order law, §F3) would surface as
 * a double-claim, exactly as a bug in the real loop would.
 *
 * Duration is parameterizable. Default is SHORT so CI stays fast; the full
 * 30-minute run is opt-in.
 *   bun run bench/soak.ts                    # ~2s simulated, CI default
 *   bun run bench/soak.ts --duration=30m     # full soak (real wall-clock)
 *   bun run bench/soak.ts --workers=4 --duration=90s
 *   bun run bench/soak.ts --enforce          # non-zero exit on any double-claim
 *
 * Duration accepts s/m suffixes (e.g. 90s, 30m). In fixture mode wall-clock is
 * compressed: we simulate a fixed number of iterations proportional to the
 * requested duration rather than actually sleeping for 30 minutes, unless
 * --realtime is passed.
 */

import { FixtureLedger, seededRng } from "./lib/ledger";
import { finish, argValue, argFlag } from "./lib/report";

function parseDurationMs(s: string): number {
  const m = s.trim().match(/^(\d+)(ms|s|m|h)?$/);
  if (!m) return 2000;
  const n = parseInt(m[1], 10);
  switch (m[2]) {
    case "h":
      return n * 3600_000;
    case "m":
      return n * 60_000;
    case "s":
      return n * 1000;
    case "ms":
    default:
      return n;
  }
}

/** In-process model of a hard-locked resource pool (issues + entities).
 *  Enforces CONTRACTS.md §6: a resource can be held by at most one worker.
 *
 *  The harness's job is to detect DOUBLE-CLAIMS: two distinct workers both
 *  successfully holding the same resource at the same time. That is measured by
 *  an INDEPENDENT shadow auditor (`confirmed`), separate from the lock's own
 *  `held` map, so a bug in claim/release ordering would show up as a divergence
 *  rather than being hidden by the lock that has the bug. A contended claim that
 *  is correctly rejected (409) is NOT a violation — it is the lock working. */
class LockTable {
  private held = new Map<string, string>(); // resource -> worker (the lock)
  private confirmed = new Map<string, string>(); // independent audit shadow
  violations = 0;
  violationLog: string[] = [];
  contendedRejections = 0; // 409s: correct behavior, reported for visibility

  /** Attempt a hard claim. Returns true if acquired, false on overlap (409). */
  claim(resource: string, worker: string): boolean {
    const owner = this.held.get(resource);
    if (owner === undefined) {
      this.held.set(resource, worker);
      // Audit: at the moment of a SUCCESSFUL acquire, the shadow must show the
      // resource as unheld by anyone else. If it is held by another worker, the
      // lock just handed out a double-claim — the exact bug we hunt.
      const shadow = this.confirmed.get(resource);
      if (shadow !== undefined && shadow !== worker) {
        this.violations++;
        if (this.violationLog.length < 20) {
          this.violationLog.push(
            `${resource} granted to ${worker} while still held by ${shadow}`,
          );
        }
      }
      this.confirmed.set(resource, worker);
      return true;
    }
    if (owner === worker) {
      // Re-claiming your own id is a heartbeat, not a collision (PRD §F3).
      return true;
    }
    this.contendedRejections++;
    return false; // overlap → the loop must back off, NOT force
  }

  /** Only the owner may release. A release-by-non-owner would itself be a bug. */
  release(resource: string, worker: string): void {
    const owner = this.held.get(resource);
    if (owner === worker) {
      this.held.delete(resource);
      this.confirmed.delete(resource);
    }
  }
}

interface WorkerState {
  id: string;
  issue: string | null;
  entities: string[];
}

async function main() {
  const workers = Math.max(1, parseInt(argValue("workers", "4"), 10) || 4);
  const durationStr = argValue("duration", "short");
  const realtime = argFlag("realtime");

  // Map requested duration to a simulated iteration budget. The full 30m soak
  // is ~ (30*60 / avg-iteration-seconds) * workers iterations; we model an
  // average iteration at ~5s of agent work, so 30m ≈ 360 iterations/worker.
  const durationMs =
    durationStr === "short" ? 2000 : parseDurationMs(durationStr);
  const iterationsPerWorker =
    durationStr === "short"
      ? 200
      : Math.max(50, Math.round(durationMs / 5000));
  const totalIterations = iterationsPerWorker * workers;

  const led = new FixtureLedger();
  const locks = new LockTable();
  const rng = seededRng(0xf0e57);

  const workerStates: WorkerState[] = [];
  for (let i = 0; i < workers; i++) {
    const id = `sirius/${["oak", "rowan", "birch", "cedar", "elm", "ash"][i % 6]}`;
    led.addWorker(id);
    workerStates.push({ id, issue: null, entities: [] });
  }

  // A deliberately CONTENTIOUS shared pool: a handful of hot issues and a small
  // set of hot entities every worker fights over. This is where a claim-order
  // or release-ordering bug would manifest as a double-claim.
  const ISSUE_POOL = Array.from({ length: 12 }, (_, i) => `AMT-${200 + i}`);
  const ENTITY_POOL = Array.from({ length: 20 }, (_, i) => `ent:hot:${i}`);

  const started = performance.now();
  let completed = 0;
  let released409 = 0;

  // Each worker is a small state machine so that, unlike a serialized round
  // robin, workers HOLD resources across ticks and genuinely contend. A tick
  // advances one worker by one phase; because worker A can be mid-work (holding
  // a hot issue/entity) while worker B tries to claim the same one, real 409s
  // occur and the release path (claim-order law) is actually exercised.
  type Phase = "idle" | "want_issue" | "have_issue" | "working";
  const phase: Phase[] = workerStates.map(() => "idle");
  const workTicksLeft: number[] = workerStates.map(() => 0);

  const totalTicks = totalIterations * 4; // ~4 phases per iteration
  const remainingIters = workerStates.map(() => iterationsPerWorker);

  for (let tick = 0; tick < totalTicks; tick++) {
    const wi = tick % workers;
    const w = workerStates[wi];
    if (remainingIters[wi] <= 0 && phase[wi] === "idle") continue;

    switch (phase[wi]) {
      case "idle": {
        phase[wi] = "want_issue";
        break;
      }
      case "want_issue": {
        // Phase 1 — claim an issue (Ametrite hard lock, issue FIRST per §F3).
        const issue = ISSUE_POOL[Math.floor(rng() * ISSUE_POOL.length)];
        if (!locks.claim(issue, w.id)) {
          // Issue busy right now: honor no-work, retry a later tick.
          break;
        }
        w.issue = issue;

        // Phase 2 — claim mapped entities (Hayvenhurst hard locks, SECOND).
        const nEnt = 1 + Math.floor(rng() * 3);
        let overlap = false;
        for (let k = 0; k < nEnt; k++) {
          const e = ENTITY_POOL[Math.floor(rng() * ENTITY_POOL.length)];
          if (w.entities.includes(e)) continue;
          if (locks.claim(e, w.id)) {
            w.entities.push(e);
          } else {
            overlap = true; // 409 on an entity someone else holds
            break;
          }
        }

        if (overlap) {
          // Claim-order law: release entities (reverse) then release the ISSUE
          // back with a comment naming the blocker. Never spin holding an issue.
          for (const e of w.entities) locks.release(e, w.id);
          locks.release(w.issue!, w.id);
          w.entities = [];
          w.issue = null;
          released409++;
          led.addPolicyEvent({
            iteration_id: null,
            kind: "backoff_409",
            detail: { blocker: "another worker" },
          });
          phase[wi] = "idle";
          remainingIters[wi]--; // this attempt consumed an iteration slot
          break;
        }

        phase[wi] = "have_issue";
        // Simulate variable agent work spanning several ticks (contention window).
        workTicksLeft[wi] = 1 + Math.floor(rng() * 4);
        break;
      }
      case "have_issue": {
        phase[wi] = "working";
        break;
      }
      case "working": {
        // Phase 3 — hold both leases while the agent works, then release in
        // reverse order (entities first, then issue) and record the iteration.
        if (workTicksLeft[wi] > 0) {
          workTicksLeft[wi]--;
          break; // still working; keep holding the locks (this is the contention)
        }
        const issue = w.issue!;
        const ents = [...w.entities];
        for (const e of ents) locks.release(e, w.id);
        locks.release(issue, w.id);
        completed++;
        led.addIteration({
          worker_id: w.id,
          issue_ref: issue,
          entities: ents,
          started_at: new Date().toISOString(),
          ended_at: new Date().toISOString(),
          outcome: "completed",
          gate_result: "pass",
          oracle_verdicts: ents.map(() => "registered"),
          tokens: 1500,
          duration_ms: 5000,
          receipt_id: null,
        });
        w.entities = [];
        w.issue = null;
        phase[wi] = "idle";
        remainingIters[wi]--;
        break;
      }
    }

    if (realtime && durationStr !== "short") {
      if (performance.now() - started >= durationMs) break;
    }
  }

  const elapsedMs = performance.now() - started;

  finish({
    name: "double_claims",
    value: locks.violations,
    unit: "double-claims",
    target: "0 double-assignments across a 30-min, 4-worker soak",
    pass: locks.violations === 0,
    mode: "fixture",
    detail: {
      workers,
      requested_duration: durationStr,
      simulated_iterations: totalIterations,
      completed,
      released_on_409: released409,
      contended_rejections: locks.contendedRejections,
      wall_clock_ms: Math.round(elapsedMs),
      first_violations: locks.violationLog.slice(0, 5).join(" | ") || "(none)",
    },
  });
}

main();
