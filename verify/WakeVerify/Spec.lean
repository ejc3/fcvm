/-!
# The state of two vCPUs, and one step

A small-step model of how a guest vCPU goes to sleep and is woken, across three layers:

- the guest kernel (Linux 6.18.60): the paravirtual spinlock slow paths, a cross-CPU call that
  waits for its target, and the idle loop;
- host KVM (Linux v6.14 and v7.3.3): `mp_state`, the paravirtual kick `pv_unhalted`, the synthetic
  halt of an async page fault, and the completion of one;
- the VMM (the Firecracker fork fcvm pins) and fcvm: pause, snapshot, restore into a new VM, resume,
  and userfaultfd as the source of host page faults.

The state is plain data and `step` applies one labelled transition, or refuses it. Everything a
kernel release or a VMM does differently is a field of `Kernel` or `Rules`, filled in by
`Today.lean` (the code as it is, each rule citing file and line) and `Design.lean` (the repaired
system). The guest side is the same for every rule book and is cited here.

Citations: `kvm.c` is arch/x86/kernel/kvm.c, `pvq.h` is kernel/locking/qspinlock_paravirt.h,
`qspinlock.c` is kernel/locking/qspinlock.c, `smp.c` is kernel/smp.c, `idle.c` is
kernel/sched/idle.c, `process.c` is arch/x86/kernel/process.c, all at Linux v6.18.60.
-/

namespace Wake

/-- The two vCPUs. -/
inductive Vid where
  | a | b
  deriving DecidableEq, Repr, Hashable, Inhabited

def Vid.other : Vid → Vid
  | .a => .b
  | .b => .a

/-- `vcpu->arch.mp_state`, the values a vCPU of a running guest takes. -/
inductive Mp where
  | runnable       -- KVM_MP_STATE_RUNNABLE
  | uninitialized  -- KVM_MP_STATE_UNINITIALIZED: an application processor before its first INIT
  | halted         -- KVM_MP_STATE_HALTED
  | initReceived   -- KVM_MP_STATE_INIT_RECEIVED (wait for SIPI)
  | apResetHold    -- KVM_MP_STATE_AP_RESET_HOLD (SEV-ES)
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- The part of a vCPU's KVM state that decides whether it runs: what the kernel rule books act on,
and what the C harness compares with the kernel's own functions. -/
structure Hv where
  /-- `vcpu->arch.mp_state` -/
  mp : Mp := .runnable
  /-- `vcpu->arch.pv.pv_unhalted`: a paravirtual kick that has not been consumed -/
  pv : Bool := false
  /-- `vcpu->arch.apf.halted`: the synthetic halt of an async page fault -/
  apfHalted : Bool := false
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Where a vCPU's thread is. -/
inductive Loc where
  | guest   -- in guest mode: guest instructions run
  | host    -- in KVM's run loop (`vcpu_run`): entering, or blocked in `vcpu_block`
  | user    -- KVM_RUN has returned to the VMM (the VM is paused)
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- What a host page fault is to the guest. -/
inductive ApfKind where
  | pv      -- "page not present" was injected; the guest runs something else meanwhile
  | synth   -- nothing could be injected; KVM halted the vCPU itself (`apf.halted`)
  | all     -- the "wake everyone" completion queued when async page faults are (re)enabled
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- The one async page fault a vCPU may have outstanding. -/
inductive Apf where
  | none
  | flight (k : ApfKind)   -- on `vcpu->async_pf.queue`: the worker has not finished
  | done (k : ApfKind)     -- on `vcpu->async_pf.done`, KVM_REQ_APF_READY raised
  deriving DecidableEq, Repr, Hashable, Inhabited

def Apf.isDone : Apf → Bool
  | .done _ => true
  | _ => false

