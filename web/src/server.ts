// Sirius Console HTTP server (PRD §3, §4). Bun.serve, vanilla TS, zero npm deps.
// Reads the three stores read-only; the ONLY mutation path is shelling to
// `sirius --json` (src/sirius.ts). Live updates via data_version SSE (src/sse.ts).
import { file } from "bun";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { discoverWorkspace, type Workspace } from "./db.ts";
import { Ledger } from "./ledger.ts";
import { ParentStores } from "./stores.ts";
import { loadConfig } from "./config.ts";
import { Sirius } from "./sirius.ts";
import { VersionPoller, sseResponse } from "./sse.ts";
import { ValidationError } from "./sirius.ts";
import {
  fleetBoard,
  historyJson,
  receiptsJson,
  receiptDetail,
  configJson,
} from "./api.ts";

export interface ServerDeps {
  workspace: Workspace;
  ledger: Ledger;
  stores: ParentStores;
  sirius: Sirius;
  poller: VersionPoller;
  publicDir: string;
}

export function buildDeps(ws?: Workspace): ServerDeps {
  const workspace = ws ?? discoverWorkspace();
  const ledger = new Ledger(workspace.ledgerPath);
  const stores = new ParentStores(workspace.ametritePath);
  const sirius = new Sirius({ cwd: workspace.root });
  const poller = new VersionPoller(ledger);
  const publicDir = fileURLToPath(new URL("../public", import.meta.url));
  return { workspace, ledger, stores, sirius, poller, publicDir };
}

const JSON_HEADERS = { "content-type": "application/json; charset=utf-8" };
const json = (data: unknown, status = 200) =>
  new Response(JSON.stringify(data), { status, headers: JSON_HEADERS });

/**
 * Route one request. Exported so tests can drive it without opening a socket.
 * A top-level try/catch turns any uncaught error into a clean JSON response
 * (a bad input used to throw and reset the connection): ValidationError → 400,
 * anything else → 500. (SIRF-11)
 */
export async function handle(
  req: Request,
  deps: ServerDeps,
): Promise<Response> {
  try {
    return await route(req, deps);
  } catch (e) {
    if (e instanceof ValidationError) return json({ error: e.message }, 400);
    return json(
      { error: e instanceof Error ? e.message : String(e) },
      500,
    );
  }
}

const LOOPBACK = new Set(["127.0.0.1", "localhost", "::1", "[::1]"]);

/** The hostname of an `Origin:` value (a full URL), or null if unparseable. */
function originHostname(origin: string | null): string | null {
  if (!origin) return null;
  try {
    return new URL(origin).hostname;
  } catch {
    return null;
  }
}

/**
 * Guard the unauthenticated mutation endpoints against drive-by browser POSTs
 * (CSRF) and DNS-rebinding: the request `Host` (the authority Bun parses into
 * `req.url` from the Host header) and the `Origin` (when present) must both be
 * loopback, and the body must be declared `application/json`. Returns an error
 * Response to short-circuit, or null to proceed. (SIRF-11)
 */
function guardMutation(req: Request): Response | null {
  // Host: DNS-rebinding defense — a rebinding attack reaches the loopback bind
  // with an attacker-controlled Host, so reject anything non-loopback.
  if (!LOOPBACK.has(new URL(req.url).hostname)) {
    return json({ error: "non-loopback Host refused" }, 403);
  }
  // Origin: CSRF defense — a browser attaches the (cross-)origin on POST.
  const origin = req.headers.get("origin");
  if (origin !== null && !LOOPBACK.has(originHostname(origin) ?? "")) {
    return json({ error: "cross-origin request refused" }, 403);
  }
  const ct = (req.headers.get("content-type") ?? "").toLowerCase();
  if (!ct.includes("application/json")) {
    return json({ error: "content-type must be application/json" }, 415);
  }
  return null;
}

