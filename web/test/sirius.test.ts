import { test, expect, beforeAll, afterAll } from "bun:test";
import { mkdtempSync, writeFileSync, chmodSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Sirius, ValidationError } from "../src/sirius.ts";

// A fake `sirius` binary: a shell script that emits CONTRACTS §2 shapes on
// stdout, logs to stderr, and honours the §2 exit codes. Verifies the boundary
// parses each shape and treats exit 3 as "blocked", not an error.
let dir: string;
let bin: string;

const SCRIPT = `#!/usr/bin/env bash
# fake sirius honoring CONTRACTS.md §2
echo "log line to stderr" >&2
case "$1" in
  --version) echo "sirius 0.1.0"; exit 0 ;;
  gate)
    issue="$2"
    if [ "$issue" = "AMT-99" ]; then
      echo '{"ok":false,"issue":"AMT-99","tier":"safe","gate":"fail","advanced_to":null,"tests_selected":12,"comment_filed":true}'
      exit 3
    fi
    echo '{"ok":true,"issue":"'"$issue"'","tier":"safe","gate":"pass","advanced_to":"in_review","tests_selected":7,"comment_filed":false}'
    exit 0 ;;
  link)
    echo '{"ok":true,"receipt_id":42,"kind":"issue","ref":"AMT-7","symbols":["a","b"],"forward_ok":true,"reverse_ok":true}'
    exit 0 ;;
  why)
    echo '{"ref":"AMT-7","symbols":["a","b"],"decisions":["D-3"]}'
    exit 0 ;;
  doctor)
    echo '{"ok":true,"checks":[{"name":"amt","pass":true,"detail":"0.1.0"}]}'
    exit 0 ;;
  *) echo "usage" >&2; exit 2 ;;
esac
`;

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "sirius-bin-"));
  bin = join(dir, "sirius");
  writeFileSync(bin, SCRIPT);
  chmodSync(bin, 0o755);
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));

test("available() detects the binary", async () => {
  const s = new Sirius({ bin });
  expect(await s.available()).toBe(true);
});

test("missing binary reports unavailable, not a crash", async () => {
  const s = new Sirius({ bin: join(dir, "does-not-exist") });
  expect(await s.available()).toBe(false);
});

test("gate parses the §2 shape and advances", async () => {
  const s = new Sirius({ bin });
  const r = await s.gate("AMT-7");
  expect(r.ok).toBe(true);
  expect(r.blocked).toBe(false);
  expect(r.data?.gate).toBe("pass");
  expect(r.data?.advanced_to).toBe("in_review");
  expect(r.data?.tests_selected).toBe(7);
});

test("exit code 3 surfaces as blocked, not error", async () => {
  const s = new Sirius({ bin });
  const r = await s.gate("AMT-99");
  expect(r.exitCode).toBe(3);
  expect(r.blocked).toBe(true);
  expect(r.data?.gate).toBe("fail");
  expect(r.data?.comment_filed).toBe(true);
});

test("link parses the receipt shape", async () => {
  const s = new Sirius({ bin });
  const r = await s.linkIssue("AMT-7", ["a", "b"]);
  expect(r.data?.receipt_id).toBe(42);
  expect(r.data?.forward_ok).toBe(true);
  expect(r.data?.reverse_ok).toBe(true);
});

test("why issue parses symbols + decisions", async () => {
  const s = new Sirius({ bin });
  const r = await s.whyIssue("AMT-7");
  expect(r.data?.symbols).toEqual(["a", "b"]);
  expect(r.data?.decisions).toEqual(["D-3"]);
});

test("stdout stays json-only; stderr captured separately", async () => {
  const s = new Sirius({ bin });
  const r = await s.gate("AMT-7");
  expect(r.stderr).toContain("log line to stderr");
  expect(r.data).not.toBeNull(); // stderr didn't corrupt json parse
});

// ---- SIRF-11: input validation at the argv boundary -------------------------

test("gate rejects a ref that would become an argv flag", () => {
  const s = new Sirius({ bin });
  // Must throw BEFORE spawning — an invalid ref never reaches Bun.spawn.
  expect(() => s.gate("--tier")).toThrow(ValidationError);
  expect(() => s.gate("AMT-7; rm -rf /")).toThrow(ValidationError);
  expect(() => s.gate("not a ref")).toThrow(ValidationError);
});

test("gate rejects a non-whitelisted tier / status", () => {
  const s = new Sirius({ bin });
  expect(() => s.gate("AMT-7", "--evil")).toThrow(ValidationError);
  expect(() => s.gate("AMT-7", "safe", "--wat")).toThrow(ValidationError);
});

test("link rejects non-string symbols and injecting values", () => {
  const s = new Sirius({ bin });
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  expect(() => s.linkIssue("AMT-7", 5 as any)).toThrow(ValidationError);
  expect(() => s.linkIssue("AMT-7", ["--changed"])).toThrow(ValidationError);
  expect(() => s.linkDecision("D-3", ["ok"])).not.toThrow();
});

test("valid refs pass validation (AMT-7, SIRF-11, D-3)", () => {
  const s = new Sirius({ bin });
  expect(() => s.gate("SIRF-11")).not.toThrow();
  expect(() => s.whyIssue("D-3")).not.toThrow();
});
