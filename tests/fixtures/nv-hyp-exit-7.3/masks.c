/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Runs the register masking the patched kvm_host.h defines for the VHE world
 * switch (fcvm #659): struct resx, __kvm_get_sysreg_resx() and
 * __kvm_apply_reg_masks(), compiled from the patched excerpt into
 * extracted-masks.c, which this file includes after the stubs below. The result
 * must be what kvm_vcpu_apply_reg_masks() in arch/arm64/kvm/nested.c returns:
 * the value with its RES0 bits clear and its RES1 bits set, and the value
 * unchanged before the VM has masks.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef uint64_t u64;

#define likely(x) (x)
#define __always_inline inline __attribute__((always_inline))

enum vcpu_sysreg {
	__SANITISED_REG_START__ = 4,
	VNCR_EL2 = __SANITISED_REG_START__,
	HCR_EL2,
	NR_SYS_REGS,
};

struct kvm_sysreg_masks;

struct kvm_arch {
	struct kvm_sysreg_masks *sysreg_masks;
};

#include "extracted-masks.c"

static int check(struct kvm_arch *arch, enum vcpu_sysreg sr, u64 v, u64 want)
{
	u64 got = __kvm_apply_reg_masks(arch, sr, v);

	if (got == want)
		return 0;
	fprintf(stderr, "register %d, value 0x%llx: masked to 0x%llx, expected 0x%llx\n",
		sr, (unsigned long long)v, (unsigned long long)got,
		(unsigned long long)want);
	return 1;
}

int main(void)
{
	struct kvm_sysreg_masks masks = { 0 };
	struct kvm_arch arch = { .sysreg_masks = NULL };
	const u64 res0 = 0xff00000000000000ULL | (1ULL << 20);
	const u64 res1 = (1ULL << 1) | (1ULL << 34);
	const u64 values[] = { 0, ~0ULL, 0x0123456789abcdefULL, res0, res1 };
	int failed = 0;

	/* Before the VM has masks, nothing is masked. */
	for (unsigned int i = 0; i < sizeof(values) / sizeof(values[0]); i++)
		failed |= check(&arch, HCR_EL2, values[i], values[i]);

	masks.mask[HCR_EL2 - __SANITISED_REG_START__].res0 = res0;
	masks.mask[HCR_EL2 - __SANITISED_REG_START__].res1 = res1;
	arch.sysreg_masks = &masks;
	for (unsigned int i = 0; i < sizeof(values) / sizeof(values[0]); i++) {
		failed |= check(&arch, HCR_EL2, values[i], (values[i] & ~res0) | res1);
		/* Another register's masks are its own. */
		failed |= check(&arch, VNCR_EL2, values[i], values[i]);
	}
	if (!failed)
		printf("masks: %u values checked\n",
		       (unsigned int)(3 * sizeof(values) / sizeof(values[0])));
	return failed;
}
