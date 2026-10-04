# Sirius Forester — Build Contracts (v1)

This is the **single coordination artifact** for parallel implementation. Three agents
build against it concurrently. If any interface below must change, change it *here first*
and note it in your final report so the other agents can reconcile.

The product design and roadmap are maintained privately. This file only
pins the concrete interfaces where the three workstreams meet.

---

## 0. File ownership (collision avoidance — do NOT edit outside your tree)

| Agent | Owns (writes) | Reads only |
|---|---|---|
| **sirius-core** | `Cargo.toml`, `Cargo.lock`, `src/**`, `.sirius/` schema (created by `sirius init`) | `CONTRACTS.md` |
| **sirius-console** | `web/**` | `CONTRACTS.md`, the ledger schema below, `sirius --json` shapes below |
| **sirius-bench-docs** | `bench/**`, `.github/**`, `README.md`, `AGENTS.md`, `.claude/skills/sirius/**`, `docs/**` | everything |

Nobody but **sirius-core** touches `Cargo.toml` or `src/`. Nobody but **sirius-console**
touches `web/`. The top-level `.gitignore`, `LICENSE`, and
`CONTRACTS.md` already exist — do not overwrite them.

Commit your own work on a branch named `agent/<your-area>` (e.g. `agent/core`,
`agent/console`, `agent/bench-docs`) so merges are clean. Do not commit to a shared branch.

---

## 1. The ledger — `.sirius/sirius.db` (SQLite, WAL mode)

Sirius's ONLY write target. Created by `sirius init`. Read-only (WAL) by the Console.
`sirius init` sets `PRAGMA journal_mode=WAL` and `PRAGMA user_version` to the schema version.

```sql
-- schema_version = 1
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);  -- rows: schema_version, created_at, sirius_version

CREATE TABLE workers (
  id           TEXT PRIMARY KEY,          -- 'sirius/oak'
  created_at   TEXT NOT NULL,             -- ISO-8601 UTC
  last_seen_at TEXT,
  status       TEXT NOT NULL DEFAULT 'idle' -- idle|working|blocked|stopped
);

CREATE TABLE iterations (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  worker_id     TEXT NOT NULL REFERENCES workers(id),
  issue_ref     TEXT,                     -- 'AMT-7'
  entities      TEXT,                     -- JSON array of hayven entity ids
  started_at    TEXT NOT NULL,
  ended_at      TEXT,
  outcome       TEXT,                     -- completed|released|deadend|gate_failed|error
  gate_result   TEXT,                     -- pass|fail|skipped|null
  oracle_verdicts TEXT,                   -- JSON array (per-entity: registered|blocked|forced)
  tokens        INTEGER,                  -- nullable
  duration_ms   INTEGER,
  receipt_id    INTEGER REFERENCES receipts(id)
);

CREATE TABLE receipts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  kind          TEXT NOT NULL,            -- 'issue' | 'decision'
  ref           TEXT NOT NULL,            -- 'AMT-7' or 'D-3'
  symbols       TEXT NOT NULL,            -- JSON array of entity ids stamped
  forward_ok    INTEGER NOT NULL DEFAULT 0, -- amt comment landed (0/1)
  reverse_ok    INTEGER NOT NULL DEFAULT 0, -- hayven remember landed (0/1)
  created_at    TEXT NOT NULL,
  worker_id     TEXT
);

CREATE TABLE policy_events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  iteration_id INTEGER REFERENCES iterations(id),
  kind        TEXT NOT NULL,             -- claim_order|backoff_409|oracle_202|gate_tier|retry_budget|concurrency
  detail      TEXT,                      -- JSON
  created_at  TEXT NOT NULL
);
```

Additive table (SIRF-23) — created on `init` AND on every `Ledger::open`
(`CREATE TABLE IF NOT EXISTS`), so older ledgers gain it without a re-init and
older binaries simply never read it; `schema_version` stays 1:

```sql
CREATE TABLE review_rounds (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  iteration_id INTEGER REFERENCES iterations(id),
  issue_ref    TEXT NOT NULL,
  worker_id    TEXT,
  round        INTEGER NOT NULL,          -- 1-based review round
  result       TEXT NOT NULL,             -- clean|blocking|error|tampered|fell_back
  confirmed    INTEGER NOT NULL DEFAULT 0, -- blocking findings this round
  notes        INTEGER NOT NULL DEFAULT 0, -- non-blocking findings
  findings     TEXT,                      -- JSON {findings,notes} (or {error})
  created_at   TEXT NOT NULL
);
```

