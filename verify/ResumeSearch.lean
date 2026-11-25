import ResumeVerify.Spec
import ResumeVerify.Today
import ResumeVerify.Design
import ResumeVerify.Properties
import ResumeVerify.Configs
import Std.Data.HashMap

/-!
# `lake exe resume-breaks`: the exhaustive search

Breadth-first over every reachable state of each rule book in each configuration, so the first
state found that breaks a property is at the end of a shortest run. Every state is expanded,
broken or not. `lake exe resume-breaks` prints one line per rule book and configuration (the
text of `ResumeVerify/search.expected`); `lake exe resume-breaks traces` adds the runs, and
`lake exe resume-breaks spins` lists every kind of state with #2155's signature.
-/

open Resume

def vids : List Vid := [false, true]

def allLabels : Array Label :=
  ((vids.flatMap fun v =>
      ([GAct.arm, .toIdle, .wfi, .ack .tmr, .ack .sgi, .ack .spi, .eoi, .send, .l2].map (Label.g v))
        ++ [Label.fire v, Label.wake v])
    ++ ([.dev, .tick, .wait, .pause, .save, .resume, .restore] : List Label)).toArray

def vn (v : Vid) : String := if v then "1" else "0"

def labelTerm : Label → String
  | .g v a => s!".g {v} " ++ (match a with
      | .arm => ".arm" | .toIdle => ".toIdle" | .wfi => ".wfi" | .ack .tmr => "(.ack .tmr)"
      | .ack .sgi => "(.ack .sgi)" | .ack .spi => "(.ack .spi)" | .eoi => ".eoi" | .send => ".send"
      | .l2 => ".l2")
  | .fire v => s!".fire {v}"
  | .wake v => s!".wake {v}"
  | .dev => ".dev" | .tick => ".tick" | .wait => ".wait" | .pause => ".pause" | .save => ".save"
  | .resume => ".resume" | .restore => ".restore"

def describe : Label → String
  | .g v .arm => s!"CPU {vn v} programs its tick timer one unit ahead"
  | .g v .toIdle => s!"CPU {vn v} enters the idle loop"
  | .g v .wfi => s!"CPU {vn v} executes WFI with nothing pending: its vCPU thread blocks"
  | .g v (.ack .tmr) => s!"CPU {vn v} acknowledges its timer interrupt: active, active priority set"
  | .g v (.ack .sgi) => s!"CPU {vn v} acknowledges the IPI: active, active priority set"
  | .g v (.ack .spi) => s!"CPU {vn v} acknowledges the device's interrupt: active, active priority set"
  | .g v .eoi => s!"CPU {vn v} finishes its handler and writes the EOI"
  | .g v .send => s!"CPU {vn v} sends a cross-call IPI to the other CPU and waits for it"
  | .g v .l2 => s!"CPU {vn v} (a guest hypervisor) loads a nested vCPU: CNTVOFF_EL2 is not zero"
  | .fire v => s!"CPU {vn v}'s timer fires: its line goes up"
  | .wake v => s!"vCPU {vn v}'s thread wakes: an interrupt is pending for it"
  | .dev => "the device raises its interrupt"
  | .tick => "guest time passes"
  | .wait => "host time passes while the guest does not run"
  | .pause => "the VMM pauses the vCPUs"
  | .save => "the VMM saves the state"
  | .resume => "the VMM resumes the vCPUs"
  | .restore => "a new VM is restored from the image"

def bstr (n : String) (b : Bool) : String := if b then " " ++ n else ""

def showIrq (n : String) (i : IrqSt) : String :=
  n ++ "[" ++ (if i.en then "en" else "DIS") ++ bstr "latch" i.latch ++ bstr "line" i.line ++ bstr "ACTIVE" i.act ++ "]"

def showG : G → String
  | .run => "run" | .idle => "idle" | .hdl .tmr => "in-timer-handler" | .hdl .sgi => "in-IPI-handler"
  | .hdl .spi => "in-device-handler"

def showVcpu (c : Vcpu) : String :=
  showG c.g ++ "/" ++ (match c.thr with | .guest => "in-guest" | .blocked => "blocked" | .user => "userspace")
    ++ bstr "STOPPED" c.stopped ++ bstr "offline" c.gOff ++ bstr "AP1R" c.ap
    ++ (if c.pmr then "" else " PMR-shut") ++ (if c.grp then "" else " GRP-off")
    ++ " " ++ showIrq "tmr" c.tmr ++ " " ++ showIrq "sgi" c.sgi
    ++ (if c.tEn then s!" timer@{c.cv}" else "") ++ bstr "call-owed" c.call ++ bstr "L2" c.l2

