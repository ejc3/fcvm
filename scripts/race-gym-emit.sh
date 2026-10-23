#!/bin/bash
# Add a race gym replay as a named case in tests/test_lifecycle_interleave.rs
# (docs/race-gym.md, section 6). Usage: race-gym-emit.sh <replay.json> <name> [<oracles>]
# With oracles (PIN=outcome), the case fails while one of those oracles fails; other
# oracles' failures are printed and belong to other cases. A divergence always fails.
set -euo pipefail
replay=${1:-}
name=${2:-}
[ -n "$replay" ] && [ -f "$replay" ] || { echo "race-gym-emit: REPLAY=<file> must name a replay file" >&2; exit 2; }
[[ "$name" =~ ^[a-z0-9_]+$ ]] || { echo "race-gym-emit: EMIT=$name must be lowercase letters, digits and _" >&2; exit 2; }
command -v jq >/dev/null || { echo "race-gym-emit: jq missing" >&2; exit 2; }
jq -e '.format == 1 and (.steps | length) > 0' "$replay" >/dev/null \
  || { echo "race-gym-emit: $replay is not a format-1 replay with steps" >&2; exit 2; }

root=$(cd "$(dirname "$0")/.." && pwd)
tests="$root/tests/test_lifecycle_interleave.rs"
case_file="$root/tests/race_gym/cases/$name.json"
fn="test_lifecycle_interleave_gym_$name"
grep -q "fn $fn(" "$tests" && { echo "race-gym-emit: $fn already exists" >&2; exit 2; }
pin=${3:-}
if [ -n "$pin" ]; then
  [[ "$pin" =~ ^[a-z,]+$ ]] || { echo "race-gym-emit: PIN=$pin must be oracle names (outcome,leak,...)" >&2; exit 2; }
  # A pin must name a failure the replay recorded, unless the file already carries that pin:
  # a schedule re-recorded after its fix no longer records the failure it pins.
  jq -e --arg p "$pin" '([.failed[].oracle] + (.pin_oracles // [])) as $o | ($p | split(",")) | all(. as $x | $o | index($x))' "$replay" >/dev/null \
    || { echo "race-gym-emit: $replay recorded no failure for every oracle in PIN=$pin" >&2; exit 2; }
fi
mkdir -p "$(dirname "$case_file")"
if [ -n "$pin" ]; then
  jq --arg p "$pin" '.pin_oracles = ($p | split(","))' "$replay" > "$case_file"
else
  cp "$replay" "$case_file"
fi

if ! grep -q '^mod race_gym;' "$tests"; then
  cat >> "$tests" <<'RS'

// ---------------------------------------------------------------------------
// Race gym cases (docs/race-gym.md): each replays a recorded schedule of concurrent
// actors and fails while the race it caught exists. `make race-gym-cases` runs them.
// ---------------------------------------------------------------------------

#[cfg(feature = "race-gym")]
#[path = "race_gym/mod.rs"]
mod race_gym;
RS
fi
summary=$(jq -r '(.pin_oracles // []) as $p | [.failed[] | select(($p | length) == 0 or (.oracle as $o | $p | index($o))) | .oracle + ": " + .detail] | first // "no failure recorded"' "$case_file" | cut -c1-120 | tr -d '"')
cat >> "$tests" <<RS

/// Race gym replay \`$name\` ($(jq -r '.scenario' "$replay"), $(jq -c '.schedule' "$replay")).
/// Recorded failure: $summary
#[cfg(feature = "race-gym")]
#[tokio::test]
async fn $fn() -> Result<()> {
    race_gym::replay_case(include_str!("race_gym/cases/$name.json")).await
}
RS
# Leave the file as `make lint`'s rustfmt check expects it.
rustfmt --edition 2021 "$tests"
echo "race-gym-emit: added $fn and $case_file"
