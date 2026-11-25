import ResumeVerify.Spec

/-!
# The VMMs as they are

One rule book per VMM as fcvm pins it, every rule with the file and line it was read from.
"Read" means read in the source at the named commit; nothing here was observed in a running
VM. Host KVM is Linux v7.3.3 for all of them (`Spec.lean`).
-/

namespace Resume

namespace Today

/-- Cloud Hypervisor, ejc3/cloud-hypervisor a354afd77 (branch fcvm-v52-sve-fix).

* **The save reads vCPU 0's redistributor and CPU interface for every vCPU of a VM that was
  booted, not restored.** The vGIC is created with `gicr_typers: vec![0; vcpu_count]`
  (hypervisor/src/kvm/aarch64/gic/mod.rs:274). `state()` passes that list to `get_redist_regs`
  and `get_icc_regs` (mod.rs:334-343), which put `typer & MPIDR_MASK` in each request
  (redist_regs.rs:114-122, icc_regs.rs:99-107). MPIDR 0 is vCPU 0 (v7.3.3
  vgic-kvm-device.c:484-509, arm.c:2931-2960). The one caller of `set_gicr_typers` is the
  restore (devices/src/gic.rs:66-71, from vmm/src/device_manager.rs:1809-1813), so a restored
  VM saves each vCPU's own.
* The image's counter is the one read at the save: `Vcpu::snapshot` calls `vcpu.state()`
  (vmm/src/cpu.rs:692-703), which reads every register of `KVM_GET_REG_LIST`
  (hypervisor/src/kvm/mod.rs:2921-2988). `pause` and `resume` leave the counter offset alone
  (vmm/src/cpu.rs:2623-2683, vmm/src/vm.rs:3256-3325): nothing in the tree issues
  `KVM_ARM_SET_COUNTER_OFFSET`.
* No request for `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` anywhere in the tree.
* The restore writes every register with the saved value, the clear-registers included
  (`access_redists_aux`, redist_regs.rs:132-157; `set_dist_regs`, dist_regs.rs:167-180): a bit
  the new VM has set and the image has not is not cleared. KVM enables every SGI of a new vCPU
  (v7.3.3 vgic-init.c:276-286), so an SGI the guest disabled is enabled in the clone.
* Every resume, in place or of a restored VM, raises each virtio queue's interrupt whether or
  not the queue has anything new ("to wake up the driver to avoid a livelock",
  virtio-devices/src/device.rs:404-431; still there upstream at 4969a51a3d3f, device.rs:483-485).
  Nothing else on the pause, the save or the resume writes to KVM: the pause flushes the
  pending and ITS tables into guest memory (devices/src/gic.rs:165-173) and the save reads.
* The restore makes each vCPU and restores it before it makes the next (`create_vcpu`,
  vmm/src/cpu.rs:974-1027, called per vCPU from `create_boot_vcpus`, cpu.rs:1525; vCPUs before
  devices, vmm/src/vm.rs:974, 1059): `KVM_ARM_VCPU_INIT` with `POWER_OFF` for every vCPU but
  the first (hypervisor/src/kvm/mod.rs:2607-2610), the registers in `KVM_GET_REG_LIST` order,
  in which `CNTPCT_EL0` precedes `CNTVCT_EL0` (v7.3.3 sys_regs.c:3748-3751, 5720-5733), then
  `KVM_SET_MP_STATE` (mod.rs:3150-3180). Then the vGIC: `GICD_CTLR` and the distributor, the
  redistributors of all vCPUs, the CPU interfaces of all vCPUs (gic/mod.rs:402-413). -/
def cloudHypervisor : Rules :=
  { name := "cloud-hypervisor",
    slotsFromRestore := true, cntAtPause := false, freeze := false, freezeLoaded := false,
    advance := false, levelInfo := false, clearInverted := false,
    nestedIcc := false, onSave := [], onResume := [.kick],
    restore := [.create false, .init false false, .cntp false, .cntv false, .regs false, .mp false,
                .create true, .init true true, .cntp true, .cntv true, .regs true, .mp true,
                .gic, .dist, .redist false, .redist true, .icc false, .icc true] }

/-- Cloud Hypervisor with the six upstream commits the fork is taking (branch
fcvm-v52-gic-identities, 4c327e57): each vCPU's redistributor and CPU interface addressed by
the MPIDR derived from its index, on the save as on the restore (bf326e7c, 4ae9f7c5), and the
clear-registers written with the complement of the saved bits (4c327e57). The CPU-interface
registers are read and written at their 64-bit width (35d0e9ab), which the model does not
distinguish. Nothing in the six touches the counter or the level information. -/
def cloudHypervisorRepin : Rules :=
  { cloudHypervisor with name := "cloud-hypervisor-repin", slotsFromRestore := false,
                         clearInverted := true }

