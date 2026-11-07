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

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

const ENABLED: &str = "/sys/kernel/mm/transparent_hugepage/enabled";

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

/// Record the kernel's boot-time mode. Called once, before any mode is applied.
pub fn record_boot_default() {
    match read_boot(Path::new(ENABLED)) {
        Ok(boot) => {
            if boot == Boot::Unavailable {
                eprintln!("[fc-agent] transparent huge pages: the kernel has none");
            }
            let _ = BOOT_DEFAULT.set(boot);
        }
        Err(error) => eprintln!("[fc-agent] transparent huge pages: {error:#}"),
    }
}

/// Apply `mode` (`always`, `madvise`, `never` or `default`).
pub fn apply(mode: &str) -> Result<()> {
    apply_at(Path::new(ENABLED), mode, BOOT_DEFAULT.get())
}

fn apply_at(path: &Path, mode: &str, boot: Option<&Boot>) -> Result<()> {
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
        assert!(
            apply_at(&path, "default", None).is_err(),
            "no recorded boot mode"
        );
        let always = Boot::Mode("always".to_string());
        assert!(apply_at(&path, "sometimes", Some(&always)).is_err());
        // Neither refusal wrote anything.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[always] madvise never"
        );
    }

    /// A kernel without THP boots without `enabled`. A restore that asks for no mode
    /// sends `default`, which must not fail that clone; an explicit mode still fails.
    #[test]
    fn a_kernel_without_huge_pages_restores_the_default_and_refuses_a_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enabled");
        let boot = read_boot(&path).unwrap();
        assert_eq!(boot, Boot::Unavailable);
        apply_at(&path, "default", Some(&boot)).expect("nothing to restore");
        assert!(!path.exists(), "the default wrote nothing");
        assert!(apply_at(&path, "never", Some(&boot)).is_err());
        assert!(!path.exists(), "the refusal wrote nothing");
    }
}
