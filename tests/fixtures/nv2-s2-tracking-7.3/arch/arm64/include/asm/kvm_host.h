/*
 * Verbatim line ranges from Linux v7.3.3 arch/arm64/include/asm/kvm_host.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/include/asm/kvm_host.h
 * Complete source SHA256: ac044465ffccb1446e843e0edfb22626ce8c0c73f34339be978e1163217f65bc
 * Lines 14-19, 150-155, 211-237, 326-331, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/nv2-targeted-shadow-s2-unmap.patch to these files,
 * then compiles the functions it names from them into a C harness.
 */
#include <linux/arm-smccc.h>
#include <linux/bitmap.h>
#include <linux/types.h>
#include <linux/jump_label.h>
#include <linux/kvm_types.h>
#include <linux/maple_tree.h>

	atomic64_t id;
};

struct kvm_s2_mmu {
	struct kvm_vmid vmid;


	u64	tlb_vttbr;
	u64	tlb_vtcr;

	/*
	 * true when this represents a nested context where virtual
	 * HCR_EL2.VM == 1
	 */
	bool	nested_stage2_enabled;

#ifdef CONFIG_PTDUMP_STAGE2_DEBUGFS
	struct dentry *shadow_pt_debugfs_dentry;
#endif

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

struct kvm_arch_memory_slot {

	size_t nested_mmus_size;
	int nested_mmus_next;

	/* Interrupt controller */
	struct vgic_dist	vgic;

