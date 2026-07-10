import { test, expect } from "bun:test";
import {
  classify,
  buildRoster,
  isLoopbackUrl,
  probeCommand,
  TOOL_IDS,
  type ProbeResult,
  type ToolId,
} from "../src/discovery.ts";
import { probeUi, scan, createRosterCache, handle, isSelfUrl, type Roster } from "../src/hub.ts";

const ok = (body: unknown, code = 0): ProbeResult => ({
  found: true,
  timedOut: false,
  code,
  stdout: typeof body === "string" ? body : JSON.stringify(body),
});

const amtEnvelope = {
  tool: "amt",
  version: "0.1.0",
  schemaVersion: 1,
  ok: true,
  capabilities: ["ui", "mcp"],
  ui: "http://localhost:1776",
  checks: [{ name: "unresolved_links", ok: true, detail: "0 found" }],
};

// ---- §3.1 classification ----------------------------------------------------

test("healthy: exit 0, valid envelope, ok:true", () => {
  const s = classify("amt", ok(amtEnvelope));
  expect(s.presence).toBe("healthy");
  expect(s.version).toBe("0.1.0");
  expect(s.ui).toBe("http://localhost:1776");
  expect(s.capabilities).toEqual(["ui", "mcp"]);
  expect(s.checks[0]).toEqual({ name: "unresolved_links", ok: true, detail: "0 found" });
});

test("unhealthy: exit 0, valid envelope, ok:false — present, not hidden", () => {
  const s = classify("amt", ok({ ...amtEnvelope, ok: false }));
  expect(s.presence).toBe("unhealthy");
  expect(s.version).toBe("0.1.0"); // still tells us things
});

test("absent: CLI not on PATH", () => {
  const s = classify("catryna", { found: false, timedOut: false, code: null, stdout: "" });
  expect(s.presence).toBe("absent");
  expect(s.reason).toBe("not installed");
});

test("absent: non-zero exit", () => {
  expect(classify("amt", ok(amtEnvelope, 1)).presence).toBe("absent");
});

test("absent: timeout", () => {
  const s = classify("amt", { found: true, timedOut: true, code: null, stdout: "" });
  expect(s.presence).toBe("absent");
  expect(s.reason).toContain("timed out");
});

test("absent: exit 0 but stdout is not JSON (hayven's markdown doctor)", () => {
  const s = classify("hayven", ok("# hayven doctor\n\n- Bun version: OK\nAll checks passed.\n"));
  expect(s.presence).toBe("absent");
  expect(s.reason).toContain("did not emit JSON");
});

test("absent: JSON that isn't an object", () => {
  expect(classify("amt", ok([1, 2, 3])).presence).toBe("absent");
  expect(classify("amt", ok("null")).presence).toBe("absent");
});

test("absent: tool id mismatch is a contract violation", () => {
  const s = classify("sirius", ok({ ...amtEnvelope, tool: "amt" }));
  expect(s.presence).toBe("absent");
  expect(s.reason).toContain('expected "sirius"');
});

test("checks accept sirius's historical `pass` as well as spec `ok`", () => {
  const s = classify("sirius", ok({
    tool: "sirius", version: "0.1.0", schemaVersion: 1, ok: false,
    capabilities: ["ui"], ui: "http://localhost:1777",
    checks: [{ name: "hayven_daemon_7777", pass: false, detail: "no 200" }],
  }));
  expect(s.checks[0]).toEqual({ name: "hayven_daemon_7777", ok: false, detail: "no 200" });
});

// ---- §3.2 the `ui` field ----------------------------------------------------

test("no `ui` capability means no ui, even if a url is present", () => {
  const s = classify("amt", ok({ ...amtEnvelope, capabilities: ["mcp"] }));
  expect(s.ui).toBeNull();
});

test("`ui` capability without a usable url is reported, not silently dropped", () => {
  const s = classify("amt", ok({ ...amtEnvelope, ui: undefined }));
  expect(s.presence).toBe("healthy");
  expect(s.ui).toBeNull();
  expect(s.reason).toContain("not loopback");
});

