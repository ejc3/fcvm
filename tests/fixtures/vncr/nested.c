/* SPDX-License-Identifier: GPL-2.0-only */
/* Copyright (C) 2017 - Columbia University and Linaro Ltd.
 * Author: Jintack Lim <jintack.lim@linaro.org>
 */
/*
 * Verbatim function excerpts from Linux v6.18.60 arch/arm64/kvm/nested.c:
 * https://raw.githubusercontent.com/gregkh/linux/v6.18.60/arch/arm64/kvm/nested.c
 * Complete source SHA256: 725634c89893a145c50c1aa1fee97e76d53eed9f65c428041b5ca5ddb5ddb93f
 * kvm_vcpu_load_hw_mmu is retained as context. unmap_l1_vncr and
 * this_cpu_reset_vncr_fixmap are what the put and translation paths call.
 * The pinned release carries upstream 5949004d7032 itself, so the test
 * executes this C unpatched.
 */

void kvm_vcpu_load_hw_mmu(struct kvm_vcpu *vcpu)
{
	/*
	 * If the vCPU kept its reference on the MMU after the last put,
	 * keep rolling with it.
	 */
	if (is_hyp_ctxt(vcpu)) {
		if (!vcpu->arch.hw_mmu)
			vcpu->arch.hw_mmu = &vcpu->kvm->arch.mmu;
	} else {
		if (!vcpu->arch.hw_mmu) {
			scoped_guard(write_lock, &vcpu->kvm->mmu_lock)
				vcpu->arch.hw_mmu = get_s2_mmu_nested(vcpu);
		}

		if (__vcpu_sys_reg(vcpu, HCR_EL2) & HCR_NV)
			kvm_make_request(KVM_REQ_MAP_L1_VNCR_EL2, vcpu);
	}
}

/*
 * Unmapping an L1 VNCR can happen concurrently without the mmu lock being
 * effective (vcpu_put() vs TLBI handling). The atomic_xchg below ensures
 * that only one CPU sets it to -1 while getting a valid CPU number back.
 */
static int unmap_l1_vncr(struct vncr_tlb *vt)
{
	int cpu = atomic_xchg_relaxed(&vt->cpu, -1);

	if (cpu != -1)
		clear_fixmap(vncr_fixmap(cpu));

	return cpu;
}

static void this_cpu_reset_vncr_fixmap(struct kvm_vcpu *vcpu)
{
	if (!host_data_test_flag(L1_VNCR_MAPPED))
		return;

	BUG_ON(is_hyp_ctxt(vcpu));

	/*
	 * Unconditionally unmap the local VNCR if we have lost the race
	 * against a concurrent TLBI. Otherwise we could end-up running
	 * another vcpu with VNCR still mapped if the TLBI thread is
	 * preempted between the exchange and the clear_fixmap().
	 *
	 * Note that we do not care about the TLBI nuking the fixmap behind
	 * the back of an running vcpu. This will only generate a fault and
	 * possibly a retranslation.
	 */
	if (unmap_l1_vncr(vcpu->arch.vncr_tlb) == -1)
		clear_fixmap(vncr_fixmap(smp_processor_id()));
	host_data_clear_flag(L1_VNCR_MAPPED);
}

void kvm_vcpu_put_hw_mmu(struct kvm_vcpu *vcpu)
{
	/* Unconditionally drop the VNCR mapping if we have one */
	this_cpu_reset_vncr_fixmap(vcpu);

	/*
	 * Keep a reference on the associated stage-2 MMU if the vCPU is
	 * scheduling out and not in WFI emulation, suggesting it is likely to
	 * reuse the MMU sometime soon.
	 */
	if (vcpu->scheduled_out && !vcpu_get_flag(vcpu, IN_WFI))
		return;

	if (kvm_is_nested_s2_mmu(vcpu->kvm, vcpu->arch.hw_mmu))
		atomic_dec(&vcpu->arch.hw_mmu->refcnt);

	vcpu->arch.hw_mmu = NULL;
}

