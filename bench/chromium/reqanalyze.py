#!/usr/bin/env python3
"""Analysis for the request-optimized A/B. Medians with bootstrap CIs, drift, teardown.

Every rule here exists because of a defect listed in bench/chromium/AGENTS.md:

  * Warmups are dropped EXPLICITLY and the count is printed (defect: cold-cache
    runs inflate p50 silently).
  * Uncertainty is a bootstrap CI on the MEDIAN, and every figure is rounded to
    its own CI (defect 6: "129.0 MiB" on +/-20 MiB data is a false claim).
  * The `noop` drift-control series is split first-half/second-half and the
    difference is reported. If the control moved, the arm deltas are suspect and
    this script says so rather than letting the reader assume a quiet box
    (defect 2: a control probe drifted 631 -> 706 ms in the retracted run).
  * Teardown is never one number. blocking / reap_wall / reclaim_cpu are
    reported separately, and CPU figures flagged `complete=False` are counted as
    LOWER BOUNDS and excluded from the headline rather than averaged in.
  * AVAILABILITY IS PER ARM, and it gates publication. Every median here is
    computed over successes only, so two arms with different drop rates are
    differently censored and their difference is not a like-for-like comparison.
    Each arm therefore reports attempted/failed with an exact (Clopper-Pearson)
    binomial interval, and any arm that dropped a request is marked
    `publishable: false`. "0 failures" is not a 0% rate: 0/426 is [0, 0.86%].
    (This line said 0.70% while `clopper_pearson`'s own docstring 80 lines below
    said 0.86% for the same k/n — two point estimates of one quantity disagreeing
    inside one file, which is defect 6's cousin and exactly what the AGENTS.md
    `snapshot-load` entry was written about. The function computes 0.862%.)
  * THE LEAK CHECK RUNS OVER EVERY ATTEMPT, NOT OVER THE SUCCESSES. A request
    that failed is exactly the one whose teardown is most likely to have leaked,
    and filtering it out reported `all_gone: 27/27 confirmed` on a set containing
    three teardowns that had explicitly recorded `all_gone: false`.
  * Per-child reclaim CPU is reported PER CHILD. Pooling the children into one
    median and taking the median of {firecracker 110 ms, holder 0, pasta 0}
    publishes 0 — it medians the straggler away, which is the opposite of the
    thing being measured.
  * A URL whose host is a name needs a resolver, and the meta must name it
    (`guest_dns`). A corpus run that records no resolver had its hostnames
    answered by something the record does not say, so it is refused. IP
    literals and localhost need none. `engine` and `guest_dns` are cell
    fields: runs that differ in either never pool.
  * A load event that takes tens of seconds is a stall, not a slow page. With
    `--stall-max-ms N` every record whose navigate_load_event_ms exceeds N is
    listed under `stall_gate` and the run is refused. `per_url` reports each
    URL of a multi-URL run on its own so a pooled median cannot hide one page.
"""

import argparse
import fcntl
import hashlib
import ipaddress
import json
import math
import os
import random
import statistics
import sys
import tempfile
import uuid
from urllib.parse import urlsplit


MIN_CDP_ATTEMPTS_PER_BACKEND = 200
SUPPORTED_ENGINES = ("chromium", "webkit")


def is_cdp_class(arm):
    """Arms that pay the per-request CDP handshake: cdp, cdp-fast, and html.

    Both the sample-size gate and the stage decomposition key off this
    predicate. A name test scattered as startswith("cdp") let the html arm
    fall out of both: a mixed run could publish an html latency from fewer
    than 200 measured attempts, and analysis.json dropped every
    connection/extraction stage for it.
    """
    return isinstance(arm, str) and (arm.startswith("cdp") or arm == "html")
MIN_NOOP_ATTEMPTS = 6
DRIFT_EQUIVALENCE_MARGIN_MS = 10.0
QUIET_LOADAVG1_LIMIT = 2.0
ANALYZER_SCHEMA_VERSION = 5
SEALED_ANALYZER_FD_ENV = "REQANALYZE_SEALED_FD"
ANALYZER_SOURCE_PATH_ENV = "REQANALYZE_SOURCE_PATH"

# Fields that define one comparable benchmark population. Files may be pooled
# only when every one matches. Seed/repetition counts are deliberately excluded:
# they describe how a cell was sampled, not what was sampled.
CELL_FIELDS = (
    "backend",
    "uffd_mode",
    "arms",
    "url",
    "engine",
    "guest_dns",
    "format",
    "quality",
    "image",
    "image_id",
    "snapshot",
    "snapshot_generation_id",
    "snapshot_config_sha256",
    "snapshot_created_at",
    "snapshot_vm_id",
    "fcvm_sha256",
    "harness_sha256",
    "runtime_bundle_sha256",
    "source_revision",
    "cdp_port",
    "port_mappings",
    "network_mode",
    "cpu",
    "memory_mib",
    "rust_log",
    "ws_url_prewired",
    "allow_busy",
    "quiet_loadavg1_limit",
    "host_boot_id",
    "host_kernel_release",
    "host_machine",
)


def median_ci(xs, iters=20000, conf=0.95, seed=12345):
    """Median with a percentile-bootstrap CI. Returns (median, lo, hi, n)."""
    xs = [float(x) for x in xs if x is not None and not math.isnan(float(x))]
    n = len(xs)
    if n == 0:
        return None, None, None, 0
    med = statistics.median(xs)
    if n < 3:
        return med, min(xs), max(xs), n
    rng = random.Random(seed)
    boots = []
    for _ in range(iters):
        boots.append(statistics.median([xs[rng.randrange(n)] for _ in range(n)]))
    boots.sort()
    lo = boots[int((1 - conf) / 2 * iters)]
    hi = boots[int((1 + conf) / 2 * iters) - 1]
    return med, lo, hi, n


def fmt(med, lo, hi, n=None, unit="ms"):
    """Round the estimate to the precision its own CI can support (defect 6)."""
    if med is None:
        return "n/a"
    half = max(abs(hi - med), abs(med - lo)) if lo is not None else 0.0
    digits = 0 if half >= 5 else (1 if half >= 0.5 else 2)
    s = f"{med:.{digits}f} [{lo:.{digits}f}, {hi:.{digits}f}] {unit}"
    return s + (f" n={n}" if n else "")


def _log_binom_pmf(i: int, n: int, p: float) -> float:
    if p <= 0.0:
        return 0.0 if i == 0 else -math.inf
    if p >= 1.0:
        return 0.0 if i == n else -math.inf
    return (
        math.lgamma(n + 1) - math.lgamma(i + 1) - math.lgamma(n - i + 1)
        + i * math.log(p) + (n - i) * math.log1p(-p)
    )


def _binom_cdf(k: int, n: int, p: float) -> float:
    """P(X <= k) for X ~ Binomial(n, p). Log-space terms, so no overflow at large n."""
    if k < 0:
        return 0.0
    if k >= n:
        return 1.0
    return sum(math.exp(_log_binom_pmf(i, n, p)) for i in range(k + 1))


def _bisect(f, lo: float, hi: float, iters: int = 80) -> float:
    for _ in range(iters):
        mid = (lo + hi) / 2
        if f(mid) > 0:
            hi = mid
        else:
            lo = mid
    return (lo + hi) / 2


def clopper_pearson(k: int, n: int, conf: float = 0.95):
    """EXACT binomial CI for k events in n trials, as fractions.

    Used for failure rates. "0 failures in 426" is not a 0% failure rate — its
    two-sided 95% upper bound is 0.86%, and quoting the bare 0 reads as a
    guarantee the data does not carry (AGENTS.md defect 6, applied to counts).
    No scipy in this repo, so the tails are summed in log space and inverted by
    bisection rather than pulled from a beta quantile.
    """
    if n <= 0:
        return 0.0, 1.0
    a = (1 - conf) / 2
    lo = 0.0 if k == 0 else _bisect(lambda p: 1 - _binom_cdf(k - 1, n, p) - a, 0.0, k / n)
    hi = 1.0 if k == n else _bisect(lambda p: a - _binom_cdf(k, n, p), k / n, 1.0)
    return lo, hi


def declared_urls(meta):
    """The URL set a meta declares: `urls` when present, else `url` split on commas."""
    urls = meta.get("urls")
    if (
        isinstance(urls, list)
        and urls
        and all(isinstance(url, str) and url.strip() for url in urls)
    ):
        return [url.strip() for url in urls]
    url = meta.get("url")
    if isinstance(url, str):
        return [part.strip() for part in url.split(",") if part.strip()]
    return []


def url_needs_resolver(url):
    """Whether a URL's host is a name only a resolver can answer.

    False for an IP literal or localhost, None when the URL has no host.
    """
    try:
        host = urlsplit(url).hostname
    except ValueError:
        return None
    if not host:
        return None
    if host == "localhost":
        return False
    try:
        ipaddress.ip_address(host)
    except ValueError:
        return True
    return False


def record_url(record):
    """The URL a record rendered: its own stamp, else the render's."""
    url = record.get("url")
    if isinstance(url, str) and url:
        return url
    render = record.get("render")
    if isinstance(render, dict):
        url = render.get("url")
        if isinstance(url, str) and url:
            return url
    return None


def render_stage(record, key):
    """One finite render.stages timing, or None when the record lacks it."""
    render = record.get("render")
    if not isinstance(render, dict):
        return None
    stages = render.get("stages")
    if not isinstance(stages, dict):
        return None
    value = stages.get(key)
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
    ):
        return None
    return value


