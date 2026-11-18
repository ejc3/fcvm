#!/usr/bin/env bash
# Install the static ffmpeg the viewer smoke decodes with (stage-smoke.py decode) into DIR, from
# a pinned ffmpeg-static GitHub release checked against its SHA-256, and print DIR.
#
# Not apt: on the GitHub-hosted runner the Azure Ubuntu mirror served ffmpeg's 62.8 MB at
# 75.7 kB/s for 13m50s in the release gate's first smoke of 2026-11-18, on the path to a rollback.
#
#     fleet/coord/install-ffmpeg.sh DIR
set -euo pipefail

dir=${1:?usage: install-ffmpeg.sh DIR}
release=https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1
case "$(uname -m)" in
  x86_64) asset=ffmpeg-linux-x64.gz sha=bfe8a8fc511530457b528c48d77b5737527b504a3797a9bc4866aeca69c2dffa ;;
  aarch64) asset=ffmpeg-linux-arm64.gz sha=754a678672298bc68156adff58aa7385a592c2b30b1d0ae8750c45c915c4bac0 ;;
  *) echo "install-ffmpeg.sh: no pinned ffmpeg for $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$dir"
part="$dir/$asset.part"
rm -f "$part" "$dir/ffmpeg"
curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 -o "$part" "$release/$asset"
if ! printf '%s  %s\n' "$sha" "$part" | sha256sum -c --status; then
  rm -f "$part"
  echo "install-ffmpeg.sh: $asset does not match its pinned SHA-256" >&2
  exit 1
fi
gzip -dc "$part" > "$dir/ffmpeg.tmp"
rm -f "$part"
chmod 755 "$dir/ffmpeg.tmp"
mv "$dir/ffmpeg.tmp" "$dir/ffmpeg"
# The smoke decodes H.264 only.
if ! "$dir/ffmpeg" -hide_banner -decoders | awk '$2 == "h264" { found = 1 } END { exit !found }'; then
  echo "install-ffmpeg.sh: the installed ffmpeg has no H.264 decoder" >&2
  exit 1
fi
echo "$dir"
