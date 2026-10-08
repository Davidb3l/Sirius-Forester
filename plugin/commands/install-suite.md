---
description: Install the whole Sothis suite in one shot — the sirius + hayven CLIs (and a check for amt + catryna/bun), verified by each tool's own installer, running `sirius doctor` and ending with a verification of the Claude Code plugin half. Use for "let's Sothis this up" / installing the full fleet, not just sirius.
argument-hint: "[--skip-hayven] [--skip-amt] [--require-signature]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/scripts/install-sothis.sh":*), Bash(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${CLAUDE_PLUGIN_ROOT}/scripts/install-sothis.ps1":*)
---

# Install the Sothis suite

Sirius is the foreman of the **Sothis** suite; full fleet control comes from the
four CLIs, with PingMyBell (the desktop notch/voice app) as the optional fifth.
Installing them has two halves — this command handles the CLIs, and the script
finishes the plugin half itself via the non-interactive `claude plugin` CLI
(v2.1.195+), falling back to printed routes when it can't.

Run the bundled one-shot. It installs every missing suite CLI by delegating to
each tool's own authoritative installer (sirius verifies a Sigstore signature,
hayven verifies a sha256), detects `amt` (guiding you to the ametrite skill if
missing — it never auto-builds), checks `bun` for Catryna, then runs
`sirius doctor`.

Two one-shots ship side by side — same order, same delegation, same
verification. **Pick by shell, not by OS branding:** if `sh` / `bash` resolves
(macOS, Linux, or Windows under Git Bash / MSYS2 / Cygwin), run the `.sh`;
otherwise, on native Windows PowerShell, run the `.ps1`. A stock Windows box has
no Git Bash — do not send the user off to install one.

**WSL is not a Windows route:** inside WSL `uname` reports Linux, so the `.sh`
installs *Linux* binaries into the WSL filesystem — usable only from inside
WSL, invisible to a Windows-native Claude Code. Take it only when Claude Code
itself runs inside WSL. (The WSL launcher `C:\Windows\System32\bash.exe` on a
Windows `PATH` does not count as a POSIX shell — use the `.ps1`.) Run the
commands below exactly as written; the quoting matches this command's
allowed-tools rules.

### POSIX (macOS / Linux / Git Bash / MSYS2 / Cygwin)

```sh
"${CLAUDE_PLUGIN_ROOT}/scripts/install-sothis.sh" $ARGUMENTS
```

Forward any flags the user passed (`--skip-hayven`, `--skip-amt`,
`--require-signature`, `--prefix DIR`) verbatim as `$ARGUMENTS`. If none were
passed, run it with no arguments.

### Windows PowerShell

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${CLAUDE_PLUGIN_ROOT}/scripts/install-sothis.ps1"
```

Translate any flags the user passed into their PowerShell switch form and
append them — do NOT pass the POSIX spellings through verbatim, they will be
rejected:

| POSIX | PowerShell |
| --- | --- |
| `--skip-hayven` | `-SkipHayven` |
| `--skip-amt` | `-SkipAmt` |
| `--skip-plugins` | `-SkipPlugins` |
| `--require-signature` | `-RequireSignature` |
| `--check` | `-Check` |
| `--prefix DIR` | `-Prefix DIR` |

`-ExecutionPolicy Bypass` is scoped to this one process and only allows the
bundled script to run at all; it changes nothing about the checksum and
signature verification the delegated installers perform.

After it finishes (either path):

- **Honor the plugin-half verdict — it is checked, not assumed.** The script
  ends by AUTO-INSTALLING any missing Claude Code plugin half via the
  non-interactive `claude plugin` CLI, then VERIFYING it (bundle marketplace +
  the sirius/hayvenhurst/catryna plugins, matched by `<name>@` prefix so a
  plugin from its own standalone marketplace counts). If it still printed a
  **`YOU ARE NOT DONE`** block, the auto-attempt failed (old/missing `claude`
  CLI): relay the block prominently — you may run its `claude plugin ...`
  commands yourself via Bash, or point desktop-app users at **+ → Plugins →
  Add plugin**. Do NOT summarize the install as complete while that block is
  present. If it printed "plugin half complete", say so.
- If the script printed a **PATH note**, relay it verbatim so the user can add
  the install dir to their shell rc.
- If it reported **`amt` missing**, relay the guidance: the fastest path is
  asking Claude to "ametrite this repo" (the ametrite skill bootstraps `amt`).
- If it warned that **`bun` is missing**, relay it — the Catryna MCP server runs
  on bun.
- If it noted **pingmybell not installed**, mention it once as optional — the
  bell (voice callouts + notch board) is a desktop app built from source at
  https://github.com/Davidb3l/pingmybell; never attempt to install it yourself.
- **On Windows, a `hayven: WARNING` is expected, not a failure.** Hayvenhurst
  has no native Windows installer yet (its `install-hayven.sh` refuses Git Bash
  / MSYS / Cygwin), so both one-shots warn, print the manual route (the
  `windows-x64` release tarball + its `.sha256`), and carry on. Relay that
  route; sirius works without hayven, with reduced function, until it's done.
- If the script **fetched and ran hayven's installer over HTTPS** (no local copy
  found), that's expected; but if a download, checksum, or **signature
  verification failed**, report the exact error and stop — never work around a
  SIGNATURE VERIFICATION FAILED.
- End by showing the `sirius doctor` result. If there's no `.sirius/` workspace
  yet, tell the user to run `sirius init` then `sirius doctor` in their repo.

To install only the sirius binary (not the whole suite), use
`/sirius:install-binary` instead.
