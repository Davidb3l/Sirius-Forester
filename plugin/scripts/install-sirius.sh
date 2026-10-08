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
# verify its sha256 and its Sigstore signature, and install the binary into a
# known location.
#
# SECURITY: the `<tarball>.sha256` is served from the same origin as the
# tarball, so on its own it only catches a corrupted download, not a tampered
# release: anyone who can replace the tarball can replace its checksum too.
# Authenticity comes from the Sigstore bundle (`<tarball>.sigstore.json`), whose
# Fulcio certificate binds the artifact to THIS repo's release workflow. We pin
# both the signer identity and the OIDC issuer; otherwise an attacker could sign
# a malicious tarball with their own identity and it would still "verify".
#
# A bad signature ALWAYS aborts. A MISSING bundle ALWAYS aborts: the tarball
# came from the same origin, every release publishes a bundle, so "tarball but
# no bundle" is a signature-stripping downgrade, not a benign 404.
#
# The one soft case is a box with no verifier installed (`cosign` or
# `sigstore`): we cannot check, so we warn loudly and continue on TLS plus the
# checksum. An attacker cannot induce that state remotely (it depends on what is
# installed locally). Pass --require-signature (or SIRIUS_REQUIRE_SIGNATURE=1)
# to make it fatal too.
#
# NOTE: the trust anchor follows SIRIUS_REPO. Overriding it points both the
# download AND the expected signer at that repo, so verification then only
# proves "that repo signed its own artifact". Do not set it to a repo you do
# not trust.
#
# Idempotent + safe to re-run. POSIX sh, covering macOS, Linux AND Windows:
# under Git Bash / MSYS2 / Cygwin `uname -s` reports MINGW*/MSYS*/CYGWIN*, and
# this script installs the windows-x64 release asset (the binary inside is
# sirius.exe). windows-x64 is the only Windows asset, so a non-x64 Windows box
# is refused by name rather than 404ing on a download.
#
# If you have no POSIX layer at all (PowerShell only), use the native
# install-sirius.ps1 alongside this script instead.
#
# Usage:
#   install-sirius.sh                 # download + install latest release
#   install-sirius.sh --check         # print status only; never downloads
#                                     #   exit 0 if `sirius` is on PATH or
#                                     #   already installed, 3 if missing
#   install-sirius.sh --version vX.Y.Z   # install a specific tag
#   install-sirius.sh --prefix DIR    # install into DIR/bin (default below)
#   install-sirius.sh --require-signature  # abort unless the signature verifies
#
# Environment:
#   SIRIUS_INSTALL_PREFIX   override the install prefix (same as --prefix)
#   SIRIUS_RELEASE_TAG      pin a release tag (same as --version)
#   SIRIUS_REPO             override owner/repo (default Davidb3l/Sirius-Forester)
#   SIRIUS_REQUIRE_SIGNATURE=1   same as --require-signature

set -eu

REPO="${SIRIUS_REPO:-Davidb3l/Sirius-Forester}"
TAG="${SIRIUS_RELEASE_TAG:-}"
# Default install prefix: ${CLAUDE_PLUGIN_DATA} when invoked by the plugin
# (persists across plugin updates), else ~/.local. We install binaries into
# <prefix>/bin.
DEFAULT_PREFIX="${SIRIUS_INSTALL_PREFIX:-${CLAUDE_PLUGIN_DATA:-$HOME/.local}}"
PREFIX="$DEFAULT_PREFIX"
MODE="install"
# Make a missing verifier fatal. A BAD signature is fatal regardless.
REQUIRE_SIG="${SIRIUS_REQUIRE_SIGNATURE:-0}"

while [ $# -gt 0 ]; do
  case "$1" in
    --check) MODE="check" ;;
    --require-signature) REQUIRE_SIG=1 ;;
    --version)
      [ -n "${2:-}" ] || { echo "install-sirius: --version needs a tag (e.g. v0.1.0)" >&2; exit 2; }
      TAG="$2"; shift ;;
    --prefix)
      [ -n "${2:-}" ] || { echo "install-sirius: --prefix needs a directory" >&2; exit 2; }
      PREFIX="$2"; shift ;;
    --help|-h)
      sed -n '2,60p' "$0"
      exit 0
      ;;
    *) echo "install-sirius: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

