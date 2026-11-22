//! The guest's transparent huge page mode (`--guest-thp`).
//!
//! A clone's memory is a copy-on-write view of its snapshot, so every guest page
//! it writes becomes a private host page. With THP `always` a fault anywhere in
//! a large enough anonymous mapping allocates and zeroes a whole 2 MiB page, and
//! the clone pays for all 512 pages it wrote. The mode is applied at boot from
//! the boot plan and again at every restore from the restore document, so it is
//! not part of the snapshot key: a restore sets its own.
//!
//! `default` names the mode the guest kernel booted with, recorded here before
//! anything changes it. A run that gives no mode and restores a snapshot made by
//! one that did gets the kernel's own mode back, as its cold boot would have.
//! A kernel built without THP has no `enabled` file: `default` is then nothing to
//! restore, and an explicit mode is an error.
//!
//! Proactive memory compaction follows the mode. With the kernel's default
//! `vm.compaction_proactiveness` of 20, kcompactd wakes every half second and migrates
//! pages whenever free memory is too fragmented to hold 2 MiB blocks, which in a small
//! guest is most of the time after boot. Transparent huge pages are what those blocks are
//! for, so in a guest whose mode is `never` the passes buy nothing, and a snapshot taken
//! during one carries it into every clone: the clone's first tens of milliseconds are
//! kcompactd copying pages, each copy two guest pages the clone faults in ahead of its own
//! restore. Of 8 snapshots of a 256 MiB guest taken when it first reported healthy, 4 held
//! a pass, and their clones took 52 to 64 ms from resume to fc-agent's readiness where the
//! other 4 took 34 to 39 ms. So while the mode is `never`, or the kernel has no
//! transparent huge pages, the knob is 0; with any other mode it is what the kernel booted
//! with. The kernel itself boots a guest of less than 512 MiB with `never`, and those are
//! the guests whose free memory is fragmented enough for the passes to run. Compaction on
//! demand is never touched: an allocation that needs a contiguous block still compacts
//! for it, as does growing the hugetlb pool.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

const ENABLED: &str = "/sys/kernel/mm/transparent_hugepage/enabled";

/// How hard kcompactd works, with no allocation asking, to keep free memory in 2 MiB
/// blocks (0 to 100).
const PROACTIVENESS: &str = "/proc/sys/vm/compaction_proactiveness";

/// The modes the kernel accepts in `enabled`.
const MODES: [&str; 3] = ["always", "madvise", "never"];

/// What the kernel booted with.
#[derive(Debug, PartialEq)]
enum Boot {
    Mode(String),
    /// No `enabled` file: the kernel has no transparent huge pages.
    Unavailable,
}

static BOOT_DEFAULT: OnceLock<Boot> = OnceLock::new();

/// The compaction knob as the kernel booted with it; None when the kernel has none.
static BOOT_PROACTIVENESS: OnceLock<Option<String>> = OnceLock::new();

/// Set proactive compaction for a guest whose huge page mode is `mode` (None: the kernel
/// has no transparent huge pages): off for `never` and None, `boot` otherwise. A kernel
/// without the knob (`boot` None) is left alone. A failure is a warning: the guest runs,
/// and a snapshot of it can carry a compaction pass into its clones.
fn compaction_follows(knob: &Path, mode: Option<&str>, boot: Option<&str>) {
    let Some(boot) = boot else { return };
    let wanted = match mode {
        None | Some("never") => "0",
        Some(_) => boot,
    };
    if let Err(error) = std::fs::write(knob, wanted) {
        eprintln!(
            "[fc-agent] WARNING: cannot set {} to {wanted}: {error}",
            knob.display()
        );
    }
}

fn read_boot_proactiveness(knob: &Path) -> Option<String> {
    match std::fs::read_to_string(knob) {
        Ok(value) => Some(value.trim().to_string()),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[fc-agent] WARNING: reading {}: {error}", knob.display());
            }
            None
        }
    }
}

