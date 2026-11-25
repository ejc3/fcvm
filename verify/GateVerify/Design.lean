import GateVerify.Today

/-!
# The repaired system

`Design.rules` is PR #2124 at its head (27add63dd, `Today.cookieB`) with the repairs the search
and the proofs called for. Each is one field, and `verify/GateVerify/REPAIRS.md` has the change
it stands for and the wording proposed:

| field | repair | against |
|---|---|---|
| `snapUnpublished := false` | 2: `snapshot create` refuses a clone that has not published | the resume one-shot publishing an unrestored clone |
| `guard := true`, `reopenRetry := true` | 3: a source's reopen is owed until it lands, tried `retryMax` times, and then the host ends the guest it could not reopen | a dead or failed `snapshot create`, and an fc-agent that died before the reopen |
| `rcuUndo := true` | 4: the reopen writes `rcu_expedited` back | a prepare killed inside the barrier |
| `natGate := true` | 5: the proxy's redirect closes with the egress half | a connect behind the closed egress half |
| `wdMidBoundary := false` | 6: the freeze watchdog leaves a freeze alone while its boundary is in progress | a workload thawed behind the closed gate |
| `backFails := true` | 7: a write of `tcp_syncookies` back that keeps failing fails the restore | a clone published for good without SYN cookies |
| `startWaits := true` | 8: an fc-agent that starts behind a closed egress half waits for it to open | a container started, over vsock, while a boundary holds the guest, and captured running |

Repair 1 of the first version of this model (a record of the restore under /run, for an
fc-agent that is restarted inside a restore) is withdrawn: a restarted agent never runs a
restore again (`Spec.restart`), so nothing reads such a record. The restore's order is
27add63dd's, unchanged.
-/

namespace Gate

namespace Design

def rules : Rules where
  name := "design"
  tcpByCt := false
  manAddr := false
  list := .mirror
  look := true
  restore := Today.cookieB.restore
  natGate := true
  wdMidBoundary := false
  snapUnpublished := false
  reopenRetry := true
  backFails := true
  startWaits := true
  guard := true
  rcuUndo := true

end Design

end Gate
