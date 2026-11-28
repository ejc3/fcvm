/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Runs kvm_arch_async_page_present() and kvm_set_mp_state() of one Linux
 * release against a model of the vCPU state they touch. The functions are the
 * release's own: the test compiles them from the excerpts beside this file,
 * with or without that kernel's PV kick patch, into extracted.c, which this
 * file includes after the stubs below.
 *
 * One harness serves every release fcvm builds KVM from. The fixtures of the
 * other releases link to this file, and each runs with the patch of the
 * kernels that pin its release:
 *
 *   kvm-pv-kick-7.3   kernel/patches-x86-host/0003-... (the x86_64 host kernel;
 *                     phone.amd64 links to it from kernel/patches-phone-7.3)
 *   kvm-pv-kick-6.18  kernel/patches-x86/0008-... (nested.amd64)
 *   kvm-pv-kick-6.16  kernel/patches-mountinfo/0004-... (the mountinfo pair;
 *                     the control links to it from kernel/patches-mountinfo-base)
 *
 * Each excerpt's header names its release, its URL, the SHA-256 of the whole
 * upstream file and the line range. To check one again:
 *
 *   curl -fsSL <the URL in the header> > f
 *   sha256sum f                 # the header's SHA-256
 *   sed -n '<first>,<last>p' f  # the excerpt below the header, byte for byte
 *
 * A PV spinlock waiter checks the lock and then executes HLT. A kick
 * (KVM_HC_KICK_CPU) that arrives between the two is recorded in
 * vcpu->arch.pv.pv_unhalted, and the HLT must then not block. The vCPU is
 * still KVM_MP_STATE_RUNNABLE in that window.
 *
 *   kick    a runnable vCPU with a kick pending takes a page-ready
 *           completion, with and without the guest's async page fault
 *           interface: the kick must still be pending.
 *   wake    the function's own statements: entered with the vCPU marked
 *           halted for an async page fault it leaves it runnable, and it
 *           invents no kick. (These releases call it only for a running
 *           vCPU, whose mp_state is already RUNNABLE, so this pins the
 *           function and not a state the hang passes through.)
 *   helper  kvm_set_mp_state() itself still clears the kick on a move to
 *           runnable, which its other callers rely on, and leaves it alone
 *           on a move to halted.
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef uint32_t u32;
typedef uint64_t u64;
typedef u64 gfn_t;
typedef u64 gpa_t;

#define KVM_MP_STATE_RUNNABLE 0
#define KVM_MP_STATE_HALTED 3
#define APIC_DM_FIXED 0x000
#define WRITE_ONCE(x, val) ((x) = (val))

struct kvm_lapic_irq {
	u32 vector;
	u32 delivery_mode;
};

struct kvm_vcpu {
	struct {
		int mp_state;
		struct {
			bool pv_unhalted;
		} pv;
		struct {
			bool halted;
			bool pageready_pending;
			u32 vec;
		} apf;
	} arch;
};

struct kvm_async_pf {
	bool wakeup_all;
	bool notpresent_injected;
	gpa_t cr2_or_gpa;
	struct {
		u32 token;
		gfn_t gfn;
	} arch;
};

static bool guest_apf_enabled;
static int irqs_set;

static void kvm_del_async_pf_gfn(struct kvm_vcpu *vcpu, gfn_t gfn)
{
	(void)vcpu;
	(void)gfn;
}

static void trace_kvm_async_pf_ready(u64 token, u64 gva)
{
	(void)token;
	(void)gva;
}

static bool kvm_pv_async_pf_enabled(struct kvm_vcpu *vcpu)
{
	(void)vcpu;
	return guest_apf_enabled;
}

static int apf_put_user_ready(struct kvm_vcpu *vcpu, u32 token)
{
	(void)vcpu;
	(void)token;
	return 0;
}

static int kvm_apic_set_irq(struct kvm_vcpu *vcpu, struct kvm_lapic_irq *irq, void *dest_map)
{
	(void)vcpu;
	(void)irq;
	(void)dest_map;
	irqs_set++;
	return 1;
}