/-- The local APIC timer of a vCPU, as far as a wake-up needs it. -/
inductive Tmr where
  | off
  | armed     -- a deadline is set (`lapic_timer.tscdeadline`)
  | pending   -- expired, not yet injected (`lapic_timer.pending`)
  | irr       -- injected: the timer vector is in the IRR
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- The lock byte, `lock->locked`. -/
inductive Lock where
  | free     -- 0
  | locked   -- _Q_LOCKED_VAL
  | slow     -- _Q_SLOW_VAL: a halted (or halting) waiter is in the hash table
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Where the guest is in the modelled kernel paths. One value per atomic step. -/
inductive Pc where
  /-- anything else: user code or preemptible kernel code, interrupts on -/
  | run
  /-- `queued_spin_lock_slowpath`, before queueing: steal the lock or join the queue -/
  | lkEnter
  /-- queue head, first round: `trylock_clear_pending` in the spin loop -/
  | hdTry
  /-- queue head: `xchg(&lock->locked, _Q_SLOW_VAL)` -/
  | hdHash
  /-- queue head, later rounds: the spin loop again -/
  | hdTry2
  /-- queue head in `kvm_wait(&lock->locked, _Q_SLOW_VAL)`: interrupts off, read the byte -/
  | hdChk
  /-- queue head in `kvm_wait`: the HLT is next -/
  | hdHlt
  /-- queue head: the instruction after that HLT -/
  | hdWoke
  /-- queue node: the spin loop of `pv_wait_node` -/
  | ndSpin
  /-- queue node: `smp_store_mb(pn->state, VCPU_HALTED)` -/
  | ndSet
  /-- queue node: re-read `node->locked` -/
  | ndRe
  /-- queue node in `kvm_wait(&pn->state, VCPU_HALTED)`: interrupts off, read the state -/
  | ndChk
  /-- queue node in `kvm_wait`: the HLT is next -/
  | ndHlt
  /-- queue node: the instruction after that HLT -/
  | ndWoke
  /-- queue node: `cmpxchg(&pn->state, VCPU_HALTED, VCPU_RUNNING)` -/
  | ndCas
  /-- new owner with a successor: `arch_mcs_spin_unlock_contended(&next->locked)` -/
  | hoSet
  /-- new owner with a successor: `pv_kick_node(lock, next)` -/
  | hoKick
  /-- holds the lock -/
  | crit
  /-- `__pv_queued_spin_unlock`: `try_cmpxchg_release(&lock->locked, &locked, 0)` -/
  | ulTry
  /-- unlock slow path: unhash, `smp_store_release(&lock->locked, 0)` -/
  | ulRel
  /-- unlock slow path: `pv_kick(node->cpu)` -/
  | ulKick
  /-- cross-CPU call: queue the request and send the IPI -/
  | ccSend
  /-- cross-CPU call: `csd_lock_wait` -/
  | ccWait
  /-- idle loop: interrupts off, `need_resched()` read -/
  | idChk
  /-- idle loop: `safe_halt()` is next -/
  | idHlt
  /-- idle loop: the instruction after that HLT -/
  | idWoke
  /-- idle loop with a timer programmed: `safe_halt()` is next -/
  | idHltT
  /-- idle loop with a timer programmed: the instruction after that HLT -/
  | idWokeT
  /-- running a nested guest (L2) -/
  | l2
  /-- a nested guest that executed HLT -/
  | l2Woke
  /-- in the Xen `SCHEDOP_poll` hypercall -/
  | xenWoke
  /-- an offline CPU, parked in its HLT loop with interrupts off -/
  | off
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Queue-head positions. -/
def Pc.isHead : Pc → Bool
  | .hdTry | .hdHash | .hdTry2 | .hdChk | .hdHlt | .hdWoke => true
  | _ => false

/-- Queue-node positions. -/
def Pc.isNode : Pc → Bool
  | .ndSpin | .ndSet | .ndRe | .ndChk | .ndHlt | .ndWoke | .ndCas => true
  | _ => false

/-- The head is in its spin loop with the pending bit set (`set_pending`, pvq.h:429, to
`clear_pending`, pvq.h:435), so the lock cannot be stolen (pvq.h:91). -/
def Pc.headSpinning : Pc → Bool
  | .hdTry | .hdTry2 => true
  | _ => false

/-- Positions where a node's `pn->state` reads VCPU_HALTED unless `pv_kick_node` hashed it: from
the store (pvq.h:317) to the cmpxchg back (pvq.h:330). -/
def Pc.nodeHalted : Pc → Bool
  | .ndRe | .ndChk | .ndHlt | .ndWoke | .ndCas => true
  | _ => false

/-- Positions where the vCPU owns the lock. -/
def Pc.holds : Pc → Bool
  | .hoSet | .hoKick | .crit | .ulTry | .ulRel => true
  | _ => false

/-- One vCPU: the guest's registers and per-CPU data, then KVM's state for it. -/
structure Vcpu where
  pc : Pc := .run
  /-- Whether the lock is being taken with interrupts on (`spin_lock`) or off
  (`spin_lock_irqsave`, or a lock taken in an interrupt handler). True outside the lock paths. -/
  irqsOn : Bool := true
  /-- `node->locked` of this vCPU's queue node: the previous owner handed the queue head over -/
  mcs : Bool := false
  /-- `pn->state == VCPU_HASHED`, set by the new owner's `pv_kick_node` -/
  hashed : Bool := false
  /-- a cross-CPU call is queued for this vCPU and not yet run -/
  csd : Bool := false
  /-- the page-ready slot in guest memory is occupied: `apf_reason.token != 0`, from KVM's write
  of a token (`apf_put_user_ready`) until the guest's handler clears it -/
  slot : Bool := false
  loc : Loc := .guest
  h : Hv := {}
  apf : Apf := .none
  /-- the call-function vector is pending in this vCPU's IRR -/
  ipi : Bool := false
  /-- the page-ready vector (`apf.vec`) is pending in this vCPU's IRR -/
  apfIrq : Bool := false
  tmr : Tmr := .off
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- RFLAGS.IF, which follows from the position: `kvm_wait` turns interrupts off before it reads
(kvm.c:1102-1110) and its HLT is `halt()` with them off or `safe_halt()` (STI; HLT), the idle loop
reads `need_resched()` with them off (idle.c:330) and halts with `safe_halt()` (process.c:767), and
an offline CPU halts with them off. -/
def Vcpu.ifOn (v : Vcpu) : Bool :=
  match v.pc with
  | .run | .ccSend | .ccWait | .idWoke | .idWokeT | .l2 | .l2Woke | .xenWoke => true
  | .idChk | .idHlt | .idHltT | .off | .hdHlt | .ndHlt => false
  | _ => v.irqsOn

/-- `kvm_vcpu_has_events()` without the kick: a completion on the done list (v7.3.3
x86.c:8524-8525), or an interrupt the guest can take now (x86.c:8552-8553). The NMI, INIT and SMI
arms of that function are the `nmi` and `init` steps. -/
def Vcpu.events (v : Vcpu) : Bool :=
  v.apf.isDone || (v.ifOn && (v.ipi || v.apfIrq || v.tmr == .irr))

/-- `kvm_vcpu_running()` -/
def Vcpu.running (v : Vcpu) : Bool :=
  v.h.mp == .runnable && !v.h.apfHalted

