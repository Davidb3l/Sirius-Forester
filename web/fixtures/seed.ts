// Seed a sample ledger matching CONTRACTS.md §1, for developing the Console
// before the real `sirius` binary exists. Writes to web/fixtures/.sirius/sirius.db
// by default (NOT repo-root .sirius/, to avoid clobbering the core agent's real
// ledger). Override with the first CLI arg.
//
//   bun run fixtures/seed.ts                 -> web/fixtures/.sirius/sirius.db
//   bun run fixtures/seed.ts /tmp/x/sirius.db
//
// Then run the server against it:
//   SIRIUS_LEDGER=$(pwd)/fixtures/.sirius/sirius.db bun run src/server.ts
import { Database } from "bun:sqlite";
import { mkdirSync, rmSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { LEDGER_DDL, SCHEMA_VERSION } from "../src/schema.ts";

const target = resolve(
  process.argv[2] ??
    fileURLToPath(new URL(".sirius/sirius.db", import.meta.url)),
);

export function seed(dbPath: string): string {
  mkdirSync(dirname(dbPath), { recursive: true });
  // start clean (also drop WAL/shm siblings)
  for (const suffix of ["", "-wal", "-shm"]) {
    const p = dbPath + suffix;
    if (existsSync(p)) rmSync(p);
  }

  const db = new Database(dbPath, { create: true });
  db.exec(LEDGER_DDL);

  const iso = (minAgo: number) =>
    new Date(Date.now() - minAgo * 60_000).toISOString();

  // meta
  const meta = db.prepare("INSERT INTO meta(key,value) VALUES(?,?)");
  meta.run("schema_version", String(SCHEMA_VERSION));
  meta.run("created_at", iso(240));
  meta.run("sirius_version", "0.1.0");

  // workers
  const w = db.prepare(
    "INSERT INTO workers(id,created_at,last_seen_at,status) VALUES(?,?,?,?)",
  );
  w.run("sirius/oak", iso(240), iso(0), "working");
  w.run("sirius/rowan", iso(240), iso(1), "blocked");
  w.run("sirius/birch", iso(240), iso(0), "working");
  w.run("sirius/cedar", iso(240), iso(30), "idle");
  w.run("sirius/elm", iso(240), iso(120), "stopped");

  // receipts (a few completed, one partial)
  const rec = db.prepare(
    `INSERT INTO receipts(kind,ref,symbols,forward_ok,reverse_ok,created_at,worker_id)
     VALUES(?,?,?,?,?,?,?)`,
  );
  const r1 = rec.run(
    "issue",
    "AMT-4",
    JSON.stringify(["auth/login.ts#authenticate", "auth/session.ts#mint"]),
    1,
    1,
    iso(180),
    "sirius/oak",
  ).lastInsertRowid as number;
  const r2 = rec.run(
    "decision",
    "D-3",
    JSON.stringify([
      "db/pool.ts#acquire",
      "db/pool.ts#release",
      "db/pool.ts#Pool",
    ]),
    1,
    1,
    iso(150),
    "sirius/birch",
  ).lastInsertRowid as number;
  const r3 = rec.run(
    "issue",
    "AMT-7",
    JSON.stringify(["api/routes/claims.ts#handleClaim"]),
    1,
    0, // partial: reverse stamp didn't land
    iso(60),
    "sirius/rowan",
  ).lastInsertRowid as number;
  const r4 = rec.run(
    "issue",
    "AMT-9",
    JSON.stringify(["console/board.ts#renderFleet", "console/sse.ts#poll"]),
    1,
    1,
    iso(20),
    "sirius/oak",
  ).lastInsertRowid as number;

  // iterations
  const it = db.prepare(
    `INSERT INTO iterations
       (worker_id,issue_ref,entities,started_at,ended_at,outcome,gate_result,
        oracle_verdicts,tokens,duration_ms,receipt_id)
     VALUES(?,?,?,?,?,?,?,?,?,?,?)`,
  );
  // completed history
  it.run(
    "sirius/oak",
    "AMT-4",
    JSON.stringify(["auth/login.ts#authenticate", "auth/session.ts#mint"]),
    iso(185),
    iso(180),
    "completed",
    "pass",
    JSON.stringify(["registered", "registered"]),
    8200,
    300_000,
    r1,
  );
  it.run(
    "sirius/birch",
    "AMT-5",
    JSON.stringify(["db/pool.ts#acquire", "db/pool.ts#release"]),
    iso(158),
    iso(150),
    "completed",
    "pass",
    JSON.stringify(["registered", "forced"]),
    11400,
    480_000,
    r2,
  );
  it.run(
    "sirius/cedar",
    "AMT-6",
    JSON.stringify(["util/hash.ts#djb2"]),
    iso(140),
    iso(138),
    "gate_failed",
    "fail",
    JSON.stringify(["registered"]),
    3100,
    120_000,
    null,
  );
  it.run(
    "sirius/oak",
    "AMT-8",
    JSON.stringify(["fs/walk.ts#walkUp"]),
    iso(100),
    iso(96),
    "released",
    "skipped",
    JSON.stringify(["blocked"]),
    900,
    240_000,
    null,
  );
  it.run(
    "sirius/elm",
    "AMT-2",
    JSON.stringify(["parse/lexer.ts#next"]),
    iso(130),
    iso(120),
    "deadend",
    "fail",
    JSON.stringify(["registered"]),
    6000,
    600_000,
    null,
  );
  it.run(
    "sirius/oak",
    "AMT-9",
    JSON.stringify(["console/board.ts#renderFleet", "console/sse.ts#poll"]),
    iso(25),
    iso(20),
    "completed",
    "pass",
    JSON.stringify(["registered", "registered"]),
    7300,
    300_000,
    r4,
  );

  // OPEN iterations (drive the live fleet board)
  it.run(
    "sirius/oak",
    "AMT-12",
    JSON.stringify(["render/tree.ts#plant", "render/tree.ts#prune"]),
    iso(4),
    null,
    null,
    null,
    JSON.stringify(["registered", "registered"]),
    null,
    null,
    null,
  );
  it.run(
    "sirius/rowan",
    "AMT-7",
    JSON.stringify(["api/routes/claims.ts#handleClaim"]),
    iso(6),
    null,
    null,
    "fail",
    JSON.stringify(["blocked"]),
    null,
    null,
    r3,
  );
  it.run(
    "sirius/birch",
    "AMT-13",
    JSON.stringify(["gate/select.ts#affected", "gate/select.ts#tier"]),
    iso(2),
    null,
    null,
    null,
    JSON.stringify(["registered", "forced"]),
    null,
    null,
    null,
  );

  // policy events (collision near-misses, oracle handling)
  const pe = db.prepare(
    "INSERT INTO policy_events(iteration_id,kind,detail,created_at) VALUES(?,?,?,?)",
  );
  pe.run(
    null,
    "backoff_409",
    JSON.stringify({ issue: "AMT-8", blocker: "sirius/cedar", base_ms: 500 }),
    iso(98),
  );
  pe.run(
    null,
    "oracle_202",
    JSON.stringify({ symbol: "db/pool.ts#acquire", action: "force", budget: 2000 }),
    iso(157),
  );
  pe.run(
    null,
    "backoff_409",
    JSON.stringify({ issue: "AMT-7", blocker: "sirius/oak" }),
    iso(6),
  );
  pe.run(
    null,
    "retry_budget",
    JSON.stringify({ issue: "AMT-2", attempts: 3, exhausted: true }),
    iso(120),
  );
  pe.run(
    null,
    "gate_tier",
    JSON.stringify({ issue: "AMT-6", tier: "safe", result: "fail" }),
    iso(138),
  );

  db.close();
  return dbPath;
}

if (import.meta.main) {
  const path = seed(target);
  // eslint-disable-next-line no-console
  console.log(`Seeded fixture ledger → ${path}`);
  console.log(`Run the console against it:`);
  console.log(`  SIRIUS_LEDGER="${path}" bun run src/server.ts`);
}
