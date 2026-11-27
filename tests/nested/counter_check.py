#!/usr/bin/env python3
"""Does user space read the counter the kernel keeps time with?

Run in a nested level's container by tests/test_kvm.rs (aarch64). Prints one line,
`COUNTER_L<level> ok ...` or `COUNTER_L<level> FAIL ...`, and always exits 0: the test
that owns the assertion reads the line.

The vDSO computes a clock as base + (CNTVCT_EL0 - cycle_last) * mult >> shift, with
CNTVCT_EL0 read at EL0 and cycle_last recorded by the kernel from the counter the
kernel itself reads. A guest whose kernel runs at virtual EL2 reads the physical
counter, so the two only agree if the hypervisor below gives both the same offset.
When the EL0 counter is behind cycle_last the difference wraps through the
clocksource mask, the multiplication overflows 64 bits, and every vDSO clock is off
by up to 2^41 ns and jumps 2^33 ns whenever NTP moves mult by one.

The line also says what the kernel's boot log shows about FEAT_NV2p1 (`nv2p1=`,
`nv_frac_forced_to=`): a guest told it has FEAT_NV2p1 on a CPU that has not is how
the counters came apart (kernel/patches-arm64/mmfr4-override.vm.patch).

Two checks, three samples each:
  - CLOCK_MONOTONIC_RAW through the vDSO and through the syscall agree within 1 ms;
  - 0 <= CNTVCT_EL0 - cycle_last < 600 s of ticks, with cycle_last, mask, mult and
    shift read from the vDSO data page under its seqlock. Skipped, and said so, when
    the page does not look like the layout this was written against.
"""
import ctypes
import mmap
import platform
import struct
import sys
import time

level = sys.argv[1] if len(sys.argv) > 1 else "?"
tag = f"COUNTER_L{level}"
if platform.machine() != "aarch64":
    print(f"{tag} skipped: {platform.machine()}")
    sys.exit(0)

libc = ctypes.CDLL(None, use_errno=True)
# Code goes into a writable page that is then made executable: the kernel
# synchronises the instruction cache when it installs the executable mapping.
page = mmap.mmap(-1, mmap.PAGESIZE, prot=mmap.PROT_READ | mmap.PROT_WRITE)
# mrs x0, cntvct_el0; ret; mrs x0, cntfrq_el0; ret
page.write(struct.pack("<4I", 0xD53BE040, 0xD65F03C0, 0xD53BE000, 0xD65F03C0))
base = ctypes.addressof(ctypes.c_char.from_buffer(page))
libc.mprotect.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
if libc.mprotect(base, mmap.PAGESIZE, mmap.PROT_READ | mmap.PROT_EXEC) != 0:
    print(f"{tag} FAIL could not make the reader executable (errno {ctypes.get_errno()})")
    sys.exit(0)
cntvct = ctypes.CFUNCTYPE(ctypes.c_uint64)(base)
cntfrq = ctypes.CFUNCTYPE(ctypes.c_uint64)(base + 8)


class Timespec(ctypes.Structure):
    _fields_ = [("sec", ctypes.c_long), ("nsec", ctypes.c_long)]


SYS_CLOCK_GETTIME = 113


def syscall_clock(clock):
    ts = Timespec()
    if libc.syscall(SYS_CLOCK_GETTIME, clock, ctypes.byref(ts)) != 0:
        return float("nan")
    return ts.sec + ts.nsec / 1e9


vvar = None
with open("/proc/self/maps") as maps:
    for line in maps:
        if line.rstrip().endswith("[vvar]"):
            start = int(line.split("-")[0], 16)
            vvar = start if vvar is None else min(vvar, start)


def kernel_record():
    """(cycle_last, mask, mult, shift, CNTVCT_EL0 read inside the same seqlock pass)."""
    for _ in range(1000):
        seq, _mode, cycle_last, mask, mult, shift = struct.unpack("<IiQQII", ctypes.string_at(vvar, 32))
        if seq & 1:
            continue
        count = cntvct()
        if struct.unpack("<I", ctypes.string_at(vvar, 4))[0] == seq:
            return cycle_last, mask, mult, shift, count
    return None


freq = cntfrq()
failures = []
facts = [f"cntfrq={freq}"]
for sample in range(3):
    before = syscall_clock(time.CLOCK_MONOTONIC_RAW)
    vdso = time.clock_gettime(time.CLOCK_MONOTONIC_RAW)
    after = syscall_clock(time.CLOCK_MONOTONIC_RAW)
    gap = vdso - (before + after) / 2
    facts.append(f"raw_vdso_minus_syscall_s={gap:+.6f}")
    if not abs(gap) < 0.001:
        failures.append(f"vDSO MONOTONIC_RAW is {gap:+.3f} s from the syscall's")
    record = kernel_record() if vvar and freq else None
    if record is None:
        facts.append("vdso_data=unreadable")
    else:
        cycle_last, mask, mult, shift, count = record
        if mask not in ((1 << 56) - 1, (1 << 64) - 1) or not 0 < shift < 32 or mult == 0:
            facts.append(f"vdso_data=unrecognised(mask={mask:#x},mult={mult},shift={shift})")
        else:
            delta = (count - cycle_last) & mask
            if delta > mask // 2:
                delta -= mask + 1
            facts.append(f"cntvct_minus_cycle_last_s={delta / freq:+.6f}")
            if not 0 <= delta < 600 * freq:
                failures.append(
                    f"CNTVCT_EL0 at EL0 is {delta / freq:+.3f} s from the kernel's cycle_last "
                    f"(cntvct={count} cycle_last={cycle_last} mult={mult} shift={shift})"
                )
    time.sleep(0.5)



def kernel_log():
    """Every record still in the kernel log, or None when it cannot be read."""
    import os

    try:
        fd = os.open("/dev/kmsg", os.O_RDONLY | os.O_NONBLOCK)
    except OSError:
        return None
    records = []
    try:
        while True:
            try:
                records.append(os.read(fd, 8192).decode("utf-8", "replace"))
            except BlockingIOError:
                break
            except OSError:
                # EPIPE: the record under the reader was overwritten; carry on with the next.
                continue
    finally:
        os.close(fd)
    return records


# What this kernel believes about nested virtualization, from its own boot log:
# whether it detected FEAT_NV2p1, and what an ID register override forced
# ID_AA64MMFR4_EL1.NV_frac to (bits 23:20; 1 is NV2_ONLY, 2 is NV2P1).
records = kernel_log()
if records is None:
    facts.append("nv2p1=unknown(no /dev/kmsg)")
else:
    import re

    text = "\n".join(records)
    # Record 0 is the first line of the boot: without it the log has wrapped and a
    # missing line proves nothing.
    whole = any(r.split(";", 1)[0].split(",")[1:2] == ["0"] for r in records)
    forced = re.search(r"SYS_ID_AA64MMFR4_EL1\[23:20\]: forced to ([0-9a-fx]+)", text)
    if forced:
        facts.append(f"nv_frac_forced_to={int(forced.group(1), 0)}")
    if "FEAT_NV2p1" in text:
        facts.append("nv2p1=yes")
    elif whole:
        facts.append("nv2p1=no")
    else:
        facts.append("nv2p1=unknown(log wrapped)")

verdict = "FAIL " + "; ".join(dict.fromkeys(failures)) if failures else "ok"
print(f"{tag} {verdict} | {' '.join(facts)}")
