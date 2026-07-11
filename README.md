# Sirius Forester

**A local-first foreman for AI coding agents: it claims tasks from a local issue
tracker, locks the code each task touches, runs your agent, refuses to mark
anything done until the affected tests pass, and files a two-way receipt linking
every change to the decision behind it.**

Website & docs: **[siriusforester.com](https://siriusforester.com)** ·
[Getting started](https://siriusforester.com/docs/getting-started/)

Status: **early alpha (v0.1.0).** The `sirius` binary and all six commands below
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
local-first suite of Sirius, Hayvenhurst, Ametrite, and Catryna Wikinelli. This
repo's Claude Code marketplace is a bundle: `/plugin marketplace add
Davidb3l/Sirius-Forester` exposes the `sirius`, `hayvenhurst`, and `catryna`
plugins at once. For the CLIs, run `/sirius:install-suite` (or say "let's Sothis
this up") — it installs every missing suite binary via each tool's own installer,
detects `amt`, checks `bun` for Catryna, and ends with `sirius doctor`.

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
VERSION=0.1.0; PLATFORM=macos-arm64
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
VERSION=0.1.0
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
$Version  = "0.1.0"
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

`sirius doctor` checks the five facts Sirius depends on and tells you exactly
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
→ lock them in Hayvenhurst → run your agent command (`sh -c`, wall-clock
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

v1 runs workers sequentially in one killable foreground process and exits when
a full round finds no work. Policies (claim mode, 409 backoff, retry budget,
timeouts) live in `.sirius/config.json`; `sirius init` writes the defaults.

## Console and benchmarks

A local web console (Bun, zero npm runtime deps, port `:1777`) shows the
fleet board, receipts, and history. Try it with fixture data, no binary needed:

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
