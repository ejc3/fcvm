import Std.Data.HashMap
import WakeVerify.Properties
import WakeVerify.Today
import WakeVerify.Design

/-!
# Exhaustive search for lost wake-ups (`lake exe breaks`)

For each rule book and each configuration, a breadth-first search visits every state reachable
from `init` and tests every property of `WakeVerify/Properties.lean` on it. The state space is
finite, so the search ends: it reports either that no reachable state breaks a property, or, for
each kind of break, the shortest run that reaches it, step by step with the source each step
models.

A kind is the property, the wait site, and what happened to the kick (the step that erased it, or
that the snapshot did not hold it). The shortest run of each kind is printed as a Lean term and
copied into `WakeVerify/Breaks.lean`, where the kernel checks it on every build.

The search also audits the properties themselves (`dead`): with interrupts from unmodelled sources
arriving for ever, it computes every reachable state from which some vCPU can never execute another
instruction, and checks that each one is a state the properties flag.
-/

open Wake

/-! ## Labels -/

def allActs : List Act :=
  [.lockIrq, .lockPlain, .call, .idle, .step, .idArm, .faultPv, .faultSynth, .refault, .irq,
   .irqTmr, .hostRun, .apfDone, .tmrFire, .tmrInject, .nmi, .extIrq, .l2Enter, .l2Hlt, .l2Exit,
   .xenPoll, .xenWake, .setMpSame, .offline, .init, .sipi, .sevHold, .snpCreate]

def allLabels : Array Label :=
  ((allActs.map (Label.vcpu .a)) ++ (allActs.map (Label.vcpu .b)) ++ [Label.pause, Label.resume, Label.clone]).toArray

/-! ## Printing -/

def vidName : Vid → String
  | .a => "vCPU0"
  | .b => "vCPU1"

def pcName (p : Pc) : String := (reprStr p).replace "Wake.Pc." ""

def showVcpu (v : Vcpu) : String :=
  let mp := (reprStr v.h.mp).replace "Wake.Mp." ""
  let loc := (reprStr v.loc).replace "Wake.Loc." ""
  let apf := match v.apf with
    | .none => ""
    | .flight k => s!" apf=flight:{(reprStr k).replace "Wake.ApfKind." ""}"
    | .done k => s!" apf=done:{(reprStr k).replace "Wake.ApfKind." ""}"
  s!"{pcName v.pc} IF={if v.ifOn then 1 else 0} {loc} mp={mp} pv_unhalted={if v.h.pv then 1 else 0}"
    ++ (if v.h.apfHalted then " apf.halted" else "") ++ apf
    ++ (if v.ipi then " ipi" else "") ++ (if v.csd then " csd" else "")
    ++ (if v.mcs then " mcs" else "") ++ (if v.hashed then " hashed" else "")
    ++ (if v.tmr != .off then s!" tmr={(reprStr v.tmr).replace "Wake.Tmr." ""}" else "")

def showState (s : State) : String :=
  s!"lock={(reprStr s.lock).replace "Wake.Lock." ""}"
    ++ (if s.paused then " paused" else "") ++ (if s.cloned then " (restored VM)" else "")
    ++ s!"\n        vCPU0: {showVcpu s.a}\n        vCPU1: {showVcpu s.b}"

/-- Per release, where each kernel rule is. -/
def kcite (k : String) (site : String) : String :=
  let old := k == "v6.14"
  match site with
  | "halt" => if old then "__kvm_emulate_halt, v6.14 x86.c:11290-11310"
              else "__kvm_emulate_halt, v7.3.3 x86.c:8693-8712"
  | "unhalt" => if old then "vcpu_block, v6.14 x86.c:11169-11237"
                else "vcpu_block, v7.3.3 x86.c:8575-8640"
  | "present" =>
    if old then "kvm_arch_async_page_present, v6.14 x86.c:13455-13478"
    else if k == "v7.3.3" then "kvm_arch_async_page_present, v7.3.3 x86.c:10582-10605"
    else "kvm_arch_async_page_present, v7.3.3 x86.c:10582-10605 with the #2130 patch"
  | "kick" => if old then "APIC_DM_REMRD, v6.14 lapic.c:1351-1356"
              else "APIC_DM_REMRD, v7.3.3 lapic.c:1451-1456"
  | "enter" => if old then "vcpu_enter_guest, v6.14 x86.c:10849-10850"
               else "vcpu_enter_guest, v7.3.3 x86.c:8216-8217"
  | _ => site

