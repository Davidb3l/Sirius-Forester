---
description: Record a bug that got past review (an "escape") against the issue that shipped it, so every later review checks for that pattern and the fix becomes a reviewer canary. Also lists escapes, or retires a kind once a real check catches it.
argument-hint: "<ISSUE> <what escaped>  |  --list  |  --kind <slug> --automated-by <path>"
allowed-tools: Bash(sirius escape:*), Bash(git log:*), Bash(git show:*), Bash(amt issue show:*)
---

# Record an escape

Arguments: `$ARGUMENTS`

- `--list` → run `sirius escape --list` and summarize the kinds (count, whether
  automated).
- `--kind <slug> --automated-by <path>` → run it as given; it retires the kind
  from the review prompt (the path must exist).
- Otherwise this is a NEW escape. Work out, then confirm with the human before
  recording:
  1. **The issue that shipped the bug** (e.g. from `git log`/`git show` on the
     faulty code, or the human).
  2. **A kind** — a short lowercase slug for the class of defect
     (`migration-fork`, `money-float`, `missing-tenant-filter`). Reuse an
     existing kind from `sirius escape --list` when it fits.
  3. **What escaped** — one sentence.
  4. **Who found it** — `main-session`, `integration`, `human`, `e2e`, …
  5. **The fix commit**, if it is already fixed — it makes the escape a canary
     for `sirius review-canary`.

  Then run:
  `sirius escape <ISSUE> --kind <slug> -m "<what escaped>" --found-by <who> [--fix <sha>] --json`
  and relay any `nudge` (a kind escaping again should be automated) or
  `unretired` (an automated check missed this one).
