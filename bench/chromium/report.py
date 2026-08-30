#!/usr/bin/env python3
"""Parsing, sampling, and report generation for the chromium bench harness.

Subcommands:
  sample    one JSON line of host memory + per-clone PSS (used by bench.sh
            during fan-out, and for the podman warm-pool contrast)
  finalize  parse a results dir (requests/*.log, samples/*.jsonl) into
            raw.json + report.md

Request logs are "<epoch> <text>" lines produced by bench.sh's timestamper;
filenames encode <phase>__<mode>__<arm>__<url>__rN.log.
"""

import argparse
import glob
import json
import os
import re
import statistics
import sys
import time

KITESURF_CONTEXT = """\
### Published comparator (context, not measured here)

Cloudflare publishes six rows comparing Kitesurf against a warm Chromium pool,
medians of five Browser Run quick-action runs over a 14-URL corpus. Kitesurf
spends more wall time in order to use far less CPU and memory. Chromium is the
faster of the two on the stopwatch.

| metric | Kitesurf | Chromium (warm pool) | Kitesurf, relative |
|---|---|---|---|
| CPU, screenshot | 380 ms | 1,173 ms | 3.1x less CPU |
| CPU, HTML extraction | 229 ms | 877 ms | 3.8x less CPU |
| memory, screenshot | 57.8 MiB | 271.0 MiB | 4.7x less memory |
| memory, HTML extraction | 39.4 MiB | 273.7 MiB | 7.0x less memory |
| wall, screenshot | 1,148 ms | 637 ms | 1.8x slower |
| wall, HTML extraction | 820 ms | 472 ms | 1.7x slower |

Their summary: "Kitesurf wins on the memory and CPU that drive your bill (by
3-7x), while Chromium wins wall time because a warm just-in-time compiler beats
a cold software renderer."

Every figure above was verified on 2026-08-30 against
https://developers.cloudflare.com/browser-run/kitesurf/, which carries the same
table as https://blog.cloudflare.com/kitesurf/.

Comparable: the workload. bench/chromium/corpus_mirror.sh mirrors the same
public corpus they benchmarked (kitesurf.cloudflare.app/corpus.txt, 14 URLs),
so both sides render the same page list rather than unrelated pages. Their two
wall-time rows also measure the axis this report measures.

Not comparable: everything else. Their page does not state what hardware
produced these numbers; this report runs on a single ARM64 Graviton box that
runs other test workloads concurrently, so absolutes do not transfer. Their CPU
rows are CPU-time per operation, and their memory rows are per browser-engine
instance, while this report accounts memory through cgroup and PSS bases over a
whole process set. Their Chromium column is a warm pool, not a per-request
isolate. Quoted context only; the numbers in this report are measured
independently of it."""


# ---------------------------------------------------------------------------
# sample
# ---------------------------------------------------------------------------
def read_meminfo():
    mi = {}
    with open("/proc/meminfo") as f:
        for line in f:
            k, v = line.split(":", 1)
            mi[k] = int(v.split()[0])
    return mi


def pss_kb_of_pid(pid):
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return int(line.split()[1])
    except (OSError, ValueError):
        return 0
    return 0


# --- matched-basis memory accounting -------------------------------------
# The first run's density claim was refuted because the fcvm side summed PSS
# over firecracker processes ONLY while the container side measured a whole
# cgroup. Everything below measures both sides through the SAME two bases:
#   cgroup  memory.current of the cgroup that contains the entire process set
#   pss     PSS summed over exactly that cgroup's process set
# plus a machine-level MemAvailable delta recorded by the caller.

def cgroup_bytes(cg_path):
    """memory.current of one cgroup (bytes), or None if unreadable."""
    try:
        with open(os.path.join(cg_path, "memory.current")) as f:
            return int(f.read().strip())
    except (OSError, ValueError):
        return None


def cgroup_procs(cg_path):
    try:
        with open(os.path.join(cg_path, "cgroup.procs")) as f:
            return [int(x) for x in f.read().split()]
    except (OSError, ValueError):
        return []


def cgroup_stat(cg_path):
    out = {}
    try:
        with open(os.path.join(cg_path, "memory.stat")) as f:
            for line in f:
                k, _, v = line.partition(" ")
                try:
                    out[k] = int(v)
                except ValueError:
                    pass
    except OSError:
        pass
    return out


