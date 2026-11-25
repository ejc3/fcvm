import Lean
import GateVerify.Spec
import GateVerify.Today
import GateVerify.Design
import GateVerify.Properties
import GateVerify.Configs
import GateVerify.Breaks
import GateVerify.Lemmas
import GateVerify.Socks
import GateVerify.Gens
import GateVerify.Ends
import GateVerify.Limits
import GateVerify.Proofs

/-!
# What each theorem rests on

`lake build` fails if anything this model declares comes to depend on anything beyond Lean's own
axioms (`propext`, `Classical.choice`, `Quot.sound`): no `sorry` (`sorryAx`), no added axiom, no
`native_decide` (which adds an axiom of its own for what the compiled code returned).

Two checks, as for the wake-up model. The sweep visits every declaration of every module of the
library, so one added later is covered without being listed. The `#guard_msgs` lines pin the
exact axioms of the theorems the documents cite, so the build also fails when one of those is
renamed or removed.
-/

open Lean Elab Command in
/-- Fails if anything declared in a module of this library rests on an axiom outside the allowed
three, if a module declares an axiom, if a source file of the library is not imported here (so
its declarations would not be looked at), or if it finds too few theorems or not the ones it
must (a sweep that inspects nothing must not pass).

Declarations are chosen by the module they are declared in, not by name: a `private theorem` and
one outside the namespace are looked at like any other. `lakefile.toml` builds every file of the
library's directory (`globs`), so a file nothing imports is still compiled, and the check below
then names it. -/
elab "#check_gate_axioms" : command => do
  let env ← getEnv
  let allowed : List Name := [``propext, ``Quot.sound, ``Classical.choice]
  let lib : Name := `GateVerify
  -- every source file of the library is imported by this one
  let dir := (System.FilePath.mk (← getFileName)).parent.getD "."
  let here := env.header.mainModule
  let modules := env.header.moduleNames
  for entry in (← dir.readDir) do
    if entry.path.extension == some "lean" then
      let name := lib ++ Name.mkSimple (entry.path.fileStem.getD "")
      unless name == here || modules.contains name do
        throwError "{name} is not imported by {here}: the axiom sweep would not see it"
  -- everything declared in the library's modules
  let decls := env.constants.fold (init := #[]) fun acc name info =>
    match env.getModuleIdxFor? name with
    | some idx => if lib.isPrefixOf modules[idx.toNat]! then acc.push (name, info) else acc
    | none => acc
  let mut bad : Array String := #[]
  let mut theorems : Array Name := #[]
  for (name, info) in decls do
    match info with
    | .axiomInfo _ => bad := bad.push s!"{name} is an axiom"
    | .thmInfo _ | .defnInfo _ | .opaqueInfo _ =>
      if info matches .thmInfo _ then theorems := theorems.push name
      let axioms ← collectAxioms name
      let extra := axioms.toList.filter (!allowed.contains ·)
      if !extra.isEmpty then bad := bad.push s!"{name} depends on {extra}"
    | _ => pure ()
  let must : List Name := [`Gate.design_neverStuck, `Gate.design_noStaleAtPublication, `Gate.design_fixedGeneration,
    `Gate.design_bounded, `Gate.design_settles, `Gate.loneEndRetired, `Gate.reachable_scal,
    `Gate.breaks_cookieAck]
  if theorems.size < 300 || must.any (!theorems.contains ·) then
    throwError "the axiom sweep found {theorems.size} theorems in {lib} and not the ones it must"
  if !bad.isEmpty then
    throwError "theorems resting on more than Lean's own axioms:\n{"\n".intercalate bad.toList}"

#check_gate_axioms

open Gate

/-- info: 'Gate.design_sourceUndisturbed' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_sourceUndisturbed
/-- info: 'Gate.design_fixedGeneration' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms design_fixedGeneration
/-- info: 'Gate.design_retiresOnly' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms design_retiresOnly
/-- info: 'Gate.design_noStaleAtPublication' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms design_noStaleAtPublication
/-- info: 'Gate.design_noSmuggledLate' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noSmuggledLate
/-- info: 'Gate.design_egressBeforeRetirement' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_egressBeforeRetirement
/-- info: 'Gate.design_hookPlaced' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_hookPlaced
/-- info: 'Gate.design_workloadEgress' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_workloadEgress
/-- info: 'Gate.design_noLastingChange' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noLastingChange
/-- info: 'Gate.design_noCapturedChange' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noCapturedChange
/-- info: 'Gate.design_neverStuck' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_neverStuck
/-- info: 'Gate.own_decreases' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms own_decreases
/-- info: 'Gate.design_bounded' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_bounded
/-- info: 'Gate.design_ends' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_ends
/-- info: 'Gate.design_restore_ends' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_restore_ends
/-- info: 'Gate.design_settles' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_settles
/-- info: 'Gate.clone_never_restarts' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms clone_never_restarts
/-- info: 'Gate.kill_restart_repeats' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms kill_restart_repeats
/-- info: 'Gate.design_udpRuleHolds' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_udpRuleHolds
/-- info: 'Gate.reachable_scal' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms reachable_scal
/-- info: 'Gate.reachable_flags' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms reachable_flags
/-- info: 'Gate.reachable_sk' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms reachable_sk
/-- info: 'Gate.reachable_gn' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms reachable_gn
/-- info: 'Gate.breaks_staleTuple' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_staleTuple
/-- info: 'Gate.breaks_cookieAck' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_cookieAck
/-- info: 'Gate.breaks_earlySnapshot' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_earlySnapshot
/-- info: 'Gate.breaks_agentDies' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_agentDies
/-- info: 'Gate.breaks_reusedTuple' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_reusedTuple
/-- info: 'Gate.failedCloneSnapshotEnds' depends on axioms: [propext] -/
#guard_msgs in #print axioms failedCloneSnapshotEnds
/-- info: 'Gate.loneEndRetired' depends on axioms: [propext] -/
#guard_msgs in #print axioms loneEndRetired
/-- info: 'Gate.mixedPairGoes' depends on axioms: [propext] -/
#guard_msgs in #print axioms mixedPairGoes
/-- info: 'Gate.manifestOneLook' depends on axioms: [propext] -/
#guard_msgs in #print axioms manifestOneLook
