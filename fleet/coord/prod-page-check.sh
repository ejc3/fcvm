#!/usr/bin/env bash
# The prod page as a viewer's browser loads it: GET <origin>/ over one address family must answer 200
# with the wall page's title (fleet/web). Used by fleet-prod-smoke.yml once the page is live.
#
# Usage: prod-page-check.sh -4|-6 https://<prod origin>
set -euo pipefail
family="${1:?usage: prod-page-check.sh -4|-6 <origin>}"
origin="${2:?usage: prod-page-check.sh -4|-6 <origin>}"
case "$family" in -4|-6) ;; *) echo "prod-page-check: the family is -4 or -6" >&2; exit 2 ;; esac
case "$origin" in https://*) ;; *) echo "prod-page-check: the origin must be https" >&2; exit 2 ;; esac
want='<title>Every GPU, live</title>'
body=$(mktemp)
trap 'rm -f "$body"' EXIT
code=$(curl "$family" -sS --max-time 30 -o "$body" -w '%{http_code}' "${origin%/}/") || {
  echo "prod-page-check: the page did not answer over ${family#-}" >&2; exit 1; }
if [ "$code" != 200 ] || ! grep -qF "$want" "$body"; then
  echo "prod-page-check: the page answered $code over ${family#-} without the wall's title" >&2
  exit 1
fi
echo "prod page: 200 with the wall's title over IPv${family#-}"
