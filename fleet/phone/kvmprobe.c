// Runs a two-instruction real-mode guest with KVM and checks its exits: proves the
// machine this runs on can run a VM, not only that /dev/kvm opens.
// Exit 0 and "kvm ok" when the guest wrote 0x42 to port 0x10 and halted.
#include <fcntl.h>
#include <linux/kvm.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

static int fail(const char *what) { perror(what); printf("kvm FAIL: %s\n", what); return 1; }

int main(void) {
    // mov al, 0x42; out 0x10, al; hlt
    const uint8_t code[] = {0xb0, 0x42, 0xe6, 0x10, 0xf4};
    int kvm = open("/dev/kvm", O_RDWR | O_CLOEXEC);
    if (kvm < 0) return fail("open /dev/kvm");
    int vm = ioctl(kvm, KVM_CREATE_VM, 0);
    if (vm < 0) return fail("KVM_CREATE_VM");
    uint8_t *mem = mmap(NULL, 0x1000, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (mem == MAP_FAILED) return fail("mmap");
    memcpy(mem, code, sizeof code);
    struct kvm_userspace_memory_region region = {
        .slot = 0, .guest_phys_addr = 0x1000, .memory_size = 0x1000, .userspace_addr = (uint64_t)mem};
    if (ioctl(vm, KVM_SET_USER_MEMORY_REGION, &region) < 0) return fail("KVM_SET_USER_MEMORY_REGION");
    int vcpu = ioctl(vm, KVM_CREATE_VCPU, 0);
    if (vcpu < 0) return fail("KVM_CREATE_VCPU");
    int run_size = ioctl(kvm, KVM_GET_VCPU_MMAP_SIZE, 0);
    struct kvm_run *run = mmap(NULL, run_size, PROT_READ | PROT_WRITE, MAP_SHARED, vcpu, 0);
    if (run == MAP_FAILED) return fail("mmap kvm_run");
    struct kvm_sregs sregs;
    if (ioctl(vcpu, KVM_GET_SREGS, &sregs) < 0) return fail("KVM_GET_SREGS");
    sregs.cs.base = 0; sregs.cs.selector = 0;
    if (ioctl(vcpu, KVM_SET_SREGS, &sregs) < 0) return fail("KVM_SET_SREGS");
    struct kvm_regs regs = {.rip = 0x1000, .rflags = 0x2};
    if (ioctl(vcpu, KVM_SET_REGS, &regs) < 0) return fail("KVM_SET_REGS");
    int saw_out = 0;
    for (int i = 0; i < 4; i++) {
        if (ioctl(vcpu, KVM_RUN, 0) < 0) return fail("KVM_RUN");
        if (run->exit_reason == KVM_EXIT_IO && run->io.direction == KVM_EXIT_IO_OUT && run->io.port == 0x10
            && *((uint8_t *)run + run->io.data_offset) == 0x42) {
            saw_out = 1;
        } else if (run->exit_reason == KVM_EXIT_HLT) {
            if (!saw_out) break;
            printf("kvm ok\n");
            return 0;
        } else {
            printf("kvm FAIL: unexpected exit reason %u (hardware_exit_reason %llu)\n", run->exit_reason,
                   (unsigned long long)run->fail_entry.hardware_entry_failure_reason);
            return 1;
        }
    }
    printf("kvm FAIL: the guest did not write 0x42 to port 0x10 and halt\n");
    return 1;
}
