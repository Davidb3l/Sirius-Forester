---
description: Measure how good the reviewer is — replay recorded escapes (their fixes reverted, i.e. the real bugs back) and .sirius/canaries/*.patch through review.cmd, blind, and report recall plus false positives on a harmless control.
argument-hint: "[--n N]"
allowed-tools: Bash(sirius review-canary:*), Bash(sirius escape --list:*), Glob, Read
---

# Review canaries

This runs the configured reviewer once per canary plus once for the control, so
it costs real reviewer tokens and time. Before running:

1. Check `.sirius/config.json` has `review.cmd` set — without a reviewer there is
   nothing to measure (the command exits 1 with `{"ok":false,"error":…}`).
2. Count the canaries: `sirius escape --list --json` rows whose `fix` is not null
   (one canary per distinct fix commit), plus `.sirius/canaries/*.patch` files
   (excluding `control.patch`). Tell the human the count (capped by `--n`,
   default 10) and confirm before running.

Then run `sirius review-canary --json $ARGUMENTS` and report:

- **Recall**: `caught` of `total` reviewed, plus `errors` (a reviewer failure
  counts as a miss). For each entry in `canaries` whose `result` is `"missed"`,
  name its `kind` (or its `source` for a `.patch` canary) — those are the
  review's blind spots.
- **False positives**: confirmed blocking findings on the harmless control
  (`null` means the control did not run).
- `stale` canaries no longer apply to the current base; they are not scored.

If the human is comparing reviewers or prompts, suggest running it before and
after the change. With no canaries at all, explain that each `sirius escape`
recorded with `--fix <sha>` becomes one, and `.sirius/canaries/*.patch` files
can be added by hand.
