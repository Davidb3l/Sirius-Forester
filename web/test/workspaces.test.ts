import { test, expect, beforeAll, afterAll } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { seed } from "../fixtures/seed.ts";
import { buildDeps, handleFleets, type ServerDeps } from "../src/server.ts";
import type { Workspace } from "../src/db.ts";
import { fleetRunning, readRegistry, WorkspaceSet } from "../src/workspaces.ts";
import { rmTempDir } from "./helpers.ts";

// One console, every fleet (the Ametrite switcher pattern): repos come from
// the Ametrite registry (those with a Sirius ledger) plus the launch repo.
let dir: string;
let registry: Record<string, string>;
let set: WorkspaceSet<ServerDeps>;
const built: ServerDeps[] = [];

const repo = (name: string, ledger = true) => {
  const root = join(dir, name);
  mkdirSync(root, { recursive: true });
  if (ledger) seed(join(root, ".sirius", "sirius.db"));
  return root;
};
const home = (root: string): Workspace => ({
  root,
  ledgerPath: join(root, ".sirius", "sirius.db"),
  configPath: join(root, ".sirius", "config.json"),
  ametritePath: null,
  hayvenDir: null,
});
const get = (path: string) => handleFleets(new Request(`http://127.0.0.1${path}`), set);

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "sirius-fleets-"));
  const forester = repo("Sirius Forester");
  const lydgr = repo("Lydgr");
  repo("MainSpanX");
  const noSirius = repo("Board Only", false); // an Ametrite-only repo
  // Lydgr's fleet is running (this test process stands in for it).
  writeFileSync(join(lydgr, ".sirius", "run.pid"), String(process.pid));
  registry = {
    lydgr: lydgr,
    mainspanx: join(dir, "MainSpanX"),
    "board-only": noSirius,
    "forester-again": forester, // the launch repo, registered under another alias
  };
  set = new WorkspaceSet<ServerDeps>(
    (ws) => {
      const d = buildDeps(ws);
      built.push(d);
      return d;
    },
    () => registry,
    home(forester),
  );
});
afterAll(() => {
  for (const d of built) {
    d.ledger.close();
    d.stores.close();
  }
  rmTempDir(dir);
});

test("lists every repo with a Sirius ledger — running first — and the launch repo once", async () => {
  const r = await get("/api/workspaces");
  expect(r.status).toBe(200);
  const d = (await r.json()) as {
    default: string;
    workspaces: { alias: string; name: string; running: boolean; ledgerAvailable: boolean }[];
  };
  // The launch repo keeps the alias the Ametrite registry gives it.
  expect(d.default).toBe("forester-again");
  const names = d.workspaces.map((w) => w.name);
  expect(names).toEqual(["Lydgr", "MainSpanX", "Sirius Forester"]);
  expect(d.workspaces[0]!.running).toBe(true);
  expect(d.workspaces.filter((w) => w.running)).toHaveLength(1);
  expect(names).not.toContain("Board Only");
  expect(d.workspaces.every((w) => w.ledgerAvailable)).toBe(true);
});

test("?ws= serves that fleet's ledger; no ws serves the launch repo", async () => {
  const real = (p: string) => realpathSync(p);
  const lydgr = (await (await get("/api/health?ws=lydgr")).json()) as { workspace: string };
  expect(real(lydgr.workspace)).toBe(real(join(dir, "Lydgr")));
  const home = (await (await get("/api/health")).json()) as { workspace: string };
  expect(real(home.workspace)).toBe(real(join(dir, "Sirius Forester")));
  expect((await get("/api/fleet?ws=mainspanx")).status).toBe(200);
});

test("an unknown fleet is a 404 — never another repo's data", async () => {
  const r = await get("/api/fleet?ws=nope");
  expect(r.status).toBe(404);
  expect(((await r.json()) as { error: string }).error).toContain("nope");
  expect((await get("/events?ws=nope")).status).toBe(404);
  // Static assets ignore ws (the page itself must load to switch).
  expect((await get("/?ws=nope")).status).toBe(200);
});

test("a repo that gains a ledger later appears on the next list", async () => {
  registry["late"] = repo("Late Repo", false);
  let d = (await (await get("/api/workspaces")).json()) as { workspaces: { name: string }[] };
  expect(d.workspaces.map((w) => w.name)).not.toContain("Late Repo");
  seed(join(dir, "Late Repo", ".sirius", "sirius.db"));
  d = (await (await get("/api/workspaces")).json()) as { workspaces: { name: string }[] };
  expect(d.workspaces.map((w) => w.name)).toContain("Late Repo");
});

test("fleetRunning: a live pid is running; a dead or missing pidfile is not", () => {
  const root = repo("Pid Check");
  expect(fleetRunning(root)).toBe(false);
  writeFileSync(join(root, ".sirius", "run.pid"), String(process.pid));
  expect(fleetRunning(root)).toBe(true);
  writeFileSync(join(root, ".sirius", "run.pid"), "999999");
  expect(fleetRunning(root)).toBe(false);
  writeFileSync(join(root, ".sirius", "run.pid"), "garbage");
  expect(fleetRunning(root)).toBe(false);
});

// ---- review fixes -----------------------------------------------------------

const fresh = (reg: Record<string, string>, homeRoot: string) =>
  new WorkspaceSet<{ closed: boolean }>(
    () => ({ closed: false }),
    () => reg,
    home(homeRoot),
    (b) => {
      b.closed = true;
    },
  );

test("one repo reached through a symlink is ONE fleet", () => {
  const root = repo("Linked");
  const link = join(dir, "Linked-link");
  symlinkSync(root, link);
  const s = fresh({ a: root, b: link }, repo("Home1"));
  expect(s.list().filter((e) => e.name === "Linked")).toHaveLength(1);
});

test("a deleted repo leaves the list and its readers are closed", () => {
  const gone = repo("Doomed");
  const reg = { doomed: gone };
  const s = fresh(reg, repo("Home2"));
  const b = s.get("doomed")!;
  expect(b.closed).toBe(false);
  rmTempDir(join(gone, ".sirius"));
  s.refresh();
  expect(s.has("doomed")).toBe(false);
  expect(b.closed).toBe(true);
});

test("listing opens nothing it keeps: no fleet readers are built", async () => {
  const s = fresh({ lydgr: join(dir, "Lydgr"), mainspanx: join(dir, "MainSpanX") }, repo("Home3"));
  const res = await handleFleets(
    new Request("http://127.0.0.1/api/workspaces"),
    s as unknown as WorkspaceSet<ServerDeps>,
  );
  expect(res.status).toBe(200);
  expect(s.builtCount).toBe(0);
});

test("reads are refused for a non-loopback Host (DNS rebinding)", async () => {
  const r = await handleFleets(new Request("http://evil.example/api/workspaces"), set);
  expect(r.status).toBe(403);
  expect((await handleFleets(new Request("http://evil.example/api/fleet?ws=lydgr"), set)).status).toBe(403);
});

test("a malformed registry entry is skipped, never fatal", () => {
  const p = join(dir, "registry.json");
  writeFileSync(p, JSON.stringify({ workspaces: { good: "/x", bad: null, worse: 7 } }));
  expect(readRegistry(p)).toEqual({ good: "/x" });
});

