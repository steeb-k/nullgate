#!/usr/bin/env bash
# Bundle the GTK4/libadwaita dylib closure into a self-contained macOS tree.
#
#   scripts/bundle-gtk-macos.sh <stage-dir>
#   BUNDLE_BINDIR=MacOS scripts/bundle-gtk-macos.sh <app>/Contents
#
# <stage-dir> must already contain <bindir>/{nullgate,nullgate-daemon,nullgate-cli},
# where <bindir> is "bin" (plain tarball tree) or "MacOS" (a .app's Contents/, via
# BUNDLE_BINDIR). Either way the bindir is one level under <stage-dir>, so the
# @executable_path/../lib relocation resolves the same. Only `nullgate` (the GUI)
# links GTK dylibs (the daemon/CLI are system-only), so the closure is walked from
# nullgate plus the gdk-pixbuf + librsvg loader modules (dlopened at runtime, so
# not in the link graph).
#
# Steps (mirrors docs/macos-packaging.md "the bundling process"):
#   1. Walk the otool closure of nullgate + the pixbuf/svg loaders.
#   2. Copy every non-system dylib into lib/ (flattened by basename).
#   3. Rewrite install names to @executable_path/../lib/<name> (binary + dylibs +
#      loaders); set each dylib's id. @executable_path is stable for transitively
#      loaded dylibs too, so no LC_RPATH is needed.
#   4. Ad-hoc re-sign every dylib/loader/binary, inside-out — mandatory on Apple
#      Silicon (relocation invalidates the linker's ad-hoc signature → Killed: 9).
#   5. Regenerate the gdk-pixbuf loaders.cache (relative module names).
#   6. Compile the GSettings schemas (gtk4/glib/libadwaita) into share/.
#   7. Bundle the fontconfig config.
#
# Source-agnostic: a conda-forge env (what we ship from — minos 11) references
# siblings via @rpath/@loader_path; a Homebrew prefix uses absolute
# /opt/homebrew/opt/<f>/lib/<n> plus @rpath (librsvg). All are handled.
# bash 3.2 compatible (macOS system bash) — no associative arrays.
set -euo pipefail

STAGE="${1:?usage: bundle-gtk-macos.sh <stage-dir>}"
STAGE="$(cd "$STAGE" && pwd)"
# Directory holding the executables, relative to STAGE. "bin" for the plain
# tarball tree; "MacOS" when STAGE is a .app's Contents/ (set BUNDLE_BINDIR=MacOS).
BINDIR="${BUNDLE_BINDIR:-bin}"
BIN="$STAGE/$BINDIR"

log() { printf 'bundle-gtk: %s\n' "$*"; }
die() { printf 'bundle-gtk: error: %s\n' "$*" >&2; exit 1; }

# Prefix to source the GTK closure from. Despite the legacy name `BREW`, this is
# any prefix with the usual lib/ share/ etc/ layout: a conda-forge env (what we
# ship from — its dylibs carry a macOS-11 minos regardless of the build host's OS)
# or a Homebrew prefix. Resolution order:
#   BUNDLE_PREFIX  (conda env or any prefix; preferred)
#   BUNDLE_BREW    (legacy alias; an x86_64 brew at /usr/local)
#   `brew --prefix` (active Homebrew, if present)
# conda's flat lib/ is a subset of Homebrew's keg-only tree, so one resolver covers
# both — the Homebrew-only $BREW/opt/* fallbacks simply no-op on a conda env.
BREW="${BUNDLE_PREFIX:-${BUNDLE_BREW:-$(brew --prefix 2>/dev/null || true)}}"
[ -n "$BREW" ] || die "no source prefix (set BUNDLE_PREFIX=<conda env or brew prefix>)"
LIBDIR="$STAGE/lib"
LOADER_REL="lib/gdk-pixbuf-2.0/2.10.0/loaders"
LOADERDIR="$STAGE/$LOADER_REL"
CACHE="$STAGE/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
SCHEMA_DST="$STAGE/share/glib-2.0/schemas"

[ -x "$BIN/nullgate" ] || die "no $BINDIR/nullgate under $STAGE"
command -v install_name_tool >/dev/null || die "install_name_tool missing (need Xcode CLT)"

mkdir -p "$LIBDIR" "$LOADERDIR" "$SCHEMA_DST"

# Resolve a (possibly symlinked) path to its real file, dependency-free.
realof() { perl -MCwd -e 'print Cwd::abs_path($ARGV[0]), "\n"' "$1"; }

