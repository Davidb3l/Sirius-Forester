# Sirius Forester — Claude Code plugin

Ships the **Sirius worker Skill** (the claim → map → lock → brief → work → gate
→ receipt → release loop) plus an installer for the compiled `sirius` CLI.

## Install

```
/plugin marketplace add Davidb3l/Sirius-Forester
/plugin install sirius@sirius-forester
/sirius:install-binary
```

The plugin itself is git-based, so it can only carry text (the Skill, this
README). The compiled `sirius` binary is platform-specific and ships via GitHub
Releases; `/sirius:install-binary` downloads the tarball for your OS/arch,
verifies its sha256, and installs it into the plugin's persistent data
directory. A `SessionStart` hook reports whether the binary is present.

Windows: no installer script yet — download the `windows-x64` tarball from the
[releases page](https://github.com/Davidb3l/Sirius-Forester/releases), verify
the `.sha256`, and put `sirius.exe` on your `PATH`.

## The full suite — Sothis

Sirius is the **foreman** of **Sothis**, the local-first suite: it claims work
from an [Ametrite](https://github.com/Davidb3l/Ametrite) board, locks code
through a [Hayvenhurst](https://github.com/Davidb3l/Hayvenhurst-dev) code graph,
and pairs with [Catryna Wikinelli](https://github.com/Davidb3l/Catryna-Wikinelli)
for living "why" docs. Each tool stands alone, but full fleet control comes from
running all four.

Installing the suite has two halves. **The plugins** — this marketplace is a
bundle: one add exposes all three (Ametrite's `amt` is a CLI bootstrapped by its
own "ametrite this repo" skill, not a plugin):

```
/plugin marketplace add Davidb3l/Sirius-Forester
/plugin install sirius@sirius-forester
/plugin install hayvenhurst@sirius-forester
/plugin install catryna@sirius-forester
```

**The CLIs** — run `/sirius:install-suite` (or say "let's Sothis this up"). It
installs every missing suite binary by delegating to each tool's own installer,
detects `amt`, checks `bun` for Catryna, and ends with `sirius doctor`. To
install only the sirius binary, use `/sirius:install-binary`.
