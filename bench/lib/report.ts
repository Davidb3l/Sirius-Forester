/**
 * bench/lib/report.ts
 *
 * Uniform metric reporting for all harnesses. Each harness measures ONE number
 * tied to a PRD §8 success metric and prints it here. The last line of every
 * harness is a machine-parseable `METRIC` line so CI's bench-smoke step can
 * assert a value was produced.
 */

export interface Metric {
  /** The PRD §8 metric name, e.g. "provenance_coverage". */
  name: string;
  /** The measured value. */
  value: number;
  /** Unit label for humans, e.g. "%", "double-claims", "tokens". */
  unit: string;
  /** The PRD §8 target this metric is judged against. */
  target: string;
  /** Whether the measured value meets the target. */
  pass: boolean;
  /** "fixture" (simulated, offline) or "live" (real sirius binary). */
  mode: "fixture" | "live";
  /** Optional extra fields printed as a sub-table. */
  detail?: Record<string, string | number | boolean>;
}

export function report(m: Metric): void {
  const bar = "─".repeat(60);
  console.log(bar);
  console.log(`  metric : ${m.name}`);
  console.log(`  value  : ${m.value} ${m.unit}`);
  console.log(`  target : ${m.target}`);
  console.log(`  result : ${m.pass ? "PASS" : "FAIL"}   (mode: ${m.mode})`);
  if (m.detail) {
    console.log(`  detail :`);
    for (const [k, v] of Object.entries(m.detail)) {
      console.log(`    ${k.padEnd(28)} ${v}`);
    }
  }
  console.log(bar);
  // Machine-parseable final line. CI bench-smoke greps for `METRIC `.
  console.log(
    `METRIC ${m.name} value=${m.value} unit=${m.unit} pass=${m.pass} mode=${m.mode}`,
  );
}

/** Fail the process (non-zero exit) only when explicitly asked to enforce the
 *  target. By default harnesses report and exit 0 so a smoke run never breaks
 *  CI just because a simulated number sits outside target; pass --enforce to
 *  make the harness gate. */
export function finish(m: Metric): void {
  report(m);
  const enforce = Bun.argv.includes("--enforce");
  if (enforce && !m.pass) {
    process.exitCode = 1;
  }
}

/** Parse a common CLI flag like `--workers=4` or `--duration 90`. */
export function argValue(name: string, fallback: string): string {
  const argv = Bun.argv;
  const eq = argv.find((a) => a.startsWith(`--${name}=`));
  if (eq) return eq.split("=").slice(1).join("=");
  const idx = argv.indexOf(`--${name}`);
  if (idx >= 0 && idx + 1 < argv.length) return argv[idx + 1];
  return fallback;
}

export function argFlag(name: string): boolean {
  return Bun.argv.includes(`--${name}`);
}
