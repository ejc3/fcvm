#!/bin/bash
# run.sh must not leave its background VM running when it exits early. A stub fcvm whose
# `podman run` sleeps (standing in for a VM holding the GPU) and whose `ls` fails makes
# run.sh exit while polling; afterwards no process of the stub may be left.
#   scripts/gpu-routed/test-cleanup.sh
# run.sh insists on uid 0, so it runs as root in a user namespace (unshare -r).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
tag="gpu-routed-cleanup-$$-$RANDOM"
cat > "$dir/fcvm" <<EOF
#!/bin/bash
case "\$1" in
  podman) exec -a "$tag" sleep 600 ;;
  ls) exit 1 ;;
  *) exit 0 ;;
esac
EOF
chmod +x "$dir/fcvm"
rc=0
FCVM="$dir/fcvm" FCVM_IPV6_PREFIX=2001:db8::/64 unshare -r "$here/run.sh" 0000:00:00.0 \
  > "$dir/out" 2>&1 || rc=$?
sleep 0.5
if pgrep -f "^$tag" > /dev/null; then
  pkill -f "^$tag" || true
  echo "FAIL: run.sh exited ($rc) and left its VM running"; cat "$dir/out"; exit 1
fi
echo "ok: run.sh exited ($rc) with no VM left running"
