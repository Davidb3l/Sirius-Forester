# Changelog

User-visible changes to `sirius`. Lines accumulate under **Unreleased**; a
release moves them under its version (see AGENTS.md → "Releases are batched").

## Unreleased

### Plugin — bump its version at release (the plugin cache is keyed on version, so an unbumped change reaches no installed user — SF-10)
- Windows: native PowerShell installers `install-sirius.ps1` and `install-sothis.ps1`; `/sirius:install-binary` and `/sirius:install-suite` now reach them (their allow-lists were `.sh`-only).
- `install-sirius.sh` and `install-sothis.sh` run from Git Bash / MSYS / Cygwin — they refused `MINGW64_NT-*` although the Windows tarballs have always shipped. On Windows they print the exact PowerShell line to put the install dir on PATH.
- A failed `claude plugin install` now prints its real error, and on an SSH clone failure the HTTPS `GIT_CONFIG_*` workaround, instead of a bare "YOU ARE NOT DONE".
- The sirius skill: omitting `--from` claims from todo AND backlog (it said todo); `--agent-cmd`'s program must be on PATH, and solo mode is the path when it is not.

### Added
- Console: **one console, every fleet.** `:1777` lists every repo with a Sirius ledger (from the Ametrite registry, plus the launch repo) in a header switcher — running fleets first, with live worker counts — like the Ametrite board. The choice is remembered and carried as `?ws=<alias>`; live updates follow the switch. New endpoint `/api/workspaces`.
- `sirius doctor` `gate_configured` (gating): fails while `gate.test_cmd` is unset, naming a command detected from the repo. (SF-11)
- `sirius init` pre-fills `gate.test_cmd` from the repo (Cargo.toml; package.json with a test script; pyproject.toml / pytest.ini; go.mod). (SF-11)
- `sirius gate --json` carries `reason_code` and `structural`, so an unconfigured workspace is distinguishable from failing tests (both exit 3). (SF-11)
- `SIRIUS_SHELL` overrides the shell that gate commands and agents run through. (SF-15)
- `sirius doctor` prints the plugin version beside the CLI's (advisory). (SF-10)

### Changed
- `gate.test_cmd` and `--agent-cmd` run through an explicitly resolved shell instead of a bare `sh` inherited from the launcher, so a compound command (`a && b`) no longer passes from Git Bash and fails from PowerShell. (SF-15)
- `sirius run` refuses to start (exit 2) when `--agent-cmd`'s program is not on PATH, and a `--from` run that claimed nothing names where the work is parked. (SF-14)

### Fixed
- Console: the Fleet/History views no longer fail with "unable to open database file" when no fleet is running (an idle WAL ledger has no `-shm`/`-wal` side files, which a read-only SQLite connection cannot create). The console now falls back to a `query_only` connection — still never writes.
- Console: the header and tab title name the fleet being watched and the real port (it always said `:1777`).
- Windows: every `git worktree` sirius creates — fleet workers, review trees, `sirius integrate`, frontier checks, review canaries — failed on the `\\?\` path prefix, so the fleet could not start at all. (SF-16)
- `sirius doctor` blessed a stale hayven daemon that 500s on every claim; it now compares the daemon's build to the CLI's. (SF-13)
- `sirius link --changed` right after a commit silently filed no receipt; it now uses the work just committed (inside a fleet, the worker's own commits since `$SIRIUS_BASE` plus resumed work; else the last commit), and refuses to guess while untracked files are pending. (SF-12)
- A shell that cannot start is named as the shell, instead of reading as a missing test binary. (SF-15)

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