The Console reads these tables directly (read-only) for the fleet board and history views.
`data_version` for SSE polling = `PRAGMA data_version` on the ledger connection.

---

## 2. `sirius` CLI surface + `--json` output shapes

Every mutating command accepts `--json` and prints ONE JSON object to stdout (nothing else
on stdout; logs go to stderr). Exit codes: `0` ok, `1` operational failure, `2` usage error,
`3` gate/oracle "blocked" (soft), matching Hayvenhurst conventions.

```
sirius init                 -> {"ok":true,"ledger":".sirius/sirius.db","schema_version":1}
sirius doctor --json        -> {"ok":bool,"checks":[{"name":str,"pass":bool,"detail":str}, ...]}
                               # the five §6 contract facts: amt present+schema, hayven daemon,
                               # claim exit-code semantics, gate exit codes, fleet-memory write path

sirius link AMT-7 --symbols a,b,c [--changed [--range <git-range>]] --json
   # --changed resolves files → entities by PATH (hayven affected-tests roots);
   # the --json object also carries "changed_files": int|null
   -> {"ok":true,"receipt_id":12,"kind":"issue","ref":"AMT-7",
       "symbols":["a","b","c"],"forward_ok":true,"reverse_ok":true}
sirius link --decision D-3 --symbols ... --json    # same shape, kind:"decision"

sirius why <symbol> --json  -> {"symbol":str,"issues":[{"ref":"AMT-7","title":str}],
                                "decisions":[{"ref":"D-3","summary":str}]}
sirius why AMT-7 --json     -> {"ref":"AMT-7","symbols":[str],"decisions":[str],
                                "review":[{"round":int,"result":str,"confirmed":int,"notes":int,
                                           "worker":str,"at":str,"findings":{...}}]}

sirius gate AMT-7 [--tier safe] [--target-status in_review] [--range <git-range>] --json
   -> {"ok":bool,"issue":"AMT-7","tier":"safe","gate":"pass|fail",
       "plan":"subset(n)|full-suite|blocked|pass-with-warning|unconfigured",
       "ran_tests":bool,"advanced_to":"in_review"|null,
       "tests_selected":int,"comment_filed":bool}
   # Selects affected tests over the changed files, then RUNS them via
   # gate.test_cmd (full suite on any doubt); verdict = the runner's exit code.

sirius integrate [--clear-red] [--json]   # SIRF-32 — build the frontier, run integration.cmd on it
   -> {"ok":bool,"base_ref":str,"frontier":str,"included":["AMT-7"],
       "left_out":[{"issue":"AMT-9","files":[str]}],"complete":bool,
       "ran":bool,"exit":int|null,"timed_out":bool,"log":str|null,
       "issue":"AMT-12"|null,"red":bool}
   # frontier = base_ref tip + every sibling (§3.1) merged in order, in a throwaway
   # worktree; refs/sirius/frontier points at it. integration.cmd runs there via
   # `sh -c` with SIRIUS_INTEGRATION_DIR, SIRIUS_FRONTIER, SIRIUS_BASE_REF. A failure
   # records the red state in the ledger (meta `integration_red`) FIRST, then files
   # ONE issue (label integration; later failures comment on it while it is open or
   # unreadable). Siblings = every unmerged sirius/* branch whose issue is in
   # target_status, from ONE `amt issue list` (stale or orphaned branches are simply
   # not awaiting). Only a pass over a COMPLETE discovery (board and git answered)
   # clears red and comments on the issue; a pass over a transiently incomplete one
   # says so and stays red (the next run heals it). left_out siblings (textual
   # conflicts between siblings — the merge order's problem) do not keep it red. No
   # integration.cmd = build + report only — never green, red state untouched.
   # --clear-red: a human override — clears the red state without a green run,
   # comments BY HAND on the issue -> {"ok":true,"cleared":true,"issue":str|null}.
   # One integrate per repo (.sirius/integrate.lock, pid; a dead holder is taken over).
   # A timed-out command's whole process tree is killed.
   # Exit follows the LINE: 3 while red (failed, partial, or untested-while-red), 0
   # otherwise, 1 operational failure (incl. lock held).
   # Spine: integration.passed / .failed / .partial / .built (nothing ran) / .cleared.
   # Not (yet) implemented from SIRF-32's sketch: `integration.after_each`.

sirius run --workers N --agent-cmd "<cmd>" [--from todo] [--review-cmd "<cmd>"]
           [--model <id>|inherit] [--review-model <id>|inherit] [--allow-default-model] --json
   # first event: {"event":"fleet","phase":"start","models":{"default","source","review","fix_floor","routes","fallback"}}
   # SIRF-27: {"event":"fleet","phase":"fallback","worker","issue","reason","models":{"default","review"}} once,
   #   when a fleet stop on the primary tier switches the WHOLE fleet to models.fallback; the
   #   failed phase is retried in place on the fallback tier (review: a "fell_back" review event)
   # work/fix events carry "model" and "tier":"primary|fallback"
   # claim events carry "model" (this ticket's worker model) and "review_model"
   # exit 2 when no worker model resolves (unless --allow-default-model / models.allow_default);
   # exit 3 = refused to launch: integration red with integration.on_fail "block" (SIRF-32)
   # exit 4 = PAUSED: a fleet stop with nowhere to fall back — a usage/plan limit or an
   # unsupported model on the fallback tier (or with no fallback), or a logged-out CLI
   # ("Please run /login", never a fallback); judged on the failed run's LAST lines; every
   # worker stopped claiming/spawning, unworked issues stay in todo. Last event:
   # {"event":"fleet","phase":"paused","reason":str}; spine: fleet.paused (+ job.blocked)
   # streams NDJSON iteration events to stdout, one object per line:
   -> {"event":"iteration","worker":"sirius/oak","issue":"AMT-7","phase":"claim|map|lock|brief|work|gate|review|fix|receipt|release","...":...}
   # review (SIRF-23, only with review.cmd): {"phase":"review","round":N,"result":"clean|blocking|error|tampered|skipped","confirmed":K,"notes":M}
   # fix:  {"phase":"fix","round":N,"agent_ok":bool,...}  (then a re-gate, as today)
   # release gains "review":"review: 2 rounds, 4 bugs fixed, 1 rebuttal accepted" when a review ran
```