/// The selected mode in the contents of `enabled`: the bracketed word of
/// `always [madvise] never`.
fn selected(contents: &str) -> Option<&str> {
    let start = contents.find('[')? + 1;
    let end = start + contents[start..].find(']')?;
    Some(&contents[start..end])
}

fn read_selected(path: &Path) -> Result<String> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    selected(&contents)
        .map(str::to_string)
        .with_context(|| format!("{} has no selected mode: {contents:?}", path.display()))
}

fn read_boot(path: &Path) -> Result<Boot> {
    if !path.try_exists().unwrap_or(true) {
        return Ok(Boot::Unavailable);
    }
    read_selected(path).map(Boot::Mode)
}

/// Record the kernel's boot-time mode and compaction knob, and set the knob for that
/// mode. Called once, before any mode is applied and before any snapshot of the guest.
pub fn record_boot_default() {
    let (boot, proactiveness) = record_boot_at(Path::new(ENABLED), Path::new(PROACTIVENESS));
    let _ = BOOT_PROACTIVENESS.set(proactiveness);
    match boot {
        Ok(boot) => {
            if boot == Boot::Unavailable {
                eprintln!("[fc-agent] transparent huge pages: the kernel has none");
            }
            let _ = BOOT_DEFAULT.set(boot);
        }
        Err(error) => eprintln!("[fc-agent] transparent huge pages: {error:#}"),
    }
}

/// Read the boot-time mode and compaction knob, and set the knob for that mode. A mode
/// that cannot be read leaves the knob as the kernel booted with it.
fn record_boot_at(enabled: &Path, knob: &Path) -> (Result<Boot>, Option<String>) {
    let proactiveness = read_boot_proactiveness(knob);
    let boot = read_boot(enabled);
    match &boot {
        Ok(Boot::Mode(mode)) => compaction_follows(knob, Some(mode), proactiveness.as_deref()),
        Ok(Boot::Unavailable) => compaction_follows(knob, None, proactiveness.as_deref()),
        Err(_) => {}
    }
    (boot, proactiveness)
}

/// Apply `mode` (`always`, `madvise`, `never` or `default`).
pub fn apply(mode: &str) -> Result<()> {
    apply_at(
        Path::new(ENABLED),
        mode,
        BOOT_DEFAULT.get(),
        Path::new(PROACTIVENESS),
        BOOT_PROACTIVENESS.get().and_then(|boot| boot.as_deref()),
    )
}

