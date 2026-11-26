# ResumeVerify: a vCPU's timer and interrupt state across a snapshot and a restore, on arm64

A Lean 4 model of what happens to a guest vCPU's interrupt and timer state when a VM is paused,
saved, resumed in place, or restored into a new VM, on arm64 KVM (Linux v7.3.3, the fcvm host
kernel), for the two VMMs fcvm drives: Cloud Hypervisor and fcvm's Firecracker fork. Namespace
`Resume`, library `ResumeVerify`, search `lake exe resume-breaks`.

It was written for #2155 (a Cloud Hypervisor clone whose vCPU 1 used a full host CPU while guest
CPU 1 took no interrupt). The other two models in this directory cover x86 wake-ups and the
network boundary; nothing modelled arm64, the vGIC or the timer.

- [`BREAKS.md`](BREAKS.md): every break found, what it is in the real system, whether fcvm can
  reach it, and what a capture would show.
- [`REPAIRS.md`](REPAIRS.md): the change each real break needs, and where.

## Least sure

Where a rule decides a result and I am not certain I read it right.

1. **How #630 moved the clock on a restore: now measured for v7.3.3.** The project's record
   (CLAUDE.md, "NV2 Snapshot Lifecycle") says the offset KVM computes on a `CNTVCT` write is kept
   in `CNTVOFF_EL2` storage and that the replay of `CNTVOFF_EL2` clobbers it, so the clock
   jumps forward by the time since the snapshot. The source says otherwise at v6.18.3 (the host
   kernel when #630 was found) and at v7.3.3, which agree line for line:
   - `timer_set_offset` writes only `*ctxt->offset.vm_offset` (include/kvm/arm_arch_timer.h,
     6.18.3:179-187, 7.3.3:188-196).
   - For a vCPU with EL2, `kvm_timer_vcpu_reset` points the virtual timer's `vm_offset` at the
     VM's physical offset and its `vcpu_offset` at the `CNTVOFF_EL2` slot (arch_timer.c
     6.18.3:1027-1032, 7.3.3:1048-1053).
   - So a user write of `CNTVCT_EL0` (`arch_timer_set_user`, sys_regs.c 6.18.3:1608-1611,
     7.3.3:1790-1793) stores `now - value` in the physical offset. With `CNTVOFF_EL2 = 0` at the
     save the replay is coherent. With `CNTVOFF_EL2 = X` on the last vCPU replayed, the physical
     counter comes out X behind.

   `probe/counter-replay.c` does those writes on a vCPU with `KVM_ARM_VCPU_HAS_EL2` and reads
   the counters back after a second. On 7.3.3 with fcvm's host patches and `kvm-arm.mode=nested`
   (`probe/expected.txt`): `CNTVOFF_EL2` reads zero after the two counter writes; with
   `CNTVOFF_EL2 = 0` both counters read their saved values plus the sleep, not the host's
   counter, which was 10,000 s away; with `CNTVOFF_EL2 = X` each counter reads X behind its
   saved value. That is the model's rule, observed, for one vCPU read through
   `KVM_GET_ONE_REG`. Not run on a v6.18.3 host, where the code is the same by reading. What
   reading and this probe cannot say is why restore-hit boots were measured slow before the
   fix: `REPAIRS.md`, 7.