def showCtr : Ctr → String
  | .fresh => "FRESH"
  | .sync e => if e.t == 0 && e.o == 0 then "exact" else s!"off by {e.t} of host time, {e.o} of CNTVOFF_EL2"

def showState (s : State) : String :=
  "cpu0: " ++ showVcpu s.vm.c0 ++ "; cpu1: " ++ showVcpu s.vm.c1 ++ "; " ++ showIrq "spi" s.vm.spi
    ++ bstr "device-waits" s.vm.dev ++ "; counter " ++ showCtr s.vm.ctr

structure Found where
  states : Array State
  parent : Array (Nat × Nat)
  first : Array (Option Nat)
  complete : Bool

partial def explore (R : Rules) (C : Cfg) (limit : Nat) : Found := Id.run do
  let cs := (checks C).toArray
  let s0 := init C
  let mut states : Array State := #[s0]
  let mut parent : Array (Nat × Nat) := #[(0, 0)]
  let mut index : Std.HashMap State Nat := (Std.HashMap.emptyWithCapacity 4096).insert s0 0
  let mut first : Array (Option Nat) := cs.map (fun _ => none)
  let mut next := 0
  let mut complete := true
  while next < states.size do
    let s := states[next]!
    for ci in [0:cs.size] do
      if first[ci]!.isNone && cs[ci]!.2 s then first := first.set! ci (some next)
    for li in [0:allLabels.size] do
      match step R C allLabels[li]! s with
      | none => pure ()
      | some t =>
        if !index.contains t then
          index := index.insert t states.size
          states := states.push t
          parent := parent.push (next, li)
    next := next + 1
    if states.size > limit then
      complete := false
      break
  return { states, parent, first, complete }

partial def pathTo (f : Found) (i : Nat) : List Label :=
  let rec go (i : Nat) (acc : List Label) : List Label :=
    if i == 0 then acc else
      let (p, li) := f.parent[i]!
      go p (allLabels[li]! :: acc)
  go i []

def nvBooks : List Rules := [Today.forkBefore630, Today.forkNested, Design.firecracker]
def elBooks : List Rules :=
  [Today.cloudHypervisor, Today.cloudHypervisorRepin, Today.cloudHypervisorUpstream false,
   Today.cloudHypervisorUpstream true, Today.forkDefault, Design.cloudHypervisor, Design.firecracker]

def booksFor (C : Cfg) : List Rules := if C.nv then nvBooks else elBooks

/-- Each repair of the design put back, one at a time (`lake exe resume-breaks mutants`). -/
def mutants : List Rules :=
  let d := Design.cloudHypervisor
  let f := Design.firecracker
  [{ d with name := "ch: slots read from vCPU 0 again", slotsFromRestore := true },
   { d with name := "ch: image holds the counter of the save", cntAtPause := false },
   { d with name := "ch: resume does not take out the pause", freeze := false },
   { d with name := "ch: load to resume not taken out", freezeLoaded := false },
   { d with name := "ch: restore adds the wall time since the pause", advance := true },
   { d with name := "ch: lines not saved", levelInfo := false },
   { d with name := "ch: clear-registers written with the saved bits", clearInverted := false },
   { d with name := "ch: no KVM_ARM_SET_COUNTER_OFFSET on restore", restore := d.restore.filter (· != .offset) },
   { d with name := "ch: lines not restored", restore := d.restore.filter (· != .level) },
   { d with name := "ch: counter of the last vCPU not replayed, no offset",
            restore := d.restore.filter (fun o => o != .offset && o != .cntv true && o != .cntp true) },
   { f with name := "fc: image holds the counter of the save", cntAtPause := false },
   { f with name := "fc: resume does not take out the pause", freeze := false },
   { f with name := "fc: load to resume not taken out", freezeLoaded := false },
   { f with name := "fc: lines not saved", levelInfo := false },
   { f with name := "fc: clear-registers written with the saved bits", clearInverted := false },
   { f with name := "fc: no KVM_ARM_SET_COUNTER_OFFSET on restore", restore := f.restore.filter (· != .offset) },
   { f with name := "fc: lines not restored", restore := f.restore.filter (· != .level) }]

