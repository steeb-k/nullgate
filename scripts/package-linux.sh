#!/usr/bin/env bash
# Build the iroh-private-network Linux release tarball (run in WSL / on Linux).
#
#   scripts/package-linux.sh              cargo build --release, then package
#   scripts/package-linux.sh --skip-build package the existing target/release bin
#
# Output: dist/ipn-<version>-linux-x86_64.tar.gz
#
# Like seed-sync, this relies on SYSTEM GTK on the target (no bundling):
#   sudo apt install libgtk-4-1 libadwaita-1-0
# Build-time also needs: libgtk-4-dev libadwaita-1-dev pkg-config build-essential
# Routing needs /dev/net/tun + CAP_NET_ADMIN (run the binary with sudo).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# Version lives in the workspace root [workspace.package] (crates use version.workspace).
VERSION="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "${VERSION:-}" ] || { echo "package-linux: could not read version" >&2; exit 1; }
NAME="ipn-${VERSION}-linux-x86_64"
echo "package-linux: building $NAME"

if [ "${1:-}" != "--skip-build" ]; then
  cargo build --release -p ipn-gui -p ipn-daemon -p ipn-cli
fi

STAGE="$ROOT/dist/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/share/applications"

# GUI (unprivileged), daemon (owns the TUN), CLI.
install -m 0755 "target/release/ipn" "$STAGE/bin/ipn"
install -m 0755 "target/release/ipn-daemon" "$STAGE/bin/ipn-daemon"
install -m 0755 "target/release/ipn-cli" "$STAGE/bin/ipn-cli"

# License (GPLv3 + Wintun exception). Linux uses the kernel TUN, so no Wintun here.
install -m 0644 "$ROOT/LICENSE" "$STAGE/LICENSE.txt"

# Wrapper: run as the normal user (a GUI must NOT run under sudo). It starts the
# daemon in the background if it isn't already running, then launches the GUI.
# The daemon carries the CAP_NET_ADMIN capability granted by ./enable-routing.sh,
# so no sudo is needed at run time.
cat > "$STAGE/ipn" <<'WRAP'
#!/usr/bin/env bash
HERE="$(cd "$(dirname "$0")" && pwd)"
if [ "$(id -u)" = "0" ] && [ -z "${IPN_ALLOW_ROOT:-}" ]; then
  echo "Don't run IPN with sudo — the GUI needs your user's display." >&2
  exit 1
fi
# Start the daemon if the IPC socket isn't already there (a second daemon just
# exits when it can't bind, so this is safe even on a race).
if [ ! -S /tmp/ipn.sock ]; then
  nohup "$HERE/bin/ipn-daemon" run >/tmp/ipn-daemon.log 2>&1 &
  sleep 1
fi
exec "$HERE/bin/ipn" "$@"
WRAP
chmod 0755 "$STAGE/ipn"

# One-time helper: grant CAP_NET_ADMIN to the DAEMON so it can create the TUN
# while running as your normal user (no sudo needed afterwards).
cat > "$STAGE/enable-routing.sh" <<'CAP'
#!/usr/bin/env bash
HERE="$(cd "$(dirname "$0")" && pwd)"
set -e
sudo setcap cap_net_admin,cap_net_raw+ep "$HERE/bin/ipn-daemon"
echo "Routing enabled. Now run: ./ipn"
CAP
chmod 0755 "$STAGE/enable-routing.sh"

cat > "$STAGE/share/applications/io.github.steeb_k.IPN.desktop" <<'DESK'
[Desktop Entry]
Type=Application
Name=iroh-private-network
Comment=Peer-to-peer virtual LAN over iroh
Exec=ipn
Icon=network-workgroup
Categories=Network;
Terminal=false
DESK

cat > "$STAGE/INSTALL.txt" <<'TXT'
iroh-private-network (Linux)

Requires system GTK4 + libadwaita:
  sudo apt install libgtk-4-1 libadwaita-1-0

One-time, to enable routing (RDP/SSH over the virtual LAN):
  ./enable-routing.sh   # sudo setcap cap_net_admin,cap_net_raw+ep bin/ipn-daemon

Then just run (as your normal user — never sudo the GUI):
  ./ipn                 # starts the daemon in the background if needed, opens the GUI

Architecture: bin/ipn-daemon (owns the TUN + iroh node) runs in the background;
bin/ipn is the unprivileged GUI; bin/ipn-cli is an optional headless client.
TXT

mkdir -p "$ROOT/dist"
tar -czf "$ROOT/dist/$NAME.tar.gz" -C "$ROOT/dist" "$NAME"
echo "package-linux: wrote dist/$NAME.tar.gz"
