/-!
# The snapshot and restore network boundary: the state of one guest, and one step

A small-step model of the boundary fc-agent keeps around a memory snapshot
(`fc-agent/src/snapshot_network.rs`, `restore.rs`, `network.rs`, `proxy.rs`), with the guest
kernel's TCP, connection tracking and netfilter as far as the boundary depends on them, and
the host's steps around it (`src/commands/common.rs`).

One guest is followed at a time. A snapshot is taken of it (`Label.snap`), and at the pause the
run either stays with the source (`snap` again: the VMM resumes it) or moves to a clone restored
from the image (`Label.restore`): the same memory, so the same sockets, tracked connections,
rules, settings and files under `/run`. A published clone is a source for the next generation.

The state is plain data and `step` applies one labelled transition, or refuses it. What differs
between the code states is a field of `Rules`, filled in by `Today.lean` (each rule citing file
and line at a named commit) and `Design.lean`. The kernel is the same for every rule book and
is cited here, at Linux v6.18.60:

- `tcp_input.c`, `tcp_ipv4.c`, `syncookies.c`, `tcp_minisocks.c`, `inet_connection_sock.c`,
  `tcp_diag.c`, `tcp.c` are under net/ipv4/;
- `ct_tcp.c` is net/netfilter/nf_conntrack_proto_tcp.c, `ct_core.c` is
  net/netfilter/nf_conntrack_core.c, `xt_conntrack.c` is net/netfilter/xt_conntrack.c;
- `netfilter_ipv4.h` is include/uapi/linux/netfilter_ipv4.h.
-/

namespace Gate

/-! ## What exists around the guest -/

/-- The two address families. The gate is one set of chains in each (iptables, ip6tables). -/
inductive Fam where
  | v4 | v6
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- A client outside the guest: one address and source port, so one four-tuple a listener.
`a` speaks IPv4 and `b` IPv6. A client sends any segment at any time. -/
inductive Cli where
  | a | b
  deriving DecidableEq, Repr, Hashable, Inhabited

def Cli.fam : Cli → Fam
  | .a => .v4
  | .b => .v6

/-- The guest's two listeners. `plain` is reached with the client's own address as peer.
`lb` is a port published with `/loopback`: the guest's nat rules rewrite the client's source to
127.0.0.1 (`loopback_client_snat_args`, network.rs), IPv4 only, so its sockets record a loopback
peer for a client that is on the host. -/
inductive Port where
  | plain | lb
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Who runs in the guest: the container, which the boundary freezes (`payload_freeze`), and any
process outside it (a system daemon, an `fcvm exec --vm` command), which it does not. -/
inductive Who where
  | ctr | dmn
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- What a TCP socket is. At most one socket of a kind exists at a time, as at most one socket
has a given four-tuple (the kernel's established hash). -/
inductive Kind where
  /-- the listener's socket for client `c` on port `p` -/
  | inb (c : Cli) (p : Port)
  /-- a process's connection to a server outside the guest, no egress proxy -/
  | outb (w : Who)
  /-- with the egress proxy: the process's socket. It records the destination it asked for -/
  | proxC (w : Who)
  /-- with the egress proxy: the proxy's accepted socket. It records the guest's address as peer -/
  | proxS (w : Who)
  /-- a loopback connection inside the guest to a dual-stack listener: the IPv4 client end -/
  | loopC
  /-- its server end, an IPv6 socket whose peer is the IPv4-mapped loopback address -/
  | loopS
  /-- a connection inside the guest to a loopback listener from a socket bound to the guest's own
  address: the client end, whose peer is loopback -/
  | selfC
  /-- its server end, an IPv6 socket whose peer is the guest's (IPv4-mapped) address -/
  | selfS
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- The connection leaves the guest: the socket's other end is outside it, or the socket is one
of the egress proxy's two, which are each other's peer and carry a connection to a server
outside through the relay behind the proxy (vsock, not TCP). -/
def Kind.outside : Kind → Bool
  | .inb _ _ | .outb _ | .proxC _ | .proxS _ => true
  | _ => false

/-- An end of a loopback connection inside the guest: both addresses loopback. -/
def Kind.loop : Kind → Bool
  | .loopC | .loopS => true
  | _ => false

/-- The socket records a loopback address as its peer: what `is_preserved` reads. -/
def Kind.peerLo : Kind → Bool
  | .inb _ .lb | .loopC | .loopS | .selfC => true
  | _ => false

/-- The kind of the socket with this one's two ends swapped (`TcpSocketIdentity::ends`), if a
connection of this kind has both ends in the guest. -/
def Kind.mirror : Kind → Option Kind
  | .loopC => some .loopS
  | .loopS => some .loopC
  | .selfC => some .selfS
  | .selfS => some .selfC
  | _ => none

/-- Which of the two `SOCK_DIAG` dumps shows the socket: the family it was opened in. -/
def Kind.fam : Kind → Fam
  | .inb c _ => c.fam
  | .loopS | .selfS => .v6
  | _ => .v4

/-- A fixed order, so the socket list has one form per set of sockets. -/
def Kind.ord : Kind → Nat
  | .inb .a .plain => 0
  | .inb .a .lb => 1
  | .inb .b .plain => 2
  | .inb .b .lb => 3
  | .outb .ctr => 4
  | .outb .dmn => 5
  | .proxC .ctr => 6
  | .proxC .dmn => 7
  | .proxS .ctr => 8
  | .proxS .dmn => 9
  | .loopC => 10
  | .loopS => 11
  | .selfC => 12
  | .selfS => 13

/-- A TCP socket's state, as far as the boundary can tell sockets apart. Both are in the dump
(`dump_family` asks for every state but LISTEN; tcp_diag.c:320-327 adds request sockets). -/
inductive St where
  /-- a request socket: the listener answered a SYN and waits for the ACK -/
  | syn
  /-- established -/
  | est
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- How a socket came to be in this guest. Not kernel state: it is what the properties are
about. -/
inductive Gen where
  /-- in the memory image this guest was restored from -/
  | stale
  /-- made by a packet from outside while this guest was a clone that had not published -/
  | smuggled
  /-- opened by a process of this guest -/
  | own
  /-- made by a packet from outside while this guest was not an unpublished clone -/
  | fresh
  deriving DecidableEq, Repr, Hashable, Inhabited

structure Sock where
  kind : Kind
  /-- the kernel's socket cookie: unique, never reused (`__sock_gen_cookie`,
  net/core/sock_diag.c:27-40, a counter), and kept by the socket a request socket becomes
  (inet_connection_sock.c:1263-1264) -/
  id : Nat
  st : St := .est
  gen : Gen := .own
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- What connection tracking holds for a client's tuple. -/
inductive CtSt where
  /-- only the client's direction seen: `IP_CT_NEW` (ct_core.c:1879-1883) -/
  | new
  /-- a reply seen and no close: SYN_RECV or ESTABLISHED, `IP_CT_ESTABLISHED` -/
  | est
  /-- closed with a FIN seen (TIME_WAIT, or CLOSE after one). A SYN recycles it: the entry is
  deleted and the packet looked up again (ct_tcp.c:1036-1067, `IP_CT_TCP_FLAG_CLOSE_INIT`) -/
  | closed
  /-- the last packet was a reset from the guest's side and no FIN was seen: CLOSE, or still
  ESTABLISHED when the reset's sequence did not match exactly (ct_tcp.c:1211-1261). A client's
  SYN does not recycle it, because the reset was not from the SYN's direction
  (ct_tcp.c:1051-1054): the SYN is ignored and passes on the old entry, which has seen a reply
  and so is not NEW. The entry lasts `nf_conntrack_tcp_timeout_close`, 10 s by default -/
  | reset
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- A segment from a client. -/
inductive Seg where
  /-- a SYN that finds room in the listener's SYN queue -/
  | syn
  /-- a SYN that finds the SYN queue full (or `tcp_syncookies` at 2) -/
  | synFull
  /-- a bare ACK or a data segment -/
  | ack
  /-- the client closes, and the guest's end follows -/
  | fin
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- One step of a restore, in the order a rule book runs them (`Rules.restore`). -/
inductive RStep where
  /-- `FirewallGate::refuse_syn_cookies`: write 0 to `net.ipv4.tcp_syncookies` -/
  | cookiesOff
  /-- `is_closed`, and `close` when the inherited gate does not verify -/
  | verify
  /-- `gate_udp_ingress`: the clone's UDP rule -/
  | udp
  /-- the receive barrier begins: `rcu_expedited` written 1 (`ExpeditedGracePeriods::begin`) -/
  | barIn
  /-- the grace period has been waited and the switch written back -/
  | barOut
  /-- `store.load()`: the snapshot's manifest -/
  | load
  /-- the restore's own list: the IPv4 dump -/
  | dump4
  /-- the IPv6 dump, and the comparison with the manifest -/
  | dump6
  /-- one more dump, for an end the first missed -/
  | look
  /-- `open_egress` -/
  | egress
  /-- `SOCK_DESTROY` of every listed cookie -/
  | retire
  /-- `store.remove()` -/
  | disarm
  /-- `payload_freeze::thaw` -/
  | thaw
  /-- the restore hook -/
  | hook
  /-- `open`: the rest of the gate goes, in one nf_tables transaction for both families -/
  | ingress
  /-- `restore_syn_cookies` -/
  | cookiesBack
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- How a restore decides which listed sockets it retires beside the manifest's. -/
inductive ListRule where
  /-- it takes no list -/
  | none
  /-- every listed socket the manifest does not name, unless its peer is loopback or its mirror
  image is listed -/
  | addr
  /-- every listed socket the manifest does not name, unless its mirror image is listed -/
  | mirror
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- A rule book: what one state of the code does. -/
structure Rules where
  name : String
  /-- the gate's TCP rule rejects by connection-tracking state (`--ctstate NEW`); otherwise by
  the packet's flags (a SYN without ACK) -/
  tcpByCt : Bool
  /-- the manifest leaves out a socket for its loopback peer alone; otherwise only when its
  mirror image is in the dump too -/
  manAddr : Bool
  list : ListRule
  /-- a second dump pairs an end the first one missed -/
  look : Bool
  restore : List RStep
  /-- the egress proxy's redirect is closed with the gate's egress half -/
  natGate : Bool
  /-- the payload-freeze watchdog thaws a freeze while the boundary that made it is still in
  progress: a prepare that has not been paused yet, or a restore -/
  wdMidBoundary : Bool
  /-- `snapshot create` is admitted on a clone that has not published -/
  snapUnpublished : Bool
  /-- a reopen of the source that fails, or that no agent answers, is tried again, `retryMax`
  times; then `snapshot create` fails and the host records the guest as failed and still gated
  (`State.reported`), and refuses further creates of it. Each try also writes `rcu_expedited`
  back when it can (`Rules.rcuUndo`), whether or not the gate opens. Otherwise `snapshot create`
  returns its error after one exec and nothing tries again or records anything -/
  reopenRetry : Bool
  /-- a write of `tcp_syncookies` back that fails `retryMax` times fails the restore; otherwise
  the restore goes on with a warning -/
  backFails : Bool
  /-- a restarted fc-agent serves exec at once and holds the start of the container while a
  boundary is in progress (a `snapshot create` under way, or a half of the gate installed);
  otherwise it starts the container as soon as its boot path gets there, which over vsock is
  also behind a closed gate and inside a prepare -/
  startWaits : Bool
  /-- what a dead `snapshot create` left is undone by a guard -/
  guard : Bool
  /-- the resume one-shot writes `rcu_expedited` back when a barrier of this boundary left it
  on -/
  rcuUndo : Bool

