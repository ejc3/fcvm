# ResumeVerify: the repairs

What each real break of `BREAKS.md` needs changed, and where. The model's `Design.lean` has
repairs 1 to 5; `Proofs.lean` proves that a VMM with them keeps R1 to R6, and `Mutants.lean`
with `MutantBreaks.lean` shows each one is needed. Nothing here changes fcvm's own code except
where a pin moves.

| # | Repair | Where | For break | Status |
|---|---|---|---|---|
| 1 | Address each vCPU's redistributor and CPU interface as that vCPU's, on the save as on the restore | Cloud Hypervisor fork | 1 | on the fork's branch `fcvm-v52-gic-identities` (bf326e7c, 4ae9f7c5), upstream 18366e1b7; the pin has to move, or fcvm moves to upstream |
| 2 | Write the clear-registers with the complement of the saved bits | Cloud Hypervisor fork; Firecracker fork | 2 | Cloud Hypervisor: on the same branch (4c327e57), upstream 1ede69b5b. Firecracker: not done, in the fork or upstream (#2172) |
| 3 | Own the counter offset | Cloud Hypervisor; the default Firecracker pin; two gaps on the nested pin | 4, 6 | nested pin: partly (0baf5b0b). Upstream Cloud Hypervisor with `preserve`: the clone's half. Elsewhere: not done (#2173) |
| 4 | Save and restore `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` | both VMMs | 3 | not done; nothing under fcvm needs it today |
| 5 | Save and restore a guest hypervisor's `ICH_*_EL2` registers | Firecracker fork, nested pin | 8 | not done (#2179) |
| 6 | Pass the CPU-interface registers at 64 bits | Cloud Hypervisor fork | 7 | on the branch (35d0e9ab), upstream 2018dd4b0 |
| 7 | Correct the project's record of #630 | CLAUDE.md | "Least sure", 1 | done in this pull request, with the measurement |

## 1. Per-vCPU addressing in Cloud Hypervisor's save

`Rules.slotsFromRestore := false`. The save must build its list of redistributors from the
vCPUs before `Gic::snapshot`, as the restore does. The fork's branch and upstream do it by
deriving each MPIDR from the vCPU index on both paths (`get_gicr_typers`), which also removes
the dependence on saved vCPU states.

Until the pin moves, fcvm's users can hit break 1 on any restore of a snapshot of a booted
Cloud Hypervisor VM with more than one vCPU. A snapshot already taken from a booted VM holds
vCPU 0's state for every vCPU and stays wrong after the fix: it has to be taken again. Such an
image can be recognised: both vCPUs' `GICR_PENDBASER` are equal, and so are their `ICC` and
SGI/PPI register blocks.

## 2. The clear-registers

`Rules.clearInverted := true`. Write `ICENABLER` and `ICACTIVER` with `!saved` (or all ones)
before the set-registers, so the restored bits are the saved bits and not `current OR saved`.

- Cloud Hypervisor: `access_redists_aux` and `set_dist_regs`, as upstream 1ede69b5b does.
- Firecracker fork: `set_reg_data` writes every chunk as saved
  (src/vmm/src/arch/aarch64/gic/regs.rs:116-129). The registers to complement are
  `GICR_ICENABLER0` and `GICR_ICACTIVER0` (gicv3/regs/redist_regs.rs:39-49) and
  `GICD_ICENABLER` and `GICD_ICACTIVER` (gicv3/regs/dist_regs.rs:36-49). Upstream Firecracker
  has the same code at 2a8d0138cdf9, so the same change is an upstream report; that is the
  owner's call.

## 3. The counter

The design: `KVM_ARM_SET_COUNTER_OFFSET` on every restore (`.offset`), the image holding the
counter of the pause (`cntAtPause`), and a resume taking out the time the guest did not run,
also between a load and its resume (`freeze`, `freezeLoaded`). With those, R4 is proved: the
guest's counter is the time it has run, whatever passes while it is paused.

The model has one counter. KVM has two offsets and a latch, and a repair has to respect what
the one counter hides:

- **`KVM_ARM_SET_COUNTER_OFFSET` sets the virtual and the physical offset to the same value**
  (arch_timer.c:1745-1754), and from then on every `KVM_SET_ONE_REG` of either counter is
  ignored for the life of the VM (sys_regs.c:1791-1796). Measured: `../probe`, the three
  "KVM_ARM_SET_COUNTER_OFFSET then other values written" cases write `P + 500 s` and `V + 700 s`
  after the ioctl and both counters still read their saved values.
- **Which saved register the offset is computed from depends on the guest.** An ordinary guest
  ticks on the virtual counter: the offset is `host counter - saved CNTVCT_EL0`
  (`KVM_REG_ARM_TIMER_CNT`). A guest hypervisor ticks on the physical one: `host counter -
  saved CNTPCT_EL0`, which is what the nested pin takes. For a VM whose two counters differ the
  ioctl moves the other one.
- **So the VMM has to own the offset from boot**, as the nested pin does
  (src/vmm/src/builder.rs:323-340 at dbea5779): a VM booted without it has `voffset = host
  counter at creation` and `poffset = 0` (arch_timer.c:1115-1116; the fork also writes
  `KVM_REG_ARM_PTIMER_CNT = 0` at boot), its two counters differ, and a later ioctl cannot keep
  both. The freeze at a resume also needs the current offset, which only a VMM that set it
  knows.
- **The ioctl and a counter write at the resume cannot be combined.** Upstream Cloud
  Hypervisor's way (one `KVM_REG_ARM_TIMER_CNT` write through vCPU 0 at the resume of a
  restored VM) does nothing once the ioctl has been used. It keeps R4 for a clone without the
  ioctl, for the virtual counter only.

