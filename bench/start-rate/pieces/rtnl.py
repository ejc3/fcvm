#!/usr/bin/env python3
"""What a traced cell held rtnl_mutex for, per op: rtnl.py CELL.bt OPS

CELL.bt is a kernel.bt dump (attribution.sh writes one per cell). OPS is the number of ops
whose whole life the tracer saw: for rootless-net at N=1000 with 1000 held that is 2000,
because the held namespaces are made and destroyed inside the window too.
"""
import collections
import json
import re
import sys


def maps(text, name):
    """@name[key]: value lines as {key: value}; a key that is a stack spans lines."""
    found = {}
    for m in re.finditer(rf"^@{name}\[(.*?)\]: (\d+)$", text, re.M | re.S):
        if "\n@" not in m.group(1):
            found[m.group(1)] = int(m.group(2))
    return found


def scalar(text, name):
    m = re.search(rf"^@{name}: (\d+)$", text, re.M)
    return int(m.group(1)) if m else 0


SAMPLE_HZ = 1999  # kernel.bt's profile rate
# Frames that walk something host-wide under the mutex, so their cost grows with what is alive:
# every network namespace, every rtnetlink multicast listener, every CPU's RCU callbacks.
WALKS = ("wireless_nlevent_flush", "do_one_broadcast", "rcu_barrier")


def on_cpu(text, ops):
    """ms per op on CPU under the mutex: all samples, and those with a WALKS frame in the stack."""
    total = 0
    walks = collections.Counter()
    for m in re.finditer(r"^@rtnl_oncpu\[\n(.*?)\n\]: (\d+)$", text, re.M | re.S):
        frames = [re.sub(r"\+\d+$", "", frame.strip()) for frame in m.group(1).splitlines()]
        n = int(m.group(2))
        total += n
        for walk in WALKS:
            if walk in frames:
                walks[walk] += n
                break
    to_ms = lambda n: round(n / SAMPLE_HZ / ops * 1000, 3)
    return {"all": to_ms(total), **{walk: to_ms(walks[walk]) for walk in WALKS}}


def holder(name):
    """One name per kind of holder: pasta's comm carries its content hash, a symbol its offset."""
    name = re.sub(r"\+\d+$", "", name.strip())
    return "pasta" if name.startswith("pasta") else name


def summarize(text, ops):
    hold = collections.Counter()
    holds = collections.Counter()
    for key, us in maps(text, "rtnl_hold_us_by_work").items():
        hold["worker " + holder(key)] += us
    for key, n in maps(text, "rtnl_holds_by_work").items():
        holds["worker " + holder(key)] += n
    for key, us in maps(text, "rtnl_hold_us_by_comm").items():
        hold[holder(key)] += us
    for key, n in maps(text, "rtnl_holds").items():
        holds[holder(key)] += n
    total = scalar(text, "rtnl_hold_us_total")
    if not total:
        raise SystemExit("the dump holds no rtnl_hold_us_total: the tracer did not run or was cut off")
    # Both ends of every acquisition are counted. A thread or two can be between the two when
    # the tracer stops, and the kernel skips a run of an fentry or fexit program that is
    # already running on the same CPU (a task preempted inside it): 14 of 94,849 requests in
    # one cell at C=128 on 192 CPUs. A hold whose end was skipped is not summed, so the sums
    # are short by at most the share that differs; over 0.5% the dump is refused.
    requests = scalar(text, "rtnl_requests")
    acquired = scalar(text, "rtnl_acquired") + scalar(text, "rtnl_interrupted")
    if not requests or abs(requests - acquired) > max(2, 0.005 * requests):
        raise SystemExit(f"the dump counts {requests} requests for rtnl_mutex and {acquired} acquisitions: "
                         "the tracer dropped events, or the dump is from one that could")
    barrier = scalar(text, "rcu_barrier_us_under_rtnl")
    expedited = collections.Counter()
    for key, n in maps(text, "rcu_exp").items():
        expedited["worker" if key.startswith("kworker") else holder(key)] += n
    return {
        "ops": ops,
        "hold_ms_per_op": round(total / ops / 1000, 3),
        "hold_ms_per_op_without_rcu_barrier": round((total - barrier) / ops / 1000, 3),
        "rcu_barrier_ms_per_op": round(barrier / ops / 1000, 3),
        "rcu_barriers_per_op": round(scalar(text, "rcu_barriers_under_rtnl") / ops, 2),
        "wait_ms_per_op": round(scalar(text, "rtnl_wait_us_total") / ops / 1000, 1),
        "by_holder": {
            name: {"hold_ms_per_op": round(us / ops / 1000, 3), "holds_per_op": round(holds[name] / ops, 2)}
            for name, us in hold.most_common()
            if us / ops >= 1
        },
        "on_cpu_ms_per_op": on_cpu(text, ops),
        "expedited_grace_periods_per_op": {name: round(n / ops, 2) for name, n in expedited.most_common() if n / ops >= 0.01},
    }


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    print(json.dumps(summarize(open(sys.argv[1]).read(), float(sys.argv[2])), indent=1))
