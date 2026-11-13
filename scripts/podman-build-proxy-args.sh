#!/bin/bash
# Print the `podman build` arguments for this shell's proxy variables.
#
# podman build hands its build containers the caller's http_proxy/https_proxy (and the upper-case
# forms). A proxy on the host's loopback (127.0.0.0/8, localhost, ::1) is not reachable from a
# build container on its own network, whose loopback is its own, so every fetch in the build
# failed ("Failed to connect to 127.0.0.1"). Then the build runs on the host's network
# (--network=host): 127.0.0.1 is the host's, the proxy is reached, and the proxy variables pass
# through unchanged. Any other proxy, and no proxy, leave podman's defaults. Nothing here touches
# TLS verification.
set -euo pipefail
for v in "${https_proxy:-}" "${HTTPS_PROXY:-}" "${http_proxy:-}" "${HTTP_PROXY:-}"; do
  [ -n "$v" ] || continue
  host=${v#*://}
  host=${host##*@}
  host=${host%%/*}
  case "$host" in
    \[*\]*) host=${host%%]*}; host=${host#[} ;;
    *) host=${host%%:*} ;;
  esac
  case "$host" in
    127.*|localhost|::1)
      echo "--network=host"
      exit 0
      ;;
  esac
done
