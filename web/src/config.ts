// Read-only access to .sirius/config.json (CONTRACTS.md §3). Absent file => defaults.
import { existsSync, readFileSync } from "node:fs";

export interface SiriusConfig {
  claim_order_enforced: boolean;
  backoff_409: { strategy: string; base_ms: number; max_ms: number };
  oracle_202: "back-off" | "force-with-budget";
  force_budget_tokens: number;
  gate_tier: string;
  target_status: string;
  retry_budget: number;
  worker_concurrency: number;
  claim_mode: "always" | "never" | "adaptive";
}

export const DEFAULT_CONFIG: SiriusConfig = {
  claim_order_enforced: true,
  backoff_409: { strategy: "release_and_comment", base_ms: 500, max_ms: 8000 },
  oracle_202: "back-off",
  force_budget_tokens: 0,
  gate_tier: "safe",
  target_status: "in_review",
  retry_budget: 3,
  worker_concurrency: 3,
  claim_mode: "adaptive",
};

export interface ConfigView {
  present: boolean;
  path: string;
  config: SiriusConfig;
  /** raw parsed json when present (may contain unknown keys) */
  raw: Record<string, unknown> | null;
  error: string | null;
}

/** Load config from a given path, falling back to committed defaults. */
export function loadConfig(configPath: string): ConfigView {
  if (!existsSync(configPath)) {
    return {
      present: false,
      path: configPath,
      config: DEFAULT_CONFIG,
      raw: null,
      error: null,
    };
  }
  try {
    const raw = JSON.parse(readFileSync(configPath, "utf8")) as Record<
      string,
      unknown
    >;
    return {
      present: true,
      path: configPath,
      config: { ...DEFAULT_CONFIG, ...(raw as Partial<SiriusConfig>) },
      raw,
      error: null,
    };
  } catch (err) {
    return {
      present: true,
      path: configPath,
      config: DEFAULT_CONFIG,
      raw: null,
      error: err instanceof Error ? err.message : String(err),
    };
  }
}