/-- What a label does in the state it is taken from, in words, with the source it models. -/
def describe (R : Rules) (l : Label) (s : State) : String :=
  let k := R.kernel.name
  match l with
  | .pause => "the VMM pauses the VM: every vCPU thread leaves KVM_RUN (Firecracker vstate/vcpu.rs:271-281)"
  | .resume => "the VMM resumes the VM (Firecracker lib.rs:463-472)"
  | .clone =>
    "snapshot and restore into a new VM: KVM_GET_MP_STATE first (Firecracker arch/x86_64/vcpu.rs:591-650), "
      ++ s!"then on new vCPUs {reprStr R.restore} (vcpu.rs:684-750, builder.rs:475-494)"
  | .vcpu i act =>
    let v := s.get i
    let who := vidName i
    let d := match act with
      | .lockIrq => "spin_lock_irqsave: interrupts off, lock slow path"
      | .lockPlain => "spin_lock: lock slow path"
      | .call => "starts a cross-CPU call (smp.c:780)"
      | .idle => "enters the idle loop (idle.c:276)"
      | .idArm => "idle loop programs the timer and goes to halt (idle.c:337-350)"
      | .faultPv => "host page fault in user mode: 'page not present' injected, the guest runs another task (v7.3.3 x86.c:10550-10567)"
      | .faultSynth => "host page fault KVM cannot inject: KVM_REQ_APF_HALT, synthetic halt (v7.3.3 x86.c:10568-10579, 8147-8152)"
      | .refault => "the faulting instruction runs again, page still in flight: synthetic halt again (v7.3.3 mmu/mmu.c:4672-4675)"
      | .irq => "takes the call-function interrupt and runs the queued call (smp.c:461-464)"
      | .irqTmr => "takes the timer interrupt"
      | .apfDone => "the async page fault worker finishes: done list, KVM_REQ_APF_READY, wake (virt/kvm/async_pf.c:45-97)"
      | .tmrFire => "the APIC timer expires (v7.3.3 lapic.c:2053-2089)"
      | .tmrInject => "kvm_inject_pending_timer_irqs (v7.3.3 lapic.c:3203-3211)"
      | .nmi => s!"an NMI wakes the blocked vCPU ({kcite k "unhalt"})"
      | .extIrq => s!"an interrupt from an unmodelled source wakes the blocked vCPU ({kcite k "unhalt"})"
      | .l2Enter => "enters a nested guest"
      | .l2Hlt => s!"the nested guest executes HLT ({kcite k "halt"})"
      | .l2Exit => "nested VM-exit: mp_state made runnable (vmx/nested.c)"
      | .xenPoll => "Xen SCHEDOP_poll: mp_state halted, blocks (xen.c)"
      | .xenWake => "Xen SCHEDOP_poll returns: mp_state runnable (xen.c)"
      | .setMpSame => "userspace writes back the mp_state it read: KVM_GET_MP_STATE, KVM_SET_MP_STATE"
      | .offline => "goes offline"
      | .init => "INIT accepted: vCPU reset, wait for SIPI (lapic.c kvm_apic_accept_events)"
      | .sipi => "SIPI accepted: runnable (lapic.c kvm_apic_accept_events)"
      | .sevHold => s!"AP_RESET_HOLD ({kcite k "halt"})"
      | .snpCreate => "SEV-SNP AP creation (svm/sev.c)"
      | .hostRun =>
        if v.running then
          if v.apf.isDone then
            s!"enters the guest; first kvm_check_async_pf_completion ({kcite k "enter"}): {kcite k "present"}"
          else "enters the guest (vcpu_run)"
        else s!"leaves vcpu_block: runnable again ({kcite k "unhalt"})"
      | .step =>
        match v.pc with
        | .lkEnter => "pv_hybrid_queued_unfair_trylock / xchg_tail: steal the lock or join the queue (pvq.h:81-99, qspinlock.c:260-290)"
        | .hdTry | .hdTry2 => "queue head: trylock_clear_pending (pvq.h:429-435)"
        | .hdHash => "queue head: xchg(&lock->locked, _Q_SLOW_VAL) (pvq.h:452)"
        | .hdChk => "kvm_wait: interrupts off, READ_ONCE(*ptr) == val on the lock byte (kvm.c:1102-1110, pvq.h:466)"
        | .hdHlt => s!"kvm_wait: HLT (kvm.c:1104/1110); {kcite k "halt"}"
        | .hdWoke => "kvm_wait returns: back to the top of the loop (pvq.h:417)"
        | .ndSpin => "queue node: READ_ONCE(node->locked) (pvq.h:299)"
        | .ndSet => "queue node: smp_store_mb(pn->state, VCPU_HALTED) (pvq.h:317)"
        | .ndRe => "queue node: re-read node->locked (pvq.h:319)"
        | .ndChk => "kvm_wait: interrupts off, READ_ONCE(*ptr) == val on pn->state (kvm.c:1102-1110, pvq.h:322)"
        | .ndHlt => s!"kvm_wait: HLT (kvm.c:1104/1110); {kcite k "halt"}"
        | .ndWoke => "kvm_wait returns"
        | .ndCas => "queue node: cmpxchg(&pn->state, VCPU_HALTED, VCPU_RUNNING) (pvq.h:330)"
        | .hoSet => "new owner: arch_mcs_spin_unlock_contended(&next->locked) (qspinlock.c:370)"
        | .hoKick => "new owner: pv_kick_node (qspinlock.c:371, pvq.h:353-389)"
        | .crit => "leaves the critical section"
        | .ulTry => "__pv_queued_spin_unlock: try_cmpxchg_release(&lock->locked, &locked, 0) (pvq.h:552)"
        | .ulRel => "unlock slow path: pv_unhash, smp_store_release(&lock->locked, 0) (pvq.h:523-529)"
        | .ulKick => s!"unlock slow path: pv_kick -> KVM_HC_KICK_CPU (pvq.h:539, kvm.c:1081-1088); {kcite k "kick"}"
        | .ccSend => "queues the call and sends the IPI (smp.c:829-859)"
        | .ccWait => "csd_lock_wait returns (smp.c:877)"
        | .idChk => "idle loop: interrupts off, nothing to run (idle.c:298-350)"
        | .idHlt | .idHltT => s!"idle loop: safe_halt (process.c:765-768); {kcite k "halt"}"
        | .idWoke | .idWokeT => "leaves the idle loop"
        | .off => s!"offline CPU: HLT; {kcite k "halt"}"
        | _ => "step"
    s!"{who}: {d}"

