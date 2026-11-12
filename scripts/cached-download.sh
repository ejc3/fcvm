#!/bin/bash
# cached-download.sh URL SHA256 OUT [CACHE_DIR]
#
# Copy a pinned file to OUT from CACHE_DIR, downloading it only when CACHE_DIR holds no copy
# with the pinned checksum. A warm cache makes no request. CI uses it for GitHub release files,
# whose downloads fail the job when GitHub answers 5xx (run 42467109764: Fleet Web and Fleet
# Renderer; run 42564653741: Install Firecracker).
#
# CACHE_DIR defaults to $CACHED_DOWNLOAD_DIR, else /mnt/fcvm-btrfs/cache/downloads, which
# survives across runs on the self-hosted runners. Hosted jobs pass a directory they keep with
# actions/cache. The copy is published by a unique temporary name and a rename, so concurrent
# runs sharing CACHE_DIR never see a partial file, and a file whose checksum does not match is
# never published.
#
# It writes nothing to stdout: its callers run inside commands whose stdout is captured (make
# apk prints the built APK's path, which scripts/phone/app.sh reads), and a status line there
# became part of that path when the cache was cold (generation ten's host image build, 2026-11-12). Status goes to
# stderr.
set -euo pipefail
[ $# -ge 3 ] || { echo "usage: cached-download.sh URL SHA256 OUT [CACHE_DIR]" >&2; exit 2; }
url=$1 sha=$2 out=$3
dir=${4:-${CACHED_DOWNLOAD_DIR:-/mnt/fcvm-btrfs/cache/downloads}}
[[ $sha =~ ^[0-9a-f]{64}$ ]] || { echo "cached-download: not a sha256: $sha" >&2; exit 2; }
name=$(basename "${url%%\?*}")
cached="$dir/$sha-$name"
if [ -f "$cached" ] && echo "$sha  $cached" | sha256sum -c --status -; then
  echo "cached-download: $name from $cached, no download" >&2
else
  echo "cached-download: downloading $url" >&2
  mkdir -p "$dir"
  tmp=$(mktemp "$dir/.$name.XXXXXX")
  trap 'rm -f "$tmp"' EXIT
  curl --fail --silent --show-error --location --retry 10 --retry-all-errors --connect-timeout 20 \
    -o "$tmp" "$url"
  if ! echo "$sha  $tmp" | sha256sum -c --status -; then
    echo "cached-download: $url does not have sha256 $sha (got $(sha256sum "$tmp" | cut -d' ' -f1))" >&2
    exit 1
  fi
  chmod 644 "$tmp"
  mv "$tmp" "$cached"
  trap - EXIT
fi
otmp=$(mktemp "$out.XXXXXX")
cp "$cached" "$otmp" && chmod 644 "$otmp" && mv "$otmp" "$out"
