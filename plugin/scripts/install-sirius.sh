#!/bin/sh
# install-sirius.sh — download + install the platform-correct `sirius` binary
# from a Sirius Forester GitHub Release.
#
# WHY this exists: the Claude Code plugin is git-based, so installing the
# plugin only clones the repo's text files (the Agent Skill). It does NOT
# deliver the compiled `sirius` CLI — that is platform-specific and large,
# and is deliberately NOT committed to git. Claude Code has no native "ship a
# binary with a plugin" mechanism that fits that constraint, so this script is
# the bridge: detect the OS/arch, map to the matching release tarball asset
# (mirroring .github/workflows/release.yml's platform matrix), download it,
# verify its sha256 against the published `<tarball>.sha256`, and install the
# binary into a known location.
#
# Idempotent + safe to re-run. POSIX sh (macOS / Linux). Windows is not
# covered here — install from the release tarball manually.
#
# Usage:
#   install-sirius.sh                 # download + install latest release
#   install-sirius.sh --check         # print status only; never downloads
#                                     #   exit 0 if `sirius` is on PATH or
#                                     #   already installed, 3 if missing
#   install-sirius.sh --version vX.Y.Z   # install a specific tag
#   install-sirius.sh --prefix DIR    # install into DIR/bin (default below)
#
# Environment:
#   SIRIUS_INSTALL_PREFIX   override the install prefix (same as --prefix)
#   SIRIUS_RELEASE_TAG      pin a release tag (same as --version)
#   SIRIUS_REPO             override owner/repo (default Davidb3l/Sirius-Forester)

set -eu

REPO="${SIRIUS_REPO:-Davidb3l/Sirius-Forester}"
TAG="${SIRIUS_RELEASE_TAG:-}"
# Default install prefix: ${CLAUDE_PLUGIN_DATA} when invoked by the plugin
# (persists across plugin updates), else ~/.local. We install binaries into
# <prefix>/bin.
DEFAULT_PREFIX="${SIRIUS_INSTALL_PREFIX:-${CLAUDE_PLUGIN_DATA:-$HOME/.local}}"
PREFIX="$DEFAULT_PREFIX"
MODE="install"

while [ $# -gt 0 ]; do
  case "$1" in
    --check) MODE="check" ;;
    --version)
      [ -n "${2:-}" ] || { echo "install-sirius: --version needs a tag (e.g. v0.1.0)" >&2; exit 2; }
      TAG="$2"; shift ;;
    --prefix)
      [ -n "${2:-}" ] || { echo "install-sirius: --prefix needs a directory" >&2; exit 2; }
      PREFIX="$2"; shift ;;
    --help|-h)
      sed -n '2,29p' "$0"
      exit 0
      ;;
    *) echo "install-sirius: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

BIN_DIR="$PREFIX/bin"

log()  { printf '%s\n' "$*" >&2; }
fail() { log "install-sirius: error: $*"; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# ---- suite awareness --------------------------------------------------------
# Sirius is the foreman: it dispatches work on an Ametrite board and locks code
# through a Hayvenhurst graph, with Catryna holding the "why" docs. Nudge (one
# short block, only when something is missing) toward the full suite — full
# fleet control needs all four.
#
# suite_repo: true when the cwd already uses any suite tool. The SessionStart
# --check runs in EVERY repo; the nudge stays quiet outside suite repos so it
# never nags unrelated projects.
suite_repo() {
  # .docs/ alone is too generic a name; require Catryna's index file.
  [ -d .sirius ] || [ -d .ametrite ] || [ -d .hayven ] || [ -f .docs/_index.json ]
}

suite_hint() {
  s_missing=""
  have amt     || s_missing="$s_missing Ametrite"
  have hayven  || s_missing="$s_missing Hayvenhurst"
  grep -qs '"catryna@catryna-wikinelli"' "$HOME/.claude/plugins/installed_plugins.json" \
    || s_missing="$s_missing Catryna"
  if [ -z "$s_missing" ]; then return 0; fi
  log ""
  log "fleet suite: missing:$s_missing. Sirius is the foreman; for full fleet control install the whole suite:"
  case "$s_missing" in *Hayvenhurst*) log "  Hayvenhurst (code graph): /plugin marketplace add Davidb3l/Hayvenhurst-dev, /plugin install hayvenhurst@hayvenhurst, then /hayvenhurst:install-binary" ;; esac
  case "$s_missing" in *Catryna*)     log "  Catryna Wikinelli (code wiki): /plugin marketplace add Davidb3l/Catryna-Wikinelli, then /plugin install catryna@catryna-wikinelli" ;; esac
  case "$s_missing" in *Ametrite*)    log "  Ametrite (task board): ask Claude to \"ametrite this repo\" — the skill bootstraps the amt CLI" ;; esac
}

