import GateVerify.Spec

/-!
# The code as it is and as it was: five rule books

Each is one commit of ejc3/fcvm, and each field cites the lines that make it so. `sn.rs` is
fc-agent/src/snapshot_network.rs. A rule book is a fixed commit and is named for what the
boundary does there, not for a branch: `main` has moved through four of them
(`verify/GateVerify/README.md`, "The rule books").

| rule book | commit | what it is |
|---|---|---|
| `Today.ctstate` | 4adc1dee6 | `main` before #2113: the gate's TCP rule is `--ctstate NEW` |
| `Today.flags` | a1e5fc687 | PR #2113, `gate-syn-flags` |
| `Today.cookieA` | a80e1ad34 | PR #2124, its first commit, the one that was reviewed |
| `Today.cookieB` | 27add63dd | PR #2124, its head, and `main` from #2124 (57aa83bb6) until #2154 landed |
| `Today.owed` | da5f2bb22 and #2256 | `main` with #2151, #2154 and the guard of #2256: the boundary is owed a resume until the host's or the guard's opens it |

What the first four share, cited at `main` 57aa83bb6 (the same code is in the other three, at
other lines; `owed` changes the host's reopen, the resume, the watchdog and the restarted agent,
and its docstring says how):

- `snapshot create` on the host: the prepare one-shot, pause, resume, the resume one-shot, and
  the resume one-shot alone after a prepare that failed (src/commands/common.rs:6568-6622,
  6021). No check of the guest's restore state before the prepare (`snapUnpublished`).
- The prepare one-shot: freeze, then `prepare_with` under the boundary lock: close, barrier,
  dump, save (fc-agent/src/main.rs:108-118; sn.rs:2331-2393), and `rollback_preparation` on a
  failure (sn.rs:2322-2329).
- The resume one-shot: `reopen_with`, then `store.remove()`, whatever the reopen returned, then
  thaw (sn.rs:2779-2806; main.rs:119-126). A reopen that fails is reported in the exit status
  and nothing tries it again (`reopenRetry := false`). Nothing undoes what a dead
  `snapshot create` left (`guard := false`; #1936, open).
- The gate's chains (`gate_chain_rules`): in INPUT, TCP from a loopback source returns, anything
  that entered on `lo` returns, the TCP rule, return; in OUTPUT, TCP to a loopback destination
  returns, NTP is rejected, the TCP rule, return. A clone inserts its UDP rule third in INPUT
  (`udp_ingress_rule_args`, `-m conntrack --ctdir ORIGINAL`).
- The egress proxy's redirect is in nat OUTPUT and nothing of the gate is (`natGate := false`;
  fc-agent/src/proxy.rs:763-822).
