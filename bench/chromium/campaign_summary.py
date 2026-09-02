#!/usr/bin/env python3
"""Index the cells of one campaign into a single JSON file.

    campaign_summary.py --out PATH <run_dir>...

Each run directory holds reqanalyze's analysis.json and the reqbench.jsonl it
names in analysis_identity.inputs (both required; the size and sha256 must
match before any request load is consumed). analysis.json's stall_gate
must have been armed with --stall-max-ms and must have evaluated at least one
record, since an unarmed gate reports passed=true having evaluated nothing),
dns-evidence.json (required when the cell's guest_dns names a baked resolver,
optional otherwise; when present its verdict must be "clean", it must name
the run_id of the analysis it sits beside, it must carry
the replay server's exit status 0, every file it cites must be present and
agree with the sha256 it recorded, each verify bracket must record the run's
resolver in both captured /etc/resolv.conf views and nowhere else, with the
URL probes run under no proxy and exactly the hostnames and URLs the cell
measured answered through it, and every sample in dns-owner.log must name its
serve_pid as the owner of 127.0.0.1:53 with dnsmasq inactive and a load the
evidence accounts for) and diag/summary.json (reqbench.sh diag; required when
the cell's guest_dns names a baked resolver or its guest_env is non-empty,
optional otherwise; when present it must name the run's snapshot generation,
config, tag, engine, backend, UFFD mode and sealed runtime bundle, record
uffd_prefetch "off" (null on the file backend, which has no serve), say
passed=true with that bundle intact, list no violations, cover every URL the
cell measured, carry a load event for each and have rendered each reps times). A diag's
passed=true means only that nothing it was asked to check went wrong, so its
limits must have been armed for this run: on Chromium limits.expect_ips must
be exactly the address set the run's records name (the answers the verify
brackets recorded inside the restored clone, the BENCH_RESOLVE_ALL_TO address
of a resolver-rule golden, and the IP-literal hosts of the measured URLs; a
run whose records name no address cannot hold a diag to anything and is
refused), on WebKit it must be null (that render carries no trace and
reqbench refuses the expectation), and limits.max_load_ms must be a positive
integer no larger than the run's own stall_gate.max_ms. The corpus campaign
arms both at 15000 ms and refuses a DIAG_MAX_LOAD_MS above STALL_MAX_MS
before building anything. The index names every file it was generated from
with its sha256, and carries one cell per run: engine, cpu, memory_mib,
guest_dns, guest_env, the seal identity, publishable, stall_gate_passed,
dns_verdict, load_max_1min (the maximum 1-min load the campaign's sampler saw
during the measured run, when the evidence records it), load_evidence with
descriptive statistics from the continuous owner sampler and every measured
request (overall and per arm), the headline median
blocking_ms per arm with its CI, and for the diag its verdict, violation
count and slowest load event per URL.

The publication rule (REVIEW.md) is to quote only from sealed runs that passed
their gates and were never withdrawn, and publishable=true alone proves none
of that; the run's own gate object has to say passed=true with no reasons,
and to agree with publishable. The seal is the cell's identity, SEAL_FIELDS
below: the runtime
bundle reqbench.sh sealed, the fcvm binary and harness sources hashed into
it, the source revision, and the image and snapshot generation measured. A
cell missing any of them is not a sealed run. A run is withdrawn by a file
named WITHDRAWN in its directory whose first line is the reason, or by
"withdrawn": true in its analysis.json; either refuses the run and the
refusal quotes the reason.

The index is written only when every run is sealed, publishable and not
withdrawn, every stall gate passed, every DNS verdict is clean, every diag
passed, and every run that measured hostname URLs records the resolver that
answered them (guest_dns with its evidence, or a BENCH_RESOLVE_ALL_TO rule). Otherwise nothing is written, an index already at --out is removed,
and the exit status is 5, the same code reqanalyze uses for a refused run: an
index that quietly carried an unpublishable cell would be quoted by someone
who only opened the index. Inputs are only ever read, and each is read once
so the hash names the bytes that were parsed.
"""

import argparse
import ctypes
import fcntl
import hashlib
import ipaddress
import json
import math
import os
import re
import secrets
import statistics
import sys
import tempfile
from contextlib import contextmanager
from urllib.parse import urlsplit

import reqanalyze

VERIFY_STAGES = ("pre", "before-run", "after-run")
# One :53 owner sample, as corpus_campaign.sh's dns_owner_sample prints it.
# The load column is absent from logs written before the sampler carried it.
OWNER_SAMPLE = re.compile(
    r"^(?P<ts>\S+) owner_pid=(?P<owner>\S+) dnsmasq=(?P<dnsmasq>\S+)"
    r"(?: load1=(?P<load>\S+))?$"
)
# The sampler accepts a load only in this shape; dns_load_stats counts only
# these when it reports load_samples and load_max_1min.
LOAD_NUMBER = re.compile(r"^[0-9]+(\.[0-9]+)?$")
# One resolver line of a captured /etc/resolv.conf, read the way glibc reads
# it: the keyword at the start of the line, followed by whitespace and the
# address. An indented line is not a directive to glibc and a `#` or `;` line
# is a comment, so neither configures a resolver.
NAMESERVER_LINE = re.compile(r"^nameserver[ \t]+(\S+)", re.MULTILINE)
# The seal identity of one run, as reqbench.py stamps it into every record's
# meta and reqanalyze carries it into the cell (CELL_FIELDS).
SEAL_FIELDS = (
    "runtime_bundle_sha256",
    "fcvm_sha256",
    "harness_sha256",
    "source_revision",
    "image_id",
    "snapshot_generation_id",
    "snapshot_config_sha256",
)
WITHDRAWN_MARKER = "WITHDRAWN"
# One verify bracket's record, as corpus_campaign.sh's
# verify_replay_answered_the_guest prints it: the stage, the row the bracket's
# window opened at, how many queries fell in it, how many of the corpus hosts
# were seen answered inside it, and which were not.
REPLAY_QUERY_RECORD = re.compile(
    r"^(?P<stage>\S+) since_row=(?P<since>\d+) queries=(?P<queries>\d+) "
    r"hosts_seen=(?P<seen>\d+)/(?P<wanted>\d+) missing=(?P<missing>\S+)$"
)
# The replay evidence a run must still carry to be indexable: the two logs the
# replay server writes, and the campaign's own per-bracket record of what it
# was asked for. That third file is the only thing tying each bracket's window
# to the queries this server received, so a run that lost it cannot be read
# back to check the brackets against.
REPLAY_LOGS = {
    "corpus_dns_log_sha256": "corpus-dns.log",
    "corpus_access_log_sha256": "corpus-access.log",
    "replay_queries_log_sha256": "replay-queries.log",
}
RUN_INPUT_RELATIVE_PATHS = (
    "analysis.json",
    "reqbench.jsonl",
    "dns-evidence.json",
    "dns-owner.log",
    *(f"verify-dns-{stage}.json" for stage in VERIFY_STAGES),
    *REPLAY_LOGS.values(),
    os.path.join("diag", "summary.json"),
)


class RunError(Exception):
    """One run directory cannot be indexed; the message says why."""


def reject_duplicate_keys(pairs):
    seen = {}
    for key, value in pairs:
        if key in seen:
            raise ValueError(f"duplicate JSON key {key!r}")
        seen[key] = value
    return seen


def reject_constant(name):
    raise ValueError(f"non-standard JSON numeric constant {name}")


def read_bytes(path):
    with open(path, "rb") as handle:
        return handle.read()


def parse_json(data, path):
    try:
        return json.loads(
            data.decode("utf-8"),
            object_pairs_hook=reject_duplicate_keys,
            parse_constant=reject_constant,
        )
    except (UnicodeDecodeError, ValueError) as error:
        raise RunError(f"{path}: cannot parse: {error}")


class SourceEntries(list):
    """Public source records paired with the exact paths used to read them."""

    def __init__(self):
        super().__init__()
        self.attempted_paths = []


class Sources:
    """The files one cell was read from, each read once and hashed from those bytes."""

    def __init__(self, run_dir=None):
        self.entries = SourceEntries()
        self.access_dir = os.fspath(run_dir) if run_dir is not None else None
        self.display_dir = str(run_dir) if run_dir is not None else None
        self._attempted_paths = set()

    def display_path(self, path):
        """Translate an fd-pinned input path back to the caller's pathname."""
        path = os.fspath(path)
        if (
            self.access_dir is not None
            and self.access_dir != self.display_dir
            and (
                path == self.access_dir
                or path.startswith(self.access_dir + os.sep)
            )
        ):
            return self.display_dir + path[len(self.access_dir):]
        return path

    def protect(self, path):
        """Record a possible input before validation can stop or its read can fail."""
        display_path = self.display_path(path)
        access_path = os.fspath(path)
        pair = (display_path, access_path)
        if pair not in self._attempted_paths:
            self._attempted_paths.add(pair)
            self.entries.attempted_paths.append(pair)
        return display_path

    def read_json(self, path):
        display_path = self.protect(path)
        try:
            data = read_bytes(path)
        except OSError as error:
            raise RunError(f"{display_path}: cannot read: {error}")
        self.entries.append({
            "path": display_path,
            "sha256": hashlib.sha256(data).hexdigest(),
        })
        return parse_json(data, display_path)

    def read_hashed(self, path):
        """Record a file the index does not parse; returns its sha256."""
        display_path = self.protect(path)
        try:
            data = read_bytes(path)
        except OSError as error:
            raise RunError(f"{display_path}: cannot read: {error}")
        digest = hashlib.sha256(data).hexdigest()
        self.entries.append({"path": display_path, "sha256": digest})
        return data, digest