# Each non-system dependency load string of a Mach-O: absolute prefix paths
# (/opt/homebrew or the conda env), @rpath/ ones (Homebrew's & conda's relocatable
# dylibs), and @loader_path/ ones (conda uses these for some intra-prefix deps).
# System libs (/usr/lib, /System) and already-bundled @executable_path refs are
# skipped (resolve_dep maps the rest to real files by basename).
nonsys_deps() {
  otool -L "$1" | tail -n +2 | awk '{print $1}' | while IFS= read -r d; do
    case "$d" in
      "$BREW"/*)      printf '%s\n' "$d" ;;
      @rpath/*)       printf '%s\n' "$d" ;;
      @loader_path/*) printf '%s\n' "$d" ;;
    esac
  done
}

# Resolve a dependency load string to a real file on disk. Absolute prefix paths
# resolve directly; @rpath/<name> and @loader_path/<name> are searched by basename
# under the prefix's lib/ (conda: flat lib/; Homebrew: lib/ then keg-only opt/*/lib).
# Echoes nothing if unresolved.
resolve_dep() {
  local d="$1" base hit
  base="$(basename "$d")"
  case "$d" in
    "$BREW"/*) realof "$d"; return ;;
  esac
  if [ -f "$BREW/lib/$base" ]; then realof "$BREW/lib/$base"; return; fi
  for hit in "$BREW"/opt/*/lib/"$base"; do
    [ -f "$hit" ] && { realof "$hit"; return; }
  done
}

# --- 1+2. Walk the closure, copying each non-system dylib into lib/ by basename.
# Dedup is "is lib/<basename> already present?" (no associative array needed). ---
QUEUE=()

ingest() {
  local dep="$1" base real
  base="$(basename "$dep")"
  [ -f "$LIBDIR/$base" ] && return 0
  real="$(resolve_dep "$dep")"
  { [ -n "$real" ] && [ -f "$real" ]; } || { log "WARNING: cannot resolve '$dep' (skipped)"; return 0; }
  cp -f "$real" "$LIBDIR/$base"
  chmod u+w "$LIBDIR/$base"
  QUEUE+=("$LIBDIR/$base")
}