def _cell_from_meta(meta, source):
    errors = []
    cell = {}
    string_fields = {
        "backend", "uffd_mode", "url", "format", "image", "image_id", "snapshot",
        "snapshot_generation_id", "snapshot_config_sha256", "snapshot_created_at",
        "snapshot_vm_id", "fcvm_sha256", "harness_sha256",
        "runtime_bundle_sha256", "source_revision", "network_mode", "rust_log", "host_boot_id",
        "host_kernel_release", "host_machine",
    }
    positive_int_fields = {"quality", "cdp_port", "cpu", "memory_mib"}
    positive_number_fields = {"quiet_loadavg1_limit"}
    for field in CELL_FIELDS:
        value = meta.get(field)
        if field == "arms":
            valid = (
                isinstance(value, list)
                and bool(value)
                and all(isinstance(arm, str) and arm for arm in value)
                and len(value) == len(set(value))
            )
        elif field == "port_mappings":
            def valid_host_ip(mapping):
                host_ip = mapping.get("host_ip")
                if host_ip is None:
                    return True
                if not isinstance(host_ip, str):
                    return False
                try:
                    return str(ipaddress.ip_address(host_ip)) == host_ip
                except ValueError:
                    return False

            valid = (
                isinstance(value, list)
                and bool(value)
                and all(
                    isinstance(mapping, dict)
                    and set(mapping) == {
                        "host_ip", "host_port", "guest_port", "proto"
                    }
                    and valid_host_ip(mapping)
                    and isinstance(mapping.get("host_port"), int)
                    and not isinstance(mapping.get("host_port"), bool)
                    and isinstance(mapping.get("guest_port"), int)
                    and not isinstance(mapping.get("guest_port"), bool)
                    and 1 <= mapping.get("host_port") <= 65535
                    and 1 <= mapping.get("guest_port") <= 65535
                    and mapping.get("proto") in ("tcp", "udp")
                    for mapping in value
                )
            )
        elif field in ("ws_url_prewired", "allow_busy"):
            valid = isinstance(value, bool)
        elif field == "engine":
            # The harness stamps the engine once per run; an absent stamp is
            # chromium, the same reading _validate_schedule gives it.
            value = meta.get("engine") or "chromium"
            valid = value in SUPPORTED_ENGINES
        elif field == "guest_dns":
            # null: the guest used the host resolvers (fixture runs). A string
            # must be the canonical IP literal fcvm baked into resolv.conf.
            if value is None:
                valid = True
            else:
                try:
                    valid = (
                        isinstance(value, str)
                        and str(ipaddress.ip_address(value)) == value
                    )
                except ValueError:
                    valid = False
        else:
            valid = (
                isinstance(value, str) and bool(value.strip())
                if field in string_fields
                else isinstance(value, int) and not isinstance(value, bool) and value > 0
                if field in positive_int_fields
                else isinstance(value, (int, float))
                and not isinstance(value, bool)
                and math.isfinite(value)
                and value > 0
                if field in positive_number_fields
                else False
            )
        if not valid:
            errors.append(f"{source} metadata has no valid {field}")
        else:
            cell[field] = (
                sorted(value) if field == "arms"
                else sorted(value, key=lambda mapping: json.dumps(mapping, sort_keys=True))
                if field == "port_mappings"
                else value.strip() if isinstance(value, str)
                else value
            )
    if "url" in cell:
        declared = meta.get("urls")
        if declared is not None:
            split = [part.strip() for part in cell["url"].split(",") if part.strip()]
            if not (
                isinstance(declared, list)
                and declared
                and all(isinstance(url, str) and url.strip() for url in declared)
            ):
                errors.append(f"{source} metadata urls is not a list of URLs")
            elif [url.strip() for url in declared] != split:
                errors.append(
                    f"{source} metadata urls {declared!r} do not match url "
                    f"{cell['url']!r}"
                )
    if cell.get("backend") not in (None, "file", "uffd"):
        errors.append(f"{source} metadata has unsupported backend {cell['backend']!r}")
    if cell.get("uffd_mode") not in (None, "file", "copy", "minor"):
        errors.append(
            f"{source} metadata has unsupported uffd_mode {cell['uffd_mode']!r}"
        )
    if (
        cell.get("backend") == "file" and cell.get("uffd_mode") != "file"
    ) or (
        cell.get("backend") == "uffd" and cell.get("uffd_mode") not in ("copy", "minor")
    ):
        errors.append(
            f"{source} metadata backend={cell.get('backend')!r} is inconsistent "
            f"with uffd_mode={cell.get('uffd_mode')!r}"
        )
    if cell.get("format") not in (None, "png", "jpeg"):
        errors.append(f"{source} metadata has unsupported format {cell['format']!r}")
    if cell.get("network_mode") not in (None, "rootless", "bridged", "routed"):
        errors.append(
            f"{source} metadata has unsupported network_mode {cell['network_mode']!r}"
        )
    if cell.get("rust_log") not in (None, "fcvm=debug"):
        errors.append(
            f"{source} metadata rust_log must be 'fcvm=debug', got {cell['rust_log']!r}"
        )
    if cell.get("quiet_loadavg1_limit") not in (None, QUIET_LOADAVG1_LIMIT):
        errors.append(
            f"{source} metadata quiet_loadavg1_limit must be "
            f"{QUIET_LOADAVG1_LIMIT}"
        )
    if "quality" in cell and not 1 <= cell["quality"] <= 100:
        errors.append(f"{source} metadata quality is outside 1..100")
    if "cdp_port" in cell and not 1 <= cell["cdp_port"] <= 65535:
        errors.append(f"{source} metadata cdp_port is outside 1..65535")
    if "port_mappings" in cell and "cdp_port" in cell:
        cdp = cell["cdp_port"]
        if not any(
            mapping.get("proto") == "tcp"
            and mapping.get("host_port") == cdp
            and mapping.get("guest_port") == cdp
            for mapping in cell["port_mappings"]
        ):
            errors.append(
                f"{source} metadata does not publish TCP {cdp}:{cdp}"
            )
    if "port_mappings" in cell:
        identities = [
            (mapping.get("host_ip"), mapping["host_port"], mapping["guest_port"], mapping["proto"])
            for mapping in cell["port_mappings"]
        ]
        if len(identities) != len(set(identities)):
            errors.append(f"{source} metadata has duplicate port mappings")
    for field in (
        "snapshot_config_sha256", "fcvm_sha256", "harness_sha256",
        "runtime_bundle_sha256",
    ):
        value = cell.get(field, "")
        if len(value) != 64 or any(character not in "0123456789abcdef" for character in value):
            errors.append(f"{source} metadata has invalid {field}")
    snapshot_generation_id = cell.get("snapshot_generation_id", "")
    try:
        canonical_generation_id = str(uuid.UUID(snapshot_generation_id))
    except (AttributeError, ValueError):
        canonical_generation_id = ""
    if canonical_generation_id != snapshot_generation_id:
        errors.append(f"{source} metadata has invalid snapshot_generation_id")
    revision = cell.get("source_revision", "")
    if len(revision) != 40 or any(character not in "0123456789abcdef" for character in revision):
        errors.append(f"{source} metadata has invalid source_revision")
    image_id = cell.get("image_id", "")
    if (
        not image_id.startswith("sha256:")
        or len(image_id) != 71
        or any(character not in "0123456789abcdef" for character in image_id[7:])
    ):
        errors.append(f"{source} metadata has invalid image_id")
    snapshot_vm_id = cell.get("snapshot_vm_id", "")
    if (
        len(snapshot_vm_id) != 35
        or not snapshot_vm_id.startswith("vm-")
        or any(character not in "0123456789abcdef" for character in snapshot_vm_id[3:])
    ):
        errors.append(f"{source} metadata has invalid snapshot_vm_id")
    if errors:
        return None, errors
    return cell, []


