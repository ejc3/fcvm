/*
 * Verbatim line range from Linux v6.18.60 arch/x86/kvm/x86.h:
 * https://raw.githubusercontent.com/gregkh/linux/v6.18.60/arch/x86/kvm/x86.h
 * Complete source SHA256: 17a11c0948ce0acd814f69ea2b933becca3095aed097a481c8d964c6591c2ed9
 * Lines 194-199.
 * tests/test_kernel_patch_invariants.rs compiles this helper into a C harness beside
 * kvm_arch_async_page_present() from the x86.c excerpt.
 */
static inline void kvm_set_mp_state(struct kvm_vcpu *vcpu, int mp_state)
{
	vcpu->arch.mp_state = mp_state;
	if (mp_state == KVM_MP_STATE_RUNNABLE)
		vcpu->arch.pv.pv_unhalted = false;
}
