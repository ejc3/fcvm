/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Runs the x86 host kernel's kvm_arch_async_page_present() and
 * kvm_set_mp_state() against a model of the vCPU state they touch. The
 * functions are the release's own: the test compiles them from the excerpts,
 * with or without
 * kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-page-ready.patch,
 * into extracted.c, which this file includes after the stubs below.
 *
 * A PV spinlock waiter checks the lock and then executes HLT. A kick
 * (KVM_HC_KICK_CPU) that arrives between the two is recorded in
 * vcpu->arch.pv.pv_unhalted, and the HLT must then not block. The vCPU is
 * still KVM_MP_STATE_RUNNABLE in that window.
 *
 *   kick    a runnable vCPU with a kick pending takes a page-ready
 *           completion, with and without the guest's async page fault
 *           interface: the kick must still be pending.
 *   wake    a vCPU halted for an async page fault takes its completion: it
 *           must be runnable, with no kick invented.
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
