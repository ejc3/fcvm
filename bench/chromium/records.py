#!/usr/bin/env python3
"""Raw benchmark records kept out of the checkout.

The per-request logs, analyzer dumps, server logs and traces of a run are too
large to keep in every checkout of fcvm. They are kept in
ejc3/fcvm-bench-records at the same path, and results/records-<campaign>.json
lists each one: its path, size and sha256, the fcvm commit that last held it in
its tree (when it was ever committed here) and the commit of the records
repository that holds it. The summaries the report quotes stay in results/,
and test_records_cap.py fails any committed file there over CAP_BYTES.

read_bytes(path) returns a file's bytes: from the working tree when it is
there, otherwise from one of the two commits the manifest names, checked
against the manifest's size and sha256. The fcvm commit is read from this
repository's own object store (CI fetches it, see commits); the records
commit from a checkout of ejc3/fcvm-bench-records named by
FCVM_BENCH_RECORDS. A record that neither can supply is an error that says
how to get it, never an empty read.

    records.py commits              the fcvm commits the manifests name
    records.py verify [--all]       read every listed file from each place it
                                    can be read and check its size and sha256
    records.py move --campaign ID --records CHECKOUT RUN_DIR...
                                    copy a run's raw files into the records
                                    checkout at the same path and print what
                                    to commit; then
    records.py manifest --campaign ID --records CHECKOUT RUN_DIR...
                                    write results/records-<ID>.json from the
                                    records checkout's HEAD and remove the
                                    moved files from this tree
"""

import argparse
import fnmatch
import functools
import glob
import hashlib
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.realpath(os.path.dirname(os.path.dirname(HERE)))
RESULTS = os.path.join(HERE, "results")
# bench/boot-race keeps its campaigns' raw files the same way, with its manifests beside its runs.
BOOT_RACE_RESULTS = os.path.join(REPO, "bench", "boot-race", "results")
MANIFEST_DIRS = (RESULTS, BOOT_RACE_RESULTS)
MANIFEST_GLOB = " or ".join(os.path.join(d, "records-*.json") for d in MANIFEST_DIRS)
RECORDS_ENV = "FCVM_BENCH_RECORDS"
RECORDS_REPOSITORY = "ejc3/fcvm-bench-records"
# The largest file a records directory may commit. The largest summary that
# stays is results/campaign-40657613-summary.json, 252,164 bytes; the cap is
# twice that, rounded up to a power of two.
CAP_BYTES = 512 * 1024
# The directories that hold benchmark records, relative to the repository.
RECORD_DIRS = ("bench/chromium/results", "bench/chromium/report/evidence", "bench/boot-race/results")
# The raw files of a run, by path inside the run directory. Everything else a
# run commits (its indexes, evidence, provenance and summaries) stays.
RAW = (
    "analysis.json",
    "reqbench.jsonl",
    "restore-ack.jsonl",
    "logs/serve.log",
    "corpus-listen.log",
    "*.corpus-listen.log",
    "corpus-access.log",
    "corpus-dns.log",
    "hostcdp.jsonl",
    "*/hostcdp.jsonl",
    "memory/samples.jsonl",
    "cpuprobe-*-curated.json",
    "scale-*/requests.jsonl",
    "scale-*/bursts.jsonl",
    "scale-*/status.json",
    "scale-*/schedule.json",
    "scale-*/host-samples.jsonl",
    "scale-*/host-control.jsonl",
)
# The raw files of a bench/boot-race campaign: every sample with the workload's identities, the Lambda REPORT
# lines, the driver's configuration (instance and resource ids) and the fcvm logs. Its setup.json stays.
BOOT_RACE_RAW = (
    "samples.jsonl",
    "lambda-reports.json",
    "config.json",
    "*.log",
    "fcvm-logs/*.log",
)


class MissingRecord(FileNotFoundError):
    """A moved record that could not be read, or whose bytes do not match."""


def repo_relative(path):
    """path as the repository-relative name the manifests use. The directory's
    symlinks are resolved, so a run named through an alias finds its records;
    the file's own are not, so a listed name that is a link stays listed."""
    head, leaf = os.path.split(os.fspath(path))
    return os.path.relpath(os.path.join(os.path.realpath(head or "."), leaf), REPO).replace(os.sep, "/")


def is_raw(relative_in_run, patterns=RAW):
    return any(fnmatch.fnmatchcase(relative_in_run, pattern) for pattern in patterns)


