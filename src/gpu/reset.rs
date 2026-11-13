//! When a restore's GPUs are reset, so the T4's own memory clear overlaps the restore.
//!
//! vfio-pci resets a GPU when a VMM opens it (`vfio_pci_core_enable`: `pci_try_reset_function`),
//! and the Firecracker fork resets it again through `VFIO_DEVICE_RESET`. A Turing to Ada GPU
//! then clears its memory, about 0.9 s, and the guest's nvidia-persistenced waits for it
//! (DESIGN.md, "GPU guest memory: prefilled hugepages"). The open comes late in the restore,
//! after the memory prefill and most of the snapshot load, so most of the clear lands on the
//! guest's GPU phase.
//!
//! A root restore therefore takes the reset out of the open ([`prepare_open`]): it resets each
//! GPU itself through sysfs as soon as it holds the GPU's handoff, unless a host scrub has
//! reset and cleared the GPU since its last VM (the clean token), and then writes an empty
//! `reset_method` so neither the open nor `VFIO_DEVICE_RESET` resets it again. The guard it
//! returns writes `bus` back once the load has opened the GPU, and the next restore or scrub
//! repairs a `reset_method` a killed restore left empty.
//!
//! Why every VM still gets a GPU reset since its last tenant: the restore holds the GPU's lease
//! (no scrub runs) and its handoff (no other restore takes or opens the GPU) from before this
//! reset, or before taking the token that stands for the scrub's reset, until its load has
//! opened the GPU, after which VFIO keeps the GPU to this VM. A rootless restore cannot write
//! sysfs and keeps the reset at the open. The memory property also holds without any reset: the
//! guest scrubs unless the token says the host did (src/gpu/scrub.rs, fc-agent `bring_up`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{info, warn};

/// What the open must leave alone, and what to put back.
#[must_use = "dropping the guard puts the GPUs' reset method back"]
#[derive(Debug)]
pub struct OpenWithoutReset {
    devices: Vec<PathBuf>,
}

impl Drop for OpenWithoutReset {
    fn drop(&mut self) {
        for device in &self.devices {
            if let Err(error) = write_reset_method(device, "bus") {
                warn!(device = %device.display(), error = %format!("{error:#}"),
                    "could not restore the GPU's reset method; the next restore or scrub repairs it");
            }
        }
    }
}

fn write_reset_method(device: &Path, method: &str) -> Result<()> {
    let path = device.join("reset_method");
    std::fs::write(&path, method)
        .with_context(|| format!("writing {method:?} to {}", path.display()))
}

/// Read a GPU's reset method and put `bus` back where a killed restore left none. The scrub
/// and every root restore call this before they rely on the method.
pub fn repair_reset_method(device: &Path) -> Result<String> {
    let path = device.join("reset_method");
    let method = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_string();
    if method.is_empty() {
        warn!(device = %device.display(), "a GPU's reset method was left empty; restoring bus");
        write_reset_method(device, "bus")?;
        return Ok("bus".to_string());
    }
    Ok(method)
}

/// For a restore holding the leases and the handoff of `sbdfs`: reset each GPU now unless
/// `host_scrubbed` (every GPU's clean token was taken), then keep the open from resetting it
/// again. Only a GPU whose reset method is `bus` is handled; any other is left to the open.
/// `reset` stands in for the sysfs write in tests.
pub fn prepare_open(
    sysfs: &Path,
    sbdfs: &[String],
    host_scrubbed: bool,
    reset: &dyn Fn(&Path) -> Result<()>,
) -> Result<OpenWithoutReset> {
    let mut guard = OpenWithoutReset {
        devices: Vec::with_capacity(sbdfs.len()),
    };
    for sbdf in sbdfs {
        let device = sysfs.join("bus/pci/devices").join(sbdf);
        if repair_reset_method(&device)? != "bus" {
            continue;
        }
        if !host_scrubbed {
            let started = Instant::now();
            reset(&device).with_context(|| format!("resetting GPU {sbdf}"))?;
            info!(
                sbdf,
                ms = started.elapsed().as_millis() as u64,
                "reset the GPU before the restore opens it"
            );
        }
        // From here the guard owns the method: a failure below still puts `bus` back.
        guard.devices.push(device.clone());
        // A newline, not nothing: a write of zero bytes never reaches the attribute's store,
        // and the store reads "\n" as the empty list that turns every reset method off.
        write_reset_method(&device, "\n")?;
    }
    Ok(guard)
}

/// [`prepare_open`] running on a thread of its own, beside the restore's prefill. Dropping it,
/// on any way out of the restore, waits for the thread: the reset and the reset method are
/// changed only while the restore still holds the GPUs' leases and handoff, which it releases
/// after this (they are declared before it, so they are dropped after it).
#[derive(Debug)]
pub struct EarlyReset(Option<std::thread::JoinHandle<Result<OpenWithoutReset>>>);

impl EarlyReset {
    /// Start [`prepare_open`] for `sbdfs` with the sysfs reset.
    pub fn start(sbdfs: Vec<String>, host_scrubbed: bool) -> Result<Self> {
        Self::start_with(move || {
            prepare_open(Path::new("/sys"), &sbdfs, host_scrubbed, &sysfs_reset)
        })
    }