class PublishedOutput:
    """The still-open inode and exact bytes installed by one atomic write."""

    def __init__(self, descriptor, identity, size, digest):
        self.descriptor = descriptor
        self.identity = identity
        self.size = size
        self.digest = digest

    def close(self):
        os.close(self.descriptor)


def write_json_atomic(path, value):
    payload = (
        json.dumps(value, indent=2, sort_keys=False, allow_nan=False) + "\n"
    ).encode("utf-8")
    directory = os.path.dirname(os.path.abspath(path))
    fd, temp_path = tempfile.mkstemp(prefix=".campaign-summary.", dir=directory)
    temporary_exists = True
    try:
        with os.fdopen(fd, "wb", closefd=False) as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_path, path)
        temporary_exists = False
        return PublishedOutput(
            fd,
            os.fstat(fd),
            len(payload),
            hashlib.sha256(payload).hexdigest(),
        )
    except BaseException:
        os.close(fd)
        if temporary_exists:
            os.unlink(temp_path)
        raise


class PinnedOutput:
    """Keep publication and cleanup in the directory selected at entry."""

    def __init__(self, path):
        self.path = os.fspath(path)
        absolute = os.path.abspath(self.path)
        self.directory = os.path.dirname(absolute)
        self.name = os.path.basename(absolute)
        self.descriptor = os.open(
            self.directory,
            os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY,
        )
        self.directory_identity = os.fstat(self.descriptor)
        self.access_path = f"/proc/self/fd/{self.descriptor}/{self.name}"
        self.initial_identity = self.identity()

    def identity(self):
        try:
            info = os.stat(
                self.name, dir_fd=self.descriptor, follow_symlinks=False)
        except FileNotFoundError:
            return None
        return info

    @staticmethod
    def same_state(left, right):
        fields = (
            "st_dev", "st_ino", "st_mode", "st_size",
            "st_mtime_ns",
        )
        return left is not None and right is not None and all(
            getattr(left, field) == getattr(right, field) for field in fields)

    @staticmethod
    def same_publication_state(left, right):
        return (
            PinnedOutput.same_state(left, right)
            and left.st_ctime_ns == right.st_ctime_ns
        )

    def validation_error(self, publication=None):
        try:
            current_directory = os.stat(self.directory)
        except OSError as error:
            return f"output directory {self.directory} cannot be rechecked: {error}"
        if not os.path.samestat(current_directory, self.directory_identity):
            return f"output directory {self.directory} changed during publication"
        if publication is not None:
            try:
                before = os.fstat(publication.descriptor)
                os.lseek(publication.descriptor, 0, os.SEEK_SET)
                digest = hashlib.sha256()
                size = 0
                while True:
                    chunk = os.read(publication.descriptor, 1024 * 1024)
                    if not chunk:
                        break
                    digest.update(chunk)
                    size += len(chunk)
                after = os.fstat(publication.descriptor)
                current = self.identity()
            except OSError as error:
                return f"output {self.path} cannot be rechecked: {error}"
            if (
                not self.same_publication_state(before, publication.identity)
                or not self.same_publication_state(after, publication.identity)
                or not self.same_publication_state(current, publication.identity)
                or size != publication.size
                or digest.hexdigest() != publication.digest
            ):
                return f"output {self.path} changed during publication"
        return None

    def restore_quarantine(self, quarantine):
        try:
            rename_noreplace(self.descriptor, quarantine, self.name)
        except OSError as error:
            preserved = os.path.join(self.directory, quarantine)
            raise RuntimeError(
                f"cannot restore raced output {self.path}; its bytes remain at "
                f"{preserved}: {error}"
            ) from error

    def unlink_if_unchanged(self, expected_identity, protected_identities=()):
        """Atomically quarantine, inspect, then remove only the expected inode."""
        if (expected_identity is None
                or not self.same_state(self.identity(), expected_identity)):
            return False
        while True:
            quarantine = (
                f".campaign-summary-cleanup-{os.getpid()}-"
                f"{secrets.token_hex(8)}"
            )
            try:
                rename_noreplace(self.descriptor, self.name, quarantine)
                break
            except FileNotFoundError:
                return False
            except FileExistsError:
                continue
        try:
            quarantined = os.stat(
                quarantine,
                dir_fd=self.descriptor,
                follow_symlinks=False,
            )
        except OSError:
            self.restore_quarantine(quarantine)
            raise
        protected = any(
            os.path.samestat(quarantined, identity)
            for identity in protected_identities
        )
        if not self.same_state(quarantined, expected_identity) or protected:
            self.restore_quarantine(quarantine)
            return False
        try:
            os.unlink(quarantine, dir_fd=self.descriptor)
        except OSError:
            self.restore_quarantine(quarantine)
            raise
        return True

    def close(self):
        os.close(self.descriptor)


@contextmanager
def pinned_output(path):
    output = PinnedOutput(path)
    try:
        yield output
    finally:
        output.close()


def rename_noreplace(directory_fd, source, destination):
    """Rename one entry without overwriting a concurrent creator."""
    try:
        renameat2 = ctypes.CDLL(None, use_errno=True).renameat2
    except AttributeError as error:
        raise RuntimeError(
            "renameat2 is required for race-free output cleanup") from error
    renameat2.argtypes = (
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_uint,
    )
    renameat2.restype = ctypes.c_int
    if renameat2(
            directory_fd, os.fsencode(source),
            directory_fd, os.fsencode(destination), 1) != 0:
        error_number = ctypes.get_errno()
        raise OSError(error_number, os.strerror(error_number), source)


def positive_int(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def is_sha256(value):
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(c in "0123456789abcdef" for c in value)
    )


def withdrawal_reason(marker):
    """The first line of a WITHDRAWN marker; a marker that cannot be read
    still withdraws the run."""
    try:
        with open(marker, "rb") as handle:
            first_line = handle.readline()
    except OSError as error:
        return f"(marker present but unreadable: {error})"
    reason = first_line.decode("utf-8", errors="replace").strip()
    return reason or "(no reason recorded in the marker)"


def check_owner_log(run_dir, evidence, owner_bytes):
    """Hold every sample in dns-owner.log to the rule the campaign applied at
    the verdict: the owner of 127.0.0.1:53 is serve_pid, dnsmasq is inactive,
    and the load column adds up to load_samples and load_max_1min. The
    evidence's own first_mismatch is a claim about these lines, not proof;
    a log rewritten under the same line count indexed clean on it."""
    serve_pid = evidence.get("serve_pid")
    if not positive_int(serve_pid):
        raise RunError(
            f"{run_dir}: dns-evidence.json is clean but records serve_pid="
            f"{serve_pid!r}; the samples have no server to be held to"
        )
    try:
        text = owner_bytes.decode("utf-8")
    except UnicodeDecodeError as error:
        raise RunError(f"{run_dir}: dns-owner.log is not UTF-8: {error}")
    loads = []
    for number, line in enumerate(text.splitlines(), 1):
        match = OWNER_SAMPLE.match(line)
        if match is None:
            raise RunError(f"{run_dir}: dns-owner.log line {number} is not a sample: {line!r}")
        if match["owner"] != str(serve_pid):
            raise RunError(
                f"{run_dir}: dns-owner.log line {number} names {match['owner']} as the "
                f"owner of 127.0.0.1:53, not corpus_serve {serve_pid}: {line!r}"
            )
        if match["dnsmasq"] != "inactive":
            raise RunError(
                f"{run_dir}: dns-owner.log line {number} records dnsmasq="
                f"{match['dnsmasq']}, not inactive: {line!r}"
            )
        if match["load"] is not None:
            if not LOAD_NUMBER.match(match["load"]):
                raise RunError(
                    f"{run_dir}: dns-owner.log line {number} carries a load1 that is "
                    f"not a number: {line!r}"
                )
            load = float(match["load"])
            if not math.isfinite(load):
                raise RunError(
                    f"{run_dir}: dns-owner.log line {number} carries a load1 that is "
                    f"not finite: {line!r}"
                )
            loads.append(load)
    load_samples = evidence.get("load_samples", 0)
    load_max = evidence.get("load_max_1min")
    if isinstance(load_samples, bool) or not isinstance(load_samples, int):
        raise RunError(f"{run_dir}: dns-evidence.json records load_samples={load_samples!r}")
    if (
        isinstance(load_max, bool)
        or not (load_max is None or isinstance(load_max, (int, float)))
        or (load_max is not None and not math.isfinite(load_max))
        or (load_max is not None and load_max < 0)
    ):
        raise RunError(f"{run_dir}: dns-evidence.json records load_max_1min={load_max!r}")
    found_max = max(loads) if loads else None
    if len(loads) != load_samples or found_max != load_max:
        raise RunError(
            f"{run_dir}: dns-owner.log carries {len(loads)} load sample(s) with maximum "
            f"{found_max} but dns-evidence.json records load_samples={load_samples}, "
            f"load_max_1min={load_max}"
        )
    return loads


