// The Suite Hub: one page at :6969 that discovers every installed suite tool
// and switches between their UIs.
//
// Two rules, from CONSOLE_V2's "the deck owns no state" and SUITE_CONTRACTS §3.2:
//   1. The hub owns no state. It probes, it renders. No database, no cache of
//      "truth" that can drift beyond a few seconds of roster memoization.
//   2. The hub is a shell, not a proxy. It frames or links each tool's own UI
//      at that tool's own port. It never reverse-proxies, rewrites paths, or
//      moves anybody's port.

import { file } from "bun";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  buildRoster,
  spawnProber,
  type Prober,
  type ToolStatus,
} from "./discovery.ts";

export const HUB_PORT = 6969;
const RESCAN_MS = 10_000;
const UI_PROBE_TIMEOUT_MS = 1500;

/** A roster entry plus what we learned by poking the advertised UI. */
export interface HubEntry extends ToolStatus {
  uiReachable: boolean | null;
  uiFrameable: boolean | null;
  /** Why the UI can't be framed / reached. Null when there's nothing to say. */
  uiReason: string | null;
}

export interface Roster {
  ts: string;
  tools: HubEntry[];
}

/**
 * Can we embed `url` in an iframe from a different origin?
 *
 * The browser will not tell us why a frame was refused, so we ask the server
 * directly and read the two headers that can refuse: X-Frame-Options and CSP
 * `frame-ancestors`. Since the hub is a different origin than every tool,
 * SAMEORIGIN refuses us just as surely as DENY.
 */
export async function probeUi(
  url: string,
  fetchImpl: typeof fetch = fetch,
  timeoutMs = UI_PROBE_TIMEOUT_MS,
): Promise<{ reachable: boolean; frameable: boolean; reason: string | null }> {
  let res: Response;
  try {
    res = await fetchImpl(url, {
      method: "GET",
      redirect: "manual",
      signal: AbortSignal.timeout(timeoutMs),
    });
  } catch {
    return { reachable: false, frameable: false, reason: "UI not reachable" };
  }
  // We only ever want the headers. Release the socket instead of letting the
  // body sit unread on every 10s rescan.
  void res.body?.cancel().catch(() => {});

  // A redirect is followed by the *iframe*, not by us. `isLoopbackUrl` only
  // vetted the advertised URL, so a loopback UI that 302s to a remote origin
  // would escape the loopback guarantee. Refuse to frame anything that
  // redirects; the user can still open it in a tab, where it's just a link.
  if (res.status >= 300 && res.status < 400) {
    const loc = res.headers.get("location") ?? "(none)";
    return {
      reachable: true,
      frameable: false,
      reason: `redirects to ${loc}; not framed`,
    };
  }

  const xfo = (res.headers.get("x-frame-options") ?? "").trim().toLowerCase();
  if (xfo === "deny" || xfo === "sameorigin" || xfo.startsWith("allow-from")) {
    return {
      reachable: true,
      frameable: false,
      reason: `refuses framing (X-Frame-Options: ${xfo})`,
    };
  }

  const csp = (res.headers.get("content-security-policy") ?? "").toLowerCase();
  const fa = csp
    .split(";")
    .map((d) => d.trim())
    .find((d) => d.startsWith("frame-ancestors"));
  if (fa) {
    const value = fa.slice("frame-ancestors".length).trim();
    // 'none' refuses everyone. 'self' refuses us (we're a different origin).
    // Anything else may allow us; we don't try to out-parse the browser.
    if (value === "'none'" || value === "'self'") {
      return {
        reachable: true,
        frameable: false,
        reason: `refuses framing (CSP frame-ancestors ${value})`,
      };
    }
  }

  return { reachable: true, frameable: true, reason: null };
}

/**
 * Is `url` the hub's own address? A tool that mis-reports its UI as our port
 * (e.g. it read a generic `PORT` we exported when spawning it) would make the
 * hub frame itself, recursively. Never trust a peer about our own origin.
 */
export function isSelfUrl(url: string, selfPort: number): boolean {
  try {
    const u = new URL(url);
    const port = u.port ? Number(u.port) : u.protocol === "https:" ? 443 : 80;
    return port === selfPort;
  } catch {
    return false;
  }
}

