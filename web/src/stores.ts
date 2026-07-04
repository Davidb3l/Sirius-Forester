// Read-only, best-effort enrichment from the parent stores (PRD §2.2).
// CONTRACTS.md pins the LEDGER schema but NOT the internal Ametrite/Hayvenhurst
// SQLite schemas — those are owned by the parents and treated as opaque. So we
// probe defensively: if a recognizable issues table exists we surface titles,
// otherwise we degrade to just the ref. The authoritative source for issue and
// symbol detail at runtime is `sirius why --json` (see sirius.ts); this reader
// is only a convenience for the board so it never hard-depends on parent shape.
import type { Database } from "bun:sqlite";
import { openReadOnly } from "./db.ts";

export class ParentStores {
  private amt: Database | null;
  private amtIssuesTable: string | null = null;
  private amtCols: { ref: string; title: string; status?: string } | null =
    null;

  constructor(ametritePath: string | null) {
    this.amt = ametritePath ? openReadOnly(ametritePath) : null;
    if (this.amt) this.probeAmetrite();
  }

  get ametriteAvailable(): boolean {
    return this.amt !== null;
  }

  private probeAmetrite(): void {
    if (!this.amt) return;
    try {
      const tables = this.amt
        .query(
          "SELECT name FROM sqlite_master WHERE type='table';",
        )
        .all() as { name: string }[];
      const names = new Set(tables.map((t) => t.name.toLowerCase()));
      const candidate = ["issues", "issue", "tickets"].find((n) =>
        names.has(n),
      );
      if (!candidate) return;
      const cols = this.amt
        .query(`PRAGMA table_info(${candidate});`)
        .all() as { name: string }[];
      const colset = new Set(cols.map((c) => c.name.toLowerCase()));
      const refCol = ["ref", "key", "slug", "id"].find((c) => colset.has(c));
      const titleCol = ["title", "name", "summary"].find((c) =>
        colset.has(c),
      );
      if (!refCol || !titleCol) return;
      this.amtIssuesTable = candidate;
      this.amtCols = {
        ref: refCol,
        title: titleCol,
        status: ["status", "state"].find((c) => colset.has(c)),
      };
    } catch {
      // opaque/unknown parent shape — degrade silently
      this.amtIssuesTable = null;
    }
  }

  /** Best-effort issue title for an Ametrite ref; null if unknown/unavailable. */
  issueTitle(ref: string | null | undefined): string | null {
    if (!ref || !this.amt || !this.amtIssuesTable || !this.amtCols) return null;
    try {
      const row = this.amt
        .query(
          `SELECT ${this.amtCols.title} AS title FROM ${this.amtIssuesTable}
           WHERE ${this.amtCols.ref} = ? LIMIT 1;`,
        )
        .get(ref) as { title: string } | null;
      return row?.title ?? null;
    } catch {
      return null;
    }
  }

  close(): void {
    this.amt?.close();
    this.amt = null;
  }
}