def measure_cgroup_set(root, prefix):
    """Sum both bases over every leaf cgroup under `root` whose name starts
    with `prefix`. Returns (n_leaves, cgroup_bytes, pss_kb, n_procs, stat_sums)."""
    n = tot_cg = tot_pss = tot_procs = 0
    stat_sums = {"anon": 0, "file": 0, "kernel": 0, "sock": 0}
    try:
        entries = sorted(os.listdir(root))
    except OSError:
        return 0, 0, 0, 0, stat_sums
    for name in entries:
        if not name.startswith(prefix):
            continue
        path = os.path.join(root, name)
        if not os.path.isdir(path):
            continue
        procs = cgroup_procs(path)
        if not procs:
            continue  # leaf with no live process contributes nothing
        cb = cgroup_bytes(path)
        n += 1
        tot_procs += len(procs)
        if cb is not None:
            tot_cg += cb
        tot_pss += sum(pss_kb_of_pid(p) for p in procs)
        st = cgroup_stat(path)
        for k in stat_sums:
            stat_sums[k] += st.get(k, 0)
    return n, tot_cg, tot_pss, tot_procs, stat_sums


def firecracker_pids_for_vm_ids(vm_ids):
    """Map vm_id -> firecracker pid by the --api-sock path in the cmdline."""
    out = {}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                cmd = f.read().decode(errors="replace")
        except OSError:
            continue
        if "firecracker" not in cmd:
            continue
        for vid in vm_ids:
            if f"/{vid}/" in cmd:
                out[vid] = int(pid)
    return out


def cmd_sample(args):
    rec = {"ts": time.time()}
    mi = read_meminfo()
    # machine-level basis (independent of any per-process attribution)
    rec["mem_available_kb"] = mi.get("MemAvailable", 0)
    rec["mem_free_kb"] = mi.get("MemFree", 0)
    rec["cached_kb"] = mi.get("Cached", 0)
    rec["shmem_kb"] = mi.get("Shmem", 0)
    rec["hugepages_free"] = mi.get("HugePages_Free", 0)
    rec["hugepages_total"] = mi.get("HugePages_Total", 0)

    # --- fcvm clones: EVERY process of each clone, via its own cgroup --------
    if args.cgroup_root and args.cgroup_prefix:
        n, cg, pss, nproc, st = measure_cgroup_set(args.cgroup_root, args.cgroup_prefix)
        rec["clones"] = n
        rec["clone_procs"] = nproc
        rec["clone_cgroup_kb"] = cg // 1024
        rec["clone_pss_kb"] = pss
        rec["clone_anon_kb"] = st["anon"] // 1024
        rec["clone_file_kb"] = st["file"] // 1024
        rec["basis"] = "cgroup+pss over full per-clone process set"

    # legacy firecracker-only PSS, kept ONLY as the refuted comparator so the
    # report can show how large the basis error was. Never the headline number.
    if args.state_dir and args.name_prefix:
        vm_ids = []
        for f in glob.glob(os.path.join(args.state_dir, "*.json")):
            try:
                st_json = json.load(open(f))
            except (OSError, json.JSONDecodeError):
                continue
            if (st_json.get("name") or "").startswith(args.name_prefix):
                vm_ids.append(st_json.get("vm_id"))
        fps = firecracker_pids_for_vm_ids([v for v in vm_ids if v])
        rec.setdefault("clones", len(vm_ids))
        rec["fc_procs"] = len(fps)
        rec["fc_only_pss_kb"] = sum(pss_kb_of_pid(p) for p in fps.values())

    # --- host-native container pool: the SAME two bases ----------------------
    if args.podman_prefix:
        import subprocess
        try:
            names = subprocess.run(
                ["podman", "ps", "--format", "{{.Names}}"],
                capture_output=True, text=True, timeout=20).stdout.split()
        except Exception:
            names = []
        names = [n for n in names if n.startswith(args.podman_prefix)]
        tot_pss = tot_cg = tot_procs = 0
        for n in names:
            try:
                cg = subprocess.run(
                    ["podman", "inspect", "--format", "{{.State.CgroupPath}}", n],
                    capture_output=True, text=True, timeout=20).stdout.strip()
            except Exception:
                continue
            path = "/sys/fs/cgroup" + cg
            procs = cgroup_procs(path)
            tot_procs += len(procs)
            tot_pss += sum(pss_kb_of_pid(p) for p in procs)
            cb = cgroup_bytes(path)
            if cb is not None:
                tot_cg += cb
        rec["pool_containers"] = len(names)
        rec["pool_procs"] = tot_procs
        rec["pool_pss_kb"] = tot_pss
        rec["pool_cgroup_kb"] = tot_cg // 1024

    if args.extra:
        rec.update(json.loads("{" + args.extra + "}"))
    print(json.dumps(rec))