fn apply_at(
    path: &Path,
    mode: &str,
    boot: Option<&Boot>,
    knob: &Path,
    boot_proactiveness: Option<&str>,
) -> Result<()> {
    let wanted = match mode {
        "default" => match boot {
            Some(Boot::Mode(mode)) => mode.as_str(),
            Some(Boot::Unavailable) => return Ok(()),
            None => bail!("the boot-time transparent huge page mode is unknown"),
        },
        mode if MODES.contains(&mode) => {
            if boot == Some(&Boot::Unavailable) {
                bail!("the guest kernel has no transparent huge pages to set to {mode}");
            }
            mode
        }
        other => bail!("unknown transparent huge page mode {other:?}"),
    };
    std::fs::write(path, wanted)
        .with_context(|| format!("writing {wanted} to {}", path.display()))?;
    let now = read_selected(path)?;
    if now != wanted {
        bail!("{} selects {now} after writing {wanted}", path.display());
    }
    compaction_follows(knob, Some(wanted), boot_proactiveness);
    eprintln!("[fc-agent] transparent huge pages: {wanted}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selected_mode_is_the_bracketed_word() {
        assert_eq!(selected("always [madvise] never\n"), Some("madvise"));
        assert_eq!(selected("[always] madvise never"), Some("always"));
        assert_eq!(selected("always madvise never"), None);
    }

    #[test]
    fn default_needs_the_recorded_boot_mode_and_unknown_modes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enabled");
        std::fs::write(&path, "[always] madvise never").unwrap();
        let knob = dir.path().join("compaction_proactiveness");
        assert!(
            apply_at(&path, "default", None, &knob, Some("20")).is_err(),
            "no recorded boot mode"
        );
        let always = Boot::Mode("always".to_string());
        assert!(apply_at(&path, "sometimes", Some(&always), &knob, Some("20")).is_err());
        // Neither refusal wrote anything.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[always] madvise never"
        );
        assert!(!knob.exists());
    }

    /// A kernel without THP boots without `enabled`. A restore that asks for no mode
    /// sends `default`, which must not fail that clone; an explicit mode still fails.
    #[test]
    fn a_kernel_without_huge_pages_restores_the_default_and_refuses_a_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enabled");
        let boot = read_boot(&path).unwrap();
        assert_eq!(boot, Boot::Unavailable);
        let knob = dir.path().join("compaction_proactiveness");
        apply_at(&path, "default", Some(&boot), &knob, Some("20")).expect("nothing to restore");
        assert!(!path.exists(), "the default wrote nothing");
        assert!(apply_at(&path, "never", Some(&boot), &knob, Some("20")).is_err());
        assert!(!path.exists(), "the refusal wrote nothing");
    }

    /// A guest that boots without transparent huge pages in use (`never`, or a kernel
    /// built without them) gets proactive compaction off at once; one that boots with
    /// them keeps the kernel's value.
    ///
    /// RED BEFORE THE FIX: nothing wrote the knob, and every guest ran with the kernel's 20.
    #[test]
    fn a_guest_without_huge_pages_does_not_compact_ahead_of_demand() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = dir.path().join("enabled");
        let knob = dir.path().join("compaction_proactiveness");
        let boots = [
            (Some("always madvise [never]\n"), "0"),
            (None, "0"),
            (Some("always [madvise] never\n"), "20"),
            (Some("[always] madvise never\n"), "20"),
        ];
        for (mode, expected) in boots {
            match mode {
                Some(contents) => std::fs::write(&enabled, contents).unwrap(),
                None => std::fs::remove_file(&enabled).unwrap(),
            }
            std::fs::write(&knob, "20\n").unwrap();
            let (boot, proactiveness) = record_boot_at(&enabled, &knob);
            assert!(boot.is_ok());
            assert_eq!(proactiveness.as_deref(), Some("20"));
            assert_eq!(
                std::fs::read_to_string(&knob).unwrap().trim(),
                expected,
                "{mode:?}"
            );
        }
    }

    /// The knob follows every later mode: a restore that turns huge pages on gives the
    /// kernel's value back, and one that turns them off sets 0 again.
    #[test]
    fn compaction_follows_the_mode_a_restore_applies() {
        let dir = tempfile::tempdir().unwrap();
        let knob = dir.path().join("compaction_proactiveness");
        std::fs::write(&knob, "0").unwrap();
        compaction_follows(&knob, Some("always"), Some("20"));
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "20");
        compaction_follows(&knob, Some("madvise"), Some("35"));
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "35");
        compaction_follows(&knob, Some("never"), Some("20"));
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "0");
        // apply_at is where a boot plan's and a restore's mode land.
        let source = include_str!("thp.rs");
        let apply = &source[source.find("fn apply_at(").unwrap()..];
        let apply = &apply[..apply.find("\n}\n").unwrap()];
        let written = apply.find("std::fs::write(path, wanted)").unwrap();
        let followed = apply
            .find("compaction_follows(knob, Some(wanted), boot_proactiveness);")
            .expect("apply_at no longer sets the compaction knob for the mode it applied");
        assert!(written < followed);
    }

    /// A kernel without compaction has no knob: nothing is created, whatever the mode.
    #[test]
    fn a_kernel_without_the_knob_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = dir.path().join("enabled");
        std::fs::write(&enabled, "always madvise [never]\n").unwrap();
        let knob = dir.path().join("compaction_proactiveness");
        let (_, proactiveness) = record_boot_at(&enabled, &knob);
        assert_eq!(proactiveness, None);
        compaction_follows(&knob, Some("never"), proactiveness.as_deref());
        assert!(!knob.exists());
    }
}
