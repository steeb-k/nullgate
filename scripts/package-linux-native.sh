#!/usr/bin/env bash
# Build the Nullgate .deb and .rpm from the tree scripts/package-linux.sh stages.
#
#   scripts/package-linux.sh && scripts/package-linux-native.sh
#
# Output (distro-conventional names, from nfpm):
#   dist/nullgate_<version>-1_amd64.deb
#   dist/nullgate-<version>-1.x86_64.rpm
#
# Requires nfpm on PATH (CI pins the version in .github/workflows/build.yml):
#   https://github.com/goreleaser/nfpm/releases
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

command -v nfpm >/dev/null 2>&1 || {
  echo "package-linux-native: nfpm not found on PATH." >&2
  echo "  Download nfpm_<ver>_Linux_x86_64.tar.gz from https://github.com/goreleaser/nfpm/releases" >&2
  exit 1
}

VERSION="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "${VERSION:-}" ] || { echo "package-linux-native: could not read version from Cargo.toml" >&2; exit 1; }
STAGE="dist/nullgate-${VERSION}-linux-x86_64"
[ -x "$STAGE/bin/nullgate-daemon" ] || {
  echo "package-linux-native: $STAGE is missing; run scripts/package-linux.sh first" >&2
  exit 1
}

# nfpm.yaml reads from this fixed path. The committed unit names the tarball's
# /usr/local/bin; a package installs to /usr/bin.
NATIVE="dist/native"
rm -rf "$NATIVE"
mkdir -p "$NATIVE"
cp -a "$STAGE" "$NATIVE/tree"
# Distribution policy wants a fixed interpreter in /usr/bin, not `env`.
sed -i '1s|^#!/usr/bin/env bash$|#!/bin/bash|' "$NATIVE/tree/nullgatectl"
sed 's|/usr/local/bin/|/usr/bin/|g' "$STAGE/lib/systemd/system/nullgate-daemon.service" \
  > "$NATIVE/nullgate-daemon.service"
if grep -q '/usr/local' "$NATIVE/nullgate-daemon.service"; then
  echo "package-linux-native: the unit still names /usr/local" >&2
  exit 1
fi

export NULLGATE_VERSION="$VERSION"
rm -f "dist/nullgate_${VERSION}-1_amd64.deb" "dist/nullgate-${VERSION}-1.x86_64.rpm"
nfpm package -f packaging/linux/nfpm.yaml -p deb -t dist/
nfpm package -f packaging/linux/nfpm.yaml -p rpm -t dist/

for f in "dist/nullgate_${VERSION}-1_amd64.deb" "dist/nullgate-${VERSION}-1.x86_64.rpm"; do
  [ -f "$f" ] || { echo "package-linux-native: nfpm did not write $f" >&2; ls -la dist >&2; exit 1; }
  echo "package-linux-native: wrote $f"
done
