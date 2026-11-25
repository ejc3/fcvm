/*
 * Verbatim line range from Linux v6.16.1 arch/x86/kvm/x86.h:
 * https://raw.githubusercontent.com/gregkh/linux/v6.16.1/arch/x86/kvm/x86.h
 * Complete source SHA256: e4a86fec20efbd46d98a5f747c93f670b515fc7d214a70f61cf3d5bd5e361deb
 * Lines 147-152.
 * tests/test_kernel_patch_invariants.rs compiles this helper into a C harness beside
 * kvm_arch_async_page_present() from the x86.c excerpt.
 */
static inline void kvm_set_mp_state(struct kvm_vcpu *vcpu, int mp_state)
{
	vcpu->arch.mp_state = mp_state;
	if (mp_state == KVM_MP_STATE_RUNNABLE)
		vcpu->arch.pv.pv_unhalted = false;
}
