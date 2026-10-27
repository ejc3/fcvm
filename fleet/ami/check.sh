#!/bin/bash
# Offline checks of the fleet image recipes. CI does not run them (a path under fleet/
# outside fleet/core runs only the renderer's host tests); run before changing a recipe:
#   bash fleet/ami/check.sh
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 2
fail=0
no() { echo "FAIL: $*"; fail=1; }
scripts=(fleet/ami/*.sh fleet/ami/host/*.sh fleet/ami/tile/*.sh fleet/tools/gpubox.sh fleet/tools/issue-cert.sh)
for f in "${scripts[@]}"; do bash -n "$f" || no "bash -n $f"; done
if command -v shellcheck >/dev/null; then
  shellcheck -S warning "${scripts[@]}" || no "shellcheck -S warning"
else
  no "shellcheck is not installed"
fi
python3 -m py_compile fleet/ami/host/s3-cat.py fleet/ami/host/store-pack.py fleet/ami/tile/fetch-cert.py || no "py_compile"

# `build-ami.sh start` must clear the phase's markers before it launches the phase:
# `wait` reads <phase>.done / <phase>.failed, and a builder from a BASE_AMI (or a
# repeated phase) still holds the previous run's, which `wait` would report at once.
start=$(sed -n '/^start)/,/;;/p' fleet/ami/build-ami.sh)
# A command of its own (`;`), so it has run when the SSM command returns; joined with `&&`
# it would be part of the backgrounded list and race the next `wait`.
clear_at=$(grep -bo 'rm -f /var/lib/fleet-ami/\$4.done /var/lib/fleet-ami/\$4.failed;' <<<"$start" | head -1 | cut -d: -f1)
launch_at=$(grep -bo 'nohup setsid' <<<"$start" | head -1 | cut -d: -f1)
[ -n "$clear_at" ] && [ -n "$launch_at" ] && [ "$clear_at" -lt "$launch_at" ] ||
  no "build-ami.sh start does not clear <phase>.done and <phase>.failed before launching the phase"

# The tile's image phase must fail when the renderer never serves a ready frame: the
# health endpoint answers 200 whatever its state, so the check is jq -e on its body.
ready='.ready == true and .frame > 0'
jq -e "$ready" <<<'{"ready":false,"frame":0,"gpu":{}}' >/dev/null && no "readiness predicate accepts a renderer that is not ready"
jq -e "$ready" <<<'{"ready":true,"frame":12,"gpu":{}}' >/dev/null || no "readiness predicate refuses a ready renderer"
image=$(sed -n '/^image)/,/;;/p' fleet/ami/tile/build.sh)
grep -qF "| jq -e '$ready' >/dev/null || { echo \"renderer not ready after 300 s\"" <<<"$image" ||
  no "tile image phase does not fail when the renderer is not ready after the wait"

# gpubox passes its lease script to `aws ec2 run-instances --user-data` as the script
# itself: the CLI base64-encodes user data, so a pre-encoded script reaches cloud-init as
# base64 text and the lease timer is never installed (seen 2026-10-24: describing a
# test instance's userData and decoding it once gave base64 text back).
grep -qE 'ud=\$\(lease_userdata "\$lease"\)$' fleet/tools/gpubox.sh ||
  no "gpubox.sh must pass the lease script to --user-data unencoded"

# measure-boot.sh must not report a timing for a command that failed on the instance: its
# ssm() fails unless the invocation ended Success. Run its ssm() against a stub `A` whose
# invocation ended Failed.
ssm_def=$(sed -n '/^ssm() {/,/^}/p' fleet/ami/measure-boot.sh)
if [ -z "$ssm_def" ]; then
  no "measure-boot.sh has no ssm() to test"
elif (
  A() { case "$*" in *send-command*) echo cmd-1 ;; *"--query Status"*) echo Failed ;; *) echo "warm: ok=0" ;; esac; }
  export id=i-1234567890abcdef0  # read by the eval'd ssm()
  eval "$ssm_def"
  ssm "true" >/dev/null 2>&1
); then
  no "measure-boot.sh ssm() returns success for an invocation that ended Failed"
fi
# The host warm-up command runs on the instance under `sh` (AWS-RunShellScript; dash on
# Ubuntu, which has no pipefail) and must end with warm.sh's status. Run the command as the
# instance receives it, under dash, with a stub prlimit standing in for the warm-up.
raw=$(sed -n 's/^  ssm "\(.*G0=.*\)" ||$/\1/p' fleet/ami/measure-boot.sh)
stub=$(mktemp -d)
printf '#!/bin/sh\necho "warm: ok=$WARM_OK"\n[ "$WARM_OK" = 1 ]\n' > "$stub/prlimit"; chmod +x "$stub/prlimit"
if [ -z "$raw" ] || ! command -v dash >/dev/null; then
  no "measure-boot.sh host warm-up command not found, or no dash to run it"
else
  remote=""; eval "remote=\"$raw\""
  # fcvm logs its snapshot-cache line at info and only warnings by default.
  grep -qF 'RUST_LOG=fcvm=info bash -c' <<<"$remote" || no "measure-boot warm-up does not log fcvm's info lines"
  # Its files go to the scratch directory, not the instance's /tmp.
  remote=${remote//\/tmp\//$stub/}
  WARM_OK=1 PATH="$stub:$PATH" dash -c "$remote" >/dev/null 2>&1 || no "measure-boot warm-up command fails under dash for a good warm-up"
  WARM_OK=0 PATH="$stub:$PATH" dash -c "$remote" >/dev/null 2>&1 && no "measure-boot warm-up command succeeds under dash for a failed warm-up"
  # It prints fcvm's restored record whole, and hit or miss from fcvm's last
  # snapshot-cache line (a hit whose load fails logs a miss after it and boots cold).
  record='{"status":"restored","name":"warm","snapshot":"0123456789ab","vm_id":"vm-0","backend":"uffd","hypervisor":"firecracker","ack_ms":4157,"guest_phases":{"gpu_ms":3337.4,"total_ms":4100}}'
  hit='2026-10-24T00:00:00.000000Z  INFO fcvm: Pre-start snapshot hit! Restoring from cached snapshot snapshot_key=0123456789ab image=localhost/fcvm-renderer:fleet'
  miss='2026-10-24T00:00:01.000000Z  INFO fcvm: Snapshot miss, will create snapshot after image load snapshot_key=0123456789ab image=localhost/fcvm-renderer:fleet'
  report() { printf '%s\n' "$@" > "$stub/warm.log"; WARM_OK=1 PATH="$stub:$PATH" dash -c "$remote" 2>/dev/null; }
  out=$(report "$hit" '{"status":"starting"}' 'not json {' "$record")
  grep -qxF "$record" <<<"$out" || no "measure-boot host does not print the whole restored record"
  grep -qx 'snapshot: hit' <<<"$out" || no "measure-boot host does not report a snapshot hit"
  out=$(report "$hit" "$miss" "$record")
  grep -qx 'snapshot: miss' <<<"$out" || no "measure-boot host reports a hit whose restore fell back to a cold boot"
  out=$(report "$miss" "$record")
  grep -qx 'snapshot: miss' <<<"$out" || no "measure-boot host does not report a snapshot miss"
fi
rm -rf "$stub"
grep -qF '{ echo "host: the first GPU VM failed"; exit 1; }' fleet/ami/measure-boot.sh ||
  no "measure-boot.sh host does not stop when the warm-up fails"

# host/build.sh must check out $FCVM_COMMIT even when the builder's source tree (from a
# BASE_AMI) holds untracked files that commit tracks: on 2026-10-24 an untracked
# fleet/ami/ stopped the build with "untracked working tree files would be overwritten".
# Run build.sh's own checkout command in a scratch clone in that state.
checkout=$(sed -n 's/^  as_user "\(cd \$SRC && git fetch .*git checkout.*\)"$/\1/p' fleet/ami/host/build.sh)
gt=$(mktemp -d)
if [ -z "$checkout" ]; then
  no "host/build.sh has no source checkout command to test"
else
  (
    set -e
    g() { git -c user.email=check@example.invalid -c user.name=check -c init.defaultBranch=main "$@"; }
    g init -q "$gt/origin"; cd "$gt/origin"
    echo a > a; g add a; g commit -qm a; old=$(git rev-parse HEAD)
    mkdir -p fleet/ami; echo tracked > fleet/ami/warm.sh; g add fleet; g commit -qm b
    new=$(git rev-parse HEAD)
    g clone -q "$gt/origin" "$gt/src"; cd "$gt/src"; git checkout -q --detach "$old"
    mkdir -p fleet/ami; echo stray > fleet/ami/warm.sh; echo stray > leftover
    export SRC="$gt/src" FCVM_COMMIT="$new"  # read by the eval'd command
    c=""; eval "c=\"$checkout\""
    bash -c "$c" 2>&1 >/dev/null | sed "s/^/  checkout: /" >&2
    [ "$(git -C "$gt/src" rev-parse HEAD)" = "$new" ] && [ "$(cat "$gt/src/fleet/ami/warm.sh")" = tracked ] && [ ! -e "$gt/src/leftover" ]
  ) || no "host/build.sh's checkout does not reach FCVM_COMMIT cleanly over untracked files"
fi
rm -rf "$gt"

# The store unit writes the file the agent reads as --store-ready-file (the host's agent
# arguments, fleet/PROTOCOL.md "Running it"), with the boot times the agent reports. Nothing
# wrote it before, so every hello carried no store time.
agent_file=$(grep '^FLEET_AGENT_ARGS=' fleet/PROTOCOL.md | grep -o -- '--store-ready-file [^ ]*' | head -1 | cut -d' ' -f2)
store_file=$(sed -n 's/^READY=\${STORE_READY_FILE:-\([^}]*\)}$/\1/p' fleet/ami/host/store.sh)
[ -n "$agent_file" ] && [ "$agent_file" = "$store_file" ] ||
  no "store.sh writes '${store_file}', the agent reads --store-ready-file '${agent_file}'"
grep -q 'mv -f "$READY.tmp" "$READY"' fleet/ami/host/store.sh || no "store.sh does not replace the ready file by rename"
st=$(mktemp -d)
sed -n '/^printf .{"store_started_at"/,/> "\$READY.tmp"/p' fleet/ami/host/store.sh |
  sed "s|\$READY|$st/ready|; s|\$((t0 / 1000000))|1|; s|\$((t1 / 1000000))|2|; s|\"\$src\"|s3|" > "$st/w.sh"
bash "$st/w.sh" 2>/dev/null && jq -e '.store_started_at == 1 and .store_mounted_at == 2' "$st/ready.tmp" >/dev/null 2>&1 ||
  no "store.sh's ready file is not a JSON object of the store's times"
rm -rf "$st"
# The tile's boot path (certificate, TLS, renderer, first-frame log) must not wait for
# cloud-init's network stage: cloud-init.service is ordered before sysinit.target and
# network-online.target, so a unit with default dependencies (after sysinit.target), or one
# ordered after network-online.target or any cloud-init unit, starts only once that stage is
# done (about 8 s into a tile's first boot, issue #1257). The distribution's nginx.service is
# ordered after network-online.target, so the tile must run fcvm-fleet-nginx.service instead.
late='network-online.target|sysinit.target|basic.target|multi-user.target|cloud-[a-z-]*\.(service|target)|nginx\.service'
for u in fcvm-fleet-tls fcvm-fleet-renderer fcvm-fleet-nginx fcvm-fleet-first-frame; do
  f=fleet/ami/tile/$u.service
  [ -f "$f" ] || { no "$f is missing"; continue; }
  unit=$(sed -n '/^\[Unit\]/,/^\[/p' "$f")
  grep -qx 'DefaultDependencies=no' <<<"$unit" ||
    no "$u.service has default dependencies, so it starts after sysinit.target and cloud-init's network stage"
  bad=$(grep -E '^(After|Requires|Wants|BindsTo|Requisite)=' <<<"$unit" | grep -oE "(^|[= ])($late)" | tr -d '= ' | sort -u | paste -sd' ')
  [ -z "$bad" ] || no "$u.service waits for $bad, which waits for cloud-init's network stage"
done
enable=$(grep -E '^ *systemctl enable ' fleet/ami/tile/build.sh)
grep -qw 'fcvm-fleet-nginx.service' <<<"$enable" || no "tile/build.sh does not enable fcvm-fleet-nginx.service"
grep -qE '(^| )nginx\.service' <<<"$enable" && no "tile/build.sh enables the distribution's nginx.service, which waits for network-online.target"

# store-pack.py packs the store into parts and store.sh restores them all at once, each
# part decompressed and extracted while the others are (one zstd stream decompresses on one
# core), then the directories, then the staging copy. Both ways (staged and direct) the
# restore must give the tree one tar of the store gives: names, types, modes, owners,
# mtimes (directories' too, and to the nanosecond: fcvm's cached image digests in
# version-cache/ are valid for the image's exact mtime, so a whole-second mtime makes a host
# hash its images again, #1219), hard links, holes, bytes. The fetch below holds every part
# until all of them have been asked for, so a restore that takes the parts one by one fails.
fx=$(mktemp -d)
# Each step ends in `|| exit 1`: `set -e` does nothing in a subshell that is part of `||`.
(
  mkdir -p "$fx/src/d/e" "$fx/src/vm-disks/x" "$fx/src/f" || exit 1
  printf 'one' > "$fx/src/d/a" && ln "$fx/src/d/a" "$fx/src/d/e/a-link" && ln -s ../a "$fx/src/d/e/sym" || exit 1
  ln -s d/e "$fx/src/dir-link" && printf 'x' > "$fx/src/vm-disks/x/disk" && printf 'l' > "$fx/src/f/k.lock" || exit 1
  for i in 1 2 3 4 5 6; do head -c $((i * 4096)) /dev/urandom > "$fx/src/f/file$i" || exit 1; done
  # Hard links of equal size: split across parts, a name's link would become a copy.
  for i in 1 2 3 4; do printf 'h%s' "$i" > "$fx/src/f/h$i" && ln "$fx/src/f/h$i" "$fx/src/f/h$i-link" || exit 1; done
  truncate -s 64M "$fx/src/d/sparse" || exit 1
  printf 'tail' | dd of="$fx/src/d/sparse" bs=1 seek=$((32 << 20)) conv=notrunc status=none || exit 1
  chmod 0750 "$fx/src/d/e" && touch -d '2026-01-02 03:04:05.123456789' "$fx/src/d/a" || exit 1
  # A directory that holds a file (so a part creates it) and an empty directory (which only
  # the directory pass creates): its mtime must survive the empty one being made in it.
  mkdir -p "$fx/src/g/empty" && printf 'g' > "$fx/src/g/file" || exit 1
  touch -d '2026-01-03 00:00:00' "$fx/src/d" "$fx/src/d/e" "$fx/src/f" "$fx/src/g/empty" "$fx/src/g" "$fx/src" || exit 1
  ex=(--exclude=./vm-disks --exclude='./*.lock')
  tar -c --sparse --numeric-owner --format=posix "${ex[@]}" -C "$fx/src" . > "$fx/one.tar" || exit 1
  mkdir "$fx/one" && tar -x --sparse --same-owner --numeric-owner -C "$fx/one" < "$fx/one.tar" || exit 1
  python3 fleet/ami/host/store-pack.py "$fx/src" "$fx/pack" 3 "${ex[@]}" > /dev/null || exit 1
  mapfile -t parts < "$fx/pack/parts"
  [ "${#parts[@]}" = 3 ] || exit 1
  fns=$(sed -n '/^# --- restore functions/,/^# --- end of restore functions/p' fleet/ami/host/store.sh)
  [ -n "$fns" ] || exit 1
  for mode in direct staged; do
    M="$fx/$mode"; mkdir "$M" "$fx/seen-$mode" || exit 1; stage=""
    if [ "$mode" = staged ]; then stage=$(mktemp -d) || exit 1; fi
    fetch() {
      if [ "$1" != dirs ]; then
        touch "$fx/seen-$mode/$1"
        for _ in $(seq 100); do
          [ "$(find "$fx/seen-$mode" -type f | wc -l)" -ge "${#parts[@]}" ] && break
          sleep 0.1
        done
        [ "$(find "$fx/seen-$mode" -type f | wc -l)" -ge "${#parts[@]}" ] || return 1
      fi
      cat "$fx/pack/$1.tar.zst"
    }
    eval "$fns"
    restore fetch "${parts[@]}" || exit 1
    if [ -n "$stage" ]; then rm -rf "$stage"; fi
  done
  tree() { (cd "$1" && find . -printf '%p %y %m %U %G %T@ %n %s %l\n' | sort && find . -type f -exec sha256sum {} + | sort -k2); }
  for mode in direct staged; do
    [ "$(stat -c %y "$fx/$mode/d/a")" = "$(stat -c %y "$fx/src/d/a")" ] || { touch "$fx/mtime-lost"; exit 1; }
    diff <(tree "$fx/one") <(tree "$fx/$mode") >&2 || exit 1
    [ "$(stat -c %h "$fx/$mode/d/a")" = 2 ] || exit 1
    # The hole stays a hole: far fewer blocks than the 64 MiB the file spans.
    [ "$(stat -c %b "$fx/$mode/d/sparse")" -lt 1024 ] || exit 1
  done
) 2>/dev/null || no "store.sh does not restore store-pack.py's parts at once into the tree one tar gives"
[ -e "$fx/mtime-lost" ] && no "the store parts drop the fraction of a modification time"
rm -rf "$fx"

# The store waits for the network and nothing else of the boot: with default dependencies
# it waited for sysinit.target and basic.target, so for cloud-init's network stage (2.1 s of
# a cold g4dn.metal boot), and multi-user.target, so cloud-final, waited for the store.
su=fleet/ami/host/fcvm-fleet-store.service
after=$(sed -n 's/^After=//p' "$su" | tr ' ' '\n')
grep -qx 'DefaultDependencies=no' "$su" &&
  grep -qx 'systemd-networkd-wait-online.service' <<<"$after" && grep -qx 'nss-lookup.target' <<<"$after" &&
  grep -qx 'local-fs.target' <<<"$after" && ! grep -qE '^(cloud-init|sysinit|basic|network-online)' <<<"$after" &&
  grep -qx 'Conflicts=shutdown.target' "$su" && grep -qx 'Before=shutdown.target' "$su" ||
  no "fcvm-fleet-store.service must order itself after the network only (DefaultDependencies=no)"
# The agent is enabled in the image, so a host whose boot wrote agent.env starts it as soon
# as the store is restored, without waiting for the user data's `systemctl start`.
sed -n '/^prep)/,/;;/p' fleet/ami/host/build.sh | grep -qE '^  systemctl enable .*fcvm-fleet-agent\.service' ||
  no "host/build.sh prep does not enable fcvm-fleet-agent.service"
# The prefetch must start before the network and the store (it competes with nothing that
# waits on it), and the image must enable it.
grep -qx 'DefaultDependencies=no' fleet/ami/host/fcvm-fleet-prefetch.service &&
  grep -qx 'After=local-fs.target' fleet/ami/host/fcvm-fleet-prefetch.service ||
  no "fcvm-fleet-prefetch.service must start once local file systems are up (DefaultDependencies=no)"
sed -n '/^prep)/,/;;/p' fleet/ami/host/build.sh | grep -qE '^  systemctl enable .*fcvm-fleet-prefetch\.service' ||
  no "host/build.sh prep does not enable fcvm-fleet-prefetch.service"
# warm-root.sh lists the prefetch from a launch that restores the snapshot (what a woken
# host's slots run), not from the first launch, which also boots and exports the image.
wr=fleet/ami/host/warm-root.sh
first=$(grep -n '^launch ||' "$wr" | head -1 | cut -d: -f1)
traced=$(grep -n '^launch trace ||' "$wr" | head -1 | cut -d: -f1)
[ -n "$first" ] && [ -n "$traced" ] && [ "$first" -lt "$traced" ] && grep -q "Pre-start snapshot hit" "$wr" ||
  no "warm-root.sh must list the prefetch from a second, traced launch that hits the first one's snapshot"
# prefetch.sh reads every listed file in 1 MiB pieces (a file of 2 MiB and one byte is three
# pieces) and skips a listed file the image lacks.
pf=$(mktemp -d)
head -c $((2 * 1048576 + 1)) /dev/zero > "$pf/a"
printf '%s\n%s\n' "$pf/a" "$pf/missing" > "$pf/list"
mkdir "$pf/bin"
printf '#!/bin/sh\necho "$@" >> %s/dd.log\n' "$pf" > "$pf/bin/dd" && chmod +x "$pf/bin/dd"
out=$(PATH="$pf/bin:$PATH" bash fleet/ami/host/prefetch.sh "$pf/list" 2>&1) || no "prefetch.sh failed: $out"
[ "$(sort "$pf/dd.log" 2>/dev/null | grep -c "if=$pf/a ")" = 3 ] || no "prefetch.sh did not read 3 pieces of a 2 MiB + 1 byte file: $(cat "$pf/dd.log" 2>/dev/null)"
grep -q missing "$pf/dd.log" 2>/dev/null && no "prefetch.sh read a file that does not exist"
rm -rf "$pf"

[ "$fail" = 0 ] && echo "fleet/ami checks: ok"
exit "$fail"