# Roots of the dlopen graph that aren't in nullgate's link closure: the loader
# modules GTK loads at runtime. Copy them in, then treat them as walk roots.
LOADERS=()
copy_loaders_from() {
  local dir="$1" f base
  [ -d "$dir" ] || return 0
  for f in "$dir"/*.so; do
    [ -e "$f" ] || continue
    base="$(basename "$f")"
    cp -f "$f" "$LOADERDIR/$base"
    chmod u+w "$LOADERDIR/$base"
    LOADERS+=("$LOADERDIR/$base")
  done
}
copy_loaders_from "$BREW/lib/gdk-pixbuf-2.0/2.10.0/loaders"
[ -d "$BREW/opt/librsvg/lib/gdk-pixbuf-2.0/2.10.0/loaders" ] \
  && copy_loaders_from "$BREW/opt/librsvg/lib/gdk-pixbuf-2.0/2.10.0/loaders"

# Seed the worklist with the GUI binary + every loader module.
QUEUE+=("$BIN/nullgate")
for l in "${LOADERS[@]}"; do QUEUE+=("$l"); done

# Breadth-first: pop, copy each dep, which appends new dylibs to the queue.
i=0
while [ "$i" -lt "${#QUEUE[@]}" ]; do
  cur="${QUEUE[$i]}"; i=$((i + 1))
  while IFS= read -r dep; do
    [ -n "$dep" ] && ingest "$dep"
  done < <(nonsys_deps "$cur")
done
log "bundled $(find "$LIBDIR" -maxdepth 1 -name '*.dylib' | wc -l | tr -d ' ') dylibs + ${#LOADERS[@]} loader modules"

# --- 3. Relocate: rewrite every non-system load path to @executable_path/../lib/
# <name>. install_name_tool warns it's invalidating the ad-hoc signature on each
# edit — expected, we re-sign next; silence the noise. ---
relocate() {
  local f="$1" dep base
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    base="$(basename "$dep")"
    install_name_tool -change "$dep" "@executable_path/../lib/$base" "$f" 2>/dev/null
  done < <(nonsys_deps "$f")
}

relocate "$BIN/nullgate"
for f in "$LIBDIR"/*.dylib; do
  relocate "$f"
  install_name_tool -id "@executable_path/../lib/$(basename "$f")" "$f" 2>/dev/null
done
for l in "${LOADERS[@]}"; do
  relocate "$l"
  install_name_tool -id "@executable_path/../$LOADER_REL/$(basename "$l")" "$l" 2>/dev/null
done

# --- Verify no source-prefix/@rpath references survive before signing. ---
check_leaks() {
  { otool -L "$BIN/nullgate"; \
    for f in "$LIBDIR"/*.dylib "${LOADERS[@]}"; do otool -L "$f"; done; } \
    | awk '{print $1}' | grep -E "^$BREW|^@rpath/" || true
}
LEAKS="$(check_leaks | sort -u)"
[ -z "$LEAKS" ] || { printf '%s\n' "$LEAKS" >&2; die "unbundled references remain (see above)"; }

# --- 4. Ad-hoc re-sign, inside-out: dylibs + loaders first, then the binaries. ---
log "re-signing (ad-hoc)..."
for f in "$LIBDIR"/*.dylib "${LOADERS[@]}"; do
  codesign --force --sign - --timestamp=none "$f" >/dev/null 2>&1 || die "codesign failed on $f"
done
for b in nullgate nullgate-daemon nullgate-cli; do
  [ -f "$BIN/$b" ] && codesign --force --sign - --timestamp=none "$BIN/$b" >/dev/null 2>&1
done

# Steps 5–7 (loaders.cache, schemas, fontconfig) produce arch-independent files.
# In the universal x86_64 pass they'd be redundant (the arm64 pass already wrote
# them into the base tree) AND would force running x86_64 query-loaders under
# Rosetta — so package-macos.sh sets BUNDLE_SKIP_AUX=1 there. Only the x86_64 dylib
# closure (steps 1–4 above) is needed from that pass for the lipo merge.
if [ "${BUNDLE_SKIP_AUX:-0}" = 1 ]; then
  log "BUNDLE_SKIP_AUX=1 — skipping loaders.cache/schemas/fontconfig (reusing base tree's)"
  log "OK — dylib closure only ($(du -sh "$LIBDIR" | awk '{print $1}') in lib/)"
  exit 0
fi

# Tool discovery: prefer the prefix's own bin/ (correct version & arch for this
# closure) over whatever happens to be first on PATH (e.g. an unrelated Homebrew).
find_tool() { # <name> [legacy keg-only relpath]
  local name="$1" keg="${2:-}"
  [ -x "$BREW/bin/$name" ] && { printf '%s\n' "$BREW/bin/$name"; return; }
  [ -n "$keg" ] && [ -x "$BREW/$keg" ] && { printf '%s\n' "$BREW/$keg"; return; }
  command -v "$name" 2>/dev/null || true
}

# --- 5. gdk-pixbuf loaders.cache. query-loaders dlopens each (now relocated +
# signed) module to read its format info, so @executable_path must resolve to the
# bundle: run a *temporary* copy of query-loaders from the stage's bindir (where
# @executable_path/../lib == our lib/). Relative module names (loaders/<name>) are
# resolved at runtime via GDK_PIXBUF_MODULEDIR (set by setup_runtime_env). ---
QUERY="$(find_tool gdk-pixbuf-query-loaders opt/gdk-pixbuf/bin/gdk-pixbuf-query-loaders)"
if [ -n "$QUERY" ] && [ -x "$QUERY" ]; then
  cp -f "$QUERY" "$BIN/.query-loaders"
  ( cd "$LOADERDIR" && GDK_PIXBUF_MODULEDIR=. "$BIN/.query-loaders" ./*.so ) > "$CACHE"
  rm -f "$BIN/.query-loaders"
  log "wrote loaders.cache ($(grep -c '\.so' "$CACHE" 2>/dev/null || echo 0) modules)"
else
  log "WARNING: gdk-pixbuf-query-loaders not found; loaders.cache not generated"
fi

# --- 6. GSettings schemas: collect the XML, compile. conda keeps all schemas in
# one share/glib-2.0/schemas; Homebrew splits them per keg-only formula. Cover both. ---
SCHEMA_SRCS=("$BREW/share/glib-2.0/schemas")
for pkg in gtk4 glib libadwaita; do
  SCHEMA_SRCS+=("$BREW/opt/$pkg/share/glib-2.0/schemas")
done
for src in "${SCHEMA_SRCS[@]}"; do
  [ -d "$src" ] || continue
  for x in "$src"/*.gschema.xml "$src"/*.enums.xml; do
    [ -e "$x" ] && cp -f "$(realof "$x")" "$SCHEMA_DST/$(basename "$x")"
  done
done
COMPILE="$(find_tool glib-compile-schemas opt/glib/bin/glib-compile-schemas)"
[ -n "$COMPILE" ] || die "glib-compile-schemas not found"
"$COMPILE" "$SCHEMA_DST" || die "glib-compile-schemas failed"
log "compiled $(ls "$SCHEMA_DST"/*.xml 2>/dev/null | wc -l | tr -d ' ') schemas"

# --- 7. fontconfig config: the bundled libfontconfig's compiled-in config dir is
# under the build prefix (absent on a user's Mac). Ship the prefix's fonts.conf
# (it points at the system macOS font dirs, present everywhere); setup_runtime_env
# sets FONTCONFIG_PATH to find it. conf.d includes are ignore_missing, and the
# stale build-prefix cachedir falls through to the xdg ~/.cache/fontconfig entry. ---
if [ -f "$BREW/etc/fonts/fonts.conf" ]; then
  mkdir -p "$STAGE/etc/fonts"
  cp -f "$BREW/etc/fonts/fonts.conf" "$STAGE/etc/fonts/fonts.conf"
  [ -d "$BREW/etc/fonts/conf.d" ] && { mkdir -p "$STAGE/etc/fonts/conf.d"; cp -RL "$BREW/etc/fonts/conf.d/." "$STAGE/etc/fonts/conf.d/" 2>/dev/null || true; }
  log "bundled fontconfig config"
fi

log "OK — self-contained ($(du -sh "$LIBDIR" | awk '{print $1}') in lib/)"
