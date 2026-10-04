# Sirius Forester — Claude Code plugin

Ships the **Sirius worker Skill** (the claim → map → lock → brief → work → gate
→ receipt → release loop) plus an installer for the compiled `sirius` CLI.

## Install

```
claude plugin marketplace add Davidb3l/Sirius-Forester   # any shell — or use the
claude plugin install sirius@sirius-forester             # app's + → Plugins browser
/sirius:install-binary
```

The plugin itself is git-based, so it can only carry text (the Skill, this
README). The compiled `sirius` binary is platform-specific and ships via GitHub
Releases; `/sirius:install-binary` downloads the tarball for your OS/arch,
verifies its sha256, and installs it into the plugin's persistent data
directory. A `SessionStart` hook reports whether the binary is present and — in
repos that use the suite, checked at most once a day — when a newer release is
out (re-run `/sirius:install-binary` to update; updating the plugin alone
updates the Skill text, not the binary).

## Slash commands

Everything works the same in the terminal and in the Claude app's Code tab —
ask in plain words ("run the integration check", "a bug got past review") or
use the commands:

| Command | What it does |
|---|---|
| `/sirius:install-binary [vX.Y.Z]` | install or update the `sirius` CLI |
| `/sirius:install-suite` | install the whole Sothis suite |
| `/sirius:integrate [--clear-red]` | build the integration frontier (base + every in-flight branch) and run your integration command on it before anything merges |
| `/sirius:escape <ISSUE> <what escaped>` | record a bug that got past review, so later reviews check for it; `--list`, or retire a kind with `--kind <slug> --automated-by <path>` |
| `/sirius:review-canary [--n N]` | measure the reviewer: replay recorded escapes (the real bugs, back) and report recall and false positives |

Starting the fleet itself is a conversation, not a command: "let's get Sirius on
this repo" — the Skill walks through the launch.

Windows: no installer script yet — download the `windows-x64` tarball from the
[releases page](https://github.com/Davidb3l/Sirius-Forester/releases), verify
the `.sha256`, and put `sirius.exe` on your `PATH`.

## The full suite — Sothis

Sirius is the **foreman** of **Sothis**, the local-first suite: it claims work
from an [Ametrite](https://github.com/Davidb3l/Ametrite) board, locks code
through a [Hayvenhurst](https://github.com/Davidb3l/Hayvenhurst-dev) code graph,
pairs with [Catryna Wikinelli](https://github.com/Davidb3l/Catryna-Wikinelli)
for living "why" docs, and rings
[PingMyBell](https://github.com/Davidb3l/pingmybell) — the optional desktop
notch/voice app — when the fleet needs you. Each tool stands alone, but full
fleet control comes from running the four CLIs.

Installing the suite has two halves. **The plugins** — this marketplace is a
bundle: one add exposes all three (Ametrite's `amt` is a CLI bootstrapped by its
own "ametrite this repo" skill, not a plugin):

```
claude plugin marketplace add Davidb3l/Sirius-Forester
claude plugin install sirius@sirius-forester
claude plugin install hayvenhurst@sirius-forester
claude plugin install catryna@sirius-forester
```

(Runnable from any shell — Claude can run them for you. Desktop-app
alternative: **+** next to the prompt box → Plugins → Add plugin. The
interactive `/plugin` dialog in a terminal session works too.)

**The CLIs** — run `/sirius:install-suite` (or say "let's Sothis this up"). It
installs every missing suite binary by delegating to each tool's own installer,
detects `amt`, checks `bun` for Catryna, runs `sirius doctor`, and ends by
verifying the plugin half above — if the plugin half is still incomplete, its last
output is a `YOU ARE NOT DONE` block naming exactly what's left. To
install only the sirius binary, use `/sirius:install-binary`.
