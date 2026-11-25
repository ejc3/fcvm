#!/usr/bin/env python3
"""Pages mode under the fleet agent, checked on a host: the coordinator's side of the
connection played by this script, and what the module leaves counted as
renders_teardown_check.py counts it.

    renders_agent_check.py --unit NAME --status FILE --teardown FILE --run-dir DIR \\
        --pages N [--engines] [--hold-stages a,b] [--stages a,b,running:TERM] \\
        [--port ADDR:PORT ...] [--snapshot-tag TAG] -- COMMAND...

COMMAND starts `fcvm fleet agent --stdio ... --renders --renders-arg=...` as the systemd
unit NAME with its standard input and output on this process's pipes (`systemd-run --pipe`),
and passes the arguments appended to it on to the agent. The module writes its stage to FILE
and the reaper its report to the teardown FILE. Each case prints one line, and all of them go
to --out; the exit status is 1 when any fails.

  hello            the hello carries `renders` with the pages and the build, and no `renders`
                   report arrives before the first order
  renders          after `run: true`, a report with renders done on each side and no
                   `overlay_seq`
  overlay          after an overlay order (a text and a Chromium-only transform), a report
                   names the order's `seq`, the fcvm side's injected file holds the text and the
                   transform's style, the kitesurf side's the text and not the style
  stop             `run: false` while rendering: nothing is left (the census, with the
                   module's cgroups), the agent is up and answers a further order, and
                   `run: true` renders again
  stop:STAGE       `run: false` while the module is held at STAGE (--hold-stages): nothing is
                   left and the agent is up
  kill:STAGE:SIG   the agent killed by SIG while the module is at STAGE (--stages; held there
                   except at `running`): nothing is left, the unit's cgroup included

The agent's other options get harmless values in renders_bench.sh (`--host`, `--image`,
`--base-port`, and `--sysfs-root` at a missing directory so that it finds no GPU). Run it on
a host that is otherwise idle, as the teardown check.
"""

import argparse
import json
import os
import queue
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import renders_teardown_check as census_of  # noqa: E402

MODULE_CGROUPS = ("renders-fcvm", "renders-engines", "renders-harness")
OVERLAY = {"text": "agent check", "transform": "invert", "chromium_only": True, "seq": 1}
OVERLAY_STYLE = "filter:invert(1)"


def order(run, overlay=None):
    """The coordinator's `renders` message, as one line of the agent protocol."""
    body = {"run": run}
    if overlay:
        body["overlay"] = overlay
    return json.dumps({"type": "renders", "order": body})


def is_report(msg):
    return msg.get("type") == "renders"


def rendered(report, sides):
    """Whether `report` shows renders done on every side named."""
    got = report.get("sides", {})
    return all(got.get(side, {}).get("done", 0) > 0 for side in sides)


def hello_problems(hello, pages):
    """What is wrong with the agent's hello for a module of `pages` pages."""
    if hello.get("type") != "hello":
        return [f"the first message is {hello.get('type')!r}, not the hello"]
    renders = hello.get("renders")
    if not renders:
        return ["the hello carries no renders"]
    problems = []
    if len(renders.get("pages", [])) != pages:
        problems.append(f"{len(renders.get('pages', []))} pages, not {pages}")
    if not all(p.get("id") and p.get("url") for p in renders.get("pages", [])):
        problems.append("a page without an id or a url")
    if not renders.get("cpus"):
        problems.append("no cpus")
    missing = [k for k in ("kernel", "viewport", "vcpus", "page_size") if k not in renders.get("build", {})]
    if missing:
        problems.append(f"the build names no {', '.join(missing)}")
    return problems


def inject_problems(fcvm, kitesurf, overlay=OVERLAY, style=OVERLAY_STYLE):
    """What is wrong with the two sides' injected bytes for a Chromium-only overlay."""
    problems = []
    for side, text in (("fcvm", fcvm), ("kitesurf", kitesurf)):
        if overlay["text"] not in text:
            problems.append(f"the {side} side's file holds no text")
    if style not in fcvm:
        problems.append("the fcvm side's file holds no transform")
    if style in kitesurf:
        problems.append("the kitesurf side's file holds the Chromium-only transform")
    return problems