/-- `kvm_arch_vcpu_runnable()`: what ends `kvm_vcpu_block()`. Both releases compute the same value
(v6.14 x86.c:11107-11166 has the kick inside `kvm_vcpu_has_events`, v7.3.3 x86.c:8516-8572 beside
it). -/
def Vcpu.runnable (v : Vcpu) : Bool :=
  v.running || v.h.pv || v.events

/-- The whole model: two vCPUs, one lock, and the VMM's view. -/
structure State where
  a : Vcpu := {}
  b : Vcpu := {}
  lock : Lock := .free
  /-- the VMM has paused the VM: every vCPU thread is out of KVM_RUN -/
  paused : Bool := false
  /-- this VM was restored from a snapshot (of the first VM, or of a restored one: any number of
  generations) -/
  cloned : Bool := false
  deriving DecidableEq, Repr, Hashable, Inhabited

def State.get (s : State) : Vid → Vcpu
  | .a => s.a
  | .b => s.b

/-! ## The rule books -/

/-- What a kernel release does at each place that writes `mp_state` or the kick. One field per
place; `Today.lean` and `Design.lean` fill them in. -/
structure Kernel where
  name : String
  /-- `__kvm_emulate_halt(vcpu, state, reason)`. The Bool is `kvm_vcpu_has_events()` without the
  kick. -/
  halt : Bool → Mp → Hv → Hv
  /-- the `switch` that ends `vcpu_block()`, reached once the vCPU is runnable again -/
  unhalt : Hv → Hv
  /-- the tail of `kvm_arch_async_page_present()` -/
  present : Hv → Hv
  /-- `APIC_DM_REMRD` in `__apic_accept_irq()`: a kick arrives -/
  kick : Hv → Hv
  /-- `KVM_GET_MP_STATE` -/
  getMpState : Hv → Mp
  /-- `KVM_SET_MP_STATE` -/
  setMpState : Mp → Hv → Hv
  /-- a nested VM-exit ("in case we halted in L2") -/
  nestedExit : Hv → Hv
  /-- Xen `SCHEDOP_poll` before it blocks, and after -/
  xenBlock : Hv → Hv
  xenWake : Hv → Hv
  /-- INIT accepted by an application processor: `kvm_vcpu_reset()` and wait-for-SIPI -/
  initAp : Hv → Hv
  /-- SIPI accepted in wait-for-SIPI -/
  sipi : Hv → Hv
  /-- SEV-SNP AP creation -/
  snpReset : Hv → Hv

/-- The KVM state ioctls of a restore that the model follows. -/
inductive Ioctl where
  | setMpState   -- KVM_SET_MP_STATE
  | setLapic     -- KVM_SET_LAPIC: the IRR, and the timer's mode
  | setMsrs      -- KVM_SET_MSRS: MSR_KVM_ASYNC_PF_EN, then MSR_IA32_TSC_DEADLINE
  | kick         -- KVM_SIGNAL_MSI with the remote-read delivery mode, to a vCPU saved runnable
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- A rule book: a kernel and the VMM's restore order. -/
structure Rules where
  kernel : Kernel
  /-- the order in which a restore issues its ioctls on each new vCPU -/
  restore : List Ioctl

/-- What exists in the guest and around it. The fcvm configuration is the default. -/
structure Cfg where
  /-- the guest enabled the async page fault interface (`MSR_KVM_ASYNC_PF_EN`, kvm.c:377-395) -/
  pvApf : Bool := true
  /-- host page faults that need I/O happen (memory served through userfaultfd) -/
  faults : Bool := true
  /-- they also happen in the lock, call and idle paths, wherever interrupts are on (the theorems
  hold either way; a search with this off is smaller) -/
  kernelFaults : Bool := true
  /-- locks are taken with interrupts off -/
  irqLocks : Bool := true
  /-- locks are taken with interrupts on -/
  plainLocks : Bool := true
  /-- cross-CPU calls that wait -/
  calls : Bool := true
  /-- the idle loop -/
  idle : Bool := true
  /-- the VMM pauses, snapshots, restores and resumes -/
  snapshot : Bool := true
  /-- interrupts from sources the model does not follow (devices, the tick) may arrive at a
  blocked vCPU that can take them -/
  rescue : Bool := true
  /-- NMIs may arrive -/
  nmi : Bool := false
  /-- the idle loop programs the APIC timer -/
  timer : Bool := false
  /-- the guest runs a nested guest (not on x86 fcvm hosts) -/
  nested : Bool := false
  /-- the guest uses Xen's `SCHEDOP_poll` (never under fcvm) -/
  xen : Bool := false
  /-- userspace reads `mp_state` of a paused, live VM and writes it back (Firecracker does not;
  a VMM that synchronises vCPU state in place does) -/
  userSetMp : Bool := false
  /-- CPU hotplug: a vCPU goes offline and is restarted with INIT and SIPI -/
  hotplug : Bool := false
  /-- SEV-ES and SEV-SNP application processor paths (never under fcvm) -/
  sev : Bool := false
  deriving DecidableEq, Repr, Inhabited

/-- The fcvm configuration: two vCPUs, paravirtual spinlocks, the async page fault interface on,
memory served through userfaultfd, snapshots and restores, devices and a tick; no nested guest on
x86, no Xen, no SEV, no CPU hotplug, and a VMM that never writes `mp_state` to a live VM. -/
def Cfg.fcvm : Cfg := {}

/-! ## Labels -/

