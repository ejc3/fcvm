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

Where a rule decides a result and I am not certain I read it right. A faithfulness review
should start here.

1. **How #630 moved the clock on a restore.** The project's record (CLAUDE.md, "NV2 Snapshot
   Lifecycle") says the offset KVM computes on a `CNTVCT` write is kept in `CNTVOFF_EL2` storage
   and that the replay of `CNTVOFF_EL2` then clobbers it, so the clock jumps forward by the time
   since the snapshot. The source says otherwise at v6.18.3 (the host kernel when #630 was found)
   and at v7.3.3, which agree line for line:
   - `timer_set_offset` writes only `*ctxt->offset.vm_offset` (include/kvm/arm_arch_timer.h,
     6.18.3:179-187, 7.3.3:188-196).
   - For a vCPU with EL2, `kvm_timer_vcpu_reset` points the virtual timer's `vm_offset` at the
     VM's physical offset and its `vcpu_offset` at the `CNTVOFF_EL2` slot (arch_timer.c
     6.18.3:1027-1032, 7.3.3:1048-1053).
   - So a user write of `CNTVCT_EL0` (`arch_timer_set_user`, sys_regs.c 6.18.3:1608-1611,
     7.3.3:1790-1793) stores `now - value` in the physical offset. With `CNTVOFF_EL2 = 0` at the
     save the replay is coherent. With `CNTVOFF_EL2 = X` the physical counter comes out X behind.
   - The pre-fix fork does replay in `KVM_GET_REG_LIST` order: `CNTPCT_EL0`, `CNTVCT_EL0`, and
     later `CNTVOFF_EL2` (sys_regs.c 6.18.3:3538, 3540, 3745; ba79650e vcpu.rs:256-283).

   The model therefore produces a forward jump on the pre-fix fork by a resume in place (the
   whole pause) and, on a restore, only by the time between the pause and the save and between
   the load and the resume. If restores of that era jumped by the full time since the snapshot
   with `CNTVOFF_EL2` zero, a rule here is wrong. The measurement that settles it is in
   `REPAIRS.md`, 6.
