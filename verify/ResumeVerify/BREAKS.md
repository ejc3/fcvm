# ResumeVerify: the breaks

Every break `lake exe resume-breaks` finds under today's rule books, grouped by cause. Each run
is in `Breaks.lean` as `breaks_<book>_<configuration>_<test>`, checked by `decide`.

Classes: **real, reachable** under fcvm's use; **real, unreachable** there, and why;
**artefact** of the model. No artefact is known at this commit; "Least sure" in the README lists
where one could be.

| # | Cause | Rule books | Tests | Class | Severity |
|---|---|---|---|---|---|
| 1 | The save reads vCPU 0's redistributor and CPU interface for every vCPU of a booted VM | `cloud-hypervisor` | R1 lostIpi, R2 stuckActive, stuckPriority, strayPending, strayEnable, R3 spins, timerLoops; a lost LPI (read) | real, reachable | high: a clone's vCPU that never takes an interrupt again |
| 2 | Clear-registers written with the saved bits | `cloud-hypervisor`, every fork pin | R2 strayEnable | real, reachable | low |
| 3 | No `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` | all of today's | R1 lostDeviceIrq | real, unreachable: no device of either VMM under fcvm holds a level line | none today |
| 4 | Nothing owns the counter offset: a pause in place moves the clock | every `cloud-hypervisor` book, `fork-default`, `fork-before-630` | R4 clockOff | real, reachable | low for an ordinary guest; #630 for a guest hypervisor |
| 4a | `clock_restore_mode=advance`: a restore adds the wall time since the pause | `cloud-hypervisor-upstream-advance` | R4 clockOffClone | real; unreachable if fcvm passes `preserve` | by design upstream; every armed timer due at once in an old snapshot's clone |
| 5 | `CNTVCT_EL0` replayed into the physical offset with `CNTVOFF_EL2` not zero | `fork-before-630` | R4 clockOff, R1 timerCannotFire | real, unreachable today: the nested pin sets the offset first | was high |
| 6 | The image holds the counter of the save; load to resume is not taken out | `fork-nested` (and every book of 4 but upstream Cloud Hypervisor) | R4 clockOff | real, reachable, bounded by fcvm's own latency | low |
| 7 | CPU-interface registers passed as 32 bits | `cloud-hypervisor` | not modelled | real, reachable | no value changed on the write side; an overrun of four bytes on every read |
| 8 | A guest hypervisor's `ICH_*_EL2` registers are not in the image | every fork pin | R2 lostNestedMask; lost nested interrupts (read) | real, reachable: a nested-profile VM snapshotted while an inner VM runs | high for that inner VM: a nested vCPU that takes no interrupt again |

Not breaks, reported by the search as `-- sourceKicked`: what today's VMMs write to a paused
source. Section 9.

## 1. Cloud Hypervisor saves vCPU 0's interrupt state for every vCPU of a booted VM

