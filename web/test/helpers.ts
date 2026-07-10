// Test-teardown helper. Not a test file (bun test only collects *.test.ts).
import { rmSync } from "node:fs";

/**
 * Remove a temp dir that held a sqlite ledger.
 *
 * `Database.close()` maps to sqlite3_close_v2, a *deferred* close: the file
 * handle survives until Bun finalizes the statements `db.query()` cached. POSIX
 * happily unlinks an already-open file, so this is invisible there. Windows
 * refuses, and the unlink fails with EBUSY.
 *
 * Force a GC so the cached statements finalize and the handle actually drops,
 * then unlink. Retry a bounded number of times to absorb the residual race.
 * (rmSync's own maxRetries/retryDelay are ignored by Bun, so we do it here.)
 */
export function rmTempDir(dir: string): void {
  Bun.gc(true);
  for (let attempt = 0; ; attempt++) {
    try {
      rmSync(dir, { recursive: true, force: true });
      return;
    } catch (err) {
      if (attempt >= 20) throw err;
      Bun.gc(true);
      Bun.sleepSync(25);
    }
  }
}
