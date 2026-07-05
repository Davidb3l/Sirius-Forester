// The ONLY place the Console shells out to the `sirius` binary (PRD §2, §4).
// Every mutation and every "why" lookup goes through here so the boundary swaps
// to the real binary cleanly. Parses the CONTRACTS.md §2 --json shapes.
//
// Exit codes (CONTRACTS §2): 0 ok, 1 operational failure, 2 usage error,
// 3 gate/oracle "blocked" (soft). We surface code 3 as a non-error "blocked"
// result rather than throwing, matching Hayvenhurst conventions.

/**
 * A request-input rejection at the argv boundary — invalid ref/tier/status/
 * symbols. The server maps this to HTTP 400. Never let unvalidated body fields
 * reach `Bun.spawn` argv: `{"issue":"--tier"}` must not become a flag. (SIRF-11)
 */
export class ValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ValidationError";
  }
}

/** Issue/decision refs: a letter-prefix and a number, e.g. AMT-7, SIRF-11, D-3. */
const REF_RE = /^[A-Za-z]+-\d+$/;
/** The only tiers Sirius accepts (CONTRACTS §3). */
const TIERS = new Set(["safe", "observed"]);
/** The Ametrite workflow statuses a gate may target. */
const STATUSES = new Set([
  "backlog",
  "todo",
  "in_progress",
  "in_review",
  "done",
  "blocked",
  "cancelled",
]);

function assertRef(ref: unknown, kind: string): asserts ref is string {
  if (typeof ref !== "string" || !REF_RE.test(ref)) {
    throw new ValidationError(
      `invalid ${kind} ${JSON.stringify(ref)} (expected e.g. AMT-7)`,
    );
  }
}

function assertTier(tier: string | undefined): void {
  if (tier !== undefined && !TIERS.has(tier)) {
    throw new ValidationError(
      `invalid tier ${JSON.stringify(tier)} (expected one of ${[...TIERS].join(", ")})`,
    );
  }
}

function assertStatus(status: string | undefined): void {
  if (status !== undefined && !STATUSES.has(status)) {
    throw new ValidationError(
      `invalid target-status ${JSON.stringify(status)}`,
    );
  }
}

/** Symbols must be a string[] with no argv-injecting values. */
function assertSymbols(symbols: unknown): asserts symbols is string[] {
  if (!Array.isArray(symbols) || symbols.some((s) => typeof s !== "string")) {
    throw new ValidationError("symbols must be an array of strings");
  }
  for (const s of symbols as string[]) {
    if (s.length === 0 || s.startsWith("-") || s.includes(",")) {
      throw new ValidationError(
        `invalid symbol ${JSON.stringify(s)} (no leading '-' or ',')`,
      );
    }
  }
}

export interface SiriusResult<T> {
  ok: boolean;
  exitCode: number;
  data: T | null;
  /** stderr text (logs) for diagnostics */
  stderr: string;
  /** true when exit code 3 (gate/oracle soft-blocked) */
  blocked: boolean;
  error: string | null;
}

export interface SiriusOptions {
  /** path to the sirius binary; defaults to $SIRIUS_BIN or "sirius" on PATH */
  bin?: string;
  /** cwd for the child (repo root); defaults to process.cwd() */
  cwd?: string;
  /** timeout ms */
  timeoutMs?: number;
}

// ---- §2 output shapes -------------------------------------------------------

export interface LinkResult {
  ok: boolean;
  receipt_id: number;
  kind: "issue" | "decision";
  ref: string;
  symbols: string[];
  forward_ok: boolean;
  reverse_ok: boolean;
}

export interface WhySymbolResult {
  symbol: string;
  issues: { ref: string; title: string }[];
  decisions: { ref: string; summary: string }[];
}

export interface WhyIssueResult {
  ref: string;
  symbols: string[];
  decisions: string[];
}

export interface GateResultShape {
  ok: boolean;
  issue: string;
  tier: string;
  gate: "pass" | "fail";
  advanced_to: string | null;
  tests_selected: number;
  comment_filed: boolean;
}