#include "extracted.c"

static int failures;

static void check(bool ok, const char *scenario, const char *what)
{
	if (ok)
		return;
	fprintf(stderr, "%s: %s\n", scenario, what);
	failures++;
}

static void kick(void)
{
	for (int with_guest_apf = 0; with_guest_apf <= 1; with_guest_apf++) {
		for (int injected = 0; injected <= with_guest_apf; injected++) {
			struct kvm_vcpu vcpu;
			struct kvm_async_pf work;

			memset(&vcpu, 0, sizeof(vcpu));
			memset(&work, 0, sizeof(work));
			guest_apf_enabled = with_guest_apf;
			work.notpresent_injected = injected;
			vcpu.arch.mp_state = KVM_MP_STATE_RUNNABLE;
			vcpu.arch.pv.pv_unhalted = true;

			kvm_arch_async_page_present(&vcpu, &work);

			check(vcpu.arch.pv.pv_unhalted, "kick",
			      "a page-ready completion erased a pending PV kick: the vCPU's next HLT blocks, with interrupts off in a spinlock wait");
			check(vcpu.arch.mp_state == KVM_MP_STATE_RUNNABLE, "kick",
			      "the vCPU is no longer runnable");
		}
	}
}

static void wake(void)
{
	struct kvm_vcpu vcpu;
	struct kvm_async_pf work;

	memset(&vcpu, 0, sizeof(vcpu));
	memset(&work, 0, sizeof(work));
	guest_apf_enabled = false;
	vcpu.arch.mp_state = KVM_MP_STATE_HALTED;
	vcpu.arch.apf.halted = true;

	kvm_arch_async_page_present(&vcpu, &work);

	check(vcpu.arch.mp_state == KVM_MP_STATE_RUNNABLE, "wake",
	      "a vCPU halted for an async page fault stayed halted after its completion");
	check(!vcpu.arch.apf.halted, "wake", "apf.halted is still set");
	check(!vcpu.arch.pv.pv_unhalted, "wake", "a completion invented a PV kick");

	memset(&vcpu, 0, sizeof(vcpu));
	memset(&work, 0, sizeof(work));
	guest_apf_enabled = true;
	work.notpresent_injected = true;
	irqs_set = 0;
	vcpu.arch.mp_state = KVM_MP_STATE_RUNNABLE;

	kvm_arch_async_page_present(&vcpu, &work);

	check(irqs_set == 1 && vcpu.arch.apf.pageready_pending, "wake",
	      "the guest was not told its page is ready");
}

static void helper(void)
{
	struct kvm_vcpu vcpu;

	memset(&vcpu, 0, sizeof(vcpu));
	vcpu.arch.mp_state = KVM_MP_STATE_HALTED;
	vcpu.arch.pv.pv_unhalted = true;
	kvm_set_mp_state(&vcpu, KVM_MP_STATE_RUNNABLE);
	check(!vcpu.arch.pv.pv_unhalted && vcpu.arch.mp_state == KVM_MP_STATE_RUNNABLE, "helper",
	      "kvm_set_mp_state() no longer consumes the kick when a halted vCPU becomes runnable");

	memset(&vcpu, 0, sizeof(vcpu));
	vcpu.arch.pv.pv_unhalted = true;
	kvm_set_mp_state(&vcpu, KVM_MP_STATE_HALTED);
	check(vcpu.arch.pv.pv_unhalted && vcpu.arch.mp_state == KVM_MP_STATE_HALTED, "helper",
	      "kvm_set_mp_state() dropped the kick on a move to halted");
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: %s kick|wake|helper\n", argv[0]);
		return 2;
	}
	if (!strcmp(argv[1], "kick"))
		kick();
	else if (!strcmp(argv[1], "wake"))
		wake();
	else if (!strcmp(argv[1], "helper"))
		helper();
	else
		return 2;
	return failures ? 1 : 0;
}
