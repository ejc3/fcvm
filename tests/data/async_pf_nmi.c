// Reproduces the KVM async #PF / NMI race in a restored guest.
// Usage: apfnmi <MiB> <threads> <seconds>
//   1. Fills <MiB> of anonymous memory, prints "filled", then waits for /tmp/apfnmi-go.
//   2. Then each thread opens a high-rate hardware-cycles sampling event with user call chains on itself and reads
//      its share of the memory one byte per page while its frame pointer points at an unmapped page, so every
//      sample's user stack walk takes a real page fault inside the NMI. After a restore the pages are missing on
//      the host, so the reads take KVM async page faults.
#define _GNU_SOURCE
#include <linux/perf_event.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static char *mem;
static size_t len, share;
static int secs;
static int events_opened;

static void touch(char *p, char *end) {
    // rbp = 0x1000 (unmapped) while reading, so perf's frame-pointer walk faults. The operands are pinned
    // to rsi and rdi so the compiler cannot place them in rbp.
    __asm__ volatile(
        "push %%rbp\n\t"
        "mov $0x1000, %%rbp\n\t"
        "1: movb (%0), %%al\n\t"
        "add $4096, %0\n\t"
        "cmp %1, %0\n\t"
        "jb 1b\n\t"
        "pop %%rbp\n\t"
        : "+S"(p)
        : "D"(end)
        : "rax", "memory", "cc");
}

static void *worker(void *arg) {
    long i = (long)arg;
    struct perf_event_attr a;
    memset(&a, 0, sizeof a);
    a.size = sizeof a;
    a.type = PERF_TYPE_HARDWARE;
    a.config = PERF_COUNT_HW_CPU_CYCLES;
    a.sample_period = 20000;
    a.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_CALLCHAIN;
    int fd = syscall(SYS_perf_event_open, &a, 0, -1, -1, 0);
    if (fd < 0 && i == 0) perror("perf_event_open");
    if (fd >= 0) __atomic_add_fetch(&events_opened, 1, __ATOMIC_RELAXED);
    char *start = mem + i * share, *end = start + share;
    // Check the clock between 16 MiB chunks, not between passes: under call-chain sampling one pass over
    // a few GB of demand-paged memory can take minutes.
    const size_t chunk = 16UL << 20;
    time_t stop = time(NULL) + secs;
    long chunks = 0;
    char *p = start;
    while (time(NULL) < stop) {
        char *q = p + chunk < end ? p + chunk : end;
        touch(p, q);
        chunks++;
        p = q < end ? q : start;
    }
    if (i == 0) printf("thread 0: %ld chunks, perf fd %d\n", chunks, fd);
    return NULL;
}

int main(int argc, char **argv) {
    size_t mib = strtoul(argv[1], 0, 10);
    int threads = atoi(argv[2]);
    secs = atoi(argv[3]);
    len = mib << 20;
    share = (len / threads) & ~4095UL;
    mem = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mem == MAP_FAILED) { perror("mmap"); return 1; }
    for (size_t off = 0; off < len; off += 4096) mem[off] = (char)(off >> 12) | 1;
    printf("filled %zu MiB\n", mib);
    fflush(stdout);
    while (access("/tmp/apfnmi-go", F_OK) != 0) usleep(100000);
    printf("go\n");
    fflush(stdout);
    pthread_t t[256];
    for (long i = 0; i < threads; i++) pthread_create(&t[i], 0, worker, (void *)i);
    for (long i = 0; i < threads; i++) pthread_join(t[i], 0);
    printf("sampling events opened: %d of %d\n", events_opened, threads);
    printf("survived %d s\n", secs);
    return 0;
}
