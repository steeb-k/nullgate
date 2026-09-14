#!/bin/sh
# Nullgate native package: pre-install. Shared by the .deb (preinst) and the .rpm
# (%pre); dpkg passes a word ("install", "upgrade", ...), rpm a count.
set -e

case "${1:-}" in
  abort-upgrade) exit 0 ;;
esac

# Refuse to install over a tarball install (nullgatectl). Its binaries live in
# /usr/local/bin, which is ahead of /usr/bin on PATH, and its units in
# /etc/systemd/system, which shadow the packaged ones in /usr/lib/systemd/system.
# Both would run silently in place of this package, and the tarball's own daily
# self-update would keep replacing them.
if [ -e /usr/local/bin/nullgate-daemon ]; then
  echo "nullgate: Nullgate is already installed from the release tarball (/usr/local/bin)." >&2
  echo "nullgate: Remove that install first, then install this package again:" >&2
  echo "nullgate:     sudo nullgatectl --uninstall" >&2
  echo "nullgate: Your network membership and keys are kept." >&2
  exit 1
fi

exit 0
