---
description: Install the whole Sothis suite in one shot — the sirius + hayven CLIs (and a check for amt + catryna/bun), verified by each tool's own installer, ending with `sirius doctor`. Use for "let's Sothis this up" / installing the full fleet, not just sirius.
argument-hint: "[--skip-hayven] [--skip-amt] [--require-signature]"
allowed-tools: Bash(${CLAUDE_PLUGIN_ROOT}/scripts/install-sothis.sh:*)
---

# Install the Sothis suite

Sirius is the foreman of the **Sothis** suite; full fleet control comes from all
four tools. Installing them has two halves — this command handles the CLIs; the
marketplace bundle handles the interactive `/plugin` steps.

Run the bundled one-shot. It installs every missing suite CLI by delegating to
each tool's own authoritative installer (sirius verifies a Sigstore signature,
hayven verifies a sha256), detects `amt` (guiding you to the ametrite skill if
missing — it never auto-builds), checks `bun` for Catryna, then runs
`sirius doctor`:

```sh
"${CLAUDE_PLUGIN_ROOT}/scripts/install-sothis.sh" $ARGUMENTS
```

Forward any flags the user passed (`--skip-hayven`, `--skip-amt`,
`--require-signature`, `--prefix DIR`) verbatim as `$ARGUMENTS`. If none were
passed, run it with no arguments.

After it finishes:

- **Do the interactive half yourself, once the CLIs are in.** The script can't
  run `/plugin` commands. Tell the user (or do it if they're driving Claude Code
  interactively) to run the Sothis bundle:
  ```
  /plugin marketplace add Davidb3l/Sirius-Forester
  /plugin install sirius@sirius-forester
  /plugin install hayvenhurst@sirius-forester
  /plugin install catryna@sirius-forester
  ```
  One marketplace add exposes all three plugins.
- If the script printed a **PATH note**, relay it verbatim so the user can add
  the install dir to their shell rc.
- If it reported **`amt` missing**, relay the guidance: the fastest path is
  asking Claude to "ametrite this repo" (the ametrite skill bootstraps `amt`).
- If it warned that **`bun` is missing**, relay it — the Catryna MCP server runs
  on bun.
- If the script **fetched and ran hayven's installer over HTTPS** (no local copy
  found), that's expected; but if a download, checksum, or **signature
  verification failed**, report the exact error and stop — never work around a
  SIGNATURE VERIFICATION FAILED.
- End by showing the `sirius doctor` result. If there's no `.sirius/` workspace
  yet, tell the user to run `sirius init` then `sirius doctor` in their repo.

To install only the sirius binary (not the whole suite), use
`/sirius:install-binary` instead.
