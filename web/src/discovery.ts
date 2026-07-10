// Suite discovery: probe each tool's `<tool> doctor --json` and build a roster.
// Implements SUITE_CONTRACTS §3 (doctor handshake), §3.1 (absent vs
// present-but-unhealthy), §3.2 (the `ui` capability).
//
// The hub owns no state and never writes a peer's store. It probes, classifies,
// and displays. Peer absence is a state, not an error.

/** The four tool ids, pinned to their CLI names (§3). */
export const TOOL_IDS = ["amt", "hayven", "sirius", "catryna"] as const;
export type ToolId = (typeof TOOL_IDS)[number];

export const TOOL_META: Record<
  ToolId,
  { label: string; blurb: string; install: string }
> = {
  amt: {
    label: "Ametrite",
    blurb: "Issues and knowledge base",
    install: "https://github.com/Davidb3l/Ametrite",
  },
  hayven: {
    label: "Hayvenhurst",
    blurb: "Code graph daemon",
    install: "https://github.com/Davidb3l/Hayvenhurst-dev",
  },
  sirius: {
    label: "Sirius Forester",
    blurb: "Fleet foreman",
    install: "https://github.com/Davidb3l/Sirius-Forester",
  },
  catryna: {
    label: "Catryna Wikinelli",
    blurb: "Code wiki",
    install: "https://github.com/Davidb3l/Catryna-Wikinelli",
  },
};

/** §3.1: absent (nothing trustworthy said) vs present-healthy vs present-unhealthy. */
export type Presence = "healthy" | "unhealthy" | "absent";

export interface DoctorCheck {
  name: string;
  ok: boolean;
  detail: string;
}

export interface ToolStatus {
  id: ToolId;
  label: string;
  blurb: string;
  install: string;
  presence: Presence;
  /** Why it is absent, or why its UI cannot be framed. Human-readable. */
  reason: string | null;
  version: string | null;
  capabilities: string[];
  /** Only set when the tool advertises the `ui` capability AND a loopback URL. */
  ui: string | null;
  checks: DoctorCheck[];
}

/** Raw result of invoking a CLI. `found:false` means the binary isn't on PATH. */
export interface ProbeResult {
  found: boolean;
  timedOut: boolean;
  code: number | null;
  stdout: string;
}

export type Prober = (id: ToolId, timeoutMs: number) => Promise<ProbeResult>;

export const DEFAULT_TIMEOUT_MS = 2000;

/**
 * A tool may only point us at its own machine. The hub embeds `ui` in an
 * iframe, so a tool that reported `https://evil.example` would have the hub
 * frame it. Loopback only, http(s) only.
 */
export function isLoopbackUrl(raw: unknown): raw is string {
  if (typeof raw !== "string" || raw === "") return false;
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return false;
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") return false;
  return (
    u.hostname === "localhost" ||
    u.hostname === "127.0.0.1" ||
    u.hostname === "::1" ||
    u.hostname === "[::1]"
  );
}

function absent(id: ToolId, reason: string): ToolStatus {
  const m = TOOL_META[id];
  return {
    id,
    label: m.label,
    blurb: m.blurb,
    install: m.install,
    presence: "absent",
    reason,
    version: null,
    capabilities: [],
    ui: null,
    checks: [],
  };
}

function normalizeChecks(raw: unknown): DoctorCheck[] {
  if (!Array.isArray(raw)) return [];
  const out: DoctorCheck[] = [];
  for (const c of raw) {
    if (!c || typeof c !== "object") continue;
    const o = c as Record<string, unknown>;
    if (typeof o.name !== "string") continue;
    // §3 says `ok`; Sirius historically emitted `pass`. Accept either.
    const ok = typeof o.ok === "boolean" ? o.ok : o.pass === true;
    out.push({
      name: o.name,
      ok,
      detail: typeof o.detail === "string" ? o.detail : "",
    });
  }
  return out;
}

/**
 * Turn a raw probe into a roster entry, per §3.1's table. Pure: no I/O, so the
 * classification rules are directly testable.
 */
