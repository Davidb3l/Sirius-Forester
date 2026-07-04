/**
 * bench/lib/sirius.ts
 *
 * Thin bridge to the `sirius` binary for harnesses that CAN run live.
 *
 * The core binary does not exist yet, so every harness must run to completion
 * offline. This module exposes `siriusAvailable()` and a `runSiriusJson()` that
 * shells `sirius <args> --json` per CONTRACTS.md §2. Harnesses call these only
 * behind a guard; when the binary is missing they fall back to fixture mode.
 *
 * ASSUMPTIONS the core agent must honor (reported back to the parent):
 *  - Every mutating subcommand accepts `--json` and prints exactly ONE JSON
 *    object to stdout, nothing else (logs to stderr). CONTRACTS.md §2.
 *  - `sirius run ... --json` streams NDJSON, one iteration event per line.
 *  - Exit codes: 0 ok, 1 operational failure, 2 usage error, 3 soft blocked.
 */

export interface RunResult {
  ok: boolean;
  code: number;
  stdout: string;
  stderr: string;
}

/** Whether a real `sirius` binary is on PATH (or SIRIUS_BIN points at one). */
export async function siriusAvailable(): Promise<boolean> {
  const bin = Bun.env.SIRIUS_BIN ?? "sirius";
  try {
    const proc = Bun.spawn([bin, "--version"], {
      stdout: "pipe",
      stderr: "pipe",
    });
    const code = await proc.exited;
    return code === 0;
  } catch {
    return false;
  }
}

/** Run `sirius <args>` and capture stdout/stderr/exit code. */
export async function runSirius(args: string[]): Promise<RunResult> {
  const bin = Bun.env.SIRIUS_BIN ?? "sirius";
  const proc = Bun.spawn([bin, ...args], { stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  const code = await proc.exited;
  return { ok: code === 0, code, stdout, stderr };
}

/** Run `sirius <args> --json` and parse the single stdout JSON object. */
export async function runSiriusJson<T = unknown>(
  args: string[],
): Promise<{ result: RunResult; json: T | null }> {
  const result = await runSirius([...args, "--json"]);
  let json: T | null = null;
  try {
    json = JSON.parse(result.stdout.trim()) as T;
  } catch {
    json = null;
  }
  return { result, json };
}

/** Are we running in CI? Used to pick short soak durations by default. */
export function isCI(): boolean {
  return Bun.env.CI === "true" || Bun.env.CI === "1";
}