BIN_DIR="$PREFIX/bin"

# Host-shape facts we need BEFORE the platform→asset mapping runs: --check
# returns long before detect_platform is called, but it still has to look for
# the right file name and print the right PATH advice.
#
# On Windows the release tarball contains sirius.exe, so every place that names
# the binary on disk goes through $BIN_NAME. (MSYS/Cygwin also resolve a bare
# `sirius` to sirius.exe, but relying on that magic makes the script read as if
# a Unix-named file were installed, which it is not.)
IS_WINDOWS=0
BIN_NAME="sirius"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) IS_WINDOWS=1; BIN_NAME="sirius.exe" ;;
esac

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
  # Prefix match: catryna installs as `catryna@<marketplace>` and there are two
  # legitimate marketplaces (the Sothis bundle `sirius-forester`, and its own
  # `catryna-wikinelli`). Pinning one key nags the other's users forever.
  grep -qs '"catryna@' "$HOME/.claude/plugins/installed_plugins.json" \
    || s_missing="$s_missing Catryna"
  if [ -z "$s_missing" ]; then return 0; fi
  log ""
  log "fleet suite: missing:$s_missing. Sirius is the foreman; for full fleet control install the whole suite (one-shot: /sirius:install-suite):"
  # Anyone running this script has the sirius plugin, hence the sirius-forester
  # marketplace: the Sothis bundle entries install with no extra marketplace add.
  case "$s_missing" in *Hayvenhurst*) log "  Hayvenhurst (code graph): claude plugin install hayvenhurst@sirius-forester, then /hayvenhurst:install-binary" ;; esac
  case "$s_missing" in *Catryna*)     log "  Catryna Wikinelli (code wiki): claude plugin install catryna@sirius-forester" ;; esac
  case "$s_missing" in *Ametrite*)    log "  Ametrite (task board): ask Claude to \"ametrite this repo\" (the skill bootstraps the amt CLI)" ;; esac
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
    # Git Bash reports MINGW64_NT-10.0-<build>; MSYS2's msys shell reports
    # MSYS_NT-...; Cygwin reports CYGWIN_NT-.... All three run this script
    # fine and all three want the windows-x64 asset.
    MINGW*|MSYS*|CYGWIN*) os="windows" ;;
    *) fail "unsupported OS '$uname_s' (this script covers macOS, Linux, and Windows under Git Bash / MSYS / Cygwin)" ;;
  esac
  case "$uname_m" in
    x86_64|amd64) arch="x64" ;;
    arm64|aarch64) arch="arm64" ;;
    *) fail "unsupported CPU arch '$uname_m'" ;;
  esac
  # The only x64 Linux release is the glibc build; musl is not a release target.
  if [ "$os" = "linux" ] && [ "$arch" = "x64" ]; then
    PLATFORM="linux-x64-glibc"
  elif [ "$os" = "windows" ]; then
    # windows-x64 is the ONLY Windows asset in release.yml's matrix. Fail here
    # by name rather than letting the download 404 on an asset that was never
    # built. (Windows-on-ARM usually reports x86_64 through the x64 emulation
    # layer, in which case the x64 build is the correct answer anyway.)
    [ "$arch" = "x64" ] || fail "no Windows release asset for CPU arch '$uname_m'.
        The only Windows asset is sirius-forester-<version>-windows-x64.tar.gz (x86_64).
        Run this from an x64 Git Bash, or build sirius from source."
    PLATFORM="windows-x64"
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
  # Fed a FILENAME containing a backslash — which is what $TMP looks like on
  # Windows whenever TMPDIR is inherited as a native path like C:\Users\...\Temp
  # — both shasum and sha256sum escape the output line and prefix it with a
  # literal "\", so awk '{print $1}' yields "\<hex>" and every comparison below
  # fails as a bogus checksum mismatch. Fed on stdin there is no filename to
  # escape, and the digest is identical on every platform.
  if have shasum; then
    shasum -a 256 < "$f" | awk '{print $1}'
  elif have sha256sum; then
    sha256sum < "$f" | awk '{print $1}'
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

