#!/bin/bash
# Upload the pinned passt source archive as a release asset of ejc3/fcvm, the
# first source scripts/build-passt.sh and `fcvm setup` try (#1008). Run it once
# after moving the [pasta] pin in rootfs-config.toml; it does nothing when the
# release already holds the archive.
set -euo pipefail
cd "$(dirname "$0")/.."
commit=$(awk -F'"' '/^\[pasta\]/{p=1} p && /^commit = /{print $2; exit}' rootfs-config.toml)
sha256=$(awk -F'"' '/^\[pasta\]/{p=1} p && /^archive_sha256 = /{print $2; exit}' rootfs-config.toml)
[[ "$commit" =~ ^[0-9a-f]{40}$ && "$sha256" =~ ^[0-9a-f]{64}$ ]] ||
    { echo "cannot read the [pasta] commit and archive_sha256 from rootfs-config.toml" >&2; exit 1; }
tag="passt-${commit:0:12}"
file="passt-${commit}.tar.xz"
if gh release view "$tag" -R ejc3/fcvm --json assets --jq '.assets[].name' 2>/dev/null | grep -qx "$file"; then
    echo "==> $tag already holds $file"
    exit 0
fi
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 -o "$dir/$file" "https://passt.top/passt/snapshot/$file"
echo "$sha256  $dir/$file" | sha256sum -c -
if gh release view "$tag" -R ejc3/fcvm >/dev/null 2>&1; then
    gh release upload "$tag" "$dir/$file" -R ejc3/fcvm
else
    gh release create "$tag" "$dir/$file" -R ejc3/fcvm --title "passt source ${commit:0:12}" \
        --notes "Snapshot archive of passt commit $commit from passt.top, sha256 $sha256. scripts/build-passt.sh and fcvm setup verify that checksum." --latest=false
fi
echo "==> uploaded $file to $tag"
