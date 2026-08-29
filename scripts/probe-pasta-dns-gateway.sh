#!/usr/bin/env bash
# Does a guest's DNS to the pasta gateway reach a service on host 127.0.0.1:53?
#
# fcvm's `--dns 10.0.2.2` says "resolve at the host loopback service behind the
# gateway". pasta does not always do that: conf.c's add_dns_resolv4() sets
# dns_match to the gateway whenever the HOST's first /etc/resolv.conf nameserver
# is a loopback address, and fwd_nat_from_tap() applies that redirect before the
# gateway-to-loopback translation, so the query lands on the host's own resolver
# instead. fcvm passes `-D none` for such a guest to switch the redirect off
# (src/network/pasta.rs, resolver_is_pasta_gateway).
#
# The unit tests can only check that the flag is in the argument vector. This
# probe runs the pinned pasta binary and asks which server actually answered,
# and it asserts BOTH directions: with the flag the replay answers, without it
# the host's resolver does. A probe that only checked the fixed case would pass
# on a pasta that never redirected anything.
#
# No VM, no privileged port on the real host: everything runs inside a private
# user + net + mount namespace, so the :53 listeners and the resolv.conf this
# reads are that namespace's, and the host's DNS configuration is untouched.
#
#   scripts/probe-pasta-dns-gateway.sh
#
# Exit 0 both expectations held, 1 an expectation failed, 2 could not run.
set -euo pipefail

REPLAY_ANSWER=10.0.2.2       # what the "corpus_serve" stand-in answers
RESOLVER_ANSWER=203.0.113.99 # what the "host resolver" stand-in answers
GUEST_GATEWAY=10.0.2.2
GUEST_IP=10.0.2.100

if [ "${1:-}" != "--inside" ]; then
    for tool in unshare nsenter dig ip python3; do
        command -v "$tool" >/dev/null 2>&1 \
            || { echo "BLOCKED: '$tool' missing; this probe cannot evaluate anything" >&2; exit 2; }
    done
    PASTA_BIN="${PASTA_BIN:-$(ls -t /mnt/fcvm-btrfs/pasta/pasta-*.bin 2>/dev/null | head -1 || true)}"
    [ -x "${PASTA_BIN:-}" ] \
        || { echo "BLOCKED: no pasta binary; set PASTA_BIN or run 'make setup-fcvm'" >&2; exit 2; }
    unshare --user --map-root-user --net --mount --fork -- \
        "$0" --inside "$PASTA_BIN" "$(mktemp -d)" \
        || exit $?
    exit 0
fi

PASTA_BIN="$2"
WORK="$3"
trap 'rm -rf "$WORK"' EXIT

# A wildcard A responder, the shape corpus_serve.py's DNS side has.
cat >"$WORK/responder.py" <<'PY'
import socket, struct, sys
bind_addr, answer_ip, label = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind((bind_addr, 53))
print(f"{label} listening on {bind_addr}:53 answering {answer_ip}", flush=True)
while True:
    data, peer = s.recvfrom(512)
    if len(data) < 12:
        continue
    i = 12
    while i < len(data) and data[i]:
        i += 1 + data[i]
    print(f"{label} query from {peer[0]}:{peer[1]}", flush=True)
    header = data[:2] + struct.pack("!HHHHH", 0x8180, 1, 1, 0, 0)
    answer = b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + socket.inet_aton(answer_ip)
    s.sendto(header + data[12:i + 5] + answer, peer)
PY

ip link set lo up
# pasta needs an interface with a default route as its template. Both ends of
# this veth pair stay inside the namespace; nothing leaves it.
ip link add veth0 type veth peer name veth1
ip addr add 192.0.2.1/24 dev veth0
ip addr add 192.0.2.2/24 dev veth1
ip link set veth0 up
ip link set veth1 up
ip route add default via 192.0.2.2 dev veth0

python3 "$WORK/responder.py" 127.0.0.53 "$RESOLVER_ANSWER" host-resolver >"$WORK/resolver.log" 2>&1 &
python3 "$WORK/responder.py" 127.0.0.1 "$REPLAY_ANSWER" replay >"$WORK/replay.log" 2>&1 &
for _ in $(seq 1 50); do
    grep -q listening "$WORK/resolver.log" && grep -q listening "$WORK/replay.log" && break
    sleep 0.1
done
grep -q listening "$WORK/resolver.log" && grep -q listening "$WORK/replay.log" \
    || { echo "BLOCKED: the responders did not bind" >&2; cat "$WORK"/*.log >&2; exit 2; }

# The systemd-resolved shape: the host's first nameserver is a loopback address
# that is NOT where the replay is. Bound in this mount namespace only.
printf 'nameserver 127.0.0.53\n' >"$WORK/resolv.conf"
mount --bind "$WORK/resolv.conf" /etc/resolv.conf

ask() {
    # $@ = extra pasta flags. Prints what the guest's resolver answered.
    local nspid pastapid answer
    unshare --user --map-root-user --net -- sleep 60 &
    nspid=$!
    for _ in $(seq 1 50); do [ -e "/proc/$nspid/ns/net" ] && break; sleep 0.1; done
    "$PASTA_BIN" --foreground --quiet --runas 0:0 --ns-ifname pasta0 \
        -a "$GUEST_IP" -n 255.255.255.0 -g "$GUEST_GATEWAY" --no-dhcp \
        --ipv4-only --no-ndp --no-dhcpv6 --no-ra \
        -t none -u none -T none -U none --config-net "$@" "$nspid" \
        >"$WORK/pasta.log" 2>&1 &
    pastapid=$!
    for _ in $(seq 1 50); do
        nsenter -t "$nspid" -U -n --preserve-credentials -- ip -o addr show pasta0 2>/dev/null \
            | grep -q "$GUEST_IP" && break
        sleep 0.1
    done
    answer=$(nsenter -t "$nspid" -U -n --preserve-credentials -- \
        dig +short +time=2 +tries=1 +noedns "@$GUEST_GATEWAY" example.com 2>/dev/null | head -1 || true)
    kill "$pastapid" "$nspid" 2>/dev/null || true
    wait "$pastapid" "$nspid" 2>/dev/null || true
    printf '%s' "$answer"
}

rc=0
with=$(ask -D none)
if [ "$with" = "$REPLAY_ANSWER" ]; then
    echo "OK   with -D none:    $with (the replay on host 127.0.0.1:53 answered)"
else
    echo "FAIL with -D none:    '$with', want $REPLAY_ANSWER; the guest's query did not reach host 127.0.0.1:53" >&2
    cat "$WORK/pasta.log" >&2
    rc=1
fi
without=$(ask)
if [ "$without" = "$RESOLVER_ANSWER" ]; then
    echo "OK   without it:      $without (pasta redirected port 53 to the host's own resolver)"
else
    echo "FAIL without it:      '$without', want $RESOLVER_ANSWER; this probe cannot tell the two wirings apart" >&2
    cat "$WORK/pasta.log" >&2
    rc=1
fi
exit "$rc"