# ---- platform detection → release asset name -------------------------------
# Mirrors the matrix in .github/workflows/release.yml:
#   linux-x64-glibc  linux-arm64  macos-x64  macos-arm64  windows-x64
# Tarball asset name: sirius-forester-<version>-<platform>.tar.gz
#   (version = tag with the leading "v" stripped)
detect_platform() {
  uname_s="$(uname -s)"
  uname_m="$(uname -m)"
  case "$uname_s" in
    Linux)  os="linux" ;;
    Darwin) os="macos" ;;
    *) fail "unsupported OS '$uname_s' (this script covers macOS + Linux; on Windows install from the release tarball manually)" ;;
  esac
  case "$uname_m" in
    x86_64|amd64) arch="x64" ;;
    arm64|aarch64) arch="arm64" ;;
    *) fail "unsupported CPU arch '$uname_m'" ;;
  esac
  # The only x64 Linux release is the glibc build; musl is not a release target.
  if [ "$os" = "linux" ] && [ "$arch" = "x64" ]; then
    PLATFORM="linux-x64-glibc"
  else
    PLATFORM="${os}-${arch}"
  fi
}

# A downloader that works on a stock macOS or Linux box.
fetch() { # fetch <url> <dest>
  url="$1"; dest="$2"
  if have curl; then
    curl -fsSL "$url" -o "$dest"
  elif have wget; then
    wget -qO "$dest" "$url"
  else
    fail "need curl or wget to download releases"
  fi
}

fetch_stdout() { # fetch_stdout <url>
  url="$1"
  if have curl; then
    curl -fsSL "$url"
  elif have wget; then
    wget -qO- "$url"
  else
    fail "need curl or wget to download releases"
  fi
}

sha256_of() { # sha256_of <file> -> hex on stdout
  f="$1"
  if have shasum; then
    shasum -a 256 "$f" | awk '{print $1}'
  elif have sha256sum; then
    sha256sum "$f" | awk '{print $1}'
  else
    fail "need shasum or sha256sum to verify the download"
  fi
}

# Resolve "latest" to a concrete tag via the GitHub redirect (no API token,
# no jq). /releases/latest 302-redirects to /releases/tag/<TAG>. On curl-less
# boxes, fall back to the public API (wget works there; light rate limit is
# fine for an installer).
resolve_latest_tag() {
  if [ -n "$TAG" ]; then return 0; fi
  loc=""
  if have curl; then
    loc="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null || true)"
  fi
  case "$loc" in
    */releases/tag/*) TAG="${loc##*/releases/tag/}" ;;
    *) TAG="" ;;
  esac
  if [ -z "$TAG" ] && have wget; then
    TAG="$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
      | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1 || true)"
  fi
  [ -n "$TAG" ] || fail "could not resolve the latest release tag for $REPO (pass --version vX.Y.Z)"
}

