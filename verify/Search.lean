import Std.Data.HashMap
import WakeVerify.Properties
import WakeVerify.Today
import WakeVerify.Design

/-!
# Exhaustive search for lost wake-ups (`lake exe breaks`)

For each rule book and each configuration, a breadth-first search visits every state reachable
from `init` and tests every property of `WakeVerify/Properties.lean` on it. The state space is
finite, so the search ends: it reports either that no reachable state breaks a property, or, for
each kind of break, a shortest run that reaches it, step by step with the source each step
models. A kind is the property, the wait site, and what happened to the kick (the step that erased
it, or that the snapshot did not hold it). That a run is a shortest one is this program's output,
not a theorem.

Every search also audits the properties (`audit`). Without using them, it computes the states in
which some vCPU can never execute another instruction, however the other vCPU, the host and the
VMM go on (short of a snapshot restore or an NMI, which the audit does not accept as a rescue).
Each such vCPU must be flagged by a property in that very state, and the designed system must
have none. A dead vCPU that no property flags fails the run: the exit status is 1. So does a
search that stops at the state limit, under any rule book: it is not audited, and it establishes
nothing. An argument the program does not understand, or a pattern that matches no search, ends it
with exit status 2 before anything is searched. `test-search.sh` tests each of these.

A panic in this program (an array index out of bounds) prints a message and by default goes on
with a default value, so the exit status alone would not show it. Run it with
`LEAN_ABORT_ON_PANIC=1`, as `make verify-search`, `search-expected.sh` and `test-search.sh` do:
a panic then aborts the process.

`--summary` prints what `WakeVerify/search.expected` holds, and `search-expected.sh` compares the
two: the states of every search, each kind of break with its count and the length of its shortest
run, and the audit's numbers.

Every configuration lets interrupts from sources the model does not follow (devices, the tick)
arrive at a blocked vCPU that can take them, so that a vCPU asleep with interrupts on is not
counted dead.
-/

open Wake

/-! ## Labels -/

/-- Every label, from the library (`allLabels_complete` in `Proofs.lean`). -/
def labels : Array Label := allLabels.toArray

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
    ++ (if v.slot then " slot" else "") ++ (if v.apfIrq then " pr-irq" else "")
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
      ++ s!"then on new vCPUs {reprStr R.restore} (vcpu.rs:684-750; the kick, where the rule book has one, arch/x86_64/vm.rs:299-326 at 2a33289b55b7)"
  | .vcpu i act =>
    let v := s.get i
    let who := vidName i
    let d := match act with
      | .lockIrq => "spin_lock_irqsave: interrupts off, lock slow path"
      | .lockPlain => "spin_lock: lock slow path"
      | .call => "starts a cross-CPU call (smp.c:780)"
      | .idle => "enters the idle loop (idle.c:276)"
      | .idArm => "idle loop programs the timer and goes to halt (idle.c:226)"
      | .faultPv => "host page fault in user mode: 'page not present' injected, the guest runs another task (v7.3.3 x86.c:10550-10567)"
      | .faultSynth => "host page fault KVM cannot inject (kernel mode, or no interface): KVM_REQ_APF_HALT, synthetic halt (v7.3.3 x86.c:10568-10579, 8147-8152)"
      | .refault => "the faulting instruction runs again, page still in flight: synthetic halt again (v7.3.3 mmu/mmu.c:4672-4675)"
      | .irq => "takes the call-function interrupt and runs the queued call (smp.c:461-464)"
      | .irqTmr => "takes the timer interrupt"
      | .irqApf => "takes the page-ready interrupt: clears the slot, acknowledges (kvm.c:317-334)"
      | .apfDone => "the async page fault worker finishes: done list, KVM_REQ_APF_READY, wake (virt/kvm/async_pf.c:45-97)"
      | .tmrFire => "the APIC timer expires (v7.3.3 lapic.c:2053-2089)"
      | .tmrInject => "kvm_inject_pending_timer_irqs (v7.3.3 lapic.c:3203-3211)"
      | .nmi => s!"an NMI ends the blocked vCPU's wait ({kcite k "unhalt"})"
      | .extIrq => s!"an interrupt from a device or the tick ends the blocked vCPU's wait ({kcite k "unhalt"})"
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
            if v.slot then "enters the guest; the completion stays on the done list, the page-ready slot is occupied (v7.3.3 x86.c:10618-10624)"
            else
              let irq := match v.apf with
                | .done .synth => ""
                | _ => "; token written, page-ready interrupt raised (x86.c:10596-10601)"
              s!"enters the guest; first kvm_check_async_pf_completion ({kcite k "enter"}): {kcite k "present"}{irq}"
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
        | .hoKick => "new owner: pv_kick_node (qspinlock.c:371, pvq.h:357-389)"
        | .crit => "leaves the critical section"
        | .ulTry => "__pv_queued_spin_unlock: try_cmpxchg_release(&lock->locked, &locked, 0) (pvq.h:552)"
        | .ulRel => "unlock slow path: pv_unhash, smp_store_release(&lock->locked, 0) (pvq.h:523-529)"
        | .ulKick => s!"unlock slow path: pv_kick -> KVM_HC_KICK_CPU (pvq.h:539, kvm.c:1081-1088); {kcite k "kick"}"
        | .ccSend => "queues the call and sends the IPI (smp.c:829-859)"
        | .ccWait => "csd_lock_wait returns (smp.c:877)"
        | .idChk => "idle loop: interrupts off, nothing to run (idle.c:298-330, 116)"
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
  /-- successors of each state, one number an edge: `target * 64 + label`. There are 61 labels;
  `allLabels_fit_edge_encoding` in `Proofs.lean` fails the build past 64, and `main` refuses to
  run. -/
  succ : Array (Array Nat)
  complete : Bool

