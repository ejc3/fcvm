import Lean
import ResumeVerify.Spec
import ResumeVerify.Today
import ResumeVerify.Design
import ResumeVerify.Properties
import ResumeVerify.Configs
import ResumeVerify.Breaks
import ResumeVerify.Invariant
import ResumeVerify.Lemmas
import ResumeVerify.Proofs

/-!
# What each theorem rests on

`lake build` fails if anything this model declares comes to depend on anything beyond Lean's own
axioms (`propext`, `Classical.choice`, `Quot.sound`): no `sorry` (`sorryAx`), no added axiom, no
`native_decide` (which adds an axiom of its own for what the compiled code returned).

Two checks, as for the other two models. The sweep visits every declaration of every module of the
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
one outside the namespace are looked at like any other, and so is whatever this file declares
above the sweep. The sweep is this file's last command, and `../check-imports.sh` fails if it
is not, so nothing can be declared after it. `lakefile.toml` builds every file of the library's
directory (`globs`), so a file nothing imports is still compiled, and `check-imports.sh` names
it. -/
elab "#check_resume_axioms" : command => do
  let env ← getEnv
  let allowed : List Name := [``propext, ``Quot.sound, ``Classical.choice]
  let lib : Name := `ResumeVerify
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
    -- declared in this file, above the sweep
    | none => acc.push (name, info)
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
  let must : List Name := [`Resume.step_inv, `Resume.reachable_inv, `Resume.sound_healthy,
    `Resume.design_cloudHypervisor, `Resume.design_firecracker, `Resume.guest_ok,
    `Resume.breaks_ch_plain_spins, `Resume.breaks_before630_nvDump_clockOff]
  if theorems.size < 100 || must.any (!theorems.contains ·) then
    throwError "the axiom sweep found {theorems.size} theorems in {lib} and not the ones it must"
  if !bad.isEmpty then
    throwError "theorems resting on more than Lean's own axioms:\n{"\n".intercalate bad.toList}"


open Resume

/-- info: 'Resume.design_cloudHypervisor' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_cloudHypervisor
/-- info: 'Resume.design_firecracker' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_firecracker
/-- info: 'Resume.sound_noSpin' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms sound_noSpin
/-- info: 'Resume.reachable_inv' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms reachable_inv
/-- info: 'Resume.breaks_ch_plain_spins' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_ch_plain_spins
/-- info: 'Resume.breaks_before630_nvDump_clockOff' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_before630_nvDump_clockOff

-- The sweep, last: everything above is looked at. Nothing goes below this line.
#check_resume_axioms
