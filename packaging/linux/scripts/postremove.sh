#!/bin/sh
# Nullgate native package: post-remove. Shared by the .deb (postrm) and the .rpm
# (%postun).
set -e

REMOVED_MARKER=/var/lib/nullgate/.package-removed

if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || true
fi

case "${1:-}" in
  remove)
    # dpkg keeps a removed package's config files and reports a later reinstall
    # as an upgrade; this tells postinstall.sh to enable the service again.
    mkdir -p /var/lib/nullgate && touch "$REMOVED_MARKER"
    ;;
  purge)
    rm -f "$REMOVED_MARKER"
    rmdir /var/lib/nullgate 2>/dev/null || true
    rm -rf /var/log/nullgate
    ;;
esac

exit 0
