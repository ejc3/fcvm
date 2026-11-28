/*
 * The least of the kernel that the cut functions need to compile: the fields of struct kvm_vcpu
 * they read and write, a real list, and every function they call that is not itself cut, as a
 * stub. Nothing here decides a wake-up: that is all in the cut code. What a stub stands for:
 *
 *   guest memory     the page-ready slot (apf_reason.token) is one u32 behind the vCPU's
 *                    gfn_to_hva_cache; kvm_{read,write}_guest_offset_cached read and write it
 *   the local APIC   delivering a fixed interrupt records it as pending (hx.irr); whether the
 *                    guest can take interrupts (hx.if_on), whether another interrupt is pending
 *                    (hx.other_irq) and whether an NMI is requested (hx.nmi) are set by the
 *                    harness, and kvm_vcpu_has_events() reads them through the stubs below
 *   sleeping         kvm_vcpu_halt()/kvm_vcpu_block() only record that the vCPU went to sleep
 *   not followed     INIT/SIPI (kvm_apic_has_pending_init_or_sipi is false), SMM, nested guests,
 *                    Xen, Hyper-V timers: those arms of kvm_vcpu_has_events() are constant false
 *   CONFIG_KVM_ASYNC_PF_SYNC is off, as on x86 (only s390 selects it)
 */
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef uint8_t u8;
typedef uint32_t u32;
typedef uint64_t u64;
typedef u64 gpa_t;
typedef u64 gfn_t;

#define KVM_MP_STATE_RUNNABLE 0
#define KVM_MP_STATE_UNINITIALIZED 1
#define KVM_MP_STATE_INIT_RECEIVED 2
#define KVM_MP_STATE_HALTED 3
#define KVM_MP_STATE_SIPI_RECEIVED 4
#define KVM_MP_STATE_AP_RESET_HOLD 9
#define KVM_EXIT_HLT 5
#define KVM_EXIT_AP_RESET_HOLD 32
#define EINVAL 22
#define EBUSY 16
#define ENOMEM 12

#define APIC_DM_FIXED 0x00000
#define APIC_DM_LOWEST 0x00100
#define APIC_DM_SMI 0x00200
#define APIC_DM_REMRD 0x00300
#define APIC_DM_NMI 0x00400
#define APIC_DM_INIT 0x00500
#define APIC_DM_STARTUP 0x00600
#define APIC_DM_EXTINT 0x00700
#define APIC_TMR 0x180
#define KVM_APIC_INIT 0
#define KVM_APIC_SIPI 1

/* arch/x86/include/uapi/asm/kvm_para.h */
#define KVM_ASYNC_PF_ENABLED (1 << 0)
#define KVM_ASYNC_PF_SEND_ALWAYS (1 << 1)
#define KVM_ASYNC_PF_DELIVERY_AS_PF_VMEXIT (1 << 2)
#define KVM_ASYNC_PF_DELIVERY_AS_INT (1 << 3)
#define KVM_FEATURE_ASYNC_PF_VMEXIT 10
#define KVM_FEATURE_ASYNC_PF_INT 14
struct kvm_vcpu_pv_apf_data {
	u32 flags;
	u32 token;
	u8 pad[56];
};

enum { KVM_REQ_EVENT, KVM_REQ_NMI, KVM_REQ_SMI, KVM_REQ_PMI, KVM_REQ_APF_READY,
       KVM_REQ_UPDATE_PROTECTED_GUEST_STATE, KVM_REQ_UNBLOCK };
#define GFP_ATOMIC 0

struct list_head { struct list_head *next, *prev; };
static inline void INIT_LIST_HEAD(struct list_head *l) { l->next = l->prev = l; }
static inline bool list_empty(const struct list_head *l) { return l->next == l; }
static inline bool list_empty_careful(const struct list_head *l) { return l->next == l && l->prev == l; }
static inline void list_add_tail(struct list_head *n, struct list_head *head)
{
	n->prev = head->prev;
	n->next = head;
	head->prev->next = n;
	head->prev = n;
}
static inline void list_del(struct list_head *e)
{
	e->next->prev = e->prev;
	e->prev->next = e->next;
	e->next = e->prev = e;
}
#define container_of(ptr, type, member) ((type *)((char *)(ptr) - offsetof(type, member)))
#define list_first_entry(head, type, member) container_of((head)->next, type, member)

typedef struct { int unused; } spinlock_t;
static inline void spin_lock(spinlock_t *l) { (void)l; }
static inline void spin_unlock(spinlock_t *l) { (void)l; }