# On Windows this is the norm, not the exception: ~/.local/bin is a Unix
# convention that nothing on Windows puts on PATH, so a fresh install lands a
# working sirius.exe that PowerShell, cmd, editors and Claude Code cannot see.
# We PRINT the fix; we never mutate the user's PATH from a shell script.
print_path_hint() {
  case ":$PATH:" in
    *":$BIN_DIR:"*) return 0 ;; # already on PATH
  esac
  log ""
  log "note: $BIN_DIR is not on your PATH."
  if [ "$IS_WINDOWS" = "1" ]; then
    log ""
    log "  This shell only (Git Bash):"
    log "      export PATH=\"$BIN_DIR:\$PATH\"   # add to ~/.bashrc to persist it here"
    log ""
    log "  Permanently, for ALL of Windows (PowerShell, cmd, editors, Claude Code)"
    log "  — run this ONCE in PowerShell, then close and reopen your shells:"
    log ""
    log "      [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ';' + \"\$env:USERPROFILE\\.local\\bin\", 'User')"
    log ""
    log "  That one-liner appends the DEFAULT prefix (%USERPROFILE%\\.local\\bin)."
    log "  You installed into: $BIN_DIR"
    log "  If those differ, substitute the Windows form of the path above."
    log "  Already-running shells, editors and apps must be RESTARTED to see it."
  else
    log "      export PATH=\"$BIN_DIR:\$PATH\"   # add to ~/.zshrc or ~/.bashrc"
  fi
}

# ---- --check: status only, never downloads ---------------------------------
# ---- update nudge (--check) --------------------------------------------------
# A plugin update ships new skill text, but the binary only moves when the user
# re-runs the installer — so app users would never hear about a new release.
# In suite repos only, at most once a day (cached), 3 s cap, never fatal: say
# so when the latest release is newer than the installed binary.
# A release version: x.y.z with an optional -prerelease; a leading "v" is
# dropped. Anything else (a garbage cache, an odd --version) is rejected.
norm_version() { # norm_version <v> -> normalized on stdout, or fails
  v="${1#v}"
  printf '%s' "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' || return 1
  printf '%s' "$v"
}

older_than() { # older_than A B -> true if A < B (x.y.z; a prerelease < its release)
  awk -v a="$1" -v b="$2" 'BEGIN {
    pa = index(a, "-"); pb = index(b, "-");
    ca = pa ? substr(a, 1, pa - 1) : a; cb = pb ? substr(b, 1, pb - 1) : b;
    split(ca, x, "."); split(cb, y, ".");
    for (i = 1; i <= 3; i++) { if ((x[i] + 0) < (y[i] + 0)) exit 0; if ((x[i] + 0) > (y[i] + 0)) exit 1 }
    exit (pa && !pb) ? 0 : 1 }'
}

update_hint() { # update_hint <sirius binary>
  have curl || return 0
  installed="$(norm_version "$("$1" --version 2>/dev/null | awk '{print $2}')")" || return 0
  cache_dir="${CLAUDE_PLUGIN_DATA:-$HOME/.cache/sirius}"
  cache="$cache_dir/latest-release-tag"
  latest=""
  if [ -f "$cache" ] && [ -n "$(find "$cache" -mtime -1 2>/dev/null)" ]; then
    # Fresh: use it (an empty file = the last lookup failed; retry tomorrow).
    latest="$(norm_version "$(head -c 64 "$cache" 2>/dev/null)")" || latest=""
  else
    loc="$(curl -fsSLI --max-time 3 -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null || true)"
    case "$loc" in
      */releases/tag/*) latest="$(norm_version "${loc##*/releases/tag/}")" || latest="" ;;
    esac
    # Cache the answer — or the failure, so a dead network costs one 3 s
    # wait a day, not one per session. Never let a write error print.
    { mkdir -p "$cache_dir" && printf '%s' "$latest" > "$cache"; } 2>/dev/null || true
  fi
  [ -n "$latest" ] || return 0
  if older_than "$installed" "$latest"; then
    log "sirius: v$latest is out (you have $installed at $(command -v "$1" 2>/dev/null || echo "$1")) — run /sirius:install-binary to update"
  fi
}

