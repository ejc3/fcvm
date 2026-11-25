# ResumeVerify: the repairs

What each real break of `BREAKS.md` needs changed, and where. The model's `Design.lean` has
repairs 1 to 4; `Proofs.lean` proves that a VMM with them keeps R1 to R6, and the README's
mutation table shows each one is needed. Nothing here changes fcvm's own code except where a
pin moves.

| # | Repair | Where | For break | Status |
|---|---|---|---|---|
| 1 | Address each vCPU's redistributor and CPU interface as that vCPU's, on the save as on the restore | Cloud Hypervisor fork | 1 | on the fork's branch `fcvm-v52-gic-identities` (bf326e7c, 4ae9f7c5), upstream 18366e1b7; the pin has to move |
| 2 | Write the clear-registers with the complement of the saved bits | Cloud Hypervisor fork; Firecracker fork | 2 | Cloud Hypervisor: on the same branch (4c327e57), upstream 1ede69b5b. Firecracker: not done, in the fork or upstream |
| 3 | Own the counter offset | Cloud Hypervisor; the default Firecracker pin; two gaps on the nested pin | 4, 6 | nested pin: partly (0baf5b0b). Elsewhere: not done |
| 4 | Save and restore `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` | both VMMs | 3 | not done; nothing under fcvm needs it today |
| 5 | Pass the CPU-interface registers at 64 bits | Cloud Hypervisor fork | 7 | on the branch (35d0e9ab), upstream 2018dd4b0 |
| 6 | Correct the project's record of #630, after a review or a measurement | CLAUDE.md | "Least sure", 1 | proposed below |

## 1. Per-vCPU addressing in Cloud Hypervisor's save

`Rules.slotsFromRestore := false`. The save must build its list of redistributors from the
vCPUs before `Gic::snapshot`, as the restore does. The fork's branch does it by deriving each
MPIDR from the vCPU index on both paths (`get_gicr_typers`), which also removes the dependence
on saved vCPU states.

Until the pin moves, fcvm's users can hit break 1 on any restore of a snapshot of a booted
Cloud Hypervisor VM with more than one vCPU. A snapshot already taken from a booted VM holds
vCPU 0's state for every vCPU and stays wrong after the fix: it has to be taken again. fcvm's
snapshot cache keys would have to change with the pin for that to happen by itself; whether
they do is for the pull request that moves the pin to check.

## 2. The clear-registers

`Rules.clearInverted := true`. Write `ICENABLER` and `ICACTIVER` with `!saved` (or all ones)
before the set-registers, so the restored bits are the saved bits and not `current OR saved`.

- Cloud Hypervisor: `access_redists_aux` and `set_dist_regs`, as upstream 1ede69b5b does.
- Firecracker fork: `set_reg_data` writes every chunk as saved
  (src/vmm/src/arch/aarch64/gic/regs.rs:116-129). The registers to complement are
  `GICR_ICENABLER0` and `GICR_ICACTIVER0` (gicv3/regs/redist_regs.rs:39-49) and
  `GICD_ICENABLER` and `GICD_ICACTIVER` (gicv3/regs/dist_regs.rs:27-40). Upstream Firecracker
  has the same code at 2a8d0138cdf9, so the same change is an upstream report; that is the
  owner's call.

## 3. The counter

The design: `KVM_ARM_SET_COUNTER_OFFSET` on every restore (`.offset`), the image holding the
counter of the pause (`cntAtPause`), and a resume taking out the time the guest did not run,
also between a load and its resume (`freeze`, `freezeLoaded`). With those, R4 is proved: the
guest's counter is the time it has run, whatever passes while it is paused.

Why the design freezes the clock for every guest, not only a guest hypervisor: R4 as stated
asks for it, and a guest whose timers all come due at a resume does work it would not have
done. Whether that is worth changing for ordinary guests is a judgement the model does not
make: Linux tolerates the jump, and it is what every KVM pause does. In order of what it buys:

- **Nested pin, the two gaps of break 6.** Small today. `paused_at_counter` set at the load
  closes the second; saving the counter captured at `pause_vm` (the fork already has it) in
  place of the one read at the save closes the first. Worth doing before anything is put
  between fcvm's pause and its save, or between a load and its resume, for a nested guest.
- **Default pin.** The fork has the code for the nested pin; the default pin's branch does not
  carry it. Without it a source resumed in place has its clock moved by the pause.
- **Cloud Hypervisor.** It has no counter code at all; this is a change to Cloud Hypervisor,
  not to how fcvm uses it. Lowest priority: fcvm's Cloud Hypervisor guests are not hypervisors.

## 4. Level information

`Rules.levelInfo := true` and `.level`. Read the group after the redistributors on the save,
write it last on the restore. No device under fcvm holds a level line today (`BREAKS.md`, 3), so
this is for a device that would.

## 5. The CPU-interface register width

Use a `u64` for the value behind `KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS`, as the Firecracker fork
does (`RegChunk = u64`, gicv3/regs/icc_regs.rs:78).

## 6. The record of #630: a proposed correction, not a fact

If the reading of "Least sure", 1 is right, CLAUDE.md's "NV2 Snapshot Lifecycle" paragraph on
the timer would have to say this in place of its first two sentences:

> **1. The guest's clock moved by the time it was paused (firecracker fork fix).** Nothing owned
> the VM's counter offset, so the counter ran while the vCPUs were paused: a VM resumed after a
> snapshot had its clock ahead by the whole pause, and every armed timer CVAL in the past. On a
> restore the register replay kept the counters continuous (KVM stores `now - value` in the
> VM-wide physical offset on a `CNTVCT`/`CNTPCT` write; nothing is kept in `CNTVOFF_EL2`
> storage), except that a vCPU with `CNTVOFF_EL2` not zero had the physical counter set back by
> it. `kvm_timer_emulate` at about 14k a second is the exit rate of a guest hypervisor without
> FEAT_ECV, which re-emulates both EL1 timers on every exit.

What would decide between this and the record, on a build of the fork at ba79650e with a guest
hypervisor and `CNTVOFF_EL2 = 0` (shown by `FC_DEBUG_REG_DUMP` on a later build, or read through
ONE_REG): read `CNTPCT_EL0` of vCPU 0 just before the pause and just after a restore's resume,
with a measured wait between the save and the restore. A difference near the wait is the
record; a difference near the two gaps of break 6 is this reading.