@functools.lru_cache(maxsize=None)
def _manifests():
    entries = {}
    for manifest_path in sorted(p for d in MANIFEST_DIRS for p in glob.glob(os.path.join(d, "records-*.json"))):
        with open(manifest_path) as handle:
            manifest = json.load(handle)
        for entry in manifest["files"]:
            if entry["path"] in entries:
                raise ValueError(f"{entry['path']} is listed by two manifests")
            entries[entry["path"]] = {
                **entry,
                "manifest": os.path.relpath(manifest_path, REPO),
                "fcvm_commit": manifest.get("fcvm_commit"),
                "repository": manifest["repository"],
                "records_commit": manifest["records_commit"],
            }
    return entries


def entry(path):
    """The manifest entry for path, or None when no manifest lists it."""
    return _manifests().get(repo_relative(path))


def listed(prefix=""):
    """Every listed path under the repository-relative prefix."""
    return sorted(p for p in _manifests() if p.startswith(prefix))


def exists(path):
    return os.path.isfile(path) or entry(path) is not None


class _Batch:
    """One `git cat-file --batch` per object store: a run lint reads hundreds of
    blobs, and a process per blob costs more than the read."""

    def __init__(self, git_dir):
        self.proc = subprocess.Popen(
            ["git", "-C", git_dir, "cat-file", "--batch"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )

    def read(self, spec):
        self.proc.stdin.write(spec.encode() + b"\n")
        self.proc.stdin.flush()
        header = self.proc.stdout.readline().decode().split()
        if len(header) != 3 or header[1] != "blob":
            return None
        size = int(header[2])
        data = self.proc.stdout.read(size)
        self.proc.stdout.read(1)
        return data


@functools.lru_cache(maxsize=None)
def _batch(git_dir):
    return _Batch(git_dir)


def _sources(found):
    """(where, git directory, object spec) for each place found can be read."""
    out = []
    if found.get("fcvm_commit"):
        out.append((f"ejc3/fcvm@{found['fcvm_commit']}:{found['path']}", REPO,
                    f"{found['fcvm_commit']}:{found['path']}"))
    checkout = os.environ.get(RECORDS_ENV)
    where = f"{found['repository']}@{found['records_commit']}:{found['path']}"
    if checkout:
        out.append((where, checkout, f"{found['records_commit']}:{found['path']}"))
    return out, where


def checked(found, data, where):
    """data, when it is the size and sha256 found lists; MissingRecord otherwise."""
    digest = hashlib.sha256(data).hexdigest()
    if len(data) != found["size"] or digest != found["sha256"]:
        raise MissingRecord(
            f"{found['path']}: {where} holds {len(data)} bytes with sha256 {digest}, "
            f"not the {found['size']} bytes with sha256 {found['sha256']} "
            f"{found['manifest']} lists"
        )
    return data


def _read_from(found, where, git_dir, spec):
    """found's bytes from one place, or None when that place lacks the object."""
    data = _batch(git_dir).read(spec)
    return None if data is None else checked(found, data, where)


def read_moved(found):
    """The bytes of one manifest entry, checked against its size and sha256."""
    sources, records_where = _sources(found)
    tried = []
    for where, git_dir, spec in sources:
        data = _read_from(found, where, git_dir, spec)
        if data is not None:
            return data
        tried.append(f"{where} is not in {git_dir}")
    hint = []
    if found.get("fcvm_commit"):
        hint.append(f"`git fetch --depth=1 origin {found['fcvm_commit']}`")
    hint.append(f"set {RECORDS_ENV} to a checkout of {found['repository']} that holds {records_where}")
    raise MissingRecord(
        f"{found['path']} is not in this checkout; {found['manifest']} lists it. "
        f"Not read: {'; '.join(tried) or 'no source to read it from'}. To read it, "
        + " or ".join(hint)
    )


def read_bytes(path):
    """A record's bytes, from the working tree or, for a moved one, its manifest.
    A listed record is held to the manifest wherever its bytes come from."""
    found = entry(path)
    if os.path.isfile(path):
        with open(path, "rb") as handle:
            data = handle.read()
        return data if found is None else checked(found, data, path)
    if found is None:
        raise FileNotFoundError(f"{path}: no such record in the tree or in any {MANIFEST_GLOB}")
    return read_moved(found)


def read_text(path):
    return read_bytes(path).decode("utf-8")


def read_json(path):
    return json.loads(read_bytes(path))


def first_line(path):
    data = read_bytes(path)
    end = data.find(b"\n")
    return data[: end + 1 if end >= 0 else len(data)].decode("utf-8")


def _git(*args, cwd=REPO):
    return subprocess.run(["git", *args], cwd=cwd, check=True,
                          capture_output=True, text=True).stdout


def committed_files(prefix):
    """Committed (index) paths under the repository-relative prefix."""
    return _git("ls-files", "-z", "--", prefix).split("\0")[:-1]


def run_raw_files(run_dir):
    """The committed raw files of one run directory, repository-relative."""
    prefix = os.path.relpath(os.path.realpath(run_dir), REPO).replace(os.sep, "/") + "/"
    boot_race = os.path.relpath(BOOT_RACE_RESULTS, REPO).replace(os.sep, "/") + "/"
    patterns = BOOT_RACE_RAW if prefix.startswith(boot_race) else RAW
    return [p for p in committed_files(prefix) if is_raw(p[len(prefix):], patterns)]


def results_dir(run_dirs):
    """The results directory the runs belong to, where their manifest goes."""
    found = {BOOT_RACE_RESULTS if os.path.realpath(d).startswith(os.path.realpath(BOOT_RACE_RESULTS) + os.sep)
             else RESULTS for d in run_dirs}
    if len(found) != 1:
        raise SystemExit("one manifest covers runs of one results directory")
    return found.pop()


def cmd_commits(_args):
    for commit in sorted({e["fcvm_commit"] for e in _manifests().values() if e.get("fcvm_commit")}):
        print(commit)
    return 0


def cmd_verify(args):
    bad = 0
    count = 0
    if args.all and not os.environ.get(RECORDS_ENV):
        print(f"FAIL: --all needs {RECORDS_ENV}", file=sys.stderr)
        bad += 1
    for path, found in sorted(_manifests().items()):
        sources, _where = _sources(found)
        if not sources:
            print(f"FAIL: {path}: no source to read it from", file=sys.stderr)
            bad += 1
        for where, git_dir, spec in sources:
            try:
                data = _read_from(found, where, git_dir, spec)
            except MissingRecord as error:
                data = None
                print(f"FAIL: {error}", file=sys.stderr)
            else:
                if data is None:
                    print(f"FAIL: {path}: {where} is not in {git_dir}", file=sys.stderr)
            if data is None:
                bad += 1
            else:
                count += 1
    print(f"verified {count} reads of {len(_manifests())} listed files; {bad} failures")
    return 1 if bad else 0


def cmd_move(args):
    for run_dir in args.run_dirs:
        for path in run_raw_files(run_dir):
            target = os.path.join(args.records, path)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            shutil.copyfile(os.path.join(REPO, path), target)
            print(path)
    print(f"now commit them in {args.records}, push, and run "
          f"`records.py manifest --campaign {args.campaign} --records {args.records} ...`",
          file=sys.stderr)
    return 0


def cmd_manifest(args):
    records_commit = _git("rev-parse", "HEAD", cwd=args.records).strip()
    fcvm_commit = _git("rev-parse", "HEAD").strip()
    files = []
    for run_dir in args.run_dirs:
        for path in run_raw_files(run_dir):
            data = open(os.path.join(REPO, path), "rb").read()
            held = _batch(args.records).read(f"{records_commit}:{path}")
            if held != data:
                raise SystemExit(f"{path}: {args.records}@{records_commit} does not hold these bytes")
            in_head = _batch(REPO).read(f"{fcvm_commit}:{path}") == data
            files.append({"path": path, "size": len(data),
                          "sha256": hashlib.sha256(data).hexdigest(),
                          "in_fcvm": in_head})
    if not files:
        raise SystemExit("no raw file to list")
    in_fcvm = {f.pop("in_fcvm") for f in files}
    out = os.path.join(results_dir(args.run_dirs), f"records-{args.campaign}.json")
    if os.path.exists(out):
        raise SystemExit(f"{out} exists; one manifest per campaign")
    manifest = {
        "schema": 1,
        "campaign": args.campaign,
        "repository": RECORDS_REPOSITORY,
        "records_commit": records_commit,
        # Read from this repository's history when every file was committed here.
        "fcvm_commit": fcvm_commit if in_fcvm == {True} else None,
        "files": sorted(files, key=lambda f: f["path"]),
    }
    with open(out, "w") as handle:
        json.dump(manifest, handle, indent=1, sort_keys=True)
        handle.write("\n")
    _git("rm", "-q", "--", *[f["path"] for f in files])
    print(f"{os.path.relpath(out, REPO)}: {len(files)} files, "
          f"{sum(f['size'] for f in files)} bytes")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("commits")
    verify = sub.add_parser("verify")
    verify.add_argument("--all", action="store_true",
                        help=f"also require {RECORDS_ENV}, so every file is read from both places")
    for name in ("move", "manifest"):
        p = sub.add_parser(name)
        p.add_argument("--campaign", required=True)
        p.add_argument("--records", required=True)
        p.add_argument("run_dirs", nargs="+")
    args = parser.parse_args(argv)
    return {"commits": cmd_commits, "verify": cmd_verify,
            "move": cmd_move, "manifest": cmd_manifest}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