/-- What one vCPU, or the host on its behalf, does in one step. -/
inductive Act where
  /-- `spin_lock_irqsave()`: start taking the lock with interrupts off -/
  | lockIrq
  /-- `spin_lock()`: start taking the lock with interrupts on -/
  | lockPlain
  /-- start a cross-CPU call to the other vCPU and wait for it -/
  | call
  /-- enter the idle loop -/
  | idle
  /-- the next instruction of the path the vCPU is on -/
  | step
  /-- idle loop: program the timer, then halt -/
  | idArm
  /-- a host page fault in user mode: "page not present" is injected -/
  | faultPv
  /-- a host page fault KVM cannot inject: synthetic halt -/
  | faultSynth
  /-- the faulting instruction runs again before its page is ready -/
  | refault
  /-- the guest takes the call-function interrupt and runs the queued call -/
  | irq
  /-- the guest takes the timer interrupt -/
  | irqTmr
  /-- the guest takes the page-ready interrupt: its handler clears the slot and acknowledges -/
  | irqApf
  /-- one iteration of `vcpu_run()`: enter the guest, or leave `vcpu_block()` -/
  | hostRun
  /-- the async page fault worker finishes -/
  | apfDone
  /-- the APIC timer expires -/
  | tmrFire
  /-- `kvm_inject_pending_timer_irqs()` -/
  | tmrInject
  /-- an NMI arrives at a blocked vCPU -/
  | nmi
  /-- an interrupt from a source the model does not follow arrives at a blocked vCPU -/
  | extIrq
  | l2Enter | l2Hlt | l2Exit
  | xenPoll | xenWake
  /-- userspace writes back, with `KVM_SET_MP_STATE`, what `KVM_GET_MP_STATE` reported, on the
  paused VM -/
  | setMpSame
  | offline | init | sipi | sevHold | snpCreate
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Every action. `Proofs.lean` proves the list complete (`allActs_complete`), so a search over it
misses no transition. -/
def allActs : List Act :=
  [.lockIrq, .lockPlain, .call, .idle, .step, .idArm, .faultPv, .faultSynth, .refault, .irq,
   .irqTmr, .irqApf, .hostRun, .apfDone, .tmrFire, .tmrInject, .nmi, .extIrq, .l2Enter, .l2Hlt,
   .l2Exit, .xenPoll, .xenWake, .setMpSame, .offline, .init, .sipi, .sevHold, .snpCreate]

inductive Label where
  | vcpu (i : Vid) (act : Act)
  /-- the VMM pauses the VM: every vCPU thread leaves KVM_RUN -/
  | pause
  /-- the VMM resumes the VM -/
  | resume
  /-- the VMM saves the paused VM and restores the snapshot into a new VM; the run follows the
  new VM -/
  | clone
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Every label (`allLabels_complete`). -/
def allLabels : List Label :=
  (allActs.map (Label.vcpu .a)) ++ (allActs.map (Label.vcpu .b)) ++ [.pause, .resume, .clone]

/-! ## The guest

A vCPU's step is written over three things: the vCPU that moves (`me`), the other one (`o`) and the
lock byte. It returns the three as they are afterwards. -/

/-- The moving vCPU, the other vCPU, the lock byte. -/
abbrev View := Vcpu × Vcpu × Lock

/-- A HLT: KVM takes the exit and runs `__kvm_emulate_halt`, having moved RIP past the instruction
(`kvm_emulate_halt`, v7.3.3 x86.c:8720-8728). `next` is the instruction after the HLT. -/
def Vcpu.hlt (K : Kernel) (st : Mp) (next : Pc) (v : Vcpu) : Vcpu :=
  { v with pc := next, h := K.halt { v with pc := next }.events st v.h, loc := .host }

/-- The new owner's next position: hand the queue head to a successor if there is one
(qspinlock.c:325-371). -/
def gotLock (o : Vcpu) : Pc :=
  if o.pc.isNode then .hoSet else .crit

