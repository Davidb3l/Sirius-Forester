# Sirius Forester

**Sirius Forester is the foreman of the Sothis suite — a local-first fleet
orchestrator for Claude Code and other AI coding agents: it claims tasks from a
local issue tracker, locks the code each task touches, runs your agent, refuses
to mark anything done until the affected tests pass, and files a two-way receipt
linking every change to the decision behind it.**

> **Part of the [Sothis suite](https://github.com/Davidb3l/Sothis)** — the
> local-first fleet for Claude Code agents:
> **Sirius Forester** (foreman) ·
> [Hayvenhurst](https://hayvenhurst.dev) (code graph) ·
> [Ametrite](https://ametrite.com) (board) ·
> [Catryna Wikinelli](https://catrynawiki.com) (docs) ·
> [PingMyBell](https://github.com/Davidb3l/pingmybell) (the bell)

Website & docs: **[siriusforester.com](https://siriusforester.com)** ·
[Getting started](https://siriusforester.com/docs/getting-started/)

Status: **early alpha (v0.1.7).** The `sirius` binary and all nine commands below
are implemented and covered by an offline test suite (`cargo test`); CI runs on
macOS/Linux/Windows. Prebuilt binaries are published for five platforms on the
[Releases](https://github.com/Davidb3l/Sirius-Forester/releases) page, each with
a sha256 checksum and a Sigstore signature. It depends on two companion tools,
both public: [Hayvenhurst](https://github.com/Davidb3l/Hayvenhurst-dev) (a local
code graph) and [Ametrite](https://github.com/Davidb3l/Ametrite) (a local issue
tracker). With both installed the full loop runs end to end.

## Why

Run two or three coding agents against one repo and the same failures show up
fast: two agents grab the same task, edits collide silently, an agent declares
"done" without running the tests its change actually affects, and a week later
nobody can say why a function changed. Sirius is the supervisor loop that
enforces: one claim per task, one lock per symbol, tests gate every completion,
and every completion leaves a receipt. All on your machine — SQLite ledger, no
cloud, no accounts, and no LLM calls of its own (agents bring their own model).

It is **not** an issue tracker, a code graph, CI, or a merge tool. It writes
only its own ledger (`.sirius/sirius.db`); it talks to Ametrite and Hayvenhurst
strictly through their CLIs.

## Requirements

- **Rust** ≥ 1.74, only to build the `sirius` binary from source. The prebuilt
  binaries need no toolchain.
- **[Ametrite](https://github.com/Davidb3l/Ametrite)** (`amt` CLI, schema ≥ v3)
  — the issue tracker it claims from.
- **[Hayvenhurst](https://github.com/Davidb3l/Hayvenhurst-dev)** (`hayven` CLI,
  daemon on `:7777`) — the code graph used for locking, test selection, and
  provenance stamps.
- **[Bun](https://bun.sh)** (optional) — only for the web console and `bench/`.

## Install

Every release attaches a tarball per platform (`macos-arm64`, `macos-x64`,
`linux-x64-glibc`, `linux-arm64`, `windows-x64`), each alongside a `.sha256`
checksum and a Sigstore signature bundle.

**Claude Code plugin.** Run `/sirius:install-binary`. It picks the tarball for
this machine's OS and CPU, verifies the checksum and the Sigstore signature,
and installs `sirius`.

**The whole suite (Sothis).** Sirius is the foreman of **Sothis** — the
local-first suite of Sirius, Hayvenhurst, Ametrite, and Catryna Wikinelli
(plus PingMyBell, the optional bell). The install is two halves — the Claude
Code **plugins**, then the **CLIs** — and only the first rung depends on where
you are, because every richer entry point lives inside the plugin you haven't
installed yet:

**First rung — get the `sirius` plugin (pick ONE):**

- **Desktop app, zero terminal:** click **+** next to the prompt box →
  **Plugins** → **Add plugin** → add the `Davidb3l/Sirius-Forester`
  marketplace and install `sirius`.
- **Any shell** (or just ask Claude to run it — it's non-interactive,
  Claude Code ≥ 2.1.195):

  ```bash
  claude plugin marketplace add Davidb3l/Sirius-Forester
  claude plugin install sirius@sirius-forester
  ```

- **Terminal session:** the interactive `/plugin` dialog, same two steps.

**Everything else — one sentence:** say **"let's Sothis this up"** (or run
`/sirius:install-suite`). It installs every missing suite CLI via each tool's
own installer, detects `amt`, checks `bun` for Catryna, runs `sirius doctor`,
**auto-installs any missing plugins** via the non-interactive `claude plugin`
CLI, and verifies the result — a half-done install says `YOU ARE NOT DONE`
loudly instead of looking finished. `sirius doctor` reports the plugin half as
an advisory check with exact fix commands, and plugins are per-machine, so one
install covers the app, the terminal, and every repo.

**Team repos:** commit this to the repo's `.claude/settings.json` and everyone
who opens the folder gets a trust prompt that installs the suite plugins for
them — no commands at all:

```json
{
  "extraKnownMarketplaces": {
    "sirius-forester": {
      "source": { "source": "github", "repo": "Davidb3l/Sirius-Forester" }
    }
  },
  "enabledPlugins": {
    "sirius@sirius-forester": true,
    "hayvenhurst@sirius-forester": true,
    "catryna@sirius-forester": true
  }
}
```

### Verifying what you downloaded

The `.sha256` is served from the same origin as the tarball, so by itself it
only catches a corrupted download: anyone who could replace the tarball could
replace its checksum too. Authenticity comes from the Sigstore bundle, whose
certificate binds the artifact to this repo's release workflow.

`install-sirius.sh` verifies the signature whenever [`cosign`][cosign] or
[`sigstore`][sigstore] is installed. Two rules are absolute: **a bad signature
aborts the install**, and **a missing bundle aborts the install**. The second
matters as much as the first: an attacker who can serve a tampered tarball can
also serve a 404 for its signature, and a "skip when absent" policy would hand
them a free downgrade.

The one soft case is a machine with neither verifier installed. There we cannot
check, so the installer warns loudly and proceeds on TLS plus the checksum. No
attacker can induce that state remotely, since it depends on what you have
installed locally. Pass `--require-signature` (or set
`SIRIUS_REQUIRE_SIGNATURE=1`) to make it fatal:

```bash
brew install cosign            # or: pip install sigstore
./install-sirius.sh --require-signature
```

To check a download by hand, pin both the signer identity and the OIDC issuer
(an unpinned verify only proves *somebody* signed it):

```bash
VERSION=0.1.7; PLATFORM=macos-arm64
STEM="sirius-forester-$VERSION-$PLATFORM"
cosign verify-blob \
  --bundle "$STEM.tar.gz.sigstore.json" \
  --certificate-identity "https://github.com/Davidb3l/Sirius-Forester/.github/workflows/release.yml@refs/tags/v$VERSION" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  "$STEM.tar.gz"
# Verified OK
```

[cosign]: https://github.com/sigstore/cosign
[sigstore]: https://pypi.org/project/sigstore/

**Manual (macOS / Linux).** Download, verify, install:

```bash
VERSION=0.1.7
PLATFORM=macos-arm64             # or macos-x64, linux-x64-glibc, linux-arm64
BASE="https://github.com/Davidb3l/Sirius-Forester/releases/download/v$VERSION"
STEM="sirius-forester-$VERSION-$PLATFORM"

curl -fsSLO "$BASE/$STEM.tar.gz"
curl -fsSLO "$BASE/$STEM.tar.gz.sha256"
shasum -a 256 -c "$STEM.tar.gz.sha256"   # prints: <file>: OK
tar -xzf "$STEM.tar.gz"
install "$STEM/sirius" /usr/local/bin/   # anywhere on your PATH
```

**Manual (Windows).** Download `sirius-forester-<version>-windows-x64.tar.gz`
and its `.sha256` from the releases page, check the hash, then extract it and
put the binary, which is named **`sirius.exe`**, anywhere on your `%PATH%`:

```powershell
$Version  = "0.1.7"
$Stem     = "sirius-forester-$Version-windows-x64"
$Base     = "https://github.com/Davidb3l/Sirius-Forester/releases/download/v$Version"

curl.exe -fsSLO "$Base/$Stem.tar.gz"
curl.exe -fsSLO "$Base/$Stem.tar.gz.sha256"
# compare against the hash in the .sha256 file
(Get-FileHash "$Stem.tar.gz" -Algorithm SHA256).Hash.ToLower()
tar -xzf "$Stem.tar.gz"
# then move $Stem\sirius.exe onto your %PATH%
```

**From source.** Needs Rust ≥ 1.74:

```bash
git clone https://github.com/Davidb3l/Sirius-Forester && cd Sirius-Forester
cargo install --path .           # puts `sirius` on your PATH
# (or: cargo build --release  → target/release/sirius)
```

## Quickstart

```bash
cd /path/to/your/repo            # one that has .ametrite/ and .hayven/
sirius init                      # creates .sirius/{sirius.db,config.json}
sirius doctor                    # verifies the integration contracts, live
```

`sirius doctor` checks the five facts Sirius depends on — plus an advisory
`plugin_handoff` check that warns (never fails) when the Claude Code plugin
half of the suite is missing, naming the exact `claude plugin` commands to finish.
It tells you exactly
what is missing:

```
[OK] amt_present_and_schema — amt 0.1.0, ametrite schema v4 (>= v3)
[FAIL] hayven_daemon_7777 — no 200 from http://localhost:7777; .hayven/ present
...
CONTRACT DRIFT DETECTED
```

Then set the one config value the gate needs — your full-suite test command —
in `.sirius/config.json` (the gate **fails closed** without it):

```json
{ "gate": { "test_cmd": "cargo test" } }
```

## Usage

Every command takes `--json` (one JSON object on stdout, logs on stderr).
Exit codes: `0` ok, `1` failure, `2` usage error, `3` gate blocked.

**`sirius link`** — file a receipt by hand (useful with zero agents running).
Stamps the symbols onto the issue (via `amt issue comment`) and the issue onto
each code node (via `hayven remember`):

```bash
sirius link AMT-7 --symbols auth::verify,auth::mint
sirius link AMT-7 --changed --range main..HEAD   # resolve symbols from git
sirius link --decision D-3 --symbols auth::mint
# linked issue AMT-7 → 2 symbols (forward: true, reverse: true)
```

**`sirius why`** — read provenance in either direction:

```bash
sirius why auth::verify    # → the issues and decisions behind this symbol
sirius why AMT-7           # → the symbols and decisions this issue touched
```

**`sirius gate`** — test-gate a completion (works for humans and CI, not just
agents). `hayven affected-tests` *selects* the tests for the changed files;
Sirius *runs* them via your `gate.test_cmd`, and runs the full suite whenever
the selection can't be trusted. Pass advances the issue's status via `amt`;
fail files the failure as an issue comment and exits `3`:

```bash
sirius gate AMT-7 --tier safe --target-status in_review
# gate safe for AMT-7: PASS [subset(3)] (3 tests) → in_review
```

**`sirius run`** — the loop. Each iteration: claim an issue → map it to symbols
→ lock them in Hayvenhurst → run your agent command (`sh -c`, progress-aware
timeout, lease heartbeats, output captured to a log) → gate → file the receipt
→ release. Claim order is enforced (issue first, symbols second, release in
reverse); a lock collision releases the issue back with a comment naming the
blocker. Streams NDJSON events, one per phase:

```bash
sirius run --workers 3 --agent-cmd 'claude -p "fix the claimed issue"' --from todo
# {"event":"iteration","worker":"sirius/oak","phase":"claim","issue":"AMT-12","claimed":true,...}
# {"event":"iteration","worker":"sirius/oak","phase":"gate","issue":"AMT-12",...}
# {"event":"iteration","worker":"sirius/oak","phase":"release","issue":"AMT-12","status":"in_review","advanced":true}
```

**Timeouts** (SIRF-41). An agent is killed only when it is *idle* — no
output and no worktree change (no file write, no commit) for
`timeouts.idle_secs` (default 1800) — or when it hits the hard cap
`timeouts.hard_secs` (default 10800, 3 h). An agent that committed in the last
minute before a kill gets one 60 s grace to finish. Big tickets can get more
room by Ametrite label, first match wins:

```json
"timeouts": {"idle_secs": 1800, "hard_secs": 10800,
             "routes": [{"labels": ["ui", "epic"], "hard_secs": 14400}]}
```

The old `agent_timeout_secs` still works as the hard cap, except the value
`1800` that `sirius init` used to write, which is ignored. `sirius doctor`
warns when a hard cap is under an hour or an idle window under 5 minutes. The
work event and release comment say which limit fired (`timeout_kind`:
`idle` | `hard`). Agents get `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0`, so
`claude -p` never kills a worker's helper agents after 600 s. An exit where it
did ("Background tasks still running … terminating") counts as incomplete,
whatever the exit code.

**Review stage** (recommended): add `--review-cmd` (or `review.cmd` in
`.sirius/config.json`) and every gated diff gets an unbiased fresh-eyes review
before it advances. A separate reviewer process — no access to the worker's
session — reads the issue, the diff, and the repo, by default merged onto the
*current* base so clashes with work merged mid-run surface too. Confirmed bugs
send the worker back in fix mode, then re-gate and re-review, until the review
is clean or `review.max_rounds` is spent (then `advance-flagged`: it advances
with a `review:open` label and every open finding in a comment). The reviewer is
read-only — a review that edits the tree is discarded. Doc-only diffs
(`review.skip_paths`) skip it. Contract: [`CONTRACTS.md` §3.1](CONTRACTS.md).

```bash
sirius run --workers 2 --from todo \
  --agent-cmd 'claude -p "work issue {issue} the Sirius way"' \
  --review-cmd 'claude -p "$(cat "$SIRIUS_REVIEW_PROMPT")" --allowedTools "Bash(git diff:*)" "Bash(git log:*)" "Bash(git show:*)" "Bash(amt issue show:*)" Read Grep Glob'
```

**Models** (SIRF-26): pass `--model <exact id>` (and ideally a different
`--review-model`). `sirius run` refuses to launch with no model — otherwise
every Claude worker silently inherits the global `~/.claude/settings.json`
default. The model reaches agents as `ANTHROPIC_MODEL` (Claude Code honors it
over settings.json) plus `SIRIUS_MODEL` and a `{model}` placeholder. Tickets
can be routed to models by Ametrite label (`models.routes`; fix rounds use at
least `models.fix_floor`), and the resolved model is in every `claim` event and
in `sirius doctor`. If any agent hits a usage/plan limit (or a model the
CLI is too old to run), the whole fleet switches to an optional **fallback
tier** (`models.fallback`) and retries in place — e.g. Opus implements and Fable
reviews, and when the Fable allotment runs out Sonnet implements and Opus
reviews. With no fallback left, it **pauses** (exit `4`) instead of bouncing the
board; a logged-out CLI always pauses.

**Review what will actually land** (SIRF-29–32). Every defect that escaped the
first large fleet run lived *between* branches — two migrations on the same
parent, two features each correct and inconsistent together. So Sirius keeps an
**integration frontier**: the current base tip plus every in-flight `sirius/*`
branch awaiting integration, merged in order (a speculative merge queue).

- `review.against: "frontier"` reviews each change merged onto that frontier;
  the reviewer is told what else is in flight (`$SIRIUS_SIBLINGS`), and a
  conflict with a sibling is named (`sibling-conflict`) instead of discovered
  at merge time.
- `review.sequences: [{"dir": "drizzle"}]` checks migration-style directories
  mechanically: a new entry must come after the base's last one and must not
  share a slot with a sibling's. These `AUTO-` findings are facts — no
  reviewer verdict or rebuttal can close them.
- `sirius integrate` builds the frontier (`refs/sirius/frontier`) and runs
  `integration.cmd` (e.g. your e2e suite) on it **before** anything merges.
  Red files one issue naming the combined branches; with
  `integration.on_fail: "block"` the fleet stops (exit 4) and refuses to
  relaunch (exit 3) until `sirius integrate` is green — or a human runs
  `sirius integrate --clear-red`. Work finished meanwhile is held, then resumed.
- The reviewer always runs in a throwaway worktree, so its hooks and test runs
  never touch the worker's tree (one checkout per review round — seconds on a
  large repo; ignored files such as `node_modules` are not in it).

**Learn from what escapes** (SIRF-35). A defect that got past review is the
most valuable data a review system has. Record it against the issue that
shipped it:

```bash
sirius escape LYD-52 --kind migration-fork -m "two branches both took slot 0042" --found-by e2e --fix <fix-sha>
```

Every later review prompt lists the repo's live escape patterns. A kind that
escapes twice is nudged toward a real check; once one exists,
`sirius escape --kind migration-fork --automated-by tests/migrations.rs`
retires it from the prompt. And every escape with a fix commit becomes a
**canary**: `sirius review-canary` reverts each fix onto the current base (the
real bug, back) — plus any `.sirius/canaries/*.patch` you write — runs your
reviewer on it exactly as a review round would, and reports **recall** on real
misses, with a benign control change for false positives. Canaries are blind
on every channel Sirius controls (neutral issue key, no fix history, no
same-kind escape in the prompt). Run it whenever you change the reviewer model
or prompt.

The reviewer needs only **read** tools: it delivers its findings as JSON in its
final message (headless `claude -p` is not allowed to write files by default —
and a reviewer that cannot write cannot tamper). The built-in adversarial
prompt ships with each release; a file at `review.prompt_file`
(`.sirius/review-prompt.md`) overrides it.

Workers run as parallel threads in one killable foreground process, each in
its own private git worktree (`.sirius/worktrees/<worker>`) with each issue's
work on its own `sirius/<issue>` branch; a worker exits when the board is dry
and the process exits when all workers have. Policies (claim mode, 409 backoff, retry budget,
timeouts) live in `.sirius/config.json`; `sirius init` writes the defaults.

## Console and benchmarks

A local web console (Bun, zero npm runtime deps, port `:1777`) shows the
fleet board, receipts, and history — for **every** fleet on the machine, with
a switcher (running fleets first), like the Ametrite board. Try it with fixture data, no binary needed:

```bash
cd web && bun run demo    # → http://localhost:1777
```

`bench/` holds the harnesses behind every quantitative claim (claim integrity,
gate-escape rate, provenance coverage). They run offline in fixture mode:
`bun run bench/soak.ts` etc. See [`bench/README.md`](bench/README.md).

## Docs

- [`docs/architecture.md`](docs/architecture.md) — how the pieces fit.
- [`CONTRACTS.md`](CONTRACTS.md) — the CLI surface, ledger schema, and the
  integration contracts `sirius doctor` enforces.
- [`web/README.md`](web/README.md) — the console.
- [`AGENTS.md`](AGENTS.md) — etiquette for agents working in this repo.

## License

MIT. See [LICENSE](LICENSE).
