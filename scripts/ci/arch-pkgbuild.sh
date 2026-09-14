#!/usr/bin/env bash
# Build, install and remove the Arch package from packaging/arch/PKGBUILD, inside
# an archlinux container, as root. Proves the PKGBUILD the AUR ships still works.
#
#   docker run --rm -v "$PWD:/src:ro" -v "$PWD/dist/arch:/out" archlinux:latest \
#     bash /src/scripts/ci/arch-pkgbuild.sh <source-tarball>
#
# <source-tarball> is a path under /src to `nullgate-<version>.tar.gz` holding the
# tree under a `nullgate-<version>/` prefix, which is what GitHub's tag archive
# looks like (CI makes it with `git archive`). It stands in for the download, so
# the build never depends on a tag that has not been published yet.
#
# When /out is mounted, the built package (not the -debug split) is copied there:
# release.yml attaches it, and apps.kznjk.com's pacman repo signs and serves it.
set -euo pipefail

SRC=/src
TARBALL="$1"
VERSION="$(grep -m1 '^version = ' "$SRC/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
[ "$(basename "$TARBALL")" = "nullgate-$VERSION.tar.gz" ] || {
  echo "arch-pkgbuild: expected nullgate-$VERSION.tar.gz, got $TARBALL" >&2; exit 1; }

pacman -Syu --noconfirm --needed base-devel namcap rust pkgconf gtk4 libadwaita dbus hicolor-icon-theme >/dev/null

# makepkg refuses to run as root.
id builder >/dev/null 2>&1 || useradd -m builder
WORK=/home/builder/pkg
rm -rf "$WORK"; mkdir -p "$WORK"
cp "$SRC/packaging/arch/PKGBUILD" "$SRC/packaging/arch/nullgate.install" "$WORK/"
# A file named like the source's `name::` part is used instead of downloading it.
cp "$TARBALL" "$WORK/"
sed -i "s/^pkgver=.*/pkgver=$VERSION/; s/^pkgrel=.*/pkgrel=1/" "$WORK/PKGBUILD"
chown -R builder: "$WORK"

cd "$WORK"
runuser -u builder -- makepkg --noconfirm --skipchecksums
PKG="$(ls "$WORK"/nullgate-"$VERSION"-1-x86_64.pkg.tar.zst)"

echo "== namcap"
namcap PKGBUILD || true
namcap "$PKG" || true

echo "== install"
pacman -U --noconfirm "$PKG"
nullgate-daemon --version | grep -qx "nullgate-daemon $VERSION"
grep -qx 'ExecStart=/usr/bin/nullgate-daemon run' /usr/lib/systemd/system/nullgate-daemon.service
if nullgatectl --update >/tmp/ctl.out 2>&1; then
  echo "arch-pkgbuild: nullgatectl --update did not refuse a package-managed install" >&2; exit 1
fi
grep -q 'installed as a system package' /tmp/ctl.out

echo "== remove"
pacman -R --noconfirm nullgate
[ ! -e /usr/bin/nullgate-daemon ]
if [ -d /out ]; then
  cp "$PKG" /out/
  echo "arch-pkgbuild: wrote /out/$(basename "$PKG")"
fi
echo "arch-pkgbuild: OK"
