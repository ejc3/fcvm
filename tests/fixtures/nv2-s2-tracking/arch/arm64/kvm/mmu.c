/*
 * Verbatim line ranges from Linux v6.18.50 arch/arm64/kvm/mmu.c:
 * https://raw.githubusercontent.com/gregkh/linux/v6.18.50/arch/arm64/kvm/mmu.c
 * Complete source SHA256: d9e472facd95407b71deb71559ffff43c0c2a9a6bc359709be4cb4bb464db4a9
 * Lines 1-90, 290-345, 905-1092, 1515-1888, 2085-2110, 2380-2399, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64/nv2-targeted-shadow-s2-unmap.patch to these files,
 * then compiles the functions it names from them into a C harness.
 */
// SPDX-License-Identifier: GPL-2.0-only
/*
 * Copyright (C) 2012 - Virtual Open Systems and Columbia University
 * Author: Christoffer Dall <c.dall@virtualopensystems.com>
 */

#include <linux/acpi.h>
#include <linux/mman.h>
#include <linux/kvm_host.h>
#include <linux/io.h>
#include <linux/hugetlb.h>
#include <linux/sched/signal.h>
#include <trace/events/kvm.h>
#include <asm/acpi.h>
#include <asm/pgalloc.h>
#include <asm/cacheflush.h>
#include <asm/kvm_arm.h>
#include <asm/kvm_mmu.h>
#include <asm/kvm_pgtable.h>
#include <asm/kvm_pkvm.h>
#include <asm/kvm_asm.h>
#include <asm/kvm_emulate.h>
#include <asm/virt.h>

#include "trace.h"

static struct kvm_pgtable *hyp_pgtable;
static DEFINE_MUTEX(kvm_hyp_pgd_mutex);

static unsigned long __ro_after_init hyp_idmap_start;
static unsigned long __ro_after_init hyp_idmap_end;
static phys_addr_t __ro_after_init hyp_idmap_vector;

u32 __ro_after_init __hyp_va_bits;

static unsigned long __ro_after_init io_map_base;

#define KVM_PGT_FN(fn)		(!is_protected_kvm_enabled() ? fn : p ## fn)

static phys_addr_t __stage2_range_addr_end(phys_addr_t addr, phys_addr_t end,
					   phys_addr_t size)
{
	phys_addr_t boundary = ALIGN_DOWN(addr + size, size);

	return (boundary - 1 < end - 1) ? boundary : end;
}

static phys_addr_t stage2_range_addr_end(phys_addr_t addr, phys_addr_t end)
{
	phys_addr_t size = kvm_granule_size(KVM_PGTABLE_MIN_BLOCK_LEVEL);

	return __stage2_range_addr_end(addr, end, size);
}

/*
 * Release kvm_mmu_lock periodically if the memory region is large. Otherwise,
 * we may see kernel panics with CONFIG_DETECT_HUNG_TASK,
 * CONFIG_LOCKUP_DETECTOR, CONFIG_LOCKDEP. Additionally, holding the lock too
 * long will also starve other vCPUs. We have to also make sure that the page
 * tables are not freed while we released the lock.
 */
static int stage2_apply_range(struct kvm_s2_mmu *mmu, phys_addr_t addr,
			      phys_addr_t end,
			      int (*fn)(struct kvm_pgtable *, u64, u64),
			      bool resched)
{
	struct kvm *kvm = kvm_s2_mmu_to_kvm(mmu);
	int ret;
	u64 next;

	do {
		struct kvm_pgtable *pgt = mmu->pgt;
		if (!pgt)
			return -EINVAL;

		next = stage2_range_addr_end(addr, end);
		ret = fn(pgt, addr, next - addr);
		if (ret)
			break;

		if (resched && next != end)
			cond_resched_rwlock_write(&kvm->mmu_lock);
	} while (addr = next, addr != end);

	return ret;
}

#define stage2_apply_range_resched(mmu, addr, end, fn)			\
	stage2_apply_range(mmu, addr, end, fn, true)


}