**In the fork's pinned commit a354afd77.** Fixed upstream in 18366e1b7 (with efa34d789), in
v54.0 and later. The save-side call was removed upstream by def1d7cf8 (2022-11-30, "vmm: Remove
GICR typers in snapshot on AArch64"). The fork's fix branch `fcvm-v52-gic-identities` (4c327e57)
carries the upstream fix; `Today.cloudHypervisorRepin` and `Today.cloudHypervisorUpstream`
model the fixed code and have none of these breaks.

**The rule** (read):

- The vGIC is created with `gicr_typers: vec![0; vcpu_count]`
  (hypervisor/src/kvm/aarch64/gic/mod.rs:274).
- `state()` passes that list to `get_redist_regs` and `get_icc_regs` (mod.rs:334-343), which
  put `typer & MPIDR_MASK` in each request (redist_regs.rs:114-122, icc_regs.rs:99-107).
- KVM resolves MPIDR 0 to vCPU 0 (v7.3.3 vgic-kvm-device.c:484-509, arm.c:2931-2960).
- The only caller of `set_gicr_typers` is the restore (devices/src/gic.rs:66-71, from
  vmm/src/device_manager.rs:1809-1813). A VM that was itself restored saves each vCPU's own.

So the image of a booted VM holds, once per vCPU, vCPU 0's:

- `GICR_ISENABLER0`, `ISPENDR0`, `ISACTIVER0`, `IPRIORITYR0` (the SGIs and PPIs);
- `ICC_PMR`, `IGRPEN0/1`, `BPR0/1`, `AP0R0`, `AP1R0` (the CPU interface);
- `GICR_PROPBASER` and `GICR_PENDBASER` (`VGIC_RDIST_REGS`, redist_regs.rs:78-84), the second
  being the address of the vCPU's LPI pending table.

A restore writes them into every vCPU.

**Observed** (#2155, comment 7174817511; a stock 7.0.0-1018-aws host, not the fcvm host
kernel), with this scope:

- The save defect, deterministic. A booted 2-vCPU VM with guest CPU 1 taken offline: KVM's
  debugfs vgic-state at the pause shows vCPU 1's SGI 0, PPI 23 and PPI 27 disabled, while the
  state file has `ISENABLER0 = 0x88000ff` for both vCPUs, the halves byte-identical. The same
  steps on a VM that was itself restored give `0x880ffff` and `0xff00`. About 310 snapshots of
  booted VMs: halves always identical.
- The consequence, by injection. One such state file edited so that vCPU 1's `ICC_AP1R0` is
  `0x1000000`: 3 of 3 restores have vCPU 1's thread at 100% of a host CPU from the first second
  and guest CPU 1's timer interrupt count frozen; 2 of 2 restores of the unedited file healthy.
- Not observed: a pause that catches vCPU 0 inside a handler without help (about 620 pauses
  under two loads found none). **That the one hang of #2155 was this defect is inferred.**

**What the search returns** (`plain`, `cloud-hypervisor`):

| Test | Shortest run | What it is |
|---|---|---|
| R3 spins@8 | CPU 1 sends an IPI to CPU 0 and goes idle; the device raises its interrupt; CPU 0 acknowledges it; pause, save, restore, resume | vCPU 1 has vCPU 0's active-priority bit and no handler to drop it. Its own SGI (vCPU 0's pending one, copied) is pending and enabled, so `kvm_arch_vcpu_runnable` is true and its thread cannot block; nothing can be signalled. #2155's signature |
| R3 timerLoops@11 | both CPUs arm their timers; CPU 1 goes idle; CPU 0's timer fires and CPU 0 acknowledges it; pause, save, restore, resume; CPU 1 executes WFI | vCPU 1 has vCPU 0's active timer PPI. Nothing is pending for KVM, so its thread blocks; its own timer is due and cannot be made pending, so KVM's background timer wakes the thread again and again. A full host CPU with the thread never in the guest: #2155's signature again |
| R2 stuckPriority@6 | the device raises its interrupt; CPU 0 acknowledges it; pause, save, restore, resume | the first state before anything is pending for vCPU 1 |
| R2 stuckActive@6 | CPU 1 sends an IPI; CPU 0 acknowledges it; pause, save, restore, resume | vCPU 1 has the SGI active with no handler: that SGI is never delivered to it again |
| R1 lostIpi@5 | CPU 0 sends an IPI to CPU 1; pause, save, restore, resume | vCPU 1's pending SGI is not in the image (vCPU 0's, not pending, is). The sender waits in `csd_lock_wait` for ever: vCPU 0 spins in the guest, vCPU 1 is healthy |
| R2 strayPending@5 | CPU 1 sends an IPI to CPU 0; pause, save, restore, resume | vCPU 0's pending SGI is delivered to vCPU 1 as well: an IPI nobody sent it. Linux's IPI handlers tolerate that |
| R2 strayEnable@4 (`offline`) | pause, save, restore, resume, guest CPU 1 offline | vCPU 1 gets vCPU 0's enables |

