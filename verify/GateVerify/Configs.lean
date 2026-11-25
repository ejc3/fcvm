import GateVerify.Spec

/-!
# The configurations the search runs

Each turns on the moves one question needs, and bounds the search (`Cfg.maxId` socket cookies,
`Cfg.maxGens` restores). `Breaks.lean` replays its runs under the same values, so a recorded run
is a run of the configuration the search found it in.
-/

namespace Gate

namespace Runs

/-- No nat rules, so the guest tracks nothing unless a rule asks; two clients. -/
def routed : Cfg :=
  { cliB := true, maxId := 3 }

/-- No nat rules, and the tracker holds an ESTABLISHED entry for client A's tuple (#2106). -/
def routedStale : Cfg :=
  { staleCt := true, maxId := 3 }

/-- IPv4 nat rules at boot (a `/loopback` port's), so the guest always tracks IPv4 and tracks
IPv6 only while a rule asks; two clients. -/
def nat : Cfg :=
  { nat := true, cliB := true, maxId := 3 }

/-- A listener in SYN-queue overflow answers with cookies (#2123). -/
def cookie : Cfg :=
  { flood := true, maxId := 3 }

/-- A port published with /loopback, and SYN cookies. -/
def loopbackPort : Cfg :=
  { nat := true, lb := true, flood := true, maxId := 3 }

/-- Connections inside the guest, made while a list is taken. -/
def pairs : Cfg :=
  { loops := true, selfs := true, maxId := 5 }

/-- The egress proxy, and a process outside the container that connects out. -/
def proxy : Cfg :=
  { nat := true, nat6 := true, proxy := true, dmnOut := true, maxId := 5 }

/-- No proxy; the container and another process connect out. -/
def egress : Cfg :=
  { dmnOut := true, ctrOut := true, maxId := 4 }

/-- Any step of the restore or of a one-shot can fail; SYN cookies. -/
def fails : Cfg :=
  { fails := true, flood := true, maxId := 3 }

/-- fc-agent or a one-shot can be killed at any step, in a source and in a clone; a process
connects out. The boot plan is over MMDS (Firecracker). -/
def kills : Cfg :=
  { kills := true, dmnOut := true, maxId := 3 }

/-- The host's snapshot create can die, and a step of it can fail. -/
def host : Cfg :=
  { hostDies := true, fails := true, maxId := 2 }

/-- A boundary that outlives the payload freeze's deadline; the container connects out. -/
def slow : Cfg :=
  { slow := true, ctrOut := true, maxId := 3 }

/-- UDP,
    the guest tracking all the time; the host keeps client datagrams from a clone until it has
    published. -/
def udp : Cfg :=
  { udp := true, nat := true, maxId := 2 }

/-- UDP, and nothing on the host side keeps a datagram from a clone. -/
def udpReach : Cfg :=
  { udp := true, udpReach := true, maxId := 2 }

/-- The host is asked for a snapshot of a clone that has not published. -/
def early : Cfg :=
  { earlySnap := true, maxId := 2 }

/-- The same, and a step can fail. -/
def earlyFails : Cfg :=
  { earlySnap := true, fails := true, maxId := 2 }

/-- Two restores, kills and failures: a snapshot of a published clone. -/
def twoGen : Cfg :=
  { maxGens := 2, kills := true, fails := true, maxId := 2 }

/-- Everything at once without nat rules: failures, kills, a dying host, slow boundaries,
    early snapshots, cookies, pairs, outbound connects. -/
def allRouted : Cfg :=
  { cliB := true, flood := true, loops := true, dmnOut := true, ctrOut := true, fails := true,
    kills := true, hostDies := true, slow := true, earlySnap := true, maxId := 3 }

/-- Everything at once with the egress proxy and a /loopback port. -/
def allProxy : Cfg :=
  { nat := true, nat6 := true, proxy := true, lb := true, flood := true, selfs := true,
    dmnOut := true, ctrOut := true, fails := true, kills := true, hostDies := true,
    slow := true, earlySnap := true, maxId := 3 }

end Runs

/-- The runs of `lake exe gate-breaks`: a name, what it is for, the configuration. -/
def runs : List (String × String × Cfg) := [
  ("routed", "no nat rules, so the guest tracks nothing unless a rule asks; two clients",
    Runs.routed),
  ("routed-stale", "no nat rules, and the tracker holds an ESTABLISHED entry for A's tuple", Runs.routedStale),
  ("nat", "IPv4 nat rules at boot (a /loopback port's): IPv4 always tracked; two clients",
    Runs.nat),
  ("cookie", "a listener in SYN-queue overflow answers with cookies (#2123)", Runs.cookie),
  ("loopback-port", "a port published with /loopback, and SYN cookies", Runs.loopbackPort),
  ("pairs", "connections inside the guest, made while a list is taken", Runs.pairs),
  ("proxy", "the egress proxy, and a process outside the container that connects out", Runs.proxy),
  ("egress", "no proxy; the container and another process connect out", Runs.egress),
  ("fails", "any step of the restore or of a one-shot can fail; SYN cookies", Runs.fails),
  ("kills", "fc-agent or a one-shot can be killed at any step; a process connects out", Runs.kills),
  ("host", "the host's snapshot create can die, and a step of it can fail", Runs.host),
  ("slow", "a boundary that outlives the payload freeze's deadline; the container connects out",
    Runs.slow),
  ("udp", "UDP, the guest tracking all the time; the host gates client datagrams", Runs.udp),
  ("udp-reach", "UDP, and nothing on the host side keeps a datagram from a clone", Runs.udpReach),
  ("early", "the host is asked for a snapshot of a clone that has not published", Runs.early),
  ("early-fails", "the same, and a step can fail", Runs.earlyFails),
  ("two-gen", "two restores, kills and failures: a snapshot of a published clone", Runs.twoGen),
  ("all-routed", "everything at once, no nat rules", Runs.allRouted),
  ("all-proxy", "everything at once, the egress proxy and a /loopback port", Runs.allProxy)]

end Gate
