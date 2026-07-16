#!/bin/sh
# install-sothis.sh — one-shot installer for the Sothis suite CLIs.
#
# Sothis is the local-first suite led by Sirius Forester: the foreman (`sirius`)
# claims work from an Ametrite board (`amt`), locks code through a Hayvenhurst
# code graph (`hayven`), and pairs with Catryna Wikinelli (`catryna`) for the
# "why" docs. Each tool stands alone; full fleet control comes from all four.
#
# WHY this exists: the four tools install four different ways, and the two
# interactive `/plugin` steps can't run from a shell. This script is the CLI
# half of "let's Sothis this up" — it installs the binaries the plugins can't
# ship, then hands you to the marketplace bundle for the plugin adds:
#
#   sirius   — signed prebuilt binary  → this repo's install-sirius.sh (delegated)
#   hayven   — prebuilt binary         → Hayvenhurst's install-hayven.sh (delegated)
#   amt      — Rust binary (cargo)     → detected; guided if missing (never auto-built)
#   catryna  — bun-based MCP plugin    → bun checked; the plugin itself is a
#                                        `/plugin install` (see the marketplace bundle)
#
# DELEGATION, not duplication: each binary is fetched and verified by that
# tool's OWN authoritative installer (sirius verifies a Sigstore signature;
# hayven verifies a sha256). This script never re-implements a download or a
# signature check — it orchestrates. The one supply-chain note: when the
# Hayvenhurst plugin isn't already on disk, this fetches its install-hayven.sh
# over HTTPS from $HAYVEN_REPO at $HAYVEN_INSTALLER_REF (default: a release
# TAG, so the fetched script is an immutable, reviewed revision rather than
# whatever `main` holds today) and runs it. That fetched script then verifies
# the hayven binary's sha256 itself. Prefer the local copy (found
# automatically) or pass --skip-hayven and run /hayvenhurst:install-binary
# yourself if you'd rather not run a fetched script at all.
#
# Idempotent + safe to re-run: anything already on PATH is left alone. POSIX sh
# (macOS / Linux). Windows: install each tool from its release tarball manually.
#
# Usage:
#   install-sothis.sh                   # install every missing suite CLI
#   install-sothis.sh --check           # report presence of all four; never installs
#   install-sothis.sh --prefix DIR      # install binaries into DIR/bin (forwarded)
#   install-sothis.sh --require-signature  # forwarded to sirius; abort if unverifiable
#   install-sothis.sh --skip-hayven     # don't touch hayven (e.g. install it via its plugin)
#   install-sothis.sh --skip-amt        # don't check/guide amt
#   install-sothis.sh --help
#
# Environment:
#   SOTHIS_INSTALL_PREFIX   override the install prefix (same as --prefix)
#   HAYVEN_REPO             override hayven's owner/repo (default Davidb3l/Hayvenhurst-dev)
#   HAYVEN_INSTALLER_REF    ref for the fetched install-hayven.sh (default: a release tag)
#   AMETRITE_REPO           shown in the amt hint (default Davidb3l/Ametrite)
#   Plus every variable the delegated installers honor (SIRIUS_REPO,
#   SIRIUS_RELEASE_TAG, SIRIUS_INSTALL_PREFIX, HAYVEN_INSTALL_PREFIX, …).
#   --prefix / SOTHIS_INSTALL_PREFIX override the per-tool *_INSTALL_PREFIX;
#   when neither is given, each tool's own default chain applies.

set -eu

PREFIX="${SOTHIS_INSTALL_PREFIX:-${CLAUDE_PLUGIN_DATA:-$HOME/.local}}"
# Whether the CALLER chose a prefix (flag or SOTHIS_INSTALL_PREFIX). Only then
# do we force --prefix onto the delegated installers; otherwise each tool's own
# default chain (TOOL_INSTALL_PREFIX > CLAUDE_PLUGIN_DATA > ~/.local) wins.
PREFIX_EXPLICIT=0
if [ -n "${SOTHIS_INSTALL_PREFIX:-}" ]; then PREFIX_EXPLICIT=1; fi
HAYVEN_REPO="${HAYVEN_REPO:-Davidb3l/Hayvenhurst-dev}"
# A TAG, not `main`: the fetched-over-HTTPS installer should be an immutable,
# reviewed revision. Bump deliberately when hayven ships installer changes.
HAYVEN_INSTALLER_REF="${HAYVEN_INSTALLER_REF:-v0.0.6}"
AMETRITE_REPO="${AMETRITE_REPO:-Davidb3l/Ametrite}"
MODE="install"
REQUIRE_SIG=0
SKIP_HAYVEN=0
SKIP_AMT=0

