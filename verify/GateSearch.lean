import Std.Data.HashMap
import GateVerify.Properties
import GateVerify.Today
import GateVerify.Design
import GateVerify.Configs

/-!
# Exhaustive search of the network boundary (`lake exe gate-breaks`)

For each rule book and each configuration, a breadth-first search visits every state reachable
from `init` and tests every property of `GateVerify/Properties.lean` on it. The search is bounded
by the configuration (`Cfg.maxId` socket cookies, `Cfg.maxGens` restores), so it ends: it reports,
for each property, that no reachable state breaks it, or the shortest run that does.

A state in which a step has left a record of a break (`rst`, `bad`, `late`, `eg`, `hk`, `cap`,
`udpIn`, `udpLate`) is not searched further: the record never clears, so everything past it
would be reported as that break again.

`lake exe gate-breaks` prints the table. `lake exe gate-breaks traces` also prints each shortest
run, step by step and as a Lean term for `GateVerify/Breaks.lean`. A run's name or a rule book's
name as an argument limits the search to it; the two `all-` runs are searched only when named.
-/

open Gate

/-! ## Labels -/

def allLabels : Array Label := #[
  .pkt .a .plain .syn, .pkt .a .plain .synFull, .pkt .a .plain .ack, .pkt .a .plain .fin,
  .pkt .a .lb .syn, .pkt .a .lb .synFull, .pkt .a .lb .ack, .pkt .a .lb .fin,
  .pkt .b .plain .syn, .pkt .b .plain .synFull, .pkt .b .plain .ack, .pkt .b .plain .fin,
  .udpC, .udpR, .udpS, .detach,
  .conn .ctr, .conn .dmn, .shut .ctr, .shut .dmn,
  .pair false, .pair true, .unpair false, .unpair true,
  .snap, .snapFail, .snapKill, .hostDie, .guard, .restore,
  .agent, .agentFail, .kill, .restart, .off, .timeout, .watchdog]

def strip (s : String) : String :=
  ((s.replace "Gate." "").replace "Label." "").replace "RStep." ""

def labelTerm (l : Label) : String :=
  match l with
  | .pkt c p seg =>
    s!".pkt .{strip ((reprStr c).replace "Cli." "")} .{strip ((reprStr p).replace "Port." "")} .{strip ((reprStr seg).replace "Seg." "")}"
  | .conn w => s!".conn .{strip ((reprStr w).replace "Who." "")}"
  | .shut w => s!".shut .{strip ((reprStr w).replace "Who." "")}"
  | .pair b => s!".pair {b}"
  | .unpair b => s!".unpair {b}"
  | l => "." ++ strip (reprStr l)

/-! ## Printing -/

def cliName : Cli → String
  | .a => "client A (IPv4)"
  | .b => "client B (IPv6)"

def portName : Port → String
  | .plain => "the plain port"
  | .lb => "the /loopback port"

def whoName : Who → String
  | .ctr => "the container"
  | .dmn => "a process outside the container"

def stepName : RStep → String
  | .cookiesOff => "refuse_syn_cookies: writes 0 to tcp_syncookies"
  | .verify => "verifies the inherited gate (is_closed), closing it if it does not verify"
  | .udp => "gate_udp_ingress: adds the UDP rule"
  | .barIn => "receive barrier: turns rcu_expedited on"
  | .barOut => "receive barrier: grace period waited, rcu_expedited back"
  | .load => "store.load(): reads the manifest"
  | .dump4 => "its own list: dumps the IPv4 sockets"
  | .dump6 => "its own list: dumps the IPv6 sockets and compares with the manifest"
  | .look => "its own list: dumps once more for a missing mirror image"
  | .egress => "open_egress: removes the egress half"
  | .retire => "SOCK_DESTROY of the manifest's cookies and the list's"
  | .disarm => "store.remove(): removes the manifest"
  | .thaw => "payload_freeze::thaw: the container runs"
  | .hook => "runs the restore hook"
  | .ingress => "open: removes the rest of the gate (ingress opens)"
  | .cookiesBack => "restore_syn_cookies: writes tcp_syncookies back"

