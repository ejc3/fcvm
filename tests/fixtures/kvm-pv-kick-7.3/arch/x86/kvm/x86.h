/*
 * Verbatim line range from Linux v7.3.3 arch/x86/kvm/x86.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/x86/kvm/x86.h
 * Complete source SHA256: 914834e007b2998a2e2ab5ca547967178b2a636e8dd23c3d97fe63f652ae94d2
 * Lines 143-148.
 * tests/test_kernel_patch_invariants.rs compiles this helper into a C harness beside
 * kvm_arch_async_page_present() from the x86.c excerpt.
 */
static inline void kvm_set_mp_state(struct kvm_vcpu *vcpu, int mp_state)
{
	vcpu->arch.mp_state = mp_state;
	if (mp_state == KVM_MP_STATE_RUNNABLE)
		vcpu->arch.pv.pv_unhalted = false;
}
