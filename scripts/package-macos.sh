#!/usr/bin/env bash
# Build the Nullgate macOS release tarball — a self-contained, ad-hoc-signed
# "Nullgate.app" (bundled GTK). macOS analog of package-linux.sh.
#
#   scripts/package-macos.sh                cargo build --release, bundle, package
#   scripts/package-macos.sh --skip-build   package the existing per-arch release bins
#
# Output: dist/nullgate-<version>-macos-<arch>.tar.gz
#   arch = "universal" when the osx-64 conda env + the x86_64 Rust target are
#   present (each Mach-O is lipo'd arm64+x86_64); otherwise "arm64".
#
# GTK is sourced from conda-forge envs, NOT Homebrew: conda-forge builds osx-arm64
# against the macOS 11 SDK and osx-64 against ~10.13, so the bundled dylibs carry a
# macOS-11 `minos` regardless of THIS machine's OS. (An arm64 Homebrew gtk4 on a
# macOS 26 dev box carries minos 26 — unrunnable on any older Mac.) Create the
# envs with scripts/setup-conda-macos.sh; see docs/macos-packaging.md.
#
# Install (via the bundled nullgatectl, which uses sudo) puts the .app in /Applications,
# the daemon as a ROOT LaunchDaemon (it needs root to create the utun interface),
# a root daily auto-update LaunchDaemon, and a per-user tray GUI LaunchAgent.
#
# Requires: cargo, Xcode CLT (install_name_tool/codesign/otool/lipo/iconutil),
# conda-forge envs with gtk4 + libadwaita (osx-arm64; plus osx-64 for universal).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_SRC="$ROOT/packaging/macos"
APP_ID="io.github.steeb_k.Nullgate"
APP_NAME="Nullgate.app"
# conda-forge GTK envs (built vs the macOS 11 SDK). Override via NULLGATE_CONDA_ARM/_X86.
ARM_ENV="${NULLGATE_CONDA_ARM:-$ROOT/.conda-gtk/arm64}"   # osx-arm64 env
X86_ENV="${NULLGATE_CONDA_X86:-$ROOT/.conda-gtk/x86}"     # osx-64 env (universal only)
# Match the conda-forge floor so our own Rust binaries don't raise it.
export MACOSX_DEPLOYMENT_TARGET=11.0
# Reserve Mach-O header space so the bundler can rewrite nullgate's load commands
# to @executable_path/../lib/<name>. conda dylibs are referenced via short
# @rpath/<name> install names, so the relocated paths are LONGER and won't fit a
# stock binary's header (Homebrew's long absolute paths happened to shrink, hiding
# this). Without it, install_name_tool fails "load commands do not fit".
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-headerpad_max_install_names"
SKIP_BUILD=0; [ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "package-macos: could not read version from Cargo.toml" >&2; exit 1; }

[ -d "$ARM_ENV/lib" ] || { echo "package-macos: osx-arm64 conda env not found at $ARM_ENV — run scripts/setup-conda-macos.sh" >&2; exit 1; }

# Universal iff the osx-64 conda env has GTK AND the x86_64 Rust target is installed.
UNIVERSAL=0
if ls "$X86_ENV"/lib/libgtk-4*.dylib >/dev/null 2>&1 \
   && rustup target list --installed 2>/dev/null | grep -q '^x86_64-apple-darwin$'; then
  UNIVERSAL=1
fi
[ "$UNIVERSAL" = 1 ] && SLICE=universal || SLICE=arm64
NAME="nullgate-$VERSION-macos-$SLICE"
echo "package-macos: building $NAME ($([ "$UNIVERSAL" = 1 ] && echo 'arm64 + x86_64 lipo' || echo 'arm64 only'))"

ARM_TGT="aarch64-apple-darwin"
X86_TGT="x86_64-apple-darwin"
ARM_BIN="$ROOT/target/$ARM_TGT/release"
X86_BIN="$ROOT/target/$X86_TGT/release"

if [ "$SKIP_BUILD" != 1 ]; then
  # pkg-config resolves the GTK closure from the conda env only (LIBDIR replaces
  # the default search path, so no system/Homebrew leakage). conda keeps every
  # .pc in one lib/pkgconfig, so there's no keg-only fan-out to enumerate.
  PKG_CONFIG_PATH="$ARM_ENV/lib/pkgconfig" \
  PKG_CONFIG_LIBDIR="$ARM_ENV/lib/pkgconfig" \
    cargo build --release --target "$ARM_TGT" -p ipn-daemon -p ipn-gui -p ipn-cli
  if [ "$UNIVERSAL" = 1 ]; then
    echo "package-macos: building x86_64 slice against $X86_ENV GTK"
    # ALLOW_CROSS because the host (arm64) != target (x86_64).
    PKG_CONFIG_PATH="$X86_ENV/lib/pkgconfig" \
    PKG_CONFIG_LIBDIR="$X86_ENV/lib/pkgconfig" \
    PKG_CONFIG_ALLOW_CROSS=1 \
      cargo build --release --target "$X86_TGT" -p ipn-daemon -p ipn-gui -p ipn-cli
  fi
fi

STAGE="$ROOT/dist/$NAME"
APP="$STAGE/$APP_NAME"
CONTENTS="$APP/Contents"
rm -rf "$STAGE"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$STAGE/LaunchDaemons" "$STAGE/LaunchAgents"

# --- arm64 .app: binaries + bundled arm64 GTK closure (the base tree) ---
for b in nullgate-daemon nullgate nullgate-cli; do
  install -m 0755 "$ARM_BIN/$b" "$CONTENTS/MacOS/$b"
