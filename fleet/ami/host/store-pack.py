#!/usr/bin/python3
"""Pack the fcvm store into parts that store.sh extracts at once:

    store-pack.py ROOT OUTDIR PARTS [--exclude PATTERN]...

Writes OUTDIR/part-00.tar.zst ... (the store's files, symlinks and other non-directories,
PARTS of them, balanced by the bytes each holds), OUTDIR/dirs.tar.zst (every directory,
ROOT itself first, extracted after the parts so their modes, owners and times are the
last written), and OUTDIR/parts, the part names one per line (without dirs).

A file's hard links all go to one part: tar writes the second name of an inode as a link
to the first, which it can only do within one archive.

A regular file of one link with more than SPLIT bytes of data (--split-mib, 256 MiB) is not
archived whole: its data extents (SEEK_DATA / SEEK_HOLE), joined across gaps under 8 MiB, are
cut into pieces of at most SPLIT bytes, and each piece is a member `./.fcvm-seg/<n>` of whichever part is lightest, appended
after that part's tar archive (store.sh extracts with --ignore-zeros, which reads both). The
list of pieces (`seg <n> <part> <offset> <length> <path>`) and of split files (`file <size> <mode>
<uid> <gid> <mtime ns> <path>`) is `./.fcvm-segments`, a member of `dirs`, so the restore
knows them before any part. One file is then extracted by many parts at once: the phone
snapshot's 6.4 GiB memory image was one part, whose decompress, extract and copy bound the
restore (#1257). PATTERN is matched against the path
relative to ROOT ("./a/b") as tar's --exclude matches it (fnmatch, `*` crosses "/"); an
excluded directory is left out with everything under it. Paths are passed to tar
NUL-separated with --no-recursion, so tar writes exactly what this lists.
"""
import argparse
import errno
import fnmatch
import io
import os
import subprocess
import sys
import tarfile
import threading


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


