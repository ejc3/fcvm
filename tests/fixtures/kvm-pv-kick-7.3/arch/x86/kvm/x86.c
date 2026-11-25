/*
 * Verbatim line range from Linux v7.3.3 arch/x86/kvm/x86.c:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/x86/kvm/x86.c
 * Complete source SHA256: 6cded92f7a275e369117007f0a49aa79c7ce483ae00a91fb4da0362ed1645b43
 * Lines 10582-10607.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-page-ready.patch
 * to this file, then compiles the function it names from it into a C harness.
 */
void kvm_arch_async_page_present(struct kvm_vcpu *vcpu,
				 struct kvm_async_pf *work)
{
	struct kvm_lapic_irq irq = {
		.delivery_mode = APIC_DM_FIXED,
		.vector = vcpu->arch.apf.vec
	};

	if (work->wakeup_all)
		work->arch.token = ~0; /* broadcast wakeup */
	else
		kvm_del_async_pf_gfn(vcpu, work->arch.gfn);
	trace_kvm_async_pf_ready(work->arch.token, work->cr2_or_gpa);

	if ((work->wakeup_all || work->notpresent_injected) &&
	    kvm_pv_async_pf_enabled(vcpu) &&
	    !apf_put_user_ready(vcpu, work->arch.token)) {
		WRITE_ONCE(vcpu->arch.apf.pageready_pending, true);
		kvm_apic_set_irq(vcpu, &irq, NULL);
	}

	vcpu->arch.apf.halted = false;
	kvm_set_mp_state(vcpu, KVM_MP_STATE_RUNNABLE);
}

void kvm_arch_async_page_present_queued(struct kvm_vcpu *vcpu)