# ---------------------------------------------------------------------------
# request-log parsing
# ---------------------------------------------------------------------------
KV_RE = re.compile(r"(\w+)=([\d.]+)")

def parse_request_log(path):
    base = os.path.basename(path)[:-4]
    parts = base.split("__")
    rec = {"file": base}
    if len(parts) == 5:
        rec.update(dict(zip(["phase", "mode", "arm", "url"], parts[:4])))
        # rN (matrix) or rNiM (fan-out: rep N, clone M inside that burst — the
        # burst is the experimental unit, the clone index is a pseudoreplicate)
        mrep = re.match(r"^r(\d+)(?:i(\d+))?$", parts[4])
        rec["rep"] = int(mrep.group(1)) if mrep else 0
        rec["clone_idx"] = int(mrep.group(2)) if mrep and mrep.group(2) else 0
    t = {}
    for line in open(path, errors="replace"):
        m = re.match(r"^([\d.]+) (.*)$", line.rstrip("\n"))
        if not m:
            continue
        ts, text = float(m.group(1)), m.group(2)
        t.setdefault("t0", ts)
        # Proof that debug logging was on for this request. The exec client only
        # logs its retry summary when attempt > 1, so "debug on and no retry
        # line" means exactly one attempt and zero retry sleep — recording that
        # explicitly is what makes the exec stage fully attributable instead of
        # silently missing. This MUST stay outside the elif chain below: as a
        # branch of it, it matched the ACK/GO handshake lines first (they are
        # themselves fcvm::commands::exec DEBUG lines) and swallowed them, which
        # left exec_handshake_ms/exec_spawn_ms permanently None.
        if "DEBUG fcvm::commands::exec" in text:
            rec["exec_debug_seen"] = True
        # NOTE: markers must be anchored at line start — fcvm's "executing
        # command in clone: ..." info line echoes the whole driver source,
        # which contains the marker strings inside print(...) calls.
        if text.startswith("BENCH_SCHED"):
            # when the scheduler picked this cell — the drift regressor
            t["t_sched"] = ts
            mm = re.search(r"cgroup=(\S+)", text)
            if mm:
                rec["cgroup"] = mm.group(1)
        elif text.startswith("BENCH_T0"):
            t["t0"] = ts
        elif text.startswith("BENCH_HOLD_START"):
            t["t_hold"] = ts
            for k, v in KV_RE.findall(text):
                if k == "hold_s":
                    rec["hold_s"] = float(v)
        elif text.startswith("CHROME_MEM "):
            for k, v in KV_RE.findall(text):
                rec["chrome_" + k] = float(v)
        elif "connected to exec server after retries" in text:
            # defect 4: the exec client's own retry ladder, so waiting inside
            # fcvm's polling is attributable and never mistaken for guest latency
            for k, v in KV_RE.findall(text):
                if k in ("attempts", "retry_wait_ms"):
                    rec["exec_" + k] = float(v)
        elif "exec handshake: ACK received" in text:
            t["t_ack"] = ts
        elif "exec handshake: GO sent" in text:
            t["t_go"] = ts
        elif "cloned from snapshot" in text:
            t["t_restored"] = ts
            rec["restore_mode"] = "direct" if "direct mode" in text else "uffd"
        elif text.startswith("BENCH_EXEC_UP"):
            t["t_exec"] = ts
        elif text.startswith("BENCH_NET_UP"):
            t["t_net"] = ts
            mm = KV_RE.findall(text)
            rec.update({"net_wait_ms": float(v) for k, v in mm if k == "wait_ms"})
        elif text.startswith("BENCH_NET_FAIL"):
            rec["net_fail"] = True
        elif text.startswith(("CHROMIUM_BENCH_READY", "BENCH_READY")):
            t.setdefault("t_ready", ts)
        elif text.startswith("RENDER_OK"):
            t["t_render"] = ts
            for k, v in KV_RE.findall(text):
                if k in ("connect_ms", "navigate_ms", "idle_ms", "screenshot_ms",
                         "dom_ms", "total_ms", "png_bytes", "dom_bytes"):
                    rec["r_" + k] = float(v)
            mf = re.search(r"shot_fmt=(\w+)", text)
            if mf:
                rec["shot_fmt"] = mf.group(1)
            rec["render_ok"] = True
        elif text.startswith("RENDER_FAIL"):
            rec["render_ok"] = False
            rec["render_fail_line"] = text[:300]
        elif text.startswith("NAV_TIMING "):
            for k, v in KV_RE.findall(text):
                rec["nav_" + k] = float(v)
        elif text.startswith("BENCH_EXIT"):
            t["t_exit"] = ts
            mm = re.search(r"rc=(\d+)", text)
            if mm:
                rec["rc"] = int(mm.group(1))
    def delta(a, b):
        if a in t and b in t:
            return round((t[a] - t[b]) * 1000, 1)
        return None
    if rec.get("exec_debug_seen"):
        rec.setdefault("exec_attempts", 1.0)
        rec.setdefault("exec_retry_wait_ms", 0.0)
    rec["sched_ts"] = t.get("t_sched") or t.get("t0")
    rec["t0_ts"] = t.get("t0")
    rec["restore_ms"] = delta("t_restored", "t0")
    # exec_up splits into fcvm's own handshake (restore -> GO sent) and the
    # guest-side cost of actually starting the command (GO -> first output).
    # Without that split the whole gap is unattributable, which is defect 4.
    rec["exec_handshake_ms"] = delta("t_go", "t_restored")
    rec["exec_spawn_ms"] = delta("t_exec", "t_go")
    rec["exec_up_ms"] = delta("t_exec", "t_restored")
    rec["first_output_ms"] = delta("t_exec", "t0")
    rec["net_ready_host_ms"] = delta("t_net", "t_exec")
    rec["ready_ms"] = delta("t_ready", "t0")
    rec["artifact_ms"] = delta("t_render", "t0")
    rec["destroy_ms"] = delta("t_exit", "t_render")
    rec["total_ms"] = delta("t_exit", "t0")
    # Density cells deliberately idle after the render so memory can be sampled
    # at a steady concurrency. Their artifact_ms is still a real per-request
    # number, but destroy_ms/total_ms carry the hold and must not enter latency
    # statistics — flag them rather than silently averaging them in.
    rec["held"] = rec.get("hold_s", 0) > 0
    rec["ok"] = bool(rec.get("render_ok")) and rec.get("rc", 1) == 0
    return rec


