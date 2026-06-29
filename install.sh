#!/bin/sh
# Nullgate (Nullgate) bootstrap — install, update, or remove in one command,
# on Linux OR macOS:
#
#   curl -fsSL https://raw.githubusercontent.com/steeb-k/iroh-private-network/main/install.sh | sh
#
# It detects the OS, downloads the matching release asset, unpacks it, and runs the
# bundled `nullgatectl --install`. After the first install, manage everything with the
# `nullgatectl` command. Interactive when run from a terminal; otherwise defaults to
# install/update. Non-interactive override:
#   ... | sh -s -- install      (or update / remove)
#   NULLGATE_ACTION=install ... | sh
#
# Source of truth: install.sh at the repo root. Windows users: download the signed
# .msi from the releases page instead.
set -eu

REPO="${NULLGATE_BINARIES_REPO:-steeb-k/iroh-private-network}"
API="https://api.github.com/repos/$REPO/releases/latest"

say()  { printf '%s\n' "$*"; }
die()  { printf '%s\n' "nullgate: error: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- OS detection: pick the asset regex + the unpacked-tree root marker. --------
OS="$(uname -s)"
case "$OS" in
  Linux)
    ASSET_RE='https://[^"]+linux-x86_64\.tar\.gz'
    ASSET_DESC="linux-x86_64.tar.gz"
    find_root() { find "$1" -maxdepth 2 -type d -name bin -exec dirname {} ';' | head -n1; }
    ;;
  Darwin)
    case "$(uname -m)" in arm64) HOST=arm64 ;; *) HOST=x86_64 ;; esac
    ASSET_RE="https://[^\"]+macos-(universal|$HOST)\\.tar\\.gz"
    ASSET_DESC="macos-$HOST (or -universal) tarball"
    find_root() { find "$1" -maxdepth 2 -type d -name "Nullgate.app" -exec dirname {} ';' | head -n1; }
    ;;
  *) die "unsupported OS: $OS (Linux and macOS only; Windows uses the .msi installer)" ;;
esac

have curl || die "curl is required"
have tar  || die "tar is required"

say "Nullgate installer"

INSTALLED=""
if have nullgate-daemon; then
  INSTALLED="$(nullgate-daemon --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)"
fi

# Pick the action: CLI arg > $NULLGATE_ACTION > interactive menu > sane default.
ACTION="${1:-${NULLGATE_ACTION:-}}"
if [ -z "$ACTION" ]; then
  if (exec 3</dev/tty) 2>/dev/null; then
    if [ -n "$INSTALLED" ]; then
      say "  Installed: v$INSTALLED"
      say ""
      say "  1) Update to the latest version"
      say "  2) Remove"
      say "  3) Cancel"
      printf "Choose [1]: "
      read ans < /dev/tty || ans=3
      case "${ans:-1}" in 1|"") ACTION=update ;; 2) ACTION=remove ;; *) say "Cancelled."; exit 0 ;; esac
    else
      say "  Not installed."
      say ""
      say "  1) Install"
      say "  2) Cancel"
      printf "Choose [1]: "
      read ans < /dev/tty || ans=2
      case "${ans:-1}" in 1|"") ACTION=install ;; *) say "Cancelled."; exit 0 ;; esac
    fi
  else
    ACTION=install   # piped, no terminal: install (idempotent — also upgrades)
  fi
fi

case "$ACTION" in
  install|update|--install|--update)     ACTION=install_or_update ;;
  remove|uninstall|--remove|--uninstall) ACTION=remove ;;
  cancel) say "Cancelled."; exit 0 ;;
  *) die "unknown action '$ACTION' (use install | update | remove)" ;;
esac

# Update/remove of an existing install: delegate to the installed manager, which
# already knows how to download + version-check + swap (no work to duplicate here).
if have nullgatectl; then
  case "$ACTION" in
    install_or_update) exec nullgatectl --update ;;
    remove)            exec nullgatectl --uninstall ;;
  esac
fi
[ "$ACTION" = remove ] && die "Nullgate is not installed"

# First-time install: fetch the latest release tarball and run its manager.
URL="$(curl -fsSL "$API" | grep -oE "\"browser_download_url\": *\"$ASSET_RE\"" | sed -E 's/.*"(https[^"]+)".*/\1/' | head -n1)"
[ -n "$URL" ] || die "no $ASSET_DESC in the latest release of $REPO"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
say "Downloading $URL"
curl -fsSL -o "$TMP/pkg.tgz" "$URL" || die "download failed"
tar -xzf "$TMP/pkg.tgz" -C "$TMP" || die "extract failed"
ROOT="$(find_root "$TMP")"
[ -n "$ROOT" ] || die "downloaded archive has an unexpected layout"
say "Installing (you may be prompted for your password to set up the system service)"
"$ROOT/nullgatectl" --install