Why the design freezes the clock for every guest, not only a guest hypervisor: R4 as stated
asks for it, and a guest whose timers all come due at a resume does work it would not have
done. Whether that is worth changing for ordinary guests is a judgement the model does not
make: Linux tolerates the jump, and it is what every KVM pause does. In order of what it buys:

- **Nested pin, the two gaps of break 6.** Small today. `paused_at_counter` set at the load
  closes the second; saving the counter captured at `pause_vm` (the fork already has it) in
  place of the one read at the save closes the first. Worth doing before anything is put
  between fcvm's pause and its save, or between a load and its resume, for a nested guest.
- **Default pin.** The fork has the code for the nested pin; the default pin's branch does not
  carry it. It would have to come with the offset owned at boot and computed from the virtual
  counter for an ordinary guest. Without it a source resumed in place has its clock moved by
  the pause.
- **Cloud Hypervisor.** The pin has no counter code. Upstream at 4969a51a3d3f does the clone's
  half: with `clock_restore_mode=preserve` a clone's clock is the one of the pause
  (`BREAKS.md`, 4). fcvm has to pass `preserve`; the default, `advance`, puts the whole time
  since the snapshot into the clone's clock. A source resumed in place still has its clock
  moved by the pause there. Lowest priority: fcvm's Cloud Hypervisor guests are not
  hypervisors.

## 4. Level information

`Rules.levelInfo := true` and `.level`. Read the group after the redistributors on the save,
write it last on the restore. No device under fcvm holds a level line today (`BREAKS.md`, 3), so
this is for a device that would.

## 5. A guest hypervisor's `ICH_*_EL2` registers

`Rules.nestedIcc := true`. In the fork's vGIC save and restore, for a VM whose vCPUs have
`KVM_ARM_VCPU_HAS_EL2`:

- **What:** through `KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS`, addressed by each vCPU's MPIDR as the
  `ICC_*` registers already are, at 64 bits: `ICH_HCR_EL2`, `ICH_VMCR_EL2`, `ICH_LR0_EL2` to
  `ICH_LR15_EL2`, and the `ICH_AP0Rn_EL2` and `ICH_AP1Rn_EL2` the host's priority bits allow
  (the kernel refuses an index above `vgic_v3_max_apr_idx`, vgic-sys-reg-v3.c:313-333, so the
  same rule as for `ICC_AP*`: `AP*R0` always, `R1` with 6 bits, `R2` and `R3` with 7). The
  kernel's table is vgic-sys-reg-v3.c:416-443. `ICC_SRE_EL2` and `ICH_VTR_EL2` are constants a
  write must match (335-364) and need no saving.
- **Save order:** with the other CPU-interface registers, after the vCPUs are out of `KVM_RUN`.
  A vCPU that exits while it runs its nested guest has had its list registers written back
  (`vgic_v3_sync_nested`, vgic/vgic-v3-nested.c:278-318) by then.
- **Restore order:** after each vCPU's `KVM_ARM_VCPU_INIT` (which allocates the page they live
  in, `kvm_vcpu_init_nested`) and after the vGIC is initialised, before the vCPUs run. Among
  themselves any order: the setters store the value given (`set_gic_ich_reg`,
  vgic-sys-reg-v3.c:299-304).