while [ $# -gt 0 ]; do
  case "$1" in
    --check) MODE="check" ;;
    --require-signature) REQUIRE_SIG=1 ;;
    --skip-hayven) SKIP_HAYVEN=1 ;;
    --skip-amt) SKIP_AMT=1 ;;
    --prefix)
      [ -n "${2:-}" ] || { echo "install-sothis: --prefix needs a directory" >&2; exit 2; }
      PREFIX="$2"; PREFIX_EXPLICIT=1; shift ;;
    --help|-h)
      sed -n '2,52p' "$0"
      exit 0
      ;;
    *) echo "install-sothis: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

BIN_DIR="$PREFIX/bin"
# Resolve this script's own directory so we can call its sibling install-sirius.sh
# regardless of the caller's cwd.
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

log()  { printf '%s\n' "$*" >&2; }
fail() { log "install-sothis: error: $*"; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() { # fetch <url> <dest>
  url="$1"; dest="$2"
  if have curl; then
    curl -fsSL "$url" -o "$dest"
  elif have wget; then
    wget -qO "$dest" "$url"
  else
    return 1
  fi
}

# A CLI counts as installed if it's on PATH or sitting in our install dir.
tool_present() { # tool_present <bin>
  have "$1" || [ -x "$BIN_DIR/$1" ]
}

tool_where() { # tool_where <bin> -> human location on stdout
  if have "$1"; then command -v "$1"; elif [ -x "$BIN_DIR/$1" ]; then echo "$BIN_DIR/$1 (not on PATH)"; else echo "not installed"; fi
}

# ---- --check: report all four, install nothing -----------------------------
if [ "$MODE" = "check" ]; then
  log "Sothis suite status:"
  for t in sirius hayven amt catryna; do
    log "  $t: $(tool_where "$t")"
  done
  # catryna is a plugin, not a PATH binary; report its runtime instead.
  have bun && log "  bun (catryna runtime): $(command -v bun)" || log "  bun (catryna runtime): not installed"
  # Exit 0 if the foreman + graph are present, 3 otherwise (mirrors the per-tool
  # --check contract so a SessionStart hook can branch on it).
  if tool_present sirius && tool_present hayven; then exit 0; fi
  exit 3
fi

# ---- sirius (delegated to the bundled installer) ---------------------------
install_sirius() {
  if tool_present sirius; then
    log "sirius: already installed ($(tool_where sirius)); skipping."
    return 0
  fi
  installer="$SCRIPT_DIR/install-sirius.sh"
  [ -f "$installer" ] || fail "cannot find install-sirius.sh next to this script ($installer)"
  log "sirius: installing via install-sirius.sh"
  set --
  # Force our prefix only when the caller chose one; otherwise let the tool's
  # own default chain (SIRIUS_INSTALL_PREFIX > CLAUDE_PLUGIN_DATA > ~/.local)
  # decide, as the header promises.
  [ "$PREFIX_EXPLICIT" = "1" ] && set -- --prefix "$PREFIX"
  [ "$REQUIRE_SIG" = "1" ] && set -- "$@" --require-signature
  sh "$installer" "$@" || fail "install-sirius.sh failed"
}

# ---- hayven (delegated to Hayvenhurst's own installer) ----------------------
# Prefer a copy already on disk (installed Hayvenhurst plugin); fall back to
# fetching it over HTTPS from the pinned repo. Either way, hayven's script does
# its own download + checksum verification.
#
# Layouts differ by install path:
#   marketplaces/hayvenhurst/            = a clone of Hayvenhurst-dev, so the
#                                          script sits under plugin/scripts/.
#   cache/<marketplace>/hayvenhurst/<v>/ = the INSTALLED PLUGIN root (no
#                                          plugin/ segment), so scripts/ is
#                                          top-level. <marketplace> is
#                                          `hayvenhurst` for a standalone
#                                          install and `sirius-forester` for
#                                          the Sothis bundle; accept any.
find_local_hayven_installer() {
  for cand in \
    "$HOME/.claude/plugins/marketplaces/hayvenhurst/plugin/scripts/install-hayven.sh" \
    "$HOME/.claude/plugins/cache"/*/hayvenhurst/*/scripts/install-hayven.sh ; do
    [ -f "$cand" ] && { printf '%s\n' "$cand"; return 0; }
  done
  return 1
}

install_hayven() {
  if [ "$SKIP_HAYVEN" = "1" ]; then
    log "hayven: --skip-hayven set; skipping."
    return 0
  fi
  if tool_present hayven; then
    log "hayven: already installed ($(tool_where hayven)); skipping."
    return 0
  fi
  if local_installer="$(find_local_hayven_installer)"; then
    log "hayven: installing via local install-hayven.sh ($local_installer)"
    set --
    [ "$PREFIX_EXPLICIT" = "1" ] && set -- --prefix "$PREFIX"
    sh "$local_installer" "$@" || fail "install-hayven.sh failed"
    return 0
  fi
  url="https://raw.githubusercontent.com/$HAYVEN_REPO/$HAYVEN_INSTALLER_REF/plugin/scripts/install-hayven.sh"
  log "hayven: no local installer found; fetching $url"
  tmp="$(mktemp "${TMPDIR:-/tmp}/install-hayven.XXXXXX")" || fail "mktemp failed"
  # shellcheck disable=SC2064
  trap "rm -f \"$tmp\"" EXIT INT TERM
  fetch "$url" "$tmp" || fail "could not download install-hayven.sh from $HAYVEN_REPO@$HAYVEN_INSTALLER_REF
        (need curl or wget). Install hayven yourself with /hayvenhurst:install-binary,
        or re-run with --skip-hayven."
  [ -s "$tmp" ] || fail "downloaded install-hayven.sh was empty"
  set --
  [ "$PREFIX_EXPLICIT" = "1" ] && set -- --prefix "$PREFIX"
  sh "$tmp" "$@" || fail "install-hayven.sh failed"
  rm -f "$tmp"
  trap - EXIT INT TERM
}

# ---- amt (detect only; never auto-build) -----------------------------------
check_amt() {
  if [ "$SKIP_AMT" = "1" ]; then
    log "amt: --skip-amt set; skipping."
    return 0
  fi
  if tool_present amt; then
    log "amt: already installed ($(tool_where amt))."
    return 0
  fi
  log ""
  log "amt (Ametrite, the board): not installed. It's a Rust binary, and this"
  log "one-shot deliberately does NOT clone or build it for you. Get it by"
  log "asking Claude Code to \"ametrite this repo\" (the ametrite skill bootstraps"
  log "the amt CLI), or build it yourself:"
  log "  git clone https://github.com/$AMETRITE_REPO.git && cd $(basename "$AMETRITE_REPO") && cargo build --release"
  log "  ln -sf \"\$PWD/target/release/amt\" \"$BIN_DIR/amt\""
}

# ---- catryna (a plugin; verify its bun runtime) ----------------------------
check_catryna() {
  # Catryna may be installed from the Sothis bundle (catryna@sirius-forester) or
  # its standalone marketplace (catryna@catryna-wikinelli) — accept either.
  if grep -Eqs '"catryna@(sirius-forester|catryna-wikinelli)"' "$HOME/.claude/plugins/installed_plugins.json" 2>/dev/null; then
    log "catryna: plugin installed."
  else
    log ""
    log "catryna (Catryna Wikinelli, the docs): install the plugin (interactive):"
    log "  /plugin install catryna@sirius-forester   # from the Sothis bundle"
  fi
  if ! have bun; then
    log "catryna: WARNING: bun not found. The Catryna MCP server runs on bun;"
    log "         install it: https://bun.sh  (curl -fsSL https://bun.sh/install | bash)"
  fi
}

# ---- run --------------------------------------------------------------------
log "install-sothis: installing the Sothis suite CLIs (prefix: $PREFIX)"
log ""
install_sirius
install_hayven
check_amt
check_catryna

# PATH hint if our install dir isn't on PATH.
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *)
    log ""
    log "note: $BIN_DIR is not on your PATH. Add it, e.g.:"
    log "      export PATH=\"$BIN_DIR:\$PATH\"   # add to ~/.zshrc or ~/.bashrc"
    ;;
esac

log ""
log "install-sothis: done. Finish the interactive half in Claude Code:"
log "  /plugin marketplace add Davidb3l/Sirius-Forester   # the Sothis bundle"
log "  /plugin install sirius@sirius-forester"
log "  /plugin install hayvenhurst@sirius-forester"
log "  /plugin install catryna@sirius-forester"
log ""

# End with the foreman's health check — the suite's ground truth. It needs a
# .sirius/ workspace; if there isn't one yet, point at `sirius init` instead of
# letting doctor error out.
if tool_present sirius; then
  SIRIUS_BIN="sirius"
  have sirius || SIRIUS_BIN="$BIN_DIR/sirius"
  if [ -d .sirius ]; then
    log "install-sothis: running sirius doctor"
    "$SIRIUS_BIN" doctor || true
  else
    log "Next: in your repo, run  sirius init  then  sirius doctor"
  fi
fi
