#!/usr/bin/env bash
# Build a throwaway keychain holding the Developer ID certificate, so that
# `codesign --sign "$CODESIGN_IDENTITY"` works on a runner that has no login
# keychain and no stored identity. Used by the macOS job in
# .github/workflows/build.yml before scripts/package-macos.sh runs.
#
# Environment (all from repository secrets):
#   CODESIGN_IDENTITY     'Developer ID Application: Name (TEAMID)'
#   CERTIFICATE_P12       base64 of the certificate + private key (.p12)
#   CERTIFICATE_PASSWORD  the .p12's password
#
# Why a second certificate exists at all: the one on the maintainer's Mac is
# Xcode's cloud-managed kind, whose private key syncs through iCloud Keychain
# and cannot be exported. A second Developer ID Application certificate under
# the SAME team, issued from a CSR made anywhere, is what CI uses. The team is
# what matters, not the certificate: `nullgatectl --update` compares the Team
# ID of an update against the installed app's, so a second certificate under
# the same team updates silently and one under another team is refused.
#
# The keychain is left in place for the rest of the job (package-macos.sh
# signs from it); it lives under $RUNNER_TEMP, which the runner discards.
set -euo pipefail
for required in CODESIGN_IDENTITY CERTIFICATE_P12 CERTIFICATE_PASSWORD; do
  [ -n "${!required:-}" ] || { echo "macos-keychain: $required is not set" >&2; exit 1; }
done
work="${RUNNER_TEMP:-$(mktemp -d)}/nullgate-signing"
mkdir -p "$work"
keychain="$work/build.keychain-db"
keychain_password="$(openssl rand -base64 24)"
security create-keychain -p "$keychain_password" "$keychain"
# Six hours, no auto-lock on sleep: the job must never stall on a locked keychain.
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
printf '%s' "$CERTIFICATE_P12" | base64 --decode > "$work/certificate.p12"
security import "$work/certificate.p12" -k "$keychain" -P "$CERTIFICATE_PASSWORD" \
  -T /usr/bin/codesign -T /usr/bin/security
rm -f "$work/certificate.p12"
# Let codesign use the key without a UI prompt (there is nobody to answer one).
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" >/dev/null
# Put it first in the search list, keeping whatever was there.
security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
if ! security find-identity -v -p codesigning "$keychain" | grep -Fq "$CODESIGN_IDENTITY"; then
  echo "macos-keychain: the keychain holds no identity named '$CODESIGN_IDENTITY':" >&2
  security find-identity -v -p codesigning "$keychain" >&2 || true
  exit 1
fi
echo "macos-keychain: '$CODESIGN_IDENTITY' is ready in $keychain"
# Hand the identity to later steps: package-macos.sh signs with it when set.
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "CODESIGN_IDENTITY=$CODESIGN_IDENTITY" >> "$GITHUB_ENV"
fi
