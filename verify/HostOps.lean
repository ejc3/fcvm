import WakeVerify.Breaks

/-!
# The model's kernel rules as operations for the C harness (`lake exe hostops <kernel>`)

Prints lines `operation | expected output`. `harness/run.sh` feeds the operations to a C program
built from the kernel's own functions (cut verbatim from the pinned release) and compares what
they print with the expectation, which is computed here from the rule book alone. The operations
are described at the top of `harness/harness.c`.

Two parts:

1. **Every rule on every input.** For each value of (`mp_state`, `pv_unhalted`, `apf.halted`):
   `__kvm_emulate_halt` and `vcpu_block` with and without a completion on the done list, with
   interrupts on and off, with and without an interrupt pending, and `vcpu_block` with an NMI;
   a completion of each kind dequeued through `kvm_check_async_pf_completion`, with the
   page-ready slot free and occupied; the wake-everyone completion queued through
   `kvm_pv_enable_async_pf`; the kick; `KVM_GET_MP_STATE`; `KVM_SET_MP_STATE` to every state.
2. **Every recorded run.** Each run of `Breaks.lean`, step by step: the kernel functions a step
   calls, and the state of both vCPUs after it.
-/

open Wake

def mpName : Mp → String
  | .runnable => "runnable"
  | .uninitialized => "uninitialized"
  | .halted => "halted"
  | .initReceived => "initReceived"
  | .apResetHold => "apResetHold"

def bit (b : Bool) : String := if b then "1" else "0"

def hv (h : Hv) : String := s!"{mpName h.mp} {bit h.pv} {bit h.apfHalted}"

/-- What the harness prints for a vCPU: the rule book's state, whether a completion is on the
done list, whether the page-ready slot is occupied, whether the page-ready vector is pending. -/
def full (h : Hv) (done slot pr : Bool) : String :=
  s!"{hv h} done={bit done} slot={bit slot} pr={bit pr}"

def vfull (v : Vcpu) : String := full v.h v.apf.isDone v.slot v.apfIrq

def allMp : List Mp := [.runnable, .uninitialized, .halted, .initReceived, .apResetHold]

def allHv : List Hv := Id.run do
  let mut out : List Hv := []
  for mp in allMp do
    for pv in [false, true] do
      for ah in [false, true] do
        out := out ++ [{ mp, pv, apfHalted := ah }]
  return out

def line (op expect : String) : String := s!"{op} | {expect}"

def kindName : ApfKind → String
  | .pv => "pv"
  | .synth => "synth"
  | .all => "all"

/-- Part 1. -/
def table (K : Kernel) : List String := Id.run do
  let mut out : List String := []
  for h in allHv do
    let load := line s!"set 0 {hv h}" (full h false false false)
    for done in [false, true] do
      for ifOn in [false, true] do
        for irq in [false, true] do
          let ev := done || (ifOn && irq)
          let pre := [load] ++ (if done then [line "queue 0 synth" (full h true false false)] else [])
            ++ [line s!"env 0 {bit ifOn} {bit irq} 0" (full h done false false)]
          for st in [Mp.halted, Mp.apResetHold] do
            out := out ++ pre ++ [line s!"halt 0 {mpName st}" (full (K.halt ev st h) done false false)]
          let runnable := (h.mp == .runnable && !h.apfHalted) || h.pv || ev
          out := out ++ pre ++ [line "block 0"
            (if runnable then full (K.unhalt h) done false false ++ " ran"
             else full h done false false ++ " blocked")]
    -- an NMI ends the wait whatever RFLAGS.IF is
    out := out ++ [load, line "env 0 0 0 1" (full h false false false),
      line "block 0" (full (K.unhalt h) false false false ++ " ran")]
    -- a completion dequeued on entry: the guest without the interface (synthetic halt only)
    out := out ++ [load, line "queue 0 synth" (full h true false false),
      line "enter 0" (full (K.present h) false false false)]
    -- and with it: each kind, slot free and slot occupied
    for k in [ApfKind.pv, ApfKind.synth] do
      let notify := k != .synth
      out := out ++ [load, line "pvapf 0" (full h false false false),
        line s!"queue 0 {kindName k}" (full h true false false),
        line "enter 0" (full (K.present h) false notify notify)]
      out := out ++ [load, line "pvapf 0" (full h false false false),
        line "slot 0 1" (full h false true false),
        line s!"queue 0 {kindName k}" (full h true true false),
        line "enter 0" (full h true true false)]
    -- the wake-everyone completion, queued by the MSR write itself
    out := out ++ [load, line "enable 0" (full h true false false),
      line "enter 0" (full (K.present h) false true true)]
    out := out ++ [load, line "slot 0 1" (full h false true false),
      line "enable 0" (full h true true false), line "enter 0" (full h true true false)]
    out := out ++ [load, line "kick 0" (full (K.kick h) false false false)]
    out := out ++ [load, line "getmp 0" (mpName (K.getMpState h))]
    for m in allMp do
      out := out ++ [load, line s!"setmp 0 {mpName m}" (full (K.setMpState m h) false false false)]
  return out

