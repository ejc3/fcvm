#!/usr/bin/env python3
"""Publish the VMM builds `fcvm setup --vmm-artifacts` wrote, and record their digests.

usage:
  vmm-artifacts.py publish <dir>             each <dir>/**/manifest.json entry becomes the asset of
                                             release vmm-<name without .bin>; a release that exists
                                             must already hold the same bytes, or this fails
  vmm-artifacts.py record <dir> <config>     rewrite <config>'s [[vmm_artifacts.files]] entries to
                                             the manifests' entries (src/setup/vmm_artifacts.rs)

`publish` needs gh with a token that may create releases. `record` changes nothing else in the
config: the entries sit right after the [vmm_artifacts] table's own keys.
"""
import hashlib
import json
import pathlib
import re
import subprocess
import sys

FIELDS = ("name", "commit", "arch", "libc", "sha256")


def manifests(root):
    """Every entry of every manifest.json under `root`, with the file beside it, by name."""
    entries = {}
    for manifest in sorted(pathlib.Path(root).rglob("manifest.json")):
        for entry in json.loads(manifest.read_text()):
            if set(entry) != set(FIELDS):
                raise SystemExit(f"{manifest}: entry with fields {sorted(entry)}, want {sorted(FIELDS)}")
            if not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]):
                raise SystemExit(f"{manifest}: {entry['name']} has digest {entry['sha256']!r}")
            path = manifest.parent / entry["name"]
            got = hashlib.sha256(path.read_bytes()).hexdigest()
            if got != entry["sha256"]:
                raise SystemExit(f"{path}: sha256 {got}, the manifest says {entry['sha256']}")
            if entry["name"] in entries and entries[entry["name"]][0] != entry:
                raise SystemExit(f"two builds named {entry['name']} with different entries")
            entries[entry["name"]] = (entry, path)
    if not entries:
        raise SystemExit(f"no manifest.json entries under {root}")
    return entries


def record(config, entries):
    """`config` with its [[vmm_artifacts.files]] entries replaced by `entries`, sorted by name."""
    lines = config.splitlines(keepends=True)
    try:
        start = lines.index("[vmm_artifacts]\n")
    except ValueError:
        raise SystemExit("the config has no [vmm_artifacts] table")
    # the table's own keys: up to the first blank line, comment or table header
    keys_end = start + 1
    while keys_end < len(lines) and re.match(r"[A-Za-z_]+ *=", lines[keys_end]):
        keys_end += 1
    # existing entries: [[vmm_artifacts.files]] blocks of key lines, each followed by a blank line
    end = keys_end
    while end < len(lines):
        line = lines[end].strip()
        if line not in ("", "[[vmm_artifacts.files]]") and not re.match(r"(name|commit|arch|libc|sha256) = ", line):
            break
        end += 1
    blocks = []
    for name in sorted(entries):
        entry = entries[name]
        blocks.append("\n[[vmm_artifacts.files]]\n" + "".join(f'{k} = "{entry[k]}"\n' for k in FIELDS))
    return "".join(lines[:keys_end]) + "".join(blocks) + "\n" + "".join(lines[end:])


def gh(*args, check=True):
    return subprocess.run(["gh", *args], check=check, capture_output=True, text=True)


def publish(entries):
    for name, (entry, path) in sorted(entries.items()):
        tag = "vmm-" + name.removesuffix(".bin")
        view = gh("release", "view", tag, "--json", "assets", check=False)
        if view.returncode == 0:
            assets = [a["name"] for a in json.loads(view.stdout)["assets"]]
            if name not in assets:
                raise SystemExit(f"release {tag} exists without {name}: refusing to add to it")
            download = pathlib.Path("/tmp") / f"{tag}-published"
            download.mkdir(exist_ok=True)
            gh("release", "download", tag, "-p", name, "-D", str(download), "--clobber")
            got = hashlib.sha256((download / name).read_bytes()).hexdigest()
            if got != entry["sha256"]:
                raise SystemExit(f"release {tag} holds {name} with sha256 {got}, this build is {entry['sha256']}")
            print(f"{tag}: already published with this digest")
            continue
        if "release not found" not in (view.stderr + view.stdout).lower():
            raise SystemExit(f"cannot tell whether release {tag} exists: {view.stderr.strip()}")
        notes = "\n".join(f"{k}: {entry[k]}" for k in FIELDS)
        gh("release", "create", tag, str(path), "--title", tag, "--notes", notes)
        print(f"{tag}: published")


def main(argv):
    if len(argv) == 3 and argv[1] == "publish":
        publish(manifests(argv[2]))
    elif len(argv) == 4 and argv[1] == "record":
        config = pathlib.Path(argv[3])
        entries = {name: entry for name, (entry, _) in manifests(argv[2]).items()}
        config.write_text(record(config.read_text(), entries))
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
