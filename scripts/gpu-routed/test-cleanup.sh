#!/bin/bash
# run.sh must not leave its background VM running when it exits early. A stub fcvm whose
# `podman run` sleeps (standing in for a VM holding the GPU) and whose `ls` fails makes
# run.sh exit while polling; afterwards no process of the stub may be left.
#   scripts/gpu-routed/test-cleanup.sh
# run.sh insists on uid 0, so it runs as root in a user namespace (unshare -r). The stub
# leaves a marker when the VM starts and when run.sh polls it, and the test fails unless
# both are there: a run.sh that never ran (unshare refused) must not read as clean.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
tag="gpu-routed-cleanup-$$-$RANDOM"
# `ls` waits for the VM's marker, so run.sh exits only once the VM has started.
cat > "$dir/fcvm" <<EOF
#!/bin/bash
case "\$1" in
  podman) touch "$dir/started"; exec -a "$tag" sleep 600 ;;
  ls)
    for _ in \$(seq 1 100); do [ -e "$dir/started" ] && break; sleep 0.05; done
    touch "$dir/polled"; exit 1 ;;
  *) exit 0 ;;
esac
EOF
chmod +x "$dir/fcvm"
rc=0
FCVM="$dir/fcvm" FCVM_IPV6_PREFIX=2001:db8::/64 unshare -r "$here/run.sh" 0000:00:00.0 \
  > "$dir/out" 2>&1 || rc=$?
if pgrep -f "^$tag" > /dev/null; then
  pkill -f "^$tag" || true
  echo "FAIL: run.sh exited ($rc) and left its VM running"; cat "$dir/out"; exit 1
fi
for marker in started polled; do
  if [ ! -e "$dir/$marker" ]; then
    echo "FAIL: the scenario did not run (no '$marker' marker; run.sh exited $rc):"
    cat "$dir/out"; exit 1
  fi
done
if [ "$rc" = 0 ]; then
  echo "FAIL: run.sh exited 0 although polling failed:"; cat "$dir/out"; exit 1
fi
echo "ok: run.sh exited ($rc) while polling a running VM, and no VM was left running"
