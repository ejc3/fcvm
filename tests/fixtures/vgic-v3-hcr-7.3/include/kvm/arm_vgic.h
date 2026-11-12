/*
 * Verbatim line range from Linux v7.3.3 include/kvm/arm_vgic.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/include/kvm/arm_vgic.h
 * Complete source SHA256: 81ca6126139286e3882bc61a11e4df9ecd5fdb9868d7b1da9767bbe8db8cfc2a
 * Lines 453-478.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/vgic-v3-keep-hcr-on-quiet-exits.patch
 * to this file, then compiles the definitions it names from it into a C harness.
 */
	unsigned int used_lrs;
};

struct vgic_v3_cpu_if {
	u32		vgic_hcr;
	u32		vgic_vmcr;
	u32		vgic_sre;	/* Restored only, change ignored */
	u32		vgic_ap0r[4];
	u32		vgic_ap1r[4];
	u64		vgic_lr[VGIC_V3_MAX_LRS];

	/*
	 * GICv4 ITS per-VPE data, containing the doorbell IRQ, the
	 * pending table pointer, the its_vm pointer and a few other
	 * HW specific things. As for the its_vm structure, this is
	 * linking the Linux IRQ subsystem and the ITS together.
	 */
	struct its_vpe	its_vpe;

	unsigned int used_lrs;
};

struct vgic_v5_cpu_if {
	u64	vgic_apr;
	u64	vgic_vmcr;

