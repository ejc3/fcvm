import GateVerify.Properties
import GateVerify.Design
import GateVerify.Configs

/-!
# What the proof of P2 rests on: a connection inside the guest keeps both its ends

In `step`, a connection inside the guest gets both its sockets in one step (`pair`) and loses
both in one step (`unpair`). The proof that a retirement destroys nothing it must not
(`design_fixedGeneration`, `Proofs.lean`) uses it: clause `gP` of `Gn` (`Gens.lean`) says a
socket a process of the clone opened inside the guest has its mirror image, so the restore's
second look takes it off the list.

The kernel can hold one end alone. Two ways, in Linux v6.18.60:

- the other end was closed and its socket freed. An orphaned socket in FIN-WAIT-2 becomes a
  TIME-WAIT socket for `tcp_fin_time` (net/ipv4/tcp_input.c:7133-7146, tcp.c:3250-3256), which
  is the socket's `TCP_LINGER2` or else `tcp_fin_timeout`, 60 s by default
  (include/net/tcp.h:142, 1795-1805), and a dump still shows it; after that the end that did
  not close stays alone in CLOSE-WAIT for as long as its owner keeps it;
- the handshake has not been answered: a SYN to a loopback listener whose accept queue is full
  is dropped (tcp_input.c:7458-7461, `tcp_conn_request`), and the client's socket waits alone
  in SYN-SENT.

`loseEnd` is that: one end goes, the other stays. It is NOT a step of the model. The two runs
below add it by hand to a run of the design and say what follows.

- Lost in a clone, before the restore's list, by a process outside the container (the container
  is frozen): the restore lists the end that is left, its second look finds no mirror image, and
  the retirement destroys a socket the clone's own process opened (`loneEndRetired`). So P2 is
  proved for the model's connections and does not hold for this one. For the first way the
  clone must have run for the FIN timeout before its restore's list; for the second a loopback
  listener's queue must be full.
- Lost in the source before the snapshot: the end is in the manifest, every clone retires it,
  and P2 as written allows it (`loneEndInImage`). snapshot_network.rs says so of
  `manifest_sockets`: "one whose owner now reads an error where it would have read
  end-of-file".

Whether the first is a break to repair or a cost to accept is not for the model to say. Making
`loseEnd` a step, and the rule that would keep P2 under it, wait for that decision.
-/

namespace Gate

set_option maxRecDepth 100000

/-- One end of a connection inside the guest is freed; the other stays. -/
def loseEnd (selfs : Bool) (s : State) : Option State :=
  if !s.running || !s.has (pairS selfs) then none else some (s.drop (pairS selfs))

/-- A snapshot of the source, up to the pause. -/
def Run.snapshot : List Label := [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap]

/-- The restore, through its retirement: twelve steps. -/
def Run.toRetirement : List Label :=
  [.agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

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
