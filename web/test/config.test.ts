import { test, expect } from "bun:test";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadConfig, DEFAULT_CONFIG } from "../src/config.ts";

test("absent config file yields committed defaults", () => {
  const dir = mkdtempSync(join(tmpdir(), "sirius-cfg-"));
  const view = loadConfig(join(dir, "config.json"));
  expect(view.present).toBe(false);
  expect(view.config).toEqual(DEFAULT_CONFIG);
  expect(view.raw).toBeNull();
  rmSync(dir, { recursive: true, force: true });
});

test("present config overrides defaults and records raw keys", () => {
  const dir = mkdtempSync(join(tmpdir(), "sirius-cfg-"));
  const p = join(dir, "config.json");
  writeFileSync(p, JSON.stringify({ worker_concurrency: 8, gate_tier: "full" }));
  const view = loadConfig(p);
  expect(view.present).toBe(true);
  expect(view.config.worker_concurrency).toBe(8);
  expect(view.config.gate_tier).toBe("full");
  // untouched keys keep defaults
  expect(view.config.retry_budget).toBe(DEFAULT_CONFIG.retry_budget);
  expect(view.raw).toHaveProperty("worker_concurrency");
  rmSync(dir, { recursive: true, force: true });
});

test("malformed config reports error and falls back", () => {
  const dir = mkdtempSync(join(tmpdir(), "sirius-cfg-"));
  const p = join(dir, "config.json");
  writeFileSync(p, "{ not valid json");
  const view = loadConfig(p);
  expect(view.present).toBe(true);
  expect(view.error).toBeTruthy();
  expect(view.config).toEqual(DEFAULT_CONFIG);
  rmSync(dir, { recursive: true, force: true });
});