def vidNum : Vid → String
  | .a => "0"
  | .b => "1"

/-- The interrupt environment the harness needs before a halt or a block of `v`. -/
def envOp (i : Vid) (v : Vcpu) (ifOn nmi : Bool) : String :=
  line s!"env {vidNum i} {bit ifOn} {bit (v.ipi || v.tmr == .irr)} {bit nmi}" (vfull v)

/-- The restore of one vCPU, ioctl by ioctl. -/
def restoreOps (R : Rules) (C : Cfg) (i : Vid) (second : Bool) (v : Vcpu) : List String := Id.run do
  let K := R.kernel
  let sv := save K v
  let n := vidNum i
  let mut acc : Vcpu × Bool := (v.fresh second, false)
  let mut out := [line s!"getmp {n}" (mpName sv.mp),
    line s!"fresh {n}" (full (freshHv second) false false false),
    line s!"slot {n} {bit v.slot}" (vfull acc.1)]
  for io in R.restore do
    acc := restoreOne K C sv acc io
    match io with
    | .setMpState => out := out ++ [line s!"setmp {n} {mpName sv.mp}" (vfull acc.1)]
    | .setLapic => out := out ++ [line s!"irr {n} {bit sv.apfIrq}" (vfull acc.1)]
    | .setMsrs => if C.pvApf then out := out ++ [line s!"enable {n}" (vfull acc.1)]
    | .kick => if sv.mp == .runnable then out := out ++ [line s!"kick {n}" (vfull acc.1)]
  return out

/-- The kernel functions one step calls, with the state the model expects after each. -/
def stepOps (R : Rules) (C : Cfg) (l : Label) (s t : State) : List String :=
  let both := [line "show 0" (vfull t.a), line "show 1" (vfull t.b)]
  let ops : List String :=
    match l with
    | .clone => restoreOps R C .a false s.a ++ restoreOps R C .b true s.b
    | .vcpu i act =>
      let v := s.get i
      let w := t.get i
      let n := vidNum i
      match act with
      | .step =>
        match v.pc with
        | .hdHlt | .ndHlt | .idHlt | .idHltT | .off =>
          [envOp i v w.ifOn false, line s!"halt {n} halted" (vfull w)]
        | .ulKick => [line s!"kick {vidNum i.other}" (vfull (t.get i.other))]
        | _ => []
      | .hostRun =>
        if v.running then (if v.apf.isDone then [line s!"enter {n}" (vfull w)] else [])
        else [envOp i v v.ifOn false, line s!"block {n}" (vfull w ++ " ran")]
      | .nmi => [envOp i v v.ifOn true, line s!"block {n}" (vfull w ++ " ran"), envOp i w w.ifOn false]
      | .extIrq =>
        [line s!"env {n} 1 1 0" (vfull v), line s!"block {n}" (vfull w ++ " ran"),
         envOp i w w.ifOn false]
      | .faultSynth | .refault => [line s!"apfhalt {n}" (vfull w)]
      | .apfDone =>
        match v.apf with
        | .flight k => [line s!"queue {n} {kindName k}" (vfull w)]
        | _ => []
      | .irqApf => [line s!"ack {n}" (vfull w)]
      | .setMpSame =>
        let m := R.kernel.getMpState v.h
        [line s!"getmp {n}" (mpName m), line s!"setmp {n} {mpName m}" (vfull w)]
      | _ => []
    | _ => []
  ops ++ both

/-- Part 2: one recorded run. A recorded run that uses a step the harness has no kernel function
for (nested guests, Xen, hotplug) would not be covered; none does. -/
def runOps (R : Rules) (C : Cfg) (tr : List Label) : List String := Id.run do
  let mut s := init
  let mut out := [line "set 0 runnable 0 0" (vfull s.a), line "set 1 runnable 0 0" (vfull s.b)]
  if C.pvApf then
    out := out ++ [line "pvapf 0" (vfull s.a), line "pvapf 1" (vfull s.b)]
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