def load_distribution(values):
    """The exact descriptive statistics published for one load series."""
    return {
        "samples": len(values),
        "min": min(values),
        "median": statistics.median(values),
        "max": max(values),
    }


def bind_analysis_input(run_dir, analysis, size, digest):
    """Hold reqbench.jsonl to the bytes reqanalyze's verdict consumed."""
    identity = analysis.get("analysis_identity")
    inputs = identity.get("inputs") if isinstance(identity, dict) else None
    if not isinstance(inputs, list) or len(inputs) != 1:
        raise RunError(
            f"{run_dir}: analysis.json analysis_identity.inputs must identify "
            "exactly the reqbench.jsonl used for this cell"
        )
    recorded = inputs[0]
    if not isinstance(recorded, dict):
        raise RunError(
            f"{run_dir}: analysis.json analysis_identity.inputs[0] is not an object"
        )
    recorded_digest = recorded.get("sha256")
    recorded_size = recorded.get("size")
    if (
        not isinstance(recorded_digest, str)
        or len(recorded_digest) != 64
        or any(character not in "0123456789abcdefABCDEF" for character in recorded_digest)
        or isinstance(recorded_size, bool)
        or not isinstance(recorded_size, int)
        or recorded_size < 0
    ):
        raise RunError(
            f"{run_dir}: analysis.json analysis_identity.inputs[0] has no "
            "valid size and sha256"
        )
    if recorded_size != size or recorded_digest.lower() != digest:
        raise RunError(
            f"{run_dir}: reqbench.jsonl does not match analysis_identity.inputs "
            f"(current size={size} sha256={digest}, recorded size={recorded_size} "
            f"sha256={recorded_digest})"
        )


def request_load_evidence(run_dir, analysis, arms, sources):
    """Derive per-request load from the exact reqanalyze input bytes."""
    path = os.path.join(run_dir, "reqbench.jsonl")
    display_path = sources.display_path(path)
    if not os.path.isfile(path):
        raise RunError(f"{run_dir}: reqbench.jsonl is missing")
    data, digest = sources.read_hashed(path)
    # The binding precedes parsing, so no value from bytes outside the
    # publication verdict can enter the generated cell.
    bind_analysis_input(run_dir, analysis, len(data), digest)

    rows = []
    for line_number, line in enumerate(data.splitlines(), 1):
        if not line.strip():
            raise RunError(f"{display_path}:{line_number} is blank")
        row = parse_json(line, f"{display_path}:{line_number}")
        if not isinstance(row, dict):
            raise RunError(f"{display_path}:{line_number} is not a JSON object")
        rows.append(row)
    if not rows or rows[0].get("kind") != "meta":
        raise RunError(f"{display_path}: first record is not reqbench metadata")
    if any(row.get("kind") == "meta" for row in rows[1:]):
        raise RunError(f"{display_path}: carries more than one metadata record")
    run_id = analysis.get("run_id")
    if rows[0].get("run_id") != run_id:
        raise RunError(
            f"{display_path}: metadata run_id={rows[0].get('run_id')!r}, not the "
            f"analysis.json run_id={run_id!r}"
        )

    by_arm = {arm: [] for arm in arms}
    for line_number, row in enumerate(rows[1:], 2):
        warmup = row.get("warmup")
        if warmup is True:
            continue
        if warmup is not False:
            raise RunError(
                f"{display_path}:{line_number} has warmup={warmup!r}, not a boolean"
            )
        arm = row.get("arm")
        if arm not in by_arm:
            raise RunError(
                f"{display_path}:{line_number} names arm={arm!r}, not one of "
                f"{list(by_arm)!r} in analysis.json"
            )
        if row.get("run_id") != run_id:
            raise RunError(
                f"{display_path}:{line_number} run_id={row.get('run_id')!r}, not the "
                f"analysis.json run_id={run_id!r}"
            )
        load = row.get("loadavg1")
        if (
            isinstance(load, bool)
            or not isinstance(load, (int, float))
            or not math.isfinite(load)
            or load < 0
        ):
            raise RunError(
                f"{display_path}:{line_number} has invalid loadavg1={load!r}; every "
                "measured request must carry one finite non-negative sample"
            )
        by_arm[arm].append(load)

    for arm, loads in by_arm.items():
        blocking = arms[arm].get("blocking_ms") if isinstance(arms[arm], dict) else None
        expected = blocking.get("n") if isinstance(blocking, dict) else None
        if not positive_int(expected) or len(loads) != expected:
            raise RunError(
                f"{run_dir}: reqbench.jsonl carries {len(loads)} measured load "
                f"samples for arm {arm}, but analysis.json blocking_ms.n is {expected!r}"
            )

    all_loads = [load for loads in by_arm.values() for load in loads]
    return {
        "artifact": display_path,
        **load_distribution(all_loads),
        "per_arm": {
            arm: load_distribution(loads) for arm, loads in by_arm.items()
        },
    }


def resolv_conf_resolvers(text):
    """The resolvers one captured /etc/resolv.conf establishes, in order."""
    return NAMESERVER_LINE.findall(text)


def canonical_ip(value):
    """The address as a string, or None when value is not one."""
    if not isinstance(value, str):
        return None
    try:
        return str(ipaddress.ip_address(value))
    except ValueError:
        return None


def bracket_answers(run_dir, name, verify):
    """The addresses one passing verify bracket's resolver answered.

    HOP D writes passed=true only when every host it asked resolved to the
    expected answer, so a passing bracket whose host is not ok, or whose
    answer is not an address, is a record the index cannot read an answer
    from and is refused.
    """
    hosts = verify.get("hosts")
    if not isinstance(hosts, dict):
        raise RunError(f"{run_dir}: {name} names no hosts")
    answers = set()
    for host, entry in hosts.items():
        if not isinstance(entry, dict) or entry.get("ok") is not True:
            raise RunError(
                f"{run_dir}: {name} records passed=true but host {host} is not ok"
            )
        answer = canonical_ip(entry.get("answer"))
        if answer is None:
            raise RunError(
                f"{run_dir}: {name} records answer {entry.get('answer')!r} for "
                f"{host}, which is not an address"
            )
        answers.add(answer)
    return answers


def check_coverage(run_dir, name, kind, probed, measured):
    """The bracket probed exactly what the run measured, or RunError.

    corpus_campaign.sh gives HOP D the same URL list it measures on, and
    derives the hostnames from it. A bracket over some other list is
    evidence about another campaign's configuration: one that probed a
    single page says nothing about the rest of the corpus, and one that
    probed pages this run never loaded was not run for it.
    """
    missing = sorted(measured - probed)
    if missing:
        raise RunError(
            f"{run_dir}: {name} probed no {kind} for {', '.join(missing)}, "
            f"which this run measured; a bracket over part of the corpus is "
            "not evidence for the rest of it"
        )
    extra = sorted(probed - measured)
    if extra:
        raise RunError(
            f"{run_dir}: {name} probed {kind} {', '.join(extra)}, which this "
            "run did not measure; that bracket was run for another URL list"
        )