export function classify(
  id: ToolId,
  r: ProbeResult,
  /** The budget this probe actually ran under — reported verbatim, so a caller
   *  passing a custom `timeoutMs` isn't told the default in the absent reason. */
  timeoutMs: number = DEFAULT_TIMEOUT_MS,
): ToolStatus {
  if (!r.found) return absent(id, "not installed");
  if (r.timedOut) return absent(id, `doctor timed out after ${timeoutMs}ms`);
  if (r.code !== 0) return absent(id, `doctor exited ${r.code}`);

  let env: unknown;
  try {
    env = JSON.parse(r.stdout.trim());
  } catch {
    // §4 rule 1: `--json` means exactly one JSON object on stdout. Anything
    // else is unparseable, hence absent, even with exit 0.
    return absent(id, "doctor --json did not emit JSON on stdout");
  }
  if (!env || typeof env !== "object" || Array.isArray(env)) {
    return absent(id, "doctor --json did not emit a JSON object");
  }
  const o = env as Record<string, unknown>;

  // §3: the reply's `tool` must equal the CLI we invoked.
  if (o.tool !== id) {
    return absent(id, `doctor reported tool "${String(o.tool)}", expected "${id}"`);
  }

  const capabilities = Array.isArray(o.capabilities)
    ? o.capabilities.filter((c): c is string => typeof c === "string")
    : [];

  // §3.2: `ui` counts only when advertised AND loopback.
  let ui: string | null = null;
  let reason: string | null = null;
  if (capabilities.includes("ui")) {
    if (isLoopbackUrl(o.ui)) ui = o.ui;
    else reason = "advertises `ui` but its URL is missing or not loopback";
  }

  const m = TOOL_META[id];
  return {
    id,
    label: m.label,
    blurb: m.blurb,
    install: m.install,
    presence: o.ok === true ? "healthy" : "unhealthy",
    reason,
    version: typeof o.version === "string" ? o.version : null,
    capabilities,
    ui,
    checks: normalizeChecks(o.checks),
  };
}

/** Never buffer an unbounded amount of a misbehaving tool's stdout. */
export const MAX_STDOUT_BYTES = 1 << 20; // 1 MiB

async function readCapped(stream: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!value) continue;
      total += value.byteLength;
      if (total > MAX_STDOUT_BYTES) break;
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const buf = new Uint8Array(chunks.reduce((n, c) => n + c.byteLength, 0));
  let at = 0;
  for (const c of chunks) {
    buf.set(c, at);
    at += c.byteLength;
  }
  return new TextDecoder().decode(buf);
}

/**
 * Run `<bin> doctor --json` and return within `timeoutMs`, no matter what.
 *
 * Killing the child is NOT sufficient to unblock the read: stdout only reaches
 * EOF when every write end of the pipe closes, and a grandchild that inherited
 * the fd keeps it open. (Measured: a child running `sh -c "sleep 5"` with a
 * 500ms timer still took 5021ms to resolve.) So we race the read against the
 * timer and return as soon as either wins; the kill is best-effort cleanup.
 */
export async function probeCommand(
  bin: string,
  args: string[],
  timeoutMs: number,
): Promise<ProbeResult> {
  const proc = Bun.spawn([bin, ...args], {
    stdout: "pipe",
    stderr: "ignore",
    env: process.env,
  });

  let timer: ReturnType<typeof setTimeout> | undefined;
  const expired = new Promise<ProbeResult>((resolve) => {
    timer = setTimeout(() => {
      proc.kill("SIGKILL");
      resolve({ found: true, timedOut: true, code: null, stdout: "" });
    }, timeoutMs);
  });

  const completed = (async (): Promise<ProbeResult> => {
    const stdout = await readCapped(proc.stdout as ReadableStream<Uint8Array>);
    const code = await proc.exited;
    return { found: true, timedOut: false, code, stdout };
  })().catch((): ProbeResult => ({ found: true, timedOut: false, code: null, stdout: "" }));

  try {
    return await Promise.race([completed, expired]);
  } finally {
    clearTimeout(timer);
    // If the timer won, the child may still be dying; don't leave it running.
    if (!proc.killed) proc.kill("SIGKILL");
  }
}

/** Spawn `<tool> doctor --json` with a hard wall-clock timeout. */
export const spawnProber: Prober = async (id, timeoutMs) => {
  const bin = Bun.which(id);
  if (!bin) return { found: false, timedOut: false, code: null, stdout: "" };
  return probeCommand(bin, ["doctor", "--json"], timeoutMs);
};

/** Probe all four tools concurrently. Never throws: a failed probe is `absent`. */
export async function buildRoster(
  prober: Prober = spawnProber,
  timeoutMs: number = DEFAULT_TIMEOUT_MS,
): Promise<ToolStatus[]> {
  return Promise.all(
    TOOL_IDS.map(async (id) => {
      try {
        return classify(id, await prober(id, timeoutMs), timeoutMs);
      } catch (e) {
        return absent(id, `probe failed: ${e instanceof Error ? e.message : String(e)}`);
      }
    }),
  );
}
