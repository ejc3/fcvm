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
# actions/cache. A download is published by a hard link that does not replace an entry, so
# concurrent runs sharing CACHE_DIR never see a partial file, an entry that is already valid is
# never touched, and a file whose checksum does not match is never published. OUT is copied from a
# hard link only this run holds, so a concurrent run cannot change what this one reads.
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
valid() { echo "$sha  $1" | sha256sum -c --status -; }
# OUT is always copied from a name only this run holds: a hard link to the cache entry, or the
# download itself. A concurrent run that replaces the entry changes the directory entry, never
# the inode this run reads, so cp never sees its source replaced under it.
mkdir -p "$dir"
mine=$(mktemp "$dir/.$name.XXXXXX")
trap 'rm -f "$mine" "$mine.pub"' EXIT
rm -f "$mine"
if ln "$cached" "$mine" 2>/dev/null && valid "$mine"; then
  echo "cached-download: $name from $cached, no download" >&2
else
  rm -f "$mine"
  echo "cached-download: downloading $url" >&2
  curl --fail --silent --show-error --location --retry 10 --retry-all-errors --connect-timeout 20 \
    -o "$mine" "$url"
  if ! valid "$mine"; then
    echo "cached-download: $url does not have sha256 $sha (got $(sha256sum "$mine" | cut -d' ' -f1))" >&2
    exit 1
  fi
  chmod 644 "$mine"
  # Publish without replacing: the entry is content-addressed, so one that is already there and
  # valid is kept. Only an entry that fails its checksum is replaced, atomically by rename.
  ln "$mine" "$cached" 2>/dev/null || valid "$cached" || { ln "$mine" "$mine.pub" && mv -f "$mine.pub" "$cached"; }
fi
otmp=$(mktemp "$out.XXXXXX")
cp "$mine" "$otmp" && chmod 644 "$otmp" && mv "$otmp" "$out"
