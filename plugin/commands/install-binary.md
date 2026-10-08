---
description: Download and install the platform-correct `sirius` CLI binary for this OS/arch from the latest Sirius Forester GitHub release, verifying its checksum and Sigstore signature. Use when `sirius` is not yet installed.
argument-hint: "[vX.Y.Z]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.sh":*), Bash(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.ps1":*)
---

# Install the `sirius` binary

The Sirius Forester plugin ships the Agent Skill, but a git-based plugin install
can **not** deliver the compiled `sirius` CLI (it's platform-specific and large,
so it is not committed to the repo). This command bridges that gap: it downloads
the release tarball matching this machine's OS + CPU arch, verifies its sha256,
and installs `sirius` into the plugin's persistent data directory.

Two installers ship side by side — the same download, the same checksum, the
same Sigstore verification. **Pick by shell, not by OS branding:**

- **A POSIX shell** (macOS, Linux, or Windows under Git Bash / MSYS2 /
  Cygwin) → run `install-sirius.sh`.
- **Native Windows PowerShell**, with no POSIX shell available → run
  `install-sirius.ps1`. A stock Windows box has no Git Bash; do not send the
  user off to install one, and do not fall back to hand-extracting a tarball.

**WSL is not a Windows route.** Inside WSL `uname` reports Linux, so the `.sh`
installs a *Linux* `sirius` into the WSL filesystem — usable only from inside
WSL, invisible to a Windows-native Claude Code. Only take it when the user
actually runs Claude Code inside WSL. On Windows, `bash` on `PATH` may be the
WSL launcher (`C:\Windows\System32\bash.exe`); that does not count as a POSIX
shell here — use the PowerShell path.

Determine which you have before running anything: if `sh` / `bash` resolves
(and is not the WSL launcher), take the POSIX path; otherwise take the
PowerShell path. Run the commands below exactly as written — the quoting
matches this command's allowed-tools rules.

### POSIX (macOS / Linux / Git Bash / MSYS2 / Cygwin)

If the user passed a tag (e.g. `v0.1.0`), forward it explicitly:

```sh
"${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.sh" --version "$ARGUMENTS"
```

If no tag was passed, install the latest release instead (do NOT pass an empty
`--version`):

```sh
"${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.sh"
```

### Windows PowerShell

Same contract, PowerShell-native flags (`-Version`, not `--version`). With a tag:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.ps1" -Version "$ARGUMENTS"
```

With no tag, install the latest release (do NOT pass an empty `-Version`):

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.ps1"
```

`-ExecutionPolicy Bypass` is scoped to this one process and only allows the
bundled script to run at all; it changes nothing about the checksum or
signature verification the script performs. Other switches map one-to-one:
`-Prefix DIR`, `-RequireSignature`, `-Check`, `-AddToPath`, `-Force`.

After it finishes (either path):

- If the script printed a PATH note (the install dir isn't on `PATH`), relay that
  to the user verbatim so they can add it to their shell rc.
- Tell the user the next steps the script printed: `sirius init` then
  `sirius doctor`.
- If the script printed a "fleet suite" note about missing companion tools
  (Ametrite, Hayvenhurst, Catryna Wikinelli), relay it — Sirius is the foreman
  and delivers full fleet control only with the whole suite installed.
- If the download, checksum, or signature verification failed, report the exact
  error; do not retry silently. A common cause is that no GitHub release exists
  yet for the resolved tag/platform. A SIGNATURE VERIFICATION FAILED error is
  never routine: stop and surface it, do not work around it.
- If the script warned that no signature verifier was found, relay that: the
  binary installed on the strength of TLS and a checksum alone. Suggest
  `brew install cosign` (or `pip install sigstore`) and re-running with
  `--require-signature` (`-RequireSignature` on the PowerShell path).