async function route(req: Request, deps: ServerDeps): Promise<Response> {
  const url = new URL(req.url);
  const path = url.pathname;

  // ---- live stream ----
  if (path === "/events") return sseResponse(deps.poller);

  // ---- read APIs (all read-only over the stores) ----
  if (path === "/api/fleet") {
    return json(fleetBoard(deps.ledger, deps.stores, deps.ledger.dataVersion()));
  }
  if (path === "/api/history") {
    return json(historyJson(deps.ledger));
  }
  if (path === "/api/receipts") {
    return json(receiptsJson(deps.ledger));
  }
  const rMatch = /^\/api\/receipt\/(\d+)$/.exec(path);
  if (rMatch) {
    const id = Number(rMatch[1]);
    const detail = receiptDetail(deps.ledger, id);
    if (!detail) return json({ error: "receipt not found" }, 404);
    // enrich with `sirius why <ref>` if the binary is available (never fatal)
    const why = await siriusWhy(deps.sirius, detail.receipt.ref);
    return json({ ...detail, why });
  }
  if (path === "/api/config") {
    return json(configJson(loadConfig(deps.workspace.configPath)));
  }
  if (path === "/api/doctor") {
    const r = await deps.sirius.doctor();
    return json({
      available: r.exitCode !== 127,
      ...r,
    });
  }
  if (path === "/api/health") {
    return json({
      ok: true,
      ledgerAvailable: deps.ledger.available,
      workspace: deps.workspace.root,
      dataVersion: deps.ledger.dataVersion(),
    });
  }

  // ---- mutation boundary: every mutation shells to `sirius <cmd> --json` ----
  if (req.method === "POST") {
    const blocked = guardMutation(req);
    if (blocked) return blocked;
  }
  if (req.method === "POST" && path === "/api/gate") {
    const body = (await safeBody(req)) as {
      issue?: string;
      tier?: string;
      targetStatus?: string;
    };
    if (!body.issue) return json({ error: "issue required" }, 400);
    const r = await deps.sirius.gate(body.issue, body.tier, body.targetStatus);
    return json(r, r.error ? 502 : 200);
  }
  if (req.method === "POST" && path === "/api/link") {
    const body = (await safeBody(req)) as {
      ref?: string;
      decision?: string;
      symbols?: string[];
      changed?: boolean;
    };
    const symbols = body.symbols ?? [];
    if (body.decision) {
      return json(
        await deps.sirius.linkDecision(body.decision, symbols, body.changed),
      );
    }
    if (body.ref) {
      return json(
        await deps.sirius.linkIssue(body.ref, symbols, body.changed),
      );
    }
    return json({ error: "ref or decision required" }, 400);
  }

  // ---- static assets ----
  return serveStatic(path, deps.publicDir);
}

async function siriusWhy(sirius: Sirius, ref: string) {
  try {
    if (!(await sirius.available())) {
      return { error: "sirius binary not found (why enrichment unavailable)" };
    }
    // AMT refs and decision refs both resolve through `sirius why <ref>`
    const r = await sirius.whyIssue(ref);
    if (r.data) return r.data;
    return { error: r.error ?? "no data" };
  } catch (e) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
}

async function safeBody(req: Request): Promise<Record<string, unknown>> {
  try {
    return (await req.json()) as Record<string, unknown>;
  } catch {
    return {};
  }
}

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".svg": "image/svg+xml",
  ".json": "application/json; charset=utf-8",
  ".ico": "image/x-icon",
};

async function serveStatic(path: string, publicDir: string): Promise<Response> {
  let decoded = path;
  try {
    decoded = decodeURIComponent(path);
  } catch {
    return new Response("bad path", { status: 400 });
  }
  const rel = decoded === "/" ? "/index.html" : decoded;
  // prevent traversal (also catches %2e%2e once decoded)
  if (rel.includes("..")) return new Response("bad path", { status: 400 });
  const full = join(publicDir, rel);
  const f = file(full);
  if (!(await f.exists())) {
    // SPA-ish fallback to index for unknown non-asset paths
    if (!/\.[a-z0-9]+$/i.test(rel)) {
      const idx = file(join(publicDir, "index.html"));
      if (await idx.exists())
        return new Response(idx, {
          headers: { "content-type": MIME[".html"] as string },
        });
    }
    return new Response("not found", { status: 404 });
  }
  const ext = rel.slice(rel.lastIndexOf("."));
  return new Response(f, {
    headers: { "content-type": MIME[ext] ?? "application/octet-stream" },
  });
}

// ---- boot -------------------------------------------------------------------

// SIRIUS_CONSOLE_PORT is the ONLY port override, and it is the same variable
// `sirius doctor` reads to report its SUITE_CONTRACTS §3.2 `ui` URL. The bare
// `PORT` is deliberately not honored on either side: doctor is usually spawned
// as a child (a suite hub probing its peers), and a parent exporting `PORT` for
// its own listener would otherwise make the two disagree about where the
// Console lives.
export function startServer(
  port = Number(process.env.SIRIUS_CONSOLE_PORT ?? 1777),
) {
  const deps = buildDeps();
  const server = Bun.serve({
    port,
    // Bind loopback only: the mutation endpoints are unauthenticated, so they
    // must never be reachable from the LAN (0.0.0.0). (SIRF-11)
    hostname: "127.0.0.1",
    idleTimeout: 0, // keep SSE connections open
    fetch: (req) => handle(req, deps),
  });
  const ledgerState = deps.ledger.available
    ? `ledger ${deps.workspace.ledgerPath}`
    : `no ledger at ${deps.workspace.ledgerPath} (run 'sirius init')`;
  // eslint-disable-next-line no-console
  console.log(
    `Sirius Console → http://localhost:${server.port}  ·  ${ledgerState}`,
  );
  return { server, deps };
}

// Run when invoked directly (not when imported by tests).
if (import.meta.main) {
  startServer();
}
