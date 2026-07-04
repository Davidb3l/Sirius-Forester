// Typed read queries over the ledger (CONTRACTS.md §1). Read-only.
import type { Database } from "bun:sqlite";
import { openReadOnly, dataVersion } from "./db.ts";
import type {
  GateResult,
  IterationOutcome,
  OracleVerdict,
  ReceiptKind,
  WorkerStatus,
} from "./schema.ts";

export interface WorkerRow {
  id: string;
  created_at: string;
  last_seen_at: string | null;
  status: WorkerStatus;
}

export interface IterationRow {
  id: number;
  worker_id: string;
  issue_ref: string | null;
  entities: string | null; // JSON array text
  started_at: string;
  ended_at: string | null;
  outcome: IterationOutcome | null;
  gate_result: GateResult;
  oracle_verdicts: string | null; // JSON array text
  tokens: number | null;
  duration_ms: number | null;
  receipt_id: number | null;
}

export interface ReceiptRow {
  id: number;
  kind: ReceiptKind;
  ref: string;
  symbols: string; // JSON array text
  forward_ok: number; // 0/1
  reverse_ok: number; // 0/1
  created_at: string;
  worker_id: string | null;
}

export interface PolicyEventRow {
  id: number;
  iteration_id: number | null;
  kind: string;
  detail: string | null;
  created_at: string;
}

/** A worker plus its current (still-open) iteration, for the fleet board. */
export interface FleetEntry {
  worker: WorkerRow;
  current: IterationRow | null;
  entities: string[];
  verdicts: OracleVerdict[];
  receipt: ReceiptRow | null;
}

export interface HistoryStats {
  totalIterations: number;
  completed: number;
  released: number;
  deadends: number;
  errors: number;
  gatePass: number;
  gateFail: number;
  gateEscapeAttempts: number; // gate_result='fail' iterations
  collisionNearMisses: number; // policy_events backoff_409 + blocked verdicts
  receiptsFiled: number;
  twoWayReceipts: number; // forward_ok=1 AND reverse_ok=1
  avgCycleMs: number | null;
  medianCycleMs: number | null;
  throughputPerHour: number | null; // completed per hour over observed window
  tokensTotal: number;
}

/**
 * The ledger reader owns a single read-only connection. Cheap to construct.
 * A missing DB (core agent hasn't run `sirius init`) yields empty results and
 * `available=false`, so the server renders a friendly "no ledger yet" state.
 */
export class Ledger {
  readonly path: string;
  private db: Database | null;

  constructor(path: string) {
    this.path = path;
    this.db = openReadOnly(path);
  }

  get available(): boolean {
    return this.db !== null;
  }

  /** Re-open if the file appeared after construction (e.g. sirius init ran). */
  refresh(): void {
    if (!this.db) this.db = openReadOnly(this.path);
  }

  close(): void {
    this.db?.close();
    this.db = null;
  }

  dataVersion(): number {
    this.refresh();
    if (!this.db) return 0;
    try {
      return dataVersion(this.db);
    } catch {
      return 0;
    }
  }

  meta(): Record<string, string> {
    if (!this.db) return {};
    const rows = this.db.query("SELECT key, value FROM meta;").all() as {
      key: string;
      value: string;
    }[];
    return Object.fromEntries(rows.map((r) => [r.key, r.value]));
  }

  workers(): WorkerRow[] {
    if (!this.db) return [];
    return this.db
      .query(
        "SELECT id, created_at, last_seen_at, status FROM workers ORDER BY id;",
      )
      .all() as WorkerRow[];
  }

  /** Most recent open iteration (ended_at IS NULL) for a worker. */
  currentIteration(workerId: string): IterationRow | null {
    if (!this.db) return null;
    return this.db
      .query(
        `SELECT * FROM iterations
         WHERE worker_id = ? AND ended_at IS NULL
         ORDER BY started_at DESC, id DESC LIMIT 1;`,
      )
      .get(workerId) as IterationRow | null;
  }

  receipt(id: number): ReceiptRow | null {
    if (!this.db) return null;
    return this.db
      .query("SELECT * FROM receipts WHERE id = ?;")
      .get(id) as ReceiptRow | null;
  }

  receipts(limit = 200): ReceiptRow[] {
    if (!this.db) return [];
    return this.db
      .query("SELECT * FROM receipts ORDER BY id DESC LIMIT ?;")
      .all(limit) as ReceiptRow[];
  }

  /** Iterations that reference a receipt (for the receipt browser join). */
  iterationsForReceipt(receiptId: number): IterationRow[] {
    if (!this.db) return [];
    return this.db
      .query(
        "SELECT * FROM iterations WHERE receipt_id = ? ORDER BY id DESC;",
      )
      .all(receiptId) as IterationRow[];
  }

