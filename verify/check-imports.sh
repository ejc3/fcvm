#!/usr/bin/env bash
# Every Lean file of a library is imported by that library's Axioms.lean, and the library's
# root imports Axioms.lean. The axiom sweep in Axioms.lean looks at what the modules it imports
# declare; lake does not elaborate Axioms.lean again when a file it does not import is added, so
# this is checked here, outside lake, on every run of `make verify` and of the CI job.
set -euo pipefail
cd "$(dirname "$0")"
rc=0
for lib in WakeVerify GateVerify; do
  [ -f "$lib/Axioms.lean" ] || { echo "check-imports: $lib/Axioms.lean is missing" >&2; exit 2; }
  n=0
  while IFS= read -r f; do
    m=${f#"$lib"/}; m=${m%.lean}; m=${m//\//.}
    n=$((n + 1))
    [ "$m" = Axioms ] && continue
    if ! grep -qx "import $lib\.$m" "$lib/Axioms.lean"; then
      echo "check-imports: $f is not imported by $lib/Axioms.lean: the axiom sweep would not see it" >&2
      rc=1
    fi
  done < <(find "$lib" -name '*.lean' | sort)
  [ "$n" -gt 1 ] || { echo "check-imports: found no Lean files under $lib" >&2; exit 2; }
  if ! grep -qx "import $lib\.Axioms" "$lib.lean"; then
    echo "check-imports: $lib.lean does not import $lib.Axioms" >&2
    rc=1
  fi
done
exit $rc
