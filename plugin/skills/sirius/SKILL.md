---
name: sirius
description: >-
  Loop etiquette for an AI agent that IS the worker in a Sirius Forester
  iteration — human-driven or single-agent mode, where you run the loop by hand
  instead of `sirius run` spawning you. Use when working a task in a repo that
  has a .sirius/ ledger beside .ametrite/ and .hayven/, or whenever asked to
  "work an issue the Sirius way", claim → map → lock → brief → work → gate →
  receipt → release. Covers claim order, 409 backoff, the gate, and filing a
  two-way receipt. If `sirius` is not installed, run /sirius:install-binary
  first.
---

# Being a Sirius worker

You are running **one iteration** of the Sirius Forester loop yourself: pull a
task, turn it into claimed code, do the work, gate the finish, file the
paperwork, release. This is the exact sequence `sirius run` automates (PRD §9) —
you are doing it by hand as a single worker.

If the `sirius` binary is missing, install it first with `/sirius:install-binary`
(never build from source unless asked). Sirius is the foreman of the full suite:
Ametrite (the board), Hayvenhurst (the code graph), Catryna Wikinelli (the
wiki). If the install script reports missing companions, relay its suggestion —
full fleet control needs all four.

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