def labelTerm (l : Label) : String :=
  match l with
  | .vcpu i act =>
    s!".vcpu .{(reprStr i).replace "Wake.Vid." ""} .{(reprStr act).replace "Wake.Act." ""}"
  | .pause => ".pause"
  | .resume => ".resume"
  | .clone => ".clone"

/-! ## The search -/

structure Graph where
  states : Array State
  /-- for each state but the first: the state it was first reached from, and by which label -/
  parent : Array (Nat × Nat)
  /-- successors of each state: (label index, state index) -/
  succ : Array (Array (Nat × Nat))
  complete : Bool

/-- Breadth-first search from `init`, to exhaustion or `limit` states. -/
partial def explore (R : Rules) (C : Cfg) (limit : Nat) (keepEdges : Bool) : Graph := Id.run do
  let mut states : Array State := #[init]
  let mut parent : Array (Nat × Nat) := #[(0, 0)]
  let mut succ : Array (Array (Nat × Nat)) := #[]
  let mut index : Std.HashMap State Nat := (Std.HashMap.emptyWithCapacity 1024).insert init 0
  let mut next := 0
  let mut complete := true
  while next < states.size do
    let s := states[next]!
    let mut out : Array (Nat × Nat) := #[]
    for li in [0:allLabels.size] do
      match step R C allLabels[li]! s with
      | none => pure ()
      | some t =>
        match index[t]? with
        | some j => if keepEdges then out := out.push (li, j)
        | none =>
          let j := states.size
          states := states.push t
          parent := parent.push (next, li)
          index := index.insert t j
          if keepEdges then out := out.push (li, j)
    if keepEdges then succ := succ.push out
    next := next + 1
    if states.size > limit then
      complete := false
      break
  return { states, parent, succ, complete }