- Both one-shots are exec'd through the long-running fc-agent (src/commands/common.rs:6021).
  When that agent has died, systemd has killed the unit's cgroup with it (the container, and a
  one-shot in progress) and starts a new one (`Restart=on-failure`, `Delegate=yes`, no
  `KillMode`, src/setup/rootfs.rs:1610-1626). The new process loops on its boot plan before it
  starts the exec server or the restore watcher (fc-agent/src/agent.rs:74-86, 215). A guest
  restored from a memory image is served no plan, so its agent never comes back and the
  restore is never run a second time; the agent of a guest that booted without the snapshot
  cache comes back unless the fetch is over MMDS behind the closed egress half
  (`Spec.restart`). Its start of the container is then refused and it powers the VM off
  (`Spec.boot`, `restartKeeps := false`; #2158, observed in VMs). When no agent answers,
  the host's one exec of the resume one-shot gives up after its 41 connect attempts, about
  54 s (src/commands/exec.rs), and `snapshot create` returns its error. Read from the source;
  not run.
- A write of `tcp_syncookies` back that fails is tried three times and then the restore goes on
  with a warning (`restore_syn_cookies`; `backFails := false`).
- The receive barrier turns `/sys/kernel/rcu_expedited` on and off around its grace period
  (sn.rs:486-529), and nothing else ever writes it back (`rcuUndo := false`).
- The payload-freeze watchdog thaws any freeze it has seen 60 times, a second apart, whatever
  the restore is doing (fc-agent/src/payload_freeze.rs:87, 419; `wdMidBoundary := true`).
-/

namespace Gate

namespace Today

/-- `origin/main`, 4adc1dee6.

- `tcpByCt`: the TCP rule is `-p tcp -m conntrack --ctstate NEW -j REJECT --reject-with
  tcp-reset` (sn.rs:943-954, in `gate_chain_rules`, sn.rs:912-957).
- `manAddr`: the manifest is the dump less every socket `is_preserved` (sn.rs:2075-2086), which
  is a loopback peer, IPv4-mapped included (sn.rs:117-124).
- `list := none`: the restore retires the manifest's sockets and takes no list of its own
  (sn.rs:2248-2290).
- `restore` (`restore_boundary_with`, sn.rs:2142-2313): `is_closed` and `close` (2188, 2202),
  `gate_udp_ingress` (2208), the barrier (2218), `open_egress` (2243), `store.load()` (2248),
  `destroy` (2264), `store.remove()` (2291); then, in `handle_clone_restore`,
  `payload_freeze::thaw` (fc-agent/src/restore.rs:792) and `publish` (restore.rs:814), which is
  the hook (sn.rs:2369) and `reopen_with` (sn.rs:2374). -/
def ctstate : Rules where
  name := "ctstate"
  tcpByCt := true
  manAddr := true
  list := .none
  look := false
  restore := [.verify, .udp, .barIn, .barOut, .egress, .load, .retire, .disarm, .thaw, .hook,
              .ingress]
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  restartKeeps := false
  startWaits := false
  execFirst := false
  guard := false
  rcuUndo := false

/-- PR #2113, a1e5fc687. One change against `ctstate`:

- `tcpByCt := false`: the TCP rule is `TCP_NEW_FLOW_REJECT`, `-p tcp -m tcp --tcp-flags SYN,ACK
  SYN -j REJECT --reject-with tcp-reset` (sn.rs:990-1002, used at sn.rs:942).

The manifest (sn.rs:2142-2153, `is_preserved` at sn.rs:117-124) and the restore's order
(sn.rs:2209-2380: 2255, 2269, 2275, 2285, 2310, 2315, 2331, 2358; restore.rs:795, 818;
sn.rs:2436, 2441) are `ctstate`'s. -/
def flags : Rules where
  name := "flags"
  tcpByCt := false
  manAddr := true
  list := .none
  look := false
  restore := ctstate.restore
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  restartKeeps := false
  startWaits := false
  execFirst := false
  guard := false
  rcuUndo := false

/-- PR #2124 at its first commit, a80e1ad34.

- `restore` (`restore_boundary_with`, sn.rs:2376-2575): `refuse_syn_cookies` first (2423), then
  `is_closed` and `close` (2426, 2440), `gate_udp_ingress` (2446), the barrier (2456),
  `store.load()` (2474), the dump and `unlisted_sockets` (2475), `open_egress` (2493),
  `destroy` (2515), `store.remove()` (2547); thaw (restore.rs:795); the hook (sn.rs:2626); and
  `open` (sn.rs:1686-1692), which writes `tcp_syncookies` back BEFORE it removes the gate.
- `list := addr`: `unlisted_sockets` (sn.rs:1856-1880) skips a socket the manifest names or
  whose peer is loopback (1871), and one whose mirror image is in the same list (1874-1876).
- `look := false`: the list is one `dump_tcp_sockets`, which is the IPv4 dump and then the IPv6
  dump.
- `manAddr`: the manifest still leaves out by address (sn.rs:2314). -/
def cookieA : Rules where
  name := "cookieA"
  tcpByCt := false
  manAddr := true
  list := .addr
  look := false
  restore := [.cookiesOff, .verify, .udp, .barIn, .barOut, .load, .dump4, .dump6, .egress,
              .retire, .disarm, .thaw, .hook, .cookiesBack, .ingress]
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  restartKeeps := false
  startWaits := false
  execFirst := false
  guard := false
  rcuUndo := false

/-- PR #2124 at its head, 27add63dd, and `main` since it landed: cited at `main` 57aa83bb6. The
merge of `main` into #2124 (26b7de0ed, the restore hook's phases of #2088) changed nothing in
the functions cited from sn.rs, which are line for line where 27add63dd has them.

- `restore`, in the order `handle_clone_restore` (restore.rs:388) runs it. First
  `restore_snapshot_network` (restore.rs:566), which is `restore_boundary_with`
  (sn.rs:2432-2621): `refuse_syn_cookies` (2479), `is_closed` and `close` (2482, 2496),
  `gate_udp_ingress` (2502), the barrier (2512), the routed clone's move to its own IPv6 address
  (`before_egress`, 2522; restore.rs:539), `store.load()` (2530), `unlisted_sockets` (2531),
  `open_egress` (2545), `destroy` (2567), `store.remove()` (2599). Then, back in
  `handle_clone_restore`: the clock step (restore.rs:593), journald's restart (616), the NFS
  remount (624-725), the waits for the exec server and the egress proxy (747, 759), and the
  thaw (792). Then `publish` (restore.rs:814; sn.rs:3612, `publish_with` at 2672): the hook and
  the retired addresses' reconcile after it (2678), and `open` (sn.rs:1684-1694), which removes
  the gate and then writes `tcp_syncookies` back (1692). Last, chronyd's start (restore.rs:817)
  and the acknowledgement (`restore_status.succeed`, mmds.rs:477).
- The steps of that order the rule book leaves out touch nothing a property reads, with one
  exception: the address move, which is why a retired socket's reset does not always reach its
  other end in a routed clone (`Limits.lean`). Left out: the address move, the clock, journald,
  NFS, the two waits, the reconcile, chronyd, the acknowledgement (the model's `advance` to
  `done` after the last step).
- `manAddr := false`: `manifest_sockets` (sn.rs:1902-1907) leaves a socket out only when its
  peer is loopback and its mirror image is in the list (`sockets_leaving_the_guest`,
  sn.rs:1854-1883).
- `list := mirror`: `unlisted_sockets` (sn.rs:1934-1947) keeps a socket the manifest does not
  name only when its mirror image is in the list.
- `look`: when a socket that may stay has no mirror image, the sockets are dumped once more
  (sn.rs:1872-1881). -/
def cookieB : Rules where
  name := "cookieB"
  tcpByCt := false
  manAddr := false
  list := .mirror
  look := true
  restore := [.cookiesOff, .verify, .udp, .barIn, .barOut, .load, .dump4, .dump6, .look,
              .egress, .retire, .disarm, .thaw, .hook, .ingress, .cookiesBack]
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  restartKeeps := false
  startWaits := false
  execFirst := false
  guard := false
  rcuUndo := false

/-- `main` at da5f2bb22, which has #2151 and #2154 on top of `cookieB`, with #2256's guard of a
dead `snapshot create`: the source's boundary is owed a resume until the host's resume, or the
guard's, opens it. Named for that (`snapshot-boundary-owed`).
Cited at da5f2bb22. `sn.rs` is fc-agent/src/snapshot_network.rs, `common.rs`
src/commands/common.rs.

The restore is `cookieB`'s, step for step: `restore_boundary_with` (sn.rs:3212), `publish_with`
(sn.rs:3452), `manifest_sockets` (2672), `unlisted_sockets` (2706) and `gate_chain_rules` (1736)
are line for line 57aa83bb6's, and `handle_clone_restore` (fc-agent/src/restore.rs:388) differs
only by a failpoint (`restore.pre_boundary`, restore.rs:182). The order fc-agent's tests pin is
the same 16 sequences, so `gate-seq check` cannot tell this rule book from `cookieB`; the
differences are the fields below. `sockets_leaving_the_guest` (sn.rs:2606) adds #2152's
reserved source ports for a `/loopback` port's clients, which the model's kinds cannot express
(`Limits.lean`).

- `snapUnpublished := false`: `snapshot create` waits for, and then refuses, a VM whose restore
  has not been acknowledged (`await_restore_acknowledged`, src/commands/snapshot.rs:1502, 1577;
  `ensure_restore_acknowledged`, common.rs:6116, #2151). In the guest a restore's dispatch
  writes `restore-under-way` (`begin_restore_boundary`, sn.rs:772, from fc-agent/src/mmds.rs:415)
  and both one-shots refuse with exit 3 while it is there, before and again under the boundary
  lock (`prepare_snapshot_network_in`, sn.rs:3533; `resume_source_network_in`, sn.rs:3622).
- `reopenRetry := true`: the host runs the resume one-shot until it opens the gate or 150 s have
  passed (`reopen_source_network`, common.rs:7070; `SOURCE_REOPEN_BOUND`, common.rs:7004), with
  `network_gate_closed` written on the VM's state before the first ask and cleared by the try
  that opens it (`record_source_gate`, common.rs:7087). A source still gated then is left
  running, the create fails naming it, and the next create of it asks once and is refused while
  the gate stays closed (`refuse_a_source_left_gated`, common.rs:7128, the first step of
  `create_snapshot_core`, common.rs:7656). The model's `retryMax` tries stand for the 150 s.
- `rcuUndo := true`: the resume one-shot writes `rcu_expedited` back from its record before it
  tries the gate (`ExpeditedGracePeriods::put_back`, sn.rs:3691). The agent's start does the same
  (fc-agent/src/main.rs:261), which the model does not hold.
- `wdMidBoundary := false`: the watchdog does not count a look while the boundary lock is held or
  a restore is under way (`boundary_in_progress`, sn.rs:881; fc-agent/src/payload_freeze.rs:425),
  nor 150 more while a source's boundary is owed and its gate installed (`source_gate_is_owed`,
  sn.rs:806; `SOURCE_REOPEN_LOOKS`, payload_freeze.rs:389, 426). The model's switch `Cfg.slow`
  does not count looks; it leaves the freeze alone while a clone has not published or a prepare
  is in progress, as `Design.rules` does.
- `startWaits := true`: a restarted agent holds its container's start while a restore is under
  way, or the `owed` mark is there and the gate installed
  (`hold_the_container_start_through_a_boundary`, sn.rs:665, called at fc-agent/src/agent.rs:702,
  after the boot plan), and gives up after 300 looks that find nobody at work on the boundary.
  The model does not count those looks.
- `execFirst := true`: an agent that starts and finds the `owed` mark opens exec before its boot
  plan, for the resume one-shot alone (fc-agent/src/agent.rs:104-128;
  `exec::serve_only_the_boundary_resume`, fc-agent/src/exec.rs:404).
- `restartKeeps := false`: #2158 is open; the restarted agent's start of the container is still
  refused and it powers the VM off, after the hold.
- `guard := true`: #2256 (ejc3/fcvm acf8b6259, which lands with or before this rule book). A
  create whose source keeps running spawns a guard that holds the per-VM snapshot lock and, when
  the create dies without standing it down, resumes the VMM if the pause was asked for and runs
  the create's own reopen (`reopen_source_network`, its 150 s bound, the gate recorded closed
  until it opens): src/commands/snapshot_guard.rs:321 (`cmd_snapshot_source_guard`), 457
  (`undo`); spawned at src/commands/common.rs:7678 and 8622. A prepare the guard's reopen
  overtook refuses in the guest (the cancelled record, `prepare_snapshot_network_in`). The model
  has one `snapshot create` at a time, so that race is outside it.
