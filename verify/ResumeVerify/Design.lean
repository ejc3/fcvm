import ResumeVerify.Today

/-!
# The VMMs as they should be

Each VMM with the repairs the breaks call for (`REPAIRS.md`):

1. Cloud Hypervisor's save addresses each vCPU's redistributor and CPU interface as that
   vCPU's (`slotsFromRestore := false`).
2. The VMM owns the counter offset: `KVM_ARM_SET_COUNTER_OFFSET` before anything else of a
   restore, the image holding the counter of the pause, and a resume taking out the time the
   guest did not run, also between a load and its resume (`.offset` first, `cntAtPause`,
   `freeze`, `freezeLoaded`).
3. The image holds the line of every level-triggered interrupt (`levelInfo`, `.level`).
4. The restore writes the clear-registers with the complement of the saved bits
   (`clearInverted`).
-/

namespace Resume

namespace Design

def cloudHypervisor : Rules :=
  { name := "design-cloud-hypervisor",
    slotsFromRestore := false, cntAtPause := true, freeze := true, freezeLoaded := true,
    levelInfo := true, clearInverted := true,
    restore := [.offset,
                .create false, .init false false, .cntp false, .cntv false, .regs false, .mp false,
                .create true, .init true true, .cntp true, .cntv true, .regs true, .mp true,
                .gic, .dist, .redist false, .redist true, .icc false, .icc true, .level] }

def firecracker : Rules :=
  { name := "design-firecracker",
    slotsFromRestore := false, cntAtPause := true, freeze := true, freezeLoaded := true,
    levelInfo := true, clearInverted := true,
    restore := [.offset] ++ Today.fcOrder false ++ [.level] }

end Design

end Resume
