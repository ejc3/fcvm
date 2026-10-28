#!/usr/bin/python3
"""List the pieces of the root volume's files that are in the page cache, as `path<TAB>offset<TAB>length`
lines (byte ranges, page aligned, merged), for fleet/ami/tile/build.sh `trace`.

Run right after a tile's first boot reached its first frame, the page cache holds what that boot
read from the root volume: a volume made from the AMI's snapshot fetches each block from S3 the
first time it is read, so these are the pieces whose first read was slow. Files the boot wrote
(modification time after the boot began) and the build's own directories are left out: a write
needs no fetch, and the build tree is not read by a boot.

    resident.py LIST [TOP [SKIP_PREFIX...]]    walks TOP (default /), writes LIST, prints a count to stderr
"""
import ctypes
import mmap
import os
import stat
import sys
import time

PAGE = mmap.PAGESIZE
SKIP = ("/proc/", "/sys/", "/dev/", "/run/", "/tmp/", "/var/tmp/", "/var/log/", "/var/lib/fleet-ami/",
        "/opt/fleet-ami/", "/var/lib/cloud/", "/var/lib/amazon/", "/var/lib/snapd/", "/snap/")

FIRST = ("/usr/lib/modules/", "/lib/modules/")

libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p]
MAP_FAILED = ctypes.c_void_p(-1).value


def resident_ranges(path, size):
    """Byte ranges of `path` in the page cache, or [] (a file that cannot be opened or mapped)."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOATIME | os.O_NONBLOCK)
    except PermissionError:  # O_NOATIME needs the file's owner (or root, as the build runs)
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        except OSError:
            return []
    except OSError:
        return []
    try:
        addr = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
        if addr == MAP_FAILED or addr is None:
            return []
        try:
            pages = (size + PAGE - 1) // PAGE
            vec = ctypes.create_string_buffer(pages)
            if libc.mincore(addr, size, vec) != 0:
                return []
        finally:
            libc.munmap(addr, size)
    finally:
        os.close(fd)
    out, start = [], None
    for i, b in enumerate(vec.raw[:pages]):
        if b & 1:
            if start is None:
                start = i
        elif start is not None:
            out.append((start * PAGE, (i - start) * PAGE))
            start = None
    if start is not None:
        out.append((start * PAGE, min(size, pages * PAGE) - start * PAGE))
    return out


def main():
    out_path = sys.argv[1]
    top_dir = (sys.argv[2] if len(sys.argv) > 2 else "/").rstrip("/") or "/"
    skip = SKIP + tuple(sys.argv[3:])
    with open("/proc/uptime") as f:
        boot_began = time.time() - float(f.read().split()[0])
    root_dev = os.stat(top_dir).st_dev
    files = pieces = nbytes = 0
    found = []

    def rel(path):  # the path as the booted image names it
        return path[len(top_dir):] if top_dir != "/" else path

    for top, dirs, names in os.walk(top_dir, followlinks=False):
        if (rel(top) + "/").startswith(skip):
            dirs[:] = []
            continue
        try:
            if os.lstat(top).st_dev != root_dev:
                dirs[:] = []
                continue
        except OSError:
            dirs[:] = []
            continue
        for n in names:
            p = os.path.join(top, n)
            name = rel(p)
            if name.startswith(skip) or "\t" in name or "\n" in name:
                continue
            try:
                st = os.lstat(p)
            except OSError:
                continue
            if not stat.S_ISREG(st.st_mode) or st.st_size == 0 or st.st_mtime >= boot_began:
                continue
            r = resident_ranges(p, st.st_size)
            if r:
                files += 1
                found.extend((name, off, ln) for off, ln in r)
    # prefetch.c starts the list's pieces in order, many at a time: the modules the first
    # long wait of a boot is for (systemd-modules-load) go first, the rest in walk order.
    found.sort(key=lambda piece: not piece[0].startswith(FIRST))
    with open(out_path, "w") as out:
        for name, off, ln in found:
            out.write(f"{name}\t{off}\t{ln}\n")
            pieces += 1
            nbytes += ln
    print(f"resident.py: {files} files, {pieces} pieces, {nbytes / 1048576:.1f} MiB", file=sys.stderr)


if __name__ == "__main__":
    main()
