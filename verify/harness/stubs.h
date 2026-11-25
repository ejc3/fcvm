/*
 * The least of the kernel that the cut functions need to compile: the fields of struct kvm_vcpu
 * they read and write, and every function they call that is not itself cut, as a stub whose
 * answer the harness sets. Nothing here decides a wake-up: that is all in the cut code.
 */
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef uint8_t u8;
typedef uint32_t u32;
typedef uint64_t u64;
typedef u64 gpa_t;

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

enum { KVM_REQ_EVENT, KVM_REQ_NMI, KVM_REQ_SMI, KVM_REQ_PMI,
       KVM_REQ_UPDATE_PROTECTED_GUEST_STATE, KVM_REQ_UNBLOCK };

struct list_head { int nonempty; };
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
struct kvm_lapic_irq { u32 delivery_mode; u32 vector; };
struct kvm_async_pf {
	bool wakeup_all;
	bool notpresent_injected;
	gpa_t cr2_or_gpa;
	struct { u32 token; u64 gfn; } arch;
};
struct rtc_status { int dummy; };
struct dest_map { unsigned long map[1]; u8 vectors[2]; };
struct kvm_vcpu {
	int vcpu_id;
	struct kvm *kvm;
	struct kvm_run *run;
	struct { unsigned long halt_exits; } stat;
	struct { struct list_head done; } async_pf;
	struct {
		int mp_state;
		struct { bool pv_unhalted; } pv;
		struct { bool halted; bool pageready_pending; u32 vec; u64 msr_en_val; } apf;
		struct kvm_lapic *apic;
		unsigned int nmi_pending;
		unsigned int smi_pending;
		unsigned int apic_arb_prio;
	} arch;
};

/* What the harness sets before a call, and reads after it. */
struct knobs {
	bool events;        /* a completion is on the done list: kvm_vcpu_has_events() is true */
	bool did_block;     /* vcpu_block() went to sleep */
	bool wake_on_block; /* an event arrives while it sleeps */
	bool kicked;        /* kvm_vcpu_kick() was called */
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
#define CONFIG_KVM_SMM 1

static inline bool list_empty_careful(struct list_head *l) { return !l->nonempty; }
static inline bool lapic_in_kernel(struct kvm_vcpu *v) { (void)v; return true; }
static inline bool kvm_apic_has_pending_init_or_sipi(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_apic_init_sipi_allowed(struct kvm_vcpu *v) { (void)v; return true; }
static inline bool kvm_is_exception_pending(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_test_request(int r, struct kvm_vcpu *v) { (void)r; (void)v; return false; }
static inline void kvm_make_request(int r, struct kvm_vcpu *v) { (void)r; (void)v; }
static inline bool kvm_is_interrupt_allowed(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_arch_interrupt_allowed(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_cpu_has_interrupt(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_hv_has_stimer_pending(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool is_guest_mode(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_xen_has_pending_events(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool kvm_guest_apic_has_interrupt(struct kvm_vcpu *v) { (void)v; return false; }
static inline bool stub_allowed(struct kvm_vcpu *v, bool b) { (void)v; (void)b; return false; }
static inline bool stub_has_events(struct kvm_vcpu *v, bool b) { (void)v; (void)b; return false; }
static inline void stub_deliver(struct kvm_lapic *a, int d, int t, int v) { (void)a; (void)d; (void)t; (void)v; }
static inline void stub_clear_hlt(struct kvm_vcpu *v) { (void)v; }
#define kvm_x86_call(f) kvm_x86_call_##f
#define kvm_x86_call_nmi_allowed stub_allowed
#define kvm_x86_call_smi_allowed stub_allowed
#define kvm_x86_call_deliver_interrupt stub_deliver
#define kvm_x86_call_clear_hlt stub_clear_hlt
#define kvm_nested_call(f) stub_has_events
#define kvm_x86_ops (*(struct { struct { bool (*has_events)(struct kvm_vcpu *, bool); } *nested_ops; } *)0)

static inline bool kvm_lapic_hv_timer_in_use(struct kvm_vcpu *v) { (void)v; return false; }
static inline void kvm_lapic_switch_to_sw_timer(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_lapic_switch_to_hv_timer(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_vcpu_srcu_read_lock(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_vcpu_srcu_read_unlock(struct kvm_vcpu *v) { (void)v; }
static inline int kvm_check_nested_events(struct kvm_vcpu *v) { (void)v; return 0; }
static inline int kvm_apic_accept_events(struct kvm_vcpu *v) { (void)v; return 0; }
/* Sleeping: recorded, and ended by an event if the harness says one arrives. */
static inline void kvm_vcpu_halt(struct kvm_vcpu *v)
{
	K.did_block = true;
	if (K.wake_on_block)
		v->async_pf.done.nonempty = 1;
}
static inline bool kvm_vcpu_block(struct kvm_vcpu *v) { kvm_vcpu_halt(v); return true; }
static inline void kvm_vcpu_kick(struct kvm_vcpu *v) { (void)v; K.kicked = true; }

static inline void vcpu_load(struct kvm_vcpu *v) { (void)v; }
static inline void vcpu_put(struct kvm_vcpu *v) { (void)v; }
static inline bool kvm_mpx_supported(void) { return false; }
static inline void kvm_load_guest_fpu(struct kvm_vcpu *v) { (void)v; }
static inline void kvm_put_guest_fpu(struct kvm_vcpu *v) { (void)v; }
static inline bool kvm_hlt_in_guest(struct kvm *k) { (void)k; return false; }
static inline void set_bit(int n, unsigned long *p) { *p |= 1UL << n; }
static inline void __set_bit(int n, unsigned long *p) { *p |= 1UL << n; }

static inline void kvm_del_async_pf_gfn(struct kvm_vcpu *v, u64 g) { (void)v; (void)g; }
static inline bool kvm_pv_async_pf_enabled(struct kvm_vcpu *v) { return v->arch.apf.msr_en_val != 0; }
static inline int apf_put_user_ready(struct kvm_vcpu *v, u32 t) { (void)v; (void)t; return 0; }
static inline int kvm_apic_set_irq(struct kvm_vcpu *v, struct kvm_lapic_irq *i, void *p) { (void)v; (void)i; (void)p; return 1; }
#define trace_kvm_async_pf_ready(a, b) do {} while (0)
#define trace_kvm_apic_accept_irq(a, b, c, d) do {} while (0)

static inline bool apic_enabled(struct kvm_lapic *a) { (void)a; return true; }
static inline int apic_test_vector(int v, void *r) { (void)v; (void)r; return 0; }
static inline void apic_set_vector(int v, void *r) { (void)v; (void)r; }
static inline void apic_clear_vector(int v, void *r) { (void)v; (void)r; }
static inline void kvm_lapic_set_vector(int v, void *r) { (void)v; (void)r; }
static inline void kvm_lapic_clear_vector(int v, void *r) { (void)v; (void)r; }
static inline int kvm_inject_smi(struct kvm_vcpu *v) { (void)v; return 0; }
static inline void kvm_inject_nmi(struct kvm_vcpu *v) { (void)v; }
