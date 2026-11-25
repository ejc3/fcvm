import GateVerify.Spec

/-!
# The properties

Each is a `Bool` on one state that says the property is BROKEN there, so the search can test it
on every reachable state and the kernel can check a counterexample (`Breaks.lean`). The property
itself is that the test is false in every reachable state; `Proofs.lean` proves that for the
design. `verify/README.md` has each in words, from the requirements.

Some are read off the state. The others are about a step (what a retirement destroyed, where the
hook ran), and the step leaves a record in the state that the test reads.
-/

namespace Gate

/-- **P1, source undisturbed.** An established connection of a guest being snapshotted was reset
by the gate; or no boundary is in progress (`snapshot create` returned, with or without an
error, on a guest that is a source or a published clone) and the guest still has a half of the
gate, the manifest, or `rcu_expedited` on. -/
def sourceDisturbed (s : State) : Bool :=
  s.rst || (s.pw && s.sp == .idle && (s.rp == .none || s.rp == .done)
              && (s.gated || s.man.isSome || s.rcu))

/-- **P2, fixed generation.** A restore destroyed a socket that a process of the clone opened, or
one end of a connection inside the guest that the manifest does not name and whose other end is
alive: something that is neither the snapshot's nor made from outside behind the closed gate. -/
def generationBroken (s : State) : Bool := s.bad

/-- **P3, no stale or smuggled socket at publication.** Ingress is open in some family on a
guest that was restored, and it holds a TCP socket whose other end is outside the guest and that
was in the image or made behind the closed gate. -/
def staleAtPublication (s : State) : Bool :=
  s.pw && s.rp != .none && (!s.in4 || !s.in6)
    && s.socks.any (fun x => x.kind.outside && (x.gen == .stale || x.gen == .smuggled))

/-- P3, the other half: a packet from outside made a socket in a clone after its restore had
retired the snapshot's sockets and before it published. -/
def smuggledLate (s : State) : Bool := s.late

/-- **P4, egress before retirement.** Sockets were retired with the egress half closed. -/
def retiredBehindEgress (s : State) : Bool := s.eg

/-- P4: the hook ran with egress closed, or with ingress open. -/
def hookMisplaced (s : State) : Bool := s.hk

/-- P4, as a state: a clone's restore is running, the egress half is closed in some family, and
either the container runs or the restore has already retired sockets. A process that connects
is answered with the gate's reset. -/
def workloadBehindEgress (s : State) : Bool :=
  s.pw && (s.out4 || s.out6) && (s.cut || !s.frozen)
    && (match s.rp with
        | .at _ => true
        | _ => false)

/-- **P5, no lasting change to the clone.** A published clone that no snapshot holds has
something the restore changed and did not put back: `tcp_syncookies`, the UDP rule, the
restore's record. (A half of the gate and `rcu_expedited` are P1's, for a source and a published
clone alike.) -/
def lastingChange (C : Cfg) (s : State) : Bool :=
  s.pw && s.rp == .done && s.sp == .idle && (s.sc != C.sc0 || s.udp || s.jr)

/-- P5: a memory image was taken while a setting was changed. -/
def capturedChange (s : State) : Bool := s.cap

/-- The steps of the system itself that move a boundary on: not the environment's, and not a
failure or a kill. -/
def progressLabels : List Label := [.snap, .guard, .agent, .restart, .off, .timeout]

/-- **P6, never left closed.** A half of the gate is installed, or the guest is paused, and no
step of the system is enabled: nothing will open it and nothing will report it. -/
def stuck (R : Rules) (C : Cfg) (s : State) : Bool :=
  s.pw && (s.gated || s.sp == .paused)
    && !(progressLabels.any (fun l => (step R C l s).isSome))

/-- **U1, no client datagram before publication.** A datagram on a flow a client began reached a
clone that had not published. -/
def clientDatagram (s : State) : Bool := s.udpIn

/-- Not a property: a datagram on a flow the guest itself began reached a clone that had not
published. The UDP rule passes it by design, as a reply (`udp_ingress_rule_args`). -/
def replyDatagram (s : State) : Bool := s.udpLate

/-- The property holds in the state a run ends in. A run that is not possible proves nothing. -/
def after (o : Option State) (broken : State → Bool) : Prop :=
  match o with
  | some s => broken s = true
  | none => False

instance (o : Option State) (broken : State → Bool) : Decidable (after o broken) := by
  unfold after
  cases o <;> infer_instance

end Gate