def check_verify_bracket(run_dir, name, verify, resolver, measured_urls, measured_hosts):
    """Hold one HOP D bracket to what the campaign asserted when it ran it:
    the clone resolved through `resolver` (None takes the bracket's own, so
    the remaining brackets are held to the first), both captured
    /etc/resolv.conf views named that resolver and no other, the URL probes
    ran with no proxy, and every host and URL the run measured answered
    through it. Returns the resolver the bracket names.

    passed=true is also what HOP D writes when it was given nothing to check,
    so it is not on its own a bracket."""
    if not isinstance(verify, dict):
        raise RunError(f"{run_dir}: {name} is not a JSON object")
    if verify.get("passed") is not True:
        raise RunError(f"{run_dir}: {name} does not record passed=true")
    dns_server = verify.get("dns_server")
    if not isinstance(dns_server, str) or not dns_server:
        raise RunError(
            f"{run_dir}: {name} records dns_server={dns_server!r}; the bracket "
            "names no resolver to have resolved through"
        )
    if resolver is not None and dns_server != resolver:
        raise RunError(
            f"{run_dir}: {name} records dns_server {dns_server!r}, not the "
            f"{resolver!r} this run was measured through"
        )
    # The two /etc/resolv.conf views HOP D captured inside the restored
    # clone: fc-agent writes the VM's from the boot plan and podman derives
    # the container's from it, which is the one the browser reads. dns_server
    # and the host answers are the bracket's claim about where queries went;
    # these are the configuration that sent them, and nothing else in the run
    # records it (proxies_disabled says only that no proxy was honoured). A
    # second nameserver is refused for the reason HOP D refuses one: glibc
    # walks the whole list, so a fallback answers the moment the replay
    # server misses a query.
    for field in ("resolv_conf_vm", "resolv_conf_container"):
        text = verify.get(field)
        if not isinstance(text, str):
            raise RunError(
                f"{run_dir}: {name} records {field}={text!r}; the bracket does "
                "not say what resolver the clone was configured with"
            )
        resolvers = resolv_conf_resolvers(text)
        if not resolvers:
            raise RunError(
                f"{run_dir}: {name} records a {field} with no nameserver line, "
                f"so nothing says the probes reached {dns_server}"
            )
        others = sorted(set(resolvers) - {dns_server})
        if others:
            raise RunError(
                f"{run_dir}: {name} records {field} naming {', '.join(others)}, "
                f"not the {dns_server!r} its answers are credited to"
            )
    if verify.get("proxies_disabled") is not True:
        raise RunError(
            f"{run_dir}: {name} records proxies_disabled="
            f"{verify.get('proxies_disabled')!r}; a URL probe that honoured the "
            "exec's proxy fetched the live site, not the replay server"
        )
    hosts = verify.get("hosts")
    if not isinstance(hosts, dict) or not hosts:
        raise RunError(
            f"{run_dir}: {name} records no resolved host; a bracket that "
            "checked nothing proves nothing"
        )
    check_coverage(run_dir, name, "host", set(hosts), measured_hosts)
    for host, record in sorted(hosts.items()):
        if not isinstance(record, dict) or record.get("ok") is not True:
            raise RunError(f"{run_dir}: {name} records {host} as {record!r}, not resolved")
        if record.get("answer") != dns_server:
            raise RunError(
                f"{run_dir}: {name} records {host} answering "
                f"{record.get('answer')!r}, not the replay address {dns_server!r}"
            )
    urls = verify.get("urls")
    if not isinstance(urls, dict) or not urls:
        raise RunError(
            f"{run_dir}: {name} records no fetched URL; a bracket that "
            "checked nothing proves nothing"
        )
    check_coverage(run_dir, name, "URL", set(urls), measured_urls)
    for url, record in sorted(urls.items()):
        if not isinstance(record, dict) or record.get("ok") is not True:
            raise RunError(f"{run_dir}: {name} records {url} as {record!r}, not fetched")
        status = record.get("status")
        if isinstance(status, bool) or not isinstance(status, int) or not 200 <= status <= 399:
            raise RunError(
                f"{run_dir}: {name} records {url} returning {status!r}, not a 2xx or 3xx"
            )
        if not isinstance(record.get("proxy_env_ignored"), list):
            raise RunError(
                f"{run_dir}: {name} does not record which proxy variables the "
                f"{url} probe ignored, so nothing says the request left through none"
            )
    return dns_server


def check_replay_queries(run_dir, name, data, dns_data, stage_hosts, answers):
    """Read the bracket records back, rather than only hashing them.

    A hash pins bytes; it does not say the bytes mean anything. Every other
    artifact the evidence cites is re-derived at this boundary -- dns-owner.log
    against the recorded sample count, each bracket through
    check_verify_bracket -- and this one was pinned and unread, which made it
    the cheapest thing to forge in a directory the index is asked to publish.

    A run the campaign produced carries exactly one well-formed record per
    bracket, because verify_replay_answered_the_guest fails the bracket when
    the append fails or a name went unanswered, and campaign_fail turns any
    failed bracket into `unclean`. That is a property of runs it produced, not
    of directories this reads. Note that a failing bracket writes its record
    BEFORE the `missing` gate, deliberately, so a `missing=` other than none is
    exactly the trace of a bracket that did not pass.

    The counts a record carries are its own claim about corpus-dns.log, and
    reading only the record left them tied to nothing: `queries=0
    hosts_seen=14/14 missing=none` is impossible and was accepted, and a clean
    fixture whose DNS log held one row claimed 42 queries over 14 hosts and
    indexed (Codex, 2026-08-30). So each claim is re-derived from the log the
    same way replay_qnames_since derives it: an A query this server answered
    with an address a bracket recorded.

    A bracket's window runs from its own `since_row` to the point the bracket
    ended, which nothing records, so the window read here runs to the NEXT
    bracket's `since_row` (the end of the log, for the last). That is a
    superset of the real window, which is the direction that cannot refuse a
    run the campaign produced: the log must merely hold at least what the
    record claimed.
    """
    text = data.decode("utf-8", "replace")
    lines = [line for line in text.splitlines() if line != ""]
    if len(lines) != len(VERIFY_STAGES):
        raise RunError(
            f"{run_dir}: {name} holds {len(lines)} bracket record(s), not the "
            f"{len(VERIFY_STAGES)} a clean campaign writes ({', '.join(VERIFY_STAGES)})"
        )
    # The log's rows, by the line index since_row counts in (replay_log_rows
    # is `wc -l`). A row that will not parse is not a qualifying answer and is
    # not dropped from the indexing, since the window bounds are line numbers.
    rows = []
    for raw in dns_data.decode("utf-8", "replace").splitlines():
        try:
            rows.append(json.loads(raw))
        except ValueError:
            rows.append(None)
    windows = [int(REPLAY_QUERY_RECORD.match(line)["since"])
               if REPLAY_QUERY_RECORD.match(line) else None
               for line in lines]

    previous = -1
    for index, (stage, line) in enumerate(zip(VERIFY_STAGES, lines)):
        record = REPLAY_QUERY_RECORD.match(line)
        if record is None:
            raise RunError(f"{run_dir}: {name} line is not a bracket record: {line!r}")
        if record["stage"] != stage:
            raise RunError(
                f"{run_dir}: {name} names bracket {record['stage']!r} where the "
                f"{stage!r} record belongs; the brackets run in "
                f"{', '.join(VERIFY_STAGES)} order"
            )
        if record["missing"] != "none":
            raise RunError(
                f"{run_dir}: {name} records the {stage} bracket missing "
                f"{record['missing']}, so this replay server did not answer "
                "those names inside that bracket's window"
            )
        seen, wanted = int(record["seen"]), int(record["wanted"])
        if wanted < 1:
            raise RunError(
                f"{run_dir}: {name} records the {stage} bracket over 0 hosts, "
                "so it checked nothing"
            )
        if seen != wanted:
            raise RunError(
                f"{run_dir}: {name} records the {stage} bracket seeing "
                f"{seen} of {wanted} hosts answered, with none reported missing"
            )
        queries = int(record["queries"])
        if queries < seen:
            raise RunError(
                f"{run_dir}: {name} records the {stage} bracket seeing {seen} "
                f"hosts answered inside a window holding {queries} queries, "
                "which cannot have happened"
            )
        hosts = stage_hosts.get(stage) or set()
        if wanted != len(hosts):
            raise RunError(
                f"{run_dir}: {name} records the {stage} bracket over {wanted} "
                f"hosts, but verify-dns-{stage}.json names {len(hosts)}; the "
                "bracket and its record must be about the same hosts"
            )
        since = int(record["since"])
        if since <= previous:
            raise RunError(
                f"{run_dir}: {name} opens the {stage} bracket at row {since}, "
                f"not after the previous bracket's {previous}; a window that "
                "reaches back credits an earlier bracket's queries to this one"
            )
        previous = since
        # Re-derive the claim from corpus-dns.log over a superset of the
        # bracket's window: since_row to the next bracket's, or the end.
        nxt = windows[index + 1] if index + 1 < len(windows) else len(rows)
        if nxt is None or nxt > len(rows):
            nxt = len(rows)
        answered = [row for row in rows[since:nxt]
                    if isinstance(row, dict)
                    and isinstance(row.get("qname"), str)
                    and row.get("qtype") == 1
                    and canonical_ip(row.get("answer")) in answers]
        if len(answered) < queries:
            raise RunError(
                f"{run_dir}: {name} records {queries} answered queries for the "
                f"{stage} bracket, but corpus-dns.log holds {len(answered)} "
                f"from row {since} to {nxt}; the record claims answers this "
                "replay server never logged"
            )
        unlogged = hosts - {row["qname"] for row in answered}
        if unlogged:
            raise RunError(
                f"{run_dir}: {name} records the {stage} bracket seeing every "
                f"host answered, but corpus-dns.log logs no answer from row "
                f"{since} to {nxt} for {', '.join(sorted(unlogged))}"
            )


