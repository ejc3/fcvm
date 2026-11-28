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
//!
//! A guest whose balloon reports free pages (`--free-page-reporting`) has a second
//! consumer of those blocks and keeps the kernel's value in every mode. The guest reports
//! free memory to the host in blocks of order 9 (2 MiB), so memory freed as scattered
//! 4 KiB pages goes back to the host only once something has gathered it into such blocks,
//! and with nothing allocating that is kcompactd's proactive pass. Measured in nine 1 GiB
//! guests per setting (`--guest-thp never --balloon 0 --free-page-reporting`; a tmpfs
//! filled with 4 KiB files until 31 to 49 MiB were free, every other file deleted, which
//! frees 245 MiB as scattered single pages, and the VMM's resident memory read 180 s
//! later): with the knob at 0 no guest gave any of it back and kcompactd migrated nothing;
//! at the kernel's 20, four guests gave back 20 to 56 MiB and kcompactd migrated pages in
//! six. Three 256 MiB guests per setting gave nothing back at either: under that load they
//! have no free 2 MiB block to report. Whether the balloon reports is read from the
//! feature its driver negotiated ([`free_page_reporting`]), which a restore cannot change.
//!
//! "The kernel's value" is what the knob held when the first fc-agent of this boot read
//! it. fc-agent can be restarted (`Restart=on-failure`), and by then the knob holds what
//! the first one wrote, so the value is saved once under `/run`
//! ([`BOOT_PROACTIVENESS_RECORD`]) and a later fc-agent takes the saved one. The knob is
//! written only when it differs from the wanted value: a write of a non-zero value wakes
//! kcompactd on every node, changed or not (`compaction_proactiveness_sysctl_handler`).

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

const ENABLED: &str = "/sys/kernel/mm/transparent_hugepage/enabled";

/// How hard kcompactd works, with no allocation asking, to keep free memory in 2 MiB
/// blocks (0 to 100).
const PROACTIVENESS: &str = "/proc/sys/vm/compaction_proactiveness";

/// The knob as the kernel booted with it, saved by the first fc-agent of this boot. `/run`
/// is a tmpfs: a snapshot carries it into the clones, which run the same booted kernel,
/// and a reboot empties it.
const BOOT_PROACTIVENESS_RECORD: &str = "/run/fcvm-compaction-proactiveness.boot";

/// The guest's virtio devices.
const VIRTIO_DEVICES: &str = "/sys/bus/virtio/devices";

/// The virtio device id of a memory balloon, as sysfs prints it.
const VIRTIO_ID_BALLOON: &str = "0x0005";

/// VIRTIO_BALLOON_F_REPORTING: the balloon has a free page reporting queue.
const VIRTIO_BALLOON_F_REPORTING: usize = 5;

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

/// Whether this guest reports free pages to the host. Read once: the balloon and the
/// features its driver negotiated are fixed from boot, across every snapshot and restore.
static FREE_PAGE_REPORTING: OnceLock<bool> = OnceLock::new();

/// Whether a balloon under `devices` negotiated free page reporting.
///
/// sysfs prints a virtio device's id in `device` and the negotiated features in
/// `features`, one `0` or `1` per bit from bit 0. No such directory, or no balloon in it,
/// is false. A balloon whose features cannot be read counts as reporting: the kernel's
/// own compaction setting is the one to keep when in doubt.
fn free_page_reporting(devices: &Path) -> bool {
    let entries = match std::fs::read_dir(devices) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[fc-agent] WARNING: reading {}: {error}", devices.display());
            }
            return false;
        }
    };
    let mut reporting = false;
    for entry in entries.flatten() {
        let device = entry.path();
        let Ok(id) = std::fs::read_to_string(device.join("device")) else {
            continue;
        };
        if id.trim() != VIRTIO_ID_BALLOON {
            continue;
        }
        match std::fs::read_to_string(device.join("features")) {
            Ok(features) => {
                reporting |= features.as_bytes().get(VIRTIO_BALLOON_F_REPORTING) == Some(&b'1');
            }
            Err(error) => {
                eprintln!(
                    "[fc-agent] WARNING: reading the features of the balloon {}: {error}",
                    device.display()
                );
                reporting = true;
            }
        }
    }
    reporting
}