/*
 * Unmapping vs dcache management:
 *
 * If a guest maps certain memory pages as uncached, all writes will
 * bypass the data cache and go directly to RAM.  However, the CPUs
 * can still speculate reads (not writes) and fill cache lines with
 * data.
 *
 * Those cache lines will be *clean* cache lines though, so a
 * clean+invalidate operation is equivalent to an invalidate
 * operation, because no cache lines are marked dirty.
 *
 * Those clean cache lines could be filled prior to an uncached write
 * by the guest, and the cache coherent IO subsystem would therefore
 * end up writing old data to disk.
 *
 * This is why right after unmapping a page/section and invalidating
 * the corresponding TLBs, we flush to make sure the IO subsystem will
 * never hit in the cache.
 *
 * This is all avoided on systems that have ARM64_HAS_STAGE2_FWB, as
 * we then fully enforce cacheability of RAM, no matter what the guest
 * does.
 */
/**
 * __unmap_stage2_range -- Clear stage2 page table entries to unmap a range
 * @mmu:   The KVM stage-2 MMU pointer
 * @start: The intermediate physical base address of the range to unmap
 * @size:  The size of the area to unmap
 * @may_block: Whether or not we are permitted to block
 *
 * Clear a range of stage-2 mappings, lowering the various ref-counts.  Must
 * be called while holding mmu_lock (unless for freeing the stage2 pgd before
 * destroying the VM), otherwise another faulting VCPU may come in and mess
 * with things behind our backs.
 */
static void __unmap_stage2_range(struct kvm_s2_mmu *mmu, phys_addr_t start, u64 size,
				 bool may_block)
{
	struct kvm *kvm = kvm_s2_mmu_to_kvm(mmu);
	phys_addr_t end = start + size;

	lockdep_assert_held_write(&kvm->mmu_lock);
	WARN_ON(size & ~PAGE_MASK);
	WARN_ON(stage2_apply_range(mmu, start, end, KVM_PGT_FN(kvm_pgtable_stage2_unmap),
				   may_block));
}

void kvm_stage2_unmap_range(struct kvm_s2_mmu *mmu, phys_addr_t start,
			    u64 size, bool may_block)
{
	__unmap_stage2_range(mmu, start, size, may_block);
}


}

/**
 * kvm_init_stage2_mmu - Initialise a S2 MMU structure
 * @kvm:	The pointer to the KVM structure
 * @mmu:	The pointer to the s2 MMU structure
 * @type:	The machine type of the virtual machine
 *
 * Allocates only the stage-2 HW PGD level table(s).
 * Note we don't need locking here as this is only called in two cases:
 *
 * - when the VM is created, which can't race against anything
 *
 * - when secondary kvm_s2_mmu structures are initialised for NV
 *   guests, and the caller must hold kvm->lock as this is called on a
 *   per-vcpu basis.
 */
int kvm_init_stage2_mmu(struct kvm *kvm, struct kvm_s2_mmu *mmu, unsigned long type)
{
	int cpu, err;
	struct kvm_pgtable *pgt;

	/*
	 * If we already have our page tables in place, and that the
	 * MMU context is the canonical one, we have a bug somewhere,
	 * as this is only supposed to ever happen once per VM.
	 *
	 * Otherwise, we're building nested page tables, and that's
	 * probably because userspace called KVM_ARM_VCPU_INIT more
	 * than once on the same vcpu. Since that's actually legal,
	 * don't kick a fuss and leave gracefully.
	 */
	if (mmu->pgt != NULL) {
		if (kvm_is_nested_s2_mmu(kvm, mmu))
			return 0;

		kvm_err("kvm_arch already initialized?\n");
		return -EINVAL;
	}

	err = kvm_init_ipa_range(mmu, type);
	if (err)
		return err;

	pgt = kzalloc(sizeof(*pgt), GFP_KERNEL_ACCOUNT);
	if (!pgt)
		return -ENOMEM;

	mmu->arch = &kvm->arch;
	err = KVM_PGT_FN(kvm_pgtable_stage2_init)(pgt, mmu, &kvm_s2_mm_ops);
	if (err)
		goto out_free_pgtable;

	mmu->pgt = pgt;
	if (is_protected_kvm_enabled())
		return 0;

	mmu->last_vcpu_ran = alloc_percpu(typeof(*mmu->last_vcpu_ran));
	if (!mmu->last_vcpu_ran) {
		err = -ENOMEM;
		goto out_destroy_pgtable;
	}

	for_each_possible_cpu(cpu)
		*per_cpu_ptr(mmu->last_vcpu_ran, cpu) = -1;

	 /* The eager page splitting is disabled by default */
	mmu->split_page_chunk_size = KVM_ARM_EAGER_SPLIT_CHUNK_SIZE_DEFAULT;
	mmu->split_page_cache.gfp_zero = __GFP_ZERO;

	mmu->pgd_phys = __pa(pgt->pgd);

	if (kvm_is_nested_s2_mmu(kvm, mmu))
		kvm_init_nested_s2_mmu(mmu);

	return 0;

out_destroy_pgtable:
	KVM_PGT_FN(kvm_pgtable_stage2_destroy)(pgt);
out_free_pgtable:
	kfree(pgt);
	return err;
}

