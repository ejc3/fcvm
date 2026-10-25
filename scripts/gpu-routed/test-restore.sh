#!/bin/bash
# run.sh's second VM must have been restored from a snapshot, not booted cold. A stub fcvm
# whose VMs come up healthy, hold their addresses and answer ping passes run.sh only when
# its `podman run` prints fcvm's {"status":"restored"} record.
#   scripts/gpu-routed/test-restore.sh
# run.sh insists on uid 0, so it runs as root in a user namespace (unshare -r).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
cat > "$dir/fcvm" <<EOF
#!/bin/bash
vms="$dir/vms"
case "\$1" in
  podman)
    name="" ip=""
    while [ \$# -gt 0 ]; do
      case "\$1" in --name) name=\$2 ;; --ipv6-address) ip=\$2 ;; esac
      shift
    done
    echo "\$name \$ip" >> "\$vms"
    [ -z "\${STUB_RESTORED:-}" ] || jq -cn --arg n "\$name" '{status: "restored", name: \$n}'
    exec sleep 600 ;;
  ls) [ -e "\$vms" ] || { echo '[]'; exit 0; }
    jq -Rn '[inputs | split(" ") | {name: .[0], pid: 1, health_status: "healthy"}]' "\$vms" ;;
  exec)
    case " \$* " in
      *" ping "*) exit 0 ;;
      *) while read -r _ ip; do echo "2: eth0    inet6 \$ip/128 scope global"; done < "\$vms" ;;
    esac ;;
esac
EOF
chmod +x "$dir/fcvm"
run() { # STUB_RESTORED value
  rm -f "$dir/vms"
  STUB_RESTORED=$1 FCVM="$dir/fcvm" FCVM_IPV6_PREFIX=2001:db8::/64 \
    unshare -r "$here/run.sh" 0000:00:00.0 > "$dir/out" 2>&1
}
if ! run 1 || ! grep -q '^gpu-routed: PASS' "$dir/out"; then
  echo "FAIL: run.sh did not pass VMs that report a restore:"; cat "$dir/out"; exit 1
fi
if run "" || ! grep -q '^FAIL gpu-routed-b-[0-9]*: did not report a snapshot restore' "$dir/out"; then
  echo "FAIL: run.sh did not refuse a second VM that reports no restore:"; cat "$dir/out"; exit 1
fi
echo "ok: run.sh passes a restored second VM and refuses one that booted cold"