/-- Cloud Hypervisor upstream, cloud-hypervisor/cloud-hypervisor 4969a51a3d3f (main, v55.0 and
117 commits), which fcvm is moving to, with `clock_restore_mode=preserve` (what fcvm will pass)
or `advance` (the default).

* Addressing and clear-registers as `cloudHypervisorRepin`
  (hypervisor/src/kvm/aarch64/gic/mod.rs:357, 428; redist_regs.rs:137; dist_regs.rs:176); the
  CPU-interface registers at 64 bits (icc_regs.rs:84, 100). Still no `LEVEL_INFO`.
* The counter is captured at the pause: `Vm::pause` reads the boot vCPU's
  `KVM_REG_ARM_TIMER_CNT` and the host's wall clock as soon as the vCPUs are quiesced
  (vmm/src/vm.rs:2915-2918; `snapshot_clock`, hypervisor/src/kvm/mod.rs:1165-1184), and the
  snapshot holds that (vm.rs:3002).
* A resume in place does nothing to it: for `SameHostResume`, `restore_clock` returns at once
  (kvm/mod.rs:1257-1268), so the counter has run through the pause.
* A restore writes one counter value through vCPU 0 at the resume, before the vCPUs run
  (`Vm::resume` calls `restore_guest_clock`, vm.rs:2877-2888, 2946-2947): the saved value for
  `preserve`, the saved value plus the wall time since the capture for `advance`
  (kvm/mod.rs:1278-1295). One write serves every vCPU because KVM's offset is the VM's
  (v7.3.3 arch_timer.c:1075-1090, include/kvm/arm_arch_timer.h:188-196). The per-vCPU register
  replay still writes each vCPU's saved counter first (kvm/mod.rs:3256-3261); the write at the
  resume is the last and wins.
* Every vCPU is created and given its `KVM_ARM_VCPU_INIT` first (`create_vcpu`,
  vmm/src/cpu.rs:1065-1094), then the saved states are replayed, on several threads
  (`create_vcpus`, cpu.rs:1169-1205; `Vcpu::restore`, cpu.rs:724-737). So no `KVM_CREATE_VCPU`
  and no `KVM_ARM_VCPU_INIT` follows a register replay: a later create would reset the VM's
  counter offsets and a later init would disarm a replayed timer (`Mutant.chInitLate`). The
  model replays vCPU 0 then vCPU 1; `upstream_replay_order` (`Proofs.lean`) is that the other
  order builds the same VM. The vGIC is made and restored after the vCPUs
  (`create_interrupt_controller` after `create_boot_vcpus`), in the order of a354afd77.
* After the first resume of a restored VM a virtio-net device announces itself: a config-change
  event with its interrupt (virtio-devices/src/net.rs:54-57, 100-106). That is an event of the
  device with its own (edge, MSI) interrupt, which the model's device may raise at any time
  once the guest runs. -/
def cloudHypervisorUpstream (advance : Bool) : Rules :=
  { name := if advance then "cloud-hypervisor-upstream-advance" else "cloud-hypervisor-upstream-preserve",
    slotsFromRestore := false, cntAtPause := true, freeze := false, freezeLoaded := !advance,
    advance := advance, levelInfo := false, clearInverted := true,
    nestedIcc := false, onSave := [], onResume := [.kick],
    restore := [.create false, .init false false, .create true, .init true true,
                .cntp false, .cntv false, .regs false, .mp false,
                .cntp true, .cntv true, .regs true, .mp true,
                .gic, .dist, .redist false, .redist true, .icc false, .icc true,
                .cntv false] }

/-- The same with the two vCPUs' states replayed in the other order. -/
def cloudHypervisorUpstreamSwapped (advance : Bool) : Rules :=
  { cloudHypervisorUpstream advance with
    restore := [.create false, .init false false, .create true, .init true true,
                .cntp true, .cntv true, .regs true, .mp true,
                .cntp false, .cntv false, .regs false, .mp false,
                .gic, .dist, .redist false, .redist true, .icc false, .icc true,
                .cntv false] }