**A pending LPI for a vCPU other than 0 is lost** (read, not modelled: the model has no LPIs).
Cloud Hypervisor's virtio interrupts are LPIs. `KVM_DEV_ARM_VGIC_SAVE_PENDING_TABLES` writes
each pending LPI's bit into its target vCPU's table (v7.3.3 vgic-v3.c:592-680), and the ITS
restore reads it back through the target vCPU's `GICR_PENDBASER`
(`vgic_v3_lpi_sync_pending_status`, vgic-v3.c:520-553). With vCPU 0's address restored into
every vCPU, an LPI that was pending for vCPU 1 is read from vCPU 0's table, where its bit is
clear. What a capture shows is not a spin: a virtio queue whose interrupt was routed to CPU 1
stops, with nothing else wrong. In the image, both vCPUs' `GICR_PENDBASER` are equal, which
no guest writes.

**Every state with #2155's signature** (`lake exe resume-breaks spins`): seven kinds, all on
vCPU 1. `idle_signalled` (`Proofs.lean`) is the general fact behind the first five: a vCPU
that KVM counts an interrupt pending for can be signalled unless an active priority is set or
its group is off.

| vCPU 0 at the pause | vCPU 1 in the clone | Its thread |
|---|---|---|
| in a device handler | `AP1R0` set; its timer PPI, an SGI, or both pending and enabled, never active (three kinds) | in the guest, WFI returns at once |
| in its timer handler | `AP1R0` set; timer PPI 27 active for ever; an SGI pending | in the guest, WFI returns at once |
| in an IPI handler | `AP1R0` set; that SGI active for ever; the timer PPI pending | in the guest, WFI returns at once |
| in its timer handler | `AP1R0` set; timer PPI 27 active for ever; nothing pending; its own timer due | blocked, woken by KVM's background timer without end |
| in its timer handler, vCPU 1 in a handler of its own | `AP1R0` clear (vCPU 1's own EOI dropped the copied bit); timer PPI 27 active for ever; its own timer due | blocked, woken by KVM's background timer without end |

**What separates it from any other cause, in a capture:**

1. `ICC_AP1R0_EL1` of the stuck vCPU is non-zero while its PC is in the idle loop: six of the
   seven kinds. The seventh has `ISACTIVER0` bit 27 set instead, with the PC in the idle loop.
   Read both by `vm.pause` and `vm.snapshot` of the stuck clone: a restored VM saves each
   vCPU's own.
2. Needs no race: in a booted VM offline one guest CPU and compare the two halves of
   `ISENABLER0` in the state file. Identical halves are the defect.
3. In the source's state file, the `icc` array (9 values per vCPU: SRE, CTLR, IGRPEN0, IGRPEN1,
   PMR, BPR0, BPR1, AP0R0, AP1R0 with 5 priority bits): index 8 and 17 equal, and non-zero in
   an image whose restores hang.
4. Where the stuck vCPU's thread is: in the guest (exit counters climbing with WFI exits) for
   the first five kinds, in the kernel with no guest entry for the last two.

Excluded if: the image that hangs has `AP1R0`, `ISACTIVER0` and `ISPENDR0` all zero, or the hang
is seen on the first restore of an image taken from a restored VM.

## 2. Clear-registers written with the saved bits

**In Cloud Hypervisor a354afd77** (fixed upstream in 1ede69b5b, v55.0; on the fix branch as
4c327e57) **and in the Firecracker fork at every pin, and in upstream Firecracker at
2a8d0138cdf9 (main, 2026-11-25).** Read; the Firecracker side not yet observed. #2172.

- Both VMMs save `ICENABLER`, `ISENABLER`, `ICACTIVER`, `ISACTIVER` by reading them (a read of
  either register of a pair returns the set bits) and restore them by writing the saved value
  to each, the clear-register first: Cloud Hypervisor redist_regs.rs:87-97, 132-157,
  dist_regs.rs:67-79, 167-180; the fork at 2a33289b src/vmm/src/arch/aarch64/gic/regs.rs:116-129
  with the lists at gicv3/regs/redist_regs.rs:39-49 and dist_regs.rs:36-49.
- Writing the saved bits to a clear-register clears those bits, and the set-register then sets
  them again. A bit the new VM has set and the image has not is never cleared: the result is
  `current OR saved` (`clearSet`, `Spec.lean`).