/-- The next instruction of the path `me` is on. -/
def advance (K : Kernel) (me o : Vcpu) (lock : Lock) : Option View :=
  match me.pc with
  -- pv_hybrid_queued_unfair_trylock (pvq.h:81-99), then xchg_tail (qspinlock.c:260-290)
  | .lkEnter =>
    if lock == .free && !o.pc.headSpinning then some ({ me with pc := .crit }, o, .locked)
    else if !(o.pc.isHead || o.pc.isNode) then some ({ me with pc := .hdTry }, o, lock)
    else if o.pc.headSpinning then some ({ me with pc := .ndSpin }, o, lock)
    else none
  -- pv_wait_head_or_lock: trylock_clear_pending in the spin loop (pvq.h:429-435)
  | .hdTry =>
    if lock == .free then some ({ me with pc := gotLock o }, o, .locked)
    else some ({ me with pc := .hdHash }, o, lock)
  -- xchg(&lock->locked, _Q_SLOW_VAL) (pvq.h:437-463)
  | .hdHash =>
    if lock == .free then some ({ me with pc := gotLock o }, o, .locked)
    else some ({ me with pc := .hdChk }, o, .slow)
  | .hdTry2 =>
    if lock == .free then some ({ me with pc := gotLock o }, o, .locked)
    else some ({ me with pc := .hdChk }, o, lock)
  -- pv_wait(&lock->locked, _Q_SLOW_VAL) (pvq.h:466) is kvm_wait (kvm.c:1092-1113): with
  -- interrupts off, READ_ONCE(*ptr) == val
  | .hdChk =>
    if lock == .slow then some ({ me with pc := .hdHlt }, o, lock)
    else some ({ me with pc := .hdTry2 }, o, lock)
  -- halt() or safe_halt() (kvm.c:1104, 1110)
  | .hdHlt => some (me.hlt K .halted .hdWoke, o, lock)
  -- back to the top of the loop in pv_wait_head_or_lock (pvq.h:417)
  | .hdWoke => some ({ me with pc := .hdTry2 }, o, lock)
  -- pv_wait_node: READ_ONCE(node->locked) in the spin loop (pvq.h:298-305); with it set the
  -- vCPU is the queue head (qspinlock.c:290-325), hashed already if pv_kick_node got to it
  -- (pvq.h:410-411, 423)
  | .ndSpin =>
    if me.mcs then
      some ({ me with pc := if me.hashed then .hdTry2 else .hdTry, mcs := false, hashed := false },
            o, lock)
    else some ({ me with pc := .ndSet }, o, lock)
  -- smp_store_mb(pn->state, VCPU_HALTED) (pvq.h:317)
  | .ndSet => some ({ me with pc := .ndRe }, o, lock)
  -- if (!READ_ONCE(node->locked)) pv_wait(&pn->state, VCPU_HALTED) (pvq.h:319-322)
  | .ndRe =>
    if me.mcs then some ({ me with pc := .ndCas }, o, lock)
    else some ({ me with pc := .ndChk }, o, lock)
  -- kvm_wait: with interrupts off, READ_ONCE(*ptr) == val (kvm.c:1102-1110)
  | .ndChk =>
    if me.hashed then some ({ me with pc := .ndCas }, o, lock)
    else some ({ me with pc := .ndHlt }, o, lock)
  | .ndHlt => some (me.hlt K .halted .ndWoke, o, lock)
  | .ndWoke => some ({ me with pc := .ndCas }, o, lock)
  -- cmpxchg(&pn->state, VCPU_HALTED, VCPU_RUNNING) (pvq.h:330)
  | .ndCas => some ({ me with pc := .ndSpin }, o, lock)
  -- arch_mcs_spin_unlock_contended(&next->locked) (qspinlock.c:370)
  | .hoSet => some ({ me with pc := .hoKick }, { o with mcs := true }, lock)
  -- pv_kick_node (qspinlock.c:371, pvq.h:357-389): if the successor's state is VCPU_HALTED, make
  -- it VCPU_HASHED and the lock byte _Q_SLOW_VAL
  | .hoKick =>
    if o.pc.nodeHalted && !o.hashed then
      some ({ me with pc := .crit }, { o with hashed := true }, .slow)
    else some ({ me with pc := .crit }, o, lock)
  | .crit => some ({ me with pc := .ulTry }, o, lock)
  -- __pv_queued_spin_unlock: try_cmpxchg_release(&lock->locked, &locked, 0) (pvq.h:543-556)
  | .ulTry =>
    if lock == .locked then some ({ me with pc := .run, irqsOn := true }, o, .free)
    else some ({ me with pc := .ulRel }, o, lock)
  -- __pv_queued_spin_unlock_slowpath: pv_unhash, smp_store_release(&lock->locked, 0)
  -- (pvq.h:523-529)
  | .ulRel => some ({ me with pc := .ulKick }, o, .free)
  -- pv_kick(node->cpu) (pvq.h:539) is kvm_kick_cpu (kvm.c:1081-1088): KVM_HC_KICK_CPU, which KVM
  -- delivers as APIC_DM_REMRD (v7.3.3 x86.c:7244-7256, 7383-7389). With two vCPUs the hashed
  -- node is the other one.
  | .ulKick => some ({ me with pc := .run, irqsOn := true }, { o with h := K.kick o.h }, lock)
  -- smp_call_function_many_cond: csd_lock, queue, send_call_function_ipi_mask (smp.c:829-859).
  -- A target that is polling in the idle loop gets need_resched set and no interrupt
  -- (call_function_single_prep_ipi, smp.c:115-120); it leaves the poll and runs the queue, which
  -- for the model is the same as taking the interrupt at once.
  | .ccSend => some ({ me with pc := .ccWait }, { o with csd := true, ipi := true }, lock)
  -- csd_lock_wait (smp.c:877)
  | .ccWait => if o.csd then none else some ({ me with pc := .run }, o, lock)
  -- do_idle: local_irq_disable, need_resched read (idle.c:298-330); default_idle_call reads it
  -- once more with interrupts off (current_clr_polling_and_test, idle.c:116)
  | .idChk => some ({ me with pc := .idHlt }, o, lock)
  -- default_idle: raw_safe_halt (process.c:765-768)
  | .idHlt => some (me.hlt K .halted .idWoke, o, lock)
  | .idWoke => some ({ me with pc := .run }, o, lock)
  | .idHltT => some (me.hlt K .halted .idWokeT, o, lock)
  | .idWokeT => some ({ me with pc := .run }, o, lock)
  -- an offline CPU halts again whenever it wakes (hlt_play_dead)
  | .off => some (me.hlt K .halted .off, o, lock)
  | _ => none