`--agent-cmd` and `--review-cmd` support `{issue}` / `{worker}` / `{model}` templating, and
every agent/reviewer process gets this environment (SIRF-22 #4/#5, SIRF-23):

| Var | Meaning |
|---|---|
| `SIRIUS_ISSUE`, `SIRIUS_WORKER`, `SIRIUS_WORKTREE` | identity + the private worktree |
| `AMT_AGENT` | `sirius/<tree>` — the agent's own `amt` writes are attributed to the worker |
| `SIRIUS_PHASE` | `work` \| `review` \| `fix` |
| `SIRIUS_BASE` | the launch base commit |
| `ANTHROPIC_MODEL`, `SIRIUS_MODEL` | (work, fix) this ticket's resolved worker model — Claude Code honors `ANTHROPIC_MODEL` over settings.json |
| `ANTHROPIC_MODEL`, `SIRIUS_REVIEW_MODEL` | (review) the reviewer's model |
| `SIRIUS_REVIEW_DIR`, `SIRIUS_DIFF_RANGE` | (review, fix) the tree to review, and `git diff $SIRIUS_DIFF_RANGE` |
| `SIRIUS_FRONTIER`, `SIRIUS_SIBLING_BRANCHES`, `SIRIUS_SIBLINGS` | (review) the frontier commit, the siblings merged into it, and their rendered summary (SIRF-30; empty / "(none)" outside `frontier`) |
| `SIRIUS_ROUND` | (review, fix) 1-based round |
| `SIRIUS_REVIEW_OUT`, `SIRIUS_REVIEW_PROMPT` | (review) where to write findings; the rendered prompt |
| `SIRIUS_REVIEW_FINDINGS` | (fix) this round's findings; (review, round > 1) the previous findings WITH the worker's responses |
| `SIRIUS_FIX_OUT` | (fix) where the worker writes its responses |

Sirius's own board writes (gate/review comments, decisions, forward stamps)
pass `--author sirius/<tree>`; releases already pass `--agent`.

Console mutations shell out to `sirius <cmd> --json` and parse these shapes. Do not invent
other stdout formats.

---

## 3. `.sirius/config.json` (Policy engine, M5) — committed defaults

```json
{
  "claim_order_enforced": true,
  "backoff_409": {"strategy": "release_and_comment", "base_ms": 500, "max_ms": 8000},
  "oracle_202": "back-off",                // "back-off" | "force-with-budget"
  "gate_tier": "safe",
  "target_status": "in_review",
  "retry_budget": 3,
  "worker_concurrency": 3,
  "claim_mode": "adaptive",                // "always" | "never" | "adaptive"
  "models": {                              // SIRF-26 — resolution: flag > config > $SIRIUS_PARENT_MODEL
    "default": null,                       // worker model when no route matches (--model)
    "routes": [],                          // [{"labels": ["security","auth"], "model": "<id>"}], first match wins
    "fix_floor": null,                     // fix rounds of UN-routed tickets use this model
    "review": null,                        // reviewer model (--review-model); default: models.default
    "allow_default": false,                // launch with no model at all (--allow-default-model)
    "fallback": null                       // SIRF-27: {"default": "<id>", "review": "<id>", ...} — the tier the
                                           // fleet switches to on a usage limit / unsupported model (one level)
  },
  "review": {                              // SIRF-23 — see §3.1
    "cmd": null,                           // or --review-cmd; null = stage off (pre-review loop exactly)
    "prompt_file": ".sirius/review-prompt.md", // an OVERRIDE; absent = the built-in default
    "max_rounds": 3,
    "block_on": ["bug", "conflict"],
    "against": "current-base-merge",       // | "launch-base" | "frontier" (SIRF-30)
    "base_ref": null,                      // default: the branch HEAD pointed to at launch
    "timeout_secs": 1500,
    "on_exhausted": "advance-flagged",     // | "release"
    "on_review_error": "advance-flagged",  // | "release"
    "skip_paths": ["**/*.md", "docs/**"],
    "sequences": []                        // SIRF-31: [{"dir": "drizzle", "key": "^(\\d+)"}] — off when empty
  },
  "integration": {                         // SIRF-32 — `sirius integrate`
    "cmd": null,                           // e.g. "./scripts/ci.sh --e2e-required"; null = build + report only
    "on_fail": "warn",                     // | "block": while red, `sirius run` refuses to start (exit 3),
                                           //   a running fleet stops claiming (paused, exit 4), and gated
                                           //   work is HELD: no review, no receipt, preserved on its
                                           //   sirius/<issue> branch AND refs/sirius/held/<issue>, released
                                           //   to todo (outcome released), and RESUMED on re-claim: merged
                                           //   onto the fresh worktree (repo identity + hooks), agent env
                                           //   SIRIUS_RESUMED_FROM=<sha>. The held ref is removed only once
                                           //   the resumed work is stamped; held work already on the base
                                           //   is dropped; work that no longer merges is parked at
                                           //   refs/sirius/held-conflicted/<issue> and the issue starts fresh
    "timeout_secs": 1800
  }
}
```

Absent file ⇒ these defaults. `sirius` reads it; Console displays it read-only.

### 3.1 The review stage (SIRF-23)

After WORK⇄GATE passes, and only in an isolated fleet worktree, Sirius runs:

1. **Checkpoint** — commit the worker's uncommitted work (`base..HEAD` is then exact).
2. **Review** — after renewing both leases (a refused amt lease aborts the
   iteration without releasing the issue), a FRESH `review.cmd` process
   (`sh -c 'cd "$SIRIUS_REVIEW_DIR" || exit 1; <cmd>'`)
   whose only inputs are the issue (via `amt`), the diff, and the repo. With
   `current-base-merge`, the review tree is a throwaway detached worktree with the
   checkpoint merged onto the CURRENT tip of `base_ref`; a merge conflict skips the
   reviewer and becomes blocking `conflict` findings. The tree is removed every round,
   and its `git worktree add/remove` are serialized across worker threads.
3. **Read-only enforcement** — the worker tree's HEAD + porcelain status + diff are
   fingerprinted before/after; any change is discarded (`reset --hard <checkpoint>`
   + `clean -fd`) and the round is `tampered` (a review error).
4. **Findings** — the reviewer's FINAL message (or `$SIRIUS_REVIEW_OUT`, if it can
   write files; the file wins when present). A headless reviewer typically has no
   write permission, so the last JSON object with a `findings` array in its
   captured output is the review:
   `{"findings":[{"id","kind":"bug|conflict|minor|design","confidence":"confirmed|uncertain","file","line","summary","scenario","fix"}],"previous":[{"id","verdict":"resolved|accepted|unresolved","note"}],"checked":[str]}`.
   Blocking = `kind ∈ block_on` AND `confidence == "confirmed"`; everything else is
   posted as notes. Missing/malformed JSON, a timeout, or tampering is a review
   error: retried ONCE, then `on_review_error`.
