#!/bin/bash
# Starts the nightly dogfood session (SKILL.md) headless, from the box's crontab, always as main has it:
#
#   30 10,11 * * * git -C $HOME/fcvm fetch -q origin main && bash -c "$(git -C $HOME/fcvm show origin/main:.claude/skills/nightly-dogfood/run.sh)"
#
# Both hours fire so that 03:30 Pacific is met in summer and winter time; the other one exits here. DOGFOOD_NOW=1 runs
# it at any hour. One run at a time (a lock). The session works in its own worktree, reset to origin/main, and writes
# to /mnt/fcvm-btrfs/fcvm-dogfood/<Pacific date>/: session.jsonl (the transcript), session.err and run.log.
set -u
[ "$(TZ=America/Los_Angeles date +%H)" = 03 ] || [ "${DOGFOOD_NOW:-}" = 1 ] || exit 0
root=/mnt/fcvm-btrfs/fcvm-dogfood
out=$root/$(TZ=America/Los_Angeles date +%F)
mkdir -p "$out"
exec 9>"$root/.lock"
flock -n 9 || { echo "$(date -u +%FT%TZ) another run holds the lock" >>"$out/run.log"; exit 0; }
repo=$HOME/fcvm wt=$HOME/src/fcvm-dogfood
export PATH=$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
fail() { echo "$(date -u +%FT%TZ) not started: $1" >>"$out/run.log"; exit 1; }
git -C "$repo" fetch -q origin main || fail "fetch"
# The worktree is this job's alone: whatever an interrupted night left in it is thrown away.
if [ -d "$wt" ]; then
  git -C "$wt" checkout -q -f --detach origin/main && git -C "$wt" clean -qfd || fail "reset of $wt"
else
  git -C "$repo" worktree add -q --detach "$wt" origin/main || fail "worktree add"
fi
cd "$wt" || fail "cd"
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] && [ -z "$(git status --porcelain)" ] || fail "$wt is not a clean origin/main"
echo "$(date -u +%FT%TZ) start $(git rev-parse --short HEAD)" >>"$out/run.log"
timeout -k 5m 6h claude -p "/nightly-dogfood" --permission-mode bypassPermissions \
  --output-format stream-json --verbose </dev/null >"$out/session.jsonl" 2>"$out/session.err"
status=$?
echo "$(date -u +%FT%TZ) exit $status" >>"$out/run.log"
exit $status
