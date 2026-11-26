#!/bin/bash
# Runs every axiom sweep, whatever the build directory holds, and pins the one package option
# the sweeps and the proofs rely on. Run after `lake build`, by `make verify` and the CI job.
#
# `lake build` elaborates a library's Axioms.lean, and so its sweep, only when that file or
# something it imports changed. On a restored build directory (CI restores verify/.lake) that
# can be never. `lake lean <file>` elaborates one file every time it is called, with the
# package's options, after building what it imports: so each */Axioms.lean is elaborated here on
# every run, and its sweep with it.
#
# The option: `leanOptions = { warningAsError = true }` in lakefile.toml. With it a warning fails
# the build: a `sorry` (also caught by the sweep, as an axiom), and a `#guard_msgs` that Lean
# could only partly check. check-imports.sh pins the rest of lakefile.toml.
#
# Exit status: 0 every sweep ran and passed; 1 one failed, or the option is not as pinned;
# 2 BLOCKED.
set -euo pipefail
cd "$(dirname "$0")"
for tool in lake grep; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
rc=0
if ! grep -qxF 'leanOptions = { warningAsError = true }' lakefile.toml; then
  echo "check-sweep: lakefile.toml has no line 'leanOptions = { warningAsError = true }': a warning would not fail the build" >&2
  rc=1
fi
n=0
for axioms in */Axioms.lean; do
  [ -f "$axioms" ] || continue
  n=$((n + 1))
  if lake lean "$axioms"; then
    echo "check-sweep: $axioms elaborated, its sweep passed"
  else
    echo "check-sweep: $axioms failed" >&2
    rc=1
  fi
done
[ "$n" -gt 0 ] || { echo "BLOCKED: found no */Axioms.lean" >&2; exit 2; }
exit $rc