class Agent:
    """One agent process, its messages read on a thread of their own."""

    def __init__(self, command, log):
        self.proc = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=log, text=True, bufsize=1)
        self.lines = queue.Queue()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        for line in self.proc.stdout:
            line = line.strip()
            if line:
                try:
                    self.lines.put(json.loads(line))
                except ValueError:
                    self.lines.put({"type": "unparsed", "line": line})
        self.lines.put(None)

    def send(self, line):
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def next(self, want, timeout, what):
        """The next message `want` accepts, within `timeout` seconds."""
        until = time.monotonic() + timeout
        while True:
            left = until - time.monotonic()
            if left <= 0:
                raise TimeoutError(f"{what}: nothing within {timeout} s")
            try:
                msg = self.lines.get(timeout=left)
            except queue.Empty:
                continue
            if msg is None:
                raise RuntimeError(f"{what}: the agent's output closed")
            if want(msg):
                return msg

    def quiet(self, reject, seconds):
        """The first message `reject` accepts within `seconds`, or None."""
        until = time.monotonic() + seconds
        while (left := until - time.monotonic()) > 0:
            try:
                msg = self.lines.get(timeout=left)
            except queue.Empty:
                return None
            if msg is None:
                return None
            if reject(msg):
                return msg
        return None

    def drain(self):
        """Drop the messages read so far, so that a wait sees only what comes after."""
        while True:
            try:
                if self.lines.get_nowait() is None:
                    self.lines.put(None)
                    return
            except queue.Empty:
                return

    def alive(self):
        return self.proc.poll() is None

    def close(self, timeout):
        """End the connection (the agent's input), and wait for the agent to exit."""
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return False
        self.reader.join(timeout=5)
        self.proc.stdout.close()
        return True