5. **Fix** — on any blocking finding (and rounds left), the worker re-runs in
   `SIRIUS_PHASE=fix` with `$SIRIUS_REVIEW_FINDINGS`, writes
   `{"responses":[{"id","status":"fixed|rebutted","note"}]}` to `$SIRIUS_FIX_OUT`, and
   is re-gated (gate failures go back to fixing within `retry_budget`). A fix
   agent failure or timeout, or a gate it cannot repair, REVERTS to the last
   reviewed checkpoint and escalates — gate-passing work is never traded for a
   broken fix, nor abandoned to a release.
6. **Re-review** — sees each previous finding next to its response; a finding is
   closed by a `resolved`/`accepted` verdict (or, with no verdict, by not being
   re-reported). An explicit `unresolved`, or re-reporting it under the same
   id, keeps it open. Reviewers must re-list an open finding under its
   ORIGINAL id (the default prompt says so).
7. **Escalation** — `advance-flagged`: advance anyway, label `review:open`, comment
   every unresolved confirmed finding. `release`: back to `todo` with the findings
   (ledger outcome `deadend`, deadend note filed). A later clean review clears a
   stale `review:open` label. An escalation where the review itself never
   completed reads "review: did not complete after N round(s)" in the receipt —
   never like a clean review. An empty diff, or one touching only `skip_paths`,
   skips the stage.