def snapName (R : Rules) (s : State) (byGuard : Bool) : String :=
  let who := if byGuard then "the guard" else "the host"
  let reopen (what : String) : String :=
    if R.reopenRetry && retryMax ≤ s.tries then
      s!"{who} ends the guest it could not reopen"
    else if s.ag then s!"{what}: the gate is opened"
    else if R.reopenRetry then s!"{what}: no fc-agent answers the exec; it will try again"
    else s!"{what}: no fc-agent answers the exec; snapshot create returns its error"
  match s.sp with
  | .idle => "the host starts snapshot create: the prepare one-shot freezes the container"
  | .frozen => "prepare: close, IPv4 chains and hooks"
  | .closed4 => "prepare: close, IPv6 chains and hooks"
  | .closed => "prepare: receive barrier, rcu_expedited on"
  | .barIn => "prepare: grace period waited, rcu_expedited back"
  | .bar => "prepare: dumps the IPv4 sockets"
  | .dump4 => "prepare: dumps the IPv6 sockets"
  | .dump6 => "prepare: saves the manifest and exits 0"
  | .saved => if byGuard then reopen "the guard runs the resume one-shot"
              else "the host pauses the VM: the memory image is this state"
  | .paused => s!"{who} resumes the VM (the run stays with the source)"
  | .resumed => reopen s!"{who} runs the resume one-shot"
  | .aborted => reopen s!"{who} runs the resume one-shot after the failed prepare"
  | .opened => "resume one-shot: removes the manifest, thaws the container, exits"

def describe (R : Rules) (l : Label) (s : State) : String :=
  match l with
  | .pkt c p seg =>
    let what := match seg with
      | .syn => "sends a SYN"
      | .synFull => "sends a SYN that finds the SYN queue full"
      | .ack => "sends an ACK"
      | .fin => "closes its connection"
    s!"{cliName c} {what} to {portName p}"
  | .udpC => "a client sends a datagram on a flow it began"
  | .udpR => "a datagram arrives from the peer of a flow the guest began"
  | .udpS => "a process of the guest sends a datagram"
  | .detach => "connection tracking's hooks are detached"
  | .conn w => s!"{whoName w} connects to a server outside the guest"
  | .shut w => s!"{whoName w} closes its outbound connection"
  | .pair b => if b then "a process connects to a loopback listener from the guest's own address"
               else "a process opens a loopback connection inside the guest"
  | .unpair _ => "that connection inside the guest is closed"
  | .snap => snapName R s false
  | .guard => snapName R s true
  | .snapFail => match s.sp with
    | .resumed | .aborted =>
      if R.reopenRetry then "the resume one-shot fails to open the gate; it will be tried again"
      else "the resume one-shot fails to open the gate; it removes the manifest, thaws and exits non-zero"
    | _ => "the prepare step fails: the one-shot rolls back (gate opened, thaw) and exits non-zero"
  | .snapKill => "the prepare one-shot is killed"
  | .hostDie => "the host's snapshot create dies"
  | .restore => "a clone is restored from the image and resumed (the run follows the clone)"
  | .agent => match nextStep R s with
    | some r => if r == .load && s.man.isNone then "restore: store.load() finds no manifest: the restore fails closed"
                else if r == .cookiesBack && R.backFails && retryMax ≤ s.tries then
                  "restore: the write of tcp_syncookies has failed three times: the restore fails"
                else s!"restore: {stepName r}"
    | none => "restore"
  | .agentFail => match nextStep R s with
    | some .cookiesBack =>
      if R.backFails then "restore: the write of tcp_syncookies fails; it is tried again"
      else "restore: the write of tcp_syncookies fails three times; the restore goes on with a warning"
    | some r => s!"restore: FAILS at the step that {stepName r}; the restore fails closed"
    | none => "restore fails"
  | .kill => "fc-agent is killed; systemd kills the container and any one-shot with it"
  | .restart => "systemd restarts fc-agent: it fetches its boot plan and runs its boot path"
  | .off => "shutdown_vm(1): the clone powers off"
  | .timeout => "the host gives up waiting for the restore and ends the VM"
  | .watchdog => "the payload-freeze watchdog thaws the container"

def showSock (x : Sock) : String :=
  let k := (strip (reprStr x.kind)).replace "Kind." ""
  let g := (strip (reprStr x.gen)).replace "Gen." ""
  s!"{k}#{x.id}{if x.st == .syn then "(syn)" else ""}:{g}"

def showState (s : State) : String :=
  if !s.pw then "powered off" else
  let b (n : String) (v : Bool) : String := if v then " " ++ n else ""
  let rp := match s.rp with
    | .none => "" | .at n => s!" restore@{n}" | .failed => " restore-failed" | .done => " published"
  let sp := if s.sp == .idle then "" else " snap=" ++ (strip (reprStr s.sp)).replace "SP." ""
  s!"gate[{b "in4" s.in4}{b "in6" s.in6}{b "out4" s.out4}{b "out6" s.out6}{b "udp" s.udp} ]"
    ++ s!" syncookies={if s.sc then 1 else 0}" ++ b "rcu_expedited" s.rcu ++ b "frozen" s.frozen
    ++ (match s.man with
        | some m => s!" manifest={m}"
        | none => "")
    ++ (if s.lst.isEmpty then "" else s!" list={s.lst}")
    ++ sp ++ rp ++ b "agent-dead" (!s.ag) ++ b "container-dead" (!s.up) ++ b "host-dead" (!s.host)
    ++ (if s.tries == 0 then "" else s!" tries={s.tries}") ++ b "old-epoch" s.again
    ++ s!" socks={s.socks.map showSock}"
    ++ (match s.ctA with
        | some c => " ct(A)=" ++ (strip (reprStr c)).replace "CtSt." ""
        | none => "")
    ++ (match s.ctL with
        | some c => " ct(A,lb)=" ++ (strip (reprStr c)).replace "CtSt." ""
        | none => "")
    ++ (match s.ctB with
        | some c => " ct(B)=" ++ (strip (reprStr c)).replace "CtSt." ""
        | none => "")
    ++ b "ct(udp-in)" s.ctUin ++ b "ct(udp-out)" s.ctUout ++ b "tracker-lag4" s.lag4
    ++ b "tracker-lag6" s.lag6