def check_evidence(run_dir, evidence, sources, guest_dns, measured_urls, run_id):
    """Hold a clean verdict to the files it cites; raise RunError otherwise.

    measured_urls is the cell's own URL list, what every bracket has to have
    covered, and run_id the analysis this evidence has to be about. Returns
    the set of addresses the verify brackets' resolver answered, the run's
    own record of where its pages came from.
    """
    if not isinstance(run_id, str) or not run_id:
        raise RunError(
            f"{run_dir}: analysis.json records run_id={run_id!r}, so its "
            "dns-evidence.json can be bound to no run; re-run reqanalyze"
        )
    if not measured_urls:
        raise RunError(
            f"{run_dir}: analysis.json cell names no url, so nothing says which "
            "pages the verify brackets had to have covered; re-run reqanalyze"
        )
    if not isinstance(evidence, dict):
        # Valid JSON that is not an object ([] for one) has no verdict to
        # read; refusing it here keeps every .get() below on a dict.
        raise RunError(f"{run_dir}: dns-evidence.json is not a JSON object")
    verdict = evidence.get("verdict")
    if verdict != "clean":
        raise RunError(f"{run_dir}: dns-evidence.json verdict is {verdict!r}, not 'clean'")
    # Every file in the bundle is pinned to the others by hash and to nothing
    # else, so a clean bundle from another campaign copied in here passes each
    # of those checks. corpus_campaign.sh records the run_id of the
    # analysis.json its measured run produced; this is where that binding is
    # read.
    evidence_run = evidence.get("run_id")
    if evidence_run != run_id:
        raise RunError(
            f"{run_dir}: dns-evidence.json records run_id={evidence_run!r}, not the "
            f"{run_id!r} of the analysis it sits beside; that evidence was written "
            "for another run"
        )
    if not positive_int(evidence.get("samples")):
        raise RunError(
            f"{run_dir}: dns-evidence.json records samples={evidence.get('samples')!r}; "
            "a clean verdict needs at least one :53 owner sample"
        )
    if evidence.get("first_mismatch") is not None:
        raise RunError(
            f"{run_dir}: dns-evidence.json is clean but records a mismatching sample: "
            f"{evidence.get('first_mismatch')!r}"
        )
    if evidence.get("sampler_alive_at_stop") is not True:
        raise RunError(
            f"{run_dir}: dns-evidence.json does not show the :53 owner sampler "
            "alive until the measured run ended"
        )
    if evidence.get("dnsmasq_active_after_restore") is not False:
        raise RunError(f"{run_dir}: dns-evidence.json records dnsmasq active after the restores")
    if evidence.get("dnsmasq_state_after_restore") != "inactive":
        raise RunError(
            f"{run_dir}: dns-evidence.json records dnsmasq state "
            f"{evidence.get('dnsmasq_state_after_restore')!r} after the restores, not 'inactive'"
        )
    owner_log = os.path.join(run_dir, "dns-owner.log")
    if not os.path.isfile(owner_log):
        raise RunError(f"{run_dir}: dns-owner.log cited by dns-evidence.json is missing")
    owner_bytes, _digest = sources.read_hashed(owner_log)
    owner_lines = owner_bytes.count(b"\n")
    if owner_lines != evidence["samples"]:
        raise RunError(
            f"{run_dir}: dns-evidence.json records {evidence['samples']} samples but "
            f"dns-owner.log holds {owner_lines} lines"
        )
    owner_loads = check_owner_log(run_dir, evidence, owner_bytes)
    owner_load_evidence = None
    if owner_loads:
        interval = evidence.get("sample_interval_s")
        if (
            isinstance(interval, bool)
            or not isinstance(interval, (int, float))
            or not math.isfinite(interval)
            or interval <= 0
        ):
            raise RunError(
                f"{run_dir}: dns-evidence.json records sample_interval_s="
                f"{interval!r}; continuous load samples need a positive interval"
            )
        owner_load_evidence = {
            "artifact": sources.display_path(owner_log),
            **load_distribution(owner_loads),
            "interval_seconds": interval,
        }
    # The replay server's exit status, recorded by the campaign once the
    # server was gone: 0 is the shutdown sequence completing with both logs
    # closed; 1 is a log line it could not write after the response was
    # sent, so the logs it hashed are short. Only 0 is a complete log.
    serve_status = evidence.get("corpus_serve_exit_status")
    if isinstance(serve_status, bool) or not isinstance(serve_status, int) or serve_status != 0:
        raise RunError(
            f"{run_dir}: dns-evidence.json records corpus_serve exit status "
            f"{serve_status!r}, not 0; the replay logs it hashed may be short"
        )
    # The brackets: every stage the campaign runs, each present, holding the
    # bytes the verdict hashed, and asserting what the campaign asserted.
    # Basenames are resolved inside run_dir so a relocated run directory
    # still indexes; the evidence's own absolute paths are not trusted.
    # Evidence that pins no bracket is refused rather than read: it cannot
    # say whether the files beside it are the ones its verdict was formed on.
    verify_files = evidence.get("verify_files")
    if not isinstance(verify_files, list) or not all(isinstance(p, str) for p in verify_files):
        raise RunError(f"{run_dir}: dns-evidence.json has no verify_files list")
    recorded = evidence.get("verify_file_sha256")
    if not isinstance(recorded, dict):
        raise RunError(
            f"{run_dir}: dns-evidence.json has no verify_file_sha256 map; its "
            "brackets are unpinned, so an edited one cannot be told from the "
            "file the verdict read"
        )
    cited = {os.path.basename(p) for p in verify_files}
    # The hostnames HOP D was given, derived from the measured URLs the way
    # corpus_campaign.sh's corpus_hosts derives them: the authority, port
    # included, in the spelling the campaign passed as VERIFY_DNS_HOSTS.
    measured_hosts = {urlsplit(url).netloc for url in measured_urls}
    resolver = guest_dns if isinstance(guest_dns, str) and guest_dns else None
    answers = set()
    stage_hosts = {}
    for stage in VERIFY_STAGES:
        name = f"verify-dns-{stage}.json"
        if name not in cited:
            raise RunError(f"{run_dir}: dns-evidence.json cites no {name} ({stage} bracket)")
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path):
            raise RunError(f"{run_dir}: {name} cited by dns-evidence.json is missing")
        want = recorded.get(name)
        if not is_sha256(want):
            raise RunError(f"{run_dir}: dns-evidence.json has no sha256 for {name}")
        data, digest = sources.read_hashed(path)
        if digest != want:
            raise RunError(
                f"{run_dir}: {name} sha256 {digest} does not match the {want} "
                "dns-evidence.json recorded at the verdict"
            )
        verify = parse_json(data, sources.display_path(path))
        resolver = check_verify_bracket(
            run_dir, name, verify, resolver, set(measured_urls), measured_hosts
        )
        answers |= bracket_answers(run_dir, name, verify)
        stage_hosts[stage] = set(verify.get("hosts") or {})
    # The replay server's own logs, pinned by hash at the verdict.
    bodies = {}
    for field, name in REPLAY_LOGS.items():
        want = evidence.get(field)
        if not is_sha256(want):
            raise RunError(f"{run_dir}: dns-evidence.json has no sha256 for {name} ({field})")
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path):
            raise RunError(f"{run_dir}: {name} cited by dns-evidence.json is missing")
        data, digest = sources.read_hashed(path)
        if digest != want:
            raise RunError(
                f"{run_dir}: {name} sha256 {digest} does not match the "
                f"{want} dns-evidence.json recorded at the verdict"
            )
        bodies[name] = data
    check_replay_queries(
        run_dir,
        "replay-queries.log",
        bodies["replay-queries.log"],
        bodies["corpus-dns.log"],
        stage_hosts,
        answers,
    )
    return answers, owner_load_evidence


def recorded_addresses(run_dir, measured_urls, guest_env, answers):
    """The addresses the run's own records say its pages came from.

    Three records name one: the resolver answers the verify brackets saw
    inside the restored clone (a corpus run), the BENCH_RESOLVE_ALL_TO
    address a resolver-rule golden baked into Chromium's host resolver
    rules, and the IP-literal hosts of the measured URLs (the medium.html
    fixture on the host loopback). The set is what the diag must have been
    held to; it is derived here rather than assumed to be the replay's
    10.0.2.2 so a golden made against another address is checked against
    that address.
    """
    addresses = set(answers or ())
    for entry in guest_env:
        key, _, value = entry.partition("=")
        if key != "BENCH_RESOLVE_ALL_TO":
            continue
        address = canonical_ip(value)
        if address is None:
            raise RunError(
                f"{run_dir}: analysis.json cell guest_env names {entry!r}, "
                "whose value is not an address"
            )
        addresses.add(address)
    for url in measured_urls:
        address = canonical_ip(urlsplit(url).hostname)
        if address is not None:
            addresses.add(address)
    return addresses


# What binds a diag to the run it sits beside: diag summary key -> analysis
# cell key. The two must name the same snapshot generation and config, the
# same tag, engine, backend and UFFD mode, or the diag diagnosed something
# other than what the run measured. The sealed runtime bundle is checked
# beside them, below.
DIAG_IDENTITY = (
    ("snapshot_generation_id", "snapshot_generation_id"),
    ("snapshot_config_sha256", "snapshot_config_sha256"),
    ("tag", "snapshot"),
    ("engine", "engine"),
    ("backend", "backend"),
    ("uffd_mode", "uffd_mode"),
)


