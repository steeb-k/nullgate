#!/usr/bin/env bash
# Build the Nullgate (Nullgate) Linux release tarball.
#
#   scripts/package-linux.sh              cargo build --release, then package
#   scripts/package-linux.sh --skip-build package the existing target/release bins
#
# Output: dist/nullgate-<version>-linux-x86_64.tar.gz
#
# The tarball is installed system-wide by the bundled `nullgatectl --install` (which
# uses sudo): binaries to /usr/local/bin, a root systemd service for the daemon
# (it needs CAP_NET_ADMIN for the TUN), a root daily update timer, and an app-menu
# entry. Relies on the target having system GTK 4.10+/libadwaita 1.4+ (not bundled):
#   sudo apt install libgtk-4-1 libadwaita-1-0
# Build-time also needs: libgtk-4-dev libadwaita-1-dev pkg-config build-essential
#
# Requires: cargo and tar. The .deb/.rpm are built from this staged tree by
# scripts/package-linux-native.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_ID="io.github.steeb_k.Nullgate"
PKG_SRC="$ROOT/packaging/linux"
# Per-size app icons (img/nullgate-icon-<sz>.png) installed as-is.
ICON_SIZES="16 32 64 128 256 512"

cd "$ROOT"
VERSION="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "${VERSION:-}" ] || { echo "package-linux: could not read version from Cargo.toml" >&2; exit 1; }
NAME="nullgate-${VERSION}-linux-x86_64"
echo "package-linux: building $NAME"

if [ "${1:-}" != "--skip-build" ]; then
  cargo build --release -p ipn-daemon -p ipn-gui -p ipn-cli
fi

STAGE="$ROOT/dist/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" \
         "$STAGE/share/applications" \
         "$STAGE/share/metainfo" \
         "$STAGE/etc/xdg/autostart" \
         "$STAGE/lib/systemd/system"

# GUI (unprivileged), daemon (owns the TUN), CLI.
for b in nullgate-daemon nullgate nullgate-cli; do
  install -m 0755 "target/release/$b" "$STAGE/bin/$b"
done

# Installer/updater manager + docs (tarball root)
install -m 0755 "$PKG_SRC/nullgatectl"      "$STAGE/nullgatectl"
install -m 0644 "$PKG_SRC/INSTALL.txt" "$STAGE/INSTALL.txt"
install -m 0644 "$ROOT/LICENSE"        "$STAGE/LICENSE"

# Desktop entry, AppStream metadata, and the tray agent's login autostart entry
# (installed under the app's own id, which is the name nullgatectl uses too).
install -m 0644 "$PKG_SRC/$APP_ID.desktop" "$STAGE/share/applications/$APP_ID.desktop"
install -m 0644 "$PKG_SRC/$APP_ID.metainfo.xml" "$STAGE/share/metainfo/$APP_ID.metainfo.xml"
install -m 0644 "$PKG_SRC/$APP_ID.Agent.desktop" "$STAGE/etc/xdg/autostart/$APP_ID.desktop"

# systemd SYSTEM units (installed to /etc/systemd/system by nullgatectl)
install -m 0644 "$PKG_SRC/nullgate-daemon.service"  "$STAGE/lib/systemd/system/nullgate-daemon.service"
install -m 0644 "$PKG_SRC/nullgate-update.service"  "$STAGE/lib/systemd/system/nullgate-update.service"
install -m 0644 "$PKG_SRC/nullgate-update.timer"    "$STAGE/lib/systemd/system/nullgate-update.timer"

# hicolor icons: the per-size app icons, installed as-is.
for sz in $ICON_SIZES; do
  dir="$STAGE/share/icons/hicolor/${sz}x${sz}/apps"
  mkdir -p "$dir"
  install -m 0644 "$ROOT/img/nullgate-icon-${sz}.png" "$dir/$APP_ID.png"
done

# Tarball
mkdir -p "$ROOT/dist"
tar -czf "$ROOT/dist/$NAME.tar.gz" -C "$ROOT/dist" "$NAME"
echo "package-linux: wrote dist/$NAME.tar.gz"