done
BUNDLE_PREFIX="$ARM_ENV" BUNDLE_BINDIR=MacOS "$ROOT/scripts/bundle-gtk-macos.sh" "$CONTENTS"

# --- universal: build a parallel x86_64 .app, then lipo each Mach-O into the base.
if [ "$UNIVERSAL" = 1 ]; then
  echo "package-macos: bundling x86_64 closure + lipo'ing into the universal app"
  X86_STAGE="$(mktemp -d)/x86"
  X86_CONTENTS="$X86_STAGE/Contents"
  mkdir -p "$X86_CONTENTS/MacOS"
  for b in nullgate-daemon nullgate nullgate-cli; do
    install -m 0755 "$X86_BIN/$b" "$X86_CONTENTS/MacOS/$b"
  done
  # SKIP_AUX: loaders.cache/schemas/fontconfig are arch-independent and already in
  # the base arm64 tree — only the x86_64 dylib closure is needed for the lipo.
  BUNDLE_PREFIX="$X86_ENV" BUNDLE_BINDIR=MacOS BUNDLE_SKIP_AUX=1 "$ROOT/scripts/bundle-gtk-macos.sh" "$X86_CONTENTS"

  # lipo every Mach-O present in both trees (binaries, dylibs, pixbuf loaders).
  lipo_merge() {
    local rel="$1" arm="$CONTENTS/$1" x86="$X86_CONTENTS/$1"
    [ -f "$x86" ] || { echo "package-macos: WARNING x86_64 missing $rel (arm64-only in fat binary)"; return; }
    lipo -create "$arm" "$x86" -output "$arm.fat" && mv -f "$arm.fat" "$arm"
  }
  for b in nullgate-daemon nullgate nullgate-cli; do lipo_merge "MacOS/$b"; done
  for f in "$CONTENTS"/lib/*.dylib; do lipo_merge "lib/$(basename "$f")"; done
  for f in "$CONTENTS"/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.so; do
    lipo_merge "lib/gdk-pixbuf-2.0/2.10.0/loaders/$(basename "$f")"
  done
  # x86_64-only dylibs the arm64 closure didn't pull in (rare) — copy + sign them.
  for f in "$X86_CONTENTS"/lib/*.dylib; do
    base="$(basename "$f")"
    [ -f "$CONTENTS/lib/$base" ] || { cp -f "$f" "$CONTENTS/lib/$base"; echo "package-macos: added x86_64-only $base"; }
  done
  rm -rf "$(dirname "$X86_STAGE")"

  # lipo invalidated every ad-hoc signature — re-sign inside-out.
  echo "package-macos: re-signing after lipo"
  for f in "$CONTENTS"/lib/*.dylib "$CONTENTS"/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.so; do
    codesign --force --sign - --timestamp=none "$f" >/dev/null 2>&1 || { echo "re-sign failed: $f" >&2; exit 1; }
  done
  for b in nullgate nullgate-daemon nullgate-cli; do
    codesign --force --sign - --timestamp=none "$CONTENTS/MacOS/$b" >/dev/null 2>&1
  done
fi

# Info.plist (CFBundleVersion from Cargo).
sed "s/__VERSION__/$VERSION/g" "$PKG_SRC/Info.plist" > "$CONTENTS/Info.plist"

# AppIcon.icns from the per-size app icons (slot:source-size). The set ships a
# native 1024 for the 512x512@2x slot, so nothing is upscaled.
ICONSET="$(mktemp -d)/AppIcon.iconset"; mkdir -p "$ICONSET"
for spec in 16x16:16 16x16@2x:32 32x32:32 32x32@2x:64 128x128:128 128x128@2x:256 \
            256x256:256 256x256@2x:512 512x512:512 512x512@2x:1024; do
  nm="${spec%%:*}"; sz="${spec##*:}"
  cp "$ROOT/img/nullgate-icon-${sz}.png" "$ICONSET/icon_$nm.png"
done
iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"
echo "package-macos: wrote AppIcon.icns"

# Seal the bundle (ad-hoc). Nested dylibs/helpers are already individually signed.
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 \
  || { echo "package-macos: bundle codesign failed" >&2; exit 1; }

# Verify the arch(es) actually landed.
echo "package-macos: nullgate arches -> $(lipo -archs "$CONTENTS/MacOS/nullgate" 2>/dev/null)"

# Tarball-root extras: bootstrap manager, docs, launchd templates.
install -m 0755 "$PKG_SRC/nullgatectl"      "$STAGE/nullgatectl"
install -m 0644 "$PKG_SRC/INSTALL.txt" "$STAGE/INSTALL.txt"
install -m 0644 "$ROOT/LICENSE"        "$STAGE/LICENSE"
install -m 0644 "$PKG_SRC/$APP_ID.daemon.plist" "$STAGE/LaunchDaemons/$APP_ID.daemon.plist"
install -m 0644 "$PKG_SRC/$APP_ID.update.plist" "$STAGE/LaunchDaemons/$APP_ID.update.plist"
install -m 0644 "$PKG_SRC/$APP_ID.gui.plist"    "$STAGE/LaunchAgents/$APP_ID.gui.plist"

# Tarball (preserve the .app's signature/symlinks).
mkdir -p "$ROOT/dist"
tar -czf "$ROOT/dist/$NAME.tar.gz" -C "$ROOT/dist" "$NAME"
echo "package-macos: wrote dist/$NAME.tar.gz ($(du -sh "$ROOT/dist/$NAME.tar.gz" | awk '{print $1}'))"
