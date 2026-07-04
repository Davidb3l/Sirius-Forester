// JSON payload assembly for the frontend. Pure read composition over Ledger +
// ParentStores; the frontend fetches these and renders. Keeping shapes here (not
// inline in the server) makes them easy to unit-test against fixtures.
import type { Ledger, FleetEntry, ReceiptRow } from "./ledger.ts";
import { parseJsonArray } from "./ledger.ts";
import type { ParentStores } from "./stores.ts";
import type { ConfigView } from "./config.ts";

export interface FleetBoardJson {
  dataVersion: number;
  ledgerAvailable: boolean;
  generatedAt: string;
  meta: Record<string, string>;
  workers: {
    id: string;
    status: string;
    lastSeenAt: string | null;
    issueRef: string | null;
    issueTitle: string | null;
    entities: string[];
    verdicts: string[];
    blocked: number;
    forced: number;
    gateResult: string | null;
    outcome: string | null;
    startedAt: string | null;
    receipt: {
      id: number;
      forwardOk: boolean;
      reverseOk: boolean;
      twoWay: boolean;
    } | null;
  }[];
}

export function fleetBoard(
  ledger: Ledger,
  stores: ParentStores,
  dataVersion: number,
): FleetBoardJson {
  const fleet = ledger.fleet();
  return {
    dataVersion,
    ledgerAvailable: ledger.available,
    generatedAt: new Date().toISOString(),
    meta: ledger.meta(),
    workers: fleet.map((e: FleetEntry) => {
      const verdicts = e.verdicts;
      return {
        id: e.worker.id,
        status: e.worker.status,
        lastSeenAt: e.worker.last_seen_at,
        issueRef: e.current?.issue_ref ?? null,
        issueTitle: stores.issueTitle(e.current?.issue_ref),
        entities: e.entities,
        verdicts,
        blocked: verdicts.filter((v) => v === "blocked").length,
        forced: verdicts.filter((v) => v === "forced").length,
        gateResult: e.current?.gate_result ?? null,
        outcome: e.current?.outcome ?? null,
        startedAt: e.current?.started_at ?? null,
        receipt: e.receipt
          ? {
              id: e.receipt.id,
              forwardOk: e.receipt.forward_ok === 1,
              reverseOk: e.receipt.reverse_ok === 1,
              twoWay:
                e.receipt.forward_ok === 1 && e.receipt.reverse_ok === 1,
            }
          : null,
      };
    }),
  };
}

export function historyJson(ledger: Ledger) {
  return {
    dataVersion: ledger.dataVersion(),
    ledgerAvailable: ledger.available,
    stats: ledger.history(),
    recent: ledger.recentIterations(50).map((i) => ({
      id: i.id,
      worker: i.worker_id,
      issue: i.issue_ref,
      entities: parseJsonArray(i.entities),
      outcome: i.outcome,
      gate: i.gate_result,
      verdicts: parseJsonArray(i.oracle_verdicts),
      startedAt: i.started_at,
      endedAt: i.ended_at,
      durationMs: i.duration_ms,
      tokens: i.tokens,
      receiptId: i.receipt_id,
    })),
    policyEvents: ledger.policyEvents(50).map((p) => ({
      id: p.id,
      iterationId: p.iteration_id,
      kind: p.kind,
      detail: safeJson(p.detail),
      createdAt: p.created_at,
    })),
  };
}

export function receiptsJson(ledger: Ledger) {
  return {
    ledgerAvailable: ledger.available,
    receipts: ledger.receipts(300).map(receiptSummary),
  };
}

function receiptSummary(r: ReceiptRow) {
  return {
    id: r.id,
    kind: r.kind,
    ref: r.ref,
    symbols: parseJsonArray(r.symbols),
    forwardOk: r.forward_ok === 1,
    reverseOk: r.reverse_ok === 1,
    twoWay: r.forward_ok === 1 && r.reverse_ok === 1,
    createdAt: r.created_at,
    worker: r.worker_id,
  };
}

/**
 * Receipt detail: the receipt + the iterations that filed it. `sirius why`
 * enrichment (issue title, decision text) is fetched separately by the server
 * so this stays pure/DB-only for testing.
 */
export function receiptDetail(ledger: Ledger, id: number) {
  const r = ledger.receipt(id);
  if (!r) return null;
  return {
    receipt: receiptSummary(r),
    iterations: ledger.iterationsForReceipt(id).map((i) => ({
      id: i.id,
      worker: i.worker_id,
      issue: i.issue_ref,
      entities: parseJsonArray(i.entities),
      outcome: i.outcome,
      gate: i.gate_result,
      startedAt: i.started_at,
      endedAt: i.ended_at,
      durationMs: i.duration_ms,
    })),
  };
}

export function configJson(view: ConfigView) {
  return {
    present: view.present,
    path: view.path,
    error: view.error,
    config: view.config,
    raw: view.raw,
  };
}

function safeJson(text: string | null): unknown {
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}
