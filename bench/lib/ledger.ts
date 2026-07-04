/**
 * bench/lib/ledger.ts
 *
 * Fixture ledger builder. Materializes an in-memory model of the Sirius ledger
 * (CONTRACTS.md §1 schema) so bench harnesses can run offline, before the
 * `sirius` binary exists.
 *
 * IMPORTANT: fixture ledgers are seeded under bench/fixtures/ or scratch — never
 * to repo-root .sirius/, which the core agent owns. See harness call sites.
 *
 * This is a plain object model (not SQLite) on purpose: harnesses need to run
 * with `bun run` and zero external deps. The column names mirror the SQL schema
 * 1:1 so that a later `--live` mode reading the real DB can reuse the same shapes.
 */

export type Outcome =
  | "completed"
  | "released"
  | "deadend"
  | "gate_failed"
  | "error";

export type GateResult = "pass" | "fail" | "skipped" | null;

export interface Worker {
  id: string; // 'sirius/oak'
  created_at: string;
  last_seen_at: string | null;
  status: "idle" | "working" | "blocked" | "stopped";
}

export interface Receipt {
  id: number;
  kind: "issue" | "decision";
  ref: string; // 'AMT-7' | 'D-3'
  symbols: string[]; // entity ids stamped
  forward_ok: 0 | 1; // amt comment landed
  reverse_ok: 0 | 1; // hayven remember landed
  created_at: string;
  worker_id: string | null;
}

export interface Iteration {
  id: number;
  worker_id: string;
  issue_ref: string | null;
  entities: string[]; // hayven entity ids
  started_at: string;
  ended_at: string | null;
  outcome: Outcome | null;
  gate_result: GateResult;
  oracle_verdicts: string[]; // per-entity: registered|blocked|forced
  tokens: number | null;
  duration_ms: number | null;
  receipt_id: number | null;
}

export interface PolicyEvent {
  id: number;
  iteration_id: number | null;
  kind:
    | "claim_order"
    | "backoff_409"
    | "oracle_202"
    | "gate_tier"
    | "retry_budget"
    | "concurrency";
  detail: unknown; // JSON
  created_at: string;
}

export class FixtureLedger {
  workers: Worker[] = [];
  iterations: Iteration[] = [];
  receipts: Receipt[] = [];
  policy_events: PolicyEvent[] = [];

  private nextIterId = 1;
  private nextReceiptId = 1;
  private nextPolicyId = 1;

  meta = {
    schema_version: "1",
    created_at: new Date(0).toISOString(),
    sirius_version: "0.0.0-fixture",
  };

  addWorker(id: string): Worker {
    const w: Worker = {
      id,
      created_at: new Date(0).toISOString(),
      last_seen_at: null,
      status: "idle",
    };
    this.workers.push(w);
    return w;
  }

  addReceipt(r: Omit<Receipt, "id">): Receipt {
    const receipt: Receipt = { id: this.nextReceiptId++, ...r };
    this.receipts.push(receipt);
    return receipt;
  }

  addIteration(it: Omit<Iteration, "id">): Iteration {
    const iter: Iteration = { id: this.nextIterId++, ...it };
    this.iterations.push(iter);
    return iter;
  }

  addPolicyEvent(e: Omit<PolicyEvent, "id" | "created_at">): PolicyEvent {
    const evt: PolicyEvent = {
      id: this.nextPolicyId++,
      created_at: new Date().toISOString(),
      ...e,
    };
    this.policy_events.push(evt);
    return evt;
  }

  /** Iterations whose outcome moved an issue to done (completed). */
  doneIterations(): Iteration[] {
    return this.iterations.filter((i) => i.outcome === "completed");
  }

  /** A done iteration "carries a two-way receipt" iff it references a receipt
   *  with both forward_ok and reverse_ok set. */
  receiptFor(iter: Iteration): Receipt | undefined {
    if (iter.receipt_id == null) return undefined;
    return this.receipts.find((r) => r.id === iter.receipt_id);
  }
}

/** A tiny seeded PRNG (mulberry32) so contentious workloads are reproducible. */
export function seededRng(seed: number): () => number {
  let a = seed >>> 0;
  return function () {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
