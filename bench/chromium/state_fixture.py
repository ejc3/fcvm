"""Builds fcvm state layout 2 stores, and fake fcvm binaries that name them, for the
harness unit tests.

Every fixture goes through reqbench's own path functions, so a test store and the
readers cannot disagree about where a state lives.  This file is test support and is
not part of the sealed runtime.
"""

import json
import os
import shlex

import reqbench

# Shell for fake fcvm stubs: answer `fcvm paths --json` the way fcvm does, with the
# directories the test put in FCVM_FAKE_STATE_DIR and FCVM_FAKE_DATA_DIR (the data
# directory defaults to the state directory's parent). Put it first in the stub.
SH_FCVM_PATHS = r'''if [ "${1:-}" = paths ]; then
    printf '{"state_dir": "%s", "data_dir": "%s"}\n' "$FCVM_FAKE_STATE_DIR" \
        "${FCVM_FAKE_DATA_DIR:-$(dirname "$FCVM_FAKE_STATE_DIR")}"
    exit 0
fi
'''


def sh_fcvm_paths(state_dir, data_dir=None):
    """Shell for a fake fcvm stub that answers `fcvm paths --json` with these directories,
    written into the stub so the answer survives an environment reset (sudo)."""
    answer = json.dumps({"state_dir": state_dir,
                         "data_dir": data_dir or os.path.dirname(state_dir)})
    return (f'if [ "${{1:-}}" = paths ]; then printf \'%s\\n\' {shlex.quote(answer)}; '
            "exit 0; fi\n")


def write_fake_fcvm(path, state_dir, data_dir=None, rest="exit 0\n"):
    """A fake fcvm that answers `paths --json` with these directories and runs ``rest``
    (bash) for every other command."""
    with open(path, "w") as handle:
        handle.write("#!/bin/bash\n" + sh_fcvm_paths(state_dir, data_dir) + rest)
    os.chmod(path, 0o755)
    return path


# Shell for fake fcvm stubs: `fixture_state_path <state_dir> <vm_id>` makes the bucket
# directory and prints the state path, the way fcvm's first write does.
SH_STATE_PATH = r'''fixture_state_path() {
    local b
    b=$(printf %s "$2" | sha256sum | cut -c1-1)
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
