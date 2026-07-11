---
name: sothis
description: >-
  Install the whole Sothis suite — the local-first fleet led by Sirius Forester:
  Sirius (foreman), Hayvenhurst (code graph), Ametrite (board), Catryna Wikinelli
  (docs). Trigger when the human says "let's Sothis this up", "let's Sothis up",
  "install the Sothis suite", "set up the whole suite/fleet", "get the full fleet
  on this repo", or otherwise asks for all the tools at once (not just Sirius).
  Two halves: run the install-sothis.sh one-shot for the CLIs, then the
  marketplace bundle for the interactive `/plugin` adds. To install ONLY the
  sirius binary, use /sirius:install-binary; to just RUN the foreman once it's
  installed, that's the `sirius` skill.
---

# Installing the Sothis suite — "let's Sothis this up"

**Sothis** is the local-first suite led by Sirius Forester. Four standalone
tools that compose through the suite contracts:

- **Sirius Forester** (`sirius`) — the foreman/loop. Signed prebuilt binary.
- **Hayvenhurst** (`hayven`) — the code graph. Prebuilt binary.
- **Ametrite** (`amt`) — the task board + decisions. Rust binary (cargo).
- **Catryna Wikinelli** (`catryna`) — the living "why" docs. bun-based MCP plugin.

Each stands alone; **full fleet control needs all four.** They install four
different ways, and two of the steps are interactive `/plugin` commands, so the
install has two halves. Do both.

## Half 1 — the CLIs (one shot)

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
5. Ends with **`sirius doctor`** — the suite's ground-truth health check.

Relay anything the script surfaces: a PATH note, a missing-`amt` hint, a missing
`bun` warning. **Never** work around a `SIGNATURE VERIFICATION FAILED` error —
stop and report it.

`/sirius:install-suite` is the slash-command entry to this same one-shot (forward
`--skip-hayven`, `--skip-amt`, `--require-signature`, `--prefix` if the user asks).

## Half 2 — the plugins (interactive `/plugin`)

The CLIs are the binaries; the plugins are the Skills, commands, and MCP servers
Claude Code loads. One marketplace add exposes all three plugins in the Sothis
bundle:

```
/plugin marketplace add Davidb3l/Sirius-Forester
/plugin install sirius@sirius-forester
/plugin install hayvenhurst@sirius-forester
/plugin install catryna@sirius-forester
```

(Ametrite has no plugin here — its `amt` CLI comes from the ametrite skill and
Half 1, not from a `/plugin install`.)

If you're driving Claude Code interactively you can run these yourself; otherwise
hand them to the human — `/plugin` commands can't be run from a script.

## Done when

- `install-sothis.sh --check` reports `sirius` and `hayven` present (exit 0), and
  `amt` + `bun` present.
- The three plugins are installed.
- `sirius doctor` is clean in a repo with a `.sirius/` workspace (run `sirius
  init` first if there isn't one).

Then it's foreman time: **"let's get Sirius"** (the `sirius` skill) kicks off the
loop.