/-- What `me` does in guest mode. -/
def guest (K : Kernel) (C : Cfg) (act : Act) (me o : Vcpu) (lock : Lock) : Option View :=
  if me.loc != .guest then none else
  match act with
  | .lockIrq =>
    if C.irqLocks && me.pc == .run then
      some ({ me with pc := .lkEnter, irqsOn := false }, o, lock) else none
  | .lockPlain =>
    if C.plainLocks && me.pc == .run then
      some ({ me with pc := .lkEnter, irqsOn := true }, o, lock) else none
  | .call =>
    if C.calls && me.pc == .run && !o.csd && o.pc != .off then
      some ({ me with pc := .ccSend }, o, lock) else none
  | .idle =>
    if C.idle && me.pc == .run then some ({ me with pc := .idChk }, o, lock) else none
  | .step => advance K me o lock
  -- tick_nohz_idle_stop_tick programs the next event before the halt (idle.c:226); the
  -- write of MSR_IA32_TSC_DEADLINE arms the timer (v7.3.3 lapic.c:2749-2759)
  | .idArm =>
    if C.timer && me.pc == .idChk && me.tmr == .off then
      some ({ me with pc := .idHltT, tmr := .armed }, o, lock) else none
  -- A host page fault that needs I/O, taken where the guest can take interrupts
  -- (kvm_can_do_async_pf, v7.3.3 x86.c:10533-10548; mmu/mmu.c:4670-4678). In user mode with the
  -- interface on, "page not present" is injected and the guest schedules something else
  -- (x86.c:10550-10567; kvm_can_deliver_async_pf, x86.c:10507-10531, refuses kernel mode because
  -- the guest does not set KVM_ASYNC_PF_SEND_ALWAYS). User mode is ordinary code here.
  | .faultPv =>
    if C.faults && C.pvApf && me.pc == .run && me.apf == .none then
      some ({ me with apf := .flight .pv }, o, lock) else none
  -- Otherwise (kernel mode, or no interface) KVM_REQ_APF_HALT: the vCPU is halted artificially
  -- (x86.c:10568-10579, 8147-8152). Anywhere interrupts are on: in the lock paths too.
  | .faultSynth =>
    if C.faults && me.ifOn && (C.kernelFaults || me.pc == .run) && me.apf == .none then
      some ({ me with apf := .flight .synth, h := { me.h with apfHalted := true }, loc := .host },
            o, lock)
    else none
  -- The instruction faults again while its page is in flight (mmu/mmu.c:4672-4675). The code
  -- halts again for a repeated fault on the same frame after either kind of first fault; the
  -- model does it after a synthetic one only. A halt that is not modelled is a wake-up that is.
  | .refault =>
    if me.ifOn && (C.kernelFaults || me.pc == .run) && me.apf == .flight .synth then
      some ({ me with h := { me.h with apfHalted := true }, loc := .host }, o, lock) else none
  -- generic_smp_call_function_single_interrupt runs the queued call and unlocks its csd
  -- (smp.c:461-464, 547)
  | .irq =>
    if me.ifOn && me.ipi then some ({ me with ipi := false, csd := false }, o, lock) else none
  | .irqTmr =>
    if me.ifOn && me.tmr == .irr then some ({ me with tmr := .off }, o, lock) else none
  -- sysvec_kvm_asyncpf_interrupt (kvm.c:317-334): wake the task the token names, clear
  -- apf_reason.token, write MSR_KVM_ASYNC_PF_ACK. The write makes KVM run
  -- kvm_check_async_pf_completion again (v7.3.3 msrs.c:1695-1707): that second caller is the
  -- `hostRun` step below, which a vCPU in guest mode may take at any position.
  | .irqApf =>
    if me.ifOn && me.apfIrq then some ({ me with apfIrq := false, slot := false }, o, lock)
    else none
  | .l2Enter =>
    if C.nested && me.pc == .run then some ({ me with pc := .l2 }, o, lock) else none
  -- the nested guest's HLT, when L1 does not intercept it
  | .l2Hlt =>
    if me.pc == .l2 then some (me.hlt K .halted .l2Woke, o, lock) else none
  | .l2Exit =>
    if me.pc == .l2 || me.pc == .l2Woke then
      some ({ me with pc := .run, h := K.nestedExit me.h }, o, lock) else none
  | .xenPoll =>
    if C.xen && me.pc == .run then
      some ({ me with pc := .xenWoke, h := K.xenBlock me.h, loc := .host }, o, lock) else none
  | .offline =>
    if C.hotplug && me.pc == .run && !me.csd && o.pc != .ccSend then
      some ({ me with pc := .off }, o, lock) else none
  -- SEV-ES: an AP parks with the AP_RESET_HOLD exit instead of HLT (kvm_emulate_ap_reset_hold)
  | .sevHold =>
    if C.sev && me.pc == .off then some (me.hlt K .apResetHold .off, o, lock) else none
  | _ => none

/-! ## The host -/

/-- `kvm_arch_async_page_present()` for a completion of kind `k` (v7.3.3 x86.c:10582-10605). For
the wake-everyone completion and for one whose "page not present" was injected, it writes the
token into the guest's page-ready slot and raises the page-ready interrupt (x86.c:10596-10601).
For the completion of a synthetic halt it does neither. Then the rule book's tail. -/
def Vcpu.present (K : Kernel) (k : ApfKind) (v : Vcpu) : Vcpu :=
  { v with h := K.present v.h, apf := .none,
           slot := v.slot || k != .synth, apfIrq := v.apfIrq || k != .synth }

/-- One iteration of the loop in `vcpu_run()` (v7.3.3 x86.c:8643-8691) for a vCPU thread that is in
KVM_RUN. A vCPU that is running enters the guest, after `kvm_check_async_pf_completion` if a
completion is on the done list (x86.c:8216-8217, virt/kvm/async_pf.c:153-174) and the guest's
page-ready slot is free (`kvm_arch_can_dequeue_async_page_present`, x86.c:10618-10624; the slot is
only ever occupied when the guest enabled the interface). A vCPU already in the guest is brought
out and back in for that by the worker's `kvm_vcpu_kick` (x86.c:10607-10616), or runs it from its
own write of `MSR_KVM_ASYNC_PF_ACK` (msrs.c:1695-1707). With the slot occupied the completion
stays on the done list, where it keeps `kvm_vcpu_has_events()` true.