/-- The labels of the shortest run from `init` to state `i`. -/
def Graph.trace (g : Graph) (i : Nat) : List Label := Id.run do
  let mut out : List Label := []
  let mut j := i
  let mut fuel := g.states.size
  while j != 0 && fuel > 0 do
    let (p, li) := g.parent[j]!
    out := allLabels[li]! :: out
    j := p
    fuel := fuel - 1
  return out

/-! ## Properties and kinds -/

structure Check where
  name : String
  /-- which vCPU breaks it in this state, if any -/
  bad : State → Option Vid

def firstVid (f : Vid → Bool) : Option Vid :=
  if f .a then some .a else if f .b then some .b else none

def checks : List Check :=
  [ ⟨"NoLostKick (hang: interrupts off)", fun s => firstVid s.lostKickHard⟩,
    ⟨"NoLostKick (interrupts on)", fun s => firstVid s.lostKickSoft⟩,
    ⟨"NoStuckCall", fun s => firstVid s.stuckCall⟩,
    ⟨"NoStuckCall (interrupt lost)", fun s => firstVid s.lostIpi⟩,
    ⟨"MutualExclusion", fun s => if s.a.pc.holds && s.b.pc.holds then some .a else none⟩,
    ⟨"NoOrphanHalt", fun s => firstVid s.orphanHalt⟩,
    ⟨"NoLostTimer", fun s => firstVid s.lostTimer⟩,
    ⟨"NoStrandedNode", fun s => firstVid s.strandedNode⟩ ]

/-- What happened to the kick of vCPU `i` along a run: the last step that cleared a recorded kick,
or that the run went through a snapshot, or neither. -/
def kickFate (R : Rules) (C : Cfg) (i : Vid) (tr : List Label) : String := Id.run do
  let mut s := init
  let mut fate := "the kick was never recorded"
  for l in tr do
    match step R C l s with
    | none => return "?"
    | some t =>
      if (s.get i).h.pv && !(t.get i).h.pv then
        fate := match l with
          | .clone => "the snapshot does not hold the kick"
          | .vcpu j .hostRun =>
            if j == i && (s.get i).running && (s.get i).apf.isDone then
              let how := match (s.get i).apf with
                | .done .all => "the restore's wake-everyone completion"
                | .done .pv => "a page-ready completion"
                | _ => "the completion of a synthetic halt"
              s!"{how} cleared the kick ({R.kernel.name} kvm_arch_async_page_present)"
            else "leaving vcpu_block cleared the kick"
          | .vcpu _ .step => "a HLT consumed the kick"
          | .vcpu _ act => s!"{(reprStr act).replace "Wake.Act." ""} cleared the kick"
          | _ => "?"
      s := t
  return fate

def waitSite (s : State) (i : Vid) : String :=
  match (s.get i).pc with
  | .hdWoke => "queue head"
  | .ndWoke => "queue node"
  | .ccWait => "cross-CPU call"
  | .idWokeT => "idle"
  | p => pcName p

structure Found where
  check : String
  kind : String
  count : Nat
  first : Nat
  who : Vid

/-- Every kind of break in the graph, with how many states have it and the first (so, by
breadth-first order, a nearest) one. -/
def findBreaks (R : Rules) (C : Cfg) (g : Graph) : Array Found := Id.run do
  let mut found : Array Found := #[]
  for c in checks do
    for idx in [0:g.states.size] do
      let s := g.states[idx]!
      match c.bad s with
      | none => pure ()
      | some i =>
        let victim := if c.name == "NoStuckCall" then i.other else i
        let kind :=
          if c.name.startsWith "NoLostKick" || c.name == "NoStuckCall" then
            s!"{waitSite s victim}; {kickFate R C victim (g.trace idx)}"
          else if c.name == "NoLostTimer" then waitSite s i
          else "any" 
        match found.findIdx? (fun f => f.check == c.name && f.kind == kind) with
        | some k => found := found.modify k (fun f => { f with count := f.count + 1 })
        | none => found := found.push { check := c.name, kind, count := 1, first := idx, who := i }
  return found

