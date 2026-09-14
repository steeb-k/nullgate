#!/bin/sh
# Nullgate native package: pre-remove. Shared by the .deb (prerm) and the .rpm
# (%preun). Stops the daemon only on a real removal, never on an upgrade.
set -e

case "${1:-}" in
  remove|0) ;;            # dpkg removal / rpm erase
  *) exit 0 ;;            # upgrade, deconfigure, failed-upgrade, rpm count >= 1
esac

if [ -d /run/systemd/system ]; then
  systemctl disable --now nullgate-daemon.service >/dev/null 2>&1 || true
fi

# rpm erase: take back the repository definitions postinstall added, unless they
# were edited. (The .deb's are conffiles: kept on remove, deleted on purge.)
# (cmp is not guaranteed: minimal openSUSE images have no diffutils.)
REPO_DIR=/usr/share/nullgate/repo
same() { [ -f "$1" ] && [ -f "$2" ] && [ "$(cat "$1")" = "$(cat "$2")" ]; }
if [ "${1:-}" = 0 ] && [ -d "$REPO_DIR" ]; then
  if same "$REPO_DIR/kznjk.repo" /etc/yum.repos.d/kznjk.repo; then rm -f /etc/yum.repos.d/kznjk.repo; fi
  if same "$REPO_DIR/kznjk-zypper.repo" /etc/zypp/repos.d/kznjk.repo; then rm -f /etc/zypp/repos.d/kznjk.repo; fi
fi

# The tray agent runs in each user's session from the binary being removed.
pkill -f 'nullgate --agent' >/dev/null 2>&1 || true

exit 0