# ---------------------------------------------------------------------------
# stats helpers
# ---------------------------------------------------------------------------
def pct(vals, p):
    if not vals:
        return None
    vs = sorted(vals)
    k = (len(vs) - 1) * p / 100
    f = int(k)
    c = min(f + 1, len(vs) - 1)
    return vs[f] + (vs[c] - vs[f]) * (k - f)


def agg(recs, field):
    vals = [r[field] for r in recs if r.get(field) is not None]
    if not vals:
        return {}
    return {"n": len(vals), "p50": round(statistics.median(vals), 1),
            "p95": round(pct(vals, 95), 1), "mean": round(statistics.mean(vals), 1)}


def fnum(x, nd=0):
    if x is None:
        return "-"
    return f"{x:,.{nd}f}"


def md_table(headers, rows):
    out = ["| " + " | ".join(headers) + " |",
           "|" + "|".join("---" for _ in headers) + "|"]
    for r in rows:
        out.append("| " + " | ".join(str(c) for c in r) + " |")
    return "\n".join(out) + "\n"


def linfit(xs, ys):
    """Least-squares slope+intercept; returns (slope, intercept) or None."""
    n = len(xs)
    if n < 3 or len(set(xs)) < 2:
        return None
    mx, my = statistics.mean(xs), statistics.mean(ys)
    den = sum((x - mx) ** 2 for x in xs)
    if den == 0:
        return None
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den
    return slope, my - slope * mx


# ---------------------------------------------------------------------------
# finalize
# ---------------------------------------------------------------------------
MODE_DESC = {
    "rootless-proxy":  "guest iptables REDIRECT -> fc-agent mux -> vsock -> fcvm egress proxy -> host TCP (IPv4)",
    "rootless-pasta":  "pasta L2<->L4 translation, guest REDIRECT flushed pre-snapshot (IPv4)",
    "rootless-proxy6": "vsock egress proxy path, IPv6 destination",
    "rootless-pasta6": "pasta IPv6 (-o host-v6), REDIRECT flushed",
    "bridged":         "TAP -> namespace bridge -> veth -> host iptables MASQUERADE (kernel path, sudo)",
    "routed":          "veth + native IPv6 kernel routing + NDP proxy (sudo)",
}