def edgeLabel (e : Nat) : Label := labels[e % 64]!
def edgeTarget (e : Nat) : Nat := e / 64

/-- Breadth-first search from `init`, to exhaustion or `limit` states. -/
partial def explore (R : Rules) (C : Cfg) (limit : Nat) : Graph := Id.run do
  let mut states : Array State := #[init]
  let mut parent : Array (Nat × Nat) := #[(0, 0)]
  let mut succ : Array (Array Nat) := #[]
  let mut index : Std.HashMap State Nat := (Std.HashMap.emptyWithCapacity 1024).insert init 0
  let mut next := 0
  let mut complete := true
  while next < states.size do
    let s := states[next]!
    let mut out : Array Nat := #[]
    for li in [0:labels.size] do
      match step R C labels[li]! s with
      | none => pure ()
      | some t =>
        match index[t]? with
        | some j => out := out.push (j * 64 + li)
        | none =>
          let j := states.size
          states := states.push t
          parent := parent.push (next, li)
          index := index.insert t j
          out := out.push (j * 64 + li)
    succ := succ.push out
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
    out := labels[li]! :: out
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
    ⟨"HasWaker", fun s => firstVid s.orphan⟩,
    ⟨"NoMutualSleep", fun s =>
      if s.sleeps .a && s.sleeps .b && !s.a.h.pv && !s.b.h.pv then some .a else none⟩,
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
            if j == i && (s.get i).running && (s.get i).apf.isDone && !(s.get i).slot then
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

/-- An edge the audit follows: every step but a snapshot restore (a new VM is not a rescue of the
old one) and an NMI (the hang is what an NMI ends). -/
def auditEdge (l : Label) : Bool :=
  match l with
  | .clone => false
  | .vcpu _ .nmi => false
  | _ => true