test("a non-loopback ui url is refused (the hub would iframe it)", () => {
  for (const bad of [
    "https://evil.example/",
    "http://10.0.0.5:1776",
    "file:///etc/passwd",
    "javascript:alert(1)",
    "not a url",
  ]) {
    expect(isLoopbackUrl(bad)).toBe(false);
    expect(classify("amt", ok({ ...amtEnvelope, ui: bad })).ui).toBeNull();
  }
});

test("loopback hosts are accepted", () => {
  expect(isLoopbackUrl("http://localhost:1776")).toBe(true);
  expect(isLoopbackUrl("http://127.0.0.1:1776/board")).toBe(true);
  expect(isLoopbackUrl("https://localhost:1776")).toBe(true);
});

// ---- roster -----------------------------------------------------------------

test("buildRoster probes all four ids and never throws", async () => {
  const roster = await buildRoster(async (id) => {
    if (id === "amt") return ok(amtEnvelope);
    if (id === "sirius") throw new Error("boom");
    return { found: false, timedOut: false, code: null, stdout: "" };
  });
  expect(roster.map((r) => r.id).sort()).toEqual([...TOOL_IDS].sort());
  expect(roster.find((r) => r.id === "amt")!.presence).toBe("healthy");
  // a thrown prober is absent, not a crash
  const sirius = roster.find((r) => r.id === "sirius")!;
  expect(sirius.presence).toBe("absent");
  expect(sirius.reason).toContain("probe failed");
});

// ---- the probe actually bounds wall-clock time -------------------------------

test("probeCommand returns within the timeout even when a grandchild holds stdout open", async () => {
  // Regression: killing the child does not close the stdout pipe if a
  // grandchild inherited it, so awaiting EOF blocked for the full sleep.
  const t0 = Date.now();
  const r = await probeCommand("sh", ["-c", "sleep 5; echo done"], 300);
  const elapsed = Date.now() - t0;
  expect(r.timedOut).toBe(true);
  expect(elapsed).toBeLessThan(2000); // was ~5000 before the fix
});

test("probeCommand returns a fast command's stdout and exit code", async () => {
  const r = await probeCommand("sh", ["-c", 'printf "{\\"ok\\":true}"; exit 0'], 2000);
  expect(r.timedOut).toBe(false);
  expect(r.code).toBe(0);
  expect(r.stdout).toBe('{"ok":true}');
});

test("probeCommand propagates a non-zero exit", async () => {
  const r = await probeCommand("sh", ["-c", "exit 3"], 2000);
  expect(r.code).toBe(3);
});

// ---- framing probe ----------------------------------------------------------

const resWith = (headers: Record<string, string>) =>
  (async () => new Response("ok", { headers })) as unknown as typeof fetch;

test("probeUi: plain 200 is frameable", async () => {
  expect(await probeUi("http://localhost:1776", resWith({}))).toEqual({
    reachable: true, frameable: true, reason: null,
  });
});

test("probeUi: X-Frame-Options DENY / SAMEORIGIN refuse framing", async () => {
  for (const v of ["DENY", "SAMEORIGIN", "sameorigin"]) {
    const r = await probeUi("http://localhost:1776", resWith({ "x-frame-options": v }));
    expect(r.reachable).toBe(true);
    expect(r.frameable).toBe(false);
  }
});

test("probeUi: CSP frame-ancestors 'none'/'self' refuse framing", async () => {
  for (const v of ["frame-ancestors 'none'", "default-src *; frame-ancestors 'self'"]) {
    const r = await probeUi("http://localhost:1776", resWith({ "content-security-policy": v }));
    expect(r.frameable).toBe(false);
  }
});

test("probeUi: a redirect is never framed (the iframe would follow it off loopback)", async () => {
  const redirector = (async () =>
    new Response(null, { status: 302, headers: { location: "https://evil.example/" } })) as unknown as typeof fetch;
  const r = await probeUi("http://localhost:1776", redirector);
  expect(r.reachable).toBe(true);
  expect(r.frameable).toBe(false);
  expect(r.reason).toContain("evil.example");
});

test("probeUi: unreachable UI is not a crash", async () => {
  const dead = (async () => { throw new Error("ECONNREFUSED"); }) as unknown as typeof fetch;
  expect(await probeUi("http://localhost:1776", dead)).toEqual({
    reachable: false, frameable: false, reason: "UI not reachable",
  });
});

