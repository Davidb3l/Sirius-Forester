import { test, expect, beforeAll, afterAll } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { seed } from "../fixtures/seed.ts";
import { buildDeps, handle, type ServerDeps } from "../src/server.ts";
import type { Workspace } from "../src/db.ts";

let dir: string;
let deps: ServerDeps;

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "sirius-srv-"));
  const dbPath = seed(join(dir, ".sirius", "sirius.db"));
  const siriusDir = dirname(dbPath);
  const root = dirname(siriusDir);
  const ws: Workspace = {
    root,
    ledgerPath: dbPath,
    configPath: join(siriusDir, "config.json"),
    ametritePath: null,
    hayvenDir: null,
  };
  deps = buildDeps(ws);
});
afterAll(() => {
  deps.ledger.close();
  deps.stores.close();
  rmSync(dir, { recursive: true, force: true });
});

// Loopback base URL so the mutation guard (Host must be loopback) is exercised
// the same way the real bind on 127.0.0.1 sees it.
const get = (path: string) =>
  handle(new Request(`http://127.0.0.1${path}`), deps);

const postJson = (path: string, body: unknown, headers: Record<string, string> = {}) =>
  handle(
    new Request(`http://127.0.0.1${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...headers },
      body: JSON.stringify(body),
    }),
    deps,
  );

test("GET /api/health reports ledger available", async () => {
  const res = await get("/api/health");
  expect(res.status).toBe(200);
  const body = await res.json();
  expect(body.ok).toBe(true);
  expect(body.ledgerAvailable).toBe(true);
});

test("GET /api/fleet returns workers with issues + verdicts", async () => {
  const res = await get("/api/fleet");
  const body = await res.json();
  expect(body.ledgerAvailable).toBe(true);
  expect(body.workers.length).toBe(5);
  const rowan = body.workers.find((w: any) => w.id === "sirius/rowan");
  expect(rowan.issueRef).toBe("AMT-7");
  expect(rowan.blocked).toBeGreaterThanOrEqual(1);
  expect(rowan.receipt.id).toBeGreaterThan(0);
});

test("GET /api/history returns stats + recent + policy", async () => {
  const res = await get("/api/history");
  const body = await res.json();
  expect(body.stats.completed).toBe(3);
  expect(body.recent.length).toBeGreaterThan(0);
  expect(body.policyEvents.length).toBeGreaterThan(0);
});

test("GET /api/receipts lists receipts with two-way flags", async () => {
  const res = await get("/api/receipts");
  const body = await res.json();
  expect(body.receipts.length).toBe(4);
  const partial = body.receipts.find((r: any) => r.ref === "AMT-7");
  expect(partial.twoWay).toBe(false);
  const full = body.receipts.find((r: any) => r.ref === "AMT-4");
  expect(full.twoWay).toBe(true);
});

test("GET /api/receipt/:id returns detail + iterations", async () => {
  const list = await (await get("/api/receipts")).json();
  const id = list.receipts.find((r: any) => r.ref === "AMT-4").id;
  const res = await get(`/api/receipt/${id}`);
  const body = await res.json();
  expect(body.receipt.ref).toBe("AMT-4");
  expect(body.receipt.symbols.length).toBeGreaterThan(0);
  expect(Array.isArray(body.iterations)).toBe(true);
  // why enrichment present (object with either data or {error} — never throws)
  expect(body).toHaveProperty("why");
});

test("GET /api/receipt/:id 404 for unknown id", async () => {
  const res = await get("/api/receipt/99999");
  expect(res.status).toBe(404);
});

test("GET /api/config falls back to defaults when file absent", async () => {
  const res = await get("/api/config");
  const body = await res.json();
  expect(body.present).toBe(false);
  expect(body.config.gate_tier).toBe("safe");
  expect(body.config.worker_concurrency).toBe(3);
});

test("GET / serves the console HTML", async () => {
  const res = await get("/");
  expect(res.status).toBe(200);
  expect(res.headers.get("content-type")).toContain("text/html");
  const html = await res.text();
  expect(html).toContain("SIRIUS FORESTER");
});

test("static asset content-types are correct", async () => {
  const css = await get("/app.css");
  expect(css.headers.get("content-type")).toContain("text/css");
  const js = await get("/app.js");
  expect(js.headers.get("content-type")).toContain("javascript");
});

test("path traversal cannot escape the public dir", async () => {
  // The WHATWG URL parser normalizes ".." / "%2e%2e" before handle() sees it,
  // and the serveStatic guard rejects any residual "..". Either way the file
  // outside public/ is never served.
  const res = await get("/%2e%2e/package.json");
  expect([400, 404]).toContain(res.status);
  const body = await res.text();
  expect(body).not.toContain("sirius-console"); // package.json contents not leaked
});

test("POST /api/link requires ref or decision", async () => {
  const res = await postJson("/api/link", { symbols: ["a"] });
  expect(res.status).toBe(400);
});

// ---- SIRF-11: console security ---------------------------------------------

test("POST without application/json content-type is rejected (415)", async () => {
  const res = await handle(
    new Request("http://127.0.0.1/api/gate", {
      method: "POST",
      body: JSON.stringify({ issue: "AMT-7" }),
    }),
    deps,
  );
  expect(res.status).toBe(415);
});

test("POST with a cross-origin Origin is refused (403)", async () => {
  const res = await postJson(
    "/api/gate",
    { issue: "AMT-7" },
    { origin: "https://evil.example" },
  );
  expect(res.status).toBe(403);
});

test("POST with a non-loopback Host is refused (403)", async () => {
  const res = await handle(
    new Request("http://evil.example/api/gate", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ issue: "AMT-7" }),
    }),
    deps,
  );
  expect(res.status).toBe(403);
});

test("a same-origin loopback Origin is allowed through the guard", async () => {
  // Passes the guard, then fails validation on a bad ref → 400 (not 403).
  const res = await postJson(
    "/api/gate",
    { issue: "--tier" },
    { origin: "http://localhost:1777" },
  );
  expect(res.status).toBe(400);
});

test("POST /api/gate with an argv-injecting issue is rejected (400, no spawn)", async () => {
  const res = await postJson("/api/gate", { issue: "--target-status" });
  expect(res.status).toBe(400);
  const body = await res.json();
  expect(body.error).toContain("invalid issue");
});
