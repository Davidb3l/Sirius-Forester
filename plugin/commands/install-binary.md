---
description: Download and install the platform-correct `sirius` CLI binary for this OS/arch from the latest Sirius Forester GitHub release, verifying its checksum. Use when `sirius` is not yet installed.
argument-hint: "[vX.Y.Z]"
allowed-tools: Bash(${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.sh:*)
---

# Install the `sirius` binary

The Sirius Forester plugin ships the Agent Skill, but a git-based plugin install
can **not** deliver the compiled `sirius` CLI (it's platform-specific and large,
so it is not committed to the repo). This command bridges that gap: it downloads
the release tarball matching this machine's OS + CPU arch, verifies its sha256,
and installs `sirius` into the plugin's persistent data directory.

Run the bundled install script. If the user passed a tag (e.g. `v0.1.0`),
forward it explicitly:

```sh
"${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.sh" --version "$ARGUMENTS"
```

If no tag was passed, install the latest release instead (do NOT pass an empty
`--version`):

```sh
"${CLAUDE_PLUGIN_ROOT}/scripts/install-sirius.sh"
```

After it finishes:

- If the script printed a PATH note (the install dir isn't on `PATH`), relay that
  to the user verbatim so they can add it to their shell rc.
- Tell the user the next steps the script printed: `sirius init` then
  `sirius doctor`.
- If the script printed a "fleet suite" note about missing companion tools
  (Ametrite, Hayvenhurst, Catryna Wikinelli), relay it — Sirius is the foreman
  and delivers full fleet control only with the whole suite installed.
- If the download or checksum verification failed, report the exact error; do not
  retry silently. A common cause is that no GitHub release exists yet for the
  resolved tag/platform.