  recentIterations(limit = 100): IterationRow[] {
    if (!this.db) return [];
    return this.db
      .query(
        "SELECT * FROM iterations ORDER BY started_at DESC, id DESC LIMIT ?;",
      )
      .all(limit) as IterationRow[];
  }

  policyEvents(limit = 200): PolicyEventRow[] {
    if (!this.db) return [];
    return this.db
      .query(
        "SELECT * FROM policy_events ORDER BY id DESC LIMIT ?;",
      )
      .all(limit) as PolicyEventRow[];
  }

  /** Fleet board: each worker + its live iteration, entities, verdicts, receipt. */
  fleet(): FleetEntry[] {
    return this.workers().map((worker) => {
      const current = this.currentIteration(worker.id);
      const entities = parseJsonArray(current?.entities);
      const verdicts = parseJsonArray(
        current?.oracle_verdicts,
      ) as OracleVerdict[];
      const receipt =
        current?.receipt_id != null ? this.receipt(current.receipt_id) : null;
      return { worker, current, entities, verdicts, receipt };
    });
  }

  history(): HistoryStats {
    if (!this.db) return emptyStats();
    const iters = this.db
      .query("SELECT * FROM iterations;")
      .all() as IterationRow[];
    const receipts = this.receipts(100000);
    const policy = this.policyEvents(100000);

    const completed = iters.filter((i) => i.outcome === "completed").length;
    const released = iters.filter((i) => i.outcome === "released").length;
    const deadends = iters.filter((i) => i.outcome === "deadend").length;
    const errors = iters.filter((i) => i.outcome === "error").length;
    const gatePass = iters.filter((i) => i.gate_result === "pass").length;
    const gateFail = iters.filter((i) => i.gate_result === "fail").length;

    const blockedVerdicts = iters.reduce((n, i) => {
      const v = parseJsonArray(i.oracle_verdicts) as OracleVerdict[];
      return n + v.filter((x) => x === "blocked").length;
    }, 0);
    const backoff409 = policy.filter((p) => p.kind === "backoff_409").length;

    const durations = iters
      .map((i) => i.duration_ms)
      .filter((d): d is number => typeof d === "number" && d > 0)
      .sort((a, b) => a - b);
    const avgCycleMs = durations.length
      ? Math.round(durations.reduce((a, b) => a + b, 0) / durations.length)
      : null;
    const medianCycleMs = durations.length
      ? (durations[Math.floor(durations.length / 2)] ?? null)
      : null;

    const tokensTotal = iters.reduce((n, i) => n + (i.tokens ?? 0), 0);

    // throughput: completed iterations per hour across observed span
    let throughputPerHour: number | null = null;
    const ended = iters
      .filter((i) => i.outcome === "completed" && i.ended_at)
      .map((i) => Date.parse(i.ended_at as string))
      .filter((t) => !Number.isNaN(t));
    const starts = iters
      .map((i) => Date.parse(i.started_at))
      .filter((t) => !Number.isNaN(t));
    if (completed > 0 && starts.length && ended.length) {
      const spanMs = Math.max(...ended) - Math.min(...starts);
      const hours = spanMs / 3_600_000;
      throughputPerHour = hours > 0 ? +(completed / hours).toFixed(2) : null;
    }

    return {
      totalIterations: iters.length,
      completed,
      released,
      deadends,
      errors,
      gatePass,
      gateFail,
      gateEscapeAttempts: gateFail,
      collisionNearMisses: blockedVerdicts + backoff409,
      receiptsFiled: receipts.length,
      twoWayReceipts: receipts.filter(
        (r) => r.forward_ok === 1 && r.reverse_ok === 1,
      ).length,
      avgCycleMs,
      medianCycleMs,
      throughputPerHour,
      tokensTotal,
    };
  }
}

export function parseJsonArray(text: string | null | undefined): string[] {
  if (!text) return [];
  try {
    const v = JSON.parse(text);
    return Array.isArray(v) ? v.map(String) : [];
  } catch {
    return [];
  }
}

function emptyStats(): HistoryStats {
  return {
    totalIterations: 0,
    completed: 0,
    released: 0,
    deadends: 0,
    errors: 0,
    gatePass: 0,
    gateFail: 0,
    gateEscapeAttempts: 0,
    collisionNearMisses: 0,
    receiptsFiled: 0,
    twoWayReceipts: 0,
    avgCycleMs: null,
    medianCycleMs: null,
    throughputPerHour: null,
    tokensTotal: 0,
  };
}
