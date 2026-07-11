---
name: sirius
description: >-
  Drive Sirius Forester, the local-first foreman for a fleet of AI coding
  agents, in any repo that has a .sirius/ ledger beside .ametrite/ and .hayven/.
  TWO WAYS IN. (1) START THE FOREMAN — when the human says "let's get Sirius",
  "get Sirius on this repo", "Sirius this repo/Forester", "run the fleet/foreman
  here", or otherwise asks to kick off the loop, run `sirius run` (or drive one
  iteration by hand). (2) BE A WORKER — when you ARE the agent inside an
  iteration ("work an issue the Sirius way"), follow the etiquette: claim → map →
  lock → brief → work → gate → receipt → release, honoring claim order, 409
  backoff, the gate, and a two-way receipt. If `sirius` is not installed, run
  /sirius:install-binary first.
---

# Driving Sirius Forester

Sirius is the **foreman** of the local-first suite: it claims tasks from
Ametrite (the board), locks the code each touches via Hayvenhurst (the code
graph), runs your agent, refuses to call anything done until the affected tests
pass, and files a two-way receipt for every change. (Catryna Wikinelli, the
wiki, is the fourth tool.)

There are two ways you'll use this skill. **Starting the foreman** — the human
wants the loop running on a repo. **Being a worker** — you *are* the agent an
iteration runs. Pick the section that matches; when unsure, ask.

If the `sirius` binary is missing, install it first with `/sirius:install-binary`
(never build from source unless asked). If the install script reports missing
companions, relay its suggestion — full fleet control needs all four.

---

## A. Start the foreman — `sirius run`

When the human says **"let's get Sirius"**, "get Sirius on this repo", "Sirius
this Forester", "run the fleet here", or anything that means *kick off the loop*,
they want the foreman working the board on the current repo.

1. **Preflight.** `sirius doctor` must be clean — the five §6 contract facts. If
   it reports drift, stop and surface it: the loop's guarantees don't hold on a
   broken workspace. (`/sirius:install-binary` first if `sirius` is missing.)
2. **Agree on the launch before you unleash it.** `sirius run` spawns worker
   agents that edit code autonomously, without asking per change — so on the
   first run in a repo, confirm scope with the human and settle three flags:
   - `--workers N` — how many workers (start small, e.g. 2–3);
   - `--from <stage>` — which column to pull from (default `todo`);
   - `--agent-cmd '<command>'` — the command each iteration runs to do the work,
     e.g. `claude -p "work the claimed issue the Sirius way"`. This is the agent;
     without it the loop has nothing to run.
3. **Run it and watch the fleet.**
   ```bash
   sirius run --workers 2 --from todo \
     --agent-cmd 'claude -p "work the claimed issue the Sirius way"'
   ```
   It streams one NDJSON event per phase (`claim` → … → `release`), and each
   iteration also appends to the suite event spine. Tail it to watch provenance
   land in real time:
   ```bash
   tail -f .suite/events/$(date -u +%F).jsonl
   ```
   The loop exits when a full round finds no claimable work. Exit codes: `0` ok,
   `1` failure, `2` usage, `3` gate blocked.
4. **Read the receipts.** Every completed issue leaves a two-way receipt; spot
   check with `sirius why <symbol>` and `sirius why <ISSUE>` — both must answer.

Want a single iteration by hand instead of spawning subprocess workers (solo
mode, tighter control, no autonomous fan-out)? That's section B.

---

## B. Being a Sirius worker

You are running **one iteration** of the loop yourself: pull a task, turn it
into claimed code, do the work, gate the finish, file the paperwork, release.
This is the exact sequence `sirius run` automates (PRD §9) — you are doing it by
hand as a single worker. Reuse the same worker identity throughout (below).

Your worker identity is `sirius/<tree>` (e.g. `sirius/oak`). Use the **same** id
for every step of one iteration: it flows into `AMT_AGENT`, the Hayvenhurst claim
`agent`, and the receipt. Reusing your own id on a claim is a *heartbeat*, not a
collision.

## The cardinal rule: claim order is law

**Ametrite issue first, Hayvenhurst entities second. Release in reverse.**

Never hold an issue while spinning on a code lock. If an entity claim collides
(409), release the issue back immediately with a comment naming the blocker, and
move on. Holding an issue while you wait for code is how a fleet deadlocks.

## Before you start

Confirm the workspace is healthy and you know the ground-truth flags:

```bash
sirius doctor           # the five §6 contract facts must pass
amt --help; hayven --help   # installed binaries are authoritative, not the PRD
```

If `sirius doctor` reports a failing contract fact, stop and surface it — a
broken parent invariant means the loop's guarantees no longer hold.

---

## The iteration, step by step

