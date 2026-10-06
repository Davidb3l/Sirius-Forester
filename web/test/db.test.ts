import { test, expect } from "bun:test";
import { Database } from "bun:sqlite";
import { existsSync, mkdtempSync, unlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openReadOnly } from "../src/db.ts";
import { rmTempDir } from "./helpers.ts";

// The state between fleet runs: a WAL ledger whose last writer closed, so its
// -shm/-wal side files are gone. A SQLITE_OPEN_READONLY connection cannot
// create them ("unable to open database file") — the console's Fleet tab
// returned 500 whenever no fleet was running.
test("an idle WAL ledger (no -shm/-wal) still opens — and stays read-only", () => {
  const dir = mkdtempSync(join(tmpdir(), "sirius-db-"));
  try {
    const path = join(dir, "sirius.db");
    const w = new Database(path);
    w.exec("PRAGMA journal_mode=WAL; CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (7);");
    w.exec("PRAGMA wal_checkpoint(TRUNCATE);");
    w.close();
    // What sirius leaves when its last connection closes: no side files.
    for (const f of [`${path}-shm`, `${path}-wal`]) if (existsSync(f)) unlinkSync(f);

    const db = openReadOnly(path)!;
    expect((db.query("SELECT v FROM t").get() as { v: number }).v).toBe(7);
    expect(() => db.exec("INSERT INTO t VALUES (8)")).toThrow();
    db.close();

    const check = new Database(path, { readonly: true });
    expect((check.query("SELECT count(*) AS n FROM t").get() as { n: number }).n).toBe(1);
    check.close();
  } finally {
    rmTempDir(dir);
  }
});

test("a missing file is null, not an error", () => {
  expect(openReadOnly(join(tmpdir(), "sirius-no-such-ledger.db"))).toBeNull();
});