def extents(path):
    """The file's data ranges [(offset, length)], from SEEK_DATA and SEEK_HOLE."""
    out = []
    fd = os.open(path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        off = 0
        while off < size:
            try:
                data = os.lseek(fd, off, os.SEEK_DATA)
            except OSError as e:
                if e.errno != errno.ENXIO:
                    raise  # an extent map that cannot be read packs nothing, not zeros
                break  # no data after off
            hole = os.lseek(fd, data, os.SEEK_HOLE)
            out.append((data, hole - data))
            off = hole
    finally:
        os.close(fd)
    return out


GAP = 8 << 20


def split(root, groups, limit):
    """Take the files with more than `limit` bytes of data out of `groups`: returns the
    groups left, the split files [(rel, stat)] and their pieces [(rel, offset, length)]."""
    kept, files, pieces = [], [], []
    for g in groups:
        rel = g[1][0]
        full = os.path.join(root, rel)
        st = os.lstat(full)
        if len(g[1]) != 1 or st.st_nlink != 1 or not os.path.isfile(full) or os.path.islink(full) \
                or g[0] <= limit or any(c in rel for c in "\t\n\r"):
            kept.append(g)
            continue
        files.append((rel, st))
        # Data extents with gaps under GAP between them are one run (the gap travels as
        # zeros, which the restore writes as holes again), so a fragmented image is a few
        # runs, not one piece per extent.
        runs = []
        for off, length in extents(full):
            if runs and off - (runs[-1][0] + runs[-1][1]) < GAP:
                runs[-1][1] = off + length - runs[-1][0]
            else:
                runs.append([off, length])
        for off, length in runs:
            while length > 0:
                n = min(length, limit)
                pieces.append((rel, off, n))
                off += n
                length -= n
    return kept, files, pieces


def balance(groups, n):
    """Largest first into the lightest part. A group whose name list is a piece index
    (an int) is a piece of a split file."""
    parts = [[0, [], []] for _ in range(n)]
    for size, names in sorted(groups, key=lambda g: (-g[0], str(g[1][0]))):
        p = min(parts, key=lambda p: p[0])
        p[0] += size
        if isinstance(names[0], int):
            p[2].extend(names)
        else:
            p[1].extend(names)
    return parts


class Range(io.RawIOBase):
    """A file's bytes [offset, offset + length), for tarfile.addfile."""

    def __init__(self, path, offset, length):
        self.f = open(path, "rb")
        self.f.seek(offset)
        self.left = length

    def readable(self):
        return True

    def readinto(self, b):
        n = self.f.readinto(memoryview(b)[: min(len(b), self.left)])
        self.left -= n
        return n

    def close(self):
        self.f.close()
        super().close()


def write_pieces(root, pieces, idxs, out):
    """Append the pieces `idxs` to `out` as a tar archive of `./.fcvm-seg/<n>` members."""
    with tarfile.open(fileobj=out, mode="w|", format=tarfile.PAX_FORMAT) as t:
        for i in idxs:
            rel, off, length = pieces[i]
            info = tarfile.TarInfo(f"./.fcvm-seg/{i}")
            info.size = length
            info.mode = 0o600
            # Owned as the file is: the restore extracts as that owner (--same-owner) and
            # writes the piece into the file.
            st = os.stat(os.path.join(root, rel))
            info.uid, info.gid = st.st_uid, st.st_gid
            r = Range(os.path.join(root, rel), off, length)
            t.addfile(info, io.BufferedReader(r, 1 << 20))
            r.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("outdir")
    ap.add_argument("parts", type=int)
    ap.add_argument("--exclude", action="append", default=[])
    ap.add_argument("--split-mib", type=int, default=256)
    a = ap.parse_args()
    if a.split_mib < 1:
        ap.error("--split-mib must be at least 1")
    root = os.path.abspath(a.root).rstrip("/") or "/"
    dirs, groups = walk(root, a.exclude)
    if os.path.lexists(os.path.join(root, ".fcvm-segments")) or os.path.lexists(os.path.join(root, ".fcvm-seg")):
        sys.exit("store-pack: the store already holds .fcvm-segments or .fcvm-seg")
    groups, files, pieces = split(root, groups, a.split_mib << 20)
    groups += [[length + 512, [i]] for i, (_, _, length) in enumerate(pieces)]
    parts = balance(groups, max(1, a.parts))
    os.makedirs(a.outdir, exist_ok=True)
    # The list of split files and pieces rides in `dirs`, from a directory of its own.
    meta = os.path.join(a.outdir, "meta")
    os.makedirs(meta, exist_ok=True)
    jobs = [(f"part-{i:02d}", size, names, idxs) for i, (size, names, idxs) in enumerate(parts) if names or idxs]
    part_of = {i: name for name, _, _, idxs in jobs for i in idxs}
    with open(os.path.join(meta, ".fcvm-segments"), "w") as f:
        for rel, st in files:
            f.write(f"file\t{st.st_size}\t{st.st_mode & 0o7777:o}\t{st.st_uid}\t{st.st_gid}\t{st.st_mtime_ns}\t{rel}\n")
        for i, (rel, off, length) in enumerate(pieces):
            f.write(f"seg\t{i}\t{part_of[i]}\t{off}\t{length}\t{rel}\n")
    jobs.append(("dirs", 0, dirs, []))
    procs = []
    for name, _, names, idxs in jobs:
        # The list goes through a file: written to tar's stdin, one part's list would hold
        # this loop until that tar had read it all, and the parts would pack one by one.
        lst = os.path.join(a.outdir, f"{name}.list")
        with open(lst, "wb") as f:
            f.write(b"".join(n.encode() + b"\0" for n in names))
        out = open(os.path.join(a.outdir, f"{name}.tar.zst"), "wb")
        zst = subprocess.Popen(["zstd", "-q", "-6", "-T0", "-c"], stdin=subprocess.PIPE, stdout=out)
        tar = subprocess.Popen(
            ["tar", "-c", "--sparse", "--numeric-owner", "--format=posix", "--no-recursion", "--null",
             "-C", root, "-T", lst],
            stdout=zst.stdin)
        result = {}

        def feed(tar=tar, zst=zst, idxs=idxs, result=result, name=name):
            # The tar archive, then the pieces' archive, into the one zstd stream.
            try:
                result["tar"] = tar.wait()
                if idxs:
                    write_pieces(root, pieces, idxs, zst.stdin)
                if name == "dirs":
                    with tarfile.open(fileobj=zst.stdin, mode="w|", format=tarfile.PAX_FORMAT) as t:
                        t.add(os.path.join(meta, ".fcvm-segments"), arcname="./.fcvm-segments")
                result["pieces"] = 0
            except Exception as e:  # noqa: BLE001
                print(f"store-pack: {e}", file=sys.stderr)
                result["pieces"] = 1
            finally:
                zst.stdin.close()

        th = threading.Thread(target=feed)
        th.start()
        procs.append((name, th, result, zst, out, lst))
    failed = False
    for name, th, result, zst, out, lst in procs:
        th.join()
        if result.get("tar") != 0 or result.get("pieces") != 0 or zst.wait() != 0:
            print(f"store-pack: packing {name} failed", file=sys.stderr)
            failed = True
        out.close()
        os.unlink(lst)
    os.unlink(os.path.join(meta, ".fcvm-segments"))
    os.rmdir(meta)
    if failed:
        sys.exit(1)
    with open(os.path.join(a.outdir, "parts"), "w") as f:
        f.write("".join(f"{name}\n" for name, _, _, _ in jobs if name != "dirs"))
    for name, size, names, idxs in jobs:
        z = os.path.getsize(os.path.join(a.outdir, f"{name}.tar.zst"))
        print(f"store-pack: {name}: {len(names)} entries and {len(idxs)} pieces, {size >> 20} MiB in, {z >> 20} MiB packed")
    print(f"store-pack: {len(files)} files split into {len(pieces)} pieces of at most {a.split_mib} MiB")


if __name__ == "__main__":
    main()
