#!/usr/bin/python3
"""Pack the fcvm store into parts that store.sh extracts at once:

    store-pack.py ROOT OUTDIR PARTS [--exclude PATTERN]...

Writes OUTDIR/part-00.tar.zst ... (the store's files, symlinks and other non-directories,
PARTS of them, balanced by the bytes each holds), OUTDIR/dirs.tar.zst (every directory,
ROOT itself first, extracted after the parts so their modes, owners and times are the
last written), and OUTDIR/parts, the part names one per line (without dirs).

A file's hard links all go to one part: tar writes the second name of an inode as a link
to the first, which it can only do within one archive. PATTERN is matched against the path
relative to ROOT ("./a/b") as tar's --exclude matches it (fnmatch, `*` crosses "/"); an
excluded directory is left out with everything under it. Paths are passed to tar
NUL-separated with --no-recursion, so tar writes exactly what this lists.
"""
import argparse
import fnmatch
import os
import subprocess
import sys


def walk(root, excludes):
    """(directories, groups): every directory, and the non-directories grouped by inode,
    each group with the bytes it holds."""
    def excluded(rel):
        return any(fnmatch.fnmatchcase(rel, p) for p in excludes)

    dirs = ["."]
    groups = {}
    for top, subdirs, files in os.walk(root):
        reltop = "." + top[len(root):] if top != root else "."
        keep = []
        for d in sorted(subdirs):
            rel = f"{reltop}/{d}"
            full = os.path.join(top, d)
            if excluded(rel):
                continue
            if os.path.islink(full):
                files.append(d)  # a symlink to a directory is archived as a symlink
                continue
            keep.append(d)
            dirs.append(rel)
        subdirs[:] = keep
        for f in sorted(files):
            rel = f"{reltop}/{f}"
            if excluded(rel):
                continue
            st = os.lstat(os.path.join(top, f))
            key = (st.st_dev, st.st_ino) if st.st_nlink > 1 else rel
            g = groups.setdefault(key, [0, []])
            if not g[1]:
                # Allocated bytes (a sparse image's holes cost tar nothing) plus a header.
                g[0] = st.st_blocks * 512 + 512
            g[1].append(rel)
    return dirs, list(groups.values())


def balance(groups, n):
    """Largest first into the lightest part."""
    parts = [[0, []] for _ in range(n)]
    for size, names in sorted(groups, key=lambda g: (-g[0], g[1][0])):
        p = min(parts, key=lambda p: p[0])
        p[0] += size
        p[1].extend(names)
    return parts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("outdir")
    ap.add_argument("parts", type=int)
    ap.add_argument("--exclude", action="append", default=[])
    a = ap.parse_args()
    root = os.path.abspath(a.root).rstrip("/") or "/"
    dirs, groups = walk(root, a.exclude)
    parts = balance(groups, max(1, a.parts))
    os.makedirs(a.outdir, exist_ok=True)
    jobs = [(f"part-{i:02d}", size, names) for i, (size, names) in enumerate(parts) if names]
    jobs.append(("dirs", 0, dirs))
    procs = []
    for name, _, names in jobs:
        # The list goes through a file: written to tar's stdin, one part's list would hold
        # this loop until that tar had read it all, and the parts would pack one by one.
        lst = os.path.join(a.outdir, f"{name}.list")
        with open(lst, "wb") as f:
            f.write(b"".join(n.encode() + b"\0" for n in names))
        out = open(os.path.join(a.outdir, f"{name}.tar.zst"), "wb")
        tar = subprocess.Popen(
            ["tar", "-c", "--sparse", "--numeric-owner", "--format=posix", "--no-recursion", "--null",
             "-C", root, "-T", lst],
            stdout=subprocess.PIPE)
        zst = subprocess.Popen(["zstd", "-q", "-6", "-T0", "-c"], stdin=tar.stdout, stdout=out)
        tar.stdout.close()
        procs.append((name, tar, zst, out, lst))
    failed = False
    for name, tar, zst, out, lst in procs:
        if tar.wait() != 0 or zst.wait() != 0:
            print(f"store-pack: packing {name} failed", file=sys.stderr)
            failed = True
        out.close()
        os.unlink(lst)
    if failed:
        sys.exit(1)
    with open(os.path.join(a.outdir, "parts"), "w") as f:
        f.write("".join(f"{name}\n" for name, _, _ in jobs if name != "dirs"))
    for name, size, names in jobs:
        z = os.path.getsize(os.path.join(a.outdir, f"{name}.tar.zst"))
        print(f"store-pack: {name}: {len(names)} entries, {size >> 20} MiB in, {z >> 20} MiB packed")


if __name__ == "__main__":
    main()