**Throwaway tree, always (SIRF-29).** The reviewer never runs in the worker's tree:
without a merge tree it gets a throwaway detached worktree at the checkpoint
(`.sirius/worktrees/<tree>-review`), removed every round. The read-only fingerprint
still covers the worker's tree, minus suite-owned paths (`.suite/`, `.hayven/`,
`.ametrite/`, `.sirius/`) whose writers are hooks and daemons, not the reviewer.

**Siblings and the integration frontier (SIRF-30).** A *sibling* is a local
`sirius/*` branch not merged into the current `base_ref` tip whose issue is in
`target_status` (awaiting integration), other than this issue's own branch — oldest
completion first, at most 12. With `against: "frontier"` the review tree is the
current base tip, then each sibling merged in order (a sibling that conflicts with
the ones before it is left out), then the checkpoint; the reviewer reviews
`$SIRIUS_DIFF_RANGE` = `<frontier>..HEAD`. A conflict is attributed by probing: if the
work conflicts with the bare base it is a blocking `conflict` (as with
`current-base-merge` — a base conflict blocks whatever `block_on` says); otherwise each
sibling that conflicts with the work on its own is a `sibling-conflict` finding
(confirmed; blocking only if listed in `block_on`) and is dropped from the retried
merge. Throwaway merges run no repo hooks (`--no-verify`, a null `core.hooksPath`) and
no rerere. Extra reviewer env:
`SIRIUS_FRONTIER` (the frontier commit), `SIRIUS_SIBLING_BRANCHES` (comma list of
merged siblings), `SIRIUS_SIBLINGS` (rendered summary of the oldest 12 merged — every sibling still counts
for sequence checks — with issue, title, branch, files,
overlap with this diff — also a prompt placeholder, appended to the prompt when a
custom template lacks it).

**Sequence collisions (SIRF-31).** For each `review.sequences` entry, the direct
children of `dir` whose name matches `key` (capture group 1; numeric when both parse)
form a sequence. Every entry this branch ADDS (vs the launch base) must have a key
greater than every key on the current base tip and must not share a key with an entry
a sibling adds — siblings here also include this fleet's peers under review right
now (no branch yet), in arrival order: the peer that reached review FIRST owns a
contested slot. A violation is a `conflict`/`confirmed` finding with a stable `AUTO-…`
id, recomputed every round — a fact, not an opinion: a rebuttal cannot close it (a
vanished one counts as fixed, never as an accepted rebuttal), regenerating the entry
does. If a round cannot recompute the facts completely (git or amt errors), the
previous round's `AUTO-` findings stay open. Entries compare by name AND content (a
same-named entry with different content collides). `key` must have a capture group
(checked when the config loads); `dir` may start with `./`. Computed for every `against` mode.