/-! ## The audit: vCPUs that can never run again -/

/-- For vCPU `i`: the states from which no run changes its program counter. Computed backwards
over the stored edges, ignoring NMIs. -/
def deadStates (g : Graph) (i : Vid) : Array Bool := Id.run do
  let n := g.states.size
  -- reverse edges
  let mut pred : Array (Array Nat) := Array.replicate n #[]
  let mut alive : Array Bool := Array.replicate n false
  let mut work : Array Nat := #[]
  for u in [0:n] do
    for (li, v) in g.succ[u]! do
      let isNmi := match allLabels[li]! with
        | .vcpu _ .nmi => true
        | _ => false
      if !isNmi then
        pred := pred.modify v (·.push u)
        if (g.states[u]!.get i).pc != (g.states[v]!.get i).pc && !alive[u]! then
          alive := alive.set! u true
          work := work.push u
  let mut k := 0
  while k < work.size do
    let v := work[k]!
    for u in pred[v]! do
      if !alive[u]! then
        alive := alive.set! u true
        work := work.push u
    k := k + 1
  return alive.map (!·)

def flagged (s : State) : Bool :=
  checks.any (fun c => (c.bad s).isSome)

/-- The states from which a state some property flags can be reached (NMIs aside). -/
def reachesFlagged (g : Graph) : Array Bool := Id.run do
  let n := g.states.size
  let mut pred : Array (Array Nat) := Array.replicate n #[]
  let mut mark : Array Bool := Array.replicate n false
  let mut work : Array Nat := #[]
  for u in [0:n] do
    if flagged g.states[u]! then
      mark := mark.set! u true
      work := work.push u
    for (li, v) in g.succ[u]! do
      let isNmi := match allLabels[li]! with
        | .vcpu _ .nmi => true
        | _ => false
      if !isNmi then pred := pred.modify v (·.push u)
  let mut k := 0
  while k < work.size do
    let v := work[k]!
    for u in pred[v]! do
      if !mark[u]! then
        mark := mark.set! u true
        work := work.push u
    k := k + 1
  return mark

/-! ## Reports -/

def printTrace (R : Rules) (C : Cfg) (tr : List Label) : IO Unit := do
  let mut s := init
  let mut n := 1
  IO.println s!"     0. start: {showState s}"
  for l in tr do
    match step R C l s with
    | none => IO.println s!"    {n}. NOT ENABLED: {labelTerm l}"; return
    | some t =>
      IO.println s!"    {if n < 10 then " " else ""}{n}. {describe R l s}\n        {showState t}"
      s := t
      n := n + 1

structure Run where
  name : String
  rules : Rules
  cfg : Cfg
  cfgName : String

def report (r : Run) (limit : Nat) (audit : Bool) (verbose : Bool) : IO Bool := do
  let t0 ← IO.monoMsNow
  let g := explore r.rules r.cfg limit audit
  let found := findBreaks r.rules r.cfg g
  let t1 ← IO.monoMsNow
  IO.println s!"\n## {r.name}, {r.cfgName}"
  IO.println s!"{g.states.size} reachable states{if g.complete then "" else " (LIMIT REACHED: NOT EXHAUSTIVE)"}, {t1 - t0} ms"
  if found.isEmpty then
    IO.println "no reachable state breaks a property"
  for f in found do
    let tr := g.trace f.first
    IO.println s!"\n### {f.check}: {f.kind}"
    IO.println s!"{f.count} states; shortest run {tr.length} steps, {vidName f.who}"
    if verbose then
      printTrace r.rules r.cfg tr
      IO.println s!"    Lean: [{", ".intercalate (tr.map labelTerm)}]"
  if audit then
    let mut unexplained := 0
    let mut deadCount := 0
    let mut witness : Option Nat := none
    let da := deadStates g .a
    let db := deadStates g .b
    let rf := reachesFlagged g
    for idx in [0:g.states.size] do
      let s := g.states[idx]!
      let d := (da[idx]! && s.a.pc != .off) || (db[idx]! && s.b.pc != .off)
      if d then
        deadCount := deadCount + 1
        if !rf[idx]! then
          unexplained := unexplained + 1
          if witness.isNone then witness := some idx
    IO.println s!"audit: in {deadCount} states some vCPU can never run again; {unexplained} of them are not on the way to a state a property flags"
    match witness with
    | some idx =>
      IO.println "  first such state:"
      printTrace r.rules r.cfg (g.trace idx)
    | none => pure ()
  (← IO.getStdout).flush
  return found.isEmpty && g.complete

