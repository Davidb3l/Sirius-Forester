---
name: sothis
description: >-
  Install the whole Sothis suite — the local-first fleet led by Sirius Forester:
  Sirius (foreman), Hayvenhurst (code graph), Ametrite (board), Catryna Wikinelli
  (docs), PingMyBell (the bell — optional desktop notch/voice app). Trigger when
  the human says "let's Sothis this up", "let's Sothis up",
  "install the Sothis suite", "set up the whole suite/fleet", "get the full fleet
  on this repo", or otherwise asks for all the tools at once (not just Sirius).
  Two halves IN ORDER: the marketplace bundle's plugins first (Claude runs the
  non-interactive `claude plugin` CLI itself — works from a cold start), then
  the install-sothis.sh one-shot for the CLIs — which auto-installs and
  verifies any remaining plugin half. To install ONLY the
  sirius binary, use /sirius:install-binary; to just RUN the foreman once it's
  installed, that's the `sirius` skill.
---

# Installing the Sothis suite — "let's Sothis this up"

**Sothis** is the local-first suite led by Sirius Forester. Five standalone
tools that compose through the suite contracts:

- **Sirius Forester** (`sirius`) — the foreman/loop. Signed prebuilt binary.
- **Hayvenhurst** (`hayven`) — the code graph. Prebuilt binary.
- **Ametrite** (`amt`) — the task board + decisions. Rust binary (cargo).
- **Catryna Wikinelli** (`catryna`) — the living "why" docs. bun-based MCP plugin.
- **PingMyBell** — the bell: voice callouts + a notch command center when the
  fleet needs you. Desktop app, built from source (optional; early alpha, macOS).

Each stands alone; **full fleet control needs the CLIs (first four).** They
install five different ways, split across two halves — **plugins first, then
CLIs, in that order** (both halves are runnable by Claude itself; nothing
requires the human to touch a terminal). The plugin half is the one that historically got dropped (a real
audit found machines with every CLI installed and the sirius plugin missing),
and every other entry point lives inside the plugin — so it goes first.
PingMyBell is the optional fifth: the one-shot detects it and points at its
repo, never auto-installs it.

## Half 1 — the plugins (Claude runs these via Bash)

The CLIs are the binaries; the plugins are the Skills, commands, and MCP servers
Claude Code loads. **Run this half yourself via Bash** — Claude Code v2.1.195+
ships a NON-interactive `claude plugin` CLI, so no human terminal work is
needed and this works from a completely cold start on any surface, including
the desktop app:

```bash
claude plugin marketplace add Davidb3l/Sirius-Forester
claude plugin install sirius@sirius-forester
claude plugin install hayvenhurst@sirius-forester
claude plugin install catryna@sirius-forester
```

A plugin already installed from its own standalone marketplace
(`hayvenhurst@hayvenhurst`, `catryna@catryna-wikinelli`) counts — check with
`claude plugin list` and skip what's present. (Ametrite has no plugin here —
its `amt` CLI comes from the ametrite skill and Half 2, not from a plugin.)

Fallbacks, only if the `claude` CLI is missing or too old for non-interactive
plugin commands: tell the human to use the desktop app's plugin browser
(**+** next to the prompt box → Plugins → Add plugin), or the interactive
`/plugin` dialog in a terminal `claude` session. Plugins are per-machine —
any one route covers every surface. New sessions pick them up; the current
session may need a restart to see newly installed plugins.

## Half 2 — the CLIs (one shot)

Run the bundled one-shot installer. It's idempotent (anything already installed
is left alone) and delegates each binary to that tool's own authoritative,
security-reviewed installer — it never re-implements a download or a signature
check:

```bash
"${CLAUDE_PLUGIN_ROOT}/scripts/install-sothis.sh"
```

What it does, in order:

1. **sirius** — installs via the bundled `install-sirius.sh` (verifies a Sigstore
   signature; a bad or missing signature aborts).
2. **hayven** — installs via Hayvenhurst's own `install-hayven.sh` (verifies a
   sha256). It prefers a copy already on disk from an installed Hayvenhurst
   plugin, and only falls back to fetching that script over HTTPS from the
   Hayvenhurst repo. If you'd rather install hayven through its plugin, pass
   `--skip-hayven` and run `/hayvenhurst:install-binary`.
3. **amt** — detected only. If missing, the script prints how to get it; the
   fastest path is asking Claude to **"ametrite this repo"** (the ametrite skill
   bootstraps `amt`). It deliberately does not clone or `cargo build` for you.
4. **catryna** — checks that its plugin is installed and that `bun` (its MCP
   runtime) is present; warns if either is missing.
5. Runs **`sirius doctor`** — the suite's ground-truth health check.
6. Ends by **auto-installing any missing plugin half** via the non-interactive
   `claude plugin` CLI (skippable with `--skip-plugins`), then **verifying** it
   (bundle marketplace + the three plugins, matched by `<name>@` prefix so any
   marketplace counts). Only if something is STILL missing is its last output
   a `YOU ARE NOT DONE` block listing the remaining routes (shell commands,
   the desktop app's plugin browser, or `/plugin`).

Relay anything the script surfaces: a PATH note, a missing-`amt` hint, a missing
`bun` warning. **If it printed a `YOU ARE NOT DONE` block, relay that block
prominently and do NOT summarize the install as complete** — run the block's
listed `claude plugin` commands yourself (or point the human at the desktop
app's + → Plugins browser) and re-verify. **Never** work around a
`SIGNATURE VERIFICATION FAILED` error — stop and report it.

`/sirius:install-suite` is the slash-command entry to this same one-shot (forward
`--skip-hayven`, `--skip-amt`, `--require-signature`, `--prefix` if the user asks).

## Done when

- `install-sothis.sh --check` reports `sirius` and `hayven` present (exit 0),
  `amt` + `bun` present, and **`claude code plugin half: complete`**.
- `sirius doctor` is clean in a repo with a `.sirius/` workspace (run `sirius
  init` first if there isn't one) — including no `plugin_handoff` warning.

Then it's foreman time: **"let's get Sirius"** (the `sirius` skill) kicks off the
loop.
