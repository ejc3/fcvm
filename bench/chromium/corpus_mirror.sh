#!/bin/bash
# Mirror the Kitesurf public corpus (kitesurf.cloudflare.app/corpus.txt) into
# self-contained local fixtures under corpus/, one directory per site, links
# rewritten so each page renders entirely from our fixture server with no
# external network. A manifest records source URL, fetch date, and content
# hashes so the freeze is reproducible evidence, not a mystery blob.
#
# Fidelity bar ("equivalent level"): the page + its render-blocking and
# visible subresources (CSS, JS, images, fonts) — captured by wget
# --page-requisites with cross-host spanning. Third-party analytics/ads that
# no-op offline are acceptable losses; corpus_check.py measures whether the
# local render is actually equivalent (requests + DOM + pixels) and is the
# authority on which sites need deeper capture.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CORPUS_DIR="${CORPUS_DIR:-$HERE/corpus}"
UA="Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"

URLS=(
  "https://example.com/"
  "https://news.ycombinator.com/"
  "https://developers.cloudflare.com/"
  "https://blog.cloudflare.com/"
  "https://en.wikipedia.org/"
  "https://developer.mozilla.org/en-US/"
  "https://www.elmundo.es/"
  "https://www.rtp.pt/noticias/"
  "https://www.theguardian.com/international"
  "https://todomvc.com/examples/javascript-es6/dist/"
  "https://todomvc.com/examples/react/dist/index.html"
  "https://todomvc.com/examples/vue/dist/"
  "https://todomvc.com/examples/angular/dist/browser/"
  "https://todomvc.com/examples/preact/dist/"
)

# Stable site key from a URL: host + first path segment-ish, filesystem-safe.
site_key() {
  python3 - "$1" <<'PY'
import sys, urllib.parse
u = urllib.parse.urlparse(sys.argv[1])
path = u.path.strip("/").replace("/", "-") or "root"
print(f"{u.netloc}_{path}"[:80])
PY
}

mkdir -p "$CORPUS_DIR"
manifest="$CORPUS_DIR/MANIFEST.jsonl"
: > "$manifest"

for url in "${URLS[@]}"; do
  key=$(site_key "$url")
  dest="$CORPUS_DIR/$key"
  echo "── $key  ($url)"
  rm -rf "$dest"
  mkdir -p "$dest"
  # -p page requisites; -k convert links to relative; -H span hosts for CDN
  # assets; -E adjust extensions so served MIME types resolve; quota bounds a
  # runaway site; timeouts bound a hung one. wget exit 8 (server errors on
  # some requisite) is tolerated — the equivalence CHECK decides acceptance.
  wget --directory-prefix="$dest" --page-requisites --convert-links \
       --span-hosts --adjust-extension --no-parent \
       --timeout=20 --tries=2 --quota=60m --user-agent="$UA" \
       --restrict-file-names=windows --no-verbose \
       "$url" 2>"$dest/wget.log" || rc=$?
  rc=${rc:-0}
  entry=$(python3 - "$url" "$key" "$dest" "$rc" <<'PY'
import hashlib, json, os, sys, time
url, key, dest, rc = sys.argv[1:5]
files, total = 0, 0
h = hashlib.sha256()
for root, _, names in os.walk(dest):
    for n in sorted(names):
        if n == "wget.log": continue
        p = os.path.join(root, n)
        files += 1
        total += os.path.getsize(p)
        h.update(open(p, "rb").read())
print(json.dumps({"url": url, "key": key, "wget_rc": int(rc),
                  "files": files, "bytes": total,
                  "tree_sha256": h.hexdigest(),
                  "fetched_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}))
PY
)
  echo "$entry" >> "$manifest"
  echo "   $entry"
  unset rc
done
echo "corpus mirrored into $CORPUS_DIR; manifest: $manifest"
