import Lean
import WakeVerify.Spec
import WakeVerify.Today
import WakeVerify.Design
import WakeVerify.Properties
import WakeVerify.Breaks
import WakeVerify.Lemmas
import WakeVerify.Proofs

/-!
# What each theorem rests on

`lake build` fails if any theorem of the model comes to depend on anything beyond Lean's own
axioms (`propext`, `Classical.choice`, `Quot.sound`): no `sorry` (`sorryAx`), no added axiom, no
`native_decide` (which adds an axiom of its own for what the compiled code returned).

Two checks. The sweep below visits every declaration of every module of the library, so one
added later is covered without being listed. The `#guard_msgs` lines after it pin the exact axioms of the
theorems the documents cite, so the build also fails when one of those is renamed or removed.
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
elab "#check_wake_axioms" : command => do
  let env ← getEnv
  let allowed : List Name := [``propext, ``Quot.sound, ``Classical.choice]
  let lib : Name := `WakeVerify
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
  let must : List Name := [`Wake.design_noLostKick, `Wake.breaks_pageReady]
  if theorems.size < 100 || must.any (!theorems.contains ·) then
    throwError "the axiom sweep found {theorems.size} theorems in {lib} and not the ones it must"
  if !bad.isEmpty then
    throwError "theorems resting on more than Lean's own axioms:\n{"\n".intercalate bad.toList}"

#check_wake_axioms

open Wake

/-- info: 'Wake.design_noLostKick' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noLostKick
/-- info: 'Wake.design_noHang' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noHang
/-- info: 'Wake.design_noStuckCall' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noStuckCall
/-- info: 'Wake.design_mutualExclusion' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_mutualExclusion
/-- info: 'Wake.design_noOrphanHalt' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noOrphanHalt
/-- info: 'Wake.design_noLostTimer' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noLostTimer
/-- info: 'Wake.design_noStrandedNode' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noStrandedNode
/-- info: 'Wake.design_sound' depends on axioms: [propext] -/
#guard_msgs in #print axioms design_sound
/-- info: 'Wake.v614Kick_sound' depends on axioms: [propext] -/
#guard_msgs in #print axioms v614Kick_sound
/-- info: 'Wake.v614_sound_without_snapshots' depends on axioms: [propext] -/
#guard_msgs in #print axioms v614_sound_without_snapshots
/-- info: 'Wake.v733Patched_sound_without_snapshots' depends on axioms: [propext] -/
#guard_msgs in #print axioms v733Patched_sound_without_snapshots
/-- info: 'Wake.v733_not_keepsKick' depends on axioms: [propext] -/
#guard_msgs in #print axioms v733_not_keepsKick
/-- info: 'Wake.hlt_rechecks' depends on axioms: [propext] -/
#guard_msgs in #print axioms hlt_rechecks
/-- info: 'Wake.design_survives' depends on axioms: [propext] -/
#guard_msgs in #print axioms design_survives
/-- info: 'Wake.breaks_pageReady' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_pageReady
/-- info: 'Wake.breaks_snapshot' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_snapshot
/-- info: 'Wake.breaks_wakeAll' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_wakeAll