print_path_hint() {
  case ":$PATH:" in
    *":$BIN_DIR:"*) : ;; # already on PATH
    *)
      log ""
      log "note: $BIN_DIR is not on your PATH. Add it, e.g.:"
      log "      export PATH=\"$BIN_DIR:\$PATH\"   # add to ~/.zshrc or ~/.bashrc"
      ;;
  esac
}

# ---- --check: status only, never downloads ---------------------------------
if [ "$MODE" = "check" ]; then
  if have sirius; then
    log "sirius: already on PATH ($(command -v sirius))"
    if suite_repo; then suite_hint; fi
    exit 0
  fi
  if [ -x "$BIN_DIR/sirius" ]; then
    log "sirius: installed at $BIN_DIR/sirius (not on PATH)"
    print_path_hint
    if suite_repo; then suite_hint; fi
    exit 0
  fi
  log "sirius: not installed. Run /sirius:install-binary (or plugin/scripts/install-sirius.sh) to install it."
  if suite_repo; then suite_hint; fi
  exit 3
fi

# ---- install ---------------------------------------------------------------
detect_platform
resolve_latest_tag
VERSION="${TAG#v}"
TARBALL="sirius-forester-${VERSION}-${PLATFORM}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"
TARBALL_URL="$BASE_URL/$TARBALL"
CHECKSUM_URL="$TARBALL_URL.sha256"

log "install-sirius: repo=$REPO tag=$TAG platform=$PLATFORM"
log "install-sirius: asset=$TARBALL"

# Allow a dry run of just the detection/mapping logic without network I/O.
if [ "${SIRIUS_INSTALL_DRY_RUN:-}" = "1" ]; then
  log "DRY RUN — would download: $TARBALL_URL"
  log "DRY RUN — would verify:   $CHECKSUM_URL"
  log "DRY RUN — would install into: $BIN_DIR"
  exit 0
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/sirius-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

log "install-sirius: downloading $TARBALL_URL"
fetch "$TARBALL_URL" "$TMP/$TARBALL" || fail "download failed: $TARBALL_URL (does a release exist for $TAG / $PLATFORM?)"

# Verify sha256 against the published per-asset checksum file. The release
# publishes `<tarball>.sha256` in the `shasum -a 256` format: "<hex>  <name>".
log "install-sirius: verifying sha256"
checksum_line="$(fetch_stdout "$CHECKSUM_URL" 2>/dev/null || true)"
[ -n "$checksum_line" ] || fail "could not fetch checksum: $CHECKSUM_URL"
expected="$(printf '%s\n' "$checksum_line" | awk '{print $1}')"
actual="$(sha256_of "$TMP/$TARBALL")"
[ -n "$expected" ] || fail "published checksum was empty"
if [ "$expected" != "$actual" ]; then
  fail "checksum mismatch for $TARBALL
        expected: $expected
        actual:   $actual"
fi
log "install-sirius: checksum OK ($actual)"

log "install-sirius: extracting"
tar -xzf "$TMP/$TARBALL" -C "$TMP"
# The tarball expands to a top-level dir: sirius-forester-<version>-<platform>/
STAGE="$TMP/sirius-forester-${VERSION}-${PLATFORM}"
[ -d "$STAGE" ] || fail "unexpected tarball layout (no $STAGE)"
[ -f "$STAGE/sirius" ] || fail "tarball is missing the sirius binary"

mkdir -p "$BIN_DIR"
# Atomic-ish: write then move into place.
tmp_dst="$BIN_DIR/.sirius.tmp.$$"
cp "$STAGE/sirius" "$tmp_dst"
chmod +x "$tmp_dst"
mv -f "$tmp_dst" "$BIN_DIR/sirius"
log "install-sirius: installed $BIN_DIR/sirius"

log ""
log "install-sirius: done — sirius $VERSION installed for $PLATFORM."
print_path_hint
log ""
log "Next steps:"
log "  sirius init      # set up the .sirius/ ledger in your repo"
log "  sirius doctor    # verify the workspace contracts (amt + hayven + config)"
suite_hint
