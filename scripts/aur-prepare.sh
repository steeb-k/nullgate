#!/usr/bin/env bash
# Prepare the AUR package `nullgate` for a published release.
#
#   git clone ssh://aur@aur.archlinux.org/nullgate.git ../nullgate-aur   # once
#   scripts/aur-prepare.sh 0.7.2 ../nullgate-aur [pkgrel]
#   cd ../nullgate-aur && git diff && git commit -am "nullgate 0.7.2" && git push
#
# Copies packaging/arch/{PKGBUILD,nullgate.install} into the clone, sets pkgver
# and pkgrel, replaces the SKIP checksum with the sha256 of GitHub's tag archive
# (so the tag must already be published), and regenerates .SRCINFO. makepkg is
# used when present; elsewhere it runs in an archlinux container (podman or docker).
# Nothing is committed or pushed.
set -euo pipefail

VERSION="${1:-}"; AUR="${2:-}"; PKGREL="${3:-1}"
[ -n "$VERSION" ] && [ -d "$AUR" ] || { sed -n '2,13p' "$0" >&2; exit 2; }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
AUR="$(cd "$AUR" && pwd)"

URL="https://github.com/steeb-k/nullgate/archive/refs/tags/v$VERSION.tar.gz"
echo "aur-prepare: hashing $URL"
SUM="$(curl -fsSL "$URL" | sha256sum | cut -d' ' -f1)" || { echo "aur-prepare: tag v$VERSION is not downloadable" >&2; exit 1; }
[ "${#SUM}" = 64 ] || { echo "aur-prepare: bad checksum '$SUM'" >&2; exit 1; }

cp "$ROOT/packaging/arch/nullgate.install" "$AUR/nullgate.install"
sed -e "s/^pkgver=.*/pkgver=$VERSION/" \
    -e "s/^pkgrel=.*/pkgrel=$PKGREL/" \
    -e "s/^sha256sums=.*/sha256sums=('$SUM')/" \
    "$ROOT/packaging/arch/PKGBUILD" > "$AUR/PKGBUILD"
# The in-repo comment about SKIP does not apply to the published copy.
sed -i '/aur-prepare.sh replaces this/d' "$AUR/PKGBUILD"
grep -q "^sha256sums=('$SUM')$" "$AUR/PKGBUILD" || { echo "aur-prepare: checksum not written" >&2; exit 1; }

if command -v makepkg >/dev/null 2>&1; then
  (cd "$AUR" && makepkg --printsrcinfo > .SRCINFO)
else
  engine="$(command -v podman || command -v docker || true)"
  [ -n "$engine" ] || { echo "aur-prepare: need makepkg, podman or docker for .SRCINFO" >&2; exit 1; }
  # makepkg will not run as root; stdout carries .SRCINFO back to the host.
  "$engine" run --rm -v "$AUR:/in:ro" docker.io/archlinux/archlinux:latest bash -c '
    useradd -m b && install -d -o b /home/b/p && cp /in/PKGBUILD /in/nullgate.install /home/b/p/ &&
    chown -R b /home/b/p && cd /home/b/p && runuser -u b -- makepkg --printsrcinfo' > "$AUR/.SRCINFO"
fi
grep -q "pkgver = $VERSION" "$AUR/.SRCINFO" || { echo "aur-prepare: .SRCINFO looks wrong" >&2; exit 1; }
echo "aur-prepare: $AUR is ready for nullgate $VERSION-$PKGREL; review, commit and push it."
