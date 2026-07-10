import { test, expect, beforeAll, afterAll } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { seed } from "../fixtures/seed.ts";
import { Ledger, parseJsonArray } from "../src/ledger.ts";

let dir: string;
let dbPath: string;
let ledger: Ledger;

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "sirius-ledger-"));
  dbPath = seed(join(dir, ".sirius", "sirius.db"));
  ledger = new Ledger(dbPath);
});
afterAll(() => {
  ledger.close();
  // Windows keeps the sqlite file locked briefly after close(); the unlink
  // races it and throws EBUSY. Retry instead of failing the suite on cleanup.
  rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 });
});

test("ledger opens read-only and reports available", () => {
  expect(ledger.available).toBe(true);
});

test("meta carries schema_version 1", () => {
  const m = ledger.meta();
  expect(m.schema_version).toBe("1");
  expect(m.sirius_version).toBe("0.1.0");
});

test("workers roster has expected statuses", () => {
  const ws = ledger.workers();
  expect(ws.length).toBe(5);
  const oak = ws.find((w) => w.id === "sirius/oak");
  expect(oak?.status).toBe("working");
  const rowan = ws.find((w) => w.id === "sirius/rowan");
  expect(rowan?.status).toBe("blocked");
});

test("fleet board pairs workers with their open iteration", () => {
  const fleet = ledger.fleet();
  const oak = fleet.find((f) => f.worker.id === "sirius/oak");
  expect(oak?.current?.issue_ref).toBe("AMT-12"); // most recent open
  expect(oak?.entities).toContain("render/tree.ts#plant");

  const rowan = fleet.find((f) => f.worker.id === "sirius/rowan");
  expect(rowan?.current?.issue_ref).toBe("AMT-7");
  expect(rowan?.verdicts).toContain("blocked");
  expect(rowan?.receipt?.id).toBeGreaterThan(0);

  const cedar = fleet.find((f) => f.worker.id === "sirius/cedar");
  expect(cedar?.current).toBeNull(); // idle, no open iteration
});

test("history stats compute completed / gate / collisions", () => {
  const h = ledger.history();
  expect(h.completed).toBe(3);
  expect(h.gateFail).toBeGreaterThanOrEqual(2);
  expect(h.gateEscapeAttempts).toBe(h.gateFail);
  // collision near-misses = blocked verdicts + backoff_409 events
  expect(h.collisionNearMisses).toBeGreaterThan(0);
  expect(h.receiptsFiled).toBe(4);
  expect(h.twoWayReceipts).toBe(3); // r3 is partial (reverse_ok=0)
  expect(h.tokensTotal).toBeGreaterThan(0);
});

test("receipt detail joins its filing iterations", () => {
  const receipts = ledger.receipts();
  const amt4 = receipts.find((r) => r.ref === "AMT-4");
  expect(amt4).toBeTruthy();
  const iters = ledger.iterationsForReceipt(amt4!.id);
  expect(iters.length).toBeGreaterThanOrEqual(1);
  expect(iters[0]?.issue_ref).toBe("AMT-4");
});

test("parseJsonArray is defensive", () => {
  expect(parseJsonArray(null)).toEqual([]);
  expect(parseJsonArray("not json")).toEqual([]);
  expect(parseJsonArray('["a","b"]')).toEqual(["a", "b"]);
});

test("missing ledger degrades gracefully", () => {
  const l = new Ledger(join(dir, "nope", "sirius.db"));
  expect(l.available).toBe(false);
  expect(l.workers()).toEqual([]);
  expect(l.fleet()).toEqual([]);
  expect(l.history().totalIterations).toBe(0);
  expect(l.dataVersion()).toBe(0);
});