def printRun (R : Rules) (C : Cfg) (title : String) (labels : List Label) : IO Unit := do
  IO.println s!"\n### {title} ({labels.length} steps)"
  let mut st := init C
  let mut n := 1
  for l in labels do
    IO.println s!"  {n}. {describe l}"
    n := n + 1
    match step R C l st with
    | some t => st := t
    | none => IO.println "  NOT POSSIBLE"
  IO.println s!"  => {showState st}"
  IO.println s!"  [{", ".intercalate (labels.map labelTerm)}]"

/-- What makes a vCPU spin, as a capture would show it. -/
def spinKind (C : Cfg) (s : State) (v : Vid) : String :=
  let c := s.vm.get v
  let p := (if pend true c.tmr && c.tmr.en && !c.tmr.act then ["timer PPI pending"] else [])
    ++ (if pend false c.sgi && c.sgi.en && !c.sgi.act then ["SGI pending"] else [])
    ++ (if !v && pend C.spiLevel s.vm.spi && s.vm.spi.en && !s.vm.spi.act then ["SPI pending"] else [])
  let w := (if c.ap then ["AP1R0 bit set, CPU in no handler"] else [])
    ++ (if !c.grp then ["IGRPEN1 off"] else [])
    ++ (if c.tmr.act then ["timer PPI active, CPU not in its handler"] else [])
    ++ (if c.sgi.act then ["SGI active, CPU not in its handler"] else [])
  s!"vCPU {vn v}: " ++ ", ".intercalate w ++ "; " ++ ", ".intercalate p

def main (args : List String) : IO UInt32 := do
  if args.contains "mutants" then
    for R in mutants do
      let mut line := s!"{R.name}:"
      for (name, _, C) in runs do
        let f := explore R C 30000000
        let cs := (checks C).toArray
        let mut found : List String := []
        for ci in [0:cs.size] do
          if f.first[ci]!.isSome then found := found ++ [cs[ci]!.1]
        if !found.isEmpty then line := line ++ s!" [{name}: {", ".intercalate found}]"
      IO.println line
    return 0
  let traces := args.contains "traces"
  let spinsOnly := args.contains "spins"
  let limit := 30000000
  let mut total := 0
  let mut incomplete := false
  for (name, what, C) in runs do
    if !spinsOnly then IO.println s!"\n## {name}: {what}"
    for R in booksFor C do
      let f := explore R C limit
      total := total + f.states.size
      if !f.complete then incomplete := true
      let cs := (checks C).toArray
      if spinsOnly then
        if R.name == "cloud-hypervisor" || R.name == "fork-default" then
          let mut kinds : Std.HashMap String (Nat × Nat) := {}
          for i in [0:f.states.size] do
            let s := f.states[i]!
            if spins C s then
              for v in vids do
                let c := s.vm.get v
                if c.thr == .guest && !c.stopped && c.g == .idle && kvmPending C s.vm v
                    && !deliverable C s.vm v .tmr && !deliverable C s.vm v .sgi && !deliverable C s.vm v .spi then
                  let k := spinKind C s v
                  match kinds[k]? with
                  | some (n, d) => kinds := kinds.insert k (n + 1, d)
                  | none => kinds := kinds.insert k (1, i)
          IO.println s!"\n## {name}, {R.name}: {kinds.size} kinds of spinning vCPU"
          let sorted := kinds.toArray.qsort (fun a b => a.1 < b.1)
          for (k, (n, i)) in sorted do
            IO.println s!"  {n} states, shortest run {(pathTo f i).length} steps: {k}"
            if traces then printRun R C k (pathTo f i)
      else
        let mut line := s!"{R.name}: {f.states.size} states;"
        for ci in [0:cs.size] do
          match f.first[ci]! with
          | some i => line := line ++ s!" {cs[ci]!.1}@{(pathTo f i).length}"
          | none => pure ()
        if !f.complete then line := line ++ " LIMIT REACHED"
        IO.println line
        if traces then
          for ci in [0:cs.size] do
            match f.first[ci]! with
            | some i => printRun R C s!"{name}, {R.name}: {cs[ci]!.1}" (pathTo f i)
            | none => pure ()
  if !spinsOnly then IO.println s!"\n{total} states in all"
  return (if incomplete then 1 else 0)