def summarize_diag(run_dir, diag, cell, measured_urls, addresses, stall_max_ms):
    """The diag's verdict, violation count and slowest load per URL; RunError otherwise.

    addresses is the set recorded_addresses derived for the cell, and
    stall_max_ms the run's armed stall gate: the two things the diag's
    limits are held to.
    """
    if not isinstance(diag, dict):
        raise RunError(f"{run_dir}: diag/summary.json is not a JSON object")
    for diag_key, cell_key in DIAG_IDENTITY:
        if diag_key not in diag:
            raise RunError(f"{run_dir}: diag/summary.json names no {diag_key}")
        if cell_key not in cell:
            raise RunError(f"{run_dir}: analysis.json cell names no {cell_key}")
        if diag[diag_key] != cell[cell_key]:
            raise RunError(
                f"{run_dir}: diag/summary.json {diag_key}={diag[diag_key]!r} is not the "
                f"run's {cell_key}={cell[cell_key]!r}; that diag diagnosed something else"
            )
    # The sealed runtime the diag rendered from. reqbench.sh stages fcvm,
    # fc-agent and its five sources into one hash-bound bundle; the run stamps
    # that bundle's hash into every record's meta and reqanalyze carries it
    # into the cell's seal, and reqbench.sh diag records the same hash under
    # the same name. runtime_bundle_intact below says only that the bundle did
    # not change under the phase, so a later standalone diag staged from
    # edited sources overwrites the campaign's summary, matches every snapshot
    # field, reports itself intact, and rendered with other code. A summary
    # naming no bundle cannot be bound to the run at all.
    if "runtime_bundle_sha256" not in diag:
        raise RunError(
            f"{run_dir}: diag/summary.json names no runtime_bundle_sha256; a diag that "
            "does not say which sealed runtime it ran from is not this run's evidence"
        )
    if "runtime_bundle_sha256" not in cell:
        raise RunError(f"{run_dir}: analysis.json cell names no runtime_bundle_sha256")
    if diag["runtime_bundle_sha256"] != cell["runtime_bundle_sha256"]:
        raise RunError(
            f"{run_dir}: diag/summary.json runtime_bundle_sha256="
            f"{diag['runtime_bundle_sha256']!r} is not the run's "
            f"{cell['runtime_bundle_sha256']!r}; that diag rendered with other code "
            "than the run measured with"
        )
    # The diag's clones fault the golden's pages, and a UFFD serve with
    # working-set replay on records those faults into memory.bin.working-set
    # beside the golden, the file the measured run replays. reqbench.sh diag
    # serves with --uffd-prefetch off and records "off"; on the file backend
    # there is no serve and it records null. Anything else, including a
    # summary from before the field existed, is not evidence that the run
    # measured the golden's own working set.
    if "uffd_prefetch" not in diag:
        raise RunError(
            f"{run_dir}: diag/summary.json names no uffd_prefetch; a diag that may have "
            "recorded its renders into the golden's working set is not this run's evidence"
        )
    expected = None if diag["backend"] == "file" else "off"
    if diag["uffd_prefetch"] != expected:
        raise RunError(
            f"{run_dir}: diag/summary.json records uffd_prefetch={diag['uffd_prefetch']!r} "
            f"on the {diag['backend']} backend, not {expected!r}; a diag served with "
            "working-set replay on recorded its renders into the sidecar the run replays"
        )
    if diag.get("runtime_bundle_intact") is not True:
        raise RunError(
            f"{run_dir}: diag/summary.json records runtime_bundle_intact="
            f"{diag.get('runtime_bundle_intact')!r}; the sealed bundle changed under the diag"
        )
    passed = diag.get("passed")
    if not isinstance(passed, bool):
        raise RunError(f"{run_dir}: diag/summary.json records no boolean passed")
    violations = diag.get("violations")
    if not isinstance(violations, list):
        raise RunError(f"{run_dir}: diag/summary.json has no violations list")
    urls = diag.get("urls")
    if not isinstance(urls, dict) or not urls:
        raise RunError(f"{run_dir}: diag/summary.json diagnosed no urls")
    max_load = {}
    for url, data in urls.items():
        value = data.get("max_load_ms") if isinstance(data, dict) else None
        # A passing diag timed a load event on every rep; null here is a
        # summary the index cannot quote a load from.
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
            raise RunError(f"{run_dir}: diag/summary.json max_load_ms for {url} is {value!r}")
        max_load[url] = value
    # A diag over other pages says nothing about the pages this run measured,
    # and a cell that names no url gives the comparison nothing to hold the
    # diag to.
    if not measured_urls:
        raise RunError(
            f"{run_dir}: analysis.json cell names no url, so diag/summary.json "
            "cannot be checked against what the run measured"
        )
    missing = [url for url in measured_urls if url not in urls]
    if missing:
        raise RunError(
            f"{run_dir}: diag/summary.json did not diagnose {missing}, which the run measured"
        )
    if passed is not True or violations:
        kinds = sorted({v.get("kind") for v in violations if isinstance(v, dict)})
        raise RunError(
            f"{run_dir}: diag/summary.json records passed={passed} with "
            f"{len(violations)} violation(s) {kinds}; a run whose diag failed is not indexed"
        )
    check_diag_limits(run_dir, diag, measured_urls, addresses, stall_max_ms)
    return {"diag_passed": True, "violations_count": 0, "max_load_ms": max_load}


def check_diag_limits(run_dir, diag, measured_urls, addresses, stall_max_ms):
    """Refuse a passing diag whose limits were not armed for this run.

    reqbench.sh diag writes passed=true when nothing it was asked to check
    went wrong, and records what it was asked under limits: expect_ips (the
    DIAG_EXPECT_IPS list, null when unset, always null on WebKit whose
    render carries no trace) and max_load_ms (DIAG_MAX_LOAD_MS, null when
    unset). A diag run without them allowed every remote address and held
    no load event to a limit, and a later standalone diag over the same
    RESULTS replaces the campaign's summary with exactly that shape. So the
    address set must be the one the run's records name, the load limit a
    positive integer no larger than the run's own stall gate, and every
    measured URL rendered reps times.
    """
    limits = diag.get("limits")
    if (
        not isinstance(limits, dict)
        or "expect_ips" not in limits
        or "max_load_ms" not in limits
    ):
        raise RunError(
            f"{run_dir}: diag/summary.json records no limits (expect_ips, max_load_ms); "
            "a diag whose limits are unknown is not this run's evidence"
        )
    expect_ips = limits["expect_ips"]
    if diag["engine"] == "webkit":
        if expect_ips is not None:
            raise RunError(
                f"{run_dir}: diag/summary.json records limits.expect_ips={expect_ips!r} "
                "on webkit, whose render carries no trace to hold to an address; "
                "reqbench refuses that expectation, so the diag did not write this"
            )
    else:
        if (
            not isinstance(expect_ips, list)
            or not expect_ips
            or any(canonical_ip(ip) is None for ip in expect_ips)
        ):
            raise RunError(
                f"{run_dir}: diag/summary.json records limits.expect_ips={expect_ips!r}; "
                "a diag run without DIAG_EXPECT_IPS allowed every remote address"
            )
        held = {canonical_ip(ip) for ip in expect_ips}
        if not addresses:
            raise RunError(
                f"{run_dir}: the run's records name no address its pages came from "
                "(no verify bracket answer, no BENCH_RESOLVE_ALL_TO, no IP-literal url "
                f"host), so diag/summary.json limits.expect_ips={sorted(held)} cannot "
                "be held to the run"
            )
        if held != addresses:
            raise RunError(
                f"{run_dir}: diag/summary.json limits.expect_ips={sorted(held)} is not "
                f"the address set the run's records name {sorted(addresses)}; that diag "
                "held its renders to other addresses"
            )
    max_load_ms = limits["max_load_ms"]
    if not positive_int(max_load_ms):
        raise RunError(
            f"{run_dir}: diag/summary.json records limits.max_load_ms={max_load_ms!r}; "
            "a diag run without DIAG_MAX_LOAD_MS held no load event to a limit"
        )
    if max_load_ms > stall_max_ms:
        raise RunError(
            f"{run_dir}: diag/summary.json limits.max_load_ms={max_load_ms} is above "
            f"the run's stall_gate max_ms={stall_max_ms}; the diag was allowed more "
            "than the measured run"
        )
    reps = diag.get("reps")
    if not positive_int(reps):
        raise RunError(f"{run_dir}: diag/summary.json records reps={reps!r}")
    for url in measured_urls:
        data = diag["urls"][url]
        if data.get("reps") != reps or data.get("renders_ok") != reps:
            raise RunError(
                f"{run_dir}: diag/summary.json records renders_ok={data.get('renders_ok')!r} "
                f"of reps={data.get('reps')!r} for {url}, not {reps} of {reps}"
            )