if [ "$MODE" = "check" ]; then
  if have sirius; then
    log "sirius: already on PATH ($(command -v sirius))"
    if suite_repo; then suite_hint; update_hint sirius || true; fi
    exit 0
  fi
  if [ -x "$BIN_DIR/$BIN_NAME" ]; then
    log "sirius: installed at $BIN_DIR/$BIN_NAME (not on PATH)"
    print_path_hint
    if suite_repo; then suite_hint; update_hint "$BIN_DIR/sirius" || true; fi
    exit 0
  fi
  log "sirius: not installed. Run /sirius:install-binary (or plugin/scripts/install-sirius.sh) to install it."
  if suite_repo; then suite_hint; fi
  exit 3
fi

# ---- signature verification --------------------------------------------------
# Verify with whichever Sigstore verifier is on the box. Pin BOTH the signer
# identity (this repo's release.yml, at this tag) and the OIDC issuer: an
# unpinned verify only proves "somebody signed this", not "the release workflow
# signed this".
verify_signature() {
  bundle="$1"
  artifact="$2"
  identity="https://github.com/$REPO/.github/workflows/release.yml@refs/tags/$TAG"
  issuer="https://token.actions.githubusercontent.com"

  sig_fail="SIGNATURE VERIFICATION FAILED for $TARBALL
        expected signer: $identity
        expected issuer: $issuer
        Refusing to install: this artifact was not produced by $REPO's release workflow."

  if have cosign; then
    log "install-sirius: verifying signature (cosign)"
    # Keep the verifier's own diagnostics: on a real identity mismatch cosign
    # prints "expected X, got Y", and an OLD cosign (< 3.x) instead fails to
    # parse sigstore-python v3's `.sigstore.json` bundle at all. Swallowing
    # both makes a stale toolchain look identical to a tampered artifact.
    if ! verify_out="$(cosign verify-blob \
      --bundle "$bundle" \
      --certificate-identity "$identity" \
      --certificate-oidc-issuer "$issuer" \
      "$artifact" 2>&1)"; then
      fail "$sig_fail

        verifier output:
$verify_out

        If your cosign predates v3.0, it cannot read this bundle format —
        upgrade cosign (or install the \`sigstore\` python tool) and retry."
    fi
    log "install-sirius: signature OK (cosign)"
    return 0
  fi

  sig_cmd=""
  if have sigstore; then
    sig_cmd="sigstore"
  elif have python3 && python3 -c 'import sigstore' >/dev/null 2>&1; then
    sig_cmd="python3 -m sigstore"
  fi

  if [ -n "$sig_cmd" ]; then
    log "install-sirius: verifying signature (sigstore)"
    # shellcheck disable=SC2086
    if ! verify_out="$($sig_cmd verify identity \
      --bundle "$bundle" \
      --cert-identity "$identity" \
      --cert-oidc-issuer "$issuer" \
      "$artifact" 2>&1)"; then
      fail "$sig_fail

        verifier output:
$verify_out"
    fi
    log "install-sirius: signature OK (sigstore)"
    return 0
  fi

  if [ "$REQUIRE_SIG" = "1" ]; then
    fail "no signature verifier found, and --require-signature was set.
        Install one:  brew install cosign   (or)   pip install sigstore"
  fi
  log "install-sirius: WARNING: no signature verifier (cosign / sigstore) found."
  log "install-sirius: WARNING: proceeding on TLS + checksum alone, which cannot"
  log "install-sirius: WARNING: detect a tampered release. To verify provenance:"
  log "install-sirius: WARNING:   brew install cosign  (or)  pip install sigstore"
  log "install-sirius: WARNING: then re-run with --require-signature."
}

# ---- install ---------------------------------------------------------------
detect_platform
resolve_latest_tag
VERSION="${TAG#v}"
TARBALL="sirius-forester-${VERSION}-${PLATFORM}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"
TARBALL_URL="$BASE_URL/$TARBALL"
CHECKSUM_URL="$TARBALL_URL.sha256"
BUNDLE_URL="$TARBALL_URL.sigstore.json"

log "install-sirius: repo=$REPO tag=$TAG platform=$PLATFORM"
log "install-sirius: asset=$TARBALL"

# Allow a dry run of just the detection/mapping logic without network I/O.
if [ "${SIRIUS_INSTALL_DRY_RUN:-}" = "1" ]; then
  log "DRY RUN: would download: $TARBALL_URL"
  log "DRY RUN: would verify:   $CHECKSUM_URL"
  log "DRY RUN: would verify:   $BUNDLE_URL"
  log "DRY RUN: would install into: $BIN_DIR"
  exit 0
fi

# Windows inherits TMPDIR as a NATIVE path (C:\Users\...\Temp) often enough to
# matter here, and GNU tar reads a leading "C:" as a remote host:path spec —
# `tar -xzf C:\...\x.tar.gz` dies with "Cannot connect to C: resolve failed"
# rather than extracting anything. Fall back to the POSIX /tmp that MSYS always
# provides when TMPDIR looks native.
TMP_BASE="${TMPDIR:-/tmp}"
if [ "$IS_WINDOWS" = "1" ]; then
  case "$TMP_BASE" in
    *\\*|?:*) TMP_BASE="/tmp" ;;
  esac
fi
TMP="$(mktemp -d "$TMP_BASE/sirius-install.XXXXXX")"
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

# Authenticity. The checksum above came from the same origin as the tarball, so
# it proves nothing about provenance on its own.
#
# A missing bundle is ALWAYS fatal, never a skip. The tarball just downloaded
# from this same origin, and every release publishes <tarball>.sigstore.json
# (release.yml uploads with if-no-files-found: error). So "tarball present,
# bundle absent" is not a benign 404 -- it is exactly what an attacker who can
# serve a tampered tarball would return in order to strip the signature and
# downgrade us to the checksum, which they also control.
log "install-sirius: fetching signature bundle"
fetch "$BUNDLE_URL" "$TMP/$TARBALL.sigstore.json" 2>/dev/null || fail "no Sigstore bundle at $BUNDLE_URL
        The tarball downloaded but its signature did not. Refusing to install.
        Every Sirius Forester release publishes <tarball>.sigstore.json, so a
        missing bundle means the release is malformed or the download was
        tampered with."
verify_signature "$TMP/$TARBALL.sigstore.json" "$TMP/$TARBALL"

log "install-sirius: extracting"
tar -xzf "$TMP/$TARBALL" -C "$TMP"
# The tarball expands to a top-level dir: sirius-forester-<version>-<platform>/
STAGE="$TMP/sirius-forester-${VERSION}-${PLATFORM}"
[ -d "$STAGE" ] || fail "unexpected tarball layout (no $STAGE)"
# $BIN_NAME is sirius.exe on Windows, sirius everywhere else.
[ -f "$STAGE/$BIN_NAME" ] || fail "tarball is missing the $BIN_NAME binary"

mkdir -p "$BIN_DIR"
# Atomic-ish: write then move into place.
tmp_dst="$BIN_DIR/.sirius.tmp.$$"
cp "$STAGE/$BIN_NAME" "$tmp_dst"
# Harmless (and still the right thing) under MSYS/Cygwin, which map the x bit
# onto the file's ACL.
chmod +x "$tmp_dst"
mv -f "$tmp_dst" "$BIN_DIR/$BIN_NAME"
log "install-sirius: installed $BIN_DIR/$BIN_NAME"

log ""
log "install-sirius: done. sirius $VERSION installed for $PLATFORM."
print_path_hint
log ""
log "Next steps:"
log "  sirius init      # set up the .sirius/ ledger in your repo"
log "  sirius doctor    # verify the workspace contracts (amt + hayven + config)"
suite_hint
