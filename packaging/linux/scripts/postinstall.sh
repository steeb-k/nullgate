#!/bin/sh
# Nullgate native package: post-install. Shared by the .deb (postinst) and the
# .rpm (%post). Enables the daemon on a fresh install and restarts it on upgrade.
set -e

# The marker left by a .deb removal (see postremove.sh): dpkg reports a reinstall
# after `apt remove` as an upgrade, but the removal disabled the service.
REMOVED_MARKER=/var/lib/nullgate/.package-removed

fresh=0
case "${1:-}" in
  configure)
    # dpkg: $2 is the previously configured version, empty on a first install.
    if [ -z "${2:-}" ] || [ -e "$REMOVED_MARKER" ]; then fresh=1; fi
    rm -f "$REMOVED_MARKER"
    ;;
  abort-*) exit 0 ;;
  1) fresh=1 ;;          # rpm: first install
  [0-9]*) fresh=0 ;;     # rpm: upgrade (2 or more instances)
  *) exit 0 ;;
esac

# rpm, first install only: add the apps.kznjk.com repository for whichever of
# dnf and zypper this system has, unless one is already configured under that
# name. An upgrade never re-adds it, so deleting it is a lasting opt-out (dpkg
# gives the .deb's conffiles the same behaviour).
REPO_DIR=/usr/share/nullgate/repo
if [ "$fresh" = 1 ] && [ -d "$REPO_DIR" ]; then
  if [ -d /etc/yum.repos.d ] && [ ! -e /etc/yum.repos.d/kznjk.repo ]; then
    cp "$REPO_DIR/kznjk.repo" /etc/yum.repos.d/kznjk.repo
  fi
  if [ -d /etc/zypp/repos.d ] && [ ! -e /etc/zypp/repos.d/kznjk.repo ]; then
    cp "$REPO_DIR/kznjk-zypper.repo" /etc/zypp/repos.d/kznjk.repo
  fi
fi

# No systemd in charge (a container, a chroot): the files are in place and the
# unit starts at the next boot if it is enabled later.
[ -d /run/systemd/system ] || exit 0

systemctl daemon-reload >/dev/null 2>&1 || true
if [ "$fresh" = 1 ]; then
  # The daemon owns the virtual interface and must run for the app to do anything.
  systemctl enable --now nullgate-daemon.service >/dev/null 2>&1 \
    || echo "nullgate: could not start the daemon; run: sudo systemctl enable --now nullgate-daemon" >&2
else
  # Respect a service the user stopped or disabled; restart only if it is running.
  # The tray agent notices the daemon's new version and relaunches itself.
  systemctl try-restart nullgate-daemon.service >/dev/null 2>&1 || true
fi

exit 0
