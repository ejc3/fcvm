"""Builds fcvm state layout 2 stores for the harness unit tests.

Every fixture goes through reqbench's own path functions, so a test store and the
readers cannot disagree about where a state lives.  This file is test support and is
not part of the sealed runtime.
"""

import json
import os

import reqbench

# Shell for fake fcvm stubs: `fixture_state_path <state_dir> <vm_id>` makes the bucket
# directory and prints the state path, the way fcvm's first write does.
SH_STATE_PATH = r'''fixture_state_path() {
    local b
    b=$(printf %s "$2" | sha256sum | cut -c1-2)
    mkdir -p "$1/vms/$b"
    printf '%s/vms/%s/%s.json' "$1" "$b" "$2"
}
'''


def make_store(state_dir, layout=True):
    """Create the state directory, with the layout marker fcvm writes first."""
    os.makedirs(state_dir, exist_ok=True)
    if layout:
        with open(os.path.join(state_dir, "layout"), "w") as handle:
            handle.write(reqbench.STATE_LAYOUT + "\n")
    return state_dir


def write_vm_state(state_dir, vm_id, state, lock=True, pid_entry=True):
    """Publish ``state`` like fcvm: lock, temp file, rename, then the PID index entry."""
    path = reqbench.vm_state_path(state_dir, vm_id)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if lock:
        open(path + ".lock", "a").close()
    with open(path + ".tmp", "w") as handle:
        json.dump(state, handle)
    os.rename(path + ".tmp", path)
    pid = state.get("pid")
    if pid_entry and pid:
        entry_dir = reqbench.pid_bucket_dir(state_dir, pid)
        os.makedirs(entry_dir, exist_ok=True)
        open(os.path.join(entry_dir, f"{pid}.{vm_id}"), "a").close()
    return path


def leftover(state_dir):
    """Every file still under ``vms/``: what a finished run must not leave behind."""
    found = []
    for root, _dirs, files in os.walk(os.path.join(state_dir, "vms")):
        found.extend(os.path.join(root, name) for name in files)
    return sorted(found)
