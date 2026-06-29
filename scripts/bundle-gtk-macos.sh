#!/usr/bin/env bash
# Bundle the GTK4/libadwaita dylib closure into a self-contained macOS tree.
#
#   scripts/bundle-gtk-macos.sh <stage-dir>
#   BUNDLE_BINDIR=MacOS scripts/bundle-gtk-macos.sh <app>/Contents
#
# <stage-dir> must already contain <bindir>/{ipn,ipn-daemon,ipn-cli}, where
# <bindir> is "bin" (plain tree) or "MacOS" (a .app's Contents/, via BUNDLE_BINDIR).
# Either way the bindir is one level under <stage-dir>, so @executable_path/../lib
# resolves the same. Only `ipn` (the GUI) links Homebrew dylibs (the daemon/CLI are
# system-only), so the closure is walked from `ipn` plus the gdk-pixbuf + librsvg
# loader modules (dlopened at runtime, so not in the link graph).
#
# Steps:
#   1. Walk the otool closure of `ipn` + the pixbuf/svg loaders.
#   2. Copy every non-system dylib into lib/ (flattened by basename).
#   3. Rewrite install names to @executable_path/../lib/<name> (binary + dylibs +
#      loaders); set each dylib's id.
#   4. Ad-hoc re-sign every dylib/loader/binary, inside-out — mandatory on Apple
#      Silicon (relocation invalidates the linker's ad-hoc signature -> Killed: 9).
#   5. Regenerate the gdk-pixbuf loaders.cache (relative module names).
#   6. Compile the GSettings schemas (gtk4/glib/libadwaita) into share/.
#   7. Bundle the fontconfig config.
#
# bash 3.2 compatible (macOS system bash) — no associative arrays.
set -euo pipefail

STAGE="${1:?usage: bundle-gtk-macos.sh <stage-dir>}"
STAGE="$(cd "$STAGE" && pwd)"
BINDIR="${BUNDLE_BINDIR:-bin}"
BIN="$STAGE/$BINDIR"
BREW="${BUNDLE_BREW:-$(brew --prefix)}"
LIBDIR="$STAGE/lib"
LOADER_REL="lib/gdk-pixbuf-2.0/2.10.0/loaders"
LOADERDIR="$STAGE/$LOADER_REL"
CACHE="$STAGE/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
SCHEMA_DST="$STAGE/share/glib-2.0/schemas"

log() { printf 'bundle-gtk: %s\n' "$*"; }
die() { printf 'bundle-gtk: error: %s\n' "$*" >&2; exit 1; }

[ -x "$BIN/ipn" ] || die "no $BINDIR/ipn under $STAGE"
command -v install_name_tool >/dev/null || die "install_name_tool missing (need Xcode CLT)"

mkdir -p "$LIBDIR" "$LOADERDIR" "$SCHEMA_DST"

realof() { perl -MCwd -e 'print Cwd::abs_path($ARGV[0]), "\n"' "$1"; }

nonsys_deps() {
  otool -L "$1" | tail -n +2 | awk '{print $1}' | while IFS= read -r d; do
    case "$d" in
      "$BREW"/*) printf '%s\n' "$d" ;;
      @rpath/*)  printf '%s\n' "$d" ;;
    esac
  done
}

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

# --- 1+2. Walk the closure, copying each non-system dylib into lib/ by basename. ---
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

QUEUE+=("$BIN/ipn")
for l in "${LOADERS[@]}"; do QUEUE+=("$l"); done

i=0
while [ "$i" -lt "${#QUEUE[@]}" ]; do
  cur="${QUEUE[$i]}"; i=$((i + 1))
  while IFS= read -r dep; do
    [ -n "$dep" ] && ingest "$dep"
  done < <(nonsys_deps "$cur")
done
log "bundled $(find "$LIBDIR" -maxdepth 1 -name '*.dylib' | wc -l | tr -d ' ') dylibs + ${#LOADERS[@]} loader modules"

# --- 3. Relocate to @executable_path/../lib/<name>. ---
relocate() {
  local f="$1" dep base
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    base="$(basename "$dep")"
    install_name_tool -change "$dep" "@executable_path/../lib/$base" "$f" 2>/dev/null
  done < <(nonsys_deps "$f")
}

relocate "$BIN/ipn"
for f in "$LIBDIR"/*.dylib; do
  relocate "$f"
  install_name_tool -id "@executable_path/../lib/$(basename "$f")" "$f" 2>/dev/null
done
for l in "${LOADERS[@]}"; do
  relocate "$l"
  install_name_tool -id "@executable_path/../$LOADER_REL/$(basename "$l")" "$l" 2>/dev/null
done

# --- Verify no Homebrew/@rpath references survive before signing. ---
check_leaks() {
  { otool -L "$BIN/ipn"; \
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
for b in ipn ipn-daemon ipn-cli; do
  [ -f "$BIN/$b" ] && codesign --force --sign - --timestamp=none "$BIN/$b" >/dev/null 2>&1
done

# --- 5. gdk-pixbuf loaders.cache (relative module names). ---
QUERY="$(command -v gdk-pixbuf-query-loaders || true)"
[ -n "$QUERY" ] || QUERY="$BREW/opt/gdk-pixbuf/bin/gdk-pixbuf-query-loaders"
if [ -x "$QUERY" ]; then
  cp -f "$QUERY" "$BIN/.query-loaders"
  ( cd "$LOADERDIR" && GDK_PIXBUF_MODULEDIR=. "$BIN/.query-loaders" ./*.so ) > "$CACHE"
  rm -f "$BIN/.query-loaders"
  log "wrote loaders.cache ($(grep -c '\.so' "$CACHE" 2>/dev/null || echo 0) modules)"
else
  log "WARNING: gdk-pixbuf-query-loaders not found; loaders.cache not generated"
fi

# --- 6. GSettings schemas. ---
for pkg in gtk4 glib libadwaita; do
  src="$BREW/opt/$pkg/share/glib-2.0/schemas"
  [ -d "$src" ] || continue
  for x in "$src"/*.gschema.xml "$src"/*.enums.xml; do
    [ -e "$x" ] && cp -f "$(realof "$x")" "$SCHEMA_DST/$(basename "$x")"
  done
done
COMPILE="$(command -v glib-compile-schemas || true)"
[ -n "$COMPILE" ] || COMPILE="$BREW/opt/glib/bin/glib-compile-schemas"
"$COMPILE" "$SCHEMA_DST" || die "glib-compile-schemas failed"
log "compiled $(ls "$SCHEMA_DST"/*.xml 2>/dev/null | wc -l | tr -d ' ') schemas"

# --- 7. fontconfig config. ---
if [ -f "$BREW/etc/fonts/fonts.conf" ]; then
  mkdir -p "$STAGE/etc/fonts"
  cp -f "$BREW/etc/fonts/fonts.conf" "$STAGE/etc/fonts/fonts.conf"
  [ -d "$BREW/etc/fonts/conf.d" ] && { mkdir -p "$STAGE/etc/fonts/conf.d"; cp -RL "$BREW/etc/fonts/conf.d/." "$STAGE/etc/fonts/conf.d/" 2>/dev/null || true; }
  log "bundled fontconfig config"
fi

log "OK — self-contained ($(du -sh "$LIBDIR" | awk '{print $1}') in lib/)"