- KVM enables every SGI of a new vCPU (v7.3.3 vgic-init.c:276-286). PPIs and SPIs start
  disabled and nothing starts active, so only SGI enables are affected.

**The run** (`offline`, 4 steps: pause, save, restore, resume, guest CPU 1 offline): vCPU 1's
SGI is enabled in the clone where the guest had disabled it.

**Prediction for a capture:** restore any arm64 snapshot and read `GICR_ISENABLER0`: SGIs 8 to
15 enabled where the source had `0x..00ff`, and every SGI of an offline CPU enabled.

**Why low.** Linux enables SGIs 0 to 7 on each online CPU and sends nothing on 8 to 15 (not
checked in the guest source). A pending SGI cannot wake a CPU the guest turned off through
PSCI: `kvm_arch_vcpu_runnable` requires the vCPU not to be stopped (arm.c:849-859), and Linux
sends IPIs only to online CPUs. When the CPU comes back, an SGI left pending for it would be
delivered once, as a spurious IPI, which the handlers tolerate. So no guest-visible effect is
known; it is the restored state differing from the saved one, and a guest that relies on a
disabled SGI would see it.

## 3. The line of a level-triggered interrupt is not in the image

Neither VMM requests `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO`. A user read of `ISPENDR` returns the
latch alone (vgic-mmio.c:229-296), so a level interrupt that is asserted and not yet
acknowledged at the pause is not pending in the clone (`level`, R1 lostDeviceIrq@5: the device
raises its interrupt; pause, save, restore, resume; @4 for the fork, whose save has the device
raise an event itself).

Unreachable under fcvm:

- Every device interrupt of the fork's device tree is `IRQ_TYPE_EDGE_RISING`
  (src/vmm/src/arch/aarch64/fdt.rs at 2a33289b, lines 297, 309, 414-416, 435-437).
- Cloud Hypervisor's one `IRQ_TYPE_LEVEL_HI` SPI is the PL031 RTC (arch/src/aarch64/fdt.rs:
  804-815), which raises no interrupt (a354afd77 vmm/src/device_manager.rs:2109; upstream
  devices/src/legacy/rtc_pl031.rs:9-10).
- The timer PPIs are level-triggered and are not affected: KVM recomputes their line at every
  vCPU load (arch_timer.c:686-707).

## 4. A pause in place moves the guest's clock

Cloud Hypervisor and the default fork never issue `KVM_ARM_SET_COUNTER_OFFSET`, and KVM's
offsets do not move while a vCPU is out of `KVM_RUN`, so the guest's counter runs through the
pause. A source resumed in place reads a counter ahead by the whole pause, and every timer that
came due fires at once (`dump`, R4 clockOff@4: pause, save, host time passes, resume). #2173.

What fcvm's users see today:

| VMM | A source resumed in place | A clone |
|---|---|---|
| Cloud Hypervisor (pinned and fix branch) | clock ahead by the pause | ahead by the gaps of 6 |
| Cloud Hypervisor upstream, `preserve` | clock ahead by the pause | as the pause left it |
| Cloud Hypervisor upstream, `advance` | clock ahead by the pause | ahead by the host time since the pause |
| default fork (2a33289b55b7) | clock ahead by the pause | ahead by the gaps of 6 |
| nested fork (dbea5779fb37) | clock as the pause left it | ahead by the gaps of 6 |

For an ordinary guest this is what every KVM pause does, and Linux tolerates it: the timers
fire once and `CLOCK_MONOTONIC` includes the pause. For a guest hypervisor it was #630: the fix
(0baf5b0b) is on the nested pin only, and was meant for NV2. `fork-before-630` reproduces it
(`nv-dump`, R4@4); `fork-nested` does not.

**Cloud Hypervisor upstream** (4969a51a3d3f, which fcvm is moving to; `Today.cloudHypervisorUpstream`):