One issue comment per round and per fix (as `sirius/<tree>`), a `review_rounds`
ledger row per attempt, spine events `review.started` / `review.finding` /
`review.passed` / `review.flagged` / `review.tampered`, and the receipt's
decision title carries the summary ("Resolved AMT-7 via sirius · review: 2
rounds, 4 bugs fixed, 1 rebuttal accepted"). The issue's status changes once,
at release — a gate pass alone no longer advances it.

---

## 4. External parent CLIs (Sirius NEVER writes their DBs — §2.2)

All Ametrite writes via `amt ... --json`. All Hayvenhurst reads/writes via `hayven` CLI or
`http://localhost:7777`. Key calls (see PRD §9 for the full reference iteration):

```
amt claim --from todo --agent sirius/oak --json      # {claimed:bool, issue?, retry_after?}
amt issue update AMT-7 --status in_review --json
amt comment AMT-7 "<text>" --json
amt decide AMT-7 "<why>" --json                      # -> {decision:"D-n"}
amt release AMT-7 --json

hayven query "<terms>" --json
hayven impact <symbol> --json
hayven claim <symbol> --intent "AMT-7: ..." --agent sirius/oak   # exit 0 ok, 1 overlap, 3 oracle
hayven context <symbol> --json
hayven recall --node <id> --json
hayven remember --kind decision --node <id> --scope <ids> "<text>"
hayven affected-tests --changed <files> --json  # SELECTS tests (exit 0 = selected, NOT passed); sirius runs them
hayven release <claimId>
```

Verify exact flag names against the installed CLIs (`amt --help`, `hayven --help`,
`hayven <sub> --help`) at build time — the PRD's forms are the intent; the installed binaries
(`amt 0.1.0`, `hayven 0.0.5`) are ground truth. If a flag differs, adapt and note it here.

---

## 5. Build / test conventions

- Rust: `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
  Deps limited to the Ametrite set: `rusqlite` (bundled feature), `serde`, `serde_json`,
  `clap` (derive), `regex`. No others without a note here.
- Console: Bun only, ZERO npm runtime deps. `bun test`. Vanilla TS.
- Bench: harnesses under `bench/`, runnable with `bun run bench/<name>.ts`, each emits a
  measured number tied to a PRD §8 metric.
- CI (`.github/workflows/ci.yml`): cargo test/clippy/fmt + web bundle check + bench smoke,
  matrix macOS/Ubuntu/Windows (mirror the Ametrite workflow).

---

## 6. Ground-truth CLI deltas

Verified against the installed `amt 0.1.0` and `hayven 0.0.5` at build time. Where a
real flag differs from §4's intent, `sirius-core` adapted its code to the form below.
These deltas are what the binary actually shells; the Console must mirror them if it
ever shells the parents directly (it should only shell `sirius --json`).

### Ametrite (`amt 0.1.0`)

- **`--json` is a global flag before the subcommand** (`amt --json claim ...`), not a
  per-subcommand suffix. (It is also accepted after, but Sirius emits it globally.)
- **Comments:** `amt issue comment <ID> -m/--body <text>` (returns `{"ok":true}`), NOT
  the §4 `amt comment <ID> "<text>"`.
- **Status updates:** `amt issue update <ID> --status <s>` (returns the updated issue
  object), NOT `amt issue update <ID> --status`.
- **`amt claim` output is NOT `{claimed:bool, issue?}`.** On success it returns the full
  issue object (`{"id":"AMT-7","title":...,"activity":[...]}`). On no-work it returns
  `{"claimed":false,"retry_after":N,"counts":{...},"reason":"..."}`. Sirius keys success
  off the presence of `id`, and no-work off `claimed:false`. `--peek` gives the no-work
  shape without taking a lease (used by `sirius doctor`).
- **`amt claim` flags:** `--from` accepts a comma-list/repeat of `backlog,todo`; `--ttl`
  (default 900), `--cooldown` (default 3600), `--agent`, `--issue <id>` (specific claim /
  heartbeat), `--peek`, `--all-workspaces` all present as in §4's intent.
- **`amt release <ID>`** requires `--agent` matching the claimant; takes `--status`
  (default `in_review`) and `-m/--comment`.
- **`amt decide` is `amt decide --issue <ID> --title <T> [-b <body>]`** (title required),
  returns `{"id":"D-n","resolves":"AMT-7",...}`, NOT `amt decide AMT-7 "<why>"`.
- **Ametrite schema version lives in the `meta` table** (`SELECT value FROM meta WHERE
  key='schema_version'`), NOT in `PRAGMA user_version` (which reads 0). Observed value on
  a freshly-`amt init`ed workspace is **v4** (≥ the PRD's "v3" floor). `sirius doctor`
  reads this read-only and checks `>= 3` pragmatically — a hardcoded `== 3` compare would
  already be wrong.

### Hayvenhurst (`hayven 0.0.5`)

- **No per-subcommand `--help`:** every `hayven <sub> --help` prints the same global help.
- **`hayven claim <ids...> --intent "..." [--force]`** — ids are **positional** (multiple
  allowed), there is **NO `--agent` flag** (agent is derived by the daemon) and **NO
  `--node`/`--scope`** on claim. Exit codes as in §4: **0 registered, 1 hard overlap
  (409), 3 oracle adjacency (202)**.
- **`hayven remember "<note>" [--node <id>] [--kind K] [--scope a,b] [--ttl S]`** — the
  note is the **first positional arg**; `--scope` is a comma list. Returns
  `{"id":"mem_...","nodeId":...,"kind":...,"scope":[...]}`. There is no `--agent` (agent
  is null in the record). This is the reverse-provenance write path (PRD §6 fact 3).
- **`hayven recall [<term>] [--node <id>] [--kind K] [--json]`** returns
  `{"count":N,"notes":[{...}]}`.
- **`hayven affected-tests` is a test SELECTOR, not a runner** (SIRF-5 / D-3). Its exit
  code means "selection computed," **never** "the tests pass" — `affected-tests --changed
  <files> --json` returns exit 0 with `{"roots":[...],"note":...,"tests":[...]}` even when
  `tests` is empty, and there are no `--gate` / `--gate-tier` flags in 0.0.5. **Sirius owns
  the run-the-tests half itself.** The gate (`src/gate.rs`): (1) resolves changed files from
  a git range, (2) calls `hayven affected-tests --changed <csv> --json` to *select*, (3)
  trusts a narrow selection **only** when the command succeeded, every changed file mapped
  (`roots >= changed files` — `roots > 0` alone blessed partial mappings), there are
  runnable ids (none dash-leading — a flag-like id would reach the test runner as a flag),
  the `note` raises no under-report/stale flag, and no global-impact file
  (Cargo.toml, package.json, `.github/`, …) changed — otherwise it **falls back to the full
  suite**, (4) runs the chosen tests via the configurable **`gate.test_cmd`** (e.g.
  `cargo test`), and (5) takes the verdict from the **test runner's** exit code. The
  governing rule is *"ran too much, never missed a test."* `gate.fallback` (`full-suite`
  default | `fail` | `pass-with-warning`) governs behavior under doubt; with no `test_cmd`
  the gate is **fail-closed** (refuses to pass). The requested tier is recorded in the
  ledger/`--json` output; `--gate-tier` is wired through only when a future hayven exposes
  it. This mirrors the ported reference recipe `ci/hayven-affected-tests.sh` in public
  Hayvenhurst (SAFE tier: 0 misses across ~62 replayed bugs).
- **Daemon is single-project-bound.** The daemon on `:7777` serves ONE project; a
  read/write against a workspace whose daemon is not the one on `:7777` fails with exit 1
  and `"daemon at ... serves a DIFFERENT project — refusing to mutate it"`. Consequence:
  `sirius` hayven calls only succeed when the `:7777` daemon matches the current workspace
  (start it with `hayven daemon start` in the repo). `sirius doctor`'s daemon check probes
  `GET http://localhost:7777/` for a 200 and reports the `hayven daemon status` line;
  when the workspace mismatches, forward stamping (`amt`) still lands while reverse
  stamping (`hayven remember`) reports `reverse_ok:false` — verified live.
- **`hayven --version` prints just `0.0.5`** (no `hayven ` prefix), unlike `amt --version`
  which prints `amt 0.1.0`.