def load_cell(run_dir, sources=None):
    """Read one run directory into an index cell. Returns (cell, source entries)."""
    if sources is None:
        sources = Sources(run_dir)
    marker = os.path.join(run_dir, WITHDRAWN_MARKER)
    if os.path.lexists(marker):
        raise RunError(f"{run_dir}: withdrawn: {withdrawal_reason(marker)}")
    analysis_path = os.path.join(run_dir, "analysis.json")
    if not os.path.isfile(analysis_path):
        raise RunError(f"{run_dir}: analysis.json is missing")
    analysis = sources.read_json(analysis_path)
    if not isinstance(analysis, dict):
        raise RunError(f"{sources.display_path(analysis_path)}: not a JSON object")
    run_id = analysis.get("run_id")
    if not isinstance(run_id, str) or re.fullmatch(r"[0-9a-f]{32}", run_id) is None:
        raise RunError(
            f"{run_dir}: analysis.json records invalid run_id={run_id!r}; "
            "reqbench runs use 32 lowercase hexadecimal characters"
        )

    if analysis.get("withdrawn", False) is not False:
        raise RunError(
            f"{run_dir}: withdrawn: analysis.json records "
            f"withdrawn={analysis.get('withdrawn')!r}"
        )
    # reqanalyze writes publishable, gate.passed and gate.reasons from one
    # value (`publishable = not overall_reasons`), so the three always agree.
    # Reading publishable alone let an analysis.json claiming publishable=true
    # over a failed gate be indexed on the claim, and reading the gate only to
    # quote a refusal's reasons raised AttributeError on a truthy non-object
    # gate: no refusal, no exit 5, and any stale index left in place. The gate
    # is read first, and a shape reqanalyze never writes is a refusal.
    gate = analysis.get("gate")
    if not isinstance(gate, dict):
        raise RunError(
            f"{run_dir}: analysis.json has no gate object (gate={gate!r}); "
            "re-run reqanalyze"
        )
    passed = gate.get("passed")
    reasons = gate.get("reasons")
    if not isinstance(passed, bool) or not isinstance(reasons, list):
        raise RunError(
            f"{run_dir}: analysis.json gate records passed={passed!r} and "
            f"reasons={reasons!r}; re-run reqanalyze"
        )
    publishable = analysis.get("publishable")
    if not isinstance(publishable, bool) or publishable != passed:
        raise RunError(
            f"{run_dir}: analysis.json records publishable={publishable!r} over a "
            f"gate that records passed={passed!r}: {reasons}"
        )
    if not publishable:
        raise RunError(f"{run_dir}: analysis.json is not publishable: {reasons}")
    if reasons:
        raise RunError(
            f"{run_dir}: analysis.json passed its gate carrying reasons: {reasons}"
        )
    cell = analysis.get("cell")
    if not isinstance(cell, dict):
        raise RunError(
            f"{run_dir}: analysis.json has no top-level cell; the index takes one "
            "run per cell, not a pooled multi-backend analysis"
        )
    for field in ("engine", "cpu", "memory_mib", "guest_dns"):
        if field not in cell:
            raise RunError(
                f"{run_dir}: analysis.json cell has no {field}; re-run reqanalyze"
            )
    seal = {}
    for field in SEAL_FIELDS:
        value = cell.get(field)
        if not isinstance(value, str) or not value.strip():
            raise RunError(
                f"{run_dir}: analysis.json cell has no {field}; a run without its "
                f"seal identity ({', '.join(SEAL_FIELDS)}) is not a sealed run"
            )
        seal[field] = value
    stall_gate = analysis.get("stall_gate")
    if not isinstance(stall_gate, dict) or not isinstance(stall_gate.get("passed"), bool):
        raise RunError(
            f"{run_dir}: analysis.json has no stall_gate verdict; re-run reqanalyze"
        )
    max_ms = stall_gate.get("max_ms")
    if (
        isinstance(max_ms, bool)
        or not isinstance(max_ms, (int, float))
        or not math.isfinite(max_ms)
        or max_ms <= 0
    ):
        # reqanalyze without --stall-max-ms writes passed=true, evaluated=0:
        # a gate that evaluated nothing has no pass to report.
        raise RunError(
            f"{run_dir}: stall_gate was not armed (max_ms is {max_ms!r}); "
            "re-run reqanalyze --stall-max-ms N"
        )
    if not positive_int(stall_gate.get("evaluated")):
        raise RunError(
            f"{run_dir}: stall_gate evaluated {stall_gate.get('evaluated')!r} "
            "record(s); a pass over nothing is not a pass"
        )
    if stall_gate["passed"] is not True:
        raise RunError(
            f"{run_dir}: stall_gate failed: {stall_gate.get('violations')}"
        )

    measured = [part.strip() for part in str(cell.get("url") or "").split(",") if part.strip()]
    dns_verdict = None
    load_max_1min = None
    answers = set()
    owner_load_evidence = None
    evidence_path = os.path.join(run_dir, "dns-evidence.json")
    if os.path.isfile(evidence_path):
        evidence = sources.read_json(evidence_path)
        answers, owner_load_evidence = check_evidence(
            run_dir, evidence, sources, cell["guest_dns"], measured,
            run_id,
        )
        dns_verdict = evidence["verdict"]
        # Reported, not gated: the run driver refused a busy box at the
        # start, and evidence from before the sampler carried the load
        # column has no value to copy.
        load_max_1min = evidence.get("load_max_1min")
    elif cell["guest_dns"] is not None:
        # A guest that resolved through a baked resolver is a campaign run;
        # only the bracket evidence says the resolver held for the whole
        # measured run.
        raise RunError(
            f"{run_dir}: guest_dns is {cell['guest_dns']!r} but there is no "
            "dns-evidence.json; a resolver run without its brackets is not indexed"
        )

    # The container environment the golden baked (reqbench.sh GUEST_ENV):
    # the resolver-rule arm carries BENCH_RESOLVE_ALL_TO here and resolves
    # nothing through resolv.conf, so its guest_dns is null.
    guest_env = cell.get("guest_env", [])
    if not isinstance(guest_env, list) or not all(
        isinstance(entry, str) and "=" in entry for entry in guest_env
    ):
        raise RunError(f"{run_dir}: analysis.json cell guest_env is not a list of KEY=VALUE")

    diag = None
    diag_path = os.path.join(run_dir, "diag", "summary.json")
    if os.path.isfile(diag_path):
        addresses = recorded_addresses(run_dir, measured, guest_env, answers)
        diag = summarize_diag(
            run_dir, sources.read_json(diag_path), cell, measured, addresses, max_ms
        )
    elif cell["guest_dns"] is not None or guest_env:
        # The campaign diagnoses the golden before measuring on it; a run
        # whose golden was shaped for the corpus (a baked resolver, or a
        # baked environment such as the resolver rule) without the summary
        # is a run nobody diagnosed.
        shaped = (
            f"guest_dns is {cell['guest_dns']!r}" if cell["guest_dns"] is not None
            else f"guest_env is {guest_env!r}"
        )
        raise RunError(
            f"{run_dir}: {shaped} but there is no "
            "diag/summary.json; a corpus run without its diag is not indexed"
        )

    # A measured URL whose host is a name was answered by some resolver. The
    # record names one in two places: guest_dns (baked into resolv.conf, held
    # by the dns-evidence brackets) or a BENCH_RESOLVE_ALL_TO rule in
    # guest_env. A run with neither resolved its corpus through whatever
    # owned port 53, and the record cannot say what that was. Every
    # results/reqbench-20260816-*-corpus record has this shape: pasta
    # redirected the guest's port 53 to the host's own resolver (fixed in
    # fcvm 90733b854e), and the index took them with dns_verdict null.
    # reqanalyze refuses the shape at analysis time (_validate_resolver); a
    # legacy or hand-edited analysis.json is refused here for the same reason.
    # This is defense in depth for a record that carries an engine but no
    # resolver: every committed 2026-08-16 record is already refused by the
    # engine check above, so on those this rule never fires.
    #
    # Only entry.sh reads BENCH_RESOLVE_ALL_TO and only Chromium takes the
    # flag it builds (reqanalyze._resolver_rule_address refuses the rule
    # under any other engine), so a rule in a WebKit cell's guest_env names
    # nothing about what answered that run's hostnames.
    rule = any(entry.partition("=")[0] == "BENCH_RESOLVE_ALL_TO" for entry in guest_env)
    rule_recorded = rule and cell["engine"] == "chromium"
    if dns_verdict is None and cell["guest_dns"] is None and not rule_recorded:
        unresolved = [
            url for url in measured if reqanalyze.url_needs_resolver(url) is not False
        ]
        if unresolved:
            rule_note = (
                f"a BENCH_RESOLVE_ALL_TO rule under engine {cell['engine']!r}, "
                "which never reads it"
                if rule else "no BENCH_RESOLVE_ALL_TO"
            )
            raise RunError(
                f"{run_dir}: measured hostname URL(s) {unresolved[:3]} with no "
                f"recorded resolver (guest_dns null, {rule_note}, no "
                "dns-evidence.json); the corpus resolved through ambient DNS"
            )

    arms = analysis.get("arms")
    if not isinstance(arms, dict) or not arms:
        raise RunError(f"{run_dir}: analysis.json has no arms")
    headline = {}
    for arm, data in arms.items():
        blocking = data.get("blocking_ms") if isinstance(data, dict) else None
        median = blocking.get("median") if isinstance(blocking, dict) else None
        if isinstance(median, bool) or not isinstance(median, (int, float)):
            raise RunError(f"{run_dir}: arm {arm} has no blocking_ms median")
        headline[arm] = {
            "blocking_ms": median,
            "blocking_ms_ci": [blocking.get("lo"), blocking.get("hi")],
            "n": blocking.get("n"),
        }
    request_loads = request_load_evidence(run_dir, analysis, arms, sources)

    return {
        "run_dir": str(run_dir),
        "run_id": run_id,
        "engine": cell["engine"],
        "cpu": cell["cpu"],
        "memory_mib": cell["memory_mib"],
        "guest_dns": cell["guest_dns"],
        "guest_env": guest_env,
        "backend": cell.get("backend"),
        "uffd_mode": cell.get("uffd_mode"),
        "seal": seal,
        "publishable": True,
        "stall_gate_passed": True,
        "dns_verdict": dns_verdict,
        "load_max_1min": load_max_1min,
        "load_evidence": {
            "continuous_owner_log": owner_load_evidence,
            "measured_requests": request_loads,
        },
        "headline": headline,
        "diag": diag,
    }, sources.entries


