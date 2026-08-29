#!/usr/bin/env python3
"""Index the cells of one campaign into a single JSON file.

    campaign_summary.py --out PATH <run_dir>...

Each run directory holds reqanalyze's analysis.json (required; its stall_gate
must have been armed with --stall-max-ms and must have evaluated at least one
record, since an unarmed gate reports passed=true having evaluated nothing),
dns-evidence.json (required when the cell's guest_dns names a baked resolver,
optional otherwise; when present its verdict must be "clean", it must carry
the replay server's exit status 0, every file it cites must be present and
agree with the sha256 it recorded, each verify bracket must record the run's
resolver in both captured /etc/resolv.conf views and nowhere else, with the
URL probes run under no proxy and every host and URL answered through it,
and every sample in dns-owner.log must name its
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
during the measured run, when the evidence records it), the headline median
blocking_ms per arm with its CI, and for the diag its verdict, violation
count and slowest load event per URL.

The publication rule (REVIEW.md) is to quote only from sealed runs that passed
their gates and were never withdrawn, and publishable=true alone proves none
of that. The seal is the cell's identity, SEAL_FIELDS below: the runtime
bundle reqbench.sh sealed, the fcvm binary and harness sources hashed into
it, the source revision, and the image and snapshot generation measured. A
cell missing any of them is not a sealed run. A run is withdrawn by a file
named WITHDRAWN in its directory whose first line is the reason, or by
"withdrawn": true in its analysis.json; either refuses the run and the
refusal quotes the reason.

The index is written only when every run is sealed, publishable and not
withdrawn, every stall gate passed, every DNS verdict is clean and every diag
passed. Otherwise nothing is written, an index already at --out is removed,
and the exit status is 5, the same code reqanalyze uses for a refused run: an
index that quietly carried an unpublishable cell would be quoted by someone
who only opened the index. Inputs are only ever read, and each is read once
so the hash names the bytes that were parsed.
"""

import argparse
import hashlib
import ipaddress
import json
import math
import os
import re
import sys
import tempfile
from urllib.parse import urlsplit

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
# One resolver line of a captured /etc/resolv.conf. resolv.conf(5) allows
# leading whitespace and ignores anything after the address; `#` and `;`
# start a comment, so a commented-out nameserver establishes nothing.
NAMESERVER_LINE = re.compile(r"^[ \t]*nameserver[ \t]+(\S+)", re.MULTILINE)
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
REPLAY_LOGS = {
    "corpus_dns_log_sha256": "corpus-dns.log",
    "corpus_access_log_sha256": "corpus-access.log",
}


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


class Sources:
    """The files one cell was read from, each read once and hashed from those bytes."""

    def __init__(self):
        self.entries = []

    def read_json(self, path):
        try:
            data = read_bytes(path)
        except OSError as error:
            raise RunError(f"{path}: cannot read: {error}")
        self.entries.append({"path": path, "sha256": hashlib.sha256(data).hexdigest()})
        return parse_json(data, path)

    def read_hashed(self, path):
        """Record a file the index does not parse; returns its sha256."""
        try:
            data = read_bytes(path)
        except OSError as error:
            raise RunError(f"{path}: cannot read: {error}")
        digest = hashlib.sha256(data).hexdigest()
        self.entries.append({"path": path, "sha256": digest})
        return data, digest


def write_json_atomic(path, value):
    directory = os.path.dirname(os.path.abspath(path))
    fd, temp_path = tempfile.mkstemp(prefix=".campaign-summary.", dir=directory)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(value, handle, indent=2, sort_keys=False)
            handle.write("\n")
        os.replace(temp_path, path)
    except BaseException:
        os.unlink(temp_path)
        raise


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
            loads.append(float(match["load"]))
    load_samples = evidence.get("load_samples", 0)
    load_max = evidence.get("load_max_1min")
    if isinstance(load_samples, bool) or not isinstance(load_samples, int):
        raise RunError(f"{run_dir}: dns-evidence.json records load_samples={load_samples!r}")
    if isinstance(load_max, bool) or not (load_max is None or isinstance(load_max, (int, float))):
        raise RunError(f"{run_dir}: dns-evidence.json records load_max_1min={load_max!r}")
    found_max = max(loads) if loads else None
    if len(loads) != load_samples or found_max != load_max:
        raise RunError(
            f"{run_dir}: dns-owner.log carries {len(loads)} load sample(s) with maximum "
            f"{found_max} but dns-evidence.json records load_samples={load_samples}, "
            f"load_max_1min={load_max}"
        )


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