export interface DoctorCheck {
  name: string;
  pass: boolean;
  detail: string;
}
export interface DoctorResult {
  ok: boolean;
  checks: DoctorCheck[];
}

// ---- runner -----------------------------------------------------------------

export class Sirius {
  private bin: string;
  private cwd: string;
  private timeoutMs: number;

  constructor(opts: SiriusOptions = {}) {
    this.bin = opts.bin ?? process.env.SIRIUS_BIN ?? "sirius";
    this.cwd = opts.cwd ?? process.cwd();
    this.timeoutMs = opts.timeoutMs ?? 60_000;
  }

  /** Is the binary resolvable on PATH / at $SIRIUS_BIN? */
  async available(): Promise<boolean> {
    try {
      const r = await this.raw(["--version"]);
      return r.exitCode === 0 || r.exitCode === 2;
    } catch {
      return false;
    }
  }

  /** Run `sirius <args>` and parse exactly ONE json object from stdout (§2). */
  async json<T>(args: string[]): Promise<SiriusResult<T>> {
    const withJson = args.includes("--json") ? args : [...args, "--json"];
    const raw = await this.raw(withJson);
    const blocked = raw.exitCode === 3;
    let data: T | null = null;
    let error: string | null = null;
    const trimmed = raw.stdout.trim();
    if (trimmed) {
      try {
        data = JSON.parse(trimmed) as T;
      } catch (e) {
        error = `unparseable sirius --json stdout: ${
          e instanceof Error ? e.message : String(e)
        }`;
      }
    } else if (raw.exitCode !== 0 && !blocked) {
      error = raw.stderr.trim() || `sirius exited ${raw.exitCode}`;
    }
    return {
      ok: raw.exitCode === 0 && data != null && error == null,
      exitCode: raw.exitCode,
      data,
      stderr: raw.stderr,
      blocked,
      error,
    };
  }

  /** Low-level exec. stdout/stderr kept separate per §2 (json on stdout only). */
  async raw(
    args: string[],
  ): Promise<{ exitCode: number; stdout: string; stderr: string }> {
    const proc = Bun.spawn([this.bin, ...args], {
      cwd: this.cwd,
      stdout: "pipe",
      stderr: "pipe",
      env: process.env,
    });
    const killer = setTimeout(() => {
      try {
        proc.kill();
      } catch {
        /* ignore */
      }
    }, this.timeoutMs);
    try {
      const [stdout, stderr, exitCode] = await Promise.all([
        new Response(proc.stdout).text(),
        new Response(proc.stderr).text(),
        proc.exited,
      ]);
      return { exitCode, stdout, stderr };
    } finally {
      clearTimeout(killer);
    }
  }

  // Typed §2 conveniences ----------------------------------------------------

  whySymbol(symbol: string) {
    return this.json<WhySymbolResult>(["why", symbol]);
  }
  whyIssue(ref: string) {
    assertRef(ref, "ref");
    return this.json<WhyIssueResult>(["why", ref]);
  }
  gate(issue: string, tier?: string, targetStatus?: string) {
    assertRef(issue, "issue");
    assertTier(tier);
    assertStatus(targetStatus);
    const args = ["gate", issue];
    if (tier) args.push("--tier", tier);
    if (targetStatus) args.push("--target-status", targetStatus);
    return this.json<GateResultShape>(args);
  }
  linkIssue(ref: string, symbols: string[], changed = false) {
    assertRef(ref, "ref");
    assertSymbols(symbols);
    const args = ["link", ref, "--symbols", symbols.join(",")];
    if (changed) args.push("--changed");
    return this.json<LinkResult>(args);
  }
  linkDecision(ref: string, symbols: string[], changed = false) {
    assertRef(ref, "decision");
    assertSymbols(symbols);
    const args = ["link", "--decision", ref, "--symbols", symbols.join(",")];
    if (changed) args.push("--changed");
    return this.json<LinkResult>(args);
  }
  doctor() {
    return this.json<DoctorResult>(["doctor"]);
  }
}
