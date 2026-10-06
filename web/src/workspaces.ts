// Every Sirius fleet on this machine, behind one console (the Ametrite board
// pattern: one server, a workspace switcher, `?ws=<alias>` on every request).
//
// Discovery needs no new state: every Sirius repo has an Ametrite board beside
// it, so the Ametrite registry (AMT_REGISTRY or ~/.ametrite/registry.json)
// already lists them — we keep the ones with a `.sirius/sirius.db`. The repo
// the console was launched in (or SIRIUS_LEDGER) is always included and is the
// default. Listing is cheap and holds nothing open; a fleet's full readers
// (ledger, parent store, poller) are built only when it is actually viewed,
// and dropped when its repo disappears.

import { existsSync, readFileSync, realpathSync } from "node:fs";
import { homedir } from "node:os";
import { basename, join, resolve } from "node:path";
import { discoverWorkspace, type Workspace } from "./db.ts";
import { Ledger } from "./ledger.ts";

export interface WorkspaceEntry {
  alias: string;
  name: string;
  root: string;
}

/** What `/api/workspaces` reports for one fleet. */
export interface WorkspaceStatus {
  alias: string;
  name: string;
  root: string;
  /** A `sirius run` for this repo is alive right now (`.sirius/run.pid`). */
  running: boolean;
  /** Workers whose last recorded status is `working`. */
  working: number;
  ledgerAvailable: boolean;
}

export function slug(s: string): string {
  return (
    s
      .normalize("NFKD")
      .replace(/[̀-ͯ]/g, "")
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-|-$/g, "") || "workspace"
  );
}

/** One identity per repo: symlinks, case variants on case-insensitive disks,
 *  `/tmp` vs `/private/tmp` all collapse to the real path. */
export function canonical(root: string): string {
  try {
    return realpathSync.native(root);
  } catch {
    return resolve(root);
  }
}

const ledgerOf = (root: string) => join(root, ".sirius", "sirius.db");

function workspaceAt(root: string): Workspace {
  return {
    root,
    ledgerPath: ledgerOf(root),
    configPath: join(root, ".sirius", "config.json"),
    ametritePath: existsSync(join(root, ".ametrite", "ametrite.db"))
      ? join(root, ".ametrite", "ametrite.db")
      : null,
    hayvenDir: existsSync(join(root, ".hayven")) ? join(root, ".hayven") : null,
  };
}

/** The Ametrite registry's `alias → root` map (empty if absent/unreadable;
 *  non-string roots are skipped, never fatal). */
export function readRegistry(
  path = process.env.AMT_REGISTRY ?? join(homedir(), ".ametrite", "registry.json"),
): Record<string, string> {
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as {
      workspaces?: Record<string, unknown>;
    };
    return Object.fromEntries(
      Object.entries(parsed.workspaces ?? {}).filter(
        (kv): kv is [string, string] => typeof kv[1] === "string",
      ),
    );
  } catch {
    return {};
  }
}

/** Is the process in `.sirius/run.pid` alive? (A stale pidfile is not.) */
export function fleetRunning(root: string): boolean {
  try {
    const pid = Number(readFileSync(join(root, ".sirius", "run.pid"), "utf8").trim());
    if (!Number.isInteger(pid) || pid <= 0) return false;
    process.kill(pid, 0); // signal 0: existence check only
    return true;
  } catch (e) {
    // EPERM = alive but not ours to signal — still running.
    return (e as NodeJS.ErrnoException)?.code === "EPERM";
  }
}

/** Workers marked `working` — a short-lived read, nothing kept open. */
export function workingCount(root: string): number {
  if (!existsSync(ledgerOf(root))) return 0;
  const l = new Ledger(ledgerOf(root));
  try {
    return l.workers().filter((w) => w.status === "working").length;
  } catch {
    return 0; // an unreadable ledger reads as no workers, never an error
  } finally {
    l.close();
  }
}

/**
 * The set of fleets, rediscovered on demand: a repo whose first `sirius init`
 * happened after the console started appears on the next refresh; a repo that
 * was deleted (or left the registry) disappears and its readers are closed.
 * `T` is whatever per-fleet bundle the server builds (its readers).
 */
export class WorkspaceSet<T> {
  private entries = new Map<string, WorkspaceEntry>();
  private built = new Map<string, T>();
  /** The launch repo (cwd or SIRIUS_LEDGER) — built from its own paths. */
  private readonly homeWorkspace: Workspace;
  readonly defaultAlias: string;

  constructor(
    private readonly build: (ws: Workspace) => T,
    private readonly registry: () => Record<string, string> = () => readRegistry(),
    home: Workspace = discoverWorkspace(),
    private readonly dispose: (t: T) => void = () => {},
  ) {
    this.homeWorkspace = home;
    // The launch repo keeps the alias Ametrite already uses for it.
    const homeRoot = canonical(home.root);
    const registered = Object.entries(this.registry()).find(
      ([, root]) => canonical(root) === homeRoot,
    )?.[0];
    this.defaultAlias = this.add(home.root, true, registered);
    this.refresh();
  }

  /** Track `root` (deduped by real path); returns its alias. */
  private add(root: string, force = false, alias?: string): string {
    const r = canonical(root);
    for (const e of this.entries.values()) if (e.root === r) return e.alias;
    if (!force && !existsSync(ledgerOf(r))) return "";
    const base = slug(alias ?? basename(r));
    let a = base;
    for (let n = 2; this.entries.has(a); n++) a = `${base}-${n}`;
    this.entries.set(a, { alias: a, name: basename(r), root: r });
    return a;
  }

  private remove(alias: string): void {
    const b = this.built.get(alias);
    if (b) this.dispose(b);
    this.built.delete(alias);
    this.entries.delete(alias);
  }

  /** Pick up new repos; drop ones that are gone (never the launch repo). */
  refresh(): void {
    const reg = this.registry();
    for (const [alias, root] of Object.entries(reg)) this.add(root, false, alias);
    const registered = new Set(Object.values(reg).map(canonical));
    for (const e of [...this.entries.values()]) {
      if (e.alias === this.defaultAlias) continue;
      if (!registered.has(e.root) || !existsSync(ledgerOf(e.root))) this.remove(e.alias);
    }
  }

  list(): WorkspaceEntry[] {
    return [...this.entries.values()];
  }

  /** The listing `/api/workspaces` serves — opens nothing it keeps. */
  status(): WorkspaceStatus[] {
    return this.list()
      .map((e) => ({
        alias: e.alias,
        name: e.name,
        root: e.root,
        running: fleetRunning(e.root),
        working: workingCount(e.root),
        ledgerAvailable: existsSync(
          e.alias === this.defaultAlias ? this.homeWorkspace.ledgerPath : ledgerOf(e.root),
        ),
      }))
      .sort((a, b) => Number(b.running) - Number(a.running) || a.name.localeCompare(b.name));
  }

  has(alias: string): boolean {
    return this.entries.has(alias);
  }

  /** How many fleets have full readers built (tests: listing builds none). */
  get builtCount(): number {
    return this.built.size;
  }

  workspace(alias: string): Workspace | null {
    const e = this.entries.get(alias);
    if (!e) return null;
    return alias === this.defaultAlias ? this.homeWorkspace : workspaceAt(e.root);
  }

  /** The per-fleet bundle, built on first use. `null` if unknown. */
  get(alias: string | null | undefined): T | null {
    const a = alias || this.defaultAlias;
    const cached = this.built.get(a);
    if (cached) return cached;
    const ws = this.workspace(a);
    if (!ws) return null;
    const b = this.build(ws);
    this.built.set(a, b);
    return b;
  }
}
