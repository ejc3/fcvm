#!/bin/bash
# Upload the pinned libseccomp release tarball as a release asset of ejc3/fcvm,
# a second source for the [libseccomp] pin in rootfs-config.toml. Run it once
# after moving that pin, then list the asset's URL first in `archives` there; it
# does nothing when the release already holds the tarball.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(awk -F'"' '/^\[libseccomp\]/{p=1} p && /^version = /{print $2; exit}' rootfs-config.toml)
sha256=$(awk -F'"' '/^\[libseccomp\]/{p=1} p && /^archive_sha256 = /{print $2; exit}' rootfs-config.toml)
[[ "$version" =~ ^[0-9]+(\.[0-9]+)+$ && "$sha256" =~ ^[0-9a-f]{64}$ ]] ||
    { echo "cannot read the [libseccomp] version and archive_sha256 from rootfs-config.toml" >&2; exit 1; }
tag="libseccomp-${version}"
file="libseccomp-${version}.tar.gz"
if gh release view "$tag" -R ejc3/fcvm --json assets --jq '.assets[].name' 2>/dev/null | grep -qx "$file"; then
    echo "==> $tag already holds $file"
    exit 0
fi
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 -o "$dir/$file" \
    "https://github.com/seccomp/libseccomp/releases/download/v${version}/$file"
echo "$sha256  $dir/$file" | sha256sum -c -
if gh release view "$tag" -R ejc3/fcvm >/dev/null 2>&1; then
    gh release upload "$tag" "$dir/$file" -R ejc3/fcvm
else
    gh release create "$tag" "$dir/$file" -R ejc3/fcvm --title "libseccomp source ${version}" \
        --notes "Release tarball of libseccomp $version from github.com/seccomp/libseccomp, sha256 $sha256. fcvm setup verifies that checksum." --latest=false
fi
echo "==> uploaded $file to $tag"