def _cell_id(cell):
    raw = json.dumps(cell, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(raw).hexdigest()


def _line_ranges(lines):
    lines = sorted(set(lines))
    ranges = []
    for line in lines:
        if not ranges or line != ranges[-1][1] + 1:
            ranges.append([line, line])
        else:
            ranges[-1][1] = line
    return ranges


def provenance(records):
    """Exact file lines and governing metadata line for contributing records."""
    grouped = {}
    for record in records:
        source = record.get("_source") or {}
        path = source.get("path")
        line = source.get("line")
        meta_line = source.get("meta_line")
        sha256 = source.get("sha256")
        size = source.get("size")
        if path is None or line is None:
            continue
        grouped.setdefault((path, sha256, size, meta_line), []).append(line)
    return [
        {
            "path": path,
            "sha256": sha256,
            "size": size,
            "meta_line": meta_line,
            "record_lines": _line_ranges(lines),
            "n": len(lines),
        }
        for (path, sha256, size, meta_line), lines in sorted(
            grouped.items(), key=lambda item: (item[0][0], item[0][3] or -1)
        )
    ]


def metric_summary(records, getter):
    contributing = []
    values = []
    for record in records:
        value = getter(record)
        if value is not None:
            contributing.append(record)
            values.append(value)
    result = dict(zip(("median", "lo", "hi", "n"), median_ci(values)))
    result["provenance"] = provenance(contributing)
    return result


def _validate_arms(arms, label, errors):
    """Validate the publication arms list, appending findings to `errors`.

    Extracted from _validate_schedule so tests exercise the ACTUAL rule rather
    than scanning source text (a rewritten requirement with different wording
    defeated the old source-scan). exec is ALLOWED but not REQUIRED: it is
    retired from measurement, and requiring it forced the arm that pollutes
    the shared prefetch working set into every publication run (run citation
    in reqbench.py's arm validation).
    """
    if (
        not isinstance(arms, list)
        or not arms
        or any(not isinstance(arm, str) or not arm for arm in arms)
        or len(arms) != len(set(arms))
    ):
        errors.append(f"{label} metadata has no valid duplicate-free arms list")
    elif any(arm not in {"exec", "cdp", "cdp-fast", "html", "noop"} for arm in arms):
        errors.append(f"{label} metadata declares an unsupported arm")
    elif "noop" not in arms or not any(is_cdp_class(arm) for arm in arms):
        errors.append(
            f"{label} publication schedule requires noop and a CDP arm"
        )


def _validate_resolver(meta, label, errors):
    """A URL whose host is a name needs the resolver the meta records.

    Appended after the record checks rather than raised as a cell error: a
    cell error stops schedule validation, and a corpus run without guest_dns
    still has to have every one of its records judged.
    """
    guest_dns = meta.get("guest_dns")
    for url in declared_urls(meta):
        needs_resolver = url_needs_resolver(url)
        if needs_resolver is None:
            errors.append(f"{label} metadata declares URL {url!r} without a hostname")
        elif needs_resolver and guest_dns is None:
            errors.append(
                f"{label} metadata declares URL {url!r} whose hostname needs "
                "a resolver but records no guest_dns"
            )


def _validate_schedule(dataset):
    """Validate one producer invocation's declared, complete arm schedule."""
    if len(dataset.get("metas", [])) != 1:
        return
    meta = dataset["metas"][0]
    source = meta.get("_source") or {}
    label = f"{source.get('path')}:{source.get('line')}"
    run_id = meta.get("run_id")
    arms = meta.get("arms")
    reps = meta.get("reps")
    warmup = meta.get("warmup")
    seed = meta.get("seed")
    started = meta.get("started")
    errors = dataset["metadata_errors"]

    def finite_number(value):
        return (
            isinstance(value, (int, float))
            and not isinstance(value, bool)
            and math.isfinite(value)
        )

    def finite_nonnegative(value):
        return finite_number(value) and value >= 0

    def finite_positive(value):
        return finite_number(value) and value > 0

    if not isinstance(run_id, str) or not run_id:
        errors.append(f"{label} metadata has no run_id")
    if (
        not isinstance(started, (int, float))
        or isinstance(started, bool)
        or not math.isfinite(started)
    ):
        errors.append(f"{label} metadata has no numeric started timestamp")
    if not isinstance(seed, int) or isinstance(seed, bool):
        errors.append(f"{label} metadata has no integer seed")
    _validate_arms(arms, label, errors)
    if not isinstance(reps, int) or isinstance(reps, bool) or reps <= 0:
        errors.append(f"{label} metadata has no positive reps count")
    if not isinstance(warmup, int) or isinstance(warmup, bool) or warmup <= 0:
        errors.append(f"{label} metadata has no positive warmup count")
    loadavg = meta.get("loadavg")
    try:
        start_load = [float(value) for value in loadavg]
    except (TypeError, ValueError):
        start_load = []
    quiet_limit = meta.get("quiet_loadavg1_limit")
    if not finite_positive(quiet_limit):
        # _cell_from_meta already reports the bad cell field. Keep schedule
        # validation total as well: malformed metadata must become a gate error,
        # never a TypeError while comparing load samples with a string/NaN.
        quiet_limit = QUIET_LOADAVG1_LIMIT
    if len(start_load) != 3 or any(not finite_nonnegative(value) for value in start_load):
        errors.append(f"{label} metadata has no valid three-value loadavg sample")
    elif start_load[0] > quiet_limit:
        errors.append(
            f"{label} host loadavg1 {start_load[0]} exceeds quiet limit "
            f"{quiet_limit}"
        )
    quiet_vm_processes = meta.get("quiet_vm_processes")
    if (
        not isinstance(quiet_vm_processes, int)
        or isinstance(quiet_vm_processes, bool)
        or quiet_vm_processes != 0
    ):
        errors.append(
            f"{label} metadata does not prove zero VM processes at guard time"
        )
    guard_load = meta.get("quiet_guard_loadavg1")
    if not finite_nonnegative(guard_load):
        errors.append(f"{label} metadata has no valid guard-time loadavg1")
    elif guard_load > quiet_limit:
        errors.append(
            f"{label} guard-time loadavg1 {guard_load} exceeds quiet limit "
            f"{quiet_limit}"
        )
    if meta.get("quiet_guard_passed") is not True:
        errors.append(f"{label} metadata does not prove the quiet-host guard passed")
    if meta.get("allow_busy") is True:
        errors.append(f"{label} metadata used ALLOW_BUSY and is exploratory only")
    if errors:
        dataset["run_id"] = run_id if isinstance(run_id, str) else None
        return

    rng = random.Random(seed)
    expected_order = []
    for rep in range(warmup + reps):
        order = list(arms)
        rng.shuffle(order)
        expected_order.extend((arm, rep, rep < warmup) for arm in order)
    expected = set(expected_order)
    observed = {}
    observed_order = []
    # Per-run constants, hoisted out of the record loop. The meta owns the
    # engine: the harness stamps it once per run, so a record's own stamp is
    # asserted against it below rather than allowed to pick the schema that
    # judges it.
    meta_engine = meta.get("engine") or "chromium"
    expected_quality = meta["quality"] if meta["format"] == "jpeg" else 0
    for record in dataset["records"]:
        rsource = record.get("_source") or {}
        rlabel = f"{rsource.get('path')}:{rsource.get('line')}"
        arm = record.get("arm")
        rep = record.get("rep")
        is_warmup = record.get("warmup")
        if (
            not isinstance(arm, str)
            or not isinstance(rep, int)
            or isinstance(rep, bool)
            or not isinstance(is_warmup, bool)
        ):
            errors.append(f"{rlabel} record has invalid arm/rep/warmup identity")
            continue
        key = (arm, rep, is_warmup)
        observed_order.append(key)
        expected_id = f"{run_id}:{arm}:{rep}:{int(is_warmup)}"
        if record.get("run_id") != run_id:
            errors.append(f"{rlabel} record run_id does not match metadata run {run_id}")
        if record.get("record_id") != expected_id:
            errors.append(f"{rlabel} record_id is not {expected_id}")
        if key in observed:
            errors.append(
                f"{rlabel} duplicates schedule record {key}; first at line {observed[key]}"
            )
        else:
            observed[key] = rsource.get("line")

        ok = record.get("ok")
        if not isinstance(ok, bool):
            errors.append(f"{rlabel} record ok must be boolean, got {ok!r}")
        teardown = record.get("teardown")
        expected_mode = (
            "exec" if arm == "exec"
            else "fast" if arm == "cdp-fast"
            else "normal"
        )
        if not isinstance(teardown, dict):
            errors.append(f"{rlabel} record has no teardown object")
        else:
            if teardown.get("all_gone") is not True:
                errors.append(f"{rlabel} record does not confirm all_gone")
            if teardown.get("disk_cleanup_verified") is not True:
                errors.append(f"{rlabel} record does not verify disk cleanup")
            if teardown.get("child_attribution_established") is not True:
                errors.append(f"{rlabel} record does not prove child attribution")
            if teardown.get("mode") != expected_mode:
                errors.append(
                    f"{rlabel} arm {arm} has teardown mode "
                    f"{teardown.get('mode')!r}, expected {expected_mode!r}"
                )
            collection_fields = {
                "survivors": dict,
                "disk_errors": list,
                "disk_reap_failed": list,
            }
            for field, expected_type in collection_fields.items():
                value = teardown.get(field)
                if value is not None and not isinstance(value, expected_type):
                    errors.append(
                        f"{rlabel} teardown {field} has invalid type "
                        f"{type(value).__name__}"
                    )
                elif value:
                    errors.append(
                        f"{rlabel} teardown claims success but also records {field}"
                    )
            skipped = teardown.get("disk_reap_skipped")
            if skipped is not None and not isinstance(skipped, bool):
                errors.append(
                    f"{rlabel} teardown disk_reap_skipped must be boolean when present"
                )
            elif skipped:
                errors.append(
                    f"{rlabel} teardown claims success but also records disk_reap_skipped"
                )
            # These are the complete set of optional numeric fields emitted by
            # teardown_normal/fast/exec. They are consumed selectively below,
            # but all of them remain publication evidence even on a failed or
            # warmup attempt. A string/boolean/negative value must not evade the
            # schema merely because that particular summary does not read it.
            for metric in (
                "signal_ms", "reap_wall_ms", "fcvm_exit_ms",
                "teardown_total_ms", "disk_reap_ms", "machine_cpu_ms",
                "harness_cpu_ms", "machine_cpu_window_ms",
                "machine_cpu_ms_net", "machine_cpu_ms_net_lo",
                "machine_cpu_ms_net_hi", "control_machine_cpu_ms",
                "control_harness_cpu_ms", "control_wall_ms", "control_target_ms",
                "control_cpu_ms_net", "control_cpu_ms_net_lo",
                "control_cpu_ms_net_hi", "control_busy_cores",
                "control_busy_cores_lo", "control_busy_cores_hi",
                "cpu_residual_uncertainty_ms", "machine_cpu_resolution_ms",
                "harness_cpu_resolution_ms", "sample_period_s", "tick_ms",
            ):
                value = teardown.get(metric)
                if value is not None and not finite_nonnegative(value):
                    errors.append(
                        f"{rlabel} teardown has invalid {metric}={value!r}"
                    )
            for metric in (
                "machine_cpu_ms_raw", "control_cpu_ms_raw",
                "machine_cpu_ms_excess", "machine_cpu_ms_excess_lo",
                "machine_cpu_ms_excess_hi",
            ):
                value = teardown.get(metric)
                if value is not None and not finite_number(value):
                    errors.append(
                        f"{rlabel} teardown has invalid {metric}={value!r}"
                    )
            for field in (
                "machine_cpu_ms_subtraction_clamped",
                "control_cpu_ms_subtraction_clamped",
            ):
                value = teardown.get(field)
                if value is not None and not isinstance(value, bool):
                    errors.append(
                        f"{rlabel} teardown has invalid {field}={value!r}"
                    )
            processes = teardown.get("per_child_cpu")
            if processes is not None:
                if not isinstance(processes, dict):
                    errors.append(f"{rlabel} teardown per_child_cpu is not an object")
                else:
                    for process_name, process in processes.items():
                        if not isinstance(process, dict):
                            # The successful fast-arm validator below adds the
                            # arm-specific message. This generic check is needed
                            # for failed/warmup records too.
                            continue
                        for metric in (
                            "cpu_before_ms", "cpu_final_ms", "reclaim_cpu_ms",
                            "reclaim_cpu_ms_lo", "reclaim_cpu_ms_hi",
                        ):
                            value = process.get(metric)
                            if value is not None and not finite_nonnegative(value):
                                errors.append(
                                    f"{rlabel} teardown process {process_name} "
                                    f"has invalid {metric}={value!r}"
                                )
                        below_resolution = process.get("below_resolution")
                        if (
                            below_resolution is not None
                            and not isinstance(below_resolution, bool)
                        ):
                            errors.append(
                                f"{rlabel} teardown process {process_name} "
                                "has invalid below_resolution classification"
                            )
        if isinstance(ok, bool):
            for metric in ("blocking_ms", "wall_ms"):
                value = record.get(metric)
                if not finite_nonnegative(value):
                    errors.append(
                        f"{rlabel} record has invalid {metric}={value!r}"
                    )
            if (
                finite_nonnegative(record.get("blocking_ms"))
                and finite_nonnegative(record.get("wall_ms"))
                and record["wall_ms"] < record["blocking_ms"]
            ):
                errors.append(f"{rlabel} record has wall_ms < blocking_ms")
        if ok is True:
            if arm == "exec":
                if record.get("rc") != 0 or record.get("timed_out") is not False:
                    errors.append(f"{rlabel} successful exec did not exit cleanly")
                if not finite_nonnegative(record.get("render_total_ms")):
                    errors.append(f"{rlabel} successful exec has no render_total_ms")
            elif arm in ("cdp", "cdp-fast", "html"):
                for metric in ("state_to_port_ms", "spawn_to_port_ms"):
                    if not finite_nonnegative(record.get(metric)):
                        errors.append(f"{rlabel} successful CDP record has no {metric}")
                render = record.get("render")
                if not isinstance(render, dict) or render.get("ok") is not True:
                    errors.append(f"{rlabel} successful CDP record has no successful render")
                else:
                    # Multi-URL runs declare meta.urls and cycle rep-modulo;
                    # the expected URL for THIS record is re-derived from the
                    # schedule, which is stricter than set membership: a
                    # record rendering the right URL at the wrong rep is a
                    # schedule violation.
                    urls = meta.get("urls")
                    if isinstance(urls, list) and urls:
                        expected_url = urls[record.get("rep", 0) % len(urls)]
                    else:
                        expected_url = meta.get("url")
                    # The two engines emit different render schemas and each
                    # is held to its own: a webkit record judged by the
                    # chromium schema fails structurally (~13 errors per
                    # healthy render: wd_host is not cdp_host, WebDriver has
                    # no idle policy, no prewire, no tcp/upgrade/enable
                    # stages), which gated every webkit run into failure.
                    engine = meta_engine
                    # cdpdrive stamps no engine field, so a missing stamp means
                    # chromium, never "whatever the meta says": a webkit run
                    # whose renders carry no stamp must fail, not inherit.
                    if (render.get("engine") or "chromium") != meta_engine:
                        errors.append(
                            f"{rlabel} render engine {render.get('engine')!r} "
                            f"mismatches metadata {meta.get('engine')!r}"
                        )
                    host_key = "wd_host" if engine == "webkit" else "cdp_host"
                    expected_fields = {
                        "url": expected_url,
                        "format": meta.get("format"),
                        host_key: record.get("endpoint"),
                    }
                    for field, expected_value in expected_fields.items():
                        if render.get(field) != expected_value:
                            errors.append(
                                f"{rlabel} CDP render {field}={render.get(field)!r} "
                                f"does not match {expected_value!r}"
                            )
                    # per_url buckets by the record's own url stamp, so the
                    # stamp is held to the schedule as strictly as render.url.
                    if "url" in record and record.get("url") != expected_url:
                        errors.append(
                            f"{rlabel} record url {record.get('url')!r} does not "
                            f"match the scheduled {expected_url!r}"
                        )
                    if engine != "webkit" and render.get("idle_timeout") != 0:
                        errors.append(
                            f"{rlabel} CDP render did not complete its declared idle policy"
                        )
                    if engine == "webkit":
                        # Classic WebDriver has no target discovery: the warm
                        # session id is snapshot state, so every rep must run
                        # prewired.
                        if render.get("session_prewired") is not True:
                            errors.append(
                                f"{rlabel} webkit render ran without its "
                                "prewired session"
                            )
                    else:
                        prewired_expected = meta.get("ws_url_prewired")
                        discovery_warmup = (
                            is_warmup
                            and prewired_expected is True
                            and render.get("target_prewired") is False
                        )
                        # --prewire pins the URL on the first successful warmup rep,
                        # which itself necessarily runs unprewired; measured reps are
                        # held strictly to the metadata.
                        if not discovery_warmup and (
                            render.get("target_prewired") is not prewired_expected
                        ):
                            errors.append(f"{rlabel} CDP target prewire mode mismatches metadata")
                    stages = render.get("stages")
                    if engine == "webkit":
                        # WebDriver classic is plain HTTP: no WebSocket
                        # (tcp/upgrade/enable), no idle policy, no separate
                        # decode, and no navigation-timing extraction. The html
                        # arm is refused upfront for this engine.
                        if arm == "html":
                            errors.append(
                                f"{rlabel} html arm is not valid for webkit records"
                            )
                        required_stages = (
                            "resolve_ms", "connect_total_ms", "navigate_ms",
                            "screenshot_ms", "total_ms",
                        )
                    elif arm == "html":
                        # The html op swaps the terminal screenshot+decode stages
                        # for a single DOM-extraction stage.
                        required_stages = (
                            "resolve_ms", "tcp_ms", "upgrade_ms", "enable_ms",
                            "connect_total_ms", "navigate_ms", "idle_ms",
                            "extract_ms", "nav_timing_ms", "total_ms",
                        )
                    else:
                        required_stages = (
                            "resolve_ms", "tcp_ms", "upgrade_ms", "enable_ms",
                            "connect_total_ms", "navigate_ms", "idle_ms",
                            "screenshot_ms", "decode_ms", "nav_timing_ms", "total_ms",
                        )
                    if not isinstance(stages, dict):
                        errors.append(f"{rlabel} successful CDP render has no stages")
                    else:
                        for stage in required_stages:
                            if not finite_nonnegative(stages.get(stage)):
                                errors.append(
                                    f"{rlabel} successful CDP render has invalid {stage}"
                                )
                    if arm == "html":
                        html_bytes = render.get("html_bytes")
                        html_sha256 = render.get("html_sha256")
                        if (
                            not isinstance(html_bytes, int)
                            or isinstance(html_bytes, bool)
                            or html_bytes <= 0
                        ):
                            errors.append(f"{rlabel} html render has no positive html_bytes")
                        if (
                            not isinstance(html_sha256, str)
                            or len(html_sha256) != 64
                            or any(c not in "0123456789abcdef" for c in html_sha256)
                        ):
                            errors.append(f"{rlabel} html render has invalid html_sha256")
                    image_bytes = render.get("image_bytes")
                    image_sha256 = render.get("image_sha256")
                    if arm != "html" and (
                        not isinstance(image_bytes, int)
                        or isinstance(image_bytes, bool)
                        or image_bytes <= 0
                    ):
                        errors.append(f"{rlabel} CDP render has no positive image_bytes")
                    if arm != "html" and (
                        not isinstance(image_sha256, str)
                        or len(image_sha256) != 64
                        or any(
                            character not in "0123456789abcdef"
                            for character in image_sha256
                        )
                    ):
                        errors.append(f"{rlabel} CDP render has invalid image_sha256")
                    for dimension in (
                        () if (arm == "html" or engine == "webkit") else ("width", "height")
                    ):
                        value = render.get(dimension)
                        if (
                            not isinstance(value, int)
                            or isinstance(value, bool)
                            or value <= 0
                        ):
                            errors.append(f"{rlabel} CDP render has invalid {dimension}")
                    if (
                        arm != "html"
                        and engine != "webkit"
                        and render.get("quality") != expected_quality
                    ):
                        errors.append(f"{rlabel} CDP render quality mismatches metadata")
                    nav_fields = (
                        "dns_ms", "connect_ms", "tls_ms", "ttfb_ms",
                        "resp_ms", "load_ms",
                    )
                    # WebDriver classic exposes no navigation timing.
                    if engine != "webkit":
                        nav = render.get("nav")
                        if not isinstance(nav, dict):
                            errors.append(f"{rlabel} CDP render has no navigation timing")
                        else:
                            for field in nav_fields:
                                if not finite_nonnegative(nav.get(field)):
                                    errors.append(
                                        f"{rlabel} CDP render has invalid nav.{field}"
                                    )
                if arm == "cdp-fast" and isinstance(teardown, dict):
                    if teardown.get("accounting_version") != "post-terminal-ambient-v2":
                        errors.append(f"{rlabel} fast teardown has stale accounting semantics")
                    for metric in (
                        "reap_wall_ms", "teardown_total_ms", "machine_cpu_ms",
                        "harness_cpu_ms", "machine_cpu_window_ms",
                        "machine_cpu_ms_net", "machine_cpu_ms_net_lo",
                        "machine_cpu_ms_net_hi", "control_machine_cpu_ms",
                        "control_harness_cpu_ms", "control_wall_ms", "control_target_ms",
                        "control_cpu_ms_net", "control_cpu_ms_net_lo",
                        "control_cpu_ms_net_hi", "control_busy_cores",
                        "control_busy_cores_lo", "control_busy_cores_hi",
                        "cpu_residual_uncertainty_ms", "machine_cpu_resolution_ms",
                        "harness_cpu_resolution_ms", "sample_period_s", "tick_ms",
                    ):
                        if not finite_nonnegative(teardown.get(metric)):
                            errors.append(
                                f"{rlabel} fast teardown has no valid {metric}"
                            )
                    for metric in (
                        "machine_cpu_window_ms", "control_wall_ms", "control_target_ms",
                        "cpu_residual_uncertainty_ms", "machine_cpu_resolution_ms",
                        "harness_cpu_resolution_ms", "sample_period_s", "tick_ms",
                    ):
                        if not finite_positive(teardown.get(metric)):
                            errors.append(
                                f"{rlabel} fast teardown has no positive {metric}"
                            )
                    for metric in (
                        "machine_cpu_ms_raw", "control_cpu_ms_raw",
                        "machine_cpu_ms_excess", "machine_cpu_ms_excess_lo",
                        "machine_cpu_ms_excess_hi",
                    ):
                        if not finite_number(teardown.get(metric)):
                            errors.append(
                                f"{rlabel} fast teardown has no finite {metric}"
                            )
                    if teardown.get("machine_cpu_source") != (
                        "/proc/stat:busy(user,nice,system,irq,softirq,steal)"
                    ):
                        errors.append(f"{rlabel} fast teardown has wrong machine CPU source")
                    if teardown.get("harness_cpu_source") != (
                        "/proc/self/stat:utime+stime"
                    ):
                        errors.append(f"{rlabel} fast teardown has wrong harness CPU source")
                    for field in (
                        "machine_cpu_ms_subtraction_clamped",
                        "control_cpu_ms_subtraction_clamped",
                    ):
                        if not isinstance(teardown.get(field), bool):
                            errors.append(f"{rlabel} fast teardown has no valid {field}")

                    def close_enough(field, expected):
                        actual = teardown.get(field)
                        if not finite_number(actual) or not math.isclose(
                            actual, expected, rel_tol=1e-9, abs_tol=1e-6
                        ):
                            errors.append(
                                f"{rlabel} fast teardown {field}={actual!r} "
                                f"does not match derived {expected!r}"
                            )

                    machine_resolution = teardown.get("machine_cpu_resolution_ms")
                    harness_resolution = teardown.get("harness_cpu_resolution_ms")
                    if finite_positive(machine_resolution) and finite_positive(
                        harness_resolution
                    ):
                        tick_ms = teardown.get("tick_ms")
                        if finite_positive(tick_ms):
                            close_enough("machine_cpu_resolution_ms", tick_ms)
                            close_enough("harness_cpu_resolution_ms", tick_ms)
                        uncertainty = (
                            6 * machine_resolution + 2 * harness_resolution
                        )
                        close_enough("cpu_residual_uncertainty_ms", uncertainty)

                        def validate_residual(prefix, machine_field, harness_field):
                            machine = teardown.get(machine_field)
                            harness = teardown.get(harness_field)
                            if not (
                                finite_nonnegative(machine)
                                and finite_nonnegative(harness)
                            ):
                                return
                            raw = machine - harness
                            close_enough(f"{prefix}_raw", raw)
                            close_enough(f"{prefix}_net", max(0.0, raw))
                            close_enough(
                                f"{prefix}_net_lo", max(0.0, raw - uncertainty)
                            )
                            close_enough(
                                f"{prefix}_net_hi", max(0.0, raw + uncertainty)
                            )
                            clamped = teardown.get(f"{prefix}_subtraction_clamped")
                            if clamped is not (raw < 0.0):
                                errors.append(
                                    f"{rlabel} fast teardown {prefix} clamp "
                                    "classification contradicts its raw residual"
                                )
                            if raw < -uncertainty - 1e-6:
                                errors.append(
                                    f"{rlabel} fast teardown {prefix} raw residual "
                                    "exceeds its negative counter-resolution bound"
                                )

                        validate_residual(
                            "machine_cpu_ms", "machine_cpu_ms", "harness_cpu_ms"
                        )
                        validate_residual(
                            "control_cpu_ms",
                            "control_machine_cpu_ms",
                            "control_harness_cpu_ms",
                        )

                        control_wall = teardown.get("control_wall_ms")
                        control_target = teardown.get("control_target_ms")
                        if finite_positive(control_target):
                            close_enough("control_target_ms", 50.0)
                        if (
                            finite_positive(control_wall)
                            and finite_positive(control_target)
                            and control_wall + 1e-6 < control_target
                        ):
                            errors.append(
                                f"{rlabel} fast teardown control window ended "
                                "before its declared target"
                            )
                        machine_window = teardown.get("machine_cpu_window_ms")
                        reap_wall = teardown.get("reap_wall_ms")
                        if (
                            finite_positive(machine_window)
                            and finite_nonnegative(reap_wall)
                            and machine_window + 1e-6 < reap_wall
                        ):
                            errors.append(
                                f"{rlabel} fast teardown machine CPU window does "
                                "not enclose reap_wall_ms"
                            )
                        control_net = teardown.get("control_cpu_ms_net")
                        control_lo = teardown.get("control_cpu_ms_net_lo")
                        control_hi = teardown.get("control_cpu_ms_net_hi")
                        machine_net = teardown.get("machine_cpu_ms_net")
                        machine_lo = teardown.get("machine_cpu_ms_net_lo")
                        machine_hi = teardown.get("machine_cpu_ms_net_hi")
                        if (
                            finite_positive(control_wall)
                            and all(
                                finite_nonnegative(value)
                                for value in (control_net, control_lo, control_hi)
                            )
                        ):
                            control_rate = control_net / control_wall
                            control_rate_lo = control_lo / control_wall
                            control_rate_hi = control_hi / control_wall
                            close_enough("control_busy_cores", control_rate)
                            close_enough("control_busy_cores_lo", control_rate_lo)
                            close_enough("control_busy_cores_hi", control_rate_hi)
                            if (
                                finite_positive(machine_window)
                                and all(
                                    finite_nonnegative(value)
                                    for value in (machine_net, machine_lo, machine_hi)
                                )
                            ):
                                close_enough(
                                    "machine_cpu_ms_excess",
                                    machine_net - control_rate * machine_window,
                                )
                                close_enough(
                                    "machine_cpu_ms_excess_lo",
                                    machine_lo - control_rate_hi * machine_window,
                                )
                                close_enough(
                                    "machine_cpu_ms_excess_hi",
                                    machine_hi - control_rate_lo * machine_window,
                                )
                                excess = teardown.get("machine_cpu_ms_excess")
                                excess_lo = teardown.get("machine_cpu_ms_excess_lo")
                                excess_hi = teardown.get("machine_cpu_ms_excess_hi")
                                if (
                                    finite_number(excess)
                                    and finite_number(excess_lo)
                                    and finite_number(excess_hi)
                                    and not excess_lo <= excess <= excess_hi
                                ):
                                    errors.append(
                                        f"{rlabel} fast teardown excess point lies "
                                        "outside its quantization interval"
                                    )
                    processes = teardown.get("per_child_cpu")
                    if not isinstance(processes, dict) or not processes:
                        errors.append(f"{rlabel} fast teardown has no per-process CPU data")
                    else:
                        if "fcvm" not in processes:
                            errors.append(f"{rlabel} fast teardown omits fcvm CPU data")
                        for process_name, process in processes.items():
                            if not isinstance(process, dict):
                                errors.append(
                                    f"{rlabel} fast teardown process {process_name} is invalid"
                                )
                                continue
                            if not isinstance(process.get("complete"), bool):
                                errors.append(
                                    f"{rlabel} fast teardown process {process_name} "
                                    "has no completion classification"
                                )
                            reclaim = process.get("reclaim_cpu_ms")
                            if reclaim is not None and not finite_nonnegative(reclaim):
                                errors.append(
                                    f"{rlabel} fast teardown process {process_name} "
                                    "has invalid reclaim CPU"
                                )
            elif arm == "noop" and not finite_nonnegative(record.get("spawn_to_port_ms")):
                errors.append(f"{rlabel} successful noop has no spawn_to_port_ms")
        elif ok is False and not isinstance(record.get("error"), str):
            errors.append(f"{rlabel} failed record has no string error")
        loadavg = record.get("loadavg1")
        if not finite_nonnegative(loadavg):
            errors.append(f"{rlabel} record has invalid loadavg1={loadavg!r}")

    missing = sorted(expected - set(observed))
    extra = sorted(set(observed) - expected)
    if missing:
        errors.append(
            f"{label} schedule is missing {len(missing)} record(s), first={missing[:5]}"
        )
    if extra:
        errors.append(
            f"{label} schedule has {len(extra)} undeclared record(s), first={extra[:5]}"
        )
    if observed_order != expected_order:
        mismatch = next(
            (
                index,
                expected_order[index] if index < len(expected_order) else None,
                observed_order[index] if index < len(observed_order) else None,
            )
            for index in range(max(len(expected_order), len(observed_order)))
            if index >= len(expected_order)
            or index >= len(observed_order)
            or expected_order[index] != observed_order[index]
        )
        errors.append(
            f"{label} record order does not match seed {seed}; first mismatch "
            f"at ordinal {mismatch[0]} expected={mismatch[1]} observed={mismatch[2]}"
        )
    _validate_resolver(meta, label, errors)
    dataset["run_id"] = run_id


def read_source_artifact(path):
    """Read one immutable input view and bind it to a content identity."""
    with open(path, "rb") as source_file:
        before = os.fstat(source_file.fileno())
        raw = source_file.read()
        after = os.fstat(source_file.fileno())
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    if any(getattr(before, field) != getattr(after, field) for field in fields):
        raise RuntimeError(f"{path}: input changed while it was being read")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise RuntimeError(f"{path}: input is not UTF-8: {error}") from error
    return {
        "path": path,
        "realpath": os.path.realpath(path),
        "device": before.st_dev,
        "inode": before.st_ino,
        "size": len(raw),
        "mtime_ns": before.st_mtime_ns,
        "ctime_ns": before.st_ctime_ns,
        "sha256": hashlib.sha256(raw).hexdigest(),
        "text": text,
    }


def exec_from_sealed_source() -> None:
    """Re-exec the analyzer from a sealed memfd and bind identity to those bytes.

    Hashing ``__file__`` after Python loaded it can attribute the analysis to a
    concurrent edit rather than the code executing. The first process performs
    no analysis: it copies its source into a sealed anonymous file and execs a
    fresh interpreter from that exact fd. The second process verifies both the
    digest and kernel seals before it is allowed to continue.
    """
    sealed_value = os.environ.get(SEALED_ANALYZER_FD_ENV)
    if sealed_value:
        try:
            sealed_fd = int(sealed_value)
            seals = fcntl.fcntl(sealed_fd, fcntl.F_GET_SEALS)
        except (ValueError, OSError) as error:
            raise RuntimeError(f"invalid sealed analyzer descriptor: {error}") from error
        required = (
            fcntl.F_SEAL_SEAL
            | fcntl.F_SEAL_SHRINK
            | fcntl.F_SEAL_GROW
            | fcntl.F_SEAL_WRITE
        )
        if seals & required != required:
            raise RuntimeError("analyzer source fd is not immutably sealed")
        sealed_path = f"/proc/self/fd/{sealed_fd}"
        try:
            executing_sealed_source = os.path.samefile(__file__, sealed_path)
        except OSError as error:
            raise RuntimeError(
                f"cannot bind executing analyzer to sealed source fd: {error}"
            ) from error
        if not executing_sealed_source:
            raise RuntimeError(
                "sealed analyzer descriptor does not identify the executing source"
            )
        artifact = read_source_artifact(sealed_path)
        if artifact["sha256"] != os.environ.get("REQANALYZE_EXPECTED_SHA256"):
            raise RuntimeError("sealed analyzer digest does not match launcher digest")
        return

    source_path = os.path.abspath(__file__)
    artifact = read_source_artifact(source_path)
    sealed_fd = os.memfd_create("fcvm-reqanalyze", flags=os.MFD_ALLOW_SEALING)
    try:
        raw = artifact["text"].encode("utf-8")
        offset = 0
        while offset < len(raw):
            offset += os.write(sealed_fd, raw[offset:])
        os.lseek(sealed_fd, 0, os.SEEK_SET)
        fcntl.fcntl(
            sealed_fd,
            fcntl.F_ADD_SEALS,
            fcntl.F_SEAL_SEAL
            | fcntl.F_SEAL_SHRINK
            | fcntl.F_SEAL_GROW
            | fcntl.F_SEAL_WRITE,
        )
        os.set_inheritable(sealed_fd, True)
        env = dict(os.environ)
        env[SEALED_ANALYZER_FD_ENV] = str(sealed_fd)
        env[ANALYZER_SOURCE_PATH_ENV] = source_path
        env["REQANALYZE_EXPECTED_SHA256"] = artifact["sha256"]
        os.execve(
            sys.executable,
            [sys.executable, f"/proc/self/fd/{sealed_fd}", *sys.argv[1:]],
            env,
        )
    except BaseException:
        os.close(sealed_fd)
        raise


def source_marker(artifact, line, meta_line):
    return {
        "path": artifact["path"],
        "line": line,
        "meta_line": meta_line,
        "sha256": artifact["sha256"],
        "size": artifact["size"],
    }


def public_artifact_identity(artifact):
    return {key: value for key, value in artifact.items() if key != "text"}


def write_json_atomic(path, value):
    """Publish analysis without truncating an existing artifact on failure."""
    directory = os.path.dirname(os.path.abspath(path)) or "."
    fd, temporary = tempfile.mkstemp(prefix=".reqanalyze-", dir=directory)
    try:
        with os.fdopen(fd, "w") as output:
            json.dump(value, output, indent=2, allow_nan=False)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def reject_json_constant(value):
    raise ValueError(f"non-standard JSON numeric constant {value}")


def load(paths):
    """Load JSONL as metadata-governed segments with file:line provenance.

    A JSONL file is appendable, so a later run can add another metadata record.
    Each ordinary record belongs to the nearest preceding metadata record, not to
    a file-wide union. Records before the first metadata line are invalid. This
    prevents two different snapshots, binaries, URLs, or render settings in one
    file from being pooled merely because both say ``backend=file``.
    """
    datasets = []
    seen_files = {}
    for path in paths:
        try:
            artifact = read_source_artifact(path)
        except (OSError, RuntimeError) as error:
            datasets.append({
                "backend": None,
                "cell": None,
                "cell_id": None,
                "run_id": None,
                "records": [],
                "metas": [],
                "sources": [path],
                "source_artifacts": [],
                "metadata_errors": [str(error)],
            })
            continue
        file_identity = (artifact["device"], artifact["inode"])
        if file_identity in seen_files:
            datasets.append({
                "backend": None,
                "cell": None,
                "cell_id": None,
                "run_id": None,
                "records": [],
                "metas": [],
                "sources": [path],
                "source_artifacts": [artifact],
                "metadata_errors": [
                    f"duplicate input {path} is the same file as {seen_files[file_identity]}"
                ],
            })
            continue
        seen_files[file_identity] = path
        before = len(datasets)
        current = None
        orphan = None
        for line_no, raw in enumerate(artifact["text"].splitlines(), 1):
                text = raw.strip()
                if not text:
                    continue
                try:
                    record = json.loads(
                        text,
                        object_pairs_hook=reject_duplicate_keys,
                        parse_constant=reject_json_constant,
                    )
                except (json.JSONDecodeError, ValueError) as error:
                    datasets.append({
                        "backend": None,
                        "cell": None,
                        "cell_id": None,
                        "records": [],
                        "metas": [],
                        "sources": [path],
                        "source_artifacts": [artifact],
                        "metadata_errors": [f"{path}:{line_no}: invalid JSON: {error}"],
                    })
                    current = None
                    continue

                if not isinstance(record, dict):
                    datasets.append({
                        "backend": None,
                        "cell": None,
                        "cell_id": None,
                        "run_id": None,
                        "records": [],
                        "metas": [],
                        "sources": [path],
                        "source_artifacts": [artifact],
                        "metadata_errors": [
                            f"{path}:{line_no}: JSONL value must be an object"
                        ],
                    })
                    current = None
                    continue

                if record.get("kind") == "meta":
                    record = dict(record)
                    record["_source"] = source_marker(artifact, line_no, line_no)
                    source = f"{path}:{line_no}"
                    cell, errors = _cell_from_meta(record, source)
                    current = {
                        "backend": cell.get("backend") if cell else None,
                        "cell": cell,
                        "cell_id": _cell_id(cell) if cell else None,
                        "run_id": record.get("run_id"),
                        "records": [],
                        "metas": [record],
                        "sources": [path],
                        "source_artifacts": [artifact],
                        "metadata_errors": errors,
                    }
                    datasets.append(current)
                    continue

                record = dict(record)
                record["_source"] = source_marker(
                    artifact,
                    line_no,
                    current["metas"][0]["_source"]["line"] if current else None,
                )
                if current is None:
                    if orphan is None:
                        orphan = {
                            "backend": None,
                            "cell": None,
                            "cell_id": None,
                            "records": [],
                            "metas": [],
                            "sources": [path],
                            "source_artifacts": [artifact],
                            "metadata_errors": [
                                f"{path}:{line_no}: record appears before any metadata"
                            ],
                        }
                        datasets.append(orphan)
                    orphan["records"].append(record)
                else:
                    current["records"].append(record)
        if len(datasets) == before:
            datasets.append({
                "backend": None,
                "cell": None,
                "cell_id": None,
                "run_id": None,
                "records": [],
                "metas": [],
                "sources": [path],
                "source_artifacts": [artifact],
                "metadata_errors": [f"{path}: empty JSONL has no metadata or records"],
            })

    seen_record_ids = {}
    for dataset in datasets:
        _validate_schedule(dataset)
        for record in dataset.get("records", []):
            record_id = record.get("record_id")
            if not isinstance(record_id, str) or not record_id:
                continue
            source = record.get("_source") or {}
            here = f"{source.get('path')}:{source.get('line')}"
            if record_id in seen_record_ids:
                dataset["metadata_errors"].append(
                    f"{here} duplicates record_id {record_id} from {seen_record_ids[record_id]}"
                )
            else:
                seen_record_ids[record_id] = here
    return datasets


def hodges_lehmann_shift(a, b, iters=20000, seed=999):
    """Bootstrap CI on the difference of medians (b - a). Sign matters, so report it."""
    a = [float(x) for x in a]
    b = [float(x) for x in b]
    if not a or not b:
        return None, None, None
    rng = random.Random(seed)
    d = statistics.median(b) - statistics.median(a)
    boots = []
    for _ in range(iters):
        sa = [a[rng.randrange(len(a))] for _ in range(len(a))]
        sb = [b[rng.randrange(len(b))] for _ in range(len(b))]
        boots.append(statistics.median(sb) - statistics.median(sa))
    boots.sort()
    return d, boots[int(0.025 * iters)], boots[int(0.975 * iters) - 1]


def failure_label(r: dict) -> str:
    """The failure string an operator needs, wherever the producer put it.

    `r.get("error", f"rc={r.get('rc')}")` alone reported every CDP drop as
    `rc=None`: `run_cdp_request` nests the whole cdpdrive result under `render`,
    and on a clean `ok: false` (cdpdrive returning rather than raising) the top
    level carried no `error` at all. The producer now lifts the label, but the
    analyzer is the PUBLICATION GATE reading a JSONL artifact that outlives the
    producer, so it reads both and prefers the top level.
    """
    parts = []
    for value in (
        r.get("request_error"),
        r.get("error"),
        (r.get("render") or {}).get("error"),
        r.get("teardown_error"),
    ):
        if value and value not in parts:
            parts.append(value)
    return "; ".join(parts) if parts else f"rc={r.get('rc')}"


def failure_class(r: dict) -> str:
    """Transport / readiness / render, wherever the producer put it."""
    return (
        r.get("failure_class")
        or (r.get("render") or {}).get("failure_class")
        or "unknown"
    )


def analyze_backend(
    recs, metas, backend, cell, cell_id, run_id, sources,
    source_artifacts, metadata_errors, stall_max_ms=None,
):
    live = [r for r in recs if not r.get("warmup")]
    warm = [r for r in recs if r.get("warmup")]
    ok = [r for r in live if r.get("ok")]
    bad = [r for r in live if not r.get("ok")]

    print("=" * 78)
    print("REQUEST-OPTIMIZED A/B  --  medians with 95% bootstrap CIs")
    print("=" * 78)
    print(f"  backend={backend or 'UNASSIGNED'}")
    sources = list(dict.fromkeys(sources))
    for source in sources:
        print(f"  input={source}")
    for error in metadata_errors:
        print(f"  BACKEND METADATA ERROR: {error}")
    for m in metas:
        print(f"  seed={m.get('seed')} arms={m.get('arms')} reps={m.get('reps')} "
              f"warmup={m.get('warmup')} url={m.get('url')}")
        print(f"  loadavg at start: {m.get('loadavg')}")
    print(f"\n  records: {len(recs)}  warmup DISCARDED: {len(warm)}  "
          f"measured: {len(live)}  ok: {len(ok)}  failed: {len(bad)}")
    if bad:
        seen: dict = {}
        for r in bad:
            seen[failure_label(r)] = seen.get(failure_label(r), 0) + 1
        for k, v in sorted(seen.items(), key=lambda kv: -kv[1]):
            print(f"    FAILURE x{v}: {k}")

    la = [r.get("loadavg1") for r in live if r.get("loadavg1") is not None]
    if la:
        print(f"  loadavg1 during run: min={min(la):.1f} median={statistics.median(la):.1f} "
              f"max={max(la):.1f}   <-- contention check (AGENTS.md)")

    arms = []
    for r in recs:
        if r.get("arm") and r["arm"] not in arms:
            arms.append(r["arm"])
    arms.sort(key=lambda a: {"exec": 0, "cdp": 1, "cdp-fast": 2, "noop": 9}.get(a, 5))
    by = {a: [r for r in ok if r.get("arm") == a] for a in arms}
    # EVERY attempt, not just the successes. The teardown/leak section reads this
    # one: a request that failed is precisely the one whose teardown is most
    # likely to have leaked, and the ok-filter made `all_gone: false` invisible.
    attempted = {a: [r for r in recs if r.get("arm") == a] for a in arms}
    measured_attempted = {a: [r for r in live if r.get("arm") == a] for a in arms}

    out = {
        "backend": backend,
        "cell": cell,
        "cell_id": cell_id,
        "run_id": run_id,
        "sources": sources,
        "source_artifacts": [
            public_artifact_identity(artifact) for artifact in source_artifacts
        ],
        "metadata_errors": metadata_errors,
        "provenance": {
            "metadata": [m.get("_source") for m in metas if m.get("_source")],
            "records": provenance(live),
        },
        "arms": {},
        "n_failed": sum(1 for record in recs if not record.get("ok")),
        "n_warmup_discarded": len(warm),
    }

    # ---- 0. availability per arm. This gates everything printed below it.
    print("\n" + "-" * 78)
    print("AVAILABILITY PER ARM (medians below are computed over SUCCESSES ONLY)")
    print("-" * 78)
    any_unpublishable = False
    for a in arms:
        n_att = len(attempted[a])
        n_bad = sum(1 for r in attempted[a] if not r.get("ok"))
        lo, hi = clopper_pearson(n_bad, n_att)
        pub = n_bad == 0
        any_unpublishable |= not pub
        note = "" if pub else "   ** DO NOT PUBLISH THIS ARM'S LATENCY **"
        print(f"  {a:9s} {n_att - n_bad}/{n_att} completed   "
              f"failure {100 * n_bad / n_att if n_att else 0:.1f}% "
              f"[{100 * lo:.2f}%, {100 * hi:.2f}%] 95% CP{note}")
        # A transport drop, a readiness exhaustion and a render error are three
        # different defects and must not share one bucket in the artifact. This is
        # what makes REVIEW.md's "200 CDP requests per backend at 0 failures" gate
        # checkable from the analyzer's own output instead of by hand-reading
        # jsonl — and `failure_class` was, until now, written by cdpdrive and read
        # by nothing at all.
        classes: dict = {}
        for r in attempted[a]:
            if not r.get("ok"):
                c = failure_class(r)
                classes[c] = classes.get(c, 0) + 1
        if classes:
            print("            failure classes: "
                  + ", ".join(f"{k}={v}" for k, v in sorted(classes.items())))
        out["arms"].setdefault(a, {}).update(
            attempted=n_att, failed=n_bad,
            failure_rate=(n_bad / n_att) if n_att else None,
            failure_rate_ci=[lo, hi], publishable=pub,
            failure_classes=classes,
            provenance=provenance(attempted[a]),
        )
    if any_unpublishable:
        print("  Arms are DIFFERENTLY CENSORED, so a cross-arm delta below is not a")
        print("  like-for-like comparison. Quote the per-arm failure rate next to any")
        print("  number taken from an arm marked DO NOT PUBLISH, or do not quote it.")

    # ---- 0b. sample size per backend and per CDP arm. Metadata names the arms
    # the producer intended to run, so an aborted arm with zero records cannot
    # disappear from the gate merely by disappearing from the observed records.
    expected_cdp_arms = set()
    for meta in metas:
        for arm in meta.get("arms") or []:
            if is_cdp_class(arm):
                expected_cdp_arms.add(arm)
    if not expected_cdp_arms:
        expected_cdp_arms.update(a for a in arms if is_cdp_class(a))
    expected_cdp_arms = sorted(expected_cdp_arms)
    cdp_counts = {
        a: len(measured_attempted.get(a, [])) for a in expected_cdp_arms
    }
    cdp_short = {
        a: count for a, count in cdp_counts.items()
        if count < MIN_CDP_ATTEMPTS_PER_BACKEND
    }
    cdp_sample_passed = bool(expected_cdp_arms) and not cdp_short
    print("\n" + "-" * 78)
    print("CDP SAMPLE-SIZE GATE (measured, non-warmup attempts; evaluated per backend)")
    print("-" * 78)
    if not expected_cdp_arms:
        print("  FAIL: no CDP arm is declared or observed")
    for arm in expected_cdp_arms:
        count = cdp_counts[arm]
        verdict = "PASS" if count >= MIN_CDP_ATTEMPTS_PER_BACKEND else "FAIL"
        print(f"  {arm:9s} {count}/{MIN_CDP_ATTEMPTS_PER_BACKEND} attempts  {verdict}")
    out["cdp_sample_size"] = {
        "required_per_arm": MIN_CDP_ATTEMPTS_PER_BACKEND,
        "expected_arms": expected_cdp_arms,
        "measured_non_warmup_attempts_per_arm": cdp_counts,
        "provenance_per_arm": {
            arm: provenance(measured_attempted.get(arm, []))
            for arm in expected_cdp_arms
        },
        "passed": cdp_sample_passed,
    }

    # ---- 1. drift control FIRST. If this moved, nothing below is trustworthy.
    print("\n" + "-" * 78)
    print("DRIFT CONTROL (arm=noop: clone spawn + restore + teardown, NO page, NO CDP)")
    print("-" * 78)
    noop = by.get("noop", [])
    drifted = False
    if len(noop) >= MIN_NOOP_ATTEMPTS:
        noop_sorted = sorted(noop, key=lambda r: r["rep"])
        half = len(noop_sorted) // 2
        f = [r["blocking_ms"] for r in noop_sorted[:half]]
        s = [r["blocking_ms"] for r in noop_sorted[half:]]
        d, dlo, dhi = hodges_lehmann_shift(f, s)
        print(f"  first half : {fmt(*median_ci(f))}")
        print(f"  second half: {fmt(*median_ci(s))}")
        print(f"  drift (2nd - 1st): {d:.1f} ms, 95% CI [{dlo:.1f}, {dhi:.1f}]")
        drift_passed = (
            dlo >= -DRIFT_EQUIVALENCE_MARGIN_MS
            and dhi <= DRIFT_EQUIVALENCE_MARGIN_MS
        )
        drifted = not drift_passed
        print(
            f"  equivalence margin: +/-{DRIFT_EQUIVALENCE_MARGIN_MS:.1f} ms"
        )
        print("  VERDICT: " + (
            "DRIFT NOT RULED OUT -- CI crosses the practical margin, do not publish"
            if drifted else
            "drift is practically equivalent within the declared margin"
        ))
        out["drift"] = {
            "evaluated": True, "n": len(noop), "delta_ms": d,
            "ci": [dlo, dhi], "significant": not (dlo <= 0 <= dhi),
            "equivalence_margin_ms": DRIFT_EQUIVALENCE_MARGIN_MS,
            "passed": drift_passed,
            "required": MIN_NOOP_ATTEMPTS,
            "provenance": provenance(noop_sorted),
        }
    else:
        print(f"  FAIL: insufficient noop samples ({len(noop)}/{MIN_NOOP_ATTEMPTS})")
        out["drift"] = {
            "evaluated": False, "n": len(noop), "delta_ms": None,
            "ci": None, "significant": False, "passed": False,
            "required": MIN_NOOP_ATTEMPTS,
            "provenance": provenance(noop),
        }

    # ---- 2. end-to-end, measured end to end, never composed from stages
    print("\n" + "-" * 78)
    print("END-TO-END, CALLER-BLOCKING (spawn -> answer in hand). Measured, not composed.")
    print("-" * 78)
    for a in arms:
        v = [r["blocking_ms"] for r in by[a] if r.get("blocking_ms") is not None]
        print(f"  {a:9s} blocking  {fmt(*median_ci(v))}")
        out["arms"].setdefault(a, {})["blocking_ms"] = metric_summary(
            by[a], lambda r: r.get("blocking_ms")
        )
    print("\n  WALL (spawn -> VM fully gone; what the MACHINE pays, not the caller)")
    for a in arms:
        v = [r["wall_ms"] for r in by[a] if r.get("wall_ms") is not None]
        print(f"  {a:9s} wall      {fmt(*median_ci(v))}")
        out["arms"][a]["wall_ms"] = metric_summary(by[a], lambda r: r.get("wall_ms"))

    # ---- 3. paired deltas with CIs
    print("\n" + "-" * 78)
    print("DELTAS (negative = faster). CI crossing zero = NOT a result.")
    print("-" * 78)
    pairs = [("exec", "cdp", "PART 1: request transport (exec'd python -> host CDP)"),
             ("cdp", "cdp-fast", "PART 2: teardown discipline (awaited -> early response)"),
             ("exec", "cdp-fast", "BOTH parts combined")]
    out["deltas"] = {}
    for a, b, label in pairs:
        if a in by and b in by:
            # A delta between differently-censored arms is not a like-for-like
            # comparison: each median is conditioned on that arm's successes.
            censor = ""
            if not (out["arms"][a]["publishable"] and out["arms"][b]["publishable"]):
                censor = (f"  [CENSORED: {a} dropped {out['arms'][a]['failed']}/"
                          f"{out['arms'][a]['attempted']}, {b} dropped "
                          f"{out['arms'][b]['failed']}/{out['arms'][b]['attempted']} "
                          f"-- DO NOT PUBLISH without both rates]")
            for metric in ("blocking_ms", "wall_ms"):
                va = [r[metric] for r in by[a] if r.get(metric) is not None]
                vb = [r[metric] for r in by[b] if r.get(metric) is not None]
                d, lo, hi = hodges_lehmann_shift(va, vb)
                if d is None:
                    continue
                sig = "" if (lo <= 0 <= hi) else "  *"
                print(f"  {label}{censor}")
                print(f"    {metric:12s} {a} -> {b}: {d:+.1f} ms  CI [{lo:+.1f}, {hi:+.1f}]{sig}")
                out["deltas"][f"{a}->{b}:{metric}"] = {
                    "delta": d, "ci": [lo, hi],
                    "significant": not (lo <= 0 <= hi),
                    "publishable": censor == "",
                    "provenance": {
                        a: provenance([r for r in by[a] if r.get(metric) is not None]),
                        b: provenance([r for r in by[b] if r.get(metric) is not None]),
                    },
                }

    # ---- 4. CDP stage decomposition
    print("\n" + "-" * 78)
    # NOT "forward-localhost". That flag is GUEST -> HOST (bench/chromium/AGENTS.md,
    # src/cli/args.rs: "Enables containers to reach host-only services via
    # localhost"), so it cannot carry this path in this direction — and pointing it
    # at the CDP port HIJACKS the guest's own loopback listener. What is actually
    # measured is `--publish 9222:9222`; fc-agent DNATs that eligible TCP port to
    # guest loopback. reqbench.sh never passes --forward-localhost and there is no
    # benchmark-owned relay in the path.
    print("CDP ARMS: per-request stage decomposition (host -> --publish -> guest loopback)")
    print("-" * 78)
    stage_keys = ["resolve_ms", "tcp_ms", "upgrade_ms", "enable_ms", "connect_total_ms",
                  "navigate_ms", "screenshot_ms", "total_ms"]
    for a in [x for x in arms if is_cdp_class(x)]:
        # html swaps the terminal screenshot stage for extract (no image);
        # every other stage is the same CDP handshake all cdp-class arms pay.
        arm_stage_keys = [
            ("extract_ms" if (k == "screenshot_ms" and a == "html") else k)
            for k in stage_keys
        ]
        print(f"  [{a}]")
        for metric, explanation in (
            ("spawn_to_port_ms", "process spawn -> first TCP accept; stable readiness boundary"),
            ("state_to_port_ms", "first state discovery -> first TCP accept; diagnostic only"),
        ):
            values = [r.get(metric) for r in by[a] if r.get(metric) is not None]
            if values:
                print(f"    {metric:16s} {fmt(*median_ci(values))}   ({explanation})")
                out["arms"][a][metric] = metric_summary(
                    by[a], lambda r, key=metric: r.get(key)
                )
        # `--ws-url` prewiring SKIPS /json/list and writes a synthetic
        # `resolve_ms = 0.0`. Pooling that with measured values publishes a stage
        # that was never timed, so the two populations are never mixed.
        prewired = {bool((r.get("render") or {}).get("target_prewired")) for r in by[a]}
        out["arms"][a]["target_prewired"] = sorted(prewired)
        if prewired == {True}:
            print("    resolve_ms       SKIPPED (--ws-url prewired; NOT measured)")
        elif len(prewired) > 1:
            print("    resolve_ms       MIXED prewired/measured records -- refusing to pool")
        for k in arm_stage_keys:
            if k == "resolve_ms" and (prewired == {True} or len(prewired) > 1):
                continue
            # cdpdrive nests its timings under render.stages; reading render[k]
            # directly silently yields nothing, which looks like "no data" rather
            # than a bug. Read both, prefer stages.
            v = [(r.get("render", {}).get("stages") or {}).get(k)
                 if (r.get("render", {}).get("stages") or {}).get(k) is not None
                 else r.get("render", {}).get(k)
                 for r in by[a]]
            v = [x for x in v if x is not None]
            if v:
                print(f"    {k:16s} {fmt(*median_ci(v))}")
                out["arms"][a][k] = metric_summary(
                    by[a],
                    lambda r, key=k: (
                        (r.get("render", {}).get("stages") or {}).get(key)
                        if (r.get("render", {}).get("stages") or {}).get(key) is not None
                        else r.get("render", {}).get(key)
                    ),
                )
    if "exec" in by:
        v = [r.get("render_total_ms") for r in by["exec"] if r.get("render_total_ms")]
        if v:
            print(f"  [exec] in-guest render.py total_ms {fmt(*median_ci(v))}")
            out["arms"]["exec"]["render_total_ms"] = metric_summary(
                by["exec"], lambda r: r.get("render_total_ms")
            )

    # ---- 4b. per-URL breakdown. A pooled corpus median hides the page that
    # stalled; each URL gets its own medians, load-event time and count of
    # distinct screenshots (an error page renders to the same bytes every time).
    urls = []
    for m in metas:
        for url in declared_urls(m):
            if url not in urls:
                urls.append(url)
    for r in live:
        url = record_url(r)
        if url is not None and url not in urls:
            urls.append(url)
    out["per_url"] = {}
    for url in urls:
        out["per_url"][url] = {}
        for a in arms:
            rows = [r for r in by[a] if record_url(r) == url]
            out["per_url"][url][a] = {
                "n": len(rows),
                "blocking_ms": metric_summary(rows, lambda r: r.get("blocking_ms")),
                "navigate_load_event_ms": metric_summary(
                    rows, lambda r: render_stage(r, "navigate_load_event_ms")
                ),
                "total_ms": metric_summary(
                    rows, lambda r: render_stage(r, "total_ms")
                ),
                "distinct_image_sha256": len({
                    r["render"]["image_sha256"]
                    for r in rows
                    if isinstance(r.get("render"), dict)
                    and isinstance(r["render"].get("image_sha256"), str)
                }),
            }
    if len(urls) > 1:
        print("\n" + "-" * 78)
        print("PER URL (measured successes: blocking, load event, distinct screenshots)")
        print("-" * 78)
        for url in urls:
            print(f"  {url}")
            for a in arms:
                per = out["per_url"][url][a]
                if not per["n"]:
                    continue
                blocking = per["blocking_ms"]
                load_event = per["navigate_load_event_ms"]
                print(
                    f"    {a:9s} n={per['n']:<4d} blocking "
                    f"{fmt(blocking['median'], blocking['lo'], blocking['hi'])}  "
                    f"load-event "
                    f"{fmt(load_event['median'], load_event['lo'], load_event['hi'])}  "
                    f"images {per['distinct_image_sha256']}"
                )

    # ---- 4c. stall gate. A load event that takes tens of seconds is the guest
    # waiting on something other than the page (a resolver that never answers,
    # for one), and one such record inside a pooled median is invisible.
    engine = next((m.get("engine") or "chromium" for m in metas), "chromium")
    violations = []
    evaluated = 0
    if stall_max_ms is not None:
        for r in recs:
            arm = r.get("arm")
            value = render_stage(r, "navigate_load_event_ms")
            # cdpdrive times the load event on every successful render; a
            # chromium success without the stage has not shown it did not
            # stall. WebDriver classic exposes no such stage.
            must_carry = (
                r.get("ok") is True and is_cdp_class(arm) and engine != "webkit"
            )
            if value is None and not must_carry:
                continue
            if value is not None:
                evaluated += 1
            if value is None or value > stall_max_ms:
                violations.append({
                    "url": record_url(r),
                    "arm": arm,
                    "navigate_load_event_ms": value,
                    "rep": r.get("rep"),
                    "warmup": bool(r.get("warmup")),
                    "record_id": r.get("record_id"),
                })
    out["stall_gate"] = {
        "max_ms": stall_max_ms,
        "passed": not violations,
        "evaluated": evaluated,
        "violations": violations,
    }
    print("\n" + "-" * 78)
    print("STALL GATE (render.stages.navigate_load_event_ms, every record incl. warmup)")
    print("-" * 78)
    if stall_max_ms is None:
        print("  not evaluated (no --stall-max-ms)")
    else:
        print(f"  limit {stall_max_ms:g} ms  evaluated {evaluated}  "
              f"violations {len(violations)}  "
              + ("PASS" if not violations else "FAIL -- do not publish"))
        for violation in violations:
            print(f"    {violation['arm']:9s} rep={violation['rep']}"
                  f"{' [warmup]' if violation['warmup'] else ''} "
                  f"load-event={violation['navigate_load_event_ms']!r} "
                  f"url={violation['url']}")

    # ---- 5. teardown, three separate numbers, never one
    print("\n" + "-" * 78)
    print("TEARDOWN -- three numbers, deliberately not summed into one")
    print("-" * 78)
    any_unconfirmed_teardown = False
    any_unverified_disk_cleanup = False
    for a in arms:
        # `attempted`, NOT `by`: see the module docstring. A failed request's
        # teardown is the one most likely to have leaked.
        td = [r.get("teardown") for r in attempted[a] if r.get("teardown")]
        if not td:
            print(f"  {a:9s} no teardown evidence")
            print(f"    disk_cleanup: 0/{len(attempted[a])} confirmed"
                  f"  ** {len(attempted[a])} NOT CONFIRMED CLEAN **")
            out["arms"][a]["disk_cleanup_confirmed"] = [0, len(attempted[a])]
            out["arms"][a]["teardown_provenance"] = []
            any_unverified_disk_cleanup = True
            any_unconfirmed_teardown = True
            continue
        ok_td = [r.get("teardown") for r in by[a] if r.get("teardown")]
        rw = [t.get("reap_wall_ms") for t in ok_td if t.get("reap_wall_ms") is not None]
        tt = [t.get("teardown_total_ms") for t in ok_td if t.get("teardown_total_ms") is not None]
        print(f"  [{a}] mode={td[0].get('mode')}")
        if rw:
            print(f"    reap_wall_ms      {fmt(*median_ci(rw))}   (kill -> processes truly gone)")
            print(f"    teardown_total_ms {fmt(*median_ci(tt))}   (incl. synchronous on-disk reap)")
            out["arms"][a]["reap_wall_ms"] = metric_summary(
                by[a], lambda r: (r.get("teardown") or {}).get("reap_wall_ms")
            )
            out["arms"][a]["teardown_total_ms"] = metric_summary(
                by[a], lambda r: (r.get("teardown") or {}).get("teardown_total_ms")
            )
        dr = [t.get("disk_reap_ms") for t in ok_td if t.get("disk_reap_ms") is not None]
        if dr:
            print(f"    disk_reap_ms      {fmt(*median_ci(dr))}   (state file + data dir)")
            out["arms"][a]["disk_reap_ms"] = metric_summary(
                by[a], lambda r: (r.get("teardown") or {}).get("disk_reap_ms")
            )

        # machine_cpu_ms_excess uses an adjacent POST-TERMINAL ambient control, so
        # the control cannot contain the live VM CPU absent from the reclaim window.
        # V2 nests the six-field /proc/stat interval around the two-field harness
        # interval and retains their deliberately conservative quantization bounds.
        mc = [t.get("machine_cpu_ms_excess") for t in ok_td
              if t.get("machine_cpu_ms_excess") is not None
              and t.get("harness_cpu_ms") is not None
              and t.get("accounting_version") == "post-terminal-ambient-v2"]
        stale = [t for t in ok_td if t.get("machine_cpu_ms_excess") is not None
                 and t.get("accounting_version") != "post-terminal-ambient-v2"]
        if stale:
            print(f"    machine_cpu_excess WITHHELD on {len(stale)} record(s): "
                  "their ambient-control semantics are stale")
        if mc:
            mc_lo = [
                t.get("machine_cpu_ms_excess_lo")
                for t in ok_td
                if t.get("accounting_version") == "post-terminal-ambient-v2"
                and isinstance(t.get("machine_cpu_ms_excess_lo"), (int, float))
                and not isinstance(t.get("machine_cpu_ms_excess_lo"), bool)
                and math.isfinite(t.get("machine_cpu_ms_excess_lo"))
            ]
            mc_hi = [
                t.get("machine_cpu_ms_excess_hi")
                for t in ok_td
                if t.get("accounting_version") == "post-terminal-ambient-v2"
                and isinstance(t.get("machine_cpu_ms_excess_hi"), (int, float))
                and not isinstance(t.get("machine_cpu_ms_excess_hi"), bool)
                and math.isfinite(t.get("machine_cpu_ms_excess_hi"))
            ]
            print(f"    machine_cpu_excess {fmt(*median_ci(mc))}   "
                  "(/proc/stat busy CPU; ambient and harness subtracted; "
                  "per-record quantization bounds retained)")
            if mc_lo and mc_hi:
                print(
                    "    counter envelope    "
                    f"lo {fmt(*median_ci(mc_lo))}  hi {fmt(*median_ci(mc_hi))}"
                )
            out["arms"][a]["machine_cpu_ms_excess"] = metric_summary(
                by[a],
                lambda r: (
                    (r.get("teardown") or {}).get("machine_cpu_ms_excess")
                    if (r.get("teardown") or {}).get("accounting_version")
                    == "post-terminal-ambient-v2"
                    else None
                ),
            )
            out["arms"][a]["machine_cpu_ms_excess_lo"] = metric_summary(
                by[a], lambda r: (r.get("teardown") or {}).get(
                    "machine_cpu_ms_excess_lo"
                )
            )
            out["arms"][a]["machine_cpu_ms_excess_hi"] = metric_summary(
                by[a], lambda r: (r.get("teardown") or {}).get(
                    "machine_cpu_ms_excess_hi"
                )
            )

        # PER CHILD, keyed by comm. Pooling loses the straggler: the median of
        # {firecracker 110 ms, holder 0, pasta 0} is 0.
        tick = next((t.get("tick_ms") for t in td if t.get("tick_ms")), None)
        by_child: dict = {}
        for t in ok_td:
            for cname, c in (t.get("per_child_cpu") or {}).items():
                if c.get("reclaim_cpu_ms") is None:
                    continue
                b = by_child.setdefault(cname, {"complete": [], "lower": [], "sub_tick": 0})
                (b["complete"] if c.get("complete") else b["lower"]).append(c["reclaim_cpu_ms"])
                if c.get("below_resolution") or c["reclaim_cpu_ms"] == 0.0:
                    b["sub_tick"] += 1
        if by_child:
            res = f" (/proc tick = {tick:.0f} ms)" if tick else ""
            print(f"    reclaim_cpu_ms per child{res}")
            out["arms"][a]["reclaim_cpu_ms_by_child"] = {}
            for cname in sorted(by_child):
                b = by_child[cname]
                n_tot = len(b["complete"]) + len(b["lower"])
                if b["sub_tick"] == n_tot and tick:
                    print(f"      {cname:20s} < {2 * tick:.0f} ms "
                          f"(below /proc tick resolution, n={n_tot})")
                    out["arms"][a]["reclaim_cpu_ms_by_child"][cname] = {
                        "median": 0.0, "below_resolution": True,
                        "upper_bound_ms": 2 * tick, "n": n_tot,
                        "provenance": provenance([
                            r for r in by[a]
                            if ((r.get("teardown") or {}).get("per_child_cpu") or {})
                            .get(cname, {}).get("reclaim_cpu_ms") is not None
                        ]),
                    }
                    continue
                if b["complete"]:
                    m = median_ci(b["complete"])
                    print(f"      {cname:20s} {fmt(*m)}   COMPLETE (zombie observed)")
                    out["arms"][a]["reclaim_cpu_ms_by_child"][cname] = dict(
                        zip(("median", "lo", "hi", "n"), m))
                    out["arms"][a]["reclaim_cpu_ms_by_child"][cname]["provenance"] = provenance([
                        r for r in by[a]
                        if (((r.get("teardown") or {}).get("per_child_cpu") or {})
                            .get(cname, {}).get("reclaim_cpu_ms") is not None
                            and ((r.get("teardown") or {}).get("per_child_cpu") or {})
                            .get(cname, {}).get("complete") is True)
                    ])
                if b["lower"]:
                    m = median_ci(b["lower"])
                    print(f"      {cname:20s} {fmt(*m)}   LOWER BOUND (reaper won the race)")
                    out["arms"][a]["reclaim_cpu_ms_by_child"].setdefault(cname, {})[
                        "lower_bound"] = dict(zip(("median", "lo", "hi", "n"), m))
                    out["arms"][a]["reclaim_cpu_ms_by_child"][cname]["lower_bound"][
                        "provenance"
                    ] = provenance([
                        r for r in by[a]
                        if (((r.get("teardown") or {}).get("per_child_cpu") or {})
                            .get(cname, {}).get("reclaim_cpu_ms") is not None
                            and ((r.get("teardown") or {}).get("per_child_cpu") or {})
                            .get(cname, {}).get("complete") is not True)
                    ])

        # CLASSIFY ON True, NOT ON False. `ag.count(False)` drove the warning
        # gate while `len(ag)` drove the denominator, so a null or an absent
        # `all_gone` landed in the denominator and OUT of the gate: 27 confirmed
        # + 2 null + 1 missing printed `all_gone: 27/30 confirmed` with no
        # warning and no per-rep dump — the numerator and denominator disagreeing
        # by three while the report reads clean. Exactly the failure mode this
        # section exists to prevent (see the module docstring). Today's producer
        # cannot emit a non-bool here, but the analyzer is the publication gate
        # for an artifact that outlives the producer.
        ag = [t.get("all_gone") for t in td]
        confirmed = ag.count(True)
        unconfirmed = len(ag) - confirmed
        print(f"    all_gone: {confirmed}/{len(ag)} confirmed"
              + (f"  ** {unconfirmed} NOT CONFIRMED GONE **" if unconfirmed else ""))
        out["arms"][a]["all_gone_confirmed"] = [confirmed, len(ag)]
        # Kept separate so "we watched it survive" is distinguishable from "we
        # have no evidence either way".
        out["arms"][a]["all_gone_no_evidence"] = sum(
            1 for x in ag if x is not True and x is not False)
        if unconfirmed:
            any_unconfirmed_teardown = True
            for r in attempted[a]:
                t = r.get("teardown") or {}
                if t.get("all_gone") is not True:
                    print(f"      rep {r.get('rep')} ok={r.get('ok')} "
                          f"all_gone={t.get('all_gone')!r} "
                          f"survivors={t.get('survivors') or t.get('children')}")

        cleanup_confirmed = sum(
            1 for t in td if t.get("disk_cleanup_verified") is True
        )
        cleanup_unverified = len(attempted[a]) - cleanup_confirmed
        print(f"    disk_cleanup: {cleanup_confirmed}/{len(attempted[a])} confirmed"
              + (f"  ** {cleanup_unverified} NOT CONFIRMED CLEAN **"
                 if cleanup_unverified else ""))
        out["arms"][a]["disk_cleanup_confirmed"] = [
            cleanup_confirmed, len(attempted[a])
        ]
        out["arms"][a]["teardown_provenance"] = provenance(
            [r for r in attempted[a] if r.get("teardown")]
        )
        if cleanup_unverified:
            any_unverified_disk_cleanup = True

    failed_by_arm = {
        arm: data["failed"] for arm, data in out["arms"].items()
        if data.get("failed")
    }
    unconfirmed_teardowns = sum(
        confirmed[1] - confirmed[0]
        for data in out["arms"].values()
        if (confirmed := data.get("all_gone_confirmed"))
    )
    reasons = []
    if metadata_errors or backend is None:
        reasons.append("metadata does not assign every record to one complete benchmark cell")
    if any_unpublishable:
        reasons.append("one or more arms dropped requests")
    if not expected_cdp_arms:
        reasons.append("no measured CDP arm was declared or observed")
    for arm, count in cdp_short.items():
        reasons.append(
            f"CDP arm {arm} has {count}/{MIN_CDP_ATTEMPTS_PER_BACKEND} "
            "measured non-warmup attempts"
        )
    if not out["drift"]["passed"]:
        if out["drift"]["evaluated"]:
            reasons.append(
                "baseline drift is not ruled out: noop CI is not contained within "
                "the declared equivalence margin"
            )
        else:
            reasons.append(
                f"noop drift control has {out['drift']['n']}/{MIN_NOOP_ATTEMPTS} "
                "measured successful attempts"
            )
    if any_unconfirmed_teardown:
        reasons.append("one or more teardowns were not confirmed gone")
    if any_unverified_disk_cleanup:
        reasons.append("one or more clone teardowns did not confirm on-disk cleanup")
    if not out["stall_gate"]["passed"]:
        reasons.append(
            f"stall_gate: {len(violations)} record(s) exceed or lack "
            f"navigate_load_event_ms under the {stall_max_ms:g} ms limit"
        )

    out["gate"] = {
        "passed": not reasons,
        "reasons": reasons,
        "backend_metadata": {
            "passed": backend is not None and not metadata_errors,
            "backend": backend,
            "cell": cell,
            "cell_id": cell_id,
            "sources": sources,
            "source_artifacts": [
                public_artifact_identity(artifact) for artifact in source_artifacts
            ],
            "errors": metadata_errors,
            "provenance": [m.get("_source") for m in metas if m.get("_source")],
        },
        "availability": {
            "passed": not any_unpublishable,
            "failed_attempts_per_arm": failed_by_arm,
        },
        "cdp_sample_size": out["cdp_sample_size"],
        "baseline_drift": out["drift"],
        "stall_gate": out["stall_gate"],
        "teardown": {
            "passed": not any_unconfirmed_teardown and not any_unverified_disk_cleanup,
            "unconfirmed": unconfirmed_teardowns,
            "disk_cleanup_unconfirmed": sum(
                total - confirmed
                for data in out["arms"].values()
                if (counts := data.get("disk_cleanup_confirmed"))
                for confirmed, total in [counts]
            ),
        },
        "provenance": provenance(live),
    }
    out["publishable"] = out["gate"]["passed"]
    return out


def main_with(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl", nargs="+")
    ap.add_argument("--json-out", default="")
    ap.add_argument("--no-gate", action="store_true",
                    help="exit 0 even when this run is unpublishable (exploratory only)")
    ap.add_argument("--stall-max-ms", type=float, default=None,
                    help="refuse publication when any record's navigate_load_event_ms "
                         "exceeds this many milliseconds (default: no stall gate)")
    args = ap.parse_args(argv)
    if args.stall_max_ms is not None and not (
        math.isfinite(args.stall_max_ms) and args.stall_max_ms > 0
    ):
        ap.error("--stall-max-ms must be a positive number of milliseconds")
    analyzer_artifact = read_source_artifact(__file__)

    # Valid metadata segments with the exact same cell can contribute to one
    # sample. Backend equality alone is not enough: a different URL, quality,
    # image, snapshot, binary, revision, network mode, CPU, or memory is a
    # different experiment and must never be pooled to satisfy n=200.
    groups = []
    valid_group = {}
    invalid_number = 0
    for dataset in load(args.jsonl):
        backend = dataset["backend"]
        cell_id = dataset["cell_id"]
        run_id = dataset.get("run_id")
        if backend is None or cell_id is None or run_id is None or dataset["metadata_errors"]:
            invalid_number += 1
            group = dict(dataset)
            group["key"] = f"unassigned-{invalid_number}"
            groups.append(group)
            continue
        group_key = (backend, cell_id, run_id)
        if group_key not in valid_group:
            group = {
                "backend": backend,
                "cell": dataset["cell"],
                "cell_id": cell_id,
                "run_id": run_id,
                "records": [],
                "metas": [],
                "sources": [],
                "source_artifacts": [],
                "metadata_errors": [],
            }
            valid_group[group_key] = group
            groups.append(group)
        group = valid_group[group_key]
        for field in (
            "records", "metas", "sources", "source_artifacts", "metadata_errors"
        ):
            group[field].extend(dataset[field])

    backend_cell_counts = {}
    for group in groups:
        if group.get("backend") is not None and group.get("cell_id") is not None:
            backend_cell_counts[group["backend"]] = backend_cell_counts.get(group["backend"], 0) + 1
    for group in groups:
        if "key" in group:
            continue
        group["key"] = (
            group["backend"]
            if backend_cell_counts[group["backend"]] == 1
            else f"{group['backend']}:{group['run_id']}"
        )

    backend_results = {}
    for group in groups:
        backend_results[group["key"]] = analyze_backend(
            group["records"], group["metas"], group["backend"],
            group.get("cell"), group.get("cell_id"), group.get("run_id"), group["sources"],
            group.get("source_artifacts", []),
            group["metadata_errors"],
            stall_max_ms=args.stall_max_ms,
        )

    overall_reasons = [
        f"{key}: {reason}"
        for key, result in backend_results.items()
        for reason in result["gate"]["reasons"]
    ]
    input_artifacts = {}
    for group in groups:
        for artifact in group.get("source_artifacts", []):
            identity = (artifact.get("device"), artifact.get("inode"))
            input_artifacts[identity] = artifact
    output_aliases_input = False
    if args.json_out:
        output_realpath = os.path.realpath(args.json_out)
        protected_artifacts = [*input_artifacts.values(), analyzer_artifact]
        protected_paths = {
            artifact.get("realpath") for artifact in protected_artifacts
        }
        source_path = os.environ.get(
            ANALYZER_SOURCE_PATH_ENV, os.path.abspath(__file__)
        )
        protected_paths.add(os.path.realpath(source_path))
        for artifact in protected_artifacts:
            if output_realpath == artifact.get("realpath"):
                overall_reasons.append(
                    f"analysis output {args.json_out} aliases protected artifact "
                    f"{artifact['path']}"
                )
                output_aliases_input = True
                break
        if not output_aliases_input and output_realpath in protected_paths:
            overall_reasons.append(
                f"analysis output {args.json_out} aliases analyzer source {source_path}"
            )
            output_aliases_input = True
        if not output_aliases_input:
            try:
                output_stat = os.stat(args.json_out)
            except FileNotFoundError:
                output_stat = None
            protected_inodes = {
                (artifact.get("device"), artifact.get("inode"))
                for artifact in protected_artifacts
            }
            if output_stat and (output_stat.st_dev, output_stat.st_ino) in protected_inodes:
                overall_reasons.append(
                    f"analysis output {args.json_out} aliases a protected inode"
                )
                output_aliases_input = True
    publishable = not overall_reasons
    exit_code_overridden = bool(args.no_gate and not publishable)
    if len(backend_results) == 1:
        # Retain the original convenient top-level `arms` report for callers
        # analyzing one backend, while making the backend boundary explicit.
        result = dict(next(iter(backend_results.values())))
        result["backends"] = backend_results
        result["gate"] = dict(result["gate"])
        result["gate"]["exit_code_overridden"] = exit_code_overridden
    else:
        result = {
            "backends": backend_results,
            "publishable": publishable,
            "gate": {
                "passed": publishable,
                "reasons": overall_reasons,
                "exit_code_overridden": exit_code_overridden,
            },
            "stall_gate": {
                "max_ms": args.stall_max_ms,
                "passed": all(
                    backend["stall_gate"]["passed"]
                    for backend in backend_results.values()
                ),
                "evaluated": sum(
                    backend["stall_gate"]["evaluated"]
                    for backend in backend_results.values()
                ),
                "violations": [
                    violation
                    for backend in backend_results.values()
                    for violation in backend["stall_gate"]["violations"]
                ],
            },
        }

    # The override changes process control only. It cannot rewrite the evidence
    # or turn an exploratory run into a publishable one in the JSON artifact.
    result["publishable"] = publishable
    result["gate"]["passed"] = publishable
    result["gate"]["reasons"] = overall_reasons
    result["analysis_identity"] = {
        "schema_version": ANALYZER_SCHEMA_VERSION,
        "reqanalyze": public_artifact_identity(analyzer_artifact),
        "inputs": [
            public_artifact_identity(artifact)
            for artifact in sorted(
                input_artifacts.values(), key=lambda artifact: artifact["path"]
            )
        ],
    }

    if args.json_out and not output_aliases_input:
        write_json_atomic(args.json_out, result)
        print(f"\nwrote {args.json_out}")

    if not publishable:
        print("\nUNPUBLISHABLE RUN:")
        for reason in overall_reasons:
            print(f"  - {reason}")
        if args.no_gate:
            print("Exit status overridden by --no-gate; publishable remains false.")
            return 0
        print("Exiting 5. Pass --no-gate to override only the exit status.")
        return 5
    return 0


def main():
    exec_from_sealed_source()
    return main_with()


if __name__ == "__main__":
    sys.exit(main())
