#!/usr/bin/env python3
"""Seal of a built Kitesurf tree: what `wrangler dev` will run, as one digest.

    kitesurf_seal.py write  --src DIR --out BUILD.json --rev SHA --node V --pnpm V --bazel V --arch A
    kitesurf_seal.py verify --src DIR --seal BUILD.json --rev SHA

The digest covers every regular file under the Kitesurf checkout except git's own
directory and the run-time state wrangler writes (every `.wrangler/`): the tracked
sources, the WebAssembly and bundles the Bazel build staged into the tree, and
the whole installed npm graph, workerd's binary included. Symlinks are recorded
by their target, not followed, so Bazel's convenience links into its output root
neither enter the digest nor hide a changed file behind one.

`verify` recomputes it and refuses a tree whose source revision, tracked files or
digest differ from the seal. kitesurf.sh runs it before the engine starts and
again after the last request, as reqbench re-checks its runtime bundle.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys

SKIP_DIRS = {".git", ".wrangler"}


PLACEHOLDER = b"@KITESURF_DIR@"


def manifest(src: str):
    """(digest, count, {path: value}) of the tree, independent of where it lives.

    Every occurrence of KITESURF_DIR (the checkout's parent) in a file or a link
    is hashed as a placeholder, so two builds of one commit in different
    directories, or on different boxes, seal to the same digest when their
    contents agree. Skipped as install-specific and never run by the arm: Bazel's
    convenience links at the root (into its output root), pnpm's `.bin` command
    shims and its `.modules.yaml` install record (a timestamp, the store path).
    """
    base = os.fsencode(os.path.dirname(os.path.abspath(src)))
    rows = []
    for root, dirs, files in os.walk(src, followlinks=False):
        rel_root = os.path.relpath(root, src)
        keep = []
        for d in dirs:
            path = os.path.join(root, d)
            rel = os.path.normpath(os.path.join(rel_root, d))
            if os.path.islink(path):
                if rel_root == "." and d.startswith("bazel-"):
                    continue
                rows.append((rel, "link:" + relocated(os.readlink(path), base)))
            elif d in SKIP_DIRS:
                continue
            elif d in (".cache", ".bin") and os.path.basename(root) == "node_modules":
                # pnpm's command shims: generated per install, not run by the arm
                # (kitesurf.sh starts wrangler from its own entry point).
                continue
            else:
                keep.append(d)
        dirs[:] = sorted(keep)
        for f in sorted(files):
            if f == ".modules.yaml" and os.path.basename(root) == "node_modules":
                continue  # pnpm's install record: timestamps and the store path
            path = os.path.join(root, f)
            rel = os.path.normpath(os.path.join(rel_root, f))
            if os.path.islink(path):
                if rel_root == "." and f.startswith("bazel-"):
                    continue
                rows.append((rel, "link:" + relocated(os.readlink(path), base)))
                continue
            st = os.lstat(path)
            with open(path, "rb") as handle:
                data = handle.read()
            h = hashlib.sha256(data.replace(base, PLACEHOLDER))
            mode = "x" if st.st_mode & 0o111 else "-"
            rows.append((rel, f"{mode}:{h.hexdigest()}"))
    rows.sort()
    digest = hashlib.sha256()
    for rel, value in rows:
        digest.update(rel.encode("utf-8", "surrogateescape") + b"\0" + value.encode() + b"\n")
    return digest.hexdigest(), len(rows), dict(rows)


def relocated(target: str, base: bytes) -> str:
    return os.fsencode(target).replace(base, PLACEHOLDER).decode("utf-8", "surrogateescape")


def file_sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def git(src, *args):
    return subprocess.run(["git", "-C", src, *args], check=True, capture_output=True,
                          text=True).stdout.strip()


def origin_url(src):
    try:
        return git(src, "remote", "get-url", "origin")
    except subprocess.CalledProcessError:
        return None


def tracked_clean(src):
    return git(src, "status", "--porcelain", "--untracked-files=no") == ""


def package_version(src, prefix):
    """The version pnpm installed for one package, from its .pnpm directory name."""
    store = os.path.join(src, "node_modules", ".pnpm")
    names = sorted(n for n in os.listdir(store) if n.startswith(prefix + "@")) if os.path.isdir(store) else []
    return [n[len(prefix) + 1:].split("_")[0].split("(")[0] for n in names]


def write(args):
    if git(args.src, "rev-parse", "HEAD") != args.rev:
        sys.exit(f"BLOCKED: {args.src} is not at {args.rev}")
    if not tracked_clean(args.src):
        sys.exit(f"BLOCKED: tracked files in {args.src} differ from {args.rev}")
    digest, count, files = manifest(args.src)
    workerd = [rel for rel in files if rel.endswith("/bin/workerd") and "@cloudflare+workerd-linux" in rel]
    record = {
        "kitesurf_rev": args.rev,
        "kitesurf_repo": origin_url(args.src),
        "tree_sha256": digest,
        "tree_files": count,
        "node": args.node,
        "pnpm": args.pnpm,
        "bazel": args.bazel,
        "arch": args.arch,
        "wrangler": package_version(args.src, "wrangler"),
        "miniflare": package_version(args.src, "miniflare"),
        "workerd": {rel: files[rel].split(":", 1)[1] for rel in workerd},
        "node_sha256": file_sha256(args.node_bin),
    }
    if not workerd:
        sys.exit("BLOCKED: the installed tree holds no workerd binary for this platform")
    tmp = args.out + ".tmp"
    with open(tmp, "w") as handle:
        json.dump(record, handle, indent=1, sort_keys=True)
        handle.write("\n")
    os.replace(tmp, args.out)
    print(json.dumps({"tree_sha256": digest, "tree_files": count}))


def verify(args):
    with open(args.seal) as handle:
        seal = json.load(handle)
    if seal.get("kitesurf_rev") != args.rev:
        sys.exit(f"BLOCKED: the seal is for {seal.get('kitesurf_rev')}, the pin is {args.rev}; "
                 "rebuild with make bench-kitesurf-build")
    if git(args.src, "rev-parse", "HEAD") != args.rev:
        sys.exit(f"BLOCKED: {args.src} is not at {args.rev}")
    if not tracked_clean(args.src):
        sys.exit(f"BLOCKED: tracked files in {args.src} differ from {args.rev}")
    # The engine runs under the pinned node from KITESURF_DIR/tools, outside the tree.
    if file_sha256(args.node_bin) != seal.get("node_sha256"):
        sys.exit(f"BLOCKED: {args.node_bin} is not the node binary the build sealed; rebuild")
    digest, count, _files = manifest(args.src)
    if digest != seal.get("tree_sha256"):
        sys.exit(f"BLOCKED: the built Kitesurf tree changed since it was sealed "
                 f"({count} files, {digest} != {seal.get('tree_sha256')}); rebuild")
    print(json.dumps({"tree_sha256": digest, "tree_files": count}))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    w = sub.add_parser("write")
    w.add_argument("--src", required=True)
    w.add_argument("--out", required=True)
    w.add_argument("--rev", required=True)
    for name in ("node", "pnpm", "bazel", "arch"):
        w.add_argument(f"--{name}", required=True)
    w.add_argument("--node-bin", required=True)
    v = sub.add_parser("verify")
    v.add_argument("--src", required=True)
    v.add_argument("--seal", required=True)
    v.add_argument("--rev", required=True)
    v.add_argument("--node-bin", required=True)
    args = ap.parse_args()
    (write if args.cmd == "write" else verify)(args)


if __name__ == "__main__":
    main()
