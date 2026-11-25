#!/usr/bin/env bash
# Every Lean file of a library is imported by that library's Axioms.lean, the library's root
# imports Axioms.lean, the sweep is the last command of Axioms.lean, every library and
# executable is a default target of `lake build`, and the executables' modules declare no
# theorem. The axiom sweep in Axioms.lean looks at what the modules it
# imports declare; lake does not elaborate Axioms.lean again when a file it does not import is
# added, so this is checked here, outside lake, on every run of `make verify` and of the CI job.
set -euo pipefail
cd "$(dirname "$0")"
rc=0
for lib in WakeVerify GateVerify ResumeVerify; do
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
  # The sweep is the last command of Axioms.lean, so nothing is declared after it.
  sweep="#check_$(echo "${lib%Verify}" | tr '[:upper:]' '[:lower:]')_axioms"
  last=$(grep -v '^[[:space:]]*$' "$lib/Axioms.lean" | tail -n 1)
  if [ "$last" != "$sweep" ]; then
    echo "check-imports: the last line of $lib/Axioms.lean is not $sweep" >&2
    rc=1
  fi
done
# Every library and executable lakefile.toml declares is a default target, and each model's
# library is declared. `lake build` builds the default targets and nothing else, so a library
# dropped from that list would leave its proofs, its recorded runs and its axiom sweep unbuilt
# with this script, the build and both searches still passing.
defaults=$(sed -n 's/^defaultTargets *= *\[\(.*\)\]/\1/p' lakefile.toml | tr -d ' "' | tr ',' '\n')
[ -n "$defaults" ] || { echo "check-imports: lakefile.toml has no one-line defaultTargets list" >&2; exit 2; }
declared=$(awk '/^\[\[lean_(lib|exe)\]\]/ { want = 1; next }
                want && /^name *=/ { gsub(/[" ]/, ""); sub(/^name=/, ""); print; want = 0 }' lakefile.toml)
[ -n "$declared" ] || { echo "check-imports: lakefile.toml declares no library or executable" >&2; exit 2; }
while IFS= read -r t; do
  if ! grep -qxF "$t" <<<"$defaults"; then
    echo "check-imports: lakefile.toml declares $t and defaultTargets does not list it: lake build would not build it" >&2
    rc=1
  fi
done <<<"$declared"
for lib in WakeVerify GateVerify ResumeVerify; do
  if ! grep -qxF "$lib" <<<"$declared"; then
    echo "check-imports: lakefile.toml does not declare the library $lib" >&2
    rc=1
  fi
done
# The executables' modules are outside the sweeps: they declare no theorem and no axiom.
for f in *.lean; do
  case "$f" in WakeVerify.lean | GateVerify.lean | ResumeVerify.lean) continue ;; esac
  if grep -nE '^[[:space:]]*((private|protected|noncomputable|unsafe|partial)[[:space:]]+)*(theorem|lemma|axiom|opaque)[[:space:]]|native_decide|sorry' "$f" >&2; then
    echo "check-imports: $f, an executable's module, declares a theorem or an axiom the sweeps do not see" >&2
    rc=1
  fi
done
exit $rc