- The counter is captured in `Vm::pause` as soon as the vCPUs are quiesced (vmm/src/vm.rs:
  2915-2918) and written once, through vCPU 0, at the resume of a restored VM (vm.rs:2877-2888,
  2946-2947; hypervisor/src/kvm/mod.rs:1257-1295). So both gaps of 6 are closed for a clone.
- `preserve` writes the saved value: the clone's clock is the one of the pause
  (`upstreamPreserve_oldSnapshot`). The search finds no break of R4 in a clone except one its
  source already had (`lags`, clockOffClone@7: the source was paused and resumed in place
  before the snapshot). Observed by the session that moves fcvm to it: a source's uptime read
  4.43 s before a snapshot and 5.16 s after one that took 0.73 s, and a clone restored 20 s
  later read 4.51 s.
- `advance`, the default, writes the saved value plus the wall time since the capture: the
  clone's counter is ahead by the whole time since the pause, and a timer armed before the
  pause is due the moment the clone runs (`upstreamAdvance_oldSnapshot`; `dump`,
  clockOffClone@5). That is the forward jump of #630's account, chosen on purpose so that the
  guest's clock follows wall time; fcvm passing `preserve` avoids it.
- A resume in place does nothing to the counter in either mode (`SameHostResume` returns at
  once, kvm/mod.rs:1257-1268): the source's clock is ahead by the pause
  (`upstream_resumeInPlace`), as with today's pin.
- Writing through vCPU 0 alone is sound for the other vCPUs because the virtual offset is the
  VM's: every vCPU's virtual timer context points at `kvm->arch.timer_data.voffset`
  (v7.3.3 arch_timer.c:1075-1090) and a user write of `KVM_REG_ARM_TIMER_CNT` stores into it
  (sys_regs.c:1790-1793, include/kvm/arm_arch_timer.h:188-196). Read at v7.3.3 and v6.18.3 and
  measured at 7.3.3 for one vCPU (`../probe`). It holds only while
  `KVM_ARM_SET_COUNTER_OFFSET` has not been used on the VM: after it the kernel ignores the
  write. It acts on the virtual counter only; the physical counter keeps what the per-vCPU
  replay gave it. It would not hold for a guest hypervisor's vCPU with `CNTVOFF_EL2` set (5),
  which upstream refuses to snapshot.
- Every vCPU is created and initialised before any state is replayed (vmm/src/cpu.rs:
  1065-1094, 1169-1205), and the states are replayed on several threads. That order is the
  safe one by the model's rules: a `KVM_CREATE_VCPU` after a replay resets the VM's counter
  offsets, and a `KVM_ARM_VCPU_INIT` after a replay disarms the replayed timer
  (`mutant_chInitLate_breaks`, R1 lostTick). `upstream_replay_order`: either order of the two
  replays builds the same VM.
- The per-vCPU register replay still writes each vCPU's saved counter (kvm/mod.rs:3256-3261);
  the write at the resume comes last and wins.

## 5. The fork before 0baf5b0b: a restore with `CNTVOFF_EL2` not zero

On a restore the saved `CNTVCT_EL0` is written through `arch_timer_set_user`, which stores `now
- value` in the VM's PHYSICAL offset for a vCPU with EL2 (sys_regs.c:1790-1793 with
arch_timer.c:1048-1053). The saved virtual counter is the physical one less `CNTVOFF_EL2`, so
the guest hypervisor's clock comes out behind by the `CNTVOFF_EL2` of the last vCPU replayed,
and an armed timer does not fire for that long (`nv-l2`, R4@5 and R1 timerCannotFire@6: a CPU
arms its timer, another loads a nested vCPU, pause, save, restore, resume). Measured for one
vCPU by `../probe` ("EL2 vCPU, CNTVOFF_EL2 = X, registers replayed"). Unreachable today: the
nested pin issues `KVM_ARM_SET_COUNTER_OFFSET` first, which makes that write do nothing. See
"Least sure", 1, for how this differs from the project's record of #630.

## 6. Two gaps the nested pin still has on a restore

A present-day property of `fork-nested`, and of every rule book of 4 except upstream Cloud
Hypervisor:

- **Pause to save.** The image holds `CNTPCT_EL0` as read at the save, and the offset does not
  move while paused, so the time between the pause and the save is in every clone's clock. In
  fcvm that is the time between the `Paused` request and `create_snapshot`
  (src/commands/common.rs:6599-6601 and 6654, at 43e8cfe4a; the balloon read and a failpoint
  sit between them). For Cloud Hypervisor it is `pause_vm` to `snapshot_vm` (common.rs:7246,
  7285).
- **Load to resume.** `KVM_ARM_SET_COUNTER_OFFSET` is issued during the load, and a restored
  VM starts with `paused_at_counter: None` (the fork's src/vmm/src/builder.rs:608 at
  dbea5779), so the time until the resume is not taken out. In fcvm that is `load_snapshot`
  with `resume_vm: false` (common.rs:4315-4319) to the `Resumed` request (common.rs:4439-4440);
  for Cloud Hypervisor the process start with `--restore` to `resume` (common.rs:4876-4879).

Both are milliseconds while fcvm's calls are back to back, and grow with whatever is put
between them (`nv-lags`, R4 clockOff@5: pause, host time passes, save, restore, resume).

## 7. Cloud Hypervisor passes the CPU-interface registers as 32 bits

**In a354afd77**; fixed upstream in 2018dd4b0 (v55.0), on the fix branch as 35d0e9ab. Read, not
modelled. `icc_attr_get` and `icc_attr_set` give KVM the address of a `u32` (icc_regs.rs:
84-115). KVM reads and writes 64 bits there (`vgic_v3_cpu_sysregs_uaccess`,
vgic-sys-reg-v3.c, through `kvm_sys_reg_get_user` and `kvm_sys_reg_set_user`). On a write KVM
takes four bytes of whatever follows the variable, and every `ICC_*` setter masks or truncates
what it keeps, so no kept value changes. On a read KVM writes eight bytes into the four-byte
variable: an overrun of four bytes on every read.

## 8. The fork does not save a guest hypervisor's `ICH_*_EL2` registers

**Every pin of the fork.** Read, not observed. Found by the faithfulness review of this model;
`lostNestedMask` is the part of it the model has.

- `ICH_HCR_EL2`, `ICH_VMCR_EL2`, `ICH_LR0..15_EL2`, `ICH_AP0Rn_EL2` and `ICH_AP1Rn_EL2` of a
  vCPU with EL2 are hidden from `KVM_GET_REG_LIST` and refused by `KVM_GET_ONE_REG`
  (`EL2_REG_VNCR_GICv3` is `hidden_visibility`, v7.3.3 sys_regs.c:2760-2761, 3919-3949). They
  are reached only through `KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS` (vgic-sys-reg-v3.c:366-443).
- Their storage is a page KVM allocates per vCPU (`kvm_vcpu_init_nested`, nested.c:94-96). It
  is not guest memory, so the memory file does not hold it.
- The fork requests `ICC_SRE`, `CTLR`, `IGRPEN0/1`, `PMR`, `BPR0/1` and the `ICC_AP*` registers
  from that group (src/vmm/src/arch/aarch64/gic/gicv3/regs/icc_regs.rs:32-51 at dbea5779). No
  `ICH_` name occurs under src/vmm/src/arch/aarch64 at any pin.
- While a guest hypervisor's vCPU has a nested vCPU loaded, that nested vCPU's priority mask
  and group enables are in `ICH_VMCR_EL2`, and between a nested entry and exit its pending and
  active interrupts are in `ICH_LRn_EL2` (`vgic_v3_sync_nested`, vgic/vgic-v3-nested.c:278-318,
  writes them back on every exit).

A snapshot taken then drops all of it and the restore leaves zeros. In the clone the guest
hypervisor reads `ICH_VMCR_EL2 = 0` when it next puts that nested vCPU and keeps it: the nested
vCPU's priority mask is shut and its group off, so it takes no interrupt again. An
edge-triggered interrupt that was pending in a list register is lost.