def check_verify_bracket(run_dir, name, verify, resolver):
    """Hold one HOP D bracket to what the campaign asserted when it ran it:
    the clone resolved through `resolver` (None takes the bracket's own, so
    the remaining brackets are held to the first), both captured
    /etc/resolv.conf views named that resolver and no other, the URL probes
    ran with no proxy, and every corpus host and URL answered through it.
    Returns the resolver the bracket names.

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


def check_evidence(run_dir, evidence, sources, guest_dns):
    """Hold a clean verdict to the files it cites; raise RunError otherwise.

    Returns the set of addresses the verify brackets' resolver answered,
    the run's own record of where its pages came from.
    """
    if not isinstance(evidence, dict):
        # Valid JSON that is not an object ([] for one) has no verdict to
        # read; refusing it here keeps every .get() below on a dict.
        raise RunError(f"{run_dir}: dns-evidence.json is not a JSON object")
    verdict = evidence.get("verdict")
    if verdict != "clean":
        raise RunError(f"{run_dir}: dns-evidence.json verdict is {verdict!r}, not 'clean'")
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
    check_owner_log(run_dir, evidence, owner_bytes)
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
    resolver = guest_dns if isinstance(guest_dns, str) and guest_dns else None
    answers = set()
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
        verify = parse_json(data, path)
        resolver = check_verify_bracket(run_dir, name, verify, resolver)
        answers |= bracket_answers(run_dir, name, verify)
    # The replay server's own logs, pinned by hash at the verdict.
    for field, name in REPLAY_LOGS.items():
        want = evidence.get(field)
        if not is_sha256(want):
            raise RunError(f"{run_dir}: dns-evidence.json has no sha256 for {name} ({field})")
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path):
            raise RunError(f"{run_dir}: {name} cited by dns-evidence.json is missing")
        _data, digest = sources.read_hashed(path)
        if digest != want:
            raise RunError(
                f"{run_dir}: {name} sha256 {digest} does not match the "
                f"{want} dns-evidence.json recorded at the verdict"
            )
    return answers


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


def load_cell(run_dir):
    """Read one run directory into an index cell. Returns (cell, source entries)."""
    sources = Sources()
    marker = os.path.join(run_dir, WITHDRAWN_MARKER)
    if os.path.lexists(marker):
        raise RunError(f"{run_dir}: withdrawn: {withdrawal_reason(marker)}")
    analysis_path = os.path.join(run_dir, "analysis.json")
    if not os.path.isfile(analysis_path):
        raise RunError(f"{run_dir}: analysis.json is missing")
    analysis = sources.read_json(analysis_path)
    if not isinstance(analysis, dict):
        raise RunError(f"{analysis_path}: not a JSON object")

    if analysis.get("withdrawn", False) is not False:
        raise RunError(
            f"{run_dir}: withdrawn: analysis.json records "
            f"withdrawn={analysis.get('withdrawn')!r}"
        )
    if analysis.get("publishable") is not True:
        reasons = (analysis.get("gate") or {}).get("reasons") or []
        raise RunError(f"{run_dir}: analysis.json is not publishable: {reasons}")
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

    dns_verdict = None
    load_max_1min = None
    answers = set()
    evidence_path = os.path.join(run_dir, "dns-evidence.json")
    if os.path.isfile(evidence_path):
        evidence = sources.read_json(evidence_path)
        answers = check_evidence(run_dir, evidence, sources, cell["guest_dns"])
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
        measured = [part.strip() for part in str(cell.get("url") or "").split(",") if part.strip()]
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

    return {
        "run_dir": run_dir,
        "run_id": analysis.get("run_id"),
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


def build_index(run_dirs):
    """Every cell or a list of refusals; never a partial index."""
    cells = []
    generated_from = []
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
        try:
            cell, sources = load_cell(run_dir)
        except RunError as error:
            errors.append(str(error))
            continue
        cells.append(cell)
        generated_from.extend(sources)
    return {"generated_from": generated_from, "cells": cells}, errors


def main_with(argv=None):
    parser = argparse.ArgumentParser(
        description="Index the publishable cells of one campaign into one JSON file.",
    )
    parser.add_argument("--out", required=True, help="index path to write")
    parser.add_argument("run_dir", nargs="+", help="run directories holding analysis.json")
    args = parser.parse_args(argv)

    run_dirs = [os.path.normpath(run_dir) for run_dir in args.run_dir]
    index, errors = build_index(run_dirs)
    out_realpath = os.path.realpath(args.out)
    aliases_input = False
    for entry in index["generated_from"]:
        if os.path.realpath(entry["path"]) == out_realpath:
            errors.append(f"--out {args.out} aliases input {entry['path']}")
            aliases_input = True
    if errors:
        print("REFUSED: no index written", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        # An index already at --out describes cells this refusal did not
        # accept; it must not outlive the refusal. Never when --out is one
        # of the inputs, which are only ever read.
        if not aliases_input and os.path.lexists(args.out):
            os.unlink(args.out)
            print(f"  removed stale index {args.out}", file=sys.stderr)
        return 5
    write_json_atomic(args.out, index)
    print(f"wrote {args.out}: {len(index['cells'])} cell(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main_with())
