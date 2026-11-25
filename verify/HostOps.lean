import WakeVerify.Breaks

/-!
# The model's kernel rules as operations for the C harness (`lake exe hostops <kernel>`)

Prints lines `operation | expected output`. `harness/run.sh` feeds the operations to a C program
built from the kernel's own functions (cut verbatim from the pinned release) and compares what
they print with the expectation, which is computed here from the rule book alone.

Two parts:

1. **Every rule on every input.** For each kernel rule the harness has the function for (`halt`,
   the end of `vcpu_block`, `present`, `kick`, `KVM_GET_MP_STATE`, `KVM_SET_MP_STATE`) and each of
   the 16 values of (`mp_state`, `pv_unhalted`, `apf.halted`), with and without an event pending.
2. **Every recorded run.** Each run of `Breaks.lean`, step by step: the kernel functions a step
   calls, and the state of both vCPUs after it.
-/

open Wake

def mpName : Mp → String
  | .runnable => "runnable"
  | .halted => "halted"
  | .initReceived => "initReceived"
  | .apResetHold => "apResetHold"

def bit (b : Bool) : String := if b then "1" else "0"

def hv (h : Hv) : String := s!"{mpName h.mp} {bit h.pv} {bit h.apfHalted}"

def allMp : List Mp := [.runnable, .halted, .initReceived, .apResetHold]

def allHv : List Hv := Id.run do
  let mut out : List Hv := []
  for mp in allMp do
    for pv in [false, true] do
      for ah in [false, true] do
        out := out ++ [{ mp, pv, apfHalted := ah }]
  return out

def line (op expect : String) : String := s!"{op} | {expect}"

/-- Part 1. -/
def table (K : Kernel) : List String := Id.run do
  let mut out : List String := []
  for h in allHv do
    let load := line s!"set 0 {hv h}" (hv h)
    for ev in [false, true] do
      for st in [Mp.halted, Mp.apResetHold] do
        out := out ++ [load, line s!"halt 0 {bit ev} {mpName st}" (hv (K.halt ev st h))]
      let runnable := (h.mp == .runnable && !h.apfHalted) || h.pv || ev
      out := out ++ [load, line s!"block 0 {bit ev}"
        (if runnable then hv (K.unhalt h) ++ " ran" else hv h ++ " blocked")]
    out := out ++ [load, line "present 0" (hv (K.present h))]
    out := out ++ [load, line "kick 0" (hv (K.kick h))]
    out := out ++ [load, line "getmp 0" (mpName (K.getMpState h))]
    for m in allMp do
      out := out ++ [load, line s!"setmp 0 {mpName m}" (hv (K.setMpState m h))]
  return out

def vidNum : Vid → String
  | .a => "0"
  | .b => "1"

/-- The restore of one vCPU, ioctl by ioctl. -/
def restoreOps (R : Rules) (C : Cfg) (i : Vid) (v : Vcpu) : List String := Id.run do
  let K := R.kernel
  let sv := save K v
  let n := vidNum i
  let mut out := [line s!"getmp {n}" (mpName sv.mp), line s!"fresh {n}" (hv v.fresh.h)]
  let mut acc : Vcpu × Bool := (v.fresh, false)
  for io in R.restore do
    acc := restoreOne K C sv acc io
    match io with
    | .setMpState => out := out ++ [line s!"setmp {n} {mpName sv.mp}" (hv acc.1.h)]
    | .kick => if sv.mp == .runnable then out := out ++ [line s!"kick {n}" (hv acc.1.h)]
    | _ => pure ()
  return out

/-- The kernel functions one step calls, with the state the model expects after each. -/
def stepOps (R : Rules) (C : Cfg) (l : Label) (s t : State) : List String :=
  let both := [line "show 0" (hv t.a.h), line "show 1" (hv t.b.h)]
  let ops : List String :=
    match l with
    | .clone => restoreOps R C .a s.a ++ restoreOps R C .b s.b
    | .vcpu i act =>
      let v := s.get i
      let n := vidNum i
      let after := hv (t.get i).h
      match act with
      | .step =>
        match v.pc with
        | .hdHlt | .ndHlt | .idHlt | .idHltT | .off =>
          [line s!"halt {n} {bit (t.get i).events} halted" after]
        | .ulKick => [line s!"kick {vidNum i.other}" (hv (t.get i.other).h)]
        | _ => []
      | .hostRun =>
        if v.running then (if v.apf.isDone then [line s!"present {n}" after] else [])
        else [line s!"block {n} {bit v.events}" (after ++ " ran")]
      | .nmi | .extIrq => [line s!"block {n} 1" (after ++ " ran")]
      | .faultSynth | .refault => [line s!"apfhalt {n}" after]
      | .setMpSame =>
        let m := R.kernel.getMpState v.h
        [line s!"getmp {n}" (mpName m), line s!"setmp {n} {mpName m}" after]
      | _ => []
    | _ => []
  ops ++ both

/-- Part 2: one recorded run. -/
def runOps (R : Rules) (C : Cfg) (tr : List Label) : List String := Id.run do
  let mut s := init
  let mut out := [line "fresh 0" (hv s.a.h), line "fresh 1" (hv s.b.h)]
  for l in tr do
    match step R C l s with
    | none => return out ++ ["MODEL: label not enabled | never"]
    | some t =>
      out := out ++ stepOps R C l s t
      s := t
  return out

def main (args : List String) : IO UInt32 := do
  let want := args.head?.getD ""
  let kernels : List (String × Kernel) :=
    [("v6.14", V614.kernel), ("v7.3.3", V733.kernel), ("v7.3.3-patched", V733Patched.kernel)]
  match kernels.find? (·.1 == want) with
  | none =>
    IO.eprintln "usage: hostops v6.14 | v7.3.3 | v7.3.3-patched"
    return 2
  | some (_, K) =>
    for l in table K do IO.println l
    for (R, C, tr) in recordedRuns do
      if R.kernel.name == K.name then
        for l in runOps R C tr do IO.println l
    return 0
