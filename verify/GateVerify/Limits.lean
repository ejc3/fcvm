import GateVerify.Properties
import GateVerify.Design
import GateVerify.Configs

/-!
# Connections inside the guest: what the retirement does to them, and where the model stops

Three cases, each a run the kernel replays, and two more the model cannot express.

**A connection with one end on the guest's own address** (a client bound to the guest's
address, a loopback listener: `selfC`, `selfS`). The server end records a peer that is not
loopback, so the manifest names it; the client end records a loopback peer and has its mirror
image, so the manifest leaves it out. The clone destroys the server end by cookie.
`SOCK_DESTROY` is `tcp_abort`, which sends a reset (Linux v6.18.60, net/ipv4/tcp.c:5058-5110);
the reset leaves through OUTPUT, where the gate refuses only a SYN and NTP, and comes in on
`lo`, which the gate's INPUT chain returns for (`gate_chain_rules`), so the client end is reset
and freed too. The whole connection goes, which is what snapshot_network.rs says of a
connection over an address a restore can take from the guest. No end is left alone, and
nothing of the clone's own is destroyed (`mixedPairGoes`). A model without the reset shows the
clone publishing with the client end alone; that end is an artefact of leaving the reset out.

This holds for a guest that still has the address the reset is sent to. The model's kinds are
IPv4 and it has no address move, so that is every guest in it. It is NOT every real guest: see
"A routed clone" below.

**The manifest's second look.** A loopback connection made between the source's IPv4 dump and
its IPv6 dump has one end in the dump. Without the second look the manifest names that end, the
clone destroys it, and the reset takes the other: a loopback connection inside the guest, both
ends alive in the image, is destroyed in every clone (`manifestOneLook`). With the look the
manifest leaves both out. The proof has the obligation as clause `gML` of `Gn` (`Gens.lean`):
with `look := false` in `Design.rules` the build fails at `manifest_loop`.

**One end alone in the kernel.** In `step`, a connection inside the guest gets both its sockets
in one step (`pair`) and loses both in one step (`unpair`). The kernel can hold one end alone,
with nothing destroyed by anyone:

- the other end was closed and its socket freed. An orphaned socket in FIN-WAIT-2 becomes a
  TIME-WAIT socket for `tcp_fin_time` (net/ipv4/tcp_input.c:7133-7146, tcp.c:3250-3256), which
  is the socket's `TCP_LINGER2` or else `tcp_fin_timeout`, 60 s by default
  (include/net/tcp.h:142, 1795-1805), and a dump still shows it; after that the end that did
  not close stays alone in CLOSE-WAIT for as long as its owner keeps it;
- the handshake has not been answered: a SYN to a loopback listener whose accept queue is full
  is dropped (tcp_input.c:7458-7461, `tcp_conn_request`), and the client's socket waits alone
  in SYN-SENT.

`loseEnd` is that: one end goes, the other stays. It is NOT a step of the model, and the proof
of P2 (`design_fixedGeneration`) uses that it is not: clause `gP` of `Gn` says a socket a
process of the clone opened inside the guest has its mirror image.