// ---- scan ordering + hub routes ---------------------------------------------

const fakeProber = (map: Partial<Record<ToolId, ProbeResult>>) =>
  async (id: ToolId) => map[id] ?? { found: false, timedOut: false, code: null, stdout: "" };

test("scan sorts healthy first, then unhealthy, then absent", async () => {
  const roster = await scan(
    fakeProber({
      amt: ok(amtEnvelope),
      sirius: ok({ tool: "sirius", version: "0.1.0", schemaVersion: 1, ok: false, capabilities: [], checks: [] }),
    }),
    resWith({}),
  );
  expect(roster.tools.map((t) => t.presence)).toEqual(["healthy", "unhealthy", "absent", "absent"]);
  const first = roster.tools[0]!;
  expect(first.id).toBe("amt");
  expect(first.uiFrameable).toBe(true);
});

test("a present tool whose UI is down stays present (degraded, not absent)", async () => {
  const dead = (async () => { throw new Error("ECONNREFUSED"); }) as unknown as typeof fetch;
  const roster = await scan(fakeProber({ amt: ok(amtEnvelope) }), dead);
  const amt = roster.tools.find((t) => t.id === "amt")!;
  expect(amt.presence).toBe("healthy");
  expect(amt.uiReachable).toBe(false);
  expect(amt.ui).toBe("http://localhost:1776");
});

test("isSelfUrl matches the hub's own port, including implicit ports", () => {
  expect(isSelfUrl("http://localhost:6969", 6969)).toBe(true);
  expect(isSelfUrl("http://localhost:1776", 6969)).toBe(false);
  expect(isSelfUrl("http://localhost", 80)).toBe(true);
  expect(isSelfUrl("https://localhost", 443)).toBe(true);
  expect(isSelfUrl("garbage", 6969)).toBe(false);
});

test("a tool claiming the hub's own port as its UI is never framed", async () => {
  // Regression: `sirius doctor` inherited a parent's PORT=6969 and advertised
  // the hub's address, which would have made the hub iframe itself.
  const roster = await scan(
    fakeProber({
      sirius: ok({
        tool: "sirius", version: "0.1.0", schemaVersion: 1, ok: true,
        capabilities: ["ui"], ui: "http://localhost:6969", checks: [],
      }),
    }),
    resWith({}),
    6969,
  );
  const s = roster.tools.find((t) => t.id === "sirius")!;
  expect(s.presence).toBe("healthy");
  expect(s.ui).toBeNull();
  expect(s.uiReason).toContain("hub's own port");
});

test("roster cache memoizes within the ttl and rescans when stale", async () => {
  let calls = 0;
  const prober = async (): Promise<ProbeResult> => {
    calls++;
    return { found: false, timedOut: false, code: null, stdout: "" };
  };
  const get = createRosterCache(prober, resWith({}), 10_000);
  await get(1000);
  await get(2000);
  expect(calls).toBe(TOOL_IDS.length); // one scan, four probes
  await get(Number.POSITIVE_INFINITY);
  expect(calls).toBe(TOOL_IDS.length * 2);
});

const stubRoster: Roster = { ts: "2026-07-10T00:00:00Z", tools: [] };

test("GET /api/roster returns json, no-store", async () => {
  const res = await handle(new Request("http://localhost:6969/api/roster"), async () => stubRoster);
  expect(res.status).toBe(200);
  expect(res.headers.get("cache-control")).toBe("no-store");
  expect(await res.json()).toEqual(stubRoster);
});

test("POST /api/roster is rejected", async () => {
  const res = await handle(
    new Request("http://localhost:6969/api/roster", { method: "POST" }),
    async () => stubRoster,
  );
  expect(res.status).toBe(405);
});

test("GET / serves the hub page; unknown paths 404", async () => {
  const page = await handle(new Request("http://localhost:6969/"), async () => stubRoster);
  expect(page.status).toBe(200);
  expect(await page.text()).toContain("Suite Hub");

  const missing = await handle(new Request("http://localhost:6969/nope"), async () => stubRoster);
  expect(missing.status).toBe(404);
});
