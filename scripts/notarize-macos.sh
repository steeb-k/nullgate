#!/usr/bin/env bash
# Notarize a signed Nullgate.app and staple the ticket to it.
#
#   scripts/notarize-macos.sh dist/nullgate-<ver>-macos-universal/Nullgate.app
#
# Called by package-macos.sh (NULLGATE_NOTARIZE=1) after the bundle is sealed
# with a Developer ID and BEFORE it is tarred: the updater downloads a tarball,
# and a ticket stapled to a disk image does not travel inside one, so the ticket
# goes on the bundle itself and Gatekeeper can check it with no network at all.
#
# Notarization is not the App Store. It is the other half of Developer ID - the
# arrangement for software distributed outside the store - and involves no
# listing and no review. The credential is nonetheless called an "App Store
# Connect" one. Either form works:
#
#   NOTARY_KEY / NOTARY_KEY_ID / NOTARY_ISSUER_ID       an API key (.p8 contents)
#   NOTARY_APPLE_ID / NOTARY_PASSWORD / NOTARY_TEAM_ID  an app-specific password
set -euo pipefail
app="${1:-}"
[ -d "$app" ] || { echo "notarize-macos: usage: notarize-macos.sh <Nullgate.app>" >&2; exit 2; }
if [ -n "${NOTARY_KEY:-}" ]; then
  for r in NOTARY_KEY_ID NOTARY_ISSUER_ID; do [ -n "${!r:-}" ] || { echo "notarize-macos: $r is not set, and NOTARY_KEY is" >&2; exit 1; }; done
elif [ -n "${NOTARY_PASSWORD:-}" ]; then
  for r in NOTARY_APPLE_ID NOTARY_TEAM_ID; do [ -n "${!r:-}" ] || { echo "notarize-macos: $r is not set, and NOTARY_PASSWORD is" >&2; exit 1; }; done
else
  echo "notarize-macos: no notary credentials (NOTARY_KEY or NOTARY_PASSWORD)" >&2; exit 1
fi
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
# The bundle must be sealed with a hardened-runtime Developer ID signature or the
# service rejects it with a log entry that names every offending file. Check the
# cheap half here so a bad signature fails in seconds, not after the upload.
codesign --verify --deep --strict --verbose=2 "$app"
if [ -n "${NOTARY_KEY:-}" ]; then
  printf '%s' "$NOTARY_KEY" > "$work/notary.p8"
  set -- --key "$work/notary.p8" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID"
else
  set -- --apple-id "$NOTARY_APPLE_ID" --password "$NOTARY_PASSWORD" --team-id "$NOTARY_TEAM_ID"
fi
ditto -c -k --keepParent "$app" "$work/notarize.zip"
echo "notarize-macos: submitting $(basename "$app") to the notary service (this takes minutes)"
xcrun notarytool submit "$work/notarize.zip" "$@" --wait --output-format json > "$work/submit.json" || true
cat "$work/submit.json"
field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2],""))' "$work/submit.json" "$1"; }
id="$(field id)"; status="$(field status)"
if [ "$status" != "Accepted" ]; then
  echo "notarize-macos: the notary service answered '${status:-nothing}'" >&2
  # The log is the only place the reason lives (unsigned dylib, missing
  # timestamp, no hardened runtime, ...). Print it whole.
  [ -n "$id" ] && xcrun notarytool log "$id" "$@" >&2 || true
  exit 1
fi
xcrun stapler staple "$app"
xcrun stapler validate "$app"
echo "notarize-macos: $(basename "$app") is notarized and stapled"