A vCPU that is not running goes through `vcpu_block()` (x86.c:8575-8640): it stays blocked until
`kvm_arch_vcpu_runnable()`, then the switch at the end makes it run. (An UNINITIALIZED vCPU
blocks in `kvm_arch_vcpu_ioctl_run` itself, x86.c:8910-8941; no modelled restore leaves one.) -/
def hostRun (K : Kernel) (v : Vcpu) : Option Vcpu :=
  if v.loc == .user then none
  else if v.running then
    match v.apf with
    | .done k =>
      if v.slot then (if v.loc == .host then some { v with loc := .guest } else none)
      else some { v.present K k with loc := .guest }
    | _ => if v.loc == .host then some { v with loc := .guest } else none
  else if v.loc == .guest then none
  else if v.pc == .xenWoke then none
  else if v.runnable then some { v with h := K.unhalt v.h }
  else none

/-- `kvm_vcpu_halt()` returns at a blocked vCPU because an event arrived. In `vcpu_block` the
switch at its end runs. In Xen's `SCHEDOP_poll` the hypercall returns to the guest, runnable,
whatever ended the wait (xen.c:1583-1592). -/
def Vcpu.wake (K : Kernel) (v : Vcpu) : Vcpu :=
  if v.pc == .xenWoke then { v with pc := .run, h := K.xenWake v.h }
  else { v with h := K.unhalt v.h }

/-- What the host does to `me`, or for it. -/
def host (K : Kernel) (C : Cfg) (paused : Bool) (act : Act) (me o : Vcpu) (lock : Lock) :
    Option View :=
  match act with
  | .hostRun => (hostRun K me).map (fun v => (v, o, lock))
  -- async_pf_execute: the page is in; the work goes on the done list, KVM_REQ_APF_READY is
  -- raised and the vCPU thread is woken (virt/kvm/async_pf.c:45-97, x86.c:10607-10616)
  | .apfDone =>
    match me.apf with
    | .flight k => some ({ me with apf := .done k }, o, lock)
    | _ => none
  -- apic_timer_expired: lapic_timer.pending, KVM_REQ_UNBLOCK (lapic.c:2053-2089)
  | .tmrFire =>
    if me.tmr == .armed then some ({ me with tmr := .pending }, o, lock) else none
  -- kvm_inject_pending_timer_irqs in the run loop (x86.c:8670-8671, lapic.c:3203-3211)
  | .tmrInject =>
    if me.tmr == .pending && me.loc != .user then some ({ me with tmr := .irr }, o, lock)
    else none
  -- An NMI makes kvm_vcpu_has_events true whatever RFLAGS.IF is (x86.c:8534-8537): the blocked
  -- vCPU's kvm_vcpu_halt returns.
  | .nmi =>
    if C.nmi && me.loc == .host && !me.running then some (me.wake K, o, lock) else none
  -- So does an interrupt from a device or the tick, with interrupts on (x86.c:8552-8553).
  | .extIrq =>
    if C.rescue && me.loc == .host && !me.running && me.ifOn then some (me.wake K, o, lock)
    else none
  -- kvm_xen_schedop_poll after kvm_vcpu_halt returns because the vCPU is runnable
  -- (xen.c:1581-1592)
  | .xenWake =>
    if me.loc == .host && me.pc == .xenWoke && me.runnable then some (me.wake K, o, lock)
    else none
  | .setMpSame =>
    if C.userSetMp && paused then
      some ({ me with h := K.setMpState (K.getMpState me.h) me.h }, o, lock) else none
  -- INIT arrives at an offline vCPU; kvm_apic_accept_events resets it and leaves it waiting for
  -- SIPI (lapic.c:3587-3623). kvm_vcpu_reset drops its async page faults (x86.c:9607-9609).
  -- Linux sends INIT to an offline CPU only, and the proofs need that: an INIT at a vCPU asleep in
  -- kvm_wait would reset it under the lock path it is in.
  | .init =>
    if C.hotplug && me.pc == .off && me.loc == .host && me.h.mp != .initReceived then
      some ({ me with h := K.initAp me.h, apf := .none }, o, lock) else none
  | .sipi =>
    if C.hotplug && me.pc == .off && me.loc == .host && me.h.mp == .initReceived then
      some ({ me with pc := .run, irqsOn := true, mcs := false, hashed := false,
                      h := K.sipi me.h }, o, lock) else none
  | .snpCreate =>
    if C.sev && me.pc == .off && me.loc == .host then
      some ({ me with pc := .run, irqsOn := true, mcs := false, hashed := false,
                      h := K.snpReset me.h }, o, lock) else none
  | _ => none

/-- One step of vCPU `me`: in the guest if the action is a guest's and enabled, else the host's. -/
def vstep (K : Kernel) (C : Cfg) (paused : Bool) (act : Act) (me o : Vcpu) (lock : Lock) :
    Option View :=
  match guest K C act me o lock with
  | some w => some w
  | none => host K C paused act me o lock

/-- Write a vCPU's view back: `i` is the vCPU that moved. -/
def State.put (s : State) (i : Vid) (w : View) : State :=
  match i with
  | .a => { s with a := w.1, b := w.2.1, lock := w.2.2 }
  | .b => { s with a := w.2.1, b := w.1, lock := w.2.2 }

/-! ## The VMM: pause, snapshot, restore, resume -/