- `natGate`, `backFails`: unchanged from `cookieB` (repairs 5 and 7 have no pull request).

Not in the model: the boundary's token (#2159): the host gives each create a token
(`run_snapshot_network_command`, common.rs:6732), the prepare writes it into the mark
(`mark_owed`), and the guest runs a resume only when it carries the mark's token
(`resume_is_for_the_owed_boundary`, sn.rs:374), refusing any other with exit 5. The model holds
one `snapshot create` at a time, so no resume of an earlier create exists in it. Nor a create
failing when the agent that served its prepare died before its resume
(`note_the_agent_restarted_in_a_boundary`, sn.rs:489; `resume_paused_source`, common.rs:6867):
the model has no property about what the image holds of the agent. -/
def owed : Rules where
  name := "owed"
  tcpByCt := false
  manAddr := false
  list := .mirror
  look := true
  restore := cookieB.restore
  natGate := false
  wdMidBoundary := false
  snapUnpublished := false
  reopenRetry := true
  backFails := false
  restartKeeps := false
  startWaits := true
  execFirst := true
  guard := true
  rcuUndo := true

/-- 27add63dd with its second dump taken out: what its red run "one dump" removes. Not a state
of the code; it shows what the second dump is for. -/
def cookieBOneLook : Rules :=
  { cookieB with
    name := "cookieB-one-dump"
    look := false
    restore := cookieB.restore.filter (· != .look) }

/-- `main` with #2158 fixed and nothing else: a restarted fc-agent gets a container running
again. Not a state of the code. It shows what the fix of #2158 uncovers on Cloud Hypervisor
unless repairs 3 and 8 come with it: today those runs end in the power-off. -/
def cookieBKept : Rules :=
  { cookieB with
    name := "cookieB-restart-kept"
    restartKeeps := true }

end Today

end Gate
