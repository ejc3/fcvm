/*
 * Verbatim line range from Linux v7.3.3 arch/arm64/include/asm/kvm_emulate.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/include/asm/kvm_emulate.h
 * Complete source SHA256: 92f7093348c599dee6b7901c24b6685fd6ff9d2caaf447f397b73477cf1c8944
 * Lines 190-260.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/nv-guest-hypervisor-exits-without-calls.patch to this file,
 * then checks or compiles the functions it names from it.
 */

static inline bool vcpu_is_el2(const struct kvm_vcpu *vcpu)
{
	return vcpu_is_el2_ctxt(&vcpu->arch.ctxt);
}

static inline bool vcpu_el2_e2h_is_set(const struct kvm_vcpu *vcpu)
{
	return (!cpus_have_final_cap(ARM64_HAS_HCR_NV1) ||
		(__vcpu_sys_reg(vcpu, HCR_EL2) & HCR_E2H));
}

static inline bool vcpu_el2_tge_is_set(const struct kvm_vcpu *vcpu)
{
	return ctxt_sys_reg(&vcpu->arch.ctxt, HCR_EL2) & HCR_TGE;
}

static inline bool vcpu_el2_amo_is_set(const struct kvm_vcpu *vcpu)
{
	/*
	 * DDI0487L.b Known Issue D22105
	 *
	 * When executing at EL2 and HCR_EL2.{E2H,TGE} = {1, 0} it is
	 * IMPLEMENTATION DEFINED whether the effective value of HCR_EL2.AMO
	 * is the value programmed or 1.
	 *
	 * Make the implementation choice of treating the effective value as 1 as
	 * we cannot subsequently catch changes to TGE or AMO that would
	 * otherwise lead to the SError becoming deliverable.
	 */
	if (vcpu_is_el2(vcpu) && vcpu_el2_e2h_is_set(vcpu) && !vcpu_el2_tge_is_set(vcpu))
		return true;

	return ctxt_sys_reg(&vcpu->arch.ctxt, HCR_EL2) & HCR_AMO;
}

static inline bool is_hyp_ctxt(const struct kvm_vcpu *vcpu)
{
	bool e2h, tge;
	u64 hcr;

	if (!vcpu_has_nv(vcpu))
		return false;

	hcr = __vcpu_sys_reg(vcpu, HCR_EL2);

	e2h = (hcr & HCR_E2H);
	tge = (hcr & HCR_TGE);

	/*
	 * We are in a hypervisor context if the vcpu mode is EL2 or
	 * E2H and TGE bits are set. The latter means we are in the user space
	 * of the VHE kernel. ARMv8.1 ARM describes this as 'InHost'
	 *
	 * Note that the HCR_EL2.{E2H,TGE}={0,1} isn't really handled in the
	 * rest of the KVM code, and will result in a misbehaving guest.
	 */
	return vcpu_is_el2(vcpu) || (e2h && tge) || tge;
}

static inline bool vcpu_is_host_el0(const struct kvm_vcpu *vcpu)
{
	return is_hyp_ctxt(vcpu) && !vcpu_is_el2(vcpu);
}

static inline bool is_nested_ctxt(struct kvm_vcpu *vcpu)
{
	return vcpu_has_nv(vcpu) && !is_hyp_ctxt(vcpu);
}

static inline bool vserror_state_is_nested(struct kvm_vcpu *vcpu)
