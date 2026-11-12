/*
 * Verbatim line range from Linux v7.3.3 arch/arm64/kvm/vgic/vgic.c:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/kvm/vgic/vgic.c
 * Complete source SHA256: 23dc80fa91beb8670ac8d404cf40555d65650c7c1be3a18f26f59eb1426edb10
 * Lines 1063-1188.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/vgic-v3-keep-hcr-on-quiet-exits.patch
 * to this file, then compiles the definitions it names from it into a C harness.
 */
	return !static_branch_unlikely(&kvm_vgic_global_state.gicv3_cpuif) || has_vhe();
}

static inline void vgic_save_state(struct kvm_vcpu *vcpu)
{
	/* No switch statement here. See comment in vgic_restore_state() */
	if (vgic_is_v5(vcpu->kvm))
		vgic_v5_save_state(vcpu);
	else if (!static_branch_unlikely(&kvm_vgic_global_state.gicv3_cpuif))
		vgic_v2_save_state(vcpu);
	else
		__vgic_v3_save_state(&vcpu->arch.vgic_cpu.vgic_v3);
}

/* Sync back the hardware VGIC state into our emulation after a guest's run. */
void kvm_vgic_sync_hwstate(struct kvm_vcpu *vcpu)
{
	if (vgic_is_v3(vcpu->kvm)) {
		/* If nesting, emulate the HW effect from L0 to L1 */
		if (vgic_state_is_nested(vcpu)) {
			vgic_v3_sync_nested(vcpu);
			return;
		}

		if (vcpu_has_nv(vcpu))
			vgic_v3_nested_update_mi(vcpu);
	}

	if (can_access_vgic_from_kernel())
		vgic_save_state(vcpu);

	vgic_fold_state(vcpu);

	if (!vgic_is_v5(vcpu->kvm))
		vgic_prune_ap_list(vcpu);
}

/* Sync interrupts that were deactivated through a DIR trap */
void kvm_vgic_process_async_update(struct kvm_vcpu *vcpu)
{
	unsigned long flags;

	/* Make sure we're in the same context as LR handling */
	local_irq_save(flags);
	vgic_prune_ap_list(vcpu);
	local_irq_restore(flags);
}

static inline void vgic_restore_state(struct kvm_vcpu *vcpu)
{
	/*
	 * As nice as it would be to restructure this code into a switch
	 * statement as can be found elsewhere, the logic quickly gets ugly.
	 *
	 * __vgic_v3_restore_state() is doing a lot of heavy lifting here. It is
	 * required for GICv3-on-GICv3, GICv2-on-GICv3, GICv3-on-GICv5, and the
	 * no-in-kernel-irqchip case on GICv3 hardware. Hence, adding a switch
	 * here results in much more complex code.
	 */
	if (vgic_is_v5(vcpu->kvm))
		vgic_v5_restore_state(vcpu);
	else if (!static_branch_unlikely(&kvm_vgic_global_state.gicv3_cpuif))
		vgic_v2_restore_state(vcpu);
	else
		__vgic_v3_restore_state(&vcpu->arch.vgic_cpu.vgic_v3);
}

static void vgic_flush_state(struct kvm_vcpu *vcpu)
{
	if (vgic_is_v5(vcpu->kvm)) {
		vgic_v5_flush_ppi_state(vcpu);
		return;
	}

	scoped_guard(raw_spinlock, &vcpu->arch.vgic_cpu.ap_list_lock)
		vgic_flush_lr_state(vcpu);
}

/* Flush our emulation state into the GIC hardware before entering the guest. */
void kvm_vgic_flush_hwstate(struct kvm_vcpu *vcpu)
{
	/*
	 * If in a nested state, we must return early. Two possibilities:
	 *
	 * - If we have any pending IRQ for the guest and the guest
	 *   expects IRQs to be handled in its virtual EL2 mode (the
	 *   virtual IMO bit is set) and it is not already running in
	 *   virtual EL2 mode, then we have to emulate an IRQ
	 *   exception to virtual EL2.
	 *
	 *   We do that by placing a request to ourselves which will
	 *   abort the entry procedure and inject the exception at the
	 *   beginning of the run loop.
	 *
	 * - Otherwise, do exactly *NOTHING* apart from enabling the virtual
	 *   CPU interface. The guest state is already loaded, and we can
	 *   carry on with running it.
	 *
	 * If we have NV, but are not in a nested state, compute the
	 * maintenance interrupt state, as it may fire.
	 */
	if (vgic_state_is_nested(vcpu)) {
		if (kvm_vgic_vcpu_pending_irq(vcpu))
			kvm_make_request(KVM_REQ_GUEST_HYP_IRQ_PENDING, vcpu);

		vgic_v3_flush_nested(vcpu);
		return;
	}

	if (vcpu_has_nv(vcpu))
		vgic_v3_nested_update_mi(vcpu);

	DEBUG_SPINLOCK_BUG_ON(!irqs_disabled());

	vgic_flush_state(vcpu);

	if (can_access_vgic_from_kernel())
		vgic_restore_state(vcpu);

	if (vgic_supports_direct_irqs(vcpu->kvm) && kvm_vgic_global_state.has_gicv4)
		vgic_v4_commit(vcpu);
}

void kvm_vgic_load(struct kvm_vcpu *vcpu)
{
	const struct vgic_dist *dist = &vcpu->kvm->arch.vgic;
