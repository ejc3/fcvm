#!/bin/bash
# Fetches the phones' web browser (docs/fleet/ARCHITECTURE.md, "The browser") into
# <aosp.sh --root-path>/phone-work/cromite-<version>-x64.apk, beside Robot Park, checking its
# SHA-256, and prints the path. The phone container installs it from /work (phone.sh browser,
# and serve with PHONE_BROWSER_APK).
#
#   scripts/phone/browser.sh              fetch if needed, print the path
#   scripts/phone/browser.sh --apk-path   the path this checkout's pin fetches to
#
# The browser is Cromite, a Chromium fork that publishes an x86_64 phone build of each release
# (github.com/uazo/cromite, GPL-3.0 for its patches, Chromium's own licenses for the rest),
# run as released. The Chromium project publishes no x86_64 phone build; its one x86_64
# Android build (AndroidDesktop_x64) needs Vulkan for its GPU process, which the phone
# cannot give it (docs/fleet/ARCHITECTURE.md). CROMITE_URL points at another copy of the
# same file.
set -euo pipefail
VERSION=153.0.8010.37
TAG=v153.0.8010.37-11507ac1061b5ea227806f5e84db5a57df6ccf6a
SHA256=f63e0c8e97a1796a82d1d2c18ae13115e5a88184e3911b43f903884fe358306a
URL=${CROMITE_URL:-https://github.com/uazo/cromite/releases/download/$TAG/x64_ChromePublic.apk}
WORK=$("$(dirname "$0")/aosp.sh" --root-path)/phone-work
APK=$WORK/cromite-$VERSION-x64.apk
if [ "${1:-}" = --apk-path ]; then echo "$APK"; exit 0; fi
if [ ! -f "$APK" ]; then
  mkdir -p "$WORK"
  tmp=$(mktemp "$WORK/cromite.XXXXXX")
  trap 'rm -f "$tmp"' EXIT
  curl -fsSL -o "$tmp" "$URL"
  echo "$SHA256  $tmp" | sha256sum -c --status || { echo "browser.sh: $URL does not have SHA-256 $SHA256" >&2; exit 1; }
  chmod 0644 "$tmp"
  # published by rename: a run never installs a partial file
  mv "$tmp" "$APK"
fi
echo "$APK"
