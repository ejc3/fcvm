import GateVerify.Today

/-!
# The repaired system

`Design.rules` is the boundary with every repair the search and the proofs called for. Each is
one field, and `verify/GateVerify/REPAIRS.md` has the change it stands for and the wording
proposed. Against `main` (`Today.owed`: da5f2bb22 with #2256), five of them have landed and three have not:

| field | repair | against | in `main` |
|---|---|---|---|
| `snapUnpublished := false` | 2: `snapshot create` refuses a clone that has not published | the resume one-shot publishing an unrestored clone | #2151, #2154 |
| `reopenRetry := true` | 3: a source's reopen is owed until it lands, tried `retryMax` times; then the create fails, the source is left running and the host records it as failed and still gated and refuses further creates of it | a failed `snapshot create`, and an fc-agent that died before the reopen | #2154 |
| `guard := true` | 3: a dead `snapshot create`'s part is done by a guard | a `snapshot create` that dies | #2256 |
| `rcuUndo := true` | 4: the resume one-shot writes `rcu_expedited` back, whether or not its reopen lands | a prepare killed inside the barrier | #2154 |
| `natGate := true` | 5: the proxy's redirect closes with the egress half | a connect behind the closed egress half | no |
| `wdMidBoundary := false` | 6: the freeze watchdog leaves a freeze alone while its boundary is in progress | a workload thawed behind the closed gate | #2154 |
| `backFails := true` | 7: a write of `tcp_syncookies` back that keeps failing fails the restore | a clone published for good without SYN cookies | no |
| `restartKeeps := true` | 9: a restarted fc-agent gets a container running again (#2158) | an agent's death ending the VM, a source's among them | no: #2158 |
| `startWaits := true` | 8: a restarted fc-agent holds the container's start while a boundary is in progress | a container started, over vsock, while a prepare holds the guest, and captured running | #2154 |

`execFirst` is `false` here: `main`'s agent opens exec before its boot plan when it finds the
boundary owed (`Today.owed`), and the design does not. The design's proofs hold without it: a
source whose agent cannot come back behind a closed egress half is reported by the host after
its tries (repair 3). `main`'s exec-first opens such a source's gate instead of reporting it.

Repair 1 of the first version of this model (a record of the restore under /run, for an
fc-agent that is restarted inside a restore) is withdrawn: a restarted agent never runs a
restore again (`Spec.restart`), so nothing reads such a record. The restore's order is
27add63dd's, unchanged in `main` since.
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
  restartKeeps := true
  startWaits := true
  execFirst := false
  guard := true
  rcuUndo := true

end Design

end Gate