The model has `ICH_VMCR_EL2` alone (`Vcpu.ichv`), and with it `design_firecracker` proves
`lostNestedMask` false. That the list registers and active priorities are then carried too is
read from the kernel's setters, not modelled.

A cheaper stop for fcvm until then: refuse `snapshot create` of a nested-profile VM while an
inner VM runs, as upstream Cloud Hypervisor refuses every nested snapshot. That is a change to
fcvm's behaviour and the owner's call.

## 6. The CPU-interface register width

Use a `u64` for the value behind `KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS`, as the Firecracker fork
does (`RegChunk = u64`, gicv3/regs/icc_regs.rs:78).

## 7. The record of #630, corrected

CLAUDE.md's "NV2 Snapshot Lifecycle" said a restore "replayed CNTVCT/CNTPCT (KVM adjusts
per-timer offsets, which for HAS_EL2 guests live in CNTVOFF_EL2 storage), then replayed
CNTVOFF_EL2, clobbering the adjustment", so that "the guest's monotonic clock jumped forward by
the host time elapsed since the snapshot".

**What the source says** (v6.18.3 and v7.3.3, README "Least sure", 1; the faithfulness review
of c58a2d263 read it the same way): nothing a counter write computes is kept in `CNTVOFF_EL2`
storage, so its replay has nothing to clobber, and with `CNTVOFF_EL2 = 0` at the save the fix's
restore half changes nothing: the pre-fix replay leaves the physical offset at `now - saved
CNTVCT`, the fix sets it to `host counter - saved CNTPCT`, and the two saved counters are equal.

**What the measurement says** (`../probe/counter-replay.c`, `../probe/expected.txt`; an
m8gd.metal-24xl, kernel 7.3.3 with fcvm's host patches, `kvm-arm.mode=nested`): on a vCPU with
`KVM_ARM_VCPU_HAS_EL2`, after writing `CNTPCT_EL0 = P` and `KVM_REG_ARM_TIMER_CNT = P` with no
`KVM_ARM_SET_COUNTER_OFFSET`, `CNTVOFF_EL2` reads zero, and after `CNTVOFF_EL2 = 0` is written
both counters read P plus the second slept. The record predicts a value near the host's
counter, which was 10,000 s away, against a tolerance of 50 ms. With a saved `CNTVOFF_EL2 = X`,
the saved virtual counter being `P - X`, each counter reads X behind its saved value; with the
ioctl first, both read their saved values. Measured on that one kernel, for one vCPU, through
`KVM_GET_ONE_REG`.

So the record's restore sentence describes a mechanism the kernel does not have. Its second
half is right: a resume in place did move the clock forward by the whole pause, and the fix's
freeze removes that.

**Not settled by either:** why restore-hit boots were measured slow before the fix and fast
after. By the above the restore-time ioctl cannot be the cause with `CNTVOFF_EL2 = 0`. What is
left: those snapshots had a nested vCPU loaded (then the clock went back, break 5); or the
improvement came from the freeze, on a restored VM that fcvm then paused for its startup
snapshot; or from something else. The record's own second finding ("create+resume stormed 3/3;
restore was clean 12/12") fits the pause path.

**The text now in CLAUDE.md** (item 1 of that section, edited in this pull request; the
wording is the delta reviewer's, which says what was read at which kernel, what was measured
on v7.3.3 only, and what stays open):

> **1. The guest's clock ran while it was paused (firecracker fork fix).** Nothing owned the
> VM's counter offset, so the guest's counter kept running while the vCPUs were out of
> `KVM_RUN`: a VM resumed in place after a snapshot had its clock ahead by the whole pause, and
> every timer that came due in that time fired at once. A restore's register replay does not
> move the clock forward: on a `CNTVCT`/`CNTPCT` write KVM stores `now - value` in the VM-wide
> physical offset and keeps nothing in `CNTVOFF_EL2` storage (read at v6.18.3 and v7.3.3;
> measured by `verify/probe` on v7.3.3 only), so the counters continue from the saved values,
> except that a saved `CNTVOFF_EL2` of X on the last vCPU replayed leaves the physical counter X
> behind. Why restore-hit boots measured slow before the fix and fast after is not explained by
> this and is open. `kvm_timer_emulate` at about 14k a second can be the exit rate of a guest
> hypervisor without FEAT_ECV (read: both EL1 timers are emulated again on every exit whether
> or not one fires), so that count alone does not show timers firing; the trace's `should_fire`
> field would.

A run of the probe on a v6.18.3 nested host would turn "read at v6.18.3" into "measured".
