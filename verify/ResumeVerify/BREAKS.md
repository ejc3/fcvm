# ResumeVerify: the breaks

Every break `lake exe resume-breaks` finds under today's rule books, grouped by cause. Each run
is in `Breaks.lean` as `breaks_<book>_<configuration>_<test>`, checked by `decide`.

Classes: **real, reachable** under fcvm's use; **real, unreachable** there, and why;
**artefact** of the model. No artefact is known at this commit; "Least sure" in the README lists
where one could be.

| # | Cause | Rule books | Tests | Class | Severity |
|---|---|---|---|---|---|
| 1 | The save reads vCPU 0's redistributor and CPU interface for every vCPU of a booted VM | `cloud-hypervisor` | R1 lostIpi, R2 stuckActive, stuckPriority, strayPending, strayEnable, R3 spins | real, reachable | high: a clone's vCPU that never takes an interrupt again |
| 2 | Clear-registers written with the saved bits | `cloud-hypervisor`, every fork pin | R2 strayEnable | real, reachable | low |
| 3 | No `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` | all of today's | R1 lostDeviceIrq | real, unreachable: no device of either VMM under fcvm holds a level line | none today |
| 4 | Nothing owns the counter offset: a pause in place moves the clock | `cloud-hypervisor`, `cloud-hypervisor-repin`, both `cloud-hypervisor-upstream`, `fork-default`, `fork-before-630` | R4 clockOff | real, reachable | low for an ordinary guest; #630 for a guest hypervisor |
| 4a | `clock_restore_mode=advance`: a restore adds the wall time since the pause | `cloud-hypervisor-upstream-advance` | R4 clockOffClone | real; unreachable if fcvm passes `preserve` | by design upstream; every armed timer due at once in an old snapshot's clone |
| 5 | `CNTVCT_EL0` replayed into the physical offset with `CNTVOFF_EL2` not zero | `fork-before-630` | R4 clockOff, R1 timerCannotFire | real, unreachable today: the nested pin sets the offset first | was high |
| 6 | The image holds the counter of the save; load to resume is not taken out | `fork-nested` (and every book of 4) | R4 clockOff | real, reachable, bounded by fcvm's own latency | low |
| 7 | CPU-interface registers passed as 32 bits | `cloud-hypervisor` | not modelled | real, reachable | unknown effect; an overrun of four bytes |

## 1. Cloud Hypervisor saves vCPU 0's interrupt state for every vCPU of a booted VM

**In the fork's pinned commit a354afd77.** Fixed upstream in 18366e1b7 (with efa34d789), in
v54.0 and later. The save-side call was removed upstream by def1d7cf8 (2022-11-30, "vmm: Remove
GICR typers in snapshot on AArch64"). The fork's fix branch `fcvm-v52-gic-identities` (4c327e57)
carries the upstream fix; `Today.cloudHypervisorRepin` models it and has none of these breaks.

**The rule** (read):

- The vGIC is created with `gicr_typers: vec![0; vcpu_count]`
  (hypervisor/src/kvm/aarch64/gic/mod.rs:274).
- `state()` passes that list to `get_redist_regs` and `get_icc_regs` (mod.rs:334-343), which
  put `typer & MPIDR_MASK` in each request (redist_regs.rs:114-122, icc_regs.rs:99-107).
- KVM resolves MPIDR 0 to vCPU 0 (v7.3.3 vgic-kvm-device.c:484-509, arm.c:2931-2960).
- The only caller of `set_gicr_typers` is the restore (devices/src/gic.rs:66-71, from
  vmm/src/device_manager.rs:1809-1813). A VM that was itself restored saves each vCPU's own.

So the image of a booted VM holds vCPU 0's `GICR_ISENABLER0`, `ISPENDR0`, `ISACTIVER0`,
`IPRIORITYR0` and `ICC_PMR`, `IGRPEN0/1`, `BPR0/1`, `AP0R0`, `AP1R0` once per vCPU, and a
restore writes them into every vCPU.

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
| R2 stuckPriority@6 | the device raises its interrupt; CPU 0 acknowledges it; pause, save, restore, resume | the same state before anything is pending for vCPU 1 |
| R2 stuckActive@6 | CPU 1 sends an IPI; CPU 0 acknowledges it; pause, save, restore, resume | vCPU 1 has the SGI active with no handler: that SGI is never delivered to it again |
| R1 lostIpi@5 | CPU 0 sends an IPI to CPU 1; pause, save, restore, resume | vCPU 1's pending SGI is not in the image (vCPU 0's, not pending, is). The sender waits in `csd_lock_wait` for ever: vCPU 0 spins in the guest, vCPU 1 is healthy |
| R2 strayPending@5 | CPU 1 sends an IPI to CPU 0; pause, save, restore, resume | vCPU 0's pending SGI is delivered to vCPU 1 as well: an IPI nobody sent it. Linux's IPI handlers tolerate that |
| R2 strayEnable@4 (`offline`) | pause, save, restore, resume, guest CPU 1 offline | vCPU 1 gets vCPU 0's enables |