/** Build the roster and enrich each advertised UI with reachability/framing. */
export async function scan(
  prober: Prober = spawnProber,
  fetchImpl: typeof fetch = fetch,
  selfPort: number = HUB_PORT,
): Promise<Roster> {
  const tools = await buildRoster(prober);
  const enriched: HubEntry[] = await Promise.all(
    tools.map(async (t): Promise<HubEntry> => {
      if (!t.ui) {
        return { ...t, uiReachable: null, uiFrameable: null, uiReason: t.reason };
      }
      if (isSelfUrl(t.ui, selfPort)) {
        return {
          ...t,
          ui: null,
          uiReachable: null,
          uiFrameable: null,
          uiReason: `reported the hub's own port (${selfPort}) as its UI; ignoring`,
        };
      }
      const probe = await probeUi(t.ui, fetchImpl);
      return {
        ...t,
        uiReachable: probe.reachable,
        uiFrameable: probe.frameable,
        uiReason: probe.reason,
      };
    }),
  );
  // Stable order: healthy first, then unhealthy, then absent; alphabetical within.
  const rank = { healthy: 0, unhealthy: 1, absent: 2 } as const;
  enriched.sort(
    (a, b) => rank[a.presence] - rank[b.presence] || a.id.localeCompare(b.id),
  );
  return { ts: new Date().toISOString(), tools: enriched };
}

/** Memoized scan. The hub holds no state beyond this few-seconds-stale roster. */
export function createRosterCache(
  prober: Prober = spawnProber,
  fetchImpl: typeof fetch = fetch,
  ttlMs = RESCAN_MS,
  selfPort: number = HUB_PORT,
) {
  let cached: Roster | null = null;
  let at = 0;
  let inflight: Promise<Roster> | null = null;

  return async function get(now = Date.now()): Promise<Roster> {
    if (cached && now - at < ttlMs) return cached;
    if (inflight) return inflight;
    inflight = scan(prober, fetchImpl, selfPort)
      .then((r) => {
        cached = r;
        at = Date.now();
        return r;
      })
      .finally(() => {
        inflight = null;
      });
    return inflight;
  };
}

const PUBLIC_DIR = fileURLToPath(new URL("../public", import.meta.url));

const STATIC: Record<string, string> = {
  "/": "hub.html",
  "/hub.html": "hub.html",
  "/hub.css": "hub.css",
  "/hub.js": "hub.js",
};

export async function handle(
  req: Request,
  getRoster: () => Promise<Roster>,
  publicDir = PUBLIC_DIR,
): Promise<Response> {
  const { pathname } = new URL(req.url);

  if (pathname === "/api/roster") {
    if (req.method !== "GET") {
      return Response.json({ error: "method not allowed" }, { status: 405 });
    }
    const roster = await getRoster();
    return Response.json(roster, {
      headers: { "cache-control": "no-store" },
    });
  }

  const name = STATIC[pathname];
  if (name && req.method === "GET") {
    const f = file(join(publicDir, name));
    if (await f.exists()) return new Response(f);
  }
  return new Response("not found", { status: 404 });
}

export function startHub(port: number = Number(process.env.HUB_PORT ?? HUB_PORT)) {
  const getRoster = createRosterCache(spawnProber, fetch, RESCAN_MS, port);
  const server = Bun.serve({
    port,
    // Local-only. The hub reads nothing secret, but it enumerates what's
    // installed on this machine; that stays on this machine.
    hostname: "127.0.0.1",
    fetch: (req) => handle(req, getRoster),
  });
  // Warm the roster, and keep it warm so the page is never waiting on four
  // CLI probes. Swallow failures: a background refresh must never become an
  // unhandled rejection and kill the process.
  const warm = (now?: number) => void getRoster(now).catch(() => {});
  warm();
  // Infinity forces the cache to consider itself stale, i.e. a real rescan.
  const timer = setInterval(() => warm(Number.POSITIVE_INFINITY), RESCAN_MS);
  timer.unref?.();
  // eslint-disable-next-line no-console
  console.log(`Suite Hub → http://localhost:${server.port}`);
  return { server, getRoster };
}

if (import.meta.main) {
  startHub();
}