def run_identity(run_dir):
    """The keys that name one run directory, so a second name for a run
    already listed can be recognised: its canonical path, and the directory
    identity the filesystem reports. A symlink alias shares the canonical
    path; a bind mount shares only (st_dev, st_ino). A path that cannot be
    stat'ed carries the first key alone and is refused by load_cell with its
    own message."""
    keys = [("path", os.path.realpath(run_dir))]
    try:
        stat = os.stat(run_dir)
    except OSError:
        return keys
    keys.append(("dir", (stat.st_dev, stat.st_ino)))
    return keys


def paths_alias(left, right):
    """Whether two paths name, or would name, the same filesystem object."""
    if os.path.realpath(left) == os.path.realpath(right):
        return True
    try:
        return os.path.samefile(left, right)
    except OSError:
        return False


def existing_path_identities(paths):
    """Snapshot entry and target inodes that cleanup must never remove."""
    identities = []
    seen = set()
    for path in paths:
        for follow_symlinks in (False, True):
            try:
                identity = os.stat(path, follow_symlinks=follow_symlinks)
            except OSError:
                continue
            key = (identity.st_dev, identity.st_ino)
            if key not in seen:
                seen.add(key)
                identities.append(identity)
    return identities


def withdrawal_errors(run_dirs):
    """Re-read every run's withdrawal marker at a publication boundary."""
    errors = []
    for run_dir in run_dirs:
        marker = os.path.join(run_dir, WITHDRAWN_MARKER)
        if os.path.lexists(marker):
            errors.append(f"{run_dir}: withdrawn: {withdrawal_reason(marker)}")
    return errors


class LockedRunDirectory:
    """One caller pathname read through the directory descriptor we locked."""

    def __init__(self, path, descriptor, identity):
        self.path = path
        self.access_path = f"/proc/self/fd/{descriptor}"
        self.identity = identity

    def __fspath__(self):
        return self.access_path

    def __str__(self):
        return self.path

    def __repr__(self):
        return repr(self.path)

    def __eq__(self, other):
        return str(self) == str(other)

    def __hash__(self):
        return hash(str(self))


class RunDirectoryLockErrors(list):
    """Lock failures plus the fd-pinned paths acquired successfully."""

    def __init__(self):
        super().__init__()
        self.run_dirs = []


def locked_run_directory_errors(run_dirs):
    """Refuse when a caller pathname no longer names its locked inode."""
    errors = []
    for run_dir in run_dirs:
        if not isinstance(run_dir, LockedRunDirectory):
            continue
        try:
            current = os.stat(run_dir.path)
        except OSError as error:
            errors.append(
                f"{run_dir}: changed after its directory lock was acquired: {error}"
            )
            continue
        identity = (current.st_dev, current.st_ino)
        if identity != run_dir.identity:
            errors.append(
                f"{run_dir}: changed after its directory lock was acquired"
            )
    return errors


@contextmanager
def shared_run_directory_locks(run_dirs):
    """Hold the reader side of the WITHDRAWN publication protocol.

    A withdrawal writer takes an exclusive flock on the run directory before
    publishing WITHDRAWN. Holding shared locks from the first input read until
    the publication decision returns gives the two operations one ordering:
    either the marker exists before validation, or it is published after this
    index and invalidates it. The inode key avoids opening a second lock
    description for a directory supplied through an alias.

    Lock acquisition failures are returned as publication errors. The caller
    still runs ordinary validation so a missing or malformed run retains its
    more specific refusal too.
    """
    descriptors = []
    identities = {}
    errors = RunDirectoryLockErrors()
    try:
        for run_dir in run_dirs:
            try:
                descriptor = os.open(
                    run_dir, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
                )
            except OSError as error:
                errors.append(f"{run_dir}: cannot open run directory lock: {error}")
                errors.run_dirs.append(run_dir)
                continue
            try:
                stat = os.fstat(descriptor)
            except OSError as error:
                os.close(descriptor)
                errors.append(f"{run_dir}: cannot identify run directory: {error}")
                errors.run_dirs.append(run_dir)
                continue
            identity = (stat.st_dev, stat.st_ino)
            if identity in identities:
                os.close(descriptor)
                errors.run_dirs.append(
                    LockedRunDirectory(run_dir, identities[identity], identity)
                )
                continue
            try:
                fcntl.flock(descriptor, fcntl.LOCK_SH)
            except OSError as error:
                os.close(descriptor)
                errors.append(f"{run_dir}: cannot lock run directory: {error}")
                errors.run_dirs.append(run_dir)
                continue
            identities[identity] = descriptor
            descriptors.append(descriptor)
            errors.run_dirs.append(
                LockedRunDirectory(run_dir, descriptor, identity)
            )
        yield errors
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)


def build_index(run_dirs):
    """Every cell or a list of refusals; never a partial index."""
    cells = []
    generated_from = []
    source_attempted_paths = []
    errors = []
    seen = {}
    for run_dir in run_dirs:
        keys = run_identity(run_dir)
        first = next((seen[key] for key in keys if key in seen), None)
        if first is not None:
            # Refused, not deduped: one run reached under two names is an
            # argument list the caller did not mean, and an index that
            # silently dropped the second would be one experiment counted
            # twice or a cell nobody notices is missing.
            if first == run_dir:
                errors.append(f"{run_dir}: listed more than once")
            else:
                errors.append(
                    f"{run_dir}: is the run directory {first} under a second name"
                )
            continue
        for key in keys:
            seen[key] = run_dir
        sources = Sources(run_dir)
        for relative_path in RUN_INPUT_RELATIVE_PATHS:
            sources.protect(os.path.join(run_dir, relative_path))
        try:
            cell, _ = load_cell(run_dir, sources)
        except RunError as error:
            errors.append(str(error))
        else:
            cells.append(cell)
        generated_from.extend(sources.entries)
        source_attempted_paths.extend(sources.entries.attempted_paths)
    return ({"generated_from": generated_from, "cells": cells}, errors,
            source_attempted_paths)


def publish_index(args, run_dirs, lock_errors, output):
    """Validate and publish while shared run-directory locks remain held."""
    validation_dirs = getattr(lock_errors, "run_dirs", run_dirs)
    index, errors, source_attempted_paths = build_index(validation_dirs)
    errors = list(lock_errors) + errors
    aliases_input = False
    protected_paths = set()
    for display_path, pinned_path in source_attempted_paths:
        protected_paths.update((display_path, pinned_path))
        if any(
                paths_alias(output_path, protected_path)
                for output_path in (args.out, output.access_path)
                for protected_path in (display_path, pinned_path)):
            errors.append(f"--out {args.out} aliases input {display_path}")
            aliases_input = True
    withdrawal_markers = {
        os.path.join(run_dir, WITHDRAWN_MARKER) for run_dir in run_dirs
    }
    withdrawal_markers.update(
        os.path.join(os.fspath(run_dir), WITHDRAWN_MARKER)
        for run_dir in validation_dirs
    )
    protected_paths.update(withdrawal_markers)
    protected_identities = existing_path_identities(protected_paths)
    aliases_withdrawal = any(
        paths_alias(output_path, marker)
        for output_path in (args.out, output.access_path)
        for marker in withdrawal_markers
    )
    if aliases_withdrawal:
        errors.append(f"--out {args.out} aliases a run's {WITHDRAWN_MARKER} marker")
    errors.extend(withdrawal_errors(validation_dirs))
    errors.extend(locked_run_directory_errors(validation_dirs))
    output_error = output.validation_error()
    if output_error is not None:
        errors.append(output_error)
    if errors:
        print("REFUSED: no index written", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        # An index already at --out describes cells this refusal did not
        # accept; it must not outlive the refusal. Never when --out is one
        # of the inputs, which are only ever read.
        if (not aliases_input and not aliases_withdrawal
                and output.unlink_if_unchanged(
                    output.initial_identity, protected_identities)):
            print(f"  removed stale index {args.out}", file=sys.stderr)
        return 5
    publication = write_json_atomic(output.access_path, index)
    try:
        output_error = output.validation_error(publication)
        if output_error is not None:
            print("REFUSED: no index written", file=sys.stderr)
            print(f"  - {output_error}", file=sys.stderr)
            if output.unlink_if_unchanged(
                    publication.identity, protected_identities):
                print(f"  removed stale index {args.out}", file=sys.stderr)
            return 5
        errors = withdrawal_errors(validation_dirs)
        output_error = output.validation_error(publication)
        if output_error is not None:
            errors.append(output_error)
        errors.extend(locked_run_directory_errors(validation_dirs))
        if errors:
            print("REFUSED: no index written", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
            if output.unlink_if_unchanged(
                    publication.identity, protected_identities):
                print(f"  removed stale index {args.out}", file=sys.stderr)
            return 5
        print(f"wrote {args.out}: {len(index['cells'])} cell(s)")
        return 0
    finally:
        publication.close()


def main_with(argv=None):
    parser = argparse.ArgumentParser(
        description="Index the publishable cells of one campaign into one JSON file.",
    )
    parser.add_argument("--out", required=True, help="index path to write")
    parser.add_argument("run_dir", nargs="+", help="run directories holding analysis.json")
    args = parser.parse_args(argv)

    run_dirs = [os.path.normpath(run_dir) for run_dir in args.run_dir]
    with shared_run_directory_locks(run_dirs) as lock_errors:
        with pinned_output(args.out) as output:
            return publish_index(args, run_dirs, lock_errors, output)


if __name__ == "__main__":
    sys.exit(main_with())
