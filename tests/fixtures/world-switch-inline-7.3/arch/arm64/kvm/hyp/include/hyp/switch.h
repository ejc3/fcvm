/*
 * Verbatim line range from Linux v7.3.3 arch/arm64/kvm/hyp/include/hyp/switch.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/kvm/hyp/include/hyp/switch.h
 * Complete source SHA256: 96b959098f33cf22680f98a206f835c4b48a6813b9264814f74617d23a3e3358
 * Lines 518-580.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/world-switch-helpers-inline.patch to this file,
 * then checks the definitions it names.
 */
	write_sysreg_s(sve_vq_from_vl(kvm_host_sve_max_vl) - 1, SYS_ZCR_EL2);
	sve_save_state(sve_regs, true);
	fpsimd_save_common(&hctxt->fp_regs);
}

static inline void fpsimd_lazy_switch_to_guest(struct kvm_vcpu *vcpu)
{
	u64 zcr_el1, zcr_el2;

	if (!guest_owns_fp_regs())
		return;

	if (vcpu_has_sve(vcpu)) {
		zcr_el2 = vcpu_sve_max_vq(vcpu) - 1;

		/* A guest hypervisor may restrict the effective max VL. */
		if (is_nested_ctxt(vcpu))
			zcr_el2 = min(zcr_el2, __vcpu_sys_reg(vcpu, ZCR_EL2));

		write_sysreg_el2(zcr_el2, SYS_ZCR);

		zcr_el1 = __vcpu_sys_reg(vcpu, vcpu_sve_zcr_elx(vcpu));
		write_sysreg_el1(zcr_el1, SYS_ZCR);
	}
}

static inline void fpsimd_lazy_switch_to_host(struct kvm_vcpu *vcpu)
{
	u64 zcr_el1, zcr_el2;

	if (!guest_owns_fp_regs())
		return;

	/*
	 * When the guest owns the FP regs, we know that guest+hyp traps for
	 * any FPSIMD/SVE/SME features exposed to the guest have been disabled
	 * by either __activate_cptr_traps() or kvm_hyp_handle_fpsimd()
	 * prior to __guest_entry(). As __guest_entry() guarantees a context
	 * synchronization event, we don't need an ISB here to avoid taking
	 * traps for anything that was exposed to the guest.
	 */
	if (vcpu_has_sve(vcpu)) {
		zcr_el1 = read_sysreg_el1(SYS_ZCR);
		__vcpu_assign_sys_reg(vcpu, vcpu_sve_zcr_elx(vcpu), zcr_el1);

		/*
		 * The guest's state is always saved using the guest's max VL.
		 * Ensure that the host has the guest's max VL active such that
		 * the host can save the guest's state lazily, but don't
		 * artificially restrict the host to the guest's max VL.
		 */
		if (has_vhe()) {
			zcr_el2 = vcpu_sve_max_vq(vcpu) - 1;
			write_sysreg_el2(zcr_el2, SYS_ZCR);
		} else {
			zcr_el2 = sve_vq_from_vl(kvm_host_sve_max_vl) - 1;
			write_sysreg_el2(zcr_el2, SYS_ZCR);

			zcr_el1 = vcpu_sve_max_vq(vcpu) - 1;
			write_sysreg_el1(zcr_el1, SYS_ZCR);
		}
	}
}