/-- For vCPU `i`: the states from which no run of audit edges ever has it execute an instruction
(its position changing in a step of a vCPU; the VMM's pause does not count). Computed backwards
from the steps in which it does. The properties are not used. -/
def deadStates (g : Graph) (i : Vid) : Array Bool := Id.run do
  let n := g.states.size
  let mut pred : Array (Array Nat) := Array.replicate n #[]
  let mut alive : Array Bool := Array.replicate n false
  let mut work : Array Nat := #[]
  for u in [0:n] do
    for e in g.succ[u]! do
      let l := edgeLabel e
      let v := edgeTarget e
      if auditEdge l then
        pred := pred.modify v (·.push u)
        let moved := match l with
          | .vcpu _ _ => (g.states[u]!.get i).pc != (g.states[v]!.get i).pc
          | _ => false
        if moved && !alive[u]! then
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

/-- vCPU `i` itself is flagged in this state: by a property about its own sleep, or, when it waits
in a cross-CPU call, by the property about that call. -/
def ownFlag (s : State) (i : Vid) : Bool :=
  s.lostKick i || s.orphan i || s.orphanHalt i || s.lostTimer i || s.strandedNode i
    || s.stuckCall i || ((s.get i).pc == .ccWait && s.lostIpi i.other)

/-- For vCPU `i`: the states from which a state where `i` itself is flagged can be reached over
audit edges (that state included). -/
def flagAhead (g : Graph) (i : Vid) : Array Bool := Id.run do
  let n := g.states.size
  let mut pred : Array (Array Nat) := Array.replicate n #[]
  let mut mark : Array Bool := Array.replicate n false
  let mut work : Array Nat := #[]
  for u in [0:n] do
    if ownFlag g.states[u]! i then
      mark := mark.set! u true
      work := work.push u
    for e in g.succ[u]! do
      if auditEdge (edgeLabel e) then pred := pred.modify (edgeTarget e) (·.push u)
  let mut k := 0
  while k < work.size do
    let v := work[k]!
    for u in pred[v]! do
      if !mark[u]! then
        mark := mark.set! u true
        work := work.push u
    k := k + 1
  return mark

structure Audit where
  /-- states in which some vCPU is dead -/
  dead : Nat
  /-- dead vCPUs flagged by a property of their own in that state -/
  flaggedNow : Nat
  /-- dead vCPUs not flagged yet, whose own flag is ahead: the vCPU they wait for has been
  doomed but has not executed its HLT yet -/
  flaggedAhead : Nat
  /-- dead vCPUs with no flag of their own, now or ahead -/
  unexplained : Nat
  witness : Option (Nat × Vid)

def audit (g : Graph) : Audit := Id.run do
  let da := deadStates g .a
  let db := deadStates g .b
  let mut dead := 0
  let mut flaggedNow := 0
  let mut later : Array (Nat × Vid) := #[]
  for idx in [0:g.states.size] do
    let s := g.states[idx]!
    if da[idx]! || db[idx]! then dead := dead + 1
    for (d, i) in [(da[idx]!, Vid.a), (db[idx]!, Vid.b)] do
      if d then
        if ownFlag s i then flaggedNow := flaggedNow + 1 else later := later.push (idx, i)
  if later.isEmpty then
    return { dead, flaggedNow, flaggedAhead := 0, unexplained := 0, witness := none }
  let fa := flagAhead g .a
  let fb := flagAhead g .b
  let mut flaggedAhead := 0
  let mut unexplained := 0
  let mut witness : Option (Nat × Vid) := none
  for (idx, i) in later do
    let ahead := match i with
      | .a => fa[idx]!
      | .b => fb[idx]!
    if ahead then flaggedAhead := flaggedAhead + 1
    else
      unexplained := unexplained + 1
      if witness.isNone then witness := some (idx, i)
  return { dead, flaggedNow, flaggedAhead, unexplained, witness }

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
  /-- one of the small searches that `--quick` selects (`search-expected.sh quick`) -/
  quick : Bool := false

def Run.title (r : Run) : String := s!"{r.name}, {r.cfgName}"

/-- How much a search prints. `summary` is the form `WakeVerify/search.expected` holds: no
timings and no runs, one line a kind, no blank line inside a search's block. -/
inductive Mode where
  | verbose | quiet | summary
  deriving DecidableEq

/-- Search one rule book in one configuration and audit it. Returns whether the search visited
every reachable state, whether no property broke, how many states have a dead vCPU, and how many
dead vCPUs no property flags. A search that stopped at the state limit is not audited: its graph
has successors for the expanded states only. -/
def report (r : Run) (limit : Nat) (mode : Mode) : IO (Bool × Bool × Nat × Nat) := do
  let t0 ← IO.monoMsNow
  let g := explore r.rules r.cfg limit
  let found := findBreaks r.rules r.cfg g
  let t1 ← IO.monoMsNow
  let summary := mode == .summary
  IO.println s!"{if summary then "" else "\n"}## {r.title}"
  if summary then
    IO.println s!"set: {if r.quick then "quick" else "full only"}"
  let size := if g.complete then s!"{g.states.size} reachable states"
    else s!"{g.states.size} states visited (LIMIT REACHED: NOT EXHAUSTIVE)"
  IO.println (if summary then size else s!"{size}, {t1 - t0} ms")
  if found.isEmpty then
    IO.println (if g.complete then "no reachable state breaks a property"
      else "no property breaks in the states visited")
  for f in found do
    let tr := g.trace f.first
    if summary then
      IO.println s!"kind: {f.check}: {f.kind}; {f.count} states; shortest run {tr.length} steps"
    else
      IO.println s!"\n### {f.check}: {f.kind}"
      IO.println s!"{f.count} states; shortest run {tr.length} steps, {vidName f.who}"
      if mode == .verbose then
        printTrace r.rules r.cfg tr
      IO.println s!"    Lean: [{", ".intercalate (tr.map labelTerm)}]"
  if !g.complete then
    IO.println "audit: not run, the search did not finish"
    if summary then IO.println ""
    (← IO.getStdout).flush
    return (false, found.isEmpty, 0, 0)
  let a := audit g
  IO.println s!"audit: {a.dead} states in which a vCPU can never run again; of those vCPUs, {a.flaggedNow} are flagged by a property of their own there, {a.flaggedAhead} have their own flag ahead, {a.unexplained} have neither"
  match a.witness with
  | some (idx, i) =>
    IO.println s!"  first dead vCPU with no flag of its own, now or ahead: {vidName i} after"
    if !summary then printTrace r.rules r.cfg (g.trace idx)
  | none => pure ()
  if summary then IO.println ""
  (← IO.getStdout).flush
  return (true, found.isEmpty, a.dead, a.unexplained)

/-- The fcvm configuration, with host page faults in ordinary code only (the searches' bound). -/
def fcvm : Cfg := { Cfg.fcvm with kernelFaults := false }

def books : List (String × Rules) :=
  [ ("Today: v6.14, Firecracker without the restore kick", Today.v614),
    ("Today: v7.3.3, Firecracker without the restore kick", Today.v733),
    ("v6.14 with the #2131 restore kick", Partial.v614Kick),
    ("v7.3.3 with the #2131 restore kick", Partial.v733Kick),
    ("v7.3.3 with the #2130 patch, no restore kick", Partial.v733Patched),
    ("Design: v7.3.3 with the #2130 patch and the #2131 restore kick", Design.rules) ]

/-- The configurations. The Bool marks the small ones (under 3.2 million states each), which
`--quick` selects: between them they have every letter of the table in BREAKS.md but M. -/
def cfgs : List (String × Bool × Cfg) :=
  [ ("fcvm configuration", false, fcvm),
    ("no snapshot taken", true, { fcvm with snapshot := false }),
    ("guest booted with no-kvmapf", true, { fcvm with pvApf := false }),
    ("with host page faults in the lock, call and idle paths", false,
      { fcvm with kernelFaults := true }),
    ("with NMIs", false, { fcvm with nmi := true }),
    ("with the idle timer, no host page faults", true,
      { fcvm with timer := true, plainLocks := false, faults := false }),
    ("with the idle timer and host page faults, no snapshot taken", false,
      { fcvm with timer := true, plainLocks := false, snapshot := false }),
    ("with a nested guest", false, { fcvm with nested := true }),
    ("with Xen SCHEDOP_poll", false, { fcvm with xen := true }),
    ("with CPU hotplug (INIT, SIPI, vCPU reset)", false, { fcvm with hotplug := true }),
    ("with SEV AP reset hold and SNP AP creation", false,
      { fcvm with hotplug := true, sev := true }),
    ("outside the design: userspace writes mp_state back on a paused VM", false,
      { fcvm with userSetMp := true }) ]

/-- Restore orders that no VMM here uses, to see which orders matter. -/
def orders : List Run :=
  [ { name := "Order: v7.3.3 with the #2130 patch, the kick BEFORE KVM_SET_MP_STATE",
      rules := Order.kickFirst, cfgName := "fcvm configuration", cfg := fcvm },
    { name := "Order: v6.14, the kick BEFORE KVM_SET_MP_STATE",
      rules := Order.kickFirst614, cfgName := "fcvm configuration", cfg := fcvm },
    { name := "Order: v7.3.3 with the #2130 patch, KVM_SET_MSRS BEFORE KVM_SET_LAPIC",
      rules := Order.msrsFirst,
      cfgName := "idle timer, guest booted with no-kvmapf, no host page faults",
      cfg := { fcvm with timer := true, plainLocks := false, faults := false, pvApf := false },
      quick := true },
    { name := "Order: v7.3.3 with the #2130 patch, no KVM_SET_LAPIC",
      rules := Order.noLapic, cfgName := "fcvm configuration", cfg := fcvm } ]

/-- Every search, in the order they are run and printed: each rule book in each configuration,
then the restore orders. -/
def searches : List Run :=
  (books.flatMap fun (bn, rules) =>
    cfgs.map fun (cn, quick, cfg) => { name := bn, rules, cfg, cfgName := cn, quick }) ++ orders

/-- The designed system in a configuration its theorems cover: no property may break and no vCPU
may be dead. -/
def Run.isDesign (r : Run) : Bool :=
  r.name.startsWith "Design" && !r.cfg.userSetMp

def usage : String :=
  "usage: breaks [LIMIT] [--quiet | --summary] [--quick] [PATTERN...]\n" ++
  "  LIMIT      stop a search at this many states (default 40000000); such a search fails\n" ++
  "  --quiet    no step-by-step runs\n" ++
  "  --summary  the form WakeVerify/search.expected holds: no timings, no runs\n" ++
  "  --quick    the small searches only\n" ++
  "  PATTERN    searches whose title contains it; every pattern must match one\n" ++
  "exit status: 0 nothing failed; 1 a gate failed (FAILED lines say which); 2 nothing was\n" ++
  "searched: an argument was not understood, or no search matched"

def main (args : List String) : IO UInt32 := do
  -- An edge of the graph is `target * 64 + label` (`Graph.succ`).
  if labels.size > 64 then
    IO.eprintln s!"breaks: {labels.size} labels do not fit the graph's edge encoding (64)"
    return 2
  let (limit, rest) := match args with
    | a :: more => match a.toNat? with
      | some n => (n, more)
      | none => (40000000, args)
    | [] => (40000000, [])
  let flags := rest.filter (·.startsWith "--")
  let pats := rest.filter (!·.startsWith "--")
  for f in flags do
    if !["--quiet", "--summary", "--quick"].contains f then
      IO.eprintln s!"breaks: unknown option {f}\n{usage}"
      return 2
  if flags.contains "--quiet" && flags.contains "--summary" then
    IO.eprintln s!"breaks: --quiet and --summary are two forms of output\n{usage}"
    return 2
  let mode : Mode := if flags.contains "--summary" then .summary
    else if flags.contains "--quiet" then .quiet else .verbose
  let pool := if flags.contains "--quick" then searches.filter (·.quick) else searches
  let has (name pat : String) : Bool := (name.splitOn pat).length > 1
  for p in pats do
    if !pool.any (fun r => has r.title p) then
      IO.eprintln s!"breaks: no search matches {repr p}: nothing was searched"
      return 2
  let chosen := pool.filter fun r => pats.isEmpty || pats.any (has r.title)
  if chosen.isEmpty then
    IO.eprintln "breaks: no search to run"
    return 2
  let mut failures : Array String := #[]
  if mode != .summary then IO.println "# Lost wake-up search"
  for r in chosen do
    let (complete, clean, dead, unexplained) ← report r limit mode
    if !complete then
      failures := failures.push s!"{r.title}: the search did not finish, so nothing is established"
    if unexplained > 0 then
      failures := failures.push s!"{r.title}: {unexplained} dead vCPUs that no property flags"
    if r.isDesign then
      if !clean then
        failures := failures.push s!"{r.title}: a property breaks"
      if dead > 0 then
        failures := failures.push s!"{r.title}: {dead} states with a vCPU that can never run again"
  if !failures.isEmpty then
    IO.println "\nFAILED:"
    for f in failures do IO.println s!"  {f}"
    return 1
  return 0
