/*
 * Verbatim line range from Linux v7.3.3 arch/arm64/include/asm/kvm_nested.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/include/asm/kvm_nested.h
 * Complete source SHA256: c15ab343081d711ecd1a3399ecd307553542c06f2ab18bb2ca4c46f9b74fc62e
 * Lines 1-20.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/nv-guest-hypervisor-exits-without-calls.patch to this file,
 * then checks or compiles the functions it names from it.
 */
/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ARM64_KVM_NESTED_H
#define __ARM64_KVM_NESTED_H

#include <linux/bitfield.h>
#include <linux/kvm_host.h>
#include <asm/kvm_emulate.h>
#include <asm/kvm_pgtable.h>

static inline bool vcpu_has_nv(const struct kvm_vcpu *vcpu)
{
	return (!__is_defined(__KVM_NVHE_HYPERVISOR__) &&
		cpus_have_final_cap(ARM64_HAS_NESTED_VIRT) &&
		vcpu_has_feature(vcpu, KVM_ARM_VCPU_HAS_EL2));
}

/* Translation helpers from non-VHE EL2 to EL1 */
static inline u64 tcr_el2_ps_to_tcr_el1_ips(u64 tcr_el2)
{
	return (u64)FIELD_GET(TCR_EL2_PS_MASK, tcr_el2) << TCR_IPS_SHIFT;
