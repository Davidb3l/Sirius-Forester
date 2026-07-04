// Read-only SQLite access to the three stores (PRD §2.2, §3).
// The Console NEVER writes any SQLite. All opens are read-only.
import { Database } from "bun:sqlite";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

export interface Workspace {
  /** dir that holds .sirius/ (repo root) */
  root: string;
  ledgerPath: string; // .sirius/sirius.db
  configPath: string; // .sirius/config.json
  ametritePath: string | null; // .ametrite/ametrite.db (read-only, optional)
  hayvenDir: string | null; // .hayven/ (read-only, optional)
}

/**
 * Walk up from `start` looking for a `.sirius/` directory. The ledger itself
 * may not exist yet (core agent builds it) but we still resolve the intended
 * paths. If SIRIUS_LEDGER env is set it wins (used by fixtures/tests).
 */
export function discoverWorkspace(start = process.cwd()): Workspace {
  const envLedger = process.env.SIRIUS_LEDGER;
  if (envLedger) {
    const ledgerPath = resolve(envLedger);
    const siriusDir = dirname(ledgerPath);
    const root = dirname(siriusDir);
    return {
      root,
      ledgerPath,
      configPath: join(siriusDir, "config.json"),
      ametritePath: firstExisting([join(root, ".ametrite", "ametrite.db")]),
      hayvenDir: firstExistingDir([join(root, ".hayven")]),
    };
  }

  let dir = resolve(start);
  // eslint-disable-next-line no-constant-condition
  while (true) {
    if (existsSync(join(dir, ".sirius"))) {
      return {
        root: dir,
        ledgerPath: join(dir, ".sirius", "sirius.db"),
        configPath: join(dir, ".sirius", "config.json"),
        ametritePath: firstExisting([join(dir, ".ametrite", "ametrite.db")]),
        hayvenDir: firstExistingDir([join(dir, ".hayven")]),
      };
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  // Fall back to cwd/.sirius even if absent, so the server can start and show
  // a "ledger not found" state rather than crashing.
  const root = resolve(start);
  return {
    root,
    ledgerPath: join(root, ".sirius", "sirius.db"),
    configPath: join(root, ".sirius", "config.json"),
    ametritePath: firstExisting([join(root, ".ametrite", "ametrite.db")]),
    hayvenDir: firstExistingDir([join(root, ".hayven")]),
  };
}

function firstExisting(paths: string[]): string | null {
  for (const p of paths) if (existsSync(p)) return p;
  return null;
}
function firstExistingDir(paths: string[]): string | null {
  for (const p of paths) if (existsSync(p)) return p;
  return null;
}

/**
 * Open a SQLite file read-only. Uses bun:sqlite readonly mode; sets a busy
 * timeout so concurrent WAL writers (the real sirius/amt) never error us out.
 * Returns null if the file does not exist yet.
 */
export function openReadOnly(path: string): Database | null {
  if (!existsSync(path)) return null;
  const db = new Database(path, { readonly: true });
  // WAL readers coexist with writers; a small busy timeout guards checkpoints.
  db.exec("PRAGMA busy_timeout=2000;");
  return db;
}

/**
 * PRAGMA data_version — bumps whenever another connection commits to the DB.
 * This is the SSE change signal (PRD §3, CONTRACTS §1). Cheap to poll.
 */
export function dataVersion(db: Database): number {
  const row = db.query("PRAGMA data_version;").get() as
    | { data_version: number }
    | Record<string, number>
    | null;
  if (!row) return 0;
  const v = (row as Record<string, number>)["data_version"];
  return typeof v === "number" ? v : Number(Object.values(row)[0] ?? 0);
}