/-- What exists around the guest, and which moves a search allows. The proofs hold for every
value; the search fixes one per run. -/
structure Cfg where
  /-- IPv4 nat rules are installed at boot (a `/loopback` port's DNAT and SNAT, or the egress
  proxy's redirect), so the guest tracks IPv4 connections all the time. Tracking is per family
  (`nf_ct_netns_get` takes the rule's family, xt_conntrack.c:266-280, and
  nf_conntrack_proto.c:441-470 counts `users4` and `users6` apart) -/
  nat : Bool := false
  /-- IPv6 nat rules too: the egress proxy's ip6tables redirect (proxy.rs
  `setup_ip6tables_redirect`). A `/loopback` port's rules are iptables only (network.rs) -/
  nat6 : Bool := false
  /-- the egress proxy: nat OUTPUT redirects outbound TCP to a loopback port (proxy.rs) -/
  proxy : Bool := false
  /-- a port is published with `/loopback` -/
  lb : Bool := false
  /-- `net.ipv4.tcp_syncookies` at boot is not 0 -/
  sc0 : Bool := true
  /-- fc-agent reads its boot plan over MMDS, a TCP connection on eth0 (Firecracker; Cloud
  Hypervisor's is over vsock): an agent that starts behind a closed egress half is answered
  with the gate's reset and tries again for ever (agent.rs:74-86) -/
  mmds : Bool := true
  /-- the guest's tracker holds an ESTABLISHED entry for client `a`'s tuple and no socket: a
  guest that has been through a boundary before (#2106) -/
  staleCt : Bool := false
  /-- a client's datagram can reach a clone before the clone's UDP rule is in. False with the
  host's gate on published UDP ports (src/network/udp_gate.rs), and for a routed clone, which
  the network reaches only at the address it takes after that rule -/
  udpReach : Bool := false
  /-- client `b` (IPv6) exists -/
  cliB : Bool := false
  /-- a SYN can find a listener's SYN queue full -/
  flood : Bool := false
  /-- processes make loopback connections inside the guest -/
  loops : Bool := false
  /-- and connections to a loopback listener from the guest's own address -/
  selfs : Bool := false
  /-- a process outside the container connects out -/
  dmnOut : Bool := false
  /-- the container connects out -/
  ctrOut : Bool := false
  /-- UDP traffic -/
  udp : Bool := false
  /-- a step of the agent or of a one-shot can fail -/
  fails : Bool := false
  /-- the agent or a one-shot can be killed, at any step, in a source and in a clone -/
  kills : Bool := false
  /-- the host's `snapshot create` can die -/
  hostDies : Bool := false
  /-- a boundary can be held past the payload freeze's deadline (60 s of guest run time) -/
  slow : Bool := false
  /-- the host is asked for a snapshot of a clone that has not published yet -/
  earlySnap : Bool := false
  /-- bounds for the search: socket cookies handed out, and restores followed -/
  maxId : Nat := 4
  maxGens : Nat := 1

/-- Where a `snapshot create` of this guest is. From `frozen` to `saved` the prepare one-shot
runs in the guest (`fc-agent --prepare-snapshot-network`, main.rs:108-118); `opened` is the
resume one-shot between its two steps (`--resume-snapshot-network`). -/
inductive SP where
  | idle
  /-- the container is frozen (`payload_freeze::freeze`); the one-shot has not taken the
  boundary lock -/
  | frozen
  /-- `close`: the gate's chains and hooks are in for IPv4 -/
  | closed4
  /-- and for IPv6 -/
  | closed
  /-- the receive barrier begins: `rcu_expedited` written 1 -/
  | barIn
  /-- the grace period waited, the switch written back -/
  | bar
  /-- the IPv4 sockets are dumped -/
  | dump4
  /-- the IPv6 sockets are dumped -/
  | dump6
  /-- the manifest is saved and the one-shot has exited 0 -/
  | saved
  /-- the one-shot exited non-zero, or was killed -/
  | aborted
  /-- the VMM has paused the guest: the memory image is this state -/
  | paused
  /-- the VMM has resumed the guest; the resume one-shot has not run -/
  | resumed
  /-- the resume one-shot has opened the gate -/
  | opened
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Where this guest's restore is. -/
inductive RP where
  /-- never restored -/
  | none
  /-- a clone whose agent has completed the first `n` steps of `Rules.restore` -/
  | at (n : Nat)
  /-- a step failed: the agent is on its way to `shutdown_vm(1)` (mmds.rs:448-465) -/
  | failed
  /-- published and acknowledged -/
  | done
  deriving DecidableEq, Repr, Hashable, Inhabited

structure State where
  -- The kernel, in the guest's network namespace.
  socks : List Sock := []
  /-- the next socket cookie -/
  next : Nat := 1
  /-- the tracker's entry for client `a` on `plain`, `a` on `lb`, `b` on `plain` -/
  ctA : Option CtSt := none
  ctL : Option CtSt := none
  ctB : Option CtSt := none
  /-- the tracker holds a UDP flow a client began, and one the guest began -/
  ctUin : Bool := false
  ctUout : Bool := false
  /-- the tracker's hooks of a family are still attached after the last rule that used it was
  removed: the rule's destruction, and with it `nf_ct_netns_put` (xt_conntrack.c:279), runs
  after the commit -/
  lag4 : Bool := false
  lag6 : Bool := false
  /-- client `a` holds a SYN cookie of `plain`, of `lb`; client `b` of `plain` -/
  ckA : Bool := false
  ckL : Bool := false
  ckB : Bool := false
  /-- `net.ipv4.tcp_syncookies` is not 0 -/
  sc : Bool := true
  /-- `/sys/kernel/rcu_expedited` is 1 on the boundary's account -/
  rcu : Bool := false
  /-- the gate's INPUT and OUTPUT halves, per family: chain present and hooked first -/
  in4 : Bool := false
  in6 : Bool := false
  out4 : Bool := false
  out6 : Bool := false
  /-- the clone's UDP rule is in the INPUT chains -/
  udp : Bool := false
  -- Files under /run, which is guest memory.
  /-- the manifest: the cookies a restore retires -/
  man : Option (List Nat) := none
  /-- the container's cgroup is frozen -/
  frozen : Bool := false
  /-- the container is alive. systemd kills the unit's cgroup, the container in it, when
  fc-agent dies (`Restart=on-failure`, `Delegate=yes`, no `KillMode`, src/setup/rootfs.rs;
  payload_freeze.rs:69 puts the container's cgroup inside the unit's) -/
  up : Bool := true
  -- Processes.
  sp : SP := .idle
  rp : RP := .none
  /-- the long-running fc-agent is alive and serves exec -/
  ag : Bool := true
  /-- the host serves this guest a boot plan: true from a cold boot, false once the guest is
  restored from a memory image. A restore replaces the MMDS store with a `latest` object that
  has no `container-plan` (src/commands/common.rs:4456-4487), and the Cloud Hypervisor restore
  document has none (src/commands/snapshot.rs:4309-4361) -/
  plan : Bool := true
  /-- the image this guest was restored from was taken inside a restore: the agent's task goes
  on with that restore, and its watcher then begins one for the new epoch -/
  again : Bool := false
  /-- failed tries of the step being tried again: the reopen, or the write of `tcp_syncookies`
  back -/
  tries : Nat := 0
  /-- the host's `snapshot create` is alive -/
  host : Bool := true
  /-- the host has recorded this guest as failed and still gated: a `snapshot create` could not
  reopen it. The guest runs on; the create failed naming it, and no further create is admitted.
  The host records what it knows: when fc-agent died after the resume one-shot had opened the
  gate and before it had removed the manifest and thawed, the gate is in fact open, and the
  host, whose exec failed, cannot tell -/
  reported : Bool := false
  /-- the VM is powered on -/
  pw : Bool := true
  /-- the host's `snapshot run` still waits for the restore's acknowledgement -/
  waiting : Bool := false
  -- The restoring agent's memory, gone when it is killed.
  /-- what `tcp_syncookies` held before `refuse_syn_cookies` (`syn_cookies_before`) -/
  saved : Option Bool := none
  /-- the sockets a dump in progress has shown -/
  seen : List Sock := []
  /-- the cookies the restore's own list adds to the manifest's -/
  lst : List Nat := []
  /-- the barrier in progress turned `rcu_expedited` on, so it turns it off (a switch already on
  is left alone, `ExpeditedGracePeriods::begin`). The memory of the agent or of a one-shot -/
  rcuOwn : Bool := false
  -- Records the properties read. Not state of the system.
  /-- restores followed so far -/
  gens : Nat := 0
  /-- this clone's restore has retired sockets -/
  cut : Bool := false
  /-- an established connection of a guest being snapshotted was reset by the gate -/
  rst : Bool := false
  /-- a restore destroyed a socket it must not -/
  bad : Bool := false
  /-- a packet from outside made a socket in a clone after its retirement, before publication -/
  late : Bool := false
  /-- sockets were retired with the egress half closed -/
  eg : Bool := false
  /-- the hook ran with egress closed, or with ingress open -/
  hk : Bool := false
  /-- a memory image was taken with a setting changed -/
  cap : Bool := false
  /-- a datagram on a flow a client began reached a clone that had not published -/
  udpIn : Bool := false
  /-- a datagram on a flow the guest began reached a clone that had not published -/
  udpLate : Bool := false
  deriving DecidableEq, Repr, Hashable, Inhabited

/-! ## Reading the state -/

def State.ctGet (s : State) : Cli → Port → Option CtSt
  | .a, .plain => s.ctA
  | .a, .lb => s.ctL
  | .b, .plain => s.ctB
  | .b, .lb => none

def State.ctSet (s : State) (c : Cli) (p : Port) (v : Option CtSt) : State :=
  match c, p with
  | .a, .plain => { s with ctA := v }
  | .a, .lb => { s with ctL := v }
  | .b, .plain => { s with ctB := v }
  | .b, .lb => s

def State.ckGet (s : State) : Cli → Port → Bool
  | .a, .plain => s.ckA
  | .a, .lb => s.ckL
  | .b, .plain => s.ckB
  | .b, .lb => false

def State.ckSet (s : State) (c : Cli) (p : Port) : State :=
  match c, p with
  | .a, .plain => { s with ckA := true }
  | .a, .lb => { s with ckL := true }
  | .b, .plain => { s with ckB := true }
  | .b, .lb => s

/-- The guest executes instructions. -/
def State.running (s : State) : Bool := s.pw && s.sp != .paused

/-- A clone that has not published. -/
def State.unpublished (s : State) : Bool :=
  match s.rp with
  | .at _ | .failed => true
  | _ => false

/-- Any half of the gate is installed in any family. -/
def State.gated (s : State) : Bool := s.in4 || s.in6 || s.out4 || s.out6

/-- Connection tracking sees a family's packets: its hooks for that family are attached while a
rule of that family uses it (`nf_ct_netns_get`, xt_conntrack.c:270), and a little longer
(`lag4`, `lag6`). The clone's UDP rule is in both families' INPUT chains. -/
def tracked (R : Rules) (C : Cfg) (s : State) : Fam → Bool
  | .v4 => C.nat || s.lag4 || s.udp || (R.tcpByCt && (s.in4 || s.out4))
  | .v6 => C.nat6 || s.lag6 || s.udp || (R.tcpByCt && (s.in6 || s.out6))

/-- How many times a step that is tried again is tried before it is given up. -/
def retryMax : Nat := 3

/-- The container's processes run. -/
def State.ctrRuns (s : State) : Bool := s.up && !s.frozen

def State.has (s : State) (k : Kind) : Bool := s.socks.any (·.kind == k)

def State.find (s : State) (k : Kind) : Option Sock := s.socks.find? (·.kind == k)

/-- Insert in `Kind.ord` order. -/
def ins (x : Sock) : List Sock → List Sock
  | [] => [x]
  | y :: l => if x.kind.ord ≤ y.kind.ord then x :: y :: l else y :: ins x l

def State.drop (s : State) (k : Kind) : State :=
  { s with socks := s.socks.filter (·.kind != k) }

/-- A new socket of kind `k`, with the next cookie. -/
def State.add (s : State) (k : Kind) (st : St) (g : Gen) : State :=
  { s with socks := ins { kind := k, id := s.next, st := st, gen := g } s.socks,
           next := s.next + 1 }

/-! ## The socket lists (`sockets_leaving_the_guest`, `manifest_sockets`, `unlisted_sockets`) -/

/-- `l` holds the socket with `x`'s two ends swapped. -/
def hasMirror (l : List Sock) (x : Sock) : Bool :=
  match x.kind.mirror with
  | some k => l.any (·.kind == k)
  | none => false

/-- The cookies a snapshot writes to its manifest, from the dump `seen` and, with `Rules.look`,
one more look at the sockets as they are now. -/
def manifestIds (R : Rules) (seen now : List Sock) : List Nat :=
  let stays (x : Sock) : Bool :=
    if R.manAddr then x.kind.peerLo
    else x.kind.peerLo && (hasMirror seen x || (R.look && hasMirror now x))
  (seen.filter (fun x => !stays x)).map (·.id)

/-- The cookies a restore's own list adds, before any second look. -/
def listIds (R : Rules) (man : List Nat) (seen : List Sock) : List Nat :=
  match R.list with
  | .none => []
  | .addr => (seen.filter (fun x => !man.contains x.id && !x.kind.peerLo && !hasMirror seen x)).map (·.id)
  | .mirror => (seen.filter (fun x => !man.contains x.id && !hasMirror seen x)).map (·.id)

/-- The second look: a listed socket whose mirror image exists now is taken off the list. -/
def lookIds (lst : List Nat) (seen now : List Sock) : List Nat :=
  lst.filter (fun i => !(seen.any (fun x => x.id == i && hasMirror now x)))

/-! ## The kernel: a segment from a client -/

/-- The tracker's entry once the packet is through, and whether `--ctstate NEW` matches it.

- no entry and a SYN: a new entry, NEW (`tcp_new`, ct_tcp.c:876-905);
- no entry and a FIN: invalid, no entry, not NEW (the `fin` row from `sNO` is `sIV`,
  ct_tcp.c:169);
- no entry and anything else: picked up as a connection already open, with `tcp_loose` at its
  default of 1, still NEW because no reply has been seen (ct_tcp.c:907-925, 1638;
  ct_core.c:1879-1883);
- an entry that has seen a reply and no close: a SYN is ignored and the entry stands, so the
  packet is ESTABLISHED (the `sIG` cells of the `syn` row, ct_tcp.c:138, and
  `TCP_CONNTRACK_IGNORE`, ct_tcp.c:1069-1125);
- an entry closed with a FIN seen, and a SYN: the entry is deleted and the packet looked up
  again, NEW (ct_tcp.c:1036-1067);
- an entry the guest's side reset, and a SYN: not recycled, ignored, and not NEW
  (ct_tcp.c:1051-1054, then the fall through to `TCP_CONNTRACK_IGNORE`). -/
def ctIn (seg : Seg) (cur : Option CtSt) : Option CtSt × Bool :=
  let syn := seg == .syn || seg == .synFull
  match cur with
  | none => if seg == .fin then (none, false) else (some .new, true)
  | some .new => (some .new, true)
  | some .est => (if seg == .fin then some .closed else some .est, false)
  | some .closed => if syn then (some .new, true) else (some .closed, false)
  | some .reset => (some .reset, false)

/-- The tracker's entry for the tuple once the guest has answered the segment: a reply makes it
ESTABLISHED and a FIN closes it. A FIN with no entry was invalid and nothing is tracked. The
guest's reset (no socket for the segment) depends on what the entry was:

- an entry that had seen a reply is left reset;
- one that had not (no entry, so the segment was picked up, or `new`) is deleted: the reset is
  its only reply, and the tracker kills an unreplied entry on a reset (ct_tcp.c:1344-1352);
- one already closed with a FIN seen stays as it is, and a SYN still recycles it.

Nothing changes when the guest does not track. -/
def answered (tr : Bool) (c : Cli) (p : Port) (seg : Seg) (reset : Bool) (s : State) : State :=
  if !tr then s
  else if seg == .fin then (if (s.ctGet c p).isNone then s else s.ctSet c p (some .closed))
  else if reset then
    match s.ctGet c p with
    | some .est | some .reset => s.ctSet c p (some .reset)
    | some .closed => s
    | _ => s.ctSet c p none
  else s.ctSet c p (some .est)

def State.noteRst (s : State) (b : Bool) : State := { s with rst := s.rst || b }

def State.noteLate (s : State) (b : Bool) : State := { s with late := s.late || b }

/-- The request socket of kind `k` becomes the established socket, keeping its cookie. -/
def State.upgrade (s : State) (k : Kind) : State :=
  { s with socks := s.socks.map (fun y => if y.kind == k then { y with st := .est } else y) }

/-- The gate's ingress half is installed for the family. -/
def State.gateIn (s : State) : Fam → Bool
  | .v4 => s.in4
  | .v6 => s.in6

/-- The gate's TCP rule rejects the segment: with the ingress half in for the client's family,
a segment the tracker calls NEW (`Rules.tcpByCt`), or else a SYN. -/
def rejects (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg) (s : State) : Bool :=
  s.gateIn c.fam
    && (if R.tcpByCt then (tracked R C s c.fam && (ctIn seg (s.ctGet c p)).2)
        else (seg == .syn || seg == .synFull))

/-- The state after a rejected segment. A closed entry that a SYN recycled is gone and the new
one is never confirmed. The record: an established connection of a guest being snapshotted was
answered with a reset. -/
def rejected (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg) (s : State) : State :=
  (if tracked R C s c.fam && (seg == .syn || seg == .synFull) && s.ctGet c p == some .closed
    then s.ctSet c p none else s).noteRst
    (!(seg == .syn || seg == .synFull) && s.sp != .idle
      && s.socks.any (fun x => x.kind == .inb c p && x.st == .est))

/-- A SYN that was let in and dropped by a full SYN queue: the tracker has its entry, unanswered. -/
def unanswered (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg) (s : State) : State :=
  if tracked R C s c.fam then s.ctSet c p (ctIn seg (s.ctGet c p)).1 else s

/-- A clone that has not published, with its ingress half still in for this family: a packet
from outside that makes a socket got past the closed gate. -/
def State.shutTo (s : State) (f : Fam) : Bool := s.unpublished && s.gateIn f

/-- How a socket made by a packet from outside is counted. -/
def genIn (s : State) (f : Fam) : Gen := if s.shutTo f then .smuggled else .fresh

/-- A segment `seg` from client `c` to port `p` arrives on eth0.

The hooks run in priority order (netfilter_ipv4.h:34-44): connection tracking (-200), nat
PREROUTING (-100, the published port's DNAT), filter INPUT (0, the gate), nat INPUT (100, the
`/loopback` SNAT), confirmation of a new entry. So the gate sees the client's own address, and
the socket of a `/loopback` port sees 127.0.0.1.

The gate's INPUT chain (`gate_chain_rules`): TCP from a loopback source returns, anything that
entered on `lo` returns, then the TCP rule. A REJECT with `tcp-reset` answers the sender with a
reset and drops the packet (ipt_REJECT.c:58-66), so an entry it would have created is never
confirmed.

Past the gate, TCP (`tcp_v4_rcv`, `tcp_v4_do_rcv`, tcp_ipv4.c:1935-1975):

- a request socket and an ACK: the established socket, with the request's cookie
  (`tcp_check_req`, tcp_minisocks.c:689-941; inet_connection_sock.c:1263-1264);
- a listener and a SYN: a request socket, or with the SYN queue full a cookie and nothing kept
  when `tcp_syncookies` is not 0, else a drop (`tcp_conn_request`, tcp_input.c:7434-7467);
- a listener and an ACK: an established socket if it is a valid cookie and `tcp_syncookies` is
  not 0 (`cookie_v4_check`, syncookies.c:405-423; net/ipv6/syncookies.c reads the same setting),
  else a reset (`tcp_rcv_state_process`, tcp_input.c:6965-6967). -/
def pkt (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg) (s : State) : Option State :=
  if !s.running then none
  else if p == .lb && (!C.lb || c == .b) then none
  else if c == .b && !C.cliB then none
  else if seg == .synFull && !C.flood then none
  else if rejects R C c p seg s then some (rejected R C c p seg s)
  else
    let tr := tracked R C s c.fam
    match seg, s.find (.inb c p) with
    | .syn, none =>
      if s.next < C.maxId then
        some (answered tr c p seg false
          ((s.add (.inb c p) .syn (genIn s c.fam)).noteLate (s.shutTo c.fam && s.cut)))
      else none
    | .synFull, none =>
      if s.sc then some (answered tr c p seg false (s.ckSet c p))
      else some (unanswered R C c p seg s)
    | .ack, some x =>
      if x.st == .syn then some (answered tr c p seg false (s.upgrade (.inb c p)))
      else some (answered tr c p seg false s)
    | .ack, none =>
      if s.sc && s.ckGet c p then
        if s.next < C.maxId then
          some (answered tr c p seg false
            ((s.add (.inb c p) .est (genIn s c.fam)).noteLate (s.shutTo c.fam && s.cut)))
        else none
      else some (answered tr c p seg true s)
    | .fin, some _ => some (answered tr c p seg false (s.drop (.inb c p)))
    | .fin, none => some (answered tr c p seg true s)
    | _, some _ => some (answered tr c p seg false s)

/-! ## The kernel: UDP -/

/-- A datagram on a flow a client began. The clone's UDP rule rejects it whatever the tracker
holds, because the client is the flow's origin (`--ctdir ORIGINAL`, xt_conntrack.c:193-195).
Before the rule is in, the host's gate or the network keeps it from the clone (`Cfg.udpReach`). -/
def udpC (R : Rules) (C : Cfg) (s : State) : Option State :=
  if !s.running || !C.udp then none
  else if s.unpublished && (s.in4 || s.in6) && !C.udpReach then none
  else if s.udp then none
  else some { s with ctUin := s.ctUin || tracked R C s .v4,
                     udpIn := s.udpIn || (s.unpublished && (s.in4 || s.in6)) }

/-- A datagram from the address and port the guest itself sent to, to the port it sent from.
With the clone's UDP rule in: an entry for that flow puts it in the reply direction and the rule
passes it; with none, the sender is the origin of a new flow and it is rejected. Before the rule
is in, what keeps it from a clone is what keeps a client's datagram (`Cfg.udpReach`). -/
def udpR (C : Cfg) (s : State) : Option State :=
  if !s.running || !C.udp then none
  else if s.udp && !s.ctUout then none
  else if !s.udp && s.unpublished && (s.in4 || s.in6) && !C.udpReach then none
  else some { s with udpLate := s.udpLate || (s.unpublished && (s.in4 || s.in6)) }

/-- A process of the guest sends a datagram. OUTPUT gates no UDP but NTP. -/
def udpS (R : Rules) (C : Cfg) (s : State) : Option State :=
  if !s.running || !C.udp then none
  else some { s with ctUout := s.ctUout || tracked R C s .v4 }

/-! ## The guest's own processes -/

def procRuns (s : State) : Who → Bool
  | .ctr => s.ctrRuns
  | .dmn => true

/-- Process `w` connects to a server outside the guest.

With the egress proxy, nat OUTPUT (priority -100) redirects the SYN to the proxy's loopback
port before filter OUTPUT (0) sees it (proxy.rs `setup_iptables_redirect_v4`; the redirect of a
locally generated packet is to 127.0.0.1, net/netfilter/nf_nat_redirect.c:59-60), and the gate's
OUTPUT chain returns for a loopback destination: the closed egress half does not refuse it.
Without the proxy, the egress half answers the SYN with a reset and no socket is left. -/
def conn (R : Rules) (C : Cfg) (w : Who) (s : State) : Option State :=
  if !s.running || !procRuns s w then none
  else if (w == .ctr && !C.ctrOut) || (w == .dmn && !C.dmnOut) then none
  else if C.proxy && !(R.natGate && s.out4) then
    if s.has (.proxC w) || s.has (.proxS w) || C.maxId ≤ s.next + 1 then none
    else some ((s.add (.proxC w) .est .own).add (.proxS w) .est .own)
  else if s.out4 then none
  else if s.has (.outb w) || C.maxId ≤ s.next then none
  else some (s.add (.outb w) .est .own)

/-- Process `w` closes its outbound connection. -/
def shut (w : Who) (s : State) : Option State :=
  if !s.running || !procRuns s w then none
  else if s.has (.outb w) || s.has (.proxC w) || s.has (.proxS w) then
    some (((s.drop (.outb w)).drop (.proxC w)).drop (.proxS w))
  else none

/-- The two kinds of connection inside the guest: a loopback connection, and (`selfs`) one to a
loopback listener from the guest's own address. -/
def pairOn (C : Cfg) : Bool → Bool
  | true => C.selfs
  | false => C.loops

def pairC : Bool → Kind
  | true => .selfC
  | false => .loopC

def pairS : Bool → Kind
  | true => .selfS
  | false => .loopS

/-- A process outside the container opens a connection inside the guest: both sockets at once.
A loopback destination passes both halves of the closed gate. -/
def pair (C : Cfg) (selfs : Bool) (s : State) : Option State :=
  if !s.running || !pairOn C selfs then none
  else if s.has (pairC selfs) || s.has (pairS selfs) || C.maxId ≤ s.next + 1 then none
  else some ((s.add (pairC selfs) .est .own).add (pairS selfs) .est .own)

def unpair (selfs : Bool) (s : State) : Option State :=
  if !s.running || !(s.has (pairC selfs) || s.has (pairS selfs)) then none
  else some ((s.drop (pairC selfs)).drop (pairS selfs))

/-! ## Rules going in and out -/

/-- `close`, for one family or both: the chains are declared, which empties a chain that was
there (the clone's UDP rule goes), and hooked first. -/
def State.closeGate (s : State) (v4 v6 : Bool) : State :=
  { s with in4 := s.in4 || v4, out4 := s.out4 || v4, in6 := s.in6 || v6, out6 := s.out6 || v6,
           udp := false }

/-- `open`: whatever is left of the gate goes, in both families. A tracker that only the gate's
rules kept attached stays attached a little longer. -/
def openGate (R : Rules) (C : Cfg) (s : State) : State :=
  { s with in4 := false, in6 := false, out4 := false, out6 := false, udp := false,
           lag4 := s.lag4 || (!C.nat && (s.udp || (R.tcpByCt && (s.in4 || s.out4)))),
           lag6 := s.lag6 || (!C.nat6 && (s.udp || (R.tcpByCt && (s.in6 || s.out6)))) }

/-- A powered-off guest: nothing of it is left but the records. -/
def State.dead (s : State) : State :=
  { pw := false, gens := s.gens, rst := s.rst, bad := s.bad, late := s.late, eg := s.eg,
    hk := s.hk, cap := s.cap, udpIn := s.udpIn, udpLate := s.udpLate }

/-! ## `snapshot create` -/

/-- The boundary lock (`/run/fcvm/snapshot-network.lock`) is held by the restoring agent: from
its first step until it publishes or exits. A dead agent holds nothing. -/
def lockRestore (s : State) : Bool :=
  s.ag && (match s.rp with
    | .at n => 0 < n
    | _ => false)

/-- The lock is held by a one-shot. -/
def lockOneShot (s : State) : Bool :=
  match s.sp with
  | .closed4 | .closed | .barIn | .bar | .dump4 | .dump6 | .opened => true
  | _ => false

/-- The reopen: the resume one-shot opens the gate (and, with `Rules.rcuUndo`, writes
`rcu_expedited` back). -/
def reopened (R : Rules) (C : Cfg) (s : State) : State :=
  { openGate R C s with sp := .opened, rcu := s.rcu && !R.rcuUndo, tries := 0 }

/-- The reopen cannot be delivered: no fc-agent answers the host's exec, which tries its
connect 41 times over about 54 s (src/commands/exec.rs, `MAX_EXEC_CONNECT_ATTEMPTS`). With
`Rules.reopenRetry` the host (or the guard) tries again; otherwise `snapshot create` returns its
error, and what the prepare left stays. -/
def unreached (R : Rules) (s : State) : State :=
  if R.reopenRetry then { s with tries := s.tries + 1 } else { s with sp := .idle }

/-- The tries are spent: `snapshot create` fails, the guest runs on with what the boundary left,
and the host records it as failed and still gated. -/
def gaveUp (s : State) : State := { s with sp := .idle, reported := true, tries := 0 }

/-- The next step of `snapshot create`, when it succeeds. `byGuard` is the guard doing a dead
host's part.

Host (common.rs `create_snapshot_core`): start the prepare one-shot, pause, resume, start the
resume one-shot; after a prepare that failed, the resume one-shot alone. In the guest
(`prepare_with`, `resume_source_network`): freeze, close, barrier, dump, save; open, remove the
manifest, thaw.

Both one-shots, and the guard's, are exec'd through the long-running fc-agent's exec server
(common.rs:6021), so each step in the guest needs the agent alive. The pause and the resume are
the VMM's and need nothing in the guest. -/
def snap (R : Rules) (C : Cfg) (byGuard : Bool) (s : State) : Option State :=
  if !s.pw then none
  else
    let hostStep := if byGuard then !s.host && R.guard else s.host
    match s.sp with
    | .idle =>
      -- the host starts a snapshot of a guest it holds no snapshot of
      if byGuard || !s.host || !s.ag || s.reported then none
      else if s.unpublished && !(R.snapUnpublished && C.earlySnap) then none
      else some { s with sp := .frozen, frozen := true, tries := 0 }
    | .frozen =>
      if byGuard || !s.ag || lockRestore s then none
      else some { s.closeGate true false with sp := .closed4 }
    | .closed4 =>
      if byGuard || !s.ag then none else some { s.closeGate false true with sp := .closed }
    | .closed =>
      if byGuard || !s.ag then none
      else some { s with sp := .barIn, rcu := true, rcuOwn := !s.rcu }
    | .barIn =>
      if byGuard || !s.ag then none
      else some { s with sp := .bar, rcu := s.rcu && !s.rcuOwn, rcuOwn := false }
    | .bar =>
      if byGuard || !s.ag then none
      else some { s with sp := .dump4, seen := s.socks.filter (·.kind.fam == .v4) }
    | .dump4 =>
      if byGuard || !s.ag then none
      else some { s with sp := .dump6, seen := s.seen ++ s.socks.filter (·.kind.fam == .v6) }
    | .dump6 =>
      if byGuard || !s.ag then none
      else some { s with sp := .saved, man := some (manifestIds R s.seen s.socks), seen := [] }
    | .saved =>
      if byGuard then
        -- nobody is left to pause: the guard undoes the prepare
        if !hostStep || lockRestore s then none
        else if R.reopenRetry && retryMax ≤ s.tries then some (gaveUp s)
        else if s.ag then some (reopened R C s)
        else some (unreached R s)
      else if s.host then
        some { s with sp := .paused, tries := 0, cap := s.cap || s.sc != C.sc0 || s.rcu }
      else none
    | .paused => if hostStep then some { s with sp := .resumed, tries := 0 } else none
    | .resumed | .aborted =>
      if !hostStep || lockRestore s then none
      -- the host gives a guest up that it could not reopen
      else if R.reopenRetry && retryMax ≤ s.tries then some (gaveUp s)
      else if s.ag then some (reopened R C s)
      else some (unreached R s)
    | .opened =>
      if s.ag then
        -- the resume one-shot goes on: it removes the manifest and thaws
        if byGuard then none
        else some { s with sp := .idle, man := none, frozen := false, host := true, tries := 0 }
      -- the one-shot died with the agent, after it had opened the gate
      else if !hostStep then none
      else if R.reopenRetry && retryMax ≤ s.tries then some (gaveUp s)
      else some (unreached R s)

/-- A step of `snapshot create` fails.

A prepare step: the one-shot rolls back (`rollback_preparation` opens the gate, the barrier's
guard writes the switch back, main.rs thaws) and exits non-zero. The reopen: with
`Rules.reopenRetry` it is tried again, `retryMax` times; otherwise the one-shot goes on to
remove the manifest and thaw, exits non-zero, and nothing opens the gate. -/
def snapFail (R : Rules) (C : Cfg) (s : State) : Option State :=
  if !s.pw || !C.fails || !s.ag then none
  else match s.sp with
    | .frozen | .closed4 | .closed | .barIn | .bar | .dump4 | .dump6 =>
      if s.sp == .frozen && lockRestore s then none
      else some { openGate R C s with sp := .aborted, rcu := s.rcu && !s.rcuOwn, rcuOwn := false,
                                      frozen := false, seen := [], tries := 0 }
    | .resumed | .aborted =>
      if !(s.host || R.guard) || lockRestore s then none
      else if R.reopenRetry then
        -- the one-shot ran and its reopen failed; it still writes `rcu_expedited` back
        if s.tries < retryMax then
          some { s with tries := s.tries + 1, rcu := s.rcu && !R.rcuUndo }
        else none
      else if !s.host then none
      else some { s with sp := .idle, man := none, frozen := false }
    | _ => none

/-- A one-shot is killed: what it had done stays, nothing more of it runs. The host's exec
fails; after a prepare it runs the resume one-shot. -/
def snapKill (C : Cfg) (s : State) : Option State :=
  if !s.pw || !C.kills then none
  else match s.sp with
    | .closed4 | .closed | .barIn | .bar | .dump4 | .dump6 =>
      some { s with sp := .aborted, seen := [], rcuOwn := false, tries := 0 }
    | _ => none

/-! ## The restore -/

/-- The sockets a retirement's resets reach. `SOCK_DESTROY` is `tcp_abort`, which sends a reset
for an established socket (tcp.c:5058-5110). When the other end is in the guest the reset goes
out through OUTPUT, which refuses only a SYN and NTP, and comes in on `lo`, which the gate's
INPUT chain returns for (`gate_chain_rules`: `-i lo -j RETURN`), so the other end is reset and
freed too. `gone` are the sockets destroyed by cookie. -/
def resetBy (gone : List Sock) (y : Sock) : Bool :=
  gone.any (fun x => x.st == .est && x.kind.mirror == some y.kind)

/-- A restore step, when it succeeds. -/
def doStep (R : Rules) (C : Cfg) (r : RStep) (s : State) : State :=
  match r with
  | .cookiesOff => { s with saved := if s.sc then some true else s.saved, sc := false }
  -- The inherited gate verifies only as exactly the gate's rules, hooked first, in both
  -- families; anything else (a half that is gone, a UDP rule an earlier run of the restore
  -- added) and `close` declares the chains again. Either way both halves are in afterwards
  -- and hold the gate's rules and nothing more.
  | .verify => s.closeGate true true
  | .udp => { s with udp := true }
  | .barIn => { s with rcu := true, rcuOwn := !s.rcu }
  | .barOut => { s with rcu := s.rcu && !s.rcuOwn, rcuOwn := false }
  | .load => s
  | .dump4 => { s with seen := s.socks.filter (·.kind.fam == .v4) }
  | .dump6 =>
    let seen := s.seen ++ s.socks.filter (·.kind.fam == .v6)
    { s with seen := seen, lst := listIds R (s.man.getD []) seen }
  | .look => { s with lst := lookIds s.lst s.seen s.socks }
  | .egress => { s with out4 := false, out6 := false, seen := [] }
  | .retire =>
    let ids := s.man.getD [] ++ s.lst
    let gone := s.socks.filter (fun x => ids.contains x.id)
    let left := s.socks.filter (fun x => !ids.contains x.id)
    -- a destroyed established socket sends a reset, which leaves its tracked entry reset
    -- (`tcp_abort`, tcp.c:5058-5110); a destroyed request socket sends nothing
    let closes (c : Cli) (p : Port) : Bool :=
      tracked R C s c.fam && gone.any (fun x => x.kind == .inb c p && x.st == .est)
    { s with socks := left.filter (fun y => !resetBy gone y), cut := true, lst := [],
             eg := s.eg || s.out4 || s.out6,
             bad := s.bad
               || gone.any (fun x => x.gen == .own || x.gen == .fresh
                    || (hasMirror s.socks x
                        && (x.kind.loop || !(s.man.getD []).contains x.id)))
               || left.any (fun y => resetBy gone y && (y.gen == .own || y.gen == .fresh)),
             ctA := if closes .a .plain then some .reset else s.ctA,
             ctL := if closes .a .lb then some .reset else s.ctL,
             ctB := if closes .b .plain then some .reset else s.ctB }
  | .disarm => { s with man := none }
  | .thaw => { s with frozen := false }
  | .hook => { s with hk := s.hk || s.out4 || s.out6 || !s.in4 || !s.in6 }
  | .ingress => openGate R C s
  | .cookiesBack => { s with sc := (s.saved.getD s.sc), saved := none, tries := 0 }

/-- The agent's next step. -/
def nextStep (R : Rules) (s : State) : Option RStep :=
  match s.rp with
  | .at n => R.restore[n]?
  | _ => none

/-- The restoring agent can run: the guest runs, the agent is alive, and no one-shot holds the
lock. -/
def agentRuns (s : State) : Bool := s.running && s.ag && !lockOneShot s

/-- The restore's next step is done. After the last one the clone has published and
acknowledged; unless the image was taken inside a restore (`again`), in which case that was
the old epoch's restore and the watcher now begins the new one. -/
def advance (R : Rules) (s : State) : State :=
  match s.rp with
  | .at n => if n + 1 < R.restore.length then { s with rp := .at (n + 1) }
             else if s.again then { s with rp := .at 0, again := false }
             else { s with rp := .done, waiting := false }
  | _ => s

/-- The agent's next restore step succeeds. `load` with no manifest cannot, and with
`Rules.backFails` neither can a write-back that has failed `retryMax` times: the restore
fails. -/
def agent (R : Rules) (C : Cfg) (s : State) : Option State :=
  if !agentRuns s then none
  else match nextStep R s with
    | none => none
    | some r =>
      if (r == .load && s.man.isNone)
          || (r == .cookiesBack && R.backFails && retryMax ≤ s.tries) then
        some { s with rp := .failed, saved := none }
      else some (advance R (doStep R C r s))

/-- The agent's next restore step fails: the restore fails closed and the agent goes to
`shutdown_vm`. The write of `tcp_syncookies` back is the exception (`restore_syn_cookies`):
it is tried three times and then the restore goes on with a warning, the setting left at 0.
With `Rules.backFails` each failed try is a step, and after `retryMax` of them the restore
fails (`agent`). -/
def agentFail (R : Rules) (C : Cfg) (s : State) : Option State :=
  if !agentRuns s || !C.fails then none
  else match nextStep R s with
    | none => none
    | some .cookiesBack =>
      if R.backFails then
        if s.tries < retryMax then some { s with tries := s.tries + 1 } else none
      else some (advance R { s with saved := none })
    | some _ =>
      some { s with rp := .failed, saved := none, seen := [], lst := [],
                    rcu := s.rcu && !s.rcuOwn, rcuOwn := false }

/-- fc-agent is killed (an OOM kill, a crash), in a source or in a clone. What it held in
memory is gone, and so is its lock. systemd kills the rest of the unit's cgroup with it: the
container, and a one-shot in progress, which leaves what it had done (`snapKill`). A resume
one-shot that had opened the gate has not removed the manifest. -/
def kill (C : Cfg) (s : State) : Option State :=
  if !s.running || !s.ag || !C.kills then none
  else
    let sp := match s.sp with
      | .frozen | .closed4 | .closed | .barIn | .bar | .dump4 | .dump6 => .aborted
      | x => x
    some { s with ag := false, up := false, sp := sp, saved := none, seen := [], lst := [],
                  rcuOwn := false }

/-- systemd starts fc-agent again (`Restart=on-failure`, `RestartSec=1`, src/setup/rootfs.rs),
once the old unit's cgroup is gone, which a frozen container that does not die on SIGTERM can
hold up for the stop timeout (90 s by systemd's default). The new process fetches its boot plan
before anything else that matters here, in a loop with no end (agent.rs:74-86), and only then
starts the restore watcher (agent.rs:215) and the exec server.

- A guest restored from a memory image is served no plan (`State.plan`): the loop never ends,
  the restore is never run again, and exec never comes back. Most guests are restored guests:
  a `podman run` that hits the snapshot cache goes through the restore path.
- A guest that booted without the cache is served its plan again, unless the fetch is over
  MMDS and the gate's egress half is closed. Over vsock (Cloud Hypervisor) it is served behind
  a closed gate too: the gate is iptables rules and vsock is outside them.

This step is the agent back with its exec server. The container is `boot`.

Before the loop the new process writes /etc/resolv.conf and adds the boot command line's IPv6
address (agent.rs:66-67), which the model does not hold. -/
def restart (C : Cfg) (s : State) : Option State :=
  if !s.running || s.ag || !s.plan || (C.mmds && s.out4) then none
  else some { s with ag := true }

/-- The restarted agent's boot path reaches the container and starts it (a provisioned guest's
existing container, `existing_container_id`). With `Rules.startWaits` it holds that while a
boundary is in progress: a `snapshot create` under way, or a half of the gate installed. -/
def boot (R : Rules) (s : State) : Option State :=
  if !s.running || !s.ag || s.up then none
  else if R.startWaits && (s.sp != .idle || s.gated) then none
  else some { s with up := true, frozen := false }

/-- The failed restore's `shutdown_vm(1)` lands. -/
def off (s : State) : Option State :=
  if s.pw && s.rp == .failed && s.ag then some s.dead else none

/-- The host's `snapshot run` gives up on a clone that has not acknowledged its restore, and
ends the VM (src/commands/snapshot.rs, `RESTORE_COMPLETION_ACK_TIMEOUT`). -/
def timeout (s : State) : Option State :=
  if s.pw && s.waiting then some s.dead else none

/-- The prepare one-shot runs, or has exited 0 and the host has not paused yet. -/
def inPrepare (s : State) : Bool :=
  match s.sp with
  | .frozen | .closed4 | .closed | .barIn | .bar | .dump4 | .dump6 | .saved => true
  | _ => false

/-- The payload-freeze watchdog thaws a container whose freeze outlived its deadline
(payload_freeze.rs). It is a thread of the long-running agent and dies with it. -/
def watchdog (R : Rules) (C : Cfg) (s : State) : Option State :=
  if !s.running || !s.frozen || !s.ag || !C.slow then none
  else if !R.wdMidBoundary && (s.unpublished || inPrepare s) then none
  else some { s with frozen := false }

/-- A clone is restored from the memory image and resumed. Everything the guest held is in it;
every socket in it is now the source's, and no boot plan is served to it.

Its agent is as the image has it. An agent that was idle reads a new restore epoch and begins
a restore. One whose restore had failed had left it for `shutdown_vm`, and the clone resumes
there. One that was inside a restore (an image of a clone that had not published) goes on with
that restore from the step it was at, its memory with it, and the watcher begins the new
epoch's restore when that one is over (`again`). -/
def restore (C : Cfg) (s : State) : Option State :=
  if s.pw && s.sp == .paused && s.gens < C.maxGens then
    let t : State :=
      { s with sp := .idle, host := true, waiting := true, plan := false, tries := 0,
               socks := s.socks.map (fun x => { x with gen := .stale }),
               cut := false, gens := s.gens + 1 }
    match s.rp with
    | .at _ => some { t with again := true }
    | .failed => some t
    | _ => some { t with rp := .at 0, saved := none, seen := [], lst := [], rcuOwn := false }
  else none

/-! ## One step -/

inductive Label where
  /-- a segment from a client -/
  | pkt (c : Cli) (p : Port) (seg : Seg)
  | udpC | udpR | udpS
  /-- the tracker's hooks are detached -/
  | detach
  | conn (w : Who) | shut (w : Who)
  | pair (selfs : Bool) | unpair (selfs : Bool)
  /-- `snapshot create`: its next step, that step failing, the one-shot killed -/
  | snap | snapFail | snapKill
  /-- the host's `snapshot create` dies -/
  | hostDie
  /-- the guard does the dead host's next step -/
  | guard
  /-- a clone is restored from the image and followed -/
  | restore
  /-- the restore: its next step, that step failing, the agent killed, restarted, and the
  restarted agent's container started -/
  | agent | agentFail | kill | restart | boot
  | off | timeout | watchdog
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- One step of the whole system. `none` when the label is not enabled. -/
def step (R : Rules) (C : Cfg) (l : Label) (s : State) : Option State :=
  match l with
  | .pkt c p seg => pkt R C c p seg s
  | .udpC => udpC R C s
  | .udpR => udpR C s
  | .udpS => udpS R C s
  | .detach =>
    if s.pw && (s.lag4 || s.lag6) then some { s with lag4 := false, lag6 := false } else none
  | .conn w => conn R C w s
  | .shut w => shut w s
  | .pair b => pair C b s
  | .unpair b => unpair b s
  | .snap => snap R C false s
  | .snapFail => snapFail R C s
  | .snapKill => snapKill C s
  | .hostDie =>
    if s.pw && s.host && C.hostDies && s.sp != .idle then some { s with host := false } else none
  | .guard => snap R C true s
  | .restore => restore C s
  | .agent => agent R C s
  | .agentFail => agentFail R C s
  | .kill => kill C s
  | .restart => restart C s
  | .boot => boot R s
  | .off => off s
  | .timeout => timeout s
  | .watchdog => watchdog R C s

/-- A running guest that booted without the snapshot cache and that no boundary holds: the
rules its boot installs, nothing else. -/
def init (C : Cfg) : State :=
  { sc := C.sc0, ctA := if C.staleCt then some .est else none }

/-- Apply a list of labels in order. `none` if one of them is not enabled. -/
def replay (R : Rules) (C : Cfg) : List Label → State → Option State
  | [], s => some s
  | l :: ls, s =>
    match step R C l s with
    | some t => replay R C ls t
    | none => none

/-- The states the system can reach from `init`. -/
inductive Reachable (R : Rules) (C : Cfg) : State → Prop where
  | init : Reachable R C (init C)
  | step {s t : State} (l : Label) : Reachable R C s → step R C l s = some t → Reachable R C t

end Gate