def fcvm : Cfg := Cfg.fcvm

def books : List (String × Rules) :=
  [ ("Today: v6.14, Firecracker as pinned", Today.v614),
    ("Today: v7.3.3, Firecracker as pinned", Today.v733),
    ("v6.14 with the #2131 restore kick", Partial.v614Kick),
    ("v7.3.3 with the #2131 restore kick", Partial.v733Kick),
    ("v7.3.3 with the #2130 patch, no restore kick", Partial.v733Patched),
    ("Design: v7.3.3 with the #2130 patch and the #2131 restore kick", Design.rules) ]

def cfgs : List (String × Cfg) :=
  [ ("fcvm configuration", fcvm),
    ("fcvm configuration, no snapshot", { fcvm with snapshot := false }),
    ("fcvm configuration, guest booted with no-kvmapf", { fcvm with pvApf := false }),
    ("fcvm configuration, unmodelled interrupts arrive (audit)", { fcvm with rescue := true }),
    ("with NMIs and unmodelled interrupts", { fcvm with rescue := true, nmi := true }),
    ("with the idle timer", { fcvm with timer := true, plainLocks := false }),
    ("with a nested guest", { fcvm with nested := true }),
    ("with Xen SCHEDOP_poll", { fcvm with xen := true }),
    ("with CPU hotplug (INIT, SIPI, vCPU reset)", { fcvm with hotplug := true }),
    ("with SEV AP reset hold and SNP AP creation", { fcvm with hotplug := true, sev := true }),
    ("outside the design: userspace writes mp_state back on a paused VM",
      { fcvm with userSetMp := true }) ]

/-- Restore orders that no VMM here uses, to see which orders matter. -/
def orders : List (String × Rules × Cfg) :=
  [ ("Order: v7.3.3 with the #2130 patch, the kick BEFORE KVM_SET_MP_STATE", Order.kickFirst, fcvm),
    ("Order: v6.14, the kick BEFORE KVM_SET_MP_STATE", Order.kickFirst614, fcvm),
    ("Order: v7.3.3 with the #2130 patch, KVM_SET_MSRS BEFORE KVM_SET_LAPIC, with the idle timer",
      Order.msrsFirst, { fcvm with timer := true, plainLocks := false, faults := false }),
    ("Order: v7.3.3 with the #2130 patch, no KVM_SET_LAPIC", Order.noLapic, fcvm) ]

def main (args : List String) : IO UInt32 := do
  let limit := (args.head? >>= String.toNat?).getD 20000000
  let only := args.drop 1
  let verbose := !only.contains "--quiet"
  let want (name : String) : Bool :=
    let pats := only.filter (fun p => !p.startsWith "--")
    pats.isEmpty || pats.any (fun p => (name.splitOn p).length > 1)
  let mut designClean := true
  IO.println "# Lost wake-up search"
  for (bn, rules) in books do
    for (cn, cfg) in cfgs do
      if want s!"{bn}, {cn}" then
        let audit := cfg.rescue && !cfg.nmi
        let ok ← report { name := bn, rules, cfg, cfgName := cn } limit audit verbose
        if bn.startsWith "Design" && !cfg.userSetMp && !ok then designClean := false
  for (bn, rules, cfg) in orders do
    if want bn then
      let _ ← report { name := bn, rules, cfg, cfgName := "fcvm configuration" } limit false verbose
  if !designClean then
    IO.println "\nFAILED: the design breaks a property, or its search did not finish"
    return 1
  return 0
