#!/usr/bin/env bash
# Build the Nullgate (Nullgate) macOS release tarball — a self-contained,
# ad-hoc-signed "Nullgate.app" (bundled GTK). macOS analog of
# package-linux.sh.
#
#   scripts/package-macos.sh                cargo build --release, bundle, package
#   scripts/package-macos.sh --skip-build   package the existing per-arch release bins
#
# Output: dist/nullgate-<version>-macos-<arch>.tar.gz
#   arch = "universal" when an x86_64 Homebrew (/usr/local) + the x86_64 Rust target
#   are present (each Mach-O is lipo'd arm64+x86_64); otherwise "arm64".
#
# Install (via the bundled nullgatectl, which uses sudo) puts the .app in /Applications,
# the daemon as a ROOT LaunchDaemon (it needs root to create the utun interface),
# a root daily auto-update LaunchDaemon, and a per-user tray GUI LaunchAgent.
#
# Requires: cargo, Xcode CLT (install_name_tool/codesign/otool/lipo/iconutil/sips),
# Homebrew gtk4 + libadwaita (arm64; plus x86_64 at /usr/local for universal).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_SRC="$ROOT/packaging/macos"
APP_ID="io.github.steeb_k.Nullgate"
APP_NAME="Nullgate.app"
ARM_BREW="$(brew --prefix)"          # arm64 Homebrew (/opt/homebrew)
X86_BREW="/usr/local"                # x86_64 Homebrew (Rosetta), if present
SKIP_BUILD=0; [ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "package-macos: could not read version from Cargo.toml" >&2; exit 1; }

UNIVERSAL=0
if [ -x "$X86_BREW/bin/brew" ] && [ -d "$X86_BREW/opt/gtk4" ] \
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
  cargo build --release --target "$ARM_TGT" -p ipn-daemon -p ipn-gui -p ipn-cli
  if [ "$UNIVERSAL" = 1 ]; then
    echo "package-macos: building x86_64 slice against $X86_BREW GTK"
    macos_maj="$(sw_vers -productVersion | cut -d. -f1)"
    x86_pc="$(ls -d "$X86_BREW"/opt/*/lib/pkgconfig 2>/dev/null | tr '\n' ':')$X86_BREW/lib/pkgconfig:$X86_BREW/share/pkgconfig:$X86_BREW/Homebrew/Library/Homebrew/os/mac/pkgconfig/$macos_maj"
    PKG_CONFIG_LIBDIR="$x86_pc" \
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
BUNDLE_BREW="$ARM_BREW" BUNDLE_BINDIR=MacOS "$ROOT/scripts/bundle-gtk-macos.sh" "$CONTENTS"

# --- universal: build a parallel x86_64 .app, then lipo each Mach-O into the base.
if [ "$UNIVERSAL" = 1 ]; then
  echo "package-macos: bundling x86_64 closure + lipo'ing into the universal app"
  X86_STAGE="$(mktemp -d)/x86"
  X86_CONTENTS="$X86_STAGE/Contents"
  mkdir -p "$X86_CONTENTS/MacOS"
  for b in nullgate-daemon nullgate nullgate-cli; do
    install -m 0755 "$X86_BIN/$b" "$X86_CONTENTS/MacOS/$b"
  done
  BUNDLE_BREW="$X86_BREW" BUNDLE_BINDIR=MacOS "$ROOT/scripts/bundle-gtk-macos.sh" "$X86_CONTENTS"

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
  for f in "$X86_CONTENTS"/lib/*.dylib; do
    base="$(basename "$f")"
    [ -f "$CONTENTS/lib/$base" ] || { cp -f "$f" "$CONTENTS/lib/$base"; echo "package-macos: added x86_64-only $base"; }
  done
  rm -rf "$(dirname "$X86_STAGE")"

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

# AppIcon.icns from the per-size, hand-tuned app icons (slot:source-size). The
# 1024 slot has no source, so it's upscaled from 512.
ICONSET="$(mktemp -d)/AppIcon.iconset"; mkdir -p "$ICONSET"
for spec in 16x16:16 16x16@2x:32 32x32:32 32x32@2x:64 128x128:128 128x128@2x:256 \
            256x256:256 256x256@2x:512 512x512:512; do
  nm="${spec%%:*}"; sz="${spec##*:}"
  cp "$ROOT/img/icon-stacked-${sz}.png" "$ICONSET/icon_$nm.png"
done
sips -z 1024 1024 "$ROOT/img/icon-stacked-512.png" --out "$ICONSET/icon_512x512@2x.png" >/dev/null 2>&1
iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"
echo "package-macos: wrote AppIcon.icns"

# Seal the bundle (ad-hoc).
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 \
  || { echo "package-macos: bundle codesign failed" >&2; exit 1; }

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
