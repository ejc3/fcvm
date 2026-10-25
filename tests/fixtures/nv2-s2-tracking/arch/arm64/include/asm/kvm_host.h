/*
 * Verbatim line ranges from Linux v6.18.50 arch/arm64/include/asm/kvm_host.h:
 * https://raw.githubusercontent.com/gregkh/linux/v6.18.50/arch/arm64/include/asm/kvm_host.h
 * Complete source SHA256: 39909c872471b008139145a2fbc88c0b82a19f22e37771b3d567233185552964
 * Lines 1-40, 140-230, 295-320, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64/nv2-targeted-shadow-s2-unmap.patch to these files,
 * then compiles the functions it names from them into a C harness.
 */
/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Copyright (C) 2012,2013 - ARM Ltd
 * Author: Marc Zyngier <marc.zyngier@arm.com>
 *
 * Derived from arch/arm/include/asm/kvm_host.h:
 * Copyright (C) 2012 - Virtual Open Systems and Columbia University
 * Author: Christoffer Dall <c.dall@virtualopensystems.com>
 */

#ifndef __ARM64_KVM_HOST_H__
#define __ARM64_KVM_HOST_H__

#include <linux/arm-smccc.h>
#include <linux/bitmap.h>
#include <linux/types.h>
#include <linux/jump_label.h>
#include <linux/kvm_types.h>
#include <linux/maple_tree.h>
#include <linux/percpu.h>
#include <linux/psci.h>
#include <asm/arch_gicv3.h>
#include <asm/barrier.h>
#include <asm/cpufeature.h>
#include <asm/cputype.h>
#include <asm/daifflags.h>
#include <asm/fpsimd.h>
#include <asm/kvm.h>
#include <asm/kvm_asm.h>
#include <asm/vncr_mapping.h>

#define __KVM_HAVE_ARCH_INTC_INITIALIZED

#define KVM_HALT_POLL_NS_DEFAULT 500000

#include <kvm/arm_vgic.h>
#include <kvm/arm_arch_timer.h>
#include <kvm/arm_pmu.h>

#define KVM_MAX_VCPUS VGIC_V3_MAX_CPUS

{
	while (mc->nr_pages)
		free_fn(pop_hyp_memcache(mc, to_va), arg);
}

void free_hyp_memcache(struct kvm_hyp_memcache *mc);
int topup_hyp_memcache(struct kvm_hyp_memcache *mc, unsigned long min_pages);

struct kvm_vmid {
	atomic64_t id;
};

struct kvm_s2_mmu {
	struct kvm_vmid vmid;

	/*
	 * stage2 entry level table
	 *
	 * Two kvm_s2_mmu structures in the same VM can point to the same
	 * pgd here.  This happens when running a guest using a
	 * translation regime that isn't affected by its own stage-2
	 * translation, such as a non-VHE hypervisor running at vEL2, or
	 * for vEL1/EL0 with vHCR_EL2.VM == 0.  In that case, we use the
	 * canonical stage-2 page tables.
	 */
	phys_addr_t	pgd_phys;
	struct kvm_pgtable *pgt;

	/*
	 * VTCR value used on the host. For a non-NV guest (or a NV
	 * guest that runs in a context where its own S2 doesn't
	 * apply), its T0SZ value reflects that of the IPA size.
	 *
	 * For a shadow S2 MMU, T0SZ reflects the PARange exposed to
	 * the guest.
	 */
	u64	vtcr;

	/* The last vcpu id that ran on each physical CPU */
	int __percpu *last_vcpu_ran;

#define KVM_ARM_EAGER_SPLIT_CHUNK_SIZE_DEFAULT 0
	/*
	 * Memory cache used to split
	 * KVM_CAP_ARM_EAGER_SPLIT_CHUNK_SIZE worth of huge pages. It
	 * is used to allocate stage2 page tables while splitting huge
	 * pages. The choice of KVM_CAP_ARM_EAGER_SPLIT_CHUNK_SIZE
	 * influences both the capacity of the split page cache, and
	 * how often KVM reschedules. Be wary of raising CHUNK_SIZE
	 * too high.
	 *
	 * Protected by kvm->slots_lock.
	 */
	struct kvm_mmu_memory_cache split_page_cache;
	uint64_t split_page_chunk_size;

	struct kvm_arch *arch;

	/*
	 * For a shadow stage-2 MMU, the virtual vttbr used by the
	 * host to parse the guest S2.
	 * This either contains:
	 * - the virtual VTTBR programmed by the guest hypervisor with
         *   CnP cleared
	 * - The value 1 (VMID=0, BADDR=0, CnP=1) if invalid
	 *
	 * We also cache the full VTCR which gets used for TLB invalidation,
	 * taking the ARM ARM's "Any of the bits in VTCR_EL2 are permitted
	 * to be cached in a TLB" to the letter.
	 */
	u64	tlb_vttbr;
	u64	tlb_vtcr;

	/*
	 * true when this represents a nested context where virtual
	 * HCR_EL2.VM == 1
	 */
	bool	nested_stage2_enabled;

	/*
	 * true when this MMU needs to be unmapped before being used for a new
	 * purpose.
	 */
	bool	pending_unmap;

	/*
	 *  0: Nobody is currently using this, check vttbr for validity
	 * >0: Somebody is actively using this.
	 */
	atomic_t refcnt;
};

	struct kvm_s2_mmu mmu;

	/*
	 * Fine-Grained UNDEF, mimicking the FGT layout defined by the
	 * architecture. We track them globally, as we present the
	 * same feature-set to all vcpus.
	 *
	 * Index 0 is currently spare.
	 */
	u64 fgu[__NR_FGT_GROUP_IDS__];

	/*
	 * Stage 2 paging state for VMs with nested S2 using a virtual
	 * VMID.
	 */
	struct kvm_s2_mmu *nested_mmus;
	size_t nested_mmus_size;
	int nested_mmus_next;

	/* Interrupt controller */
	struct vgic_dist	vgic;

	/* Timers */
	struct arch_timer_vm_data timer_data;

	/* Mandated version of PSCI */