/-! ## The properties, by name -/

def checks (R : Rules) (C : Cfg) : Array (String × (State → Bool)) := #[
  ("P1 sourceDisturbed", sourceDisturbed),
  ("P2 generationBroken", generationBroken),
  ("P3 staleAtPublication", staleAtPublication),
  ("P3 smuggledLate", smuggledLate),
  ("P4 retiredBehindEgress", retiredBehindEgress),
  ("P4 hookMisplaced", hookMisplaced),
  ("P4 workloadBehindEgress", workloadBehindEgress),
  ("P5 lastingChange", lastingChange C),
  ("P5 capturedChange", capturedChange),
  ("P6 stuck", stuck R C),
  ("U1 clientDatagram", clientDatagram),
  ("-- replyDatagram", replyDatagram)]

/-- A step has left a record of a break. -/
def recorded (s : State) : Bool :=
  s.rst || s.bad || s.late || s.eg || s.hk || s.cap || s.udpIn || s.udpLate

/-! ## The search -/

structure Found where
  states : Array State
  parent : Array (Nat × Nat)
  /-- per check: the first state found that breaks it -/
  first : Array (Option Nat)
  complete : Bool

partial def explore (R : Rules) (C : Cfg) (limit : Nat) : Found := Id.run do
  let cs := checks R C
  let s0 := init C
  let mut states : Array State := #[s0]
  let mut parent : Array (Nat × Nat) := #[(0, 0)]
  let mut index : Std.HashMap State Nat := (Std.HashMap.emptyWithCapacity 4096).insert s0 0
  let mut first : Array (Option Nat) := cs.map (fun _ => none)
  let mut next := 0
  let mut complete := true
  while next < states.size do
    let s := states[next]!
    for ci in [0:cs.size] do
      if first[ci]!.isNone && cs[ci]!.2 s then first := first.set! ci (some next)
    if !recorded s then
      for li in [0:allLabels.size] do
        match step R C allLabels[li]! s with
        | none => pure ()
        | some t =>
          if !index.contains t then
            index := index.insert t states.size
            states := states.push t
            parent := parent.push (next, li)
    next := next + 1
    if states.size > limit then
      complete := false
      break
  return { states, parent, first, complete }

partial def pathTo (f : Found) (i : Nat) : List (Nat × Nat) :=
  let rec go (i : Nat) (acc : List (Nat × Nat)) : List (Nat × Nat) :=
    if i == 0 then acc else
      let (p, li) := f.parent[i]!
      go p ((p, li) :: acc)
  go i []

/-! ## Configurations -/

structure Run where
  name : String
  what : String
  cfg : Cfg

/-- The configurations searched (`GateVerify/Configs.lean`). -/
def allRuns : List Run := Gate.runs.map (fun (n, w, c) => { name := n, what := w, cfg := c })

def books : List Rules :=
  [Today.main, Today.flags, Today.cookieA, Today.cookieB, Today.cookieBOneLook, Design.rules]