    fn start_with(
        work: impl FnOnce() -> Result<OpenWithoutReset> + Send + 'static,
    ) -> Result<Self> {
        let thread = std::thread::Builder::new()
            .name("fcvm-gpu-reset".into())
            .spawn(work)
            .context("starting the GPU reset thread")?;
        Ok(Self(Some(thread)))
    }

    /// Wait for the reset and take the guard that keeps the open from resetting again.
    pub fn finish(mut self) -> Result<OpenWithoutReset> {
        let thread = self.0.take().expect("finished once");
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("the GPU reset thread panicked"))?
    }
}

impl Drop for EarlyReset {
    fn drop(&mut self) {
        // The guard in the result, if any, puts the reset method back as it drops.
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

/// The sysfs reset (`<device>/reset`), the one the host scrub uses.
pub fn sysfs_reset(device: &Path) -> Result<()> {
    let path = device.join("reset");
    std::fs::write(&path, "1").with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_gpu(root: &Path, sbdf: &str, method: &str) -> PathBuf {
        let device = root.join("bus/pci/devices").join(sbdf);
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("reset_method"), method).unwrap();
        device
    }

    fn method(device: &Path) -> String {
        std::fs::read_to_string(device.join("reset_method")).unwrap()
    }

    /// What the guard writes to turn the open's reset off must reach the store: a zero-byte
    /// write never does, and the kernel reads a newline as the empty list.
    #[test]
    fn the_open_reset_is_turned_off_with_a_newline() {
        let root = tempfile::tempdir().unwrap();
        let device = fake_gpu(root.path(), "0000:18:00.0", "bus\n");
        let guard = prepare_open(root.path(), &["0000:18:00.0".into()], true, &|_| Ok(())).unwrap();
        assert_eq!(method(&device), "\n");
        drop(guard);
    }

    /// Without the token the GPU is reset before the open, the open is told not to reset it,
    /// and dropping the guard puts the method back.
    #[test]
    fn a_gpu_without_a_token_is_reset_early_and_not_at_the_open() {
        let root = tempfile::tempdir().unwrap();
        let device = fake_gpu(root.path(), "0000:18:00.0", "bus\n");
        let resets = std::cell::Cell::new(0);
        let guard = prepare_open(root.path(), &["0000:18:00.0".into()], false, &|_| {
            resets.set(resets.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(resets.get(), 1, "the GPU was not reset before the open");
        assert_eq!(
            method(&device).trim(),
            "",
            "the open would reset the GPU again"
        );
        drop(guard);
        assert_eq!(method(&device), "bus");
    }

    /// With the token the host scrub's reset stands: no reset here and none at the open.
    #[test]
    fn a_host_scrubbed_gpu_is_not_reset_again() {
        let root = tempfile::tempdir().unwrap();
        let device = fake_gpu(root.path(), "0000:18:00.0", "bus\n");
        let guard = prepare_open(root.path(), &["0000:18:00.0".into()], true, &|_| {
            panic!("a host-scrubbed GPU was reset again")
        })
        .unwrap();
        assert_eq!(method(&device).trim(), "");
        drop(guard);
        assert_eq!(method(&device), "bus");
    }

    /// A method a killed restore left empty is repaired, and that GPU is reset like any other;
    /// a GPU on another method is left to the open.
    #[test]
    fn an_empty_method_is_repaired_and_another_method_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let left = fake_gpu(root.path(), "0000:18:00.0", "\n");
        let other = fake_gpu(root.path(), "0000:19:00.0", "flr\n");
        let resets = std::cell::Cell::new(0);
        let guard = prepare_open(
            root.path(),
            &["0000:18:00.0".into(), "0000:19:00.0".into()],
            false,
            &|_| {
                resets.set(resets.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(resets.get(), 1);
        assert_eq!(method(&left).trim(), "");
        assert_eq!(
            method(&other),
            "flr\n",
            "a GPU on another reset method was changed"
        );
        drop(guard);
        assert_eq!(method(&left), "bus");
    }

    /// A restore that leaves before it collects the reset waits for it, and the method is back
    /// to `bus` when it has left, so nothing changes the GPU after its locks are released.
    #[test]
    fn dropping_an_early_reset_waits_for_it_and_restores_the_method() {
        let root = tempfile::tempdir().unwrap();
        let device = fake_gpu(root.path(), "0000:18:00.0", "bus\n");
        let sysfs = root.path().to_path_buf();
        let task = EarlyReset::start_with(move || {
            prepare_open(&sysfs, &["0000:18:00.0".into()], false, &|_| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(())
            })
        })
        .unwrap();
        drop(task);
        assert_eq!(
            method(&device),
            "bus",
            "the reset was still running after the drop"
        );
    }

    /// A reset that fails leaves the method as it was and fails the restore.
    #[test]
    fn a_failed_reset_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let device = fake_gpu(root.path(), "0000:18:00.0", "bus\n");
        let error = prepare_open(root.path(), &["0000:18:00.0".into()], false, &|_| {
            anyhow::bail!("no")
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("resetting GPU"));
        assert_eq!(method(&device), "bus\n");
    }
}