2. **List registers are not modelled.** A guest acknowledge or EOI changes KVM's `struct
   vgic_irq` at once in the model. In the kernel it changes a list register, folded back at the
   next exit (`vgic_v3_fold_lr`, vgic-v3.c:74-136). That is the same thing for a save only
   because every exit folds before userspace runs. fcvm's host patch
   `kernel/patches-arm64-host/vgic-v3-keep-hcr-on-quiet-exits.patch` skips the fold when no list
   register is in use and nothing is left past them; the faithfulness review read that as I do
   (nothing is left unfolded). The model cannot check it.
3. **A guest hypervisor is modelled only where this model has rules for it.** See "What `nv`
   covers" below.
4. **What the virtual CPU interface withholds.** `deliverable` adds the group enable and the
   active priority to KVM's own test. That half is the GIC architecture (IHI 0069, running
   priority), not kernel source. Observed for the active priority: `BREAKS.md`, 1.
5. **A CPU taken offline has its SGI and timer PPI disabled and its timer off.** The first is
   observed on one guest kernel (#2155, comment 7174817511); the second is read
   (drivers/clocksource/arm_arch_timer.c, the CPU-dying callback). Neither is read for every
   path.
6. **What Cloud Hypervisor's and Firecracker's device models do at a save and a resume** is
   read for the virtio transport and the vsock device only (`Today.lean`). Another device that
   wrote to KVM on those paths would be a rule this model does not have.

## Build

```sh
make verify                               # check-imports, lake build, the searches and the mutants against their expected output
make verify-resume-search                 # lake exe resume-breaks: the summary
make verify-resume-search RESUME_ARGS=traces    # every shortest run, in words and as a Lean term
make verify-resume-search RESUME_ARGS=spins     # every kind of state with #2155's signature
make verify-resume-search RESUME_ARGS=mutants   # every mutant of the designs; exits 1 if one has no break
make verify-counter-probe                 # probe/: what a counter write does on this host's KVM (arm64, kvm-arm.mode=nested)
```

The search is 2,626,361 states over eight configurations (7 s); the mutants take 22 s. The
guest-step lemmas (`Guest0.lean`, `Guest1.lean`) are most of the library's build time, about
90 s each, side by side.

## The files

| File | What it holds |
|---|---|
| `Spec.lean` | The state, the guest's steps, KVM's rules with file and line, the save, the restore one request at a time (`Op`, `apply`), what a VMM writes to a paused source (`SrcOp`, `srcApply`), `step`, `Reachable`. `Rules` is the rule books' type, `Cfg` what exists around the guest |
| `Labels.lean` | The list of steps the search expands, and `labels_complete`: none is left out |
| `Today.lean` | `cloudHypervisor` (a354afd77), `cloudHypervisorRepin` (the fork's fix branch, 4c327e57), `cloudHypervisorUpstream` (upstream main at 4969a51a3d3f, `clock_restore_mode` preserve or advance), `forkBefore630` (ba79650e), `forkDefault` (2a33289b55b7), `forkNested` (dbea5779fb37) |
| `Design.lean` | `Design.cloudHypervisor`, `Design.firecracker`: each VMM with the five repairs |
| `Mutants.lean` | 27 mutants of the designs: each repair put back, and things a VMM must not do |
| `Properties.lean` | R1 to R6 as tests on a state |
| `Configs.lean` | The eight configurations the search runs |
| `Breaks.lean` | 59 shortest runs under today's rule books, each replayed and checked by `decide`, and the same run under the design. Generated by `../gen-resume-breaks.py` |
| `MutantBreaks.lean` | For each mutant a shortest run to a break, checked by `decide`, and from it that the mutant is not `Sound`. Generated by `../gen-resume-breaks.py mutants` |
| `Invariant.lean` | `Sound` (what the proofs need of a rule book), `Quiet`, the invariant |
| `Guest0.lean`, `Guest1.lean` | Every guest step keeps the invariant, per vCPU |
| `Lemmas.lean` | The timer, the wake-up, the device, what a VMM writes to a source, and `roundTrip` |
| `Proofs.lean` | `step_inv`, `reachable_inv`, `sound_*`, `design_*`, which field of `Sound` each of today's rule books fails, `not_sound_of_run`, R5's witnesses, upstream's clock modes |
| `Axioms.lean` | Fails the build on any axiom beyond `propext` and `Quot.sound`, by module |
| `search.expected`, `mutants.expected` | The search's two summaries; CI compares both |
| `../ResumeSearch.lean` | `lake exe resume-breaks` |
| `../probe/` | `counter-replay.c`: a measurement of the host kernel's counter writes, with its expected output |

## The requirements

A vCPU is **idle** when its guest CPU is in the idle loop. "The clone" is a VM restored from an
image; "the source" is the VM the image was taken from.

| | In words | Test (`Properties.lean`) |
|---|---|---|
| R1 | No lost wake-up. An IPI sent and not yet handled at the pause is pending or active in the clone. A device event not yet handled has its interrupt pending or active. A tick the guest is owed still has its timer armed, and an armed timer can fire | `lostIpi`, `lostDeviceIrq`, `lostTick`, `timerCannotFire` |
| R2 | No stuck interrupt. No interrupt is active on a CPU that is not in its handler. No active priority is set on a CPU that is in no handler. No enable the guest set is lost, nothing it disabled is enabled, no SGI is pending that nobody sent. A nested vCPU that a guest hypervisor has loaded keeps its priority mask | `stuckActive`, `stuckPriority`, `lostEnable`, `strayEnable`, `strayPending`, `lostNestedMask` |
| R3 | No spin. No vCPU is idle with `kvm_arch_vcpu_runnable` true and no interrupt that can be signalled to it (`spins`). No vCPU thread is blocked, or stopped, with an enabled and expired timer that cannot wake it, which KVM's background timer turns into a loop (`timerLoops`). Either is a full host CPU and a guest CPU with no tick: #2155's signature | `spins`, `timerLoops` |
| R4 | Time coherence. Whenever the guest runs, its counter is the time it has run: nothing a pause, a save or a restore took is in it | `clockOff`; `clockOffClone` is the same in a VM that was restored and not paused since |
| R5 | The source is undisturbed. Nothing the VMM asks of KVM between a pause and a resume changes a vCPU's interrupt, timer or power state, or takes something from the device's interrupt | `sourceDisturbed` |
| R6 | A CPU the guest has on is not stopped, one it has off is | `powerWrong` |

`sourceKicked` is reported and is not a requirement: the VMM left the device's interrupt
pending on the source with no event behind it. Cloud Hypervisor does that at every resume, on
purpose, and a virtio driver's handler tolerates it.

**R5 used to be vacuous.** Until the review of c58a2d263 no field of `Rules` reached a paused
VM, so `sourceDisturbed` was false under every rule book whatever it contained. `Rules` now has
`onSave` and `onResume`, the requests a VMM makes that write to a paused source, read from both
VMMs (`Today.lean`), and R5 is that they take nothing (`undisturbed`). No rule book of today
breaks it: what today's VMMs write is a device event with its interrupt (Firecracker's vsock
reset at the save, #799) or an interrupt with nothing behind it. Four mutants do
(`r5_fails_saveStops`, `r5_fails_saveInits`, `r5_fails_saveUnpends`, `r5_fails_resumeStops`).

## Results

`lake exe resume-breaks`, shortest run to each break (`@n` is its length in steps; where
configurations differ the shortest is given). NV2 configurations run only the fork's rule
books.

| Rule book | R1 | R2 | R3 | R4 | R5 | R6 |
|---|---|---|---|---|---|---|
| `cloud-hypervisor` (a354afd77) | lostIpi@5; lostDeviceIrq@5 (level device only) | stuckActive@6, stuckPriority@6, strayPending@5, strayEnable@4 | spins@8, timerLoops@11 | clockOff@3 | proved | holds |
| `cloud-hypervisor-repin` (fix branch) | lostDeviceIrq@5 (level device only) | holds | holds | clockOff@3 | proved | holds |
| `cloud-hypervisor-upstream-preserve` | lostDeviceIrq@5 (level device only) | holds | holds | source resumed in place: clockOff@3. Clone: right, unless its source's clock had moved (@7) | proved | holds |
| `cloud-hypervisor-upstream-advance` | lostDeviceIrq@5 (level device only) | holds | holds | source: clockOff@3. Clone: clockOffClone@5, ahead by the host time since the pause | proved | holds |
| `fork-default` | lostDeviceIrq@4 (level device only) | strayEnable@4 | holds | clockOff@3 | proved [b] | holds |
| `fork-before-630` (NV2) [a] | timerCannotFire@6 (`CNTVOFF_EL2` not zero) | lostNestedMask@5 | holds | clockOff@3 (gaps), @4 (pause in place), @5 (`CNTVOFF_EL2` not zero) | proved [b] | holds |
| `fork-nested` (NV2) [a] | holds | lostNestedMask@5 | holds | clockOff@5, only by the two gaps of `BREAKS.md`, 6 | proved [b] | holds |
| `design-cloud-hypervisor` | proved | proved | proved | proved | proved | proved |
| `design-firecracker` (also NV2 [a]) | proved | proved | proved | proved | proved [b] | proved |

"holds" is the search finding no reachable state that breaks it in the eight configurations.
"proved" is a theorem for every configuration and runs of any length; for today's rule books
that is R5 alone (`today_r5_*`, `Proofs.lean`). `strayEnable` is not searched for the NV2 rule
books: no NV2 configuration has an offline CPU.

[a] For a guest hypervisor every cell is about the model's guest hypervisor, which is an
ordinary guest with its tick on the physical counter and one mask of its nested guest: see
"What `nv` covers". R1 to R3 say nothing of its other interrupt state.

[b] R5 is about a vCPU's interrupt, timer and power state and the device's interrupt.
Firecracker's save does change the source at the device: it resets the source's vsock
connections (#799, `BREAKS.md`, 9), which this model does not have.

The search reproduces #630 on the fork before its fix (`breaks_before630_nvDump_clockOff`,
`breaks_before630_nvL2_clockOff`) and not on the nested pin.

## What `nv` covers

`Cfg.nv` makes the guest a hypervisor in exactly two respects:

- its tick is on the physical counter offset, and a user write of `CNTVCT_EL0` lands there
  (`apply .cntp`, `.cntv`, with `CNTVOFF_EL2` as `Vcpu.l2`);
- while it has a nested vCPU loaded, KVM holds that vCPU's priority mask in the guest
  hypervisor's `ICH_VMCR_EL2` (`Vcpu.ichv`), outside guest memory and outside the register
  list.

So for a guest hypervisor the theorems and the search are about its counter (R1's timer tests
and R4) and about that one mask (`lostNestedMask`). They are not about the rest of its
interrupt state:

- it runs with EOImode 1 (drivers/irqchip/irq-gic-v3.c), where priority drop and deactivation
  are separate, so an interrupt can be active with no active priority and `stuckActive` as
  defined is a normal state there;
- the nested guest's interrupts in flight live in `ICH_LRn_EL2`, and its active priorities in
  `ICH_APxRn_EL2`, which the model does not have (the fork does not save those either:
  `BREAKS.md`, 8);
- its EL1 timers are emulated from the same page.

## What is proved, searched, read, observed and assumed

**Proved** (Lean's kernel, no `sorry`, axioms `propext` and `Quot.sound`; the sweep allows no
other):

- `design_cloudHypervisor`, `design_firecracker`: for every `Cfg` and every state reachable
  under the design, every test of R1 to R6 is false (`healthy C s = true`).
- `sound_healthy` and the fifteen other `sound_*` theorems: the same for any rule book that is `Sound`.
- `Design.cloudHypervisor_sound`, `Design.firecracker_sound`: both designs are `Sound`. That
  includes that their restore orders, different as they are, build exactly the VM the image
  describes (`Sound.builds`), and that what each writes to a paused source takes nothing
  (`quiet_kick`, `quiet_devEvent`). `Design.cloudHypervisorOffsetLast_sound`: so is the design
  with `KVM_ARM_SET_COUNTER_OFFSET` last.
- `roundTrip`: for a `Sound` rule book and any VM whose SGIs have no line (they are
  edge-triggered and never have one), the VM described by its save is that VM, field for
  field, with its threads out of `KVM_RUN`, its offset set by the ioctl and its counter the
  image's. Every enable, latch, line, active bit, timer register, guest position and device
  state is carried, whatever their values: the invariant is not a hypothesis.
- `today_r5_*` (six): R5 for each of today's rule books, for every configuration and run.
  What they write to a paused source (`[]`, a kick, a device event) leaves every VM
  undisturbed (`harmless_*`), with no invariant needed.
- `mutant_*_not_sound` (27): each mutant of `Mutants.lean` is not `Sound`, from a run to a
  state it reaches (`not_sound_of_run`), not from its flag.
- `cloudHypervisor_not_sound` and the like: each of today's rule books fails a named field of
  `Sound`.
- Each of the 59 runs of `Breaks.lean` and the 27 of `MutantBreaks.lean`: possible under the
  rule book named, and the test true at its end (`decide`). `break_reachable` turns any of them
  into a reachable state.
- `labels_complete`: the search expands every step there is.
- `upstream_replay_order`, `upstream_replay_interleaved`: upstream Cloud Hypervisor replays
  the vCPUs' states on threads; the two sequential orders and two interleavings build the same
  VM. Not every interleaving is stated.
- `upstreamAdvance_oldSnapshot`, `upstreamPreserve_oldSnapshot`, `upstream_resumeInPlace`: what
  upstream's two clock modes do on one run (`BREAKS.md`, 4).

The proof is an inductive invariant (`Inv`, `Invariant.lean`), not the search: each vCPU's
interrupt state agrees with where its guest CPU is, the device's with the device, the counter
with the phase, nothing the VMM asked of a paused source disturbed it, and an image is the save
of the paused VM it was taken from.

**The universe the theorems quantify over, exactly:**

- two vCPUs;
- per vCPU one timer PPI and one SGI; one SPI, routed to vCPU 0;
- one cross-call outstanding per target CPU, one device event at a time, one tick owed per CPU:
  a timer is armed one unit ahead and only when it is not armed;
- one interrupt priority;
- one image at a time: a new save replaces it, and a resume or a restore drops it;
- the guest never changes an enable, its priority mask or its group enable: the invariant pins
  `pmr`, `grp`, `gicEn` and the SPI's enable to true and the PPI's and SGI's to "the CPU is
  on". R2's enable tests are exercised by that one static case; `roundTrip`, which does not
  assume the invariant, covers any enable
  state;
- guest CPU 1 is on or offline for the whole run (`Cfg.cpu1Off`): there is no PSCI step;
- `CNTVOFF_EL2` goes from zero to non-zero and not back;
- any `Cfg`: any number of snapshots, restores, restores of restores, any amount of guest time,
  host time passing while paused in any of the three places, an ordinary guest or a guest
  hypervisor as far as "What `nv` covers" says, an edge or a level device, guest CPU 1 on or
  offline.

**Searched, not proved:** that today's rule books have no break beyond those in the table, in
the eight configurations of `Configs.lean` (two snapshots and two restores in `plain`, one each
elsewhere; guest time 1 unit; host time 1 or 2 units). The search program is compiled Lean and
is not itself verified. A line of `search.expected` holds the first state found per test, so a
second cause of a test already broken in that cell shows only in the state count. CI runs the
search and the mutants and compares both with their expected files.

**Read** (in the source at the named commit, not run): every rule of `Spec.lean` and
`Today.lean`. Kernel paths are under arch/arm64/kvm/ at v7.3.3. The faithfulness review of
c58a2d263 checked each rule against the source and found them faithful apart from what this
commit changes.

**Observed:**

- on a box, by the session that reproduces #2155 (a stock 7.0.0-1018-aws host, not the fcvm
  host kernel): the first rule of `Today.cloudHypervisor`, and its consequence by injection.
  Scope in `BREAKS.md`, 1;
- by `probe/counter-replay.c` on an m8gd.metal-24xl with kernel 7.3.3 and fcvm's host patches,
  `kvm-arm.mode=nested`: `apply .cntp`, `.cntv` and `.offset`, for both kinds of guest, on one
  vCPU read through `KVM_GET_ONE_REG` ("Least sure", 1). Not what a guest reads while it runs,
  and not "the last vCPU replayed decides", which are read;
- by the session that moves fcvm to upstream Cloud Hypervisor (arm64, stock kernel): a source's
  uptime read 4.43 s before a snapshot and 5.16 s after one that took 0.73 s (the clock runs
  through the pause), and a clone restored 20 s later with `preserve` read 4.51 s.

**Assumed:**

| Assumption | Why |
|---|---|
| A save and a restore are atomic: nothing else happens between two of their requests | Both VMMs wait for every vCPU thread to be out of `KVM_RUN` before the first save (Cloud Hypervisor vmm/src/cpu.rs:2623-2655; the fork's `pause_vcpus`), KVM refuses the vGIC groups while any vCPU is in an ioctl (vgic-kvm-device.c:569-574), a vCPU out of `KVM_RUN` has no timer armed in the host (`kvm_timer_vcpu_put`, arch_timer.c:924-960), and both pause their devices before the save. Host time is modelled as passing only between the VMM's calls |
| KVM's list-register handling is correct | "Least sure", 2 |
| Guest registers and memory are carried exactly | True of what is guest memory or in `KVM_GET_REG_LIST`. Not true of a guest hypervisor's `ICH_*_EL2` registers, which are neither: modelled as `ichv` and nothing more (`BREAKS.md`, 8) |
| The timer's level is recomputed at every vCPU load | `kvm_timer_vcpu_load_gic`, arch_timer.c:686-707: read |
| A device interrupt with no event behind it does the guest no harm (`sourceKicked`) | Cloud Hypervisor raises one per virtio queue at every resume, pinned and upstream. A virtio driver's handler that finds nothing returns. Linux's accounting of unhandled interrupts (kernel/irq/spurious.c) not tripping on one per resume is from memory, not read |
| A blocked vCPU runs again exactly when `kvm_arch_vcpu_runnable` holds | As in the wake-up model. What KVM's background timer does to a thread that cannot be woken is `timerLoops` |

## Each rule and its source

Host KVM, Linux v7.3.3, read:

| Rule (`Spec.lean`) | Source |
|---|---|
| `pend` | `irq_is_pending`, vgic/vgic.h; PPIs level, SGIs edge, fixed: vgic/vgic-mmio.c:733-764, vgic/vgic-init.c:276-286 |
| `kvmPending` | `kvm_vgic_vcpu_pending_irq`, vgic/vgic.c:1242-1277; `kvm_arch_vcpu_runnable`, arm.c:849-859; WFI: arm.c:1065-1089, handle_exit.c:130-178 |
| `deliverable` | the above plus the group enable and the running priority (GIC architecture) |
| `timerDue`, `fire` | `kvm_timer_pending`, arch_timer.c:366-406; `kvm_timer_update_irq`, 457-483; `kvm_bg_timer_expire`, 316-338 |
| `timerLoops` (`Properties.lean`) | `kvm_timer_earliest_exp`, arch_timer.c:293-313; `kvm_timer_blocking`, 595-615; `kvm_bg_timer_expire`, 316-338; `kvm_timer_vcpu_put`, 924-960 |
| `enter` | `kvm_timer_vcpu_load_gic`, arch_timer.c:686-707 |
| `save`: latch, active, line | vgic/vgic-mmio.c:229-296, 488-534, 766-786; addressing: vgic/vgic-kvm-device.c:484-509 |
| `Vcpu.ichv` | `kvm_vcpu_init_nested`, nested.c:83-125; hidden from the register list: sys_regs.c:2760-2761, 3919-3949; reached through the vGIC device: vgic-sys-reg-v3.c:299-311, 366-443; written back on each nested exit: vgic/vgic-v3-nested.c:278-318 |
| `apply .create` | `kvm_timer_vcpu_init`, arch_timer.c:1105-1122; `vgic_v3_reset`, vgic/vgic-v3.c:474-504 |
| `apply .offset` | `kvm_vm_ioctl_set_counter_offset`, arch_timer.c:1731-1764: both offsets, and every later counter write is ignored (sys_regs.c:1791-1796) |
| `apply .cntp`, `.cntv` | `arch_timer_set_user`, sys_regs.c:1779-1802; NV2 wiring: arch_timer.c:1048-1053, include/kvm/arm_arch_timer.h:171-196 |
| `apply .mp`, `SrcOp.mpStop` | `kvm_arch_vcpu_ioctl_set_mpstate`, arm.c:816-840 |
| `apply .dist`, `.redist`: `clearSet` | vgic/vgic-mmio.c:187-228, 593-668; ISPENDR: vgic/vgic-mmio-v3.c:383-394; ICPENDR ignored: 649-652, 733-736 |
| `apply .icc` | `vgic_v3_cpu_sysregs_uaccess`, vgic-sys-reg-v3.c |
| `apply .level` | `vgic_write_irq_line_level_info`, vgic/vgic-mmio.c:788-820 |
| `devRaise`, `SrcOp.kick` | `vgic_validate_injection`, `kvm_vgic_inject_irq`, vgic/vgic.c:379-392, 533-572 |

The VMMs' rules, with their lines, are the doc comments of `Today.lean`. fcvm itself issues no
KVM state ioctl: it calls pause, snapshot, resume and load (`src/commands/common.rs`,
`src/hypervisor/`), which are the model's `pause`, `save`, `resume` and `restore`.

## Each repair is carried by a proof and by the search

`Mutants.lean` holds 27 mutants of the two designs. For each, `lake exe resume-breaks mutants`
finds a break (it exits 1 if one has none, and CI compares its output with
`mutants.expected`), and `MutantBreaks.lean` has a shortest run checked by `decide` and the
theorem that the mutant is not `Sound`, derived from the state that run reaches. Both run on
every build.

| Mutant | Breaks |
|---|---|
| slots read from vCPU 0 until a restore | R1 lostIpi, R2 stuckActive, stuckPriority, strayPending, R3 spins, timerLoops |
| image holds the counter of the save; load to resume not taken out | R4 in `lags`, `nv-lags` |
| resume does not take out the pause | R4 in `dump`, `lags`, `nv-dump`, `nv-lags` |
| restore adds the wall time since the pause | R4 clockOffClone |
| no `KVM_ARM_SET_COUNTER_OFFSET` on restore | R1 timerCannotFire, R4 in `nv-l2` |
| no offset, and the last vCPU's counter not replayed | R1 timerCannotFire, R4 everywhere |
| lines not saved; lines not restored | R1 lostDeviceIrq in `level` |
| clear-registers written with the saved bits | R2 strayEnable in `offline` |
| nested CPU interface not saved | R2 lostNestedMask in `nv-l2` |
| `KVM_ARM_VCPU_INIT` after the register replay | R1 lostTick |
| vCPU 1's registers not replayed | R1 lostTick, R2 stuckActive, stuckPriority, R3 spins |
| vCPU 1's CPU interface not restored | R2 lostEnable, R3 timerLoops |
| vCPU 1's `mp_state` not restored | R6 powerWrong, R3 timerLoops |
| the save stops vCPU 1; the resume stops vCPU 1 | R5, R6, R3 timerLoops |
| the save re-initialises vCPU 0 | R5, R1 lostTick |
| the save writes vCPU 1's `GICR_ISPENDR0` with zero | R5, R1 lostIpi |

Two ways the witnesses of `Sound` relate to that table:

- For the flag fields (`ownSlots`, `cntAtPause`, `freeze`, `freezeLoaded`, `noAdvance`,
  `levelInfo`, `nestedIcc`) a mutant fails the design's witness by definition: the field is the
  flag. The evidence that the repair is needed is the mutant's reachable break. The proofs
  reviewer also flipped each field's required value in `Sound` together with the flag in both
  designs and saw the build fail where the field is used: `ownSlots`, `levelInfo` and
  `nestedIcc` in the round trip, `cntAtPause` in `save_since`, `freeze` and `freezeLoaded` in
  `resume_ctr`, `noAdvance` in the restore case of `step_inv`. That experiment is not in CI.
- `builds`, `quietSave` and `quietResume` are semantic: a mutant fails them because its restore
  does not build the described VM, or its request changes the source.

## What is not modelled

- **LPIs and the ITS.** Cloud Hypervisor's virtio interrupts are LPIs. Their pending state is
  saved by `KVM_DEV_ARM_VGIC_SAVE_PENDING_TABLES` into each vCPU's pending table, whose address
  is `GICR_PENDBASER`. At a354afd77 that register is among those read from vCPU 0 for every
  vCPU, so a pending LPI for another vCPU is lost on a restore (`BREAKS.md`, 1). On the fix
  branch, upstream and in the fork each vCPU's own is saved, and the restore order
  (`GICR_PROPBASER`, `GICR_PENDBASER`, `GICR_CTLR`, the ITS tables, `GITS_CTLR`) is the one the
  kernel's document asks. Read only.
- **List registers, the maintenance interrupt, more interrupts than list registers.**
- **Priorities**, preemption, group 0, pseudo-NMIs. A guest with pseudo-NMIs masks interrupts
  with `ICC_PMR_EL1`: under break 1 a vCPU can then get vCPU 0's masked PMR, KVM sees nothing
  pending and the vCPU sleeps for ever, a different signature from #2155.
- **A guest hypervisor beyond its counter and one mask** ("What `nv` covers").
- **An SPI whose affinity the guest moves between its acknowledge and its EOI.** KVM keeps an
  active interrupt on the vCPU that acknowledged it (`vgic_target_oracle`, vgic.c:264-290),
  which no user interface exposes; a restore queues it on the vCPU `GICD_IROUTER` names.
  Narrow, and neither VMM can repair it.
- **A guest write of `ISPENDR`.** With none, a latch on the level-triggered timer interrupt is
  not reachable at all, and `strayPending` tests SGIs only.
- **A timer rearmed inside its handler**, which Linux does before the EOI.
- **The physical active state of the mapped timer interrupt.** A user write of the active bit
  of a mapped interrupt is dropped (vgic-mmio.c:537-545); the timer's is mapped at the vCPU's
  first `KVM_RUN`, after every restore here.
- **What a moved clock does to a guest hypervisor** (the storm of #630): R4 flags the moved
  clock, not its consequence.
- **`KVM_SET_VCPU_EVENTS`** (a pending SError): neither VMM saves it on arm64.
- **More than two vCPUs.** KVM's MPIDR is not the vCPU index above 15; the fix branch's and
  upstream's `mpidr_from_vcpu_id` was not read.
- **The width of the CPU-interface registers** (`BREAKS.md`, 7).
- **Device state**: Firecracker's save resets the source's vsock connections (#799). The model
  has the interrupt of that event and nothing of the connections.

## CI

`.github/workflows/verify.yml`, job `Lean`: `./check-imports.sh`, `lake build`, then `lake exe
resume-breaks | diff ResumeVerify/search.expected -` and `lake exe resume-breaks mutants | diff
ResumeVerify/mutants.expected -` beside the boundary model's checks, and both generated files
against the generator (`lake exe resume-breaks traces | ./gen-resume-breaks.py | diff
ResumeVerify/Breaks.lean -`, and the same with `mutants`), so a break or a mutant added to the
search has its theorem. `tests/test_ci_workflow_coverage.rs` pins those lines. `make verify`
runs the same comparisons under `pipefail`.

`check-imports.sh` also fails when a library or executable that `lakefile.toml` declares is not
in `defaultTargets`: `lake build` builds only those, and with `ResumeVerify` dropped from the
list a `sorry` in `Proofs.lean` passed every check. The same held for the other two models,
whose libraries are in the same list. It also requires each library to be declared with
`globs = ["<Lib>.*"]`: with a list of named modules in its place, a file left off the list is
not built.

The probe is not in CI: it measures an arm64 host's KVM and needs `kvm-arm.mode=nested`.
