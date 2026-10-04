# Changelog

User-visible changes to `sirius`. Lines accumulate under **Unreleased**; a
release moves them under its version (see AGENTS.md → "Releases are batched").

## Unreleased

## 0.1.6 — 2026-10-04

### Plugin 0.2.5 (terminal and the Claude app alike)
- New slash commands `/sirius:integrate`, `/sirius:escape`, `/sirius:review-canary`.
- The sirius skill learns a third way in — "guard what lands" (integration, escapes, canaries) — plus fix-mode guidance for `AUTO-` and `sibling-conflict` findings.
- The SessionStart check now says when a newer `sirius` release is out (suite repos only, at most once a day).

### Added
- **Integration frontier** (`review.against: "frontier"`): each change is reviewed merged onto the base plus every in-flight `sirius/*` branch awaiting integration; clashes with another branch are named (`sibling-conflict` findings). (SIRF-30)
- **Sequence-collision detector** (`review.sequences`): migration-style slots taken out of order, or claimed by an in-flight sibling or a fleet peer, become unrebuttable `AUTO-` findings. (SIRF-31)
- **`sirius integrate`**: runs `integration.cmd` on the frontier before merge; red files one issue and, with `integration.on_fail: "block"`, stops the line (finished work is held and resumed later). `--clear-red` overrides. (SIRF-32)
- **`sirius escape`**: record a defect that got past review; live escape patterns go into every review prompt; `--automated-by` retires a kind once a real check exists. (SIRF-35)
- **`sirius review-canary`**: replays escapes' reverted fixes (and `.sirius/canaries/*.patch`) through the reviewer, blind, and reports recall and false positives. (SIRF-35)
- `sirius why <ISSUE>` lists the issue's escapes; `sirius doctor` reports the integration red state (advisory).

### Fixed
- A reviewer's hooks writing to `.suite/` no longer read as tampering: the reviewer always runs in a throwaway worktree. (SIRF-29)
- A timed-out agent's whole process tree is killed, not just its shell.
- `sirius doctor` reports an invalid config instead of silently using defaults.

## 0.1.5 and earlier

See the `chore(release)` / `vX.Y.Z` commits and the GitHub Releases.