struct kvm { int dummy; };
struct kvm_run { int exit_reason; };
struct kvm_mp_state { u32 mp_state; };
struct kvm_vcpu;
struct kvm_lapic {
	struct kvm_vcpu *vcpu;
	unsigned long pending_events;
	unsigned int sipi_vector;
	void *regs;
};
struct kvm_lapic_irq { u32 vector; u32 delivery_mode; int level; int trig_mode; };
struct work_struct { void *func; };
struct kvm_arch_async_pf { u32 token; gfn_t gfn; };
struct kvm_async_pf {
	struct work_struct work;
	struct list_head link;
	struct list_head queue;
	struct kvm_vcpu *vcpu;
	gpa_t cr2_or_gpa;
	unsigned long addr;
	struct kvm_arch_async_pf arch;
	bool wakeup_all;
	bool notpresent_injected;
};
struct rtc_status { unsigned long map[1]; u8 vectors[2]; };
struct dest_map { unsigned long map[1]; u8 vectors[2]; };
struct gfn_to_hva_cache { u32 token; };
struct kvm_vcpu {
	int vcpu_id;
	struct kvm *kvm;
	struct kvm_run *run;
	struct { unsigned long halt_exits; } stat;
	struct {
		struct list_head done;
		struct list_head queue;
		spinlock_t lock;
		int queued;
	} async_pf;
	struct {
		int mp_state;
		struct { bool pv_unhalted; } pv;
		struct {
			bool halted;
			bool pageready_pending;
			u32 vec;
			u64 msr_en_val;
			u64 msr_int_val;
			struct gfn_to_hva_cache data;
			bool send_user_only;
			bool delivery_as_pf_vmexit;
		} apf;
		struct kvm_lapic *apic;
		unsigned int nmi_pending;
		unsigned int smi_pending;
		unsigned int apic_arb_prio;
	} arch;
	/* The harness's stand-ins: see the top of this file. */
	struct { bool if_on, other_irq, irr, nmi; } hx;
};

/* What the last call did, for the harness to print. */
struct knobs {
	bool did_block; /* vcpu_block() went to sleep */
};
extern struct knobs K;

#define EXPORT_SYMBOL_FOR_KVM_INTERNAL(x)
#define EXPORT_SYMBOL_GPL(x)
#define unlikely(x) (x)
#define likely(x) (x)
#define fallthrough do {} while (0)
static inline int warn_on_once(int c) { return c; }
#define WARN_ON_ONCE(x) warn_on_once(!!(x))
#define KERN_ERR
#define printk(...) do {} while (0)
#define WRITE_ONCE(x, v) ((x) = (v))
#define READ_ONCE(x) (x)
#define smp_wmb() do {} while (0)
#define smp_rmb() do {} while (0)
#define smp_mb__after_atomic() do {} while (0)
#define CONFIG_KVM_SMM 1
#define IS_ENABLED(option) 0