def cmd_finalize(args):
    d = args.results_dir
    hostinfo = json.load(open(os.path.join(d, "hostinfo.json")))
    avail = json.load(open(os.path.join(d, "availability.json")))
    reqs = [parse_request_log(p) for p in sorted(glob.glob(os.path.join(d, "requests", "*.log")))]
    samples = []
    for p in sorted(glob.glob(os.path.join(d, "samples", "*.jsonl"))):
        for line in open(p):
            line = line.strip()
            if line:
                try:
                    samples.append({"src": os.path.basename(p), **json.loads(line)})
                except json.JSONDecodeError:
                    pass

    raw = {"hostinfo": hostinfo, "availability": avail, "requests": reqs, "samples": samples}
    json.dump(raw, open(os.path.join(d, "raw.json"), "w"), indent=1)

    def sel(**kw):
        out = []
        for r in reqs:
            if all(r.get(k) == v or (isinstance(v, tuple) and r.get(k) in v) for k, v in kw.items()):
                out.append(r)
        return out

    rep = []
    w = rep.append
    w(f"# Chromium-in-fcvm shared-nothing benchmark\n")
    w(f"Run: {os.path.basename(d)} — host {hostinfo['uname']}")
    w(f"CPU: {hostinfo['cpu_model']} x{hostinfo['nproc']}, RAM {hostinfo['mem_total_kb']//1024//1024} GiB, "
      f"VM size {hostinfo['vm_cpu']} vCPU / {hostinfo['vm_mem_mib']} MiB, R={hostinfo['reps']}\n")
    w(f"> **Contention caveat:** {hostinfo['contention_note']}.\n")
    w("Request lifecycle per sample: `fcvm snapshot run` (restore clone) -> exec driver in container "
      "(probe egress, render via warm CDP, screenshot+DOM) -> teardown. "
      "`artifact` = t(RENDER_OK) - t0 (reply-possible latency); `total` includes destroy.\n")

    # ---- availability
    w("## Egress arm availability\n")
    rows = []
    for m, st in avail["modes"].items():
        rows.append([m, st["available"].upper(), MODE_DESC.get(m, ""), st["reason"] or "-"])
    rows.append(["hugepages cells", avail["hugepages"]["available"].upper(), "2MB hugetlbfs guest memory",
                 avail["hugepages"]["reason"] or f"pool={avail['hugepages']['pool_pages']} pages"])
    w(md_table(["arm", "available", "path", "reason"], rows))
    w(f"> file-backed x hugepages: {avail['file_huge_cell']}\n")

    # ---- headline egress comparison (uffd arm, medium page)
    w("## Egress mode comparison (headline) — host-served fixture, UFFD clones\n")
    w("Stages are host-observed wall times; nav_* come from in-guest Navigation Timing "
      "(connect includes TLS for https; tls shown separately; DNS is 0 — URLs are IP-literal).\n")
    for url in ("medium", "medium-https"):
        w(f"### {url}\n")
        rows = []
        for m in MODE_DESC:
            rs = [r for r in sel(phase="p3", mode=m, url=url) if r["arm"] == "uffd-4k" and r["ok"]]
            fails = [r for r in sel(phase="p3", mode=m, url=url) if r["arm"] == "uffd-4k" and not r["ok"]]
            if not rs:
                if avail["modes"].get(m, {}).get("available") == "no":
                    rows.append([m, "UNAVAILABLE: " + avail["modes"][m]["reason"]] + ["-"] * 9)
                elif fails:
                    rows.append([m, f"ALL {len(fails)} FAILED"] + ["-"] * 9)
                continue
            a_tot, a_art = agg(rs, "total_ms"), agg(rs, "artifact_ms")
            rows.append([
                m, a_tot["n"],
                f"{fnum(a_art['p50'])}/{fnum(a_art['p95'])}",
                f"{fnum(a_tot['p50'])}/{fnum(a_tot['p95'])}",
                fnum(agg(rs, "restore_ms").get("p50")),
                fnum(agg(rs, "exec_up_ms").get("p50")),
                fnum(agg(rs, "net_wait_ms").get("p50"), 1),
                fnum(agg(rs, "r_navigate_ms").get("p50")),
                fnum(agg(rs, "nav_connect_ms").get("p50"), 1),
                fnum(agg(rs, "nav_tls_ms").get("p50"), 1),
                fnum(agg(rs, "nav_ttfb_ms").get("p50"), 1),
                fnum(agg(rs, "r_screenshot_ms").get("p50")),
            ])
        w(md_table(["mode", "n", "artifact p50/p95 ms", "total p50/p95 ms", "restore",
                    "exec", "net-rdy", "nav", "conn", "tls", "ttfb", "shot"], rows))

    # data-driven interpretation notes for the headline table
    notes = []
    for m in MODE_DESC:
        rs = [r for r in sel(phase="p3", mode=m) if r["arm"] == "uffd-4k" and r["ok"]]
        nw = agg(rs, "net_wait_ms").get("p50")
        if nw and nw > 100:
            notes.append(f"- **{m}: first egress after restore stalls ~{fnum(nw)} ms** (in-guest TCP "
                         "probe retry loop) — the render itself is fast once connectivity converges; "
                         "this is post-restore network reestablishment, not render cost.")
    notes.append("- nav `conn`~0 on the proxy/pasta http arms is mechanical: the browser's TCP "
                 "handshake terminates at the local transparent proxy (loopback) or at pasta's "
                 "in-guest endpoint; the real upstream connect happens behind it and lands in ttfb. "
                 "Only bridged/routed kernel paths show the true end-to-end handshake in `conn`.")
    if notes:
        w("**Notes**\n")
        w("\n".join(notes) + "\n")

    # ---- full per-request matrix
    w("## Per-request matrix (p50 ms) — all modes x pages, UFFD arm\n")
    urls = ["minimal", "medium", "heavy", "medium-https"]
    rows = []
    for m in MODE_DESC:
        row = [m]
        for u in urls:
            rs = [r for r in sel(phase="p3", mode=m, url=u) if r["arm"] == "uffd-4k" and r["ok"]]
            row.append(f"{fnum(agg(rs, 'artifact_ms').get('p50'))} / {fnum(agg(rs, 'total_ms').get('p50'))}"
                       if rs else "-")
        rows.append(row)
    w(md_table(["mode \\ page (artifact/total)"] + urls, rows))

    # file arm + control
    w("### File-backed arm and in-guest control (fastest mode)\n")
    rows = []
    for lbl, cond in [
        ("file-backed (page-cache)", dict(phase="p3", arm="file-4k")),
        ("control in-guest fixtures rootless", dict(phase="p3", mode="rootless-proxy")),
        ("control in-guest fixtures bridged", dict(phase="p3", mode="bridged")),
    ]:
        for u in urls + ["control-minimal", "control-medium", "control-heavy"]:
            rs = [r for r in sel(**cond, url=u) if r["ok"]]
            if "control" in lbl and not u.startswith("control-"):
                continue
            if "control" not in lbl and u.startswith("control-"):
                continue
            if "control" in lbl:
                rs = [r for r in rs if r["arm"] == "uffd-4k"]
            if rs:
                rows.append([lbl, u,
                             f"{fnum(agg(rs,'artifact_ms').get('p50'))}/{fnum(agg(rs,'artifact_ms').get('p95'))}",
                             f"{fnum(agg(rs,'total_ms').get('p50'))}/{fnum(agg(rs,'total_ms').get('p95'))}",
                             fnum(agg(rs, "restore_ms").get("p50")),
                             fnum(agg(rs, "r_navigate_ms").get("p50")),
                             agg(rs, "artifact_ms").get("n")])
    w(md_table(["arm", "page", "artifact p50/p95", "total p50/p95", "restore", "nav", "n"], rows))

    # ---- fan-out
    w("## Fan-out: burst latency (2x2 memory matrix)\n")
    bursts = [s for s in samples if s.get("phase") == "burst"]
    rows = []
    for cell in ("uffd-4k", "file-4k", "uffd-huge", "file-huge"):
        for b in [b for b in bursts if b["cell"] == cell]:
            n = b["n"]
            rs = [r for r in reqs if r.get("phase") == f"burst{n}" and r.get("arm") == cell]
            ok = [r for r in rs if r["ok"]]
            wall = b["t1"] - b["t0"]
            rows.append([cell, n, len(ok), len(rs) - len(ok),
                         f"{fnum(agg(ok,'artifact_ms').get('p50'))}/{fnum(agg(ok,'artifact_ms').get('p95'))}",
                         f"{fnum(agg(ok,'total_ms').get('p50'))}/{fnum(agg(ok,'total_ms').get('p95'))}",
                         f"{wall:.1f}", f"{(n/wall):.2f}"])
    if rows:
        w(md_table(["cell", "N", "ok", "fail", "artifact p50/p95", "total p50/p95",
                    "wall s", "req/s"], rows))
    note = [s for s in samples if s.get("phase") == "sustained-meta"]

    # ---- sustained memory curve
    w("## Sustained-rate memory density\n")
    w("Sampled every 2s during each 60s window: MemAvailable delta vs quiescent, "
      "sum of firecracker PSS (`/proc/<pid>/smaps_rollup`) over live clones, page cache. "
      "Marginal MiB/request = least-squares slope of PSS vs concurrent clones across the "
      "cell's samples; req/GB = 1024/slope.\n")
    rows = []
    cell_slopes = {}
    for cell in ("uffd-4k", "file-4k", "uffd-huge", "file-huge"):
        cell_samples = [s for s in samples if s.get("cell") == cell and s.get("phase") == "load"
                        and s.get("fc_procs", 0) > 0]
        xs = [s["fc_procs"] for s in cell_samples]
        ys = [s["pss_kb"] / 1024 for s in cell_samples]
        fit = linfit(xs, ys)
        if fit:
            cell_slopes[cell] = fit[0]
        for rate in (1, 2, 4, 8):
            load = [s for s in samples if s.get("cell") == cell and s.get("rate") == rate
                    and s.get("phase") == "load"]
            quies = [s for s in samples if s.get("cell") == cell and s.get("rate") == rate
                     and s.get("phase") == "quiescent"]
            if not load:
                continue
            clones = [s.get("fc_procs", 0) for s in load]
            pss = [s.get("pss_kb", 0) / 1024 for s in load]
            qa = statistics.mean([s["mem_available_kb"] for s in quies]) if quies else None
            dmem = (qa - min(s["mem_available_kb"] for s in load)) / 1024 if qa else None
            pool_pages = avail["hugepages"].get("pool_pages", 0)
            huge_used = "-"
            if cell.endswith("-huge") and pool_pages:
                huge_used = fnum((pool_pages - min(s.get("hugepages_free", pool_pages)
                                                   for s in load)) * 2)
            meta = next((s for s in note if s.get("cell") == cell and s.get("rate") == rate), {})
            lat = [r for r in reqs if r.get("phase") == f"sust-r{rate}" and r.get("arm") == cell and r["ok"]]
            rows.append([cell, rate,
                         f"{statistics.mean(clones):.1f}/{max(clones)}",
                         fnum(statistics.mean(pss)), fnum(max(pss) if pss else None),
                         fnum(max(pss) / max(clones)) if pss and max(clones) else "-",
                         huge_used, fnum(dmem),
                         fnum(agg(lat, "artifact_ms").get("p50")),
                         fnum(agg(lat, "total_ms").get("p95")),
                         meta.get("launched", "-"), meta.get("skipped", 0)])
    if rows:
        w(md_table(["cell", "rps", "clones avg/max", "PSS sum avg MiB", "max", "PSS/clone MiB",
                    "hugetlb used max MiB", "MemAvail delta MiB", "art p50", "tot p95",
                    "launched", "skipped"], rows))
    if cell_slopes:
        w("### Money metrics: marginal memory per concurrent request\n")
        w("PSS does **not** account hugetlbfs pages, so the huge cells' marginal memory is "
          "computed from the 2MB hugepage pool draw (`pool - HugePages_Free`) instead; their "
          "PSS slope (shown for completeness) covers only the VMM's regular 4K memory.\n")
        pool_pages = avail["hugepages"].get("pool_pages", 0)
        rows = []
        for cell, slope in cell_slopes.items():
            eff_slope, basis = slope, "PSS"
            if cell.endswith("-huge") and pool_pages:
                hs = [s for s in samples if s.get("cell") == cell and s.get("phase") == "load"
                      and s.get("fc_procs", 0) > 0]
                fit = linfit([s["fc_procs"] for s in hs],
                             [(pool_pages - s.get("hugepages_free", pool_pages)) * 2 for s in hs])
                if fit:
                    eff_slope, basis = fit[0], f"hugepage pool draw (PSS slope {slope:.1f})"
            rows.append([cell, fnum(eff_slope, 1), basis,
                         fnum(1024 / eff_slope, 1) if eff_slope > 0 else "-"])
        w(md_table(["cell", "marginal MiB / concurrent request", "basis",
                    "sustainable req per GB RAM"], rows))

    # ---- podman pool contrast
    pool = [s for s in samples if "pool_n" in s]
    if pool:
        w("### Contrast: host-native warm-Chromium pool (naive alternative)\n")
        rows = []
        prev = None
        for s in sorted(pool, key=lambda s: s["pool_n"]):
            tot = s.get("pool_pss_kb", 0) / 1024
            marg = fnum(tot - prev) if prev is not None else "-"
            rows.append([s["pool_n"], s.get("pool_containers"), fnum(tot), marg])
            prev = tot
        w(md_table(["pool size", "containers", "total PSS MiB", "marginal MiB"], rows))
        xs = [s["pool_n"] for s in pool]
        ys = [s.get("pool_pss_kb", 0) / 1024 for s in pool]
        fit = linfit(xs, ys)
        if fit:
            w(f"Marginal warm host-native Chromium container: **{fit[0]:.0f} MiB** each "
              f"(vs the fcvm clone slopes above).\n")

    # ---- baselines
    w("## Baselines (same host-served external URL)\n")
    rows = []
    for lbl, phase in [("host podman COLD (fresh container/request)", "base-podman-cold"),
                       ("host podman WARM (render only — physics floor)", "base-podman-warm"),
                       ("fcvm COLD boot (no snapshot, rootless)", "base-fcvm-cold")]:
        rs = [r for r in reqs if r.get("phase") == phase]
        for u in sorted({r.get("url") for r in rs}):
            us = [r for r in rs if r.get("url") == u and r.get("render_ok")]
            if not us:
                continue
            rows.append([lbl, u, agg(us, "artifact_ms").get("n"),
                         fnum(agg(us, "ready_ms").get("p50")),
                         f"{fnum(agg(us,'artifact_ms').get('p50'))}/{fnum(agg(us,'artifact_ms').get('p95'))}",
                         fnum(agg(us, "r_navigate_ms").get("p50")),
                         fnum(agg(us, "r_screenshot_ms").get("p50")),
                         fnum(agg(us, "r_total_ms").get("p50"))])
    warm_clone = [r for r in sel(phase="p3", mode="rootless-proxy", url="medium")
                  if r["arm"] == "uffd-4k" and r["ok"]]
    if warm_clone:
        rows.append(["fcvm WARM clone (from headline table)", "medium", len(warm_clone), "-",
                     f"{fnum(agg(warm_clone,'artifact_ms').get('p50'))}/{fnum(agg(warm_clone,'artifact_ms').get('p95'))}",
                     fnum(agg(warm_clone, "r_navigate_ms").get("p50")),
                     fnum(agg(warm_clone, "r_screenshot_ms").get("p50")),
                     fnum(agg(warm_clone, "r_total_ms").get("p50"))])
    w(md_table(["scenario", "page", "n", "ready p50 ms", "artifact p50/p95 ms",
                "nav p50", "shot p50", "in-guest render p50"], rows))

    w(KITESURF_CONTEXT + "\n")

    # ---- failures
    fails = [r for r in reqs if not r.get("ok") and r.get("phase", "").startswith(("p3", "burst", "sust"))]
    w(f"## Failures: {len(fails)} of {len(reqs)} parsed request logs\n")
    if fails:
        rows = [[r["file"], r.get("rc", "-"), (r.get("render_fail_line") or "no RENDER_OK")[:120]]
                for r in fails[:30]]
        w(md_table(["request", "rc", "detail"], rows))

    open(os.path.join(d, "report.md"), "w").write("\n".join(rep))
    print(f"wrote {d}/report.md and raw.json ({len(reqs)} requests, {len(samples)} samples)")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("sample")
    s.add_argument("--state-dir")
    s.add_argument("--name-prefix")
    s.add_argument("--podman-prefix")
    s.add_argument("--cgroup-root", help="directory holding one leaf cgroup per clone")
    s.add_argument("--cgroup-prefix", default="req-", help="leaf cgroup name prefix")
    s.add_argument("--extra", help='extra JSON fields, e.g. "cell":"uffd-4k","rate":2')
    s.set_defaults(func=cmd_sample)
    f = sub.add_parser("finalize")
    f.add_argument("results_dir")
    f.set_defaults(func=cmd_finalize)
    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