/-- The order of a Firecracker restore (src/vmm/src/builder.rs `build_microvm_from_snapshot`):
all vCPUs made, with the vGIC after them (`Vm::create_vcpus`), then each vCPU's
`KVM_ARM_VCPU_INIT` from the saved features with `POWER_OFF` cleared
(src/vmm/src/arch/aarch64/vcpu.rs `save_state`, `restore_state`), its registers in the saved
order and `KVM_SET_MP_STATE`, then the vGIC: the distributor, then per vCPU its redistributor
and its CPU interface (src/vmm/src/arch/aarch64/gic/gicv3/regs/mod.rs `restore_state`).
Every register is written with the saved value, the clear-registers included
(src/vmm/src/arch/aarch64/gic/regs.rs:116-129; the lists at gicv3/regs/redist_regs.rs:39-49
and dist_regs.rs:36-49), as in Cloud Hypervisor. -/
def fcOrder (offset : Bool) : List Op :=
  [.create false, .create true, .gic] ++ (if offset then [.offset] else []) ++
  [.init false false, .cntp false, .cntv false, .regs false, .mp false,
   .init true false, .cntp true, .cntv true, .regs true, .mp true,
   .dist, .redist false, .icc false, .redist true, .icc true]

/-- fcvm's Firecracker fork before the fix of #630, ejc3/firecracker ba79650e (NV2 support, no
counter code): the registers are replayed as saved and nothing owns the counter offset. Each
vCPU's redistributor and CPU interface is addressed by that vCPU's saved MPIDR
(src/vmm/src/lib.rs `construct_kvm_mpidrs`, used by `save_state`).

What every pin of the fork writes to a paused source:

* At the save, before KVM's state is read (`Vmm::save_state` saves devices first,
  src/vmm/src/lib.rs), the vsock device publishes a `TRANSPORT_RESET` event in the SOURCE's
  event queue and raises its interrupt (`prepare_save` and `send_transport_reset_event`,
  src/vmm/src/devices/virtio/vsock/device.rs:255-290, 467-475 at 2a33289b): `.devEvent`. The
  source's vsock connections are reset by every snapshot, by Firecracker's design.
* At every resume `kick_virtio_devices` runs (src/vmm/src/lib.rs `resume_vm`). At ba79650e and
  on the nested pin the vsock device's `kick` raises the event queue's interrupt whether or not
  an event was published (vsock/device.rs:408-424 at ba79650e and dbea5779): `.kick`. The
  default pin raises it only while a reset event waits for the guest (device.rs:411-432 at
  2a33289b), which in the model is the interrupt the save already raised.
* No `ICH_*_EL2` register is requested from the vGIC device: none is named anywhere under
  src/vmm/src/arch/aarch64 (`nestedIcc := false`). -/
def forkBefore630 : Rules :=
  { name := "fork-before-630",
    slotsFromRestore := false, cntAtPause := false, freeze := false, freezeLoaded := false,
    advance := false, levelInfo := false, clearInverted := false,
    nestedIcc := false, onSave := [.devEvent], onResume := [.kick], restore := fcOrder false }

/-- The fork as pinned for every profile but the nested arm64 one, ejc3/firecracker
2a33289b55b7 (rootfs-config.toml `[firecracker]`). It does not carry 0baf5b0b: for the
counter it is `forkBefore630`. Its guests are not hypervisors. -/
def forkDefault : Rules := { forkBefore630 with name := "fork-default", onResume := [] }

/-- The fork as pinned for the nested arm64 profile, ejc3/firecracker dbea5779fb37, which has
0baf5b0b and 865fcab0: `KVM_ARM_SET_COUNTER_OFFSET` at boot, before the register replay of a
restore from vCPU 0's saved `CNTPCT_EL0` (src/vmm/src/builder.rs:520-535), and on `resume_vm`
moved by the time since `pause_vm` (src/vmm/src/lib.rs:478-524). A restored VM starts with
`paused_at_counter: None`, so the time between its load and its resume is not taken out.
This is the path on which the ioctl succeeds. A failed ioctl at a restore, or an image with no
`CNTPCT_EL0`, is logged and leaves the restore as `forkBefore630` does it (builder.rs:520-548);
a failed ioctl at a resume is an error and the VM stays paused. -/
def forkNested : Rules :=
  { name := "fork-nested",
    slotsFromRestore := false, cntAtPause := false, freeze := true, freezeLoaded := false,
    advance := false, levelInfo := false, clearInverted := false,
    nestedIcc := false, onSave := [.devEvent], onResume := [.kick], restore := fcOrder true }

end Today

end Resume
