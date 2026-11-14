#!/usr/bin/env python3
"""Cloudflare's hosted Browser Run as measured arms: its Chromium and its Kitesurf.

    browserrun.py --out DIR [--engines chromium,kitesurf] [--ops screenshot,content]
                  [--reps 28] [--warmup 1] [--viewport 1280x600]

Cloudflare's published Kitesurf table is the median of five Browser Run Quick
Action runs over the 14-URL corpus, Kitesurf against Browser Run's default
Chromium (developers.cloudflare.com/browser-run/kitesurf/). This drives the same
two Quick Actions, /screenshot and /content, against both engines (`browser=
kitesurf` selects Kitesurf; no parameter is Chromium), over the same 14 URLs
(report.KITESURF_CORPUS), so the report can quote measured hosted numbers.

What it measures. Each request is one HTTPS POST from this host to
api.cloudflare.com, and wall_ms runs from the send to the last byte of the
reply. It includes the network path from this host to the Cloudflare colo that
answered (recorded per request from the cf-ray header), as well as the browser's
work there. --client-location can override the region label, and every record
carries this host's 1-minute load. Browser Run fetches every page from the live site with Cloudflare's
DNS: these arms read the live web at the moment of the request, not the byte
replay the local arms read, because a Quick Action cannot be pointed at another
resolver or proxy. Every reply header whose name mentions "browser" (Cloudflare's
browser-time accounting) is kept on the record, as are the reply's status and
size, so content drift between arms and moments is visible per page.

Correctness. Before the measured reps, each page is rendered once per engine as
a PNG screenshot and once as /content, untimed. Both are kept under
DIR/correctness/<engine>/, the PNG is checked for blankness with render_check's
decoder, and a measured request on a page that did not complete there counts as
a failure, as in the local Kitesurf arm.

Measured requests run one at a time. Rep r renders urls[r % 14]; within a rep
the (engine, op) order is shuffled from a recorded seed. The measured
screenshot is JPEG at quality 80 by default, as cdpdrive takes it for the fcvm
arm. An unauthenticated same-host Cloudflare edge probe is shuffled into each rep; it
measures network/edge drift, without invoking either browser or fetching any
corpus page. A 429 is waited out (Retry-After, capped) and retried; the wait is recorded
and not timed. Any other non-2xx reply, an empty body, a screenshot that is not
an image, or a /content reply without a closed document is a failed request,
and the run goes on.

The dedicated Ohio benchmark worker and primary ARM dev host read their own
Cloudflare Browser Run credential directly from Secrets Manager. Cloudflare
accepts each token only from its own host's public addresses. Both environment
overrides may be supplied for an external trial. The token stays in memory.
"""

import argparse
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import random
import re
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
API = "https://api.cloudflare.com/client/v4/accounts/{account}/browser-run/{op}"
TRACE_URL = "https://api.cloudflare.com/cdn-cgi/trace"
# Reference Page.captureScreenshot dimensions measured on ARM with current headless Chromium.
# --window-size=1280,800 sets the outer window; check the local trial before comparing builds.
DEFAULT_VIEWPORT = (1280, 600)
ENGINE_QUERY = {"chromium": "", "kitesurf": "?browser=kitesurf"}
OPS = ("screenshot", "content")
WORKER_MARKER = Path("/etc/fcvm-benchmark-worker")
ARM_SECRET = "cloudflare-browser-run"
WORKER_SECRET = "cloudflare-browser-run-worker"


def worker_host():
    try:
        info = WORKER_MARKER.stat()
    except FileNotFoundError:
        return False
    if info.st_uid != 0 or info.st_mode & 0o022 or not WORKER_MARKER.is_file():
        raise SystemExit("REFUSING: the FCVM benchmark worker marker must be root-owned and not writable by others")
    return True


def instance_role_env(env):
    # Use IMDS on this host, even if the invoking shell has its own AWS profile or proxy.
    clean = {key: value for key, value in env.items()
             if not key.startswith("AWS_") and not key.lower().endswith("_proxy")}
    return {**clean, "AWS_CONFIG_FILE": "/dev/null", "AWS_SHARED_CREDENTIALS_FILE": "/dev/null",
            "BOTO_CONFIG": "/dev/null", "AWS_EC2_METADATA_V1_DISABLED": "true"}