/// Set proactive compaction for a guest whose huge page mode is `mode` (None: the kernel
/// has no transparent huge pages): off for `never` and None unless the guest reports free
/// pages (`reporting`), `boot` otherwise. A kernel without the knob (`boot` None) is left
/// alone, and so is a knob that already holds the wanted value. A failure is a warning:
/// the guest runs, and a snapshot of it can carry a compaction pass into its clones.
///
/// Returns whether it wrote the knob.
fn compaction_follows(
    knob: &Path,
    mode: Option<&str>,
    boot: Option<&str>,
    reporting: bool,
) -> bool {
    let Some(boot) = boot else { return false };
    let wanted = match mode {
        None | Some("never") if !reporting => "0",
        _ => boot,
    };
    match std::fs::read_to_string(knob) {
        Ok(now) if now.trim() == wanted => return false,
        Ok(_) => {}
        Err(error) => {
            eprintln!("[fc-agent] WARNING: reading {}: {error}", knob.display());
            return false;
        }
    }
    if let Err(error) = std::fs::write(knob, wanted) {
        eprintln!(
            "[fc-agent] WARNING: cannot set {} to {wanted}: {error}",
            knob.display()
        );
        return false;
    }
    true
}

/// A compaction_proactiveness value: 0 to 100.
fn proactiveness(text: &str) -> Option<String> {
    let value = text.trim();
    matches!(value.parse::<u32>(), Ok(0..=100)).then(|| value.to_string())
}

/// The knob as the kernel booted with it; None when the kernel has none.
///
/// The first fc-agent of a boot reads the knob and saves the value at `record`. Any later
/// one (fc-agent restarted in the same boot, or in a clone of it) takes the saved value:
/// by then the knob holds what an earlier fc-agent wrote. The record appears complete or
/// not at all (written beside itself, then linked into place, which fails if it exists),
/// so an fc-agent that died while saving left nothing a later one could mistake for it,
/// and it had not yet written the knob.
fn read_boot_proactiveness(knob: &Path, record: &Path) -> Option<String> {
    match std::fs::read_to_string(record) {
        Ok(saved) => match proactiveness(&saved) {
            Some(value) => return Some(value),
            None => eprintln!(
                "[fc-agent] WARNING: {} holds {saved:?}, not a compaction setting",
                record.display()
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("[fc-agent] WARNING: reading {}: {error}", record.display()),
    }
    let value = match std::fs::read_to_string(knob) {
        Ok(value) => proactiveness(&value)?,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[fc-agent] WARNING: reading {}: {error}", knob.display());
            }
            return None;
        }
    };
    let beside = record.with_extension(format!("{}.tmp", std::process::id()));
    let saved = std::fs::write(&beside, &value).and_then(|()| std::fs::hard_link(&beside, record));
    let _ = std::fs::remove_file(&beside);
    match saved {
        Ok(()) => {}
        // Another fc-agent of this boot saved first: its value is the kernel's.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(first) = std::fs::read_to_string(record)
                .ok()
                .and_then(|saved| proactiveness(&saved))
            {
                return Some(first);
            }
        }
        Err(error) => eprintln!(
            "[fc-agent] WARNING: saving the boot compaction setting at {}: {error}",
            record.display()
        ),
    }
    Some(value)
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
    let reporting =
        *FREE_PAGE_REPORTING.get_or_init(|| free_page_reporting(Path::new(VIRTIO_DEVICES)));
    let (boot, proactiveness) = record_boot_at(
        Path::new(ENABLED),
        Path::new(PROACTIVENESS),
        Path::new(BOOT_PROACTIVENESS_RECORD),
        reporting,
    );
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
fn record_boot_at(
    enabled: &Path,
    knob: &Path,
    record: &Path,
    reporting: bool,
) -> (Result<Boot>, Option<String>) {
    let proactiveness = read_boot_proactiveness(knob, record);
    let boot = read_boot(enabled);
    match &boot {
        Ok(Boot::Mode(mode)) => {
            compaction_follows(knob, Some(mode), proactiveness.as_deref(), reporting);
        }
        Ok(Boot::Unavailable) => {
            compaction_follows(knob, None, proactiveness.as_deref(), reporting);
        }
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
        *FREE_PAGE_REPORTING.get_or_init(|| free_page_reporting(Path::new(VIRTIO_DEVICES))),
    )
}