- Lost in a clone, before the restore's list, by a process outside the container (the container
  is frozen): the restore lists the end that is left, its second look finds no mirror image, and
  the retirement destroys a socket the clone's own process opened (`loneEndRetired`). It needs
  a clone that ran for the FIN timeout before its restore took its list, or a loopback listener
  with a full accept queue at that moment. The owner of the socket reads an error and connects
  again. This is an accepted cost of the mirror rule, to be documented with it (PR #2124), and
  P2 is proved for the model's connections, not for this one.
- Lost in the source before the snapshot: the end is in the manifest, every clone retires it,
  and P2 as written allows it (`loneEndInImage`). snapshot_network.rs says so of
  `manifest_sockets`: "one whose owner now reads an error where it would have read
  end-of-file".

**A routed clone: a stated limit, not in the model.** A routed restore moves the guest from the
snapshot's global IPv6 address to the clone's own (`network::reconfigure_ipv6`), and it does so
before egress opens and so before the retirement (`before_egress` in `restore_boundary_with`,
snapshot_network.rs:2522 at 27add63dd). Take a connection inside the guest from the snapshot's
global IPv6 address to a `::1` listener. The manifest names the server end alone, as in the
first case. The clone destroys it, and its reset is addressed to the snapshot's address, which
the guest no longer has: the reset does not come back on `lo`, and the client end stays, alone,
for as long as its owner keeps it. So for a routed clone "one end destroyed and the other left"
IS a state the kernel leaves, under every rule book here. Found by the delta review of this model, by
source, not run; low, since the connection is one the snapshot named for retirement and the
end that stays is dead. The model has no address move and its connections inside the guest are
IPv4, so no theorem here is about it, and `mixedPairGoes` is about a guest that keeps its
addresses.

**A `/loopback` client that picks its source port: a stated limit, not in the model (#2152).**
The mirror rule keeps a socket whose peer is loopback and whose mirror image is in the list.
For a `/loopback` port the guest rewrites the client's source address to 127.0.0.1 and keeps
its source port (fc-agent/src/network.rs, `--to-source 127.0.0.1` with no port range), so the
client chooses the peer port the server's socket records. A client that connects to guest port
P from source port P makes a socket that is its own mirror image; with two such ports, a
connection to each from the other's number makes two sockets that are each other's. The mirror
rule takes them for a connection inside the guest: the manifest leaves them out, the restore's
list keeps them, and the clone publishes holding a connection of the source's from a client
outside. A break of P3 at 27add63dd and so in the design, found by the head review of #2124, by
reading, not run.

The model cannot make it. A socket's kind fixes its tuple, and `inb c lb` has no mirror image
(`Kind.mirror`): a client's source port is not something a run can choose. So
`design_noStaleAtPublication` is a theorem about clients that do not choose their source port
to match, and `outside_mirror` (`Socks.lean`: a connection that leaves the guest has no mirror
image), on which that proof rests, is exactly what #2152 shows to be false of the code. Making
it a kind needs the repair with it (a source port range reserved for the rewrite, and a
loopback peer in that range counted as outside the guest), which is being built on #2124's
branch; the model gets both when that lands.
-/

namespace Gate

set_option maxRecDepth 100000

/-- One end of a connection inside the guest is freed; the other stays. -/
def loseEnd (selfs : Bool) (s : State) : Option State :=
  if !s.running || !s.has (pairS selfs) then none else some (s.drop (pairS selfs))

/-- A snapshot of the source, up to the pause. -/
def Run.snapshot : List Label := [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap]

/-- The restore, through its retirement: eleven steps. -/
def Run.toRetirement : List Label :=
  [.agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

/-- A connection from the guest's own address to a loopback listener, in the image. The manifest
names its server end; the clone's retirement destroys that end and the reset takes the client
end. Nothing is left, and nothing of the clone's own was destroyed. -/
theorem mixedPairGoes :
    after (replay Design.rules Runs.pairs ([.pair true] ++ Run.snapshot ++ [.restore])
        (init Runs.pairs))
      (fun s => s.man == some [2] && s.socks.length == 2)
    ∧ after (replay Design.rules Runs.pairs
        ([.pair true] ++ Run.snapshot ++ [.restore] ++ Run.toRetirement) (init Runs.pairs))
      (fun s => !generationBroken s && s.socks.isEmpty && s.cut) := by decide

/-- The design without the manifest's second look: a loopback connection made between the
source's two dumps is destroyed in the clone. -/
theorem manifestOneLook :
    after (replay { Design.rules with look := false } Runs.pairs
        ([.snap, .snap, .snap, .snap, .snap, .snap, .pair false, .snap, .snap, .snap, .restore]
          ++ Run.toRetirement) (init Runs.pairs))
      generationBroken
    ∧ ¬ after (replay Design.rules Runs.pairs
        ([.snap, .snap, .snap, .snap, .snap, .snap, .pair false, .snap, .snap, .snap, .restore]
          ++ Run.toRetirement) (init Runs.pairs))
      generationBroken := by decide

/-- A clone is restored, a process outside the container opens a loopback connection and its
server end is freed. The restore's retirement destroys the client end. -/
theorem loneEndRetired :
    after (((replay Design.rules Runs.pairs (Run.snapshot ++ [.restore, .pair false])
        (init Runs.pairs)).bind (loseEnd false)).bind
          (replay Design.rules Runs.pairs Run.toRetirement))
      generationBroken := by decide

/-- The same end lost before the snapshot is the image's: the manifest names it, the clone
retires it, and no socket of the clone's own is destroyed. -/
theorem loneEndInImage :
    after ((((replay Design.rules Runs.pairs [.pair false] (init Runs.pairs)).bind
        (loseEnd false)).bind
          (replay Design.rules Runs.pairs (Run.snapshot ++ [.restore]))).bind
            (replay Design.rules Runs.pairs Run.toRetirement))
      (fun s => !generationBroken s && s.socks.isEmpty && s.cut) := by decide

end Gate