### 1. Claim — the issue (hard lock, 900 s lease)
```bash
amt claim --from todo --agent sirius/oak --json
```
- `{"claimed": true, "issue": "AMT-7", ...}` → you own it; proceed.
- `{"claimed": false, "retry_after": N}` → **honor it.** Wait `retry_after`,
  switch stage (`--from backlog` to scope instead of build), or exit. Do not
  busy-spin.

### 2. Map — issue → symbols
Translate the issue into the code it touches. This is deterministic (FTS +
graph), no model call:
```bash
hayven query "<terms from the issue title/body>" --json   # candidate symbols
hayven impact <symbol> --json                              # blast radius
```
Keep the mapped set tight: it is what you will claim and what the gate will test.

### 3. Lock — the entities (hard locks, claimed SECOND)
For each mapped entity, in order:
```bash
hayven claim <symbol> --intent "AMT-7: <title>" --agent sirius/oak
```
Read the exit code:
- **0** — registered, you hold it.
- **1** — hard overlap (another worker holds it). **Stop.** Release any entities
  you already claimed (reverse order), then release the issue and comment:
  ```bash
  amt release AMT-7 --json
  amt comment AMT-7 "released: entity <symbol> held by <blocker> — will retry" --json
  ```
  End the iteration cleanly. This is the cheap way to lose a race; do not force.
- **3** — a soft oracle verdict (adjacency, Layer C). Policy decides: default is
  **back off** (treat like a collision), or **force-with-budget** if the project
  config allows it. When unsure, back off.

### 4. Brief — assemble your context
You work from the briefing pack, not raw repo grep:
```bash
hayven context <symbol> --json      # precise context pack per mapped symbol
hayven recall --node <id> --json    # prior gotchas / deadends on this node
```
Plus the issue itself: title, body, comments, and any linked decisions from
`amt`. If `hayven recall` shows a prior `deadend` note for this node, read it —
someone already hit this wall; do not re-derive the failure.

### 5. Work — make the change
Do the actual edit, scoped to the entities you hold. Stay inside the claimed
blast radius; if the work pulls you into unclaimed code, that is a new entity to
claim (step 3's rules apply) or a sign the mapping was too narrow — reconsider
before sprawling.

Heartbeat while you work if the iteration is long: re-claim the same Ametrite
issue id (renews the 900 s lease) and re-post the same Hayvenhurst claim id (same
agent + same id = refresh, not collision).

### 6. Gate — test the finish
```bash
sirius gate AMT-7 --tier safe --target-status in_review
```
or directly:
```bash
hayven affected-tests --changed --gate --gate-tier safe
```
- **exit 0** → the SAFE tier's selected tests pass; the issue advances to
  `in_review`. Proceed to the receipt.
- **exit 1** → a selected test failed. The failure is filed as an issue comment
  and the status is **not** advanced. Fix and re-gate, within your retry budget.
  On budget exhaustion, release with a `deadend` fleet-memory note (step 8) so
  the next worker inherits the lesson instead of the failure.

Do **not** advance an issue past a failing gate. The gate is the whole point.

### 7. Receipt — file two-way provenance
Record a decision and stamp it in both directions:
```bash
amt decide AMT-7 "<why this change, in one line>" --json     # → {"decision":"D-n"}
sirius link --decision D-n --changed                          # forward + reverse stamp
```
`sirius link` writes the entity IDs into the issue's activity (forward) **and** a
`decision`-kind fleet memory onto each touched Hayvenhurst node naming
`AMT-7`/`D-n` (reverse). After this, `sirius why <symbol>` and `sirius why AMT-7`
both answer. **Every completed issue must carry this receipt** — it is a measured
success metric (100% coverage), not optional courtesy.

### 8. Release — reverse order
Entities first, then the issue:
```bash
hayven release <claimId>     # each entity you held
# … then close out the issue in Ametrite
```
If the iteration ended in a dead end (retry budget exhausted), leave the trail
before releasing:
```bash
hayven remember --kind decision --node <id> --scope <ids> "deadend: AMT-7 — <what failed and why>"
```

---

## Quick checklist

- [ ] `sirius doctor` clean before starting.
- [ ] Same `sirius/<tree>` id throughout.
- [ ] Issue claimed **before** any entity; released **after** all entities.
- [ ] Entity 409 → release the issue with a naming comment; never hold-and-spin.
- [ ] Worked only inside the claimed blast radius.
- [ ] Gate passed (exit 0) before the issue advanced.
- [ ] Two-way receipt filed (`amt decide` + `sirius link`).
- [ ] Released in reverse order; deadends recorded as fleet memory.

## What NOT to do

- Do not write `.ametrite/` or `.hayven/` databases directly — only via `amt` /
  `hayven` / `sirius`.
- Do not advance an issue past a failing SAFE-tier gate.
- Do not hold an Ametrite issue while waiting on a Hayvenhurst code lock.
- Do not skip the receipt because the change was "small" — coverage is measured.
- Do not force an oracle 202/exit-3 verdict unless the project config grants a
  force budget; the default is to back off.
