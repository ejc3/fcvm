#!/bin/bash
# Starts the nightly dogfood session (SKILL.md) headless, from the box's crontab, always as main has it:
#
#   30 10,11 * * * git -C $HOME/fcvm fetch -q origin main && bash -c "$(git -C $HOME/fcvm show origin/main:.claude/skills/nightly-dogfood/run.sh)"
#
# Both hours fire so that 03:30 Pacific is met in summer and winter time; the other one exits here. DOGFOOD_NOW=1 runs
# it at any hour. One run at a time (a lock). The session works in its own worktree, reset to origin/main, and writes
# to /mnt/fcvm-btrfs/fcvm-dogfood/<Pacific date>/<UTC start time>/: session.jsonl (the transcript), session.err,
# run.log and machines (one line per instance the session launched: `pbox <slot> <instance id>` or `gpubox <instance id>`).
# Whatever is listed there and still running when the session ends is taken down here, so a timeout cannot leave a box
# billing.
set -u
[ "$(TZ=America/Los_Angeles date +%H)" = 03 ] || [ "${DOGFOOD_NOW:-}" = 1 ] || exit 0
root=/mnt/fcvm-btrfs/fcvm-dogfood
out=$root/$(TZ=America/Los_Angeles date +%F)/$(date -u +%H%M%S)
mkdir -p "$out" || { echo "cannot create $out" >&2; exit 1; }
log() { echo "$(date -u +%FT%TZ) $*" >>"$out/run.log"; }
exec 9>"$root/.lock" || { log "not started: cannot open the lock file"; exit 1; }
flock -n 9
case $? in
  0) ;;
  1) log "another run holds the lock"; exit 0 ;;
  *) log "not started: flock failed"; exit 1 ;;
esac
repo=$HOME/fcvm wt=$HOME/src/fcvm-dogfood
export PATH=$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
fail() { log "not started: $1"; exit 1; }
git -C "$repo" fetch -q origin main || fail "fetch"
# The worktree is this job's alone: whatever an interrupted night left in it is thrown away. Before anything resets it,
# it must be a real directory (not a symlink) that git lists as a worktree of $repo at exactly that path.
if [ -e "$wt" ] || [ -L "$wt" ]; then
  [ -d "$wt" ] && [ ! -L "$wt" ] || fail "$wt is not a plain directory"
  git -C "$repo" worktree list --porcelain | grep -qxF "worktree $wt" || fail "$wt is not a worktree of $repo"
  # git's list can hold a stale entry for a path another clone now occupies: ask the directory itself as well.
  [ "$(git -C "$wt" rev-parse --show-toplevel 2>/dev/null)" = "$wt" ] || fail "$wt is not its own worktree root"
  [ "$(cd "$wt" && realpath "$(git rev-parse --git-common-dir)")" = "$(realpath "$repo/.git")" ] || fail "$wt does not belong to $repo"
  git -C "$wt" checkout -q -f --detach origin/main && git -C "$wt" clean -qfd || fail "reset of $wt"
else
  git -C "$repo" worktree add -q --detach "$wt" origin/main || fail "worktree add"
fi
cd "$wt" || fail "cd"
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] && [ -z "$(git status --porcelain)" ] || fail "$wt is not a clean origin/main"
export DOGFOOD_RUN_DIR=$out
: >"$out/machines"
log "start $(git rev-parse --short HEAD)"
timeout -k 5m 6h claude -p "/nightly-dogfood" --permission-mode bypassPermissions \
  --output-format stream-json --verbose </dev/null >"$out/session.jsonl" 2>"$out/session.err"
status=$?
log "exit $status"
# Take down what the session launched and left running. Only instances the session recorded, by id.
while read -r kind a b; do
  case $kind in
    # `--instance` makes pbox act only while the slot still holds that instance.
    pbox) pbox down "$a" --instance "$b" >>"$out/run.log" 2>&1 && log "pbox down $a ($b)" || log "pbox down $a ($b) refused or failed" ;;
    # gpubox.sh assumes the fleet account's role itself, so the id is handed to it rather than to a bare aws call.
    gpubox) bash fleet/tools/gpubox.sh down "$a" >>"$out/run.log" 2>&1 && log "gpubox down $a" || log "gpubox down $a failed" ;;
  esac
done <"$out/machines"
exit $status