class Check:
    def __init__(self, opts):
        self.opts = opts
        self.sides = ["fcvm"] + (["kitesurf"] if opts.engines else [])
        self.log = open(opts.log, "a") if opts.log else subprocess.DEVNULL

    def fresh(self):
        for path in (self.opts.status, self.opts.teardown):
            if os.path.exists(path):
                os.unlink(path)

    def start(self, hold=None):
        self.fresh()
        extra = [f"--renders-arg=--hold-at={hold}"] if hold else []
        agent = Agent(self.opts.command + extra, self.log)
        hello = agent.next(lambda m: True, 120, "the hello")
        return agent, hello

    def stage(self, stage):
        def at():
            now = census_of.stage_now(self.opts.status)
            if now.get("stage") == stage:
                return now
            if now.get("stage", "").startswith("stopped"):
                raise RuntimeError(f"the module ended at {now.get('stage')!r} before {stage!r}")
            return None
        return census_of.wait_for(f"stage {stage}", at, self.opts.timeout_s)

    def swept(self):
        """Wait for the reaper's report and the stage after it; returns the report."""
        census_of.wait_for("the reaper's report", lambda: os.path.exists(self.opts.teardown), 180)
        census_of.wait_for(
            "the stopped stage",
            lambda: census_of.stage_now(self.opts.status).get("stage", "").startswith("stopped"),
            60,
        )
        with open(self.opts.teardown) as handle:
            return json.load(handle)

    def unit_cgroup(self):
        return census_of.out(["systemctl", "show", "-p", "ControlGroup", "--value", self.opts.unit]).strip()

    def left(self, before, reaped, cgroups):
        time.sleep(0.5)
        after = census_of.census(self.opts)
        return {k: v for k, v in census_of.leftovers(self.opts, before, after, reaped, cgroups).items() if v}

    def stop_agent(self, agent):
        """End the agent; a module held outside its start's race is ended by stopping the unit."""
        if not agent.close(120):
            subprocess.run(["sudo", "systemctl", "stop", self.opts.unit], check=False)
            agent.proc.wait(timeout=180)
        census_of.wait_for(
            "the unit's end",
            lambda: census_of.unit_state(self.opts.unit) not in ("active", "deactivating", "activating"),
            180,
        )

    def run_case(self, name, body):
        started = time.monotonic()
        try:
            row = body()
        except Exception as error:  # a case that cannot finish is a failed case
            row = {"ok": False, "error": f"{type(error).__name__}: {error}"}
        row = {"case": name, **row, "s": round(time.monotonic() - started, 1)}
        print(json.dumps(row), flush=True)
        return row

    def running_cases(self):
        """hello, renders, overlay and stop, on one agent."""
        rows = []
        before = census_of.census(self.opts)
        agent, hello = self.start()
        try:
            def first():
                problems = hello_problems(hello, self.opts.pages)
                early = agent.quiet(is_report, self.opts.quiet_s)
                if early is not None:
                    problems.append("a renders report before any order")
                return {"ok": not problems, "problems": problems, "build": hello.get("renders", {}).get("build")}
            rows.append(self.run_case("hello", first))

            def renders():
                agent.send(order(True))
                report = agent.next(lambda m: is_report(m) and rendered(m, self.sides),
                                    self.opts.timeout_s, "renders on every side")
                problems = ["overlay_seq set"] if "overlay_seq" in report else []
                done = {s: report["sides"][s]["done"] for s in self.sides}
                return {"ok": not problems, "problems": problems, "done": done}
            rows.append(self.run_case("renders", renders))

            def overlay():
                agent.send(order(True, OVERLAY))
                agent.next(lambda m: is_report(m) and m.get("overlay_seq") == OVERLAY["seq"],
                           60, "a report naming the overlay")
                files = [os.path.join(self.opts.run_dir, f"inject-{side}.html") for side in ("fcvm", "kitesurf")]
                fcvm, kitesurf = (open(f).read() for f in files)
                problems = inject_problems(fcvm, kitesurf)
                return {"ok": not problems, "problems": problems}
            rows.append(self.run_case("overlay", overlay))

            def stop():
                pid = census_of.stage_now(self.opts.status).get("pid")
                unit_cg = self.unit_cgroup()
                if os.path.exists(self.opts.teardown):
                    os.unlink(self.opts.teardown)
                agent.send(order(False, OVERLAY))
                reaped = self.swept()
                left = self.left(before, reaped, [f"{unit_cg}/{c}" for c in MODULE_CGROUPS])
                problems = []
                if not agent.alive() or census_of.unit_state(self.opts.unit) != "active":
                    problems.append("the agent ended")
                try:
                    os.kill(pid, 0)
                except (OSError, TypeError):
                    problems.append(f"the agent's process {pid} is gone")
                agent.drain()
                sent = time.monotonic()
                agent.send(order(False))
                agent.next(is_report, 30, "a report after a further order")
                answered_s = round(time.monotonic() - sent, 1)
                self.fresh()
                agent.drain()
                agent.send(order(True))
                agent.next(lambda m: is_report(m) and rendered(m, self.sides),
                           self.opts.timeout_s, "renders after run again")
                agent.send(order(False))
                again = self.swept()
                if again.get("cgroups_left") or again.get("run_dir_left"):
                    problems.append(f"the second module's sweep: {again}")
                return {"ok": not problems and not left, "problems": problems, "left": left,
                        "answered_s": answered_s}
            rows.append(self.run_case("stop", stop))
        finally:
            self.stop_agent(agent)
        return rows

    def held_stop(self, stage):
        before = census_of.census(self.opts)
        agent, _hello = self.start(hold=stage)
        try:
            agent.send(order(True))
            self.stage(stage)
            unit_cg = self.unit_cgroup()
            agent.send(order(False))
            reaped = self.swept()
            left = self.left(before, reaped, [f"{unit_cg}/{c}" for c in MODULE_CGROUPS])
            up = agent.alive() and census_of.unit_state(self.opts.unit) == "active"
            return {"ok": up and not left, "agent_up": up, "left": left}
        finally:
            self.stop_agent(agent)

    def kill(self, stage, signame):
        before = census_of.census(self.opts)
        agent, _hello = self.start(hold=None if stage in census_of.UNHELD else stage)
        try:
            agent.send(order(True))
            reached = self.stage(stage)
            unit_cg = self.unit_cgroup()
            if stage in census_of.UNHELD and self.opts.after_s:
                time.sleep(self.opts.after_s)
            os.kill(reached["pid"], getattr(signal, f"SIG{signame}"))
            agent.proc.wait(timeout=180)
            census_of.wait_for(
                "the unit's end",
                lambda: census_of.unit_state(self.opts.unit) not in ("active", "deactivating", "activating"),
                180,
            )
            census_of.wait_for("the reaper's report", lambda: os.path.exists(self.opts.teardown), 180)
            with open(self.opts.teardown) as handle:
                reaped = json.load(handle)
            left = self.left(before, reaped, [unit_cg] if unit_cg else [])
            return {"ok": not left, "left": left}
        finally:
            if agent.alive():
                self.stop_agent(agent)