void kvm_uninit_stage2_mmu(struct kvm *kvm)
{
	kvm_free_stage2_pgd(&kvm->arch.mmu);
	kvm_mmu_free_memory_cache(&kvm->arch.mmu.split_page_cache);
}

static void stage2_unmap_memslot(struct kvm *kvm,
				 struct kvm_memory_slot *memslot)
{
	hva_t hva = memslot->userspace_addr;
	phys_addr_t addr = memslot->base_gfn << PAGE_SHIFT;
	phys_addr_t size = PAGE_SIZE * memslot->npages;
	hva_t reg_end = hva + size;

	/*
	 * A memory region could potentially cover multiple VMAs, and any holes
	 * between them, so iterate over all of them to find out if we should
	 * unmap any of them.
	 *
	 *     +--------------------------------------------+
	 * +---------------+----------------+   +----------------+
	 * |   : VMA 1     |      VMA 2     |   |    VMA 3  :    |
	 * +---------------+----------------+   +----------------+
	 *     |               memory region                |
	 *     +--------------------------------------------+
	 */
	do {
		struct vm_area_struct *vma;
		hva_t vm_start, vm_end;

		vma = find_vma_intersection(current->mm, hva, reg_end);
		if (!vma)
			break;

		/*
		 * Take the intersection of this VMA with the memory region
		 */
		vm_start = max(hva, vma->vm_start);
		vm_end = min(reg_end, vma->vm_end);

		if (!(vma->vm_flags & VM_PFNMAP)) {
			gpa_t gpa = addr + (vm_start - memslot->userspace_addr);
			kvm_stage2_unmap_range(&kvm->arch.mmu, gpa, vm_end - vm_start, true);
		}
		hva = vm_end;
	} while (hva < reg_end);
}

/**
 * stage2_unmap_vm - Unmap Stage-2 RAM mappings
 * @kvm: The struct kvm pointer
 *
 * Go through the memregions and unmap any regular RAM
 * backing memory already mapped to the VM.
 */
void stage2_unmap_vm(struct kvm *kvm)
{
	struct kvm_memslots *slots;
	struct kvm_memory_slot *memslot;
	int idx, bkt;

	idx = srcu_read_lock(&kvm->srcu);
	mmap_read_lock(current->mm);
	write_lock(&kvm->mmu_lock);

	slots = kvm_memslots(kvm);
	kvm_for_each_memslot(memslot, bkt, slots)
		stage2_unmap_memslot(kvm, memslot);

	kvm_nested_s2_unmap(kvm, true);

	write_unlock(&kvm->mmu_lock);
	mmap_read_unlock(current->mm);
	srcu_read_unlock(&kvm->srcu, idx);
}

void kvm_free_stage2_pgd(struct kvm_s2_mmu *mmu)
{
	struct kvm *kvm = kvm_s2_mmu_to_kvm(mmu);
	struct kvm_pgtable *pgt = NULL;

	write_lock(&kvm->mmu_lock);
	pgt = mmu->pgt;
	if (pgt) {
		mmu->pgd_phys = 0;
		mmu->pgt = NULL;
		free_percpu(mmu->last_vcpu_ran);
	}

	if (kvm_is_nested_s2_mmu(kvm, mmu))
		kvm_init_nested_s2_mmu(mmu);

	write_unlock(&kvm->mmu_lock);

	if (pgt) {
		KVM_PGT_FN(kvm_pgtable_stage2_destroy)(pgt);
		kfree(pgt);
	}
}