static int kvm_translate_vncr(struct kvm_vcpu *vcpu, bool *is_gmem)
{
	struct kvm_memory_slot *memslot;
	bool write_fault, writable;
	unsigned long mmu_seq;
	struct vncr_tlb *vt;
	struct page *page;
	u64 va, pfn, gfn;
	int ret;

	vt = vcpu->arch.vncr_tlb;

	/*
	 * If we're about to walk the EL2 S1 PTs, we must invalidate the
	 * current TLB, as it could be sampled from another vcpu doing a
	 * TLBI *IS. A real CPU wouldn't do that, but we only keep a single
	 * translation, so not much of a choice.
	 *
	 * We also prepare the next walk wilst we're at it.
	 */
	scoped_guard(write_lock, &vcpu->kvm->mmu_lock) {
		this_cpu_reset_vncr_fixmap(vcpu);
		vt->valid = false;

		vt->wi = (struct s1_walk_info) {
			.regime	= TR_EL20,
			.as_el0	= false,
			.pan	= false,
		};
		vt->wr = (struct s1_walk_result){};
	}

	guard(srcu)(&vcpu->kvm->srcu);

	va =  read_vncr_el2(vcpu);

	mmu_seq = vcpu->kvm->mmu_invalidate_seq;
	smp_rmb();

	ret = __kvm_translate_va(vcpu, &vt->wi, &vt->wr, va);
	if (ret)
		return ret;

	write_fault = kvm_is_write_fault(vcpu);

	gfn = vt->wr.pa >> PAGE_SHIFT;
	memslot = gfn_to_memslot(vcpu->kvm, gfn);
	if (!memslot) {
		fail_s1_walk(&vt->wr, ESR_ELx_FSC_EXTABT, false);
		return -EFAULT;
	}

	*is_gmem = kvm_slot_has_gmem(memslot);
	if (!*is_gmem) {
		pfn = __kvm_faultin_pfn(memslot, gfn, write_fault ? FOLL_WRITE : 0,
					&writable, &page);
		if (is_error_noslot_pfn(pfn)) {
			fail_s1_walk(&vt->wr, ESR_ELx_FSC_EXTABT, false);
			return -EFAULT;
		}
	} else {
		ret = kvm_gmem_get_pfn(vcpu->kvm, memslot, gfn, &pfn, &page, NULL);
		if (ret) {
			kvm_prepare_memory_fault_exit(vcpu, vt->wr.pa, PAGE_SIZE,
					      write_fault, false, false);
			return ret;
		}

		writable = !(memslot->flags & KVM_MEM_READONLY);
	}

	/*
	 * FIXME: This check is too restrictive as KVM allows cacheable memory
	 * attributes for PFNMAP VMAs that have cacheable attributes in host
	 * stage-1.
	 */
	if (!pfn_is_map_memory(pfn)) {
		kvm_release_faultin_page(vcpu->kvm, page, true, false);
		fail_s1_walk(&vt->wr, ESR_ELx_FSC_EXTABT, false);
		return -EINVAL;
	}

	scoped_guard(write_lock, &vcpu->kvm->mmu_lock) {
		if (mmu_invalidate_retry(vcpu->kvm, mmu_seq)) {
			kvm_release_faultin_page(vcpu->kvm, page, true, false);
			return -EAGAIN;
		}

		vt->gva = va;
		vt->hpa = pfn << PAGE_SHIFT;
		vt->hpa_writable = writable;
		vt->valid = true;
		atomic_set(&vt->cpu, -1);

		kvm_make_request(KVM_REQ_MAP_L1_VNCR_EL2, vcpu);
		kvm_release_faultin_page(vcpu->kvm, page, false, vt->wr.pw && vt->hpa_writable);
	}

	if (vt->wr.pw && vt->hpa_writable)
		mark_page_dirty(vcpu->kvm, gfn);

	return 0;
}
