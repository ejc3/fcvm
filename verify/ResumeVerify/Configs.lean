import ResumeVerify.Spec

/-!
# The configurations the search runs

Each fixes what exists around the guest and where host time may pass while the guest does not
run. The bounds are the search's; the proofs of `Proofs.lean` hold for every `Cfg`.
-/

namespace Resume

def Cfg.base : Cfg :=
  { nv := false, l2 := false, spiLevel := false, device := true, cpu1Off := false,
    maxSnaps := 2, maxRestores := 2, maxIdeal := 1, maxWaits := 0,
    waitBeforeSave := false, waitAfterSave := false, waitLoaded := false }

/-- name, what it is, the configuration, and whether the guest is a hypervisor's (which VMMs
run it) -/
def runs : List (String × String × Cfg) :=
  [("plain", "two CPUs, an edge-triggered device, two snapshots, two restores; no host time passes while paused",
      Cfg.base),
   ("level", "the device's interrupt is level-triggered and the device holds its line",
      { Cfg.base with spiLevel := true, maxSnaps := 1, maxRestores := 1 }),
   ("offline", "guest CPU 1 is offline",
      { Cfg.base with cpu1Off := true, maxSnaps := 1, maxRestores := 1 }),
   ("dump", "host time passes after the save: the memory dump, and the wait for a resume or a restore",
      { Cfg.base with device := false, maxSnaps := 1, maxRestores := 1, maxWaits := 1, waitAfterSave := true }),
   ("lags", "host time passes between the pause and the save, and between a load and its resume",
      { Cfg.base with device := false, maxSnaps := 1, maxRestores := 1, maxWaits := 2,
                      waitBeforeSave := true, waitLoaded := true }),
   ("nv-dump", "a guest hypervisor (NV2); host time passes after the save",
      { Cfg.base with nv := true, device := false, maxSnaps := 1, maxRestores := 1, maxWaits := 1,
                      waitAfterSave := true }),
   ("nv-l2", "a guest hypervisor with a nested vCPU loaded (CNTVOFF_EL2 not zero); no host time passes",
      { Cfg.base with nv := true, l2 := true, device := false, maxSnaps := 1, maxRestores := 1 }),
   ("nv-lags", "a guest hypervisor; host time passes between the pause and the save, and between a load and its resume",
      { Cfg.base with nv := true, device := false, maxSnaps := 1, maxRestores := 1, maxWaits := 2,
                      waitBeforeSave := true, waitLoaded := true })]

end Resume