**Every state with #2155's signature** (`lake exe resume-breaks spins`): five kinds, all on
vCPU 1, all with `AP1R0` set on a CPU in no handler. No state with the signature exists in the
model without it. They differ by what vCPU 0 was handling:

| vCPU 0 at the pause | vCPU 1 in the clone, beside `AP1R0` |
|---|---|
| in a device (SPI) handler | its timer PPI, an SGI, or both pending and enabled, never active |
| in its timer handler | timer PPI 27 active for ever; an SGI pending |
| in an IPI handler | that SGI active for ever; the timer PPI pending |

**What separates it from any other cause, in a capture:**

1. `ICC_AP1R0_EL1` of the stuck vCPU is non-zero while its PC is in the idle loop. Read it by
   `vm.pause` and `vm.snapshot` of the stuck clone: a restored VM saves each vCPU's own.
2. Needs no race: in a booted VM offline one guest CPU and compare the two halves of
   `ISENABLER0` in the state file. Identical halves are the defect.
3. In the source's state file, the `icc` array (9 values per vCPU: SRE, CTLR, IGRPEN0, IGRPEN1,
   PMR, BPR0, BPR1, AP0R0, AP1R0 with 5 priority bits): index 8 and 17 equal, and non-zero in
   an image whose restores hang.

Excluded if: the image that hangs has `AP1R0`, `ISACTIVER0` and `ISPENDR0` all zero, or the hang
is seen on the first restore of an image taken from a restored VM.

## 2. Clear-registers written with the saved bits

**In Cloud Hypervisor a354afd77** (fixed upstream in 1ede69b5b, v55.0; on the fix branch as
4c327e57) **and in the Firecracker fork at every pin, and in upstream Firecracker at
2a8d0138cdf9 (main, 2026-11-25).** Read; the Firecracker side not yet observed.

- Both VMMs save `ICENABLER`, `ISENABLER`, `ICACTIVER`, `ISACTIVER` by reading them (a read of
  either register of a pair returns the set bits) and restore them by writing the saved value
  to each: Cloud Hypervisor redist_regs.rs:87-97, 132-157, dist_regs.rs:67-79, 167-180; the
  fork at 2a33289b src/vmm/src/arch/aarch64/gic/regs.rs:116-129 with the lists at
  gicv3/regs/redist_regs.rs:39-49 and dist_regs.rs:27-40.
- Writing the saved bits to a clear-register clears those bits, and the set-register then sets
  them again. A bit the new VM has set and the image has not is never cleared: the result is
  `current OR saved` (`clearSet`, `Spec.lean`).
- KVM enables every SGI of a new vCPU (v7.3.3 vgic-init.c:276-286). PPIs and SPIs start
  disabled and nothing starts active, so only SGI enables are affected.

**The run** (`offline`, 4 steps: pause, save, restore, resume, guest CPU 1 offline): vCPU 1's
SGI is enabled in the clone where the guest had disabled it.

**Prediction for a capture:** restore any arm64 snapshot and read `GICR_ISENABLER0`: SGIs 8 to
15 enabled where the source had `0x..00ff`, and every SGI of an offline CPU enabled.

**Why low.** Linux enables SGIs 0 to 7 on each online CPU and sends nothing on 8 to 15. A
pending SGI cannot wake a CPU the guest turned off through PSCI: `kvm_arch_vcpu_runnable`
requires the vCPU not to be stopped (arm.c:849-859), and Linux sends IPIs only to online CPUs.
When the CPU comes back, an SGI left pending for it would be delivered once, as a spurious IPI,
which the handlers tolerate. So no guest-visible effect is known; it is the restored state
differing from the saved one, and a guest that relies on a disabled SGI would see it.

## 3. The line of a level-triggered interrupt is not in the image

Neither VMM requests `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO`. A user read of `ISPENDR` returns the
latch alone (vgic-mmio.c:229-296), so a level interrupt that is asserted and not yet
acknowledged at the pause is not pending in the clone (`level`, R1 lostDeviceIrq@5: the device
raises its interrupt; pause, save, restore, resume).

Unreachable under fcvm:

- Every device interrupt of the fork's device tree is `IRQ_TYPE_EDGE_RISING`
  (src/vmm/src/arch/aarch64/fdt.rs at 2a33289b, lines 297, 309, 414-416, 435-437).
- Cloud Hypervisor's one `IRQ_TYPE_LEVEL_HI` SPI is the PL031 RTC (arch/src/aarch64/fdt.rs:
  804-815), built with no interrupt (`devices::legacy::Rtc::new()`,
  vmm/src/device_manager.rs:2109): it cannot be asserted.
- The timer PPIs are level-triggered and are not affected: KVM recomputes their line at every
  vCPU load (arch_timer.c:686-707).

## 4. A pause in place moves the guest's clock