/-- Print one run: its steps in words, the state it ends in, the run as a Lean term, and the
same run under the design. -/
def printRun (R : Rules) (r : Run) (title : String) (check : String) (labels : List Label) :
    IO Unit := do
  IO.println s!"\n### {title}: {check} ({labels.length} steps)"
  let mut st := init r.cfg
  let mut n := 1
  let mut possible := true
  for l in labels do
    if possible then
      IO.println s!"  {n}. {describe R l st}"
      n := n + 1
      match step R r.cfg l st with
      | some t => st := t
      | none => possible := false
  let holds := match (checks R r.cfg).find? (·.1 == check) with
    | some (_, f) => f st
    | none => false
  if !possible then IO.println "  NOT POSSIBLE under this rule book"
  else if !holds then IO.println "  NOT BROKEN at its end under this rule book"
  IO.println s!"  => {showState st}"
  IO.println s!"  [{", ".intercalate (labels.map labelTerm)}]"
  -- The same run under the design. Where the run has the agent do a restore step, the design
  -- does its own steps up to and including that one, so the two restores are at the same
  -- place whatever their orders. Then: is the run possible, and does the test hold at its end.
  let mut so := init r.cfg
  let mut sd := init r.cfg
  let mut out : List Label := []
  let mut refused := false
  for l in labels do
    if refused then break
    let todo : List Label :=
      if l == .agent then
        match nextStep R so with
        | some rs =>
          if Design.rules.restore.contains rs then
            -- how many design steps until `rs` has run
            match sd.rp with
            | .at n =>
              match Design.rules.restore.findIdx? (· == rs) with
              | some i => if n ≤ i then List.replicate (i - n + 1) Label.agent else []
              | none => []
            -- the design's restore is over: it has done this step already
            | .done => []
            | _ => [Label.agent]
          else []
        | none => [l]
      else [l]
    match step R r.cfg l so with
    | some t => so := t
    | none => pure ()
    for d in todo do
      if !refused then
        out := out ++ [d]
        match step Design.rules r.cfg d sd with
        | some t => sd := t
        | none => refused := true
  let test := (checks Design.rules r.cfg).find? (·.1 == check)
  let broken := match test with
    | some (_, f) => f sd
    | none => false
  IO.println s!"  design-run: [{", ".intercalate (out.map labelTerm)}]"
  if refused then
    IO.println s!"  design: refuses step {out.length}"
  else
    IO.println s!"  design: runs, {if broken then "broken" else "holds"}"


def printTrace (R : Rules) (r : Run) (f : Found) (check : String) (i : Nat) : IO Unit :=
  printRun R r s!"{R.name} / {r.name}" check ((pathTo f i).map (fun (_, li) => allLabels[li]!))

/-- Runs written by hand: a break the search reports only through a shorter run of another
kind, written out so that it is recorded as itself. `Breaks.lean` checks each. -/
def handRuns : List (String × Rules × String × String × List Label) := [
  ("staleTupleGrown", Today.main, "routed", "P3 staleAtPublication",
    [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
     .pkt .a .plain .syn, .pkt .a .plain .ack, .snap, .detach, .pkt .a .plain .fin,
     .snap, .snap, .snap, .snap, .snap, .snap, .pkt .a .plain .syn, .snap, .snap, .snap,
     .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
     .agent]),
  ("reusedTuple", Today.main, "nat", "P3 smuggledLate",
    [.pkt .a .plain .syn, .pkt .a .plain .ack,
     .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .restore,
     .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pkt .a .plain .syn]),
  ("barrierKill", Today.cookieB, "kills", "P1 sourceDisturbed",
    [.snap, .snap, .snap, .snap, .snapKill, .snap, .snap]),
  ("failedCloneSnapshot", Today.cookieB, "early-fails", "P5 capturedChange",
    [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .restore, .agent,
     .agentFail, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap])]

def main (args : List String) : IO Unit := do
  let traces := args.contains "traces"
  let limits := args.filterMap (fun a => if a.startsWith "limit=" then (a.drop 6).toNat? else none)
  let named := args.filter (fun a => a != "traces" && !a.startsWith "limit=")
  let bookNames := books.map (·.name)
  let onlyBooks := named.filter (bookNames.contains ·)
  let only := named.filter (!bookNames.contains ·)
  let limit := limits.head?.getD 20000000
  for r in allRuns do
    -- the two `all-` runs are large: they run when named
    if (only.isEmpty && !r.name.startsWith "all-") || only.contains r.name then
      IO.println s!"\n## {r.name}: {r.what}"
      for R in books.filter (fun R => onlyBooks.isEmpty || onlyBooks.contains R.name) do
        let t0 ← IO.monoMsNow
        let f := explore R r.cfg limit
        let t1 ← IO.monoMsNow
        let cs := checks R r.cfg
        let mut line := s!"{R.name}: {f.states.size} states{if f.complete then "" else " (LIMIT REACHED)"}, {t1 - t0} ms;"
        for ci in [0:cs.size] do
          match f.first[ci]! with
          | some i => line := line ++ s!" {cs[ci]!.1}@{(pathTo f i).length}"
          | none => pure ()
        IO.println line
        (← IO.getStdout).flush
        if traces then
          for ci in [0:cs.size] do
            match f.first[ci]! with
            | some i => printTrace R r f cs[ci]!.1 i
            | none => pure ()
  if traces && named.isEmpty then
    for (name, R, run, check, labels) in handRuns do
      match allRuns.find? (·.name == run) with
      | some r => printRun R r s!"HAND {name} / {R.name} / {r.name}" check labels
      | none => pure ()