/-- What a snapshot holds of one vCPU's KVM state. The guest's memory and registers (everything
in `Vcpu` before `loc`, the page-ready slot included) are saved whole. `pv_unhalted` of a runnable
vCPU, `apf.halted` and the async page fault queues are in no ioctl's answer. -/
structure Saved where
  /-- KVM_GET_MP_STATE -/
  mp : Mp
  /-- KVM_GET_LAPIC: the IRR -/
  ipi : Bool
  apfIrq : Bool
  tmrIrr : Bool
  /-- KVM_GET_MSRS: MSR_IA32_TSC_DEADLINE is not zero -/
  deadline : Bool

/-- Firecracker's `save_state` (arch/x86_64/vcpu.rs:591-650). Two things the code does and the
model does not, both of which can only add a wake-up to the restored VM: `KVM_GET_MP_STATE` first
runs `kvm_apic_accept_events` (v7.3.3 x86.c:9020), which matters only with an INIT or SIPI
pending, and Firecracker replaces a zero `MSR_IA32_TSC_DEADLINE` with the current TSC
(`fix_zero_tsc_deadline_msr`, vcpu.rs:417-448), which makes a timer fire at once. -/
def save (K : Kernel) (v : Vcpu) : Saved :=
  { mp := K.getMpState v.h, ipi := v.ipi, apfIrq := v.apfIrq, tmrIrr := v.tmr == .irr,
    deadline := v.tmr == .armed || v.tmr == .pending }

/-- `mp_state` of a new vCPU: `kvm_arch_vcpu_create` makes the boot processor runnable and every
other vCPU UNINITIALIZED (v7.3.3 x86.c:9376-9379). -/
def freshHv (second : Bool) : Hv :=
  if second then { mp := .uninitialized } else {}

/-- A vCPU of the new VM before any state is set, with the guest's memory and registers from the
snapshot. `second` says it is not the boot processor. -/
def Vcpu.fresh (second : Bool) (v : Vcpu) : Vcpu :=
  { v with loc := .user, h := freshHv second, apf := .none, ipi := false, apfIrq := false,
           tmr := .off }

/-- One restore ioctl. The Bool carried along says whether KVM_SET_LAPIC has run, because
`kvm_set_lapic_tscdeadline_msr` drops a deadline written before the timer is in deadline mode
(v7.3.3 lapic.c:2749-2759). -/
def restoreOne (K : Kernel) (C : Cfg) (sv : Saved) (acc : Vcpu × Bool) : Ioctl → Vcpu × Bool
  | .setMpState => ({ acc.1 with h := K.setMpState sv.mp acc.1.h }, acc.2)
  | .setLapic =>
    ({ acc.1 with ipi := sv.ipi, apfIrq := sv.apfIrq,
                  tmr := if sv.tmrIrr then .irr else acc.1.tmr }, true)
  -- MSR_KVM_ASYNC_PF_EN: kvm_pv_enable_async_pf queues the "wake everyone" completion, whether
  -- or not the write comes from the host (v7.3.3 msrs.c:1384-1417, virt/kvm/async_pf.c:216-241)
  | .setMsrs =>
    ({ acc.1 with apf := if C.pvApf then .done .all else acc.1.apf,
                  tmr := if sv.deadline && acc.2 then .armed else acc.1.tmr }, acc.2)
  | .kick => ({ acc.1 with h := if sv.mp == .runnable then K.kick acc.1.h else acc.1.h }, acc.2)

/-- A vCPU of the new VM after the restore's ioctls, in the rule book's order. -/
def restoreVcpu (R : Rules) (C : Cfg) (second : Bool) (v : Vcpu) : Vcpu :=
  (R.restore.foldl (restoreOne R.kernel C (save R.kernel v)) (v.fresh second, false)).1

/-- A vCPU thread leaves KVM_RUN for the VMM's signal. A vCPU blocked in `vcpu_block` keeps its
state (v7.3.3 x86.c:8601-8607). One in Xen's `SCHEDOP_poll` has the hypercall return first, which
makes it runnable (xen.c:1583-1592). -/
def Vcpu.pause (K : Kernel) (v : Vcpu) : Vcpu :=
  if v.pc == .xenWoke then { v with pc := .run, h := K.xenWake v.h, loc := .user }
  else { v with loc := .user }

/-- One step of the whole system. `none` when the label is not enabled. -/
def step (R : Rules) (C : Cfg) (l : Label) (s : State) : Option State :=
  match l with
  | .vcpu i act =>
    (vstep R.kernel C s.paused act (s.get i) (s.get i.other) s.lock).map (s.put i)
  | .pause =>
    if C.snapshot && !s.paused then
      some { s with a := s.a.pause R.kernel, b := s.b.pause R.kernel, paused := true }
    else none
  | .resume =>
    if s.paused then
      some { s with a := { s.a with loc := .host }, b := { s.b with loc := .host }, paused := false }
    else none
  | .clone =>
    if s.paused then
      some { s with a := restoreVcpu R C false s.a, b := restoreVcpu R C true s.b, cloned := true }
    else none

/-- Both vCPUs running ordinary code, the lock free, nothing pending. -/
def init : State := {}

/-- Apply a list of labels in order. `none` if one of them is not enabled. -/
def replay (R : Rules) (C : Cfg) : List Label → State → Option State
  | [], s => some s
  | l :: ls, s =>
    match step R C l s with
    | some t => replay R C ls t
    | none => none

/-- The states the system can reach from `init`. -/
inductive Reachable (R : Rules) (C : Cfg) : State → Prop where
  | init : Reachable R C init
  | step {s t : State} (l : Label) : Reachable R C s → step R C l s = some t → Reachable R C t

end Wake
