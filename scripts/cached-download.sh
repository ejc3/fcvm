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
valid() { echo "$sha  $1" 2>/dev/null | sha256sum -c --status - 2>/dev/null; }
mkdir -p "$dir"
# Private names ($dir/.NAME.XXXXXX and .pub) belong to one run and a SIGKILL leaves them behind.
# Sweep those an hour old: a copy or download by mtime (its link count is 1), a hard link to an
# entry by ctime (linking bumps the shared inode's ctime, so a live link is never old).
find "$dir" -maxdepth 1 -type f -name '.*' -links 1 -mmin +60 -delete 2>/dev/null || true
find "$dir" -maxdepth 1 -type f -name '.*' -links +1 -cmin +60 -delete 2>/dev/null || true
# OUT is always copied from a name only this run holds: a hard link to the cache entry, a copy of
# it made through one open descriptor (when it cannot be linked: fs.protected_hardlinks on an
# entry another user cached, or a filesystem without hard links), or the download itself. A
# concurrent run that replaces the entry changes the directory entry, never the inode this run
# reads, so nothing is torn.
mine=$(mktemp "$dir/.$name.XXXXXX")
otmp=
trap 'rm -f "$mine" "$mine.pub" "$otmp"' EXIT
rm -f "$mine"
hit=0
if [ -f "$cached" ]; then
  { ln "$cached" "$mine" 2>/dev/null || cat < "$cached" > "$mine" 2>/dev/null; } && valid "$mine" && hit=1
fi
if [ $hit = 1 ]; then
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
  # valid is kept, even when it cannot be linked. Only a missing or corrupt entry is replaced,
  # by the rename of a copy of the private file.
  if ! ln "$mine" "$cached" 2>/dev/null && ! valid "$cached"; then
    cp "$mine" "$mine.pub" && mv -f "$mine.pub" "$cached"
  fi
fi
otmp=$(mktemp "$out.XXXXXX")
cp "$mine" "$otmp" && chmod 644 "$otmp" && mv "$otmp" "$out"