static inline bool lapic_in_kernel(struct kvm_vcpu *v) { (void)v; return true; }
static inline bool kvm_apic_has_pending_init_or_sipi(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_apic_init_sipi_allowed(struct kvm_vcpu *v) { (void)v; return true; }
static inline bool kvm_is_exception_pending(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_test_request(int r, struct kvm_vcpu *v) { return r == KVM_REQ_NMI && v->hx.nmi; }
static inline void kvm_make_request(int r, struct kvm_vcpu *v) { (void)r; (void)v; }
static inline bool kvm_is_interrupt_allowed(struct kvm_vcpu *v) { return v->hx.if_on; }
static inline bool kvm_arch_interrupt_allowed(struct kvm_vcpu *v) { return v->hx.if_on; }
static inline bool kvm_cpu_has_interrupt(struct kvm_vcpu *v) { return v->hx.other_irq || v->hx.irr; }
static inline bool kvm_hv_has_stimer_pending(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool is_guest_mode(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_xen_has_pending_events(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_guest_apic_has_interrupt(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool stub_allowed(struct kvm_vcpu *v, bool b) { (void)v; (void)b; return false; }
static inline bool stub_has_events(struct kvm_vcpu *v, bool b) { (void)v; (void)b; return false; }
/* A fixed interrupt accepted by the local APIC is pending in its IRR. */
static inline void stub_deliver(struct kvm_lapic *a, int d, int t, int vec)
{
	(void)d; (void)t; (void)vec;
	a->vcpu->hx.irr = true;
}
static inline void stub_clear_hlt(struct kvm_vcpu *v) { (void)v; }
#define kvm_x86_call(f) kvm_x86_call_##f
#define kvm_x86_call_nmi_allowed stub_allowed
#define kvm_x86_call_smi_allowed stub_allowed
#define kvm_x86_call_deliver_interrupt stub_deliver
#define kvm_x86_call_clear_hlt stub_clear_hlt
#define kvm_nested_call(f) stub_has_events
/* v6.14 spells the nested arm of kvm_vcpu_has_events() through the ops table. */
static struct { struct { bool (*has_events)(struct kvm_vcpu *, bool); } *nested_ops; } kvm_x86_ops;

static inline bool kvm_lapic_hv_timer_in_use(struct kvm_vcpu *v) { (void)v; return false; }
static inline void kvm_lapic_switch_to_sw_timer(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_lapic_switch_to_hv_timer(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_vcpu_srcu_read_lock(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_vcpu_srcu_read_unlock(struct kvm_vcpu *v) { (void)v; }
static inline int kvm_check_nested_events(struct kvm_vcpu *v) { (void)v; return 0; }
static inline int kvm_apic_accept_events(struct kvm_vcpu *v) { (void)v; return 0; }
static inline void kvm_vcpu_halt(struct kvm_vcpu *v) { (void)v; K.did_block = true; }
static inline bool kvm_vcpu_block(struct kvm_vcpu *v) { kvm_vcpu_halt(v); return true; }
static inline void kvm_vcpu_kick(struct kvm_vcpu *v) { (void)v; }

static inline void vcpu_load(struct kvm_vcpu *v) { (void)v; }
static inline void vcpu_put(struct kvm_vcpu *v) { (void)v; }
static inline bool kvm_mpx_supported(void) { return false; }
static inline void kvm_load_guest_fpu(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_put_guest_fpu(struct kvm_vcpu *v) { (void)v; }
static inline bool kvm_hlt_in_guest(struct kvm *k) { (void)k; return false; }
static inline void set_bit(int n, unsigned long *p) { *p |= 1UL << n; }
static inline void __set_bit(int n, unsigned long *p) { *p |= 1UL << n; }

/* Guest memory: the page-ready slot. */
static inline int kvm_write_guest_offset_cached(struct kvm *k, struct gfn_to_hva_cache *ghc,
						void *data, unsigned int offset, unsigned long len)
{
	(void)k;
	if (offset != offsetof(struct kvm_vcpu_pv_apf_data, token) || len != sizeof(u32))
		return -EINVAL;
	memcpy(&ghc->token, data, sizeof(u32));
	return 0;
}
static inline int kvm_read_guest_offset_cached(struct kvm *k, struct gfn_to_hva_cache *ghc,
					       void *data, unsigned int offset, unsigned long len)
{
	(void)k;
	if (offset != offsetof(struct kvm_vcpu_pv_apf_data, token) || len != sizeof(u32))
		return -EINVAL;
	memcpy(data, &ghc->token, sizeof(u32));
	return 0;
}
static inline int kvm_gfn_to_hva_cache_init(struct kvm *k, struct gfn_to_hva_cache *ghc, gpa_t gpa,
					    unsigned long len)
{
	(void)k; (void)ghc; (void)gpa; (void)len;
	return 0;
}

static inline void kvm_del_async_pf_gfn(struct kvm_vcpu *v, gfn_t g) { (void)v; (void)g; }
static inline void kvm_async_pf_hash_reset(struct kvm_vcpu *v) { (void)v; }
static inline bool guest_pv_has(struct kvm_vcpu *v, unsigned int f) { (void)v; (void)f; return true; }
static inline bool kvm_lapic_enabled(struct kvm_vcpu *v) { (void)v; return true; }
/* The MMU's retry of the fault: not followed. */
static inline void kvm_arch_async_page_ready(struct kvm_vcpu *v, struct kvm_async_pf *w) { (void)v; (void)w; }
#define trace_kvm_async_pf_ready(a, b) do {} while (0)
#define trace_kvm_apic_accept_irq(a, b, c, d) do {} while (0)

struct kmem_cache;
static struct kmem_cache *async_pf_cache;
static inline void *kmem_cache_zalloc(struct kmem_cache *c, int flags)
{
	(void)c; (void)flags;
	return calloc(1, sizeof(struct kvm_async_pf));
}
static inline void kmem_cache_free(struct kmem_cache *c, void *p) { (void)c; free(p); }
static inline void flush_work(struct work_struct *w) { (void)w; }
/* Only the path that disables the interface calls it; the harness never does. */
static inline void kvm_clear_async_pf_completion_queue(struct kvm_vcpu *v) { (void)v; abort(); }

static inline bool apic_enabled(struct kvm_lapic *a) { (void)a; return true; }
static inline int apic_test_vector(int v, void *r) { (void)v; (void)r; return 0; }
static inline void apic_set_vector(int v, void *r) { (void)v; (void)r; }
static inline void apic_clear_vector(int v, void *r) { (void)v; (void)r; }
static inline void kvm_lapic_set_vector(int v, void *r) { (void)v; (void)r; }
static inline void kvm_lapic_clear_vector(int v, void *r) { (void)v; (void)r; }
static inline int kvm_inject_smi(struct kvm_vcpu *v) { (void)v; return 0; }
static inline void kvm_inject_nmi(struct kvm_vcpu *v) { (void)v; }