def credentials(env, worker, run=subprocess.run):
    token = env.get("CLOUDFLARE_API_TOKEN", "").strip()
    account = env.get("CLOUDFLARE_ACCOUNT_ID", "").strip()
    if bool(token) != bool(account):
        raise SystemExit("REFUSING: supply both Cloudflare environment overrides, or neither")
    if not token:
        secret = WORKER_SECRET if worker else ARM_SECRET
        try:
            result = run(
                ["aws", "secretsmanager", "get-secret-value", "--region", "us-west-1",
                 "--secret-id", secret, "--version-stage", "AWSCURRENT",
                 "--query", "SecretString", "--output", "text", "--no-cli-pager"],
                capture_output=True, text=True, timeout=15, check=True, env=instance_role_env(env),
            )
            payload = json.loads(result.stdout)
            token, account = payload["api_token"], payload["account_id"]
        except (OSError, subprocess.SubprocessError, ValueError, TypeError, KeyError):
            raise SystemExit(f"REFUSING: could not load the {secret} credential from Secrets Manager") from None
    if not isinstance(account, str) or not re.fullmatch(r"[0-9a-fA-F]{32}", account) or not isinstance(token, str) \
            or not re.fullmatch(r"[A-Za-z0-9_-]{20,120}", token):
        raise SystemExit("REFUSING: invalid Cloudflare account ID or token format")
    return token, account


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def direct_opener(worker=True):
    """Never forward Authorization to a redirect. On the benchmark worker, connect
    directly: its token is pinned to the worker's own addresses and it has no
    session proxy. Elsewhere (the ARM dev host), keep the HTTPS proxy the
    environment names: it is that session's mandated egress, must not be
    bypassed, and leaves from the addresses the ARM token is pinned to."""
    proxies = {} if worker else urllib.request.getproxies()
    return urllib.request.build_opener(urllib.request.ProxyHandler(proxies), NoRedirect())



def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, f"{name}.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def endpoint(account: str, op: str, engine: str) -> str:
    return API.format(account=account, op=op) + ENGINE_QUERY[engine]


def viewport_options(viewport):
    return {"width": viewport[0], "height": viewport[1], "deviceScaleFactor": 1}


def viewport_size(value):
    match = re.fullmatch(r"([1-9][0-9]{0,3})x([1-9][0-9]{0,3})", value)
    if match is None or any(int(x) > 4096 for x in match.groups()):
        raise argparse.ArgumentTypeError("viewport must be WIDTHxHEIGHT, with dimensions in 1..4096")
    return tuple(map(int, match.groups()))


def request_body(op: str, url: str, image_type: str, viewport) -> dict:
    body = {"url": url, "viewport": viewport_options(viewport)}
    if op == "screenshot" and image_type == "jpeg":
        body["screenshotOptions"] = {"type": "jpeg", "quality": 80}
    return body