2. **List registers are not modelled.** A guest acknowledge or EOI changes KVM's `struct
   vgic_irq` at once in the model. In the kernel it changes a list register, folded back at the
   next exit (`vgic_v3_fold_lr`, vgic-v3.c:74-136). That is the same thing for a save only
   because every exit folds before userspace runs. fcvm's host patch
   `kernel/patches-arm64-host/vgic-v3-keep-hcr-on-quiet-exits.patch` skips the fold when no list
   register is in use and nothing is left past them. I read that as leaving nothing unfolded;
   the model cannot check it.
3. **The guest ends an interrupt in one step.** A Linux guest at EL1 runs with EOImode 0, where
   the EOI drops the priority and deactivates. A guest hypervisor (NV2) runs with EOImode 1
   (drivers/irqchip/irq-gic-v3.c, `gic_enable_sre` and `supports_deactivate_key`): priority drop
   and deactivation are separate writes, and a pause between them leaves an interrupt active
   with no active priority. The model does not have that state.
4. **What the virtual CPU interface withholds.** `deliverable` adds the group enable and the
   active priority to KVM's own test. That half is the GIC architecture (IHI 0069, running
   priority), not kernel source. It is now observed for the active priority: `BREAKS.md`, 1.
5. **A CPU taken offline has its SGI and timer PPI disabled.** Observed on one guest kernel
   (#2155, comment 7174817511); not read in the guest source for every path.

## Build

```sh
make verify                               # check-imports, lake build, both searches against their expected output
make verify-resume-search                 # lake exe resume-breaks: the summary
make verify-resume-search RESUME_ARGS=traces    # every shortest run, in words and as a Lean term
make verify-resume-search RESUME_ARGS=spins     # every kind of state with #2155's signature
make verify-resume-search RESUME_ARGS=mutants   # each repair of the design put back
```

Measured by CI on a hosted runner (job `Lean`, which builds all three models from cold and runs
both searches): 3 min 19 s (run 45705082234). The search is 1,011,711 states over eight
configurations and takes seconds; the guest-step lemma (`guest_ok`) is most of this library's
build time.

## The files

| File | What it holds |
|---|---|
| `Spec.lean` | The state, the guest's steps, KVM's rules with file and line, the save, the restore one request at a time (`Op`, `apply`), `step`, `Reachable`. `Rules` is the rule books' type, `Cfg` what exists around the guest |
| `Today.lean` | `cloudHypervisor` (a354afd77), `cloudHypervisorRepin` (the fork's fix branch, 4c327e57), `forkBefore630` (ba79650e), `forkDefault` (2a33289b55b7), `forkNested` (dbea5779fb37) |
| `Design.lean` | `Design.cloudHypervisor`, `Design.firecracker`: each VMM with the four repairs |
| `Properties.lean` | R1 to R6 as tests on a state |
| `Configs.lean` | The eight configurations the search runs |
| `Breaks.lean` | 38 shortest runs, each replayed and checked by `decide`, and the same run under the design. Generated by `../gen-resume-breaks.py` |
| `Invariant.lean` | `Sound` (what the proofs need of a rule book), the invariant, the guest's steps |
| `Lemmas.lean` | The timer, the wake-up, the device, pause, entry, and the VM a restore builds |
| `Proofs.lean` | `step_inv`, `reachable_inv`, `sound_*`, `design_*`, and which field of `Sound` each of today's rule books fails |
| `Axioms.lean` | Fails the build on any axiom beyond Lean's own, by module |
| `search.expected` | The search's summary; CI compares |
| `../ResumeSearch.lean` | `lake exe resume-breaks` |

## The requirements

A vCPU is **idle** when its guest CPU is in the idle loop. "The clone" is a VM restored from an
image; "the source" is the VM the image was taken from.

| | In words | Test (`Properties.lean`) |
|---|---|---|
| R1 | No lost wake-up. An IPI sent and not yet handled at the pause is pending or active in the clone. A device event not yet handled has its interrupt pending or active. An armed timer can fire | `lostIpi`, `lostDeviceIrq`, `timerCannotFire` |
| R2 | No stuck interrupt. No interrupt is active on a CPU that is not in its handler. No active priority is set on a CPU that is in no handler. No enable the guest set is lost, nothing it disabled is enabled, no SGI is pending that nobody sent | `stuckActive`, `stuckPriority`, `lostEnable`, `strayEnable`, `strayPending` |
| R3 | No spin. No vCPU is idle with `kvm_arch_vcpu_runnable` true and no interrupt that can be signalled to it. This is #2155's signature: a full host CPU, an idle guest CPU with no tick | `spins` |
| R4 | Time coherence. Whenever the guest runs, its counter is the time it has run: nothing a pause, a save or a restore took is in it | `clockOff` |
| R5 | The source is undisturbed. A resume in place finds each vCPU's interrupt, timer and power state as the pause left it | `sourceDisturbed` |
| R6 | A CPU the guest has on is not stopped, one it has off is | `powerWrong` |

## Results

`lake exe resume-breaks`, shortest run to each break (`@n` is its length in steps). The
configurations are in `Configs.lean`; NV2 ones run only the fork's rule books.

| Rule book | R1 | R2 | R3 | R4 | R5 | R6 |
|---|---|---|---|---|---|---|
| `cloud-hypervisor` (as pinned) | lostIpi@5; lostDeviceIrq@5 (level device only) | stuckActive@6, stuckPriority@6, strayPending@5, strayEnable@4 | spins@8 | clockOff@4 when host time passes while paused | holds | holds |
| `cloud-hypervisor-repin` (fix branch) | lostDeviceIrq@5 (level device only) | holds | holds | clockOff@4, as above | holds | holds |
| `fork-default` | lostDeviceIrq@5 (level device only) | strayEnable@4 | holds | clockOff@4, as above | holds | holds |
| `fork-before-630` (NV2) | timerCannotFire@6 (`CNTVOFF_EL2` not zero) | strayEnable (not run: NV2 configurations have no offline CPU) | holds | clockOff@4 (pause in place), @5 (`CNTVOFF_EL2` not zero), @3 (gaps) | holds | holds |
| `fork-nested` (NV2) | holds | as above | holds | clockOff@5, only by the two gaps of `BREAKS.md`, 6 | holds | holds |
| `design-cloud-hypervisor`, `design-firecracker` | proved | proved | proved | proved | proved | proved |

"holds" is the search finding no reachable state that breaks it in the eight configurations.
"proved" is a theorem for every configuration and runs of any length.

The search reproduces #630 on the fork before its fix (`breaks_before630_nvDump_clockOff`,
`breaks_before630_nvL2_clockOff`) and not on the nested pin, with the caveat of "Least sure", 1.

## What is proved, searched, read, observed and assumed

**Proved** (Lean's kernel, no `sorry`, axioms `propext` and `Quot.sound`):

- `design_cloudHypervisor`, `design_firecracker`: for every `Cfg` and every state reachable
  under the design, every test of R1 to R6 is false (`healthy C s = true`).
- `sound_healthy` and the eleven `sound_*` theorems: the same for any rule book that is `Sound`.
- `Design.cloudHypervisor_sound`, `Design.firecracker_sound`: both designs are `Sound`, which
  includes that their restore orders, different as they are, build exactly the VM the image
  describes (`Sound.builds`).
- `cloudHypervisor_not_sound`, `cloudHypervisorRepin_not_sound`, `forkDefault_not_sound`,
  `forkBefore630_not_sound`, `forkNested_not_sound`: each of today's rule books fails a named
  field of `Sound`.
- Each of the 38 runs of `Breaks.lean`: possible under the rule book named, and the test true at
  its end (`decide`).

The proof is an inductive invariant (`Inv`, `Invariant.lean`), not the search: each vCPU's
interrupt state agrees with where its guest CPU is, the device's with the device, the counter
with the phase, and an image is the save of the paused VM it was taken from. Two conjuncts came
from proofs that would not close: the SGI is not pending again while it is handled (`VOk.once`),
and the device's interrupt is active only while the device waits (`MOk.pdev`). Both are bounds
of the model, stated below.

**The universe the theorems quantify over, exactly:**

- two vCPUs;
- per vCPU one timer PPI and one SGI; one SPI, routed to vCPU 0;
- one cross-call outstanding per target CPU, one device event at a time;
- one interrupt priority;
- one image at a time: a new save replaces it, and a resume or a restore drops it;
- any `Cfg`: any number of snapshots, restores, restores of restores, any amount of guest time,
  host time passing while paused in any of the three places, an ordinary guest or a guest
  hypervisor with or without `CNTVOFF_EL2` set, an edge or a level device, guest CPU 1 on or
  offline.

**Searched, not proved:** that today's rule books have no break beyond those in the table, in
the eight configurations of `Configs.lean` (two snapshots and two restores in `plain`, one each
elsewhere; guest time 1 unit; host time 1 or 2 units). The search program is compiled Lean and
is not itself verified. CI runs it and compares with `search.expected`.

**Read** (in the source at the named commit, not run): every rule of `Spec.lean` and
`Today.lean`. Kernel paths are under arch/arm64/kvm/ at v7.3.3.

**Observed** (on a box, by the session that reproduces #2155; a stock 7.0.0-1018-aws host, not
the fcvm host kernel): the first rule of `Today.cloudHypervisor`, and its consequence by
injection. Scope in `BREAKS.md`, 1.

**Assumed:**

| Assumption | Why |
|---|---|
| A save and a restore are atomic: nothing else happens between two of their requests | Both VMMs wait for every vCPU thread to be out of `KVM_RUN` before the first save (Cloud Hypervisor vmm/src/cpu.rs:2623-2655; the fork's `pause_vcpus`), KVM refuses the vGIC groups while any vCPU is in an ioctl (vgic-kvm-device.c:571-574), a vCPU out of `KVM_RUN` has no timer armed in the host (`kvm_timer_vcpu_put`, arch_timer.c:924-960), and both pause their devices before the save. Host time is modelled as passing only between the VMM's calls |
| KVM's list-register handling is correct | "Least sure", 2 |
| Guest registers and memory are carried exactly | Not the subject |
| The timer's level is recomputed at every vCPU load | `kvm_timer_vcpu_load_gic`, arch_timer.c:686-707: read |
| A blocked vCPU runs again exactly when `kvm_arch_vcpu_runnable` holds | As in the wake-up model |

## Each rule and its source

Host KVM, Linux v7.3.3, read:

| Rule (`Spec.lean`) | Source |
|---|---|
| `pend` | `irq_is_pending`, vgic/vgic.h; PPIs level, SGIs edge, fixed: vgic/vgic-mmio.c:733-764, vgic/vgic-init.c:276-286 |
| `kvmPending` | `kvm_vgic_vcpu_pending_irq`, vgic/vgic.c:1242-1277; `kvm_arch_vcpu_runnable`, arm.c:849-859; WFI: arm.c:1065-1089, handle_exit.c:130-178 |
| `deliverable` | the above plus the group enable and the running priority (GIC architecture) |
| `timerDue`, `fire` | `kvm_timer_pending`, arch_timer.c:366-406; `kvm_timer_update_irq`, 457-483; `kvm_bg_timer_expire`, 316-338 |
| `enter` | `kvm_timer_enable`, arch_timer.c:1587-1635; `kvm_timer_vcpu_load_gic`, 686-707 |
| `save`: latch, active, line | vgic/vgic-mmio.c:229-296, 488-534, 766-786; addressing: vgic/vgic-kvm-device.c:484-509 |
| `apply .create` | `kvm_timer_vcpu_init`, arch_timer.c:1105-1122; `vgic_v3_reset`, vgic/vgic-v3.c:474-504 |
| `apply .offset` | `kvm_vm_ioctl_set_counter_offset`, arch_timer.c:1731-1764 |
| `apply .cntp`, `.cntv` | `arch_timer_set_user`, sys_regs.c:1779-1802; NV2 wiring: arch_timer.c:1048-1053, include/kvm/arm_arch_timer.h:171-196 |
| `apply .mp` | `kvm_arch_vcpu_ioctl_set_mpstate`, arm.c:816-840 |
| `apply .dist`, `.redist`: `clearSet` | vgic/vgic-mmio.c:189-228, 593-668; ISPENDR: vgic/vgic-mmio-v3.c:383-394; ICPENDR ignored: 649-652, 733-736; active of a mapped interrupt: vgic/vgic-mmio.c:537-545 |
| `apply .icc` | `vgic_v3_cpu_sysregs_uaccess`, vgic-sys-reg-v3.c |
| `apply .level` | `vgic_write_irq_line_level_info`, vgic/vgic-mmio.c:788-820 |
| `devRaise` | `vgic_validate_injection`, `kvm_vgic_inject_irq`, vgic/vgic.c:379-392, 533-572 |

The VMMs' rules, with their lines, are the doc comments of `Today.lean`. fcvm itself issues no
KVM state ioctl: it calls pause, snapshot, resume and load (`src/commands/common.rs`,
`src/hypervisor/`), which are the model's `pause`, `save`, `resume` and `restore`.

## Each repair is carried by a proof and by the search

Each repair put back, one at a time. "Proof build" is `lake build ResumeVerify.Proofs` in a
copy with that one edit to `Design.lean` (exit status 1 each time, 0 for the unchanged copy).
"Search" is `lake exe resume-breaks mutants`.

| Put back | Proof build fails at | Search finds |
|---|---|---|
| slots read from vCPU 0 until a restore (`slotsFromRestore := true`) | `Design.cloudHypervisor_sound`, field `ownSlots` | R1 lostIpi, R2 stuckActive, stuckPriority, strayPending, R3 spins |
| image holds the counter of the save (`cntAtPause := false`) | `_sound`, field `cntAtPause` | R4 in `lags`, `nv-lags` |
| resume does not take out the pause (`freeze := false`) | `_sound`, field `freeze` | R4 in `dump`, `lags`, `nv-dump`, `nv-lags` |
| load to resume not taken out (`freezeLoaded := false`) | `_sound`, field `freezeLoaded` | R4 in `lags`, `nv-lags` |
| lines not saved (`levelInfo := false`) | `_sound`, field `levelInfo` | R1 lostDeviceIrq in `level` |
| lines not restored (no `.level`) | `_sound`, field `builds` | R1 lostDeviceIrq in `level` |
| no `KVM_ARM_SET_COUNTER_OFFSET` on restore (no `.offset`) | `_sound`, field `builds` | R1 timerCannotFire, R4 in `nv-l2` |
| clear-registers written with the saved bits (`clearInverted := false`) | `_sound`, field `builds` | R2 strayEnable in `offline` |

Each field of `Sound` is used: `ownSlots` and `levelInfo` in `rebuilt_ok`, `cntAtPause` in
`save_since` and `save_wait`, `freeze` and `freezeLoaded` in `resume_ctr`, `builds` in the
restore case of `step_inv`.

One thing the mutations show is not needed: `.offset` may come anywhere in a restore, since
`KVM_ARM_SET_COUNTER_OFFSET` overwrites whatever the register replay left. The fork issues it
before the replay; the model's `Sound.builds` accepts either.

## What is not modelled

- **LPIs and the ITS.** Cloud Hypervisor's virtio interrupts are LPIs. Their pending state is
  saved by `KVM_DEV_ARM_VGIC_SAVE_PENDING_TABLES`, which has no vCPU addressing; both VMMs issue
  it with devices paused and restore `GICR_PROPBASER`, `GICR_PENDBASER`, `GICR_CTLR`, the ITS
  tables and `GITS_CTLR` in the order the kernel's document asks. Read only.
- **List registers, the maintenance interrupt, more interrupts than list registers.**
- **Priorities**, preemption, group 0, pseudo-NMIs.
- **EOImode 1** ("Least sure", 3).
- **A guest write of `ISPENDR`**, so a latch on a level interrupt is reachable only through a
  restore.
- **The physical active state of the mapped timer interrupt.**
- **What a moved clock does to a guest hypervisor** (the storm of #630): R4 flags the moved
  clock, not its consequence.
- **`KVM_SET_VCPU_EVENTS`** (a pending SError): neither VMM saves it on arm64.
- **More than two vCPUs.**
- **The width of the CPU-interface registers** (`BREAKS.md`, 7).

## CI

`.github/workflows/verify.yml`, job `Lean`: `./check-imports.sh`, `lake build`, then `lake exe
resume-breaks | diff ResumeVerify/search.expected -` beside the boundary model's checks.
`tests/test_ci_workflow_coverage.rs` pins that line. A draft pull request gets this workflow
here; it was also dispatched by hand for the branch.