Cloud Hypervisor and the default fork never issue `KVM_ARM_SET_COUNTER_OFFSET`, and KVM's
offsets do not move while a vCPU is out of `KVM_RUN`, so the guest's counter runs through the
pause. A source resumed in place reads a counter ahead by the whole pause, and every timer that
came due fires at once (`dump`, R4 clockOff@4: pause, save, host time passes, resume).

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
  before the snapshot).
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
  (sys_regs.c:1790-1793, include/kvm/arm_arch_timer.h:188-196). Read at v7.3.3 and v6.18.3
  only. It would not hold for a guest hypervisor's vCPU with `CNTVOFF_EL2` set (5), which
  upstream refuses to snapshot (vm.rs, "Snapshot not supported with aarch64 nested
  virtualization").
- The per-vCPU register replay still writes each vCPU's saved counter (kvm/mod.rs:3256-3261);
  the write at the resume comes last and wins.
- Its PL031 RTC still raises no interrupt (devices/src/legacy/rtc_pl031.rs:9-10), so 3 stays
  unreachable.

## 5. The fork before 0baf5b0b: a restore with `CNTVOFF_EL2` not zero

On a restore the saved `CNTVCT_EL0` is written through `arch_timer_set_user`, which stores `now
- value` in the VM's PHYSICAL offset for a vCPU with EL2 (sys_regs.c:1790-1793 with
arch_timer.c:1048-1053). The saved virtual counter is the physical one less `CNTVOFF_EL2`, so
the guest hypervisor's clock comes out behind by `CNTVOFF_EL2` and an armed timer does not fire
for that long (`nv-l2`, R4@5 and R1 timerCannotFire@6: a CPU arms its timer, another loads a
nested vCPU, pause, save, restore, resume). Unreachable today: the nested pin issues
`KVM_ARM_SET_COUNTER_OFFSET` first, which makes that write do nothing. See "Least sure", 1, for
how this differs from the project's record of #630.

## 6. Two gaps the nested pin still has on a restore

A present-day property of `fork-nested`, and of every rule book of 4:

- **Pause to save.** The image holds `CNTPCT_EL0` as read at the save, and the offset does not
  move while paused, so the time between the pause and the save is in every clone's clock. In
  fcvm that is the time between the `Paused` request and `create_snapshot`
  (src/commands/common.rs:6599-6601 and 6654, at 43e8cfe4a; the balloon read and a failpoint
  sit between them). For Cloud Hypervisor it is `pause_vm` to `snapshot_vm` (common.rs:7246,
  7285).
- **Load to resume.** `KVM_ARM_SET_COUNTER_OFFSET` is issued during the load, and a restored
  VM starts with `paused_at_counter: None` (the fork's src/vmm/src/builder.rs), so the time
  until the resume is not taken out. In fcvm that is `load_snapshot` with `resume_vm: false`
  (common.rs:4315-4319) to the `Resumed` request (common.rs:4439-4440); for Cloud Hypervisor
  the process start with `--restore` to `resume` (common.rs:4876-4879).

Both are milliseconds while fcvm's calls are back to back, and grow with whatever is put
between them (`nv-lags`, R4 clockOff@5: pause, host time passes, save, restore, resume).

## 7. Cloud Hypervisor passes the CPU-interface registers as 32 bits

**In a354afd77**; fixed upstream in 2018dd4b0 (v55.0), on the fix branch as 35d0e9ab. Read, not
modelled. `icc_attr_get` and `icc_attr_set` give KVM the address of a `u32` (icc_regs.rs:
84-115). KVM reads and writes 64 bits there (`vgic_v3_cpu_sysregs_uaccess`,
vgic-sys-reg-v3.c, through `kvm_sys_reg_get_user` and `kvm_sys_reg_set_user`): every access
overruns the variable by four bytes, and a write hands KVM four bytes of whatever follows it.
I have not shown that this changes a value KVM keeps: the setters mask the fields they use.

## Also read, and found sound

- **The distributor** has one copy and no vCPU addressing (vgic-kvm-device.c:489-501).
- **Timer registers, `mp_state`** are per-vCPU ioctls on the vCPU's own descriptor in both
  VMMs. Cloud Hypervisor creates and restores vCPU 0 before it creates vCPU 1; creating a vCPU
  resets the VM-wide counter offsets (arch_timer.c:1105-1122) and vCPU 1's own counter write
  sets them again. The last vCPU wins and the result is coherent; a mutant that skips the last
  vCPU's counter is not (`mutants`: R1 timerCannotFire, R4).
- **The pause against the saves**: no vCPU can run between another's save and the vGIC's
  (README, "Assumed").
- **Firecracker addresses each redistributor and CPU interface by that vCPU's saved MPIDR**
  (src/vmm/src/lib.rs `construct_kvm_mpidrs`, used by `save_state`).
- **R5 and R6 hold in every rule book**: a pause and a save change nothing in KVM, and
  `mp_state` is saved and restored per vCPU. Cloud Hypervisor inits every vCPU but the first
  with `POWER_OFF` and then writes the saved `mp_state` (hypervisor/src/kvm/mod.rs:2607-2610,
  3150-3180).