**The run** (`nv-l2`, R2 lostNestedMask@5: a CPU loads a nested vCPU; pause, save, restore,
resume), under `fork-before-630` and `fork-nested`.

**Reachable** when a nested-profile VM is snapshotted while an inner VM runs: a startup
snapshot or a user's `snapshot create`. Not for the pre-start snapshot, which is taken before
any inner VM exists. Upstream Cloud Hypervisor refuses to snapshot a nested guest; the fork
does not.

**A capture:** on a nested-profile VM with a busy inner VM, read each vCPU's `ICH_VMCR_EL2`
through `KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS` at the pause (non-zero on a vCPU with a nested vCPU
loaded) and again in the clone before it runs (zero). Then read the inner guest's
`/proc/interrupts` twice: the counts of one inner CPU do not move.

## 9. What today's VMMs write to a paused source

R5 asks that these take nothing from the source. None does; the search reports the second
kind as `-- sourceKicked`.

- **Firecracker's save publishes a vsock `TRANSPORT_RESET` event on the source and raises its
  interrupt** (`prepare_save`, src/vmm/src/devices/virtio/vsock/device.rs:467-475 and 255-290
  at 2a33289b; every pin), before KVM's state is read. An event of the device with its
  interrupt, so nothing of R1 to R6 breaks (`firecracker_save_raises_device`). What it does to
  the source is at the device: its vsock connections are reset by every snapshot. Known
  (#799), by Firecracker's design, and fcvm reconnects.
- **Cloud Hypervisor's resume raises every virtio queue's interrupt**, whether or not the queue
  has anything new (a354afd77 virtio-devices/src/device.rs:404-431; upstream device.rs:483-485),
  "to avoid a livelock". An interrupt with nothing behind it on every resume, in place or of a
  clone (`cloudHypervisor_kicks`). A virtio driver's handler finds nothing and returns.
- **The fork at ba79650e and on the nested pin raises the vsock event queue's interrupt at
  every resume** (vsock/device.rs:408-424), also on a pause and resume with no save. Read only,
  and at the device, outside this model: the same lines set `pending_event_ack`, which gates
  the device's receive path on an acknowledgement the guest sends only after a reset event. On
  a pause and resume with no save there is no such event, and the default pin's own comment
  (device.rs:411-432 at 2a33289b) says what follows: host-initiated connections hang. The
  default pin signals and arms only when an event was published. On the nested pin a pause
  whose save does not happen (a snapshot that fails after the pause) is the case.
- **The nested pin's resume issues `KVM_ARM_SET_COUNTER_OFFSET`** (the freeze, 4).
- Nothing else on either VMM's pause, save or resume writes to KVM: the vGIC and vCPU saves
  are reads, and `KVM_DEV_ARM_VGIC_SAVE_PENDING_TABLES` and the ITS table save write guest
  memory.

## Also read, and found sound

- **The distributor** has one copy and no vCPU addressing (vgic-kvm-device.c:489-501).
- **Timer registers, `mp_state`** are per-vCPU ioctls on the vCPU's own descriptor in both
  VMMs. Cloud Hypervisor at a354afd77 creates and restores vCPU 0 before it creates vCPU 1;
  creating a vCPU resets the VM-wide counter offsets (arch_timer.c:1105-1122) and vCPU 1's own
  counter write sets them again. The last vCPU wins and the result is coherent; a mutant that
  skips the last vCPU's counter is not (`mutant_chLastCounterSkipped_breaks`).
- **The pause against the saves**: no vCPU can run between another's save and the vGIC's
  (README, "Assumed").
- **Firecracker addresses each redistributor and CPU interface by that vCPU's saved MPIDR**
  (src/vmm/src/lib.rs `construct_kvm_mpidrs`, used by `save_state`).
- **R6 holds in every rule book**: `mp_state` is saved and restored per vCPU. Cloud Hypervisor
  inits every vCPU but the first with `POWER_OFF` and then writes the saved `mp_state`
  (hypervisor/src/kvm/mod.rs:2607-2610, 3150-3180); without that write the vCPU stays stopped
  (`mutant_chNoMp1_breaks`).