def parse(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--unit", required=True)
    p.add_argument("--status", required=True)
    p.add_argument("--teardown", required=True)
    p.add_argument("--run-dir", required=True)
    p.add_argument("--pages", type=int, required=True, help="pages the module renders")
    p.add_argument("--engines", action="store_true", help="the module runs the kitesurf side")
    p.add_argument("--cases", default="running,held,kill", help="comma separated: running, held, kill")
    p.add_argument("--hold-stages", default="replays,serve,engines-starting")
    p.add_argument("--stages", default="start,cgroups,replays,serve-starting,serve,engines-starting,"
                                       "engines,workers,running,running:TERM")
    p.add_argument("--port", action="append", default=[], metavar="ADDR:PORT")
    p.add_argument("--state-dir", default="")
    p.add_argument("--vm-dir", default="")
    p.add_argument("--data-dir", default="")
    p.add_argument("--snapshot-tag", default="")
    p.add_argument("--after-s", type=float, default=20.0, help="seconds of rendering before a kill at 'running'")
    p.add_argument("--quiet-s", type=float, default=8.0, help="seconds without a report before the first order")
    p.add_argument("--timeout-s", type=float, default=1800.0)
    p.add_argument("--log", default="", help="the agent's standard error")
    p.add_argument("--out", default="")
    p.add_argument("command", nargs=argparse.REMAINDER)
    opts = p.parse_args(argv)
    opts.command = opts.command[1:] if opts.command[:1] == ["--"] else opts.command
    if not opts.command:
        p.error("no command to start the agent")
    unknown = set(opts.cases.split(",")) - {"running", "held", "kill"}
    if unknown:
        p.error(f"no case {', '.join(sorted(unknown))}")
    return opts


def main(argv=None):
    opts = parse(argv)
    check = Check(opts)
    cases = opts.cases.split(",")
    rows = []
    if "running" in cases:
        rows += check.running_cases()
    if "held" in cases:
        for stage in filter(None, opts.hold_stages.split(",")):
            rows.append(check.run_case(f"stop:{stage}", lambda s=stage: check.held_stop(s)))
    if "kill" in cases:
        for entry in filter(None, opts.stages.split(",")):
            stage, _, signame = entry.partition(":")
            signame = signame or "KILL"
            rows.append(check.run_case(f"kill:{stage}:{signame}", lambda s=stage, g=signame: check.kill(s, g)))
    failed = [r["case"] for r in rows if not r.get("ok")]
    verdict = {"cases": len(rows), "failed": failed}
    print(json.dumps(verdict), flush=True)
    if opts.out:
        with open(opts.out, "w") as handle:
            json.dump({"rows": rows, **verdict}, handle, indent=1)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
