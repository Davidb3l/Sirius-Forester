---
description: Check that the in-flight work integrates — build the frontier (the base plus every sirius/* branch awaiting integration) and run integration.cmd on it before anything merges. Reports green/red; red files one issue and, with integration.on_fail "block", stops the fleet.
argument-hint: "[--clear-red]"
allowed-tools: Bash(sirius integrate:*), Read
---

# Integration check

Run `sirius integrate --json $ARGUMENTS` in the repo root and report back in plain
language. Read the `ran`, `ok` and `red` fields (the exit code is 3 whenever the
line is red, however it got there):

- **`ran: false`** — `integration.cmd` is not set in `.sirius/config.json`, so
  nothing was tested (the frontier was only built). Offer to set it (ask which
  command runs their integration / e2e suite). If `red` is true, the line is
  still red from an EARLIER run — nothing tested can't clear it.
- **`ran: true, ok: true, red: false`** — green: name the in-flight issues
  integrated (`included`); the line is open.
- **`ran: true, ok: true, red: true`** — the command passed, but the board or git
  could not list every in-flight issue (`complete: false`), so an earlier red
  state is not cleared; the next run retries.
- **`ran: true, ok: false`** — red: the issue it filed or updated (`issue`), the
  branches combined (`included`), the command's `exit`, and what failed — `log`
  is a file PATH; read its last lines. With `integration.on_fail: "block"` the
  fleet takes no new work until a later run is green.
- **Exit 1** — show the error (e.g. another `sirius integrate` is running).

`left_out` lists in-flight branches that conflict with each other textually —
mention them; whoever merges second resolves it.

Only pass `--clear-red` when the human explicitly asked to override the red
state without a green run — it unblocks the fleet by hand.