def post(url: str, token: str, body: dict, timeout: float, opener):
    """(status, headers, bytes, wall_ms). An HTTP error status is returned, not raised;
    a transport failure is status 0 with the error's text as the body."""
    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method="POST", headers={
        "Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    started = time.monotonic_ns()
    try:
        try:
            with opener.open(req, timeout=timeout) as resp:
                payload = resp.read()
                status, headers = resp.status, dict(resp.headers.items())
        except urllib.error.HTTPError as err:
            with err:
                payload = err.read() or b""
                status, headers = err.code, dict(err.headers.items()) if err.headers else {}
    except (http.client.HTTPException, OSError, ValueError) as err:
        payload, status, headers = f"{type(err).__name__}: {err}".encode(), 0, {}
    if token_bytes := token.encode():
        if 200 <= status < 300 and token_bytes in payload:
            payload, status = b"response echoed a private credential", 0
        else:
            payload = payload.replace(token_bytes, b"[redacted]")
    headers = {key: value.replace(token, "[redacted]") for key, value in headers.items()}
    return status, headers, payload, (time.monotonic_ns() - started) / 1e6


def judge(op: str, image_type: str, status: int, payload: bytes):
    """(ok, detail) of one reply."""
    if status == 0:
        return False, "transport: " + payload.decode("utf-8", "replace")[:300]
    if not 200 <= status < 300:
        return False, f"HTTP {status}: " + payload.decode("utf-8", "replace")[:300]
    if not payload:
        return False, "empty body"
    if op == "screenshot":
        magic = b"\x89PNG" if image_type == "png" else b"\xff\xd8\xff"
        return (True, "") if payload.startswith(magic) else (False, "not an image")
    try:
        doc = json.loads(payload)
    except ValueError:
        return False, "content reply is not JSON"
    html = doc.get("result") if isinstance(doc, dict) else None
    if not isinstance(doc, dict) or doc.get("success") is not True or not isinstance(html, str) \
            or "</html>" not in html.lower():
        return False, "content reply holds no closed document"
    return True, ""


def header(headers: dict, name: str):
    return next((v for k, v in headers.items() if k.lower() == name), None)


def colo(headers: dict):
    m = re.search(r"-([A-Z]{3})$", header(headers, "cf-ray") or "")
    return m.group(1) if m else None


def call(args, opener, token, account, engine, op, url, image_type, sleep):
    """One request, 429s waited out. (status, headers, payload, wall_ms, waited_s)."""
    waited = 0.0
    for _attempt in range(args.max_retries + 1):
        status, headers, payload, wall_ms = post(
            endpoint(account, op, engine), token, request_body(op, url, image_type, args.viewport),
            args.timeout, opener)
        if status != 429:
            break
        try:
            delay = float(header(headers, "retry-after") or 1)
        except ValueError:
            delay = 1.0
        delay = min(max(delay, 0.5), args.max_retry_wait)
        waited += delay
        sleep(delay)
    return status, headers, payload, wall_ms, waited


def loadavg1():
    """This host's 1-minute load at the request: the client box may be shared."""
    try:
        with open("/proc/loadavg") as handle:
            return float(handle.read().split()[0])
    except (OSError, ValueError, IndexError):
        return None


def record(engine, op, url, status, headers, payload, wall_ms, waited, ok, detail):
    return {"engine": engine, "op": op, "url": url, "ok": ok, "detail": detail,
            "loadavg1": loadavg1(),
            "status": status, "wall_ms": round(wall_ms, 1), "bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest() if ok else None,
            "colo": colo(headers),
            "browser_headers": {k: v for k, v in headers.items() if "browser" in k.lower()},
            "rate_limit_wait_s": round(waited, 3), "ts": time.time()}


def drift_probe(opener, timeout):
    """Time a fixed unauthenticated edge response; never retain the trace body or client IP."""
    req = urllib.request.Request(TRACE_URL, headers={"Cache-Control": "no-cache"})
    started = time.monotonic_ns()
    status, location = 0, None
    try:
        with opener.open(req, timeout=min(timeout, 8)) as response:
            response.read(4096)
            status = response.status
            location = colo(dict(response.headers.items()))
    except urllib.error.HTTPError as err:
        with err:
            status = err.code
    except (http.client.HTTPException, OSError, ValueError):
        pass
    return {"status": status, "ok": status == 200, "colo": location, "loadavg1": loadavg1(),
            "wall_ms": round((time.monotonic_ns() - started) / 1e6, 1), "ts": time.time()}


def correctness(args, opener, token, account, engines, urls, sites, sleep):
    """One PNG screenshot and one /content per (engine, page), kept and judged."""
    render_check = load("render_check")
    completed = {}
    for engine in engines:
        out_dir = os.path.join(args.out, "correctness", engine)
        os.makedirs(out_dir, exist_ok=True)
        rows = []
        for url in urls:
            site = sites[url]
            row = {"engine": engine, "url": url, "site": site}
            status, headers, payload, wall_ms, waited = call(
                args, opener, token, account, engine, "screenshot", url, "png", sleep)
            row["shot_ok"], row["shot_error"] = judge("screenshot", "png", status, payload)
            row["shot_status"], row["shot_bytes"], row["shot_colo"] = status, len(payload), colo(headers)
            if row["shot_ok"]:
                with open(os.path.join(out_dir, f"{site}.png"), "wb") as handle:
                    handle.write(payload)
                try:
                    row.update(render_check.colour_stats(payload))
                except Exception as error:  # a check that cannot read the image fails the page
                    row["shot_ok"], row["shot_error"] = False, f"unreadable: {error}"
            status, headers, payload, wall_ms, waited = call(
                args, opener, token, account, engine, "content", url, "png", sleep)
            row["html_ok"], row["html_error"] = judge("content", "png", status, payload)
            row["html_status"], row["html_bytes"] = status, len(payload)
            if row["html_ok"]:
                with open(os.path.join(out_dir, f"{site}.html"), "w") as handle:
                    handle.write(json.loads(payload)["result"])
            row["completed"] = bool(row["shot_ok"] and row["html_ok"] and row.get("blank") is False)
            completed[(engine, url)] = row["completed"]
            rows.append(row)
            print(f"correctness {engine:8} {'ok ' if row['completed'] else 'FAIL'} {site}",
                  file=sys.stderr, flush=True)
        with open(os.path.join(out_dir, "correctness.jsonl"), "w") as handle:
            for row in rows:
                handle.write(json.dumps(row, separators=(",", ":")) + "\n")
    return completed


def run(args, token, account, opener=None, sleep=time.sleep, worker=True) -> int:
    via_proxy = (not worker) and bool(urllib.request.getproxies().get("https"))
    opener = opener or direct_opener(worker)
    report = load("report")
    urls = list(report.KITESURF_CORPUS.values())
    sites = {url: key for key, url in report.KITESURF_CORPUS.items()}
    engines = [e for e in args.engines.split(",") if e]
    ops = [o for o in args.ops.split(",") if o]
    if not engines or any(e not in ENGINE_QUERY for e in engines):
        raise SystemExit(f"REFUSING: --engines must name chromium and/or kitesurf, got {args.engines!r}")
    if not ops or any(o not in OPS for o in ops):
        raise SystemExit(f"REFUSING: --ops must name screenshot and/or content, got {args.ops!r}")
    try:
        os.makedirs(args.out)
    except FileExistsError:
        raise SystemExit(f"REFUSING: output directory {args.out} already exists; choose a new --out") from None
    with open(os.path.join(args.out, "run.json"), "w") as handle:
        json.dump({"engines": engines, "ops": ops, "reps": args.reps, "warmup": args.warmup,
                   "seed": args.seed, "urls": urls, "image_type": args.image_type,
                   "quality": 80 if args.image_type == "jpeg" else None, "api": API,
                   "viewport": viewport_options(args.viewport),
                   "drift_control": {"endpoint": TRACE_URL, "method": "GET",
                                     "scope": "client-to-Cloudflare edge, not browser backend"},
                   "corpus": "live sites, fetched by Browser Run with Cloudflare's DNS",
                   "client": "browserrun.py: one request at a time over HTTPS from this host",
                   "client_location": args.client_location,
                   "via_https_proxy": via_proxy,
                   "loadavg_at_start": loadavg1(),
                   "started_at": time.time()}, handle, indent=1, sort_keys=True)
    completed = correctness(args, opener, token, account, engines, urls, sites, sleep)
    rows_path = os.path.join(args.out, "browserrun.jsonl")
    drift_path = os.path.join(args.out, "drift.jsonl")
    rng = random.Random(args.seed)
    with open(rows_path, "x") as out, open(drift_path, "x") as drift:
        for rep in range(args.warmup + args.reps):
            url = urls[rep % len(urls)]
            order = [(e, o) for e in engines for o in ops] + [(None, None)]
            rng.shuffle(order)
            warmup = rep < args.warmup
            for pos, (engine, op) in enumerate(order):
                if engine is None:
                    trace = drift_probe(opener, args.timeout)
                    trace.update(rep=rep, warmup=warmup, position=pos)
                    drift.write(json.dumps(trace, separators=(",", ":")) + "\n")
                    drift.flush()
                    continue
                status, headers, payload, wall_ms, waited = call(
                    args, opener, token, account, engine, op, url, args.image_type, sleep)
                ok, detail = judge(op, args.image_type, status, payload)
                row = record(engine, op, url, status, headers, payload, wall_ms, waited, ok, detail)
                row.update(rep=rep, warmup=warmup, position=pos,
                           page_completed=completed.get((engine, url), False))
                out.write(json.dumps(row, separators=(",", ":")) + "\n")
                out.flush()
                print(f"{engine:8} {op:10} rep {rep:3} {'ok ' if ok else 'FAIL'} "
                      f"{wall_ms:8.1f} ms {row['colo'] or '-'} {detail[:80]}", file=sys.stderr, flush=True)
    summarize(rows_path, drift_path, os.path.join(args.out, "summary.json"))
    return 0


def summarize(rows_path, drift_path, out_path):
    with open(rows_path) as handle:
        rows = [json.loads(line) for line in handle if line.strip()]
    with open(drift_path) as handle:
        controls = [json.loads(line) for line in handle if line.strip()]
    measured = [row for row in controls if not row["warmup"]]
    valid = [row for row in measured if row["ok"]]
    walls = [row["wall_ms"] for row in valid]
    drift = {"attempts": len(measured), "succeeded": len(valid), "failed": len(measured) - len(valid),
             "wall_ms": {"min": min(walls), "median": statistics.median(walls), "max": max(walls)} if walls else None,
             "colos": sorted({row["colo"] for row in valid if row["colo"]})}
    summary = {"drift_control": drift, "success_rule": "2xx with a well-formed body, and the page completed in the "
                               "correctness pass (not blank, screenshot and content both worked)",
               "p50_convention": "statistics.median over successful measured requests",
               "cells": {}}
    for engine in sorted({r["engine"] for r in rows}):
        for op in sorted({r["op"] for r in rows}):
            cell = [r for r in rows if r["engine"] == engine and r["op"] == op and not r["warmup"]]
            if not cell:
                continue
            good = [r for r in cell if r["ok"] and r["page_completed"]]
            per_page = {}
            for url in sorted({r["url"] for r in cell}):
                mine = [r for r in cell if r["url"] == url]
                walls = [r["wall_ms"] for r in mine if r in good]
                per_page[url] = {
                    "attempts": len(mine), "succeeded": len(walls),
                    "page_completed_in_check": mine[0]["page_completed"],
                    "p50_ms": round(statistics.median(walls), 1) if walls else None,
                    "statuses": sorted({r["status"] for r in mine}),
                    "bytes_min_max": [min(r["bytes"] for r in mine), max(r["bytes"] for r in mine)],
                    "distinct_outputs": len({r["sha256"] for r in mine if r["sha256"]}),
                }
            loads = [r["loadavg1"] for r in cell if isinstance(r.get("loadavg1"), (int, float))]
            summary["cells"][f"{engine}/{op}"] = {
                "loadavg1": ({"min": min(loads), "median": statistics.median(loads), "max": max(loads)}
                             if loads else None),
                "attempts": len(cell), "succeeded": len(good), "failed": len(cell) - len(good),
                "p50_ms": round(statistics.median([r["wall_ms"] for r in good]), 1) if good else None,
                "colos": sorted({r["colo"] for r in cell if r["colo"]}),
                "rate_limit_wait_s": round(sum(r["rate_limit_wait_s"] for r in cell), 3),
                "per_page": per_page,
            }
    with open(out_path + ".tmp", "w") as handle:
        json.dump(summary, handle, indent=1, sort_keys=True)
        handle.write("\n")
    os.replace(out_path + ".tmp", out_path)
    for name, c in summary["cells"].items():
        print(f"{name}: {c['succeeded']}/{c['attempts']} succeeded, p50 {c['p50_ms']} ms, "
              f"colos {c['colos']}", file=sys.stderr)
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True)
    ap.add_argument("--engines", default="chromium,kitesurf")
    ap.add_argument("--ops", default="screenshot,content")
    ap.add_argument("--reps", type=int, default=28)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--seed", type=int, default=20261114)
    ap.add_argument("--image-type", choices=("png", "jpeg"), default="jpeg")
    ap.add_argument("--viewport", type=viewport_size, default=DEFAULT_VIEWPORT,
                    help="match actual local screenshot dimensions (default: %(default)s)")
    ap.add_argument("--timeout", type=float, default=90.0)
    ap.add_argument("--max-retries", type=int, default=5)
    ap.add_argument("--max-retry-wait", type=float, default=30.0)
    ap.add_argument("--client-location",
                    help="override the AWS region label in run.json (never a host name or address)")
    args = ap.parse_args()
    if args.reps < 1 or args.warmup < 0:
        raise SystemExit("REFUSING: --reps must be positive and --warmup nonnegative")
    worker = worker_host()
    token, account = credentials(os.environ, worker)
    if not args.client_location:
        if "CLOUDFLARE_API_TOKEN" in os.environ:
            raise SystemExit("REFUSING: pass --client-location when supplying an external credential")
        args.client_location = "us-east-2" if worker else "us-west-1"
    return run(args, token, account, worker=worker)


if __name__ == "__main__":
    sys.exit(main())
