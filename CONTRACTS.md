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

-- SIRF-35: defects that got past review, and what they taught it.
CREATE TABLE escapes (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  issue_ref    TEXT NOT NULL,             -- the issue whose work introduced the defect
  kind         TEXT NOT NULL,             -- slug, e.g. 'migration-fork'
  summary      TEXT NOT NULL,             -- what escaped
  found_by     TEXT,                      -- main-session|integration|human|e2e|<free>
  fix_commit   TEXT,                      -- the commit that fixed it (a canary source)
  created_at   TEXT NOT NULL
);
CREATE TABLE escape_kinds_automated (     -- a kind now caught by a real check
  kind         TEXT PRIMARY KEY,
  automated_by TEXT NOT NULL,             -- the test/check path
  created_at   TEXT NOT NULL
);
CREATE TABLE canary_runs (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  model           TEXT,                   -- the reviewer model scored
  total           INTEGER NOT NULL,       -- canaries scored: reviewed + reviewer errors (stale excluded)
  caught          INTEGER NOT NULL,
  false_positives INTEGER NOT NULL,       -- confirmed blocking findings on the control (0 if it did
                                          --   not run; detail.control_ran says which)
  detail          TEXT NOT NULL,          -- JSON: per-canary results
  created_at      TEXT NOT NULL
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
                               # + gate_configured (GATING: fails while gate.test_cmd is null — SF-11).
                               # hayven_daemon_7777 compares the daemon's BUILD (/api/health "version")
                               # to the CLI's and fails on skew (SF-13). Advisory checks (plugin_handoff,
                               # fleet_models, integration, plugin_version, agent_timeouts) never affect "ok".

sirius link AMT-7 --symbols a,b,c [--changed [--range <git-range>]] --json
   # --changed resolves files → entities by PATH (hayven affected-tests roots);
   # with no --range and a CLEAN tree it diffs HEAD~1..HEAD — the commit just made,
   # so the documented order (work → gate → commit → receipt) files a receipt (SF-12);
   # an explicit --range is never second-guessed.
   # the --json object also carries "changed_files": int|null
   -> {"ok":true,"receipt_id":12,"kind":"issue","ref":"AMT-7",
       "symbols":["a","b","c"],"forward_ok":true,"reverse_ok":true}
sirius link --decision D-3 --symbols ... --json    # same shape, kind:"decision"

sirius why <symbol> --json  -> {"symbol":str,"issues":[{"ref":"AMT-7","title":str}],
                                "decisions":[{"ref":"D-3","summary":str}]}
sirius why AMT-7 --json     -> {"ref":"AMT-7","symbols":[str],"decisions":[str],
                                "review":[{"round":int,"result":str,"confirmed":int,"notes":int,
                                           "worker":str,"at":str,"findings":{...}}],
                                "escapes":[{"id":int,"kind":str,"summary":str,"found_by":str|null,
                                            "fix":str|null,"at":str}]}

sirius gate AMT-7 [--tier safe] [--target-status in_review] [--range <git-range>] --json
   -> {"ok":bool,"issue":"AMT-7","tier":"safe","gate":"pass|fail",
       "plan":"subset(n)|full-suite|blocked|pass-with-warning|unconfigured",
       "reason_code":"pass|tests_failed|blocked_by_policy|passed_without_tests|unconfigured_test_cmd|shell_spawn_failed",
       "structural":bool,
       "ran_tests":bool,"advanced_to":"in_review"|null,
       "tests_selected":int,"comment_filed":bool}
   # Selects affected tests over the changed files, then RUNS them via
   # gate.test_cmd (full suite on any doubt); verdict = the runner's exit code.
   # Exit 3 covers EVERY non-pass, including an unconfigured workspace; "reason_code"
   # tells them apart (SF-11), "structural":true = no retry can change the verdict.
   # gate.test_cmd runs through an EXPLICIT shell, never the launcher's (SF-15):
   # $SIRIUS_SHELL if set; else /bin/sh -c on unix; else the first sh.exe on PATH;
   # else %ComSpec% /C.
   # SIRF-48: a fail comment carries the runner's output tail (last 15 non-empty lines,
   # ≤2KB, fenced); the ledger gate_tier fail event gains "error_tail":str and
   # "env_fault":bool (the output looks like a missing dependency — SIRF-50).

sirius escape <ISSUE> --kind <slug> -m "<what escaped>" [--found-by <who>] [--fix <commit>] [--json]
   -> {"ok":true,"id":int,"issue":str,"kind":str,"kind_count":int,"unretired":str|null,"nudge":str|null}
   # SIRF-35: record a defect that got past review, against the issue that introduced it
   # (the issue must exist; its canonical key is stored; a comment there, as `sirius`);
   # --fix (resolved to a full sha now) makes it a canary. Empty -m: exit 2. A kind seen
   # twice (not yet automated) carries a nudge to encode it as a check; a kind that was
   # automated and escapes AGAIN is un-retired ("unretired": the check that missed it).
   # Summaries enter prompts as one line, capped at 200 chars, `$` neutralized.
   # Spine: escape.recorded.
sirius escape --kind <slug> --automated-by <test path> [--json]
   -> {"ok":true,"kind":str,"automated_by":str,"escapes":int}
   # retires the kind from the review prompt — a real check now catches it (the path
   # must exist in the repo, else exit 2)
sirius escape --list [--json]
   -> {"kinds":[{"kind","count","last_at","automated_by"|null}],
       "escapes":[{"id","issue","kind","summary","found_by","fix","at"}]}

sirius review-canary [--n 10] [--json]
   -> {"ok":true,"base":str,"model":str|null,"total":int,"caught":int,"stale":int,
       "errors":int,"recall":float|null,"false_positives":int|null,
       "canaries":[{"source":"escape:<id>"|"patch:<file>"|"control:built-in"|"control:<file>",
                    "issue":str|null,"kind":str|null,
                    "result":"caught|missed|stale|error" (control: "clean|flagged|stale|error"),
                    "detail":str}]}
   # on failure with --json: {"ok":false,"error":str} (exit 1); --n 0: exit 2
   # Replays up to N canaries — each escape with a fix commit (its fix REVERTED onto the
   # current base, i.e. the real bug back), plus .sirius/canaries/*.patch — through the
   # configured review.cmd exactly as a review round runs it, in a throwaway worktree.
   # caught = a blocking finding on a NON-TEST file the canary changed (reverting a fix
   # also removes its regression test — that is not catching the bug). A canary that no
   # longer applies is "stale" (not scored); a reviewer error is scored as a MISS:
   # recall = caught / (total + errors). Escapes are deduplicated by fix commit; a fix
   # that landed as a merge reverts against its mainline. One CONTROL (a trailing
   # newline on the first reviewable tracked text file, or .sirius/canaries/control.patch)
   # measures false positives (null if it did not run). A canary is BLIND on the channels
   # Sirius controls: issue key CANARY-<n>, worker sirius/reviewer, commit message "wip",
   # a parentless base commit (no fix history in `git log`), and no escape sharing its
   # kind or fix commit in its prompt; its prompt/output/log are deleted after scoring,
   # and trees/files of killed runs are swept. Writes a canary_runs row.
   # Exit 0 ran, 1 operational failure (incl. no review.cmd).
   # Scored per reviewer model; per-lens recall arrives with lenses (SIRF-38). There is
   # no built-in mutation catalog: mutations are language-specific — write them as
   # .sirius/canaries/*.patch (git apply format, against the base).

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
   # first event: {"event":"fleet","phase":"start","models":{"default","source","review","fix_floor","routes","fallback"},
   #   "workers":N,"workers_source":"flag|default","workers_why":str,
   #   "setup":{"cmd":str,"detected_from":str|null,"failed":[worker]}|null}   (SIRF-50, additive)
   # --workers N is the count — worker_concurrency no longer caps it (SIRF-50; it used to,
   # silently); no flag ⇒ 1 worker, as always. stderr says "workers: N (<why>)".
   # SIRF-50: after creating each worktree, worktree.setup_cmd (unset ⇒ detected from the
   # lockfile, "" ⇒ none) runs in it through the gate's shell, serially, before any agent.
   # A worker whose setup fails does not start: {"event":"fleet","phase":"setup_failed",
   # "worker","cmd","error"} right after the start event, and the run exits 1 at the end
   # (the other workers still run); if EVERY setup fails, only those lines are printed and
   # run exits 1 at once.
   # SIRF-27: {"event":"fleet","phase":"fallback","worker","issue","reason","models":{"default","review"}} once,
   #   when a fleet stop on the primary tier switches the WHOLE fleet to models.fallback; the
   #   failed phase is retried in place on the fallback tier (review: a "fell_back" review event)
   # work/fix events carry "model" and "tier":"primary|fallback"
   # claim events carry "model" (this ticket's worker model) and "review_model"
   # exit 2 when no worker model resolves (unless --allow-default-model / models.allow_default);
   # exit 2 also when --agent-cmd's program is not on PATH (SF-14) — a command too dynamic to
   # read statically ($VAR, $(…), {model}, a builtin) is let through rather than refused.
   # --agent-cmd runs through the same explicit shell as gate.test_cmd (SF-15); the reviewer
   # stays on `sh -c` (its script is sirius-built POSIX). --from omitted = todo AND backlog;
   # a run under --from that claimed nothing names the parked work on stderr.
   # exit 3 = refused to launch: integration red with integration.on_fail "block" (SIRF-32)
   # exit 4 = PAUSED: a fleet stop with nowhere to fall back — a usage/plan limit or an
   # unsupported model on the fallback tier (or with no fallback), or a logged-out CLI
   # ("Please run /login", never a fallback); judged on the failed run's LAST lines; every
   # worker stopped claiming/spawning, unworked issues stay in todo. Last event:
   # {"event":"fleet","phase":"paused","reason":str}; spine: fleet.paused (+ job.blocked)
   # streams NDJSON iteration events to stdout, one object per line:
   -> {"event":"iteration","worker":"sirius/oak","issue":"AMT-7","phase":"claim|map|lock|brief|work|gate|review|fix|receipt|release","...":...}
   # gate: {"result":"pass|fail|skipped","plan","tests_run","attempt",
   #        "reason_code","error_tail":str|null (fail only),"env_fault":bool,"setup_rerun":bool}
   #   SIRF-50: a fail whose output names a missing dependency ("Cannot find module",
   #   ERR_MODULE_NOT_FOUND, "ModuleNotFoundError: No module named", E0463, the test
   #   runner itself "not found", or exit 127 + "not found") in a fleet worktree with a
   #   setup command re-runs setup and re-gates ONCE per work/fix pass, WITHOUT using a
   #   retry_budget attempt; still failing ⇒ an ordinary failure. One gate event per attempt:
   #   "env_fault" = the FIRST gate of that attempt looked like an env fault; result,
   #   reason_code and error_tail describe the final (re-)gate. The lease is renewed before
   #   setup re-runs (refused ⇒ the iteration aborts, as a lost lease does elsewhere).
   # review (SIRF-23, only with review.cmd): {"phase":"review","round":N,"result":"clean|blocking|error|tampered|skipped","confirmed":K,"notes":M}
   # fix:  {"phase":"fix","round":N,"agent_ok":bool,...}  (then a re-gate, as today)
   # work/fix events (SIRF-41/52, additive): "timeout_kind":"idle"|"hard" when "timed_out":true
   #   (idle = no output and no worktree change for timeouts.idle_secs; hard = timeouts.hard_secs);
   #   "reason":"agent_bg_tasks_killed" when the agent's CLI killed its still-running background
   #   tasks on exit ("Background tasks still running … terminating") — agent_ok is then false
   #   whatever the exit code. The agent_timeout release event carries the same "timeout_kind".
   # release gains "review":"review: 2 rounds, 4 bugs fixed, 1 rebuttal accepted" when a review ran
   # SIRF-42: claim events carry "base_sha" (fleet): the base ref's tip at claim — the commit the
   #   worktree is reset to (launch base when no base ref exists / it does not resolve)
   # SIRF-41: before ANY non-advancing release (timeout, agent failure, usage limit, gate deadend,
   #   review release, failed stamp; and a lost lease, which does not release) the worktree's
   #   new work is committed ("sirius: wip <issue>", no hooks) and pinned (compare-and-swap); the
   #   release event (lease_lost included) gains "wip_ref":{"ref","sha","diffstat"[,"partial"]} —
   #   "partial" when only the committed part could be pinned — or "wip_error":str when pinning
   #   failed; the release comment ends "— work preserved at <ref> (<sha12>, <diffstat>)".
   #   An agent that itself failed counts as interrupted (wip), even if the gate then failed;
   #   resumed wip work that is then judged moves to wip-failed. A fix round's discarded
   #   attempt is parked at wip-superseded before the revert.
   #   Refs (issue key = 4th segment, so `sirius link --changed` counts them as the issue's own):
   #   refs/sirius/wip/<issue>        interrupted work — merged back on the next claim
   #                                  (SIRIUS_RESUMED_FROM / SIRIUS_RESUME_REF), removed once stamped
   #   refs/sirius/wip-failed/<issue> gate deadend / review release — offered as SIRIUS_PRIOR_WORK,
   #                                  never auto-merged; parked at completion if unused
   #   refs/sirius/wip-superseded/<issue>/<sha12>  an older pin newer work does not contain
   #   refs/sirius/wip-conflicted/<issue>          preserved work that no longer merges (fresh start)
   # SIRF-49: an agent (or fix agent) that leaves a named branch checked out in its worktree:
   #   pinned at refs/sirius/keep/<issue>/<branch-lowercased-sanitized>, HEAD detached in place,
   #   the branch untouched; stderr warning +
   #   {"event":"branch_guard","worker","issue","phase","branch","sha","ref","detached"}
```

`--agent-cmd` and `--review-cmd` support `{issue}` / `{worker}` / `{model}` templating, and
every agent/reviewer process gets this environment (SIRF-22 #4/#5, SIRF-23):

| Var | Meaning |
|---|---|
| `SIRIUS_ISSUE`, `SIRIUS_WORKER`, `SIRIUS_WORKTREE` | identity + the private worktree |
| `AMT_AGENT` | `sirius/<tree>` — the agent's own `amt` writes are attributed to the worker |
| `SIRIUS_PHASE` | `work` \| `review` \| `fix` |
| `SIRIUS_BASE` | this iteration's base commit — the tip of `SIRIUS_BASE_REF` when the issue was claimed (SIRF-42), else the launch base; the worktree was reset to it |
| `SIRIUS_RESUMED_FROM` | (fleet) the sha of earlier work merged onto the fresh worktree before the agent ran — held (SIRF-32) or preserved interrupted work (SIRF-41); the LAST one merged |
| `SIRIUS_RESUME_REF` | (fleet) the ref that resumed work came from (`refs/sirius/held/<issue>` or `refs/sirius/wip/<issue>`) |
| `SIRIUS_PRIOR_WORK` | (fleet) `refs/sirius/wip-failed/<issue>`: an earlier attempt the gate or review judged and failed — kept, NOT merged; look at it, don't trust it |
| `SIRIUS_BASE_REF` | the branch the fleet lands on (`review.base_ref`, else the launch branch); unset only when neither exists (a detached launch with no `review.base_ref`). `sirius link --changed` never stamps commits already on it |
| `ANTHROPIC_MODEL`, `SIRIUS_MODEL` | (work, fix) this ticket's resolved worker model — Claude Code honors `ANTHROPIC_MODEL` over settings.json |
| `ANTHROPIC_MODEL`, `SIRIUS_REVIEW_MODEL` | (review) the reviewer's model |
| `SIRIUS_REVIEW_DIR`, `SIRIUS_DIFF_RANGE` | (review, fix) the tree to review, and `git diff $SIRIUS_DIFF_RANGE` |
| `SIRIUS_FRONTIER`, `SIRIUS_SIBLING_BRANCHES`, `SIRIUS_SIBLINGS` | (review) the frontier commit, the siblings merged into it, and their rendered summary (SIRF-30; empty / "(none)" outside `frontier`) |
| `SIRIUS_ROUND` | (review, fix) 1-based round |
| `SIRIUS_REVIEW_OUT`, `SIRIUS_REVIEW_PROMPT` | (review) where to write findings; the rendered prompt |
| `SIRIUS_REVIEW_FINDINGS` | (fix) this round's findings; (review, round > 1) the previous findings WITH the worker's responses |
| `SIRIUS_FIX_OUT` | (fix) where the worker writes its responses |
| `SIRIUS_LAST_GATE_TAIL` | (work, fix) set only on a RETRY after a failed gate: the previous attempt's gate output tail (last 15 lines, ≤2KB), or `<reason_code>: <reason>` when nothing ran (SIRF-48) |
| `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` | always `0` (SIRF-52): headless `claude -p` waits for its background tasks instead of killing them after 600s — Sirius's idle watchdog and hard cap own the timeout |

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
                                           //   refs/sirius/held-conflicted/<issue> and the issue starts fresh;
                                           //   an older held commit a new hold does not contain is parked
                                           //   at refs/sirius/held-superseded/<issue>/<sha12>. Red arriving
                                           //   DURING a review also holds (never an unreviewed advance).
                                           //   `sirius doctor` reports red + parked refs (advisory).
    "timeout_secs": 1800
  },
  "worktree": {                            // SIRF-50 — fleet worktree preparation
    "setup_cmd": null,                     // run once per fresh worktree, before any agent; null ⇒ detected
                                           //   from the root lockfile (bun.lock/bun.lockb, pnpm-lock.yaml,
                                           //   yarn.lock, package-lock.json, uv.lock); "" ⇒ no setup.
                                           //   `sirius init` pre-fills the detected command.
                                           //   Re-run by an iteration whose base changed a lockfile
                                           //   (stamp in the worktree's git dir). Supervised: leases
                                           //   are renewed while it waits for a sibling's setup or runs.
    "setup_timeout_secs": null             // null = 1800: kill a hung install (setup failure)
  },
  "timeouts": {                            // SIRF-41 — work/fix agents only (reviewer: review.timeout_secs,
                                           //   integration: integration.timeout_secs — plain wall clocks)
    "idle_secs": null,                     // null = 1800: kill after this long with no output (log growth)
                                           //   and no worktree change (HEAD, porcelain status, size+mtime of
                                           //   each dirty path; probed ≤ every 30s, and again before any kill)
    "hard_secs": null,                     // null = 10800 (3h) — or a legacy agent_timeout_secs ≠ 1800
    "routes": []                           // [{"labels": ["ui"], "idle_secs": …, "hard_secs": …}], first label
                                           //   match wins (the models.routes rule); unset fields keep the base
  }
  // agent_timeout_secs (LEGACY, no longer written by init): the hard cap when timeouts.hard_secs
  // is null — except 1800, the old init default, which is ignored. Never sets idle.
  // A kill due within 60s of a commit (HEAD moved) is deferred ONCE by 60s (SIRF-41 #4).
  // Progress = ANY log growth or non-ignored file change: a hung agent whose background child
  // keeps printing or writing tracked/untracked files is caught only by the hard cap.
  // `sirius doctor` check agent_timeouts (advisory) warns on hard < 3600s or idle < 300s.
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

**Known escape patterns (SIRF-35).** `$SIRIUS_ESCAPES` (a prompt placeholder, appended
when a custom template lacks it) lists the repo's recorded escape kinds that are NOT yet
automated — top 8 by count, then recency — with the latest summary of each, so every
review checks the diff for what got past review before. "(none)" when there are none.

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
