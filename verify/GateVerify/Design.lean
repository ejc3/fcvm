import GateVerify.Today

/-!
# The repaired system

`Design.rules` is PR #2124 at its head (27add63dd, `Today.cookieB`) with the repairs the search
and the proofs called for. Each is one field, and `verify/REPAIRS.md` has the change it stands
for and the wording proposed:

| field | repair | against |
|---|---|---|
| `restore` begins with `journal` and ends with `unjournal` | 1: the restore's changes are owned by a record under /run | a killed agent, a failed write-back |
| `rerun := false` | 1: a restarted agent that finds the record fails closed; one that finds a published epoch does nothing | the restore running twice |
| `snapUnpublished := false` | 2: `snapshot create` refuses a clone that has not published | the resume one-shot publishing an unrestored clone |
| `guard := true`, `reopenRetry := true` | 3: a source's reopen is owed until it lands | a dead or failed `snapshot create` |
| `rcuUndo := true` | 4: the reopen writes `rcu_expedited` back | a prepare killed inside the barrier |
| `natGate := true` | 5: the proxy's redirect closes with the egress half | a connect behind the closed egress half |
| `wdMidBoundary := false` | 6: the freeze watchdog leaves a freeze alone while its boundary is in progress | a workload thawed behind the closed gate |
-/

namespace Gate

namespace Design

def rules : Rules where
  name := "design"
  tcpByCt := false
  manAddr := false
  list := .mirror
  look := true
  restore := [.journal, .cookiesOff, .verify, .udp, .barIn, .barOut, .load, .dump4, .dump6,
              .look, .egress, .retire, .disarm, .thaw, .hook, .ingress, .cookiesBack, .unjournal]
  natGate := true
  wdMidBoundary := false
  snapUnpublished := false
  rerun := false
  reopenRetry := true
  guard := true
  rcuUndo := true

end Design

end Gate
