import GateVerify.Today
import GateVerify.Design

/-!
# The rule books' boundary sequence against the Rust source (`lake exe gate-seq`)

fc-agent's own tests log the boundary's steps as events (`ModelGate`, `ModelBarrier`, `ModelDiag`,
`ModelHook` in fc-agent/src/snapshot_network.rs) and pin their order in `vec![...]` literals.

`lake exe gate-seq` prints each rule book's order in those event names. `lake exe gate-seq check
<snapshot_network.rs>` reads every pinned sequence out of the Rust source and compares: it
prints the rule books whose order the source pins, and fails when there is none, so the model
cannot drift from the code without this failing. `lake exe gate-seq check <file> <rule book>`
also fails unless that rule book is among them.

What is compared. A restore's pinned sequence (it begins with `syn-cookies-off` or `gate-verify`)
must be a beginning of the rule book's sequence, with these left out where the test's run has
none: `gate-close` (the inherited gate verified), `cookie-destroy` (nothing to retire),
`restore-hook` (no hook). A snapshot's (it begins with `gate-close`) must be a beginning of
`gate-close rx-barrier cookie-dump`, with `gate-open` after it where the run rolled back. The
source must pin the whole of both at least once. The events say nothing of the steps the Rust
models do not log (the thaw, the manifest's removal, `tcp_syncookies` written back): those are
pinned by the source-order tests of `handle_clone_restore` in fc-agent/src/restore.rs.
-/

open Gate

/-- The events a restore step logs in the Rust model, each with whether a run may lack it. -/
def events : RStep → List (String × Bool)
  | .cookiesOff => [("syn-cookies-off", false)]
  | .verify => [("gate-verify", false), ("gate-close", true)]
  | .udp => [("gate-udp-ingress", false)]
  | .barOut => [("rx-barrier", false)]
  | .dump6 => [("cookie-dump", false)]
  | .egress => [("gate-open-egress", false)]
  | .retire => [("cookie-destroy", true)]
  | .hook => [("restore-hook", true)]
  | .ingress => [("gate-open", false)]
  | _ => []

def books : List Rules := [Today.main, Today.flags, Today.cookieA, Today.cookieB, Design.rules]

def restoreSeq (R : Rules) : List (String × Bool) := R.restore.flatMap events

/-- A snapshot's sequence: `SP.closed`, `SP.bar`, `SP.dump6`, the same in every rule book. -/
def prepareSeq : List String := ["gate-close", "rx-barrier", "cookie-dump"]

/-- `v` is a beginning of `r` with only optional events left out. -/
def beginsWith : List (String × Bool) → List String → Bool
  | _, [] => true
  | [], _ :: _ => false
  | (e, optional) :: r, x :: v =>
    if e == x then beginsWith r v
    else optional && beginsWith r (x :: v)

/-- `v` is all of `r` with only optional events left out. -/
def isAllOf : List (String × Bool) → List String → Bool
  | [], [] => true
  | [], _ :: _ => false
  | (_, optional) :: r, [] => optional && isAllOf r []
  | (e, optional) :: r, x :: v =>
    if e == x then isAllOf r v
    else optional && isAllOf r (x :: v)

/-- Drop an event that repeats the one before it (one `cookie-destroy` a socket). -/
def collapse : List String → List String
  | a :: b :: l => if a == b then collapse (b :: l) else a :: collapse (b :: l)
  | l => l

def isEvent (s : String) : Bool :=
  !s.isEmpty && s.all (fun c => c.isLower || c.isDigit || c == '-')

/-- Every `vec![...]` of the source that holds only event names. -/
def pinned (src : String) : List (List String) :=
  ((src.splitOn "vec![").drop 1).filterMap fun chunk =>
    let body := (chunk.splitOn "]").headD ""
    let parts := body.splitOn "\""
    -- between the quotes: the names; outside them: commas and white space only
    let names := (parts.zipIdx.filter (fun (_, i) => i % 2 == 1)).map (·.1)
    let between := (parts.zipIdx.filter (fun (_, i) => i % 2 == 0)).map (·.1)
    if parts.length % 2 == 1 && !names.isEmpty && names.all isEvent
        && names.any (·.startsWith "gate-")
        && between.all (fun s => s.all (fun c => c == ',' || c.isWhitespace)) then
      some (collapse names)
    else none

def isRestore (v : List String) : Bool :=
  v.head? == some "syn-cookies-off" || v.head? == some "gate-verify"

def okPrepare (v : List String) : Bool :=
  let w := if v.getLast? == some "gate-open" then v.dropLast else v
  w.isPrefixOf prepareSeq

/-- The source pins `R`'s order: every pinned restore begins it, and one is all of it with a
hook and something retired. -/
def matchesBook (vs : List (List String)) (R : Rules) : Bool :=
  let rs := vs.filter isRestore
  rs.all (beginsWith (restoreSeq R))
    && rs.any (fun v => isAllOf (restoreSeq R) v && v.contains "restore-hook"
                          && v.contains "cookie-destroy")

def main (args : List String) : IO UInt32 := do
  match args with
  | [] =>
    IO.println s!"prepare {" ".intercalate prepareSeq}"
    for R in books do
      IO.println s!"{R.name} {" ".intercalate ((restoreSeq R).map (·.1))}"
    return 0
  | "check" :: path :: rest =>
    let src ← IO.FS.readFile path
    let vs := pinned src
    let restores := vs.filter isRestore
    let prepares := vs.filter (fun v => !isRestore v)
    IO.println s!"{path}: {restores.length} pinned restore sequences, {prepares.length} pinned snapshot sequences"
    if restores.isEmpty || prepares.isEmpty then
      IO.eprintln "gate-seq: found no pinned sequence to compare: the check cannot run"
      return 2
    let mut ok := true
    for v in prepares do
      if !okPrepare v then
        IO.eprintln s!"gate-seq: the source pins a snapshot sequence the model does not have: {v}"
        ok := false
    if !prepares.contains prepareSeq then
      IO.eprintln s!"gate-seq: the source no longer pins the snapshot's sequence {prepareSeq}"
      ok := false
    let matching := books.filter (matchesBook vs)
    IO.println s!"rule books whose restore order the source pins: {matching.map (·.name)}"
    if matching.isEmpty then
      IO.eprintln "gate-seq: the source's pinned restore order is no rule book's. The model has drifted from the code:"
      for v in restores do IO.eprintln s!"  pinned: {" ".intercalate v}"
      for R in books do IO.eprintln s!"  {R.name}: {" ".intercalate ((restoreSeq R).map (·.1))}"
      ok := false
    match rest with
    | [want] =>
      if !(matching.map (·.name)).contains want then
        IO.eprintln s!"gate-seq: the source does not pin the order of rule book {want}"
        ok := false
    | _ => pure ()
    return if ok then 0 else 1
  | _ =>
    IO.eprintln "usage: gate-seq [check <snapshot_network.rs> [<rule book>]]"
    return 2
