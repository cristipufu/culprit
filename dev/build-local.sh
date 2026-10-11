#!/usr/bin/env bash
# Build the desktop app and the CLI for this machine, signed with a stable identity.
#
# macOS remembers "Always Allow" for a keychain item per code signature. Unsigned (ad-hoc) builds get a new signature on
# every build, so each rebuild asks for the keychain password again. Signing every local build with the same identity
# makes one "Always Allow" stick.
#
# Uses $CULPRIT_SIGNING_IDENTITY when set, otherwise the first "Developer ID Application" identity in your keychain,
# otherwise ad-hoc signing (prompts return after each rebuild).
#
#   dev/build-local.sh          # app + CLI
#   dev/build-local.sh cli      # CLI only
set -euo pipefail
cd "$(dirname "$0")/.."

identity="${CULPRIT_SIGNING_IDENTITY:-}"
if [[ -z "$identity" && "$(uname)" == "Darwin" ]]; then
  identity=$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)
fi

cargo build --release -p culprit-cli
if [[ "$(uname)" == "Darwin" ]]; then
  codesign --force --options runtime --sign "${identity:--}" target/release/culprit
fi

if [[ "${1:-all}" != "cli" ]]; then
  (cd app && APPLE_SIGNING_IDENTITY="${identity:--}" bunx tauri build --bundles app)
fi

if [[ -n "$identity" ]]; then
  echo "Signed with your Developer ID identity: answer \"Always Allow\" once per keychain prompt and it sticks across rebuilds."
else
  echo "Signed ad-hoc: macOS will ask for keychain access again after each rebuild. Set CULPRIT_SIGNING_IDENTITY to avoid that."
fi