static void hyp_mc_free_fn(void *addr, void *mc)
{
	struct kvm_hyp_memcache *memcache = mc;


 * TLB invalidation from the guest and used to limit the invalidation scope if a
 * TTL hint or a range isn't provided.
 */
static void adjust_nested_fault_perms(struct kvm_s2_trans *nested,
				      enum kvm_pgtable_prot *prot,
				      bool *writable)
{
	*writable &= kvm_s2_trans_writable(nested);
	if (!kvm_s2_trans_readable(nested))
		*prot &= ~KVM_PGTABLE_PROT_R;

	*prot |= kvm_encode_nested_level(nested);
}

#define KVM_PGTABLE_WALK_MEMABORT_FLAGS (KVM_PGTABLE_WALK_HANDLE_FAULT | KVM_PGTABLE_WALK_SHARED)

static int gmem_abort(struct kvm_vcpu *vcpu, phys_addr_t fault_ipa,
		      struct kvm_s2_trans *nested,
		      struct kvm_memory_slot *memslot, bool is_perm)
{
	bool write_fault, exec_fault, writable;
	enum kvm_pgtable_walk_flags flags = KVM_PGTABLE_WALK_MEMABORT_FLAGS;
	enum kvm_pgtable_prot prot = KVM_PGTABLE_PROT_R;
	struct kvm_pgtable *pgt = vcpu->arch.hw_mmu->pgt;
	unsigned long mmu_seq;
	struct page *page;
	struct kvm *kvm = vcpu->kvm;
	void *memcache;
	kvm_pfn_t pfn;
	gfn_t gfn;
	int ret;

	ret = prepare_mmu_memcache(vcpu, true, &memcache);
	if (ret)
		return ret;

	if (nested)
		gfn = kvm_s2_trans_output(nested) >> PAGE_SHIFT;
	else
		gfn = fault_ipa >> PAGE_SHIFT;

	write_fault = kvm_is_write_fault(vcpu);
	exec_fault = kvm_vcpu_trap_is_exec_fault(vcpu);

	VM_WARN_ON_ONCE(write_fault && exec_fault);

	mmu_seq = kvm->mmu_invalidate_seq;
	/* Pairs with the smp_wmb() in kvm_mmu_invalidate_end(). */
	smp_rmb();

	ret = kvm_gmem_get_pfn(kvm, memslot, gfn, &pfn, &page, NULL);
	if (ret) {
		kvm_prepare_memory_fault_exit(vcpu, fault_ipa, PAGE_SIZE,
					      write_fault, exec_fault, false);
		return ret;
	}

	writable = !(memslot->flags & KVM_MEM_READONLY);

	if (nested)
		adjust_nested_fault_perms(nested, &prot, &writable);

	if (writable)
		prot |= KVM_PGTABLE_PROT_W;

	if (exec_fault ||
	    (cpus_have_final_cap(ARM64_HAS_CACHE_DIC) &&
	     (!nested || kvm_s2_trans_executable(nested))))
		prot |= KVM_PGTABLE_PROT_X;

	kvm_fault_lock(kvm);
	if (mmu_invalidate_retry(kvm, mmu_seq)) {
		ret = -EAGAIN;
		goto out_unlock;
	}

	ret = KVM_PGT_FN(kvm_pgtable_stage2_map)(pgt, fault_ipa, PAGE_SIZE,
						 __pfn_to_phys(pfn), prot,
						 memcache, flags);

out_unlock:
	kvm_release_faultin_page(kvm, page, !!ret, writable);
	kvm_fault_unlock(kvm);

	if (writable && !ret)
		mark_page_dirty_in_slot(kvm, memslot, gfn);

	return ret != -EAGAIN ? ret : 0;
}

static int user_mem_abort(struct kvm_vcpu *vcpu, phys_addr_t fault_ipa,
			  struct kvm_s2_trans *nested,
			  struct kvm_memory_slot *memslot, unsigned long hva,
			  bool fault_is_perm)
{
	int ret = 0;
	bool topup_memcache;
	bool write_fault, writable;
	bool exec_fault, mte_allowed, is_vma_cacheable;
	bool s2_force_noncacheable = false, vfio_allow_any_uc = false;
	unsigned long mmu_seq;
	phys_addr_t ipa = fault_ipa;
	struct kvm *kvm = vcpu->kvm;
	struct vm_area_struct *vma;
	short vma_shift;
	void *memcache;
	gfn_t gfn;
	kvm_pfn_t pfn;
	bool logging_active = memslot_is_logging(memslot);
	bool force_pte = logging_active;
	long vma_pagesize, fault_granule;
	enum kvm_pgtable_prot prot = KVM_PGTABLE_PROT_R;
	struct kvm_pgtable *pgt;
	struct page *page;
	vm_flags_t vm_flags;
	enum kvm_pgtable_walk_flags flags = KVM_PGTABLE_WALK_MEMABORT_FLAGS;

	if (fault_is_perm)
		fault_granule = kvm_vcpu_trap_get_perm_fault_granule(vcpu);
	write_fault = kvm_is_write_fault(vcpu);
	exec_fault = kvm_vcpu_trap_is_exec_fault(vcpu);
	VM_WARN_ON_ONCE(write_fault && exec_fault);

	/*
	 * Permission faults just need to update the existing leaf entry,
	 * and so normally don't require allocations from the memcache. The
	 * only exception to this is when dirty logging is enabled at runtime
	 * and a write fault needs to collapse a block entry into a table.
	 */
	topup_memcache = !fault_is_perm || (logging_active && write_fault);
	ret = prepare_mmu_memcache(vcpu, topup_memcache, &memcache);
	if (ret)
		return ret;

	/*
	 * Let's check if we will get back a huge page backed by hugetlbfs, or
	 * get block mapping for device MMIO region.
	 */
	mmap_read_lock(current->mm);
	vma = vma_lookup(current->mm, hva);
	if (unlikely(!vma)) {
		kvm_err("Failed to find VMA for hva 0x%lx\n", hva);
		mmap_read_unlock(current->mm);
		return -EFAULT;
	}

	if (force_pte)
		vma_shift = PAGE_SHIFT;
	else
		vma_shift = get_vma_page_shift(vma, hva);

	switch (vma_shift) {
#ifndef __PAGETABLE_PMD_FOLDED
	case PUD_SHIFT:
		if (fault_supports_stage2_huge_mapping(memslot, hva, PUD_SIZE))
			break;
		fallthrough;
#endif
	case CONT_PMD_SHIFT:
		vma_shift = PMD_SHIFT;
		fallthrough;
	case PMD_SHIFT:
		if (fault_supports_stage2_huge_mapping(memslot, hva, PMD_SIZE))
			break;
		fallthrough;
	case CONT_PTE_SHIFT:
		vma_shift = PAGE_SHIFT;
		force_pte = true;
		fallthrough;
	case PAGE_SHIFT:
		break;
	default:
		WARN_ONCE(1, "Unknown vma_shift %d", vma_shift);
	}

	vma_pagesize = 1UL << vma_shift;

	if (nested) {
		unsigned long max_map_size;

		max_map_size = force_pte ? PAGE_SIZE : PUD_SIZE;

		ipa = kvm_s2_trans_output(nested);

		/*
		 * If we're about to create a shadow stage 2 entry, then we
		 * can only create a block mapping if the guest stage 2 page
		 * table uses at least as big a mapping.
		 */
		max_map_size = min(kvm_s2_trans_size(nested), max_map_size);

		/*
		 * Be careful that if the mapping size falls between
		 * two host sizes, take the smallest of the two.
		 */
		if (max_map_size >= PMD_SIZE && max_map_size < PUD_SIZE)
			max_map_size = PMD_SIZE;
		else if (max_map_size >= PAGE_SIZE && max_map_size < PMD_SIZE)
			max_map_size = PAGE_SIZE;

		force_pte = (max_map_size == PAGE_SIZE);
		vma_pagesize = min_t(long, vma_pagesize, max_map_size);
	}

	/*
	 * Both the canonical IPA and fault IPA must be aligned to the
	 * mapping size to ensure we find the right PFN and lay down the
	 * mapping in the right place.
	 */
	fault_ipa = ALIGN_DOWN(fault_ipa, vma_pagesize);
	ipa = ALIGN_DOWN(ipa, vma_pagesize);

	gfn = ipa >> PAGE_SHIFT;
	mte_allowed = kvm_vma_mte_allowed(vma);

	vfio_allow_any_uc = vma->vm_flags & VM_ALLOW_ANY_UNCACHED;

	vm_flags = vma->vm_flags;

	is_vma_cacheable = kvm_vma_is_cacheable(vma);

	/* Don't use the VMA after the unlock -- it may have vanished */
	vma = NULL;

	/*
	 * Read mmu_invalidate_seq so that KVM can detect if the results of
	 * vma_lookup() or __kvm_faultin_pfn() become stale prior to
	 * acquiring kvm->mmu_lock.
	 *
	 * Rely on mmap_read_unlock() for an implicit smp_rmb(), which pairs
	 * with the smp_wmb() in kvm_mmu_invalidate_end().
	 */
	mmu_seq = kvm->mmu_invalidate_seq;
	mmap_read_unlock(current->mm);

	pfn = __kvm_faultin_pfn(memslot, gfn, write_fault ? FOLL_WRITE : 0,
				&writable, &page);
	if (pfn == KVM_PFN_ERR_HWPOISON) {
		kvm_send_hwpoison_signal(hva, vma_shift);
		return 0;
	}
	if (is_error_noslot_pfn(pfn))
		return -EFAULT;

	/*
	 * Check if this is non-struct page memory PFN, and cannot support
	 * CMOs. It could potentially be unsafe to access as cachable.
	 */
	if (vm_flags & (VM_PFNMAP | VM_MIXEDMAP) && !pfn_is_map_memory(pfn)) {
		if (is_vma_cacheable) {
			/*
			 * Whilst the VMA owner expects cacheable mapping to this
			 * PFN, hardware also has to support the FWB and CACHE DIC
			 * features.
			 *
			 * ARM64 KVM relies on kernel VA mapping to the PFN to
			 * perform cache maintenance as the CMO instructions work on
			 * virtual addresses. VM_PFNMAP region are not necessarily
			 * mapped to a KVA and hence the presence of hardware features
			 * S2FWB and CACHE DIC are mandatory to avoid the need for
			 * cache maintenance.
			 */
			if (!kvm_supports_cacheable_pfnmap())
				ret = -EFAULT;
		} else {
			/*
			 * If the page was identified as device early by looking at
			 * the VMA flags, vma_pagesize is already representing the
			 * largest quantity we can map.  If instead it was mapped
			 * via __kvm_faultin_pfn(), vma_pagesize is set to PAGE_SIZE
			 * and must not be upgraded.
			 *
			 * In both cases, we don't let transparent_hugepage_adjust()
			 * change things at the last minute.
			 */
			s2_force_noncacheable = true;
		}
	} else if (logging_active && !write_fault) {
		/*
		 * Only actually map the page as writable if this was a write
		 * fault.
		 */
		writable = false;
	}

	if (exec_fault && s2_force_noncacheable)
		ret = -ENOEXEC;

	if (ret) {
		kvm_release_page_unused(page);
		return ret;
	}

	if (nested)
		adjust_nested_fault_perms(nested, &prot, &writable);

	kvm_fault_lock(kvm);
	pgt = vcpu->arch.hw_mmu->pgt;
	if (mmu_invalidate_retry(kvm, mmu_seq)) {
		ret = -EAGAIN;
		goto out_unlock;
	}

	/*
	 * If we are not forced to use page mapping, check if we are
	 * backed by a THP and thus use block mapping if possible.
	 */
	if (vma_pagesize == PAGE_SIZE && !(force_pte || s2_force_noncacheable)) {
		if (fault_is_perm && fault_granule > PAGE_SIZE)
			vma_pagesize = fault_granule;
		else
			vma_pagesize = transparent_hugepage_adjust(kvm, memslot,
								   hva, &pfn,
								   &fault_ipa);

		if (vma_pagesize < 0) {
			ret = vma_pagesize;
			goto out_unlock;
		}
	}

	if (!fault_is_perm && !s2_force_noncacheable && kvm_has_mte(kvm)) {
		/* Check the VMM hasn't introduced a new disallowed VMA */
		if (mte_allowed) {
			sanitise_mte_tags(kvm, pfn, vma_pagesize);
		} else {
			ret = -EFAULT;
			goto out_unlock;
		}
	}

	if (writable)
		prot |= KVM_PGTABLE_PROT_W;

	if (exec_fault)
		prot |= KVM_PGTABLE_PROT_X;

	if (s2_force_noncacheable) {
		if (vfio_allow_any_uc)
			prot |= KVM_PGTABLE_PROT_NORMAL_NC;
		else
			prot |= KVM_PGTABLE_PROT_DEVICE;
	} else if (cpus_have_final_cap(ARM64_HAS_CACHE_DIC) &&
		   (!nested || kvm_s2_trans_executable(nested))) {
		prot |= KVM_PGTABLE_PROT_X;
	}

	/*
	 * Under the premise of getting a FSC_PERM fault, we just need to relax
	 * permissions only if vma_pagesize equals fault_granule. Otherwise,
	 * kvm_pgtable_stage2_map() should be called to change block size.
	 */
	if (fault_is_perm && vma_pagesize == fault_granule) {
		/*
		 * Drop the SW bits in favour of those stored in the
		 * PTE, which will be preserved.
		 */
		prot &= ~KVM_NV_GUEST_MAP_SZ;
		ret = KVM_PGT_FN(kvm_pgtable_stage2_relax_perms)(pgt, fault_ipa, prot, flags);
	} else {
		ret = KVM_PGT_FN(kvm_pgtable_stage2_map)(pgt, fault_ipa, vma_pagesize,
					     __pfn_to_phys(pfn), prot,
					     memcache, flags);
	}

out_unlock:
	kvm_release_faultin_page(kvm, page, !!ret, writable);
	kvm_fault_unlock(kvm);

	/* Mark the page dirty only if the fault is handled successfully */
	if (writable && !ret)
		mark_page_dirty_in_slot(kvm, memslot, gfn);

	return ret != -EAGAIN ? ret : 0;

	else
		ret = user_mem_abort(vcpu, fault_ipa, nested, memslot, hva,
				     esr_fsc_is_permission_fault(esr));
	if (ret == 0)
		ret = 1;
out:
	if (ret == -ENOEXEC)
		ret = kvm_inject_sea_iabt(vcpu, kvm_vcpu_get_hfar(vcpu));
out_unlock:
	srcu_read_unlock(&vcpu->kvm->srcu, idx);
	return ret;
}

bool kvm_unmap_gfn_range(struct kvm *kvm, struct kvm_gfn_range *range)
{
	if (!kvm->arch.mmu.pgt)
		return false;

	__unmap_stage2_range(&kvm->arch.mmu, range->start << PAGE_SHIFT,
			     (range->end - range->start) << PAGE_SHIFT,
			     range->may_block);

	kvm_nested_s2_unmap(kvm, range->may_block);
	return false;
}


void kvm_arch_free_memslot(struct kvm *kvm, struct kvm_memory_slot *slot)
{
}

void kvm_arch_memslots_updated(struct kvm *kvm, u64 gen)
{
}

void kvm_arch_flush_shadow_memslot(struct kvm *kvm,
				   struct kvm_memory_slot *slot)
{
	gpa_t gpa = slot->base_gfn << PAGE_SHIFT;
	phys_addr_t size = slot->npages << PAGE_SHIFT;

	write_lock(&kvm->mmu_lock);
	kvm_stage2_unmap_range(&kvm->arch.mmu, gpa, size, true);
	kvm_nested_s2_unmap(kvm, true);
	write_unlock(&kvm->mmu_lock);
}

