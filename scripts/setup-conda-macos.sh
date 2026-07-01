#!/usr/bin/env bash
# Create the conda-forge GTK env(s) that scripts/package-macos.sh bundles from.
#
#   scripts/setup-conda-macos.sh              # osx-arm64 env only (arm64 build)
#   scripts/setup-conda-macos.sh --universal  # + osx-64 env (arm64 + Intel lipo)
#
# Why conda-forge instead of Homebrew: conda-forge builds its osx-arm64 packages
# against the macOS 11 SDK (Big Sur — the floor for ALL Apple Silicon) and osx-64
# against ~10.13, so the GTK dylibs we bundle carry a macOS-11 `minos` no matter
# what macOS THIS machine runs. Homebrew instead stamps the build host's OS, which
# on a modern dev box would force a much higher floor (an arm64 Homebrew gtk4 on
# macOS 26 carries minos 26 — unrunnable on any older Mac). See docs/macos-packaging.md.
#
# Requires a conda/mamba/micromamba on PATH. miniforge is the lightweight, fully
# conda-forge option: https://github.com/conda-forge/miniforge — or a standalone
# micromamba binary (put its dir on PATH and set MAMBA_ROOT_PREFIX).
#
# Envs are created at .conda-gtk/{arm64,x86} (override via NULLGATE_CONDA_ARM/_X86).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARM_ENV="${NULLGATE_CONDA_ARM:-$ROOT/.conda-gtk/arm64}"
X86_ENV="${NULLGATE_CONDA_X86:-$ROOT/.conda-gtk/x86}"
# librsvg → the SVG gdk-pixbuf loader; pkg-config → building the Rust slices.
# zlib/freetype/expat → their .pc files: conda-forge ships these separately from
# the runtime libs (libzlib/libfreetype/…) that gtk4 pulls, but gio/harfbuzz/
# fontconfig list them in Requires.private, so the Rust *-sys builds need the .pc.
# libintl-devel → the unversioned libintl.dylib symlink the linker needs for the
# `-lintl` that glib's .pc emits (the env otherwise has only libintl.8.dylib).
# (libxml2 is pulled transitively by libadwaita→appstream; see synth_libxml_pc.)
PKGS="gtk4 libadwaita librsvg pkg-config zlib freetype expat libintl-devel"

UNIVERSAL=0; [ "${1:-}" = "--universal" ] && UNIVERSAL=1

CONDA=""
for c in mamba micromamba conda; do
  command -v "$c" >/dev/null 2>&1 && { CONDA="$c"; break; }
done
[ -n "$CONDA" ] || {
  echo "setup-conda-macos: need conda/mamba/micromamba on PATH." >&2
  echo "  install miniforge: https://github.com/conda-forge/miniforge" >&2
  exit 1
}

# conda-forge's libxml2 >= 2.14 ships only the runtime dylib — no headers and no
# libxml-2.0.pc. appstream (pulled by libadwaita) lists libxml-2.0 in
# Requires.private, and pkg-config errors on a missing private dep even for a
# dynamic build, so the libadwaita-1 probe fails. libxml-2.0 is never linked
# directly (private only), so a minimal, parseable .pc is enough to satisfy the
# graph. Synthesize it if absent rather than pinning the whole stack to old libxml2
# (which drags gtk4 down to 4.14 / triggers icu/zlib conflicts).
synth_libxml_pc() { # <env-path>
  local env="$1" pc="$1/lib/pkgconfig/libxml-2.0.pc" ver
  [ -f "$pc" ] && return 0
  ls "$env"/lib/libxml2*.dylib >/dev/null 2>&1 || return 0   # no libxml2 at all → leave it
  ver="$("$CONDA" list -p "$env" 2>/dev/null | awk '$1=="libxml2"{print $2; exit}')"
  cat > "$pc" <<EOF
prefix=$env
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: libXML
Version: ${ver:-2.14.0}
Description: libXML library version2 (synthesized by setup-conda-macos.sh).
Libs: -L\${libdir} -lxml2
Cflags: -I\${includedir}/libxml2
EOF
  echo "setup-conda-macos: synthesized libxml-2.0.pc (libxml2 ${ver:-?} ships none)"
}

create_env() { # <subdir> <env-path>
  local subdir="$1" env="$2"
  echo "setup-conda-macos: creating $subdir env at $env ($CONDA)"
  rm -rf "$env"
  CONDA_SUBDIR="$subdir" "$CONDA" create -y -p "$env" -c conda-forge $PKGS
  # Pin the platform so any later install into this env keeps the same arch.
  printf 'subdir: %s\n' "$subdir" > "$env/.condarc"
  synth_libxml_pc "$env"
  echo "setup-conda-macos: $subdir env ready"
}

create_env osx-arm64 "$ARM_ENV"
[ "$UNIVERSAL" = 1 ] && create_env osx-64 "$X86_ENV"

echo "setup-conda-macos: done. Now run: scripts/package-macos.sh"