fn apply_at(
    path: &Path,
    mode: &str,
    boot: Option<&Boot>,
    knob: &Path,
    boot_proactiveness: Option<&str>,
    reporting: bool,
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
    compaction_follows(knob, Some(wanted), boot_proactiveness, reporting);
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
            apply_at(&path, "default", None, &knob, Some("20"), false).is_err(),
            "no recorded boot mode"
        );
        let always = Boot::Mode("always".to_string());
        assert!(apply_at(&path, "sometimes", Some(&always), &knob, Some("20"), false).is_err());
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
        apply_at(&path, "default", Some(&boot), &knob, Some("20"), false)
            .expect("nothing to restore");
        assert!(!path.exists(), "the default wrote nothing");
        assert!(apply_at(&path, "never", Some(&boot), &knob, Some("20"), false).is_err());
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
            let record = dir.path().join("boot");
            let _ = std::fs::remove_file(&record);
            let (boot, proactiveness) = record_boot_at(&enabled, &knob, &record, false);
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
        compaction_follows(&knob, Some("always"), Some("20"), false);
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "20");
        compaction_follows(&knob, Some("madvise"), Some("35"), false);
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "35");
        compaction_follows(&knob, Some("never"), Some("20"), false);
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "0");
        // apply_at is where a boot plan's and a restore's mode land.
        let source = include_str!("thp.rs");
        let apply = &source[source.find("fn apply_at(").unwrap()..];
        let apply = &apply[..apply.find("\n}\n").unwrap()];
        let written = apply.find("std::fs::write(path, wanted)").unwrap();
        let followed = apply
            .find("compaction_follows(knob, Some(wanted), boot_proactiveness, reporting);")
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
        let record = dir.path().join("boot");
        let (_, proactiveness) = record_boot_at(&enabled, &knob, &record, false);
        assert_eq!(proactiveness, None);
        assert!(!compaction_follows(
            &knob,
            Some("never"),
            proactiveness.as_deref(),
            false
        ));
        assert!(!knob.exists());
        assert!(
            !record.exists(),
            "nothing to save for a kernel without the knob"
        );
    }

    /// A sysfs with a network device and a balloon, the balloon's feature bits as given.
    fn virtio_devices(dir: &Path, balloon_features: &str) -> std::path::PathBuf {
        let devices = dir.join("devices");
        let _ = std::fs::remove_dir_all(&devices);
        for (name, id, features) in [
            // A network device with its own bit 5 set.
            (
                "virtio0",
                "0x0001\n",
                "1100011000000000000000000000000011\n",
            ),
            ("virtio1", "0x0005\n", balloon_features),
        ] {
            std::fs::create_dir_all(devices.join(name)).unwrap();
            std::fs::write(devices.join(name).join("device"), id).unwrap();
            std::fs::write(devices.join(name).join("features"), features).unwrap();
        }
        devices
    }

    /// The balloon's negotiated VIRTIO_BALLOON_F_REPORTING (bit 5) says whether the guest
    /// reports free pages; another device's bit 5 says nothing.
    #[test]
    fn free_page_reporting_is_the_balloons_negotiated_feature() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !free_page_reporting(&dir.path().join("devices")),
            "no virtio bus"
        );
        // Stats queue and deflate-on-OOM (bits 1 and 2), version 1 (bit 32): no reporting.
        let devices = virtio_devices(dir.path(), "0110000000000000000000000000000010\n");
        assert!(!free_page_reporting(&devices));
        let devices = virtio_devices(dir.path(), "0110010000000000000000000000000010\n");
        assert!(free_page_reporting(&devices));
        // No balloon at all.
        std::fs::remove_dir_all(devices.join("virtio1")).unwrap();
        assert!(!free_page_reporting(&devices));
    }

    /// A guest that reports free pages keeps the kernel's proactive compaction in every
    /// mode: the reports are of 2 MiB blocks, and with nothing allocating the proactive
    /// pass is what gathers freed 4 KiB pages into them.
    ///
    /// RED BEFORE THE FIX: `never` set the knob to 0 whatever the balloon did, at boot and
    /// at every restore.
    #[test]
    fn a_guest_that_reports_free_pages_keeps_proactive_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = dir.path().join("enabled");
        let knob = dir.path().join("compaction_proactiveness");
        let record = dir.path().join("boot");
        for contents in [Some("always madvise [never]\n"), None] {
            match contents {
                Some(contents) => std::fs::write(&enabled, contents).unwrap(),
                None => std::fs::remove_file(&enabled).unwrap(),
            }
            std::fs::write(&knob, "20\n").unwrap();
            let (boot, proactiveness) = record_boot_at(&enabled, &knob, &record, true);
            assert!(boot.is_ok());
            assert_eq!(proactiveness.as_deref(), Some("20"));
            assert_eq!(
                std::fs::read_to_string(&knob).unwrap().trim(),
                "20",
                "boot, {contents:?}"
            );
        }
        // A restore that applies `never` to a clone of it leaves the knob too.
        compaction_follows(&knob, Some("never"), Some("20"), true);
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "20");
        // The same guest without the reports gets 0.
        compaction_follows(&knob, Some("never"), Some("20"), false);
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "0");
        // `apply`, where a boot plan's and a restore's mode land, reads the feature.
        let source = include_str!("thp.rs");
        let code = &source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        let apply = &code[code.find("pub fn apply(mode: &str)").unwrap()..];
        let apply = &apply[..apply.find("\n}\n").unwrap()];
        assert!(
            apply.contains("free_page_reporting(Path::new(VIRTIO_DEVICES))"),
            "apply no longer reads whether the guest reports free pages"
        );
        let record_boot = &code[code.find("pub fn record_boot_default()").unwrap()..];
        let record_boot = &record_boot[..record_boot.find("\n}\n").unwrap()];
        assert!(
            record_boot.contains("free_page_reporting(Path::new(VIRTIO_DEVICES))"),
            "the boot no longer reads whether the guest reports free pages"
        );
    }

    /// The kernel's value is saved by the first fc-agent of a boot, and a restarted one
    /// takes the saved value where the knob now holds what the first wrote.
    ///
    /// RED BEFORE THE FIX: the restarted fc-agent read the first one's 0 as the kernel's
    /// value, and a restore that turned huge pages on left proactive compaction off.
    #[test]
    fn a_restarted_agent_takes_the_kernels_value_from_the_first_ones_record() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = dir.path().join("enabled");
        let knob = dir.path().join("compaction_proactiveness");
        let record = dir.path().join("boot");
        std::fs::write(&enabled, "always madvise [never]\n").unwrap();
        std::fs::write(&knob, "20\n").unwrap();
        let (_, first) = record_boot_at(&enabled, &knob, &record, false);
        assert_eq!(first.as_deref(), Some("20"));
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "0");
        assert_eq!(std::fs::read_to_string(&record).unwrap(), "20");

        // fc-agent restarts in the same boot.
        let (_, restarted) = record_boot_at(&enabled, &knob, &record, false);
        assert_eq!(
            restarted.as_deref(),
            Some("20"),
            "the 0 is fc-agent's, not the kernel's"
        );
        compaction_follows(&knob, Some("always"), restarted.as_deref(), false);
        assert_eq!(std::fs::read_to_string(&knob).unwrap().trim(), "20");

        // Nothing is left beside the record, and a record that is not a setting is not
        // believed: the knob is read instead.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.contains("tmp"))
            .collect();
        assert_eq!(names, Vec::<String>::new());
        std::fs::write(&record, "").unwrap();
        std::fs::write(&knob, "35\n").unwrap();
        assert_eq!(
            read_boot_proactiveness(&knob, &record).as_deref(),
            Some("35")
        );
        assert_eq!(proactiveness("101"), None);
        assert_eq!(proactiveness(" 100\n").as_deref(), Some("100"));
    }

    /// The knob is written only when its value has to change: a write of a non-zero value
    /// wakes kcompactd on every node, whether or not the value changed.
    ///
    /// RED BEFORE THE FIX: every call wrote the knob.
    #[test]
    fn the_knob_is_written_only_when_its_value_changes() {
        let dir = tempfile::tempdir().unwrap();
        let knob = dir.path().join("compaction_proactiveness");
        std::fs::write(&knob, "20\n").unwrap();
        assert!(!compaction_follows(
            &knob,
            Some("always"),
            Some("20"),
            false
        ));
        assert!(!compaction_follows(&knob, Some("never"), Some("20"), true));
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "20\n", "untouched");
        assert!(compaction_follows(&knob, Some("never"), Some("20"), false));
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "0");
        assert!(!compaction_follows(&knob, Some("never"), Some("20"), false));
        assert!(compaction_follows(
            &knob,
            Some("madvise"),
            Some("20"),
            false
        ));
        assert_eq!(std::fs::read_to_string(&knob).unwrap(), "20");
    }
}
