//! Host-side scrub of a GPU's memory between tenants, and the quarantine of a
//! GPU whose scrub fails.
//!
//! A GPU goes from one VM to the next. fc-agent zeroes its memory inside each
//! new VM before the container starts (fc-agent/src/gpu.rs). This is the
//! second layer, run on the host after a VM's Firecracker has exited and
//! before the GPU is handed to another VM: `fcvm gpu scrub <sbdf>`.
//!
//! On Turing, Ampere and Ada the GPU clears its own memory after a secondary
//! bus reset, and reports in BAR0 when that clear has finished. This follows
//! NVIDIA's gpu-admin-tools for `--reset-with-sbr --clear-memory`: hold the
//! GPU in D0 (`power/control` on; an idle GPU on vfio-pci is in D3, where BAR0
//! reads all ones), set a scratch register the reset clears (VBIOS scratch 22,
//! `0x1458`), reset the bus, require the scratch to read 0, then wait for boot
//! (`0x118234 & 0x3ff == 0x3ff`) and for the clear (`0x100b20 & 1`). Hopper
//! and later report both elsewhere and are refused, as is anything older
//! than Turing.
//!
//! While it runs, the scrub holds the GPU's VFIO group open. A group has one
//! opener at a time, so no VM can open the GPU in the middle of a scrub, and a
//! GPU some VM still holds is refused as busy rather than reset under it.
//!
//! Every outcome is recorded per GPU under [`state_dir`]: scrubs done,
//! failures, and whether the GPU is quarantined. A failed scrub quarantines
//! the GPU, and `--gpu` refuses a quarantined GPU, until a later scrub of it
//! succeeds.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where the per-GPU records live: host-wide, not under a user's data
/// directory, because a rootless run must see a quarantine that root set.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/fcvm/gpu";

/// The records' directory: `FCVM_GPU_STATE_DIR`, else [`DEFAULT_STATE_DIR`].
pub fn state_dir() -> PathBuf {
    std::env::var_os("FCVM_GPU_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR))
}

const NVIDIA_VENDOR: &str = "0x10de";
/// `NV_PMC_BOOT_0`: bits 28:24 are the architecture.
const PMC_BOOT_0: u64 = 0x0;
/// VBIOS scratch 22: set before the reset, it must read 0 after it.
const RESET_SCRATCH: u64 = 0x1458;
const BOOT_COMPLETE: u64 = 0x118234;
const BOOT_COMPLETE_MASK: u32 = 0x3ff;
const MEMORY_CLEAR_FINISHED: u64 = 0x100b20;
/// The registers above all sit in the first 2 MiB of BAR0.
const BAR0_MAP_BYTES: usize = 2 << 20;
/// PCI command register, and its memory-space enable bit.
const PCI_COMMAND: u64 = 0x4;
const PCI_COMMAND_MEMORY: u16 = 0x2;
/// gpu-admin-tools waits 5 s for each of boot and the clear.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// The architectures whose clear this module knows how to wait for.
fn architecture(boot0: u32) -> Option<&'static str> {
    match (boot0 >> 24) & 0x1f {
        0x16 => Some("turing"),
        0x17 => Some("ampere"),
        0x19 => Some("ada"),
        _ => None,
    }
}

/// A read that did not reach the GPU: all ones (no response) or NVIDIA's
/// 0xbadfxxxx (the unit is not up).
fn unreadable(value: u32) -> bool {
    value == 0xffff_ffff || value >> 16 == 0xbadf
}

/// The scrub found the GPU's VFIO group open: a VM still holds the GPU.
#[derive(Debug)]
pub struct GpuBusy(pub String);

impl std::fmt::Display for GpuBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GpuBusy {}

/// Milliseconds spent in each part of a successful scrub.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScrubTimings {
    pub architecture: String,
    pub reset_ms: f64,
    pub boot_ms: f64,
    pub clear_ms: f64,
    pub total_ms: f64,
}

fn ms(since: Instant) -> f64 {
    (since.elapsed().as_secs_f64() * 1e6).round() / 1e3
}

/// The first 2 MiB of BAR0, mapped through sysfs `resource0`.
struct Bar0 {
    map: memmap2::MmapMut,
}

impl Bar0 {
    fn open(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {} (needs root)", path.display()))?;
        // SAFETY: a device BAR, accessed with volatile 32-bit loads and stores
        // only; nothing else in this process maps it.
        let map = unsafe {
            memmap2::MmapOptions::new()
                .len(BAR0_MAP_BYTES)
                .map_mut(&file)
        }
        .with_context(|| format!("mapping {}", path.display()))?;
        Ok(Self { map })
    }

    fn word(&self, offset: u64) -> *mut u32 {
        let offset = offset as usize;
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.map.len());
        // SAFETY: in bounds and aligned (checked above).
        unsafe { self.map.as_ptr().add(offset) as *mut u32 }
    }

    fn read32(&self, offset: u64) -> u32 {
        // SAFETY: `word` returns an in-bounds, aligned pointer into the map.
        unsafe { std::ptr::read_volatile(self.word(offset)) }
    }

    fn write32(&self, offset: u64, value: u32) {
        // SAFETY: as for read32; the map is writable.
        unsafe { std::ptr::write_volatile(self.word(offset), value) }
    }

    /// The register's first readable value within `timeout`, or the last read.
    fn wait_readable(&self, offset: u64, timeout: Duration) -> u32 {
        let started = Instant::now();
        loop {
            let value = self.read32(offset);
            if !unreadable(value) || started.elapsed() >= timeout {
                return value;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Wait until `read32(offset) & mask == mask` from a readable register.
    fn poll(&self, name: &str, offset: u64, mask: u32, timeout: Duration) -> Result<f64> {
        let started = Instant::now();
        loop {
            let value = self.read32(offset);
            if !unreadable(value) && value & mask == mask {
                return Ok(ms(started));
            }
            if started.elapsed() >= timeout {
                bail!(
                    "{name} (BAR0 {offset:#x}) did not reach {mask:#x} within {timeout:?}; \
                     last read {value:#010x}"
                );
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Restores the PCI command register on drop if the scrub had to turn on
/// memory decoding to read BAR0.
struct CommandRestore {
    config: std::fs::File,
    original: Option<u16>,
}

impl Drop for CommandRestore {
    fn drop(&mut self) {
        if let Some(original) = self.original {
            let _ = self
                .config
                .write_all_at(&original.to_le_bytes(), PCI_COMMAND);
        }
    }
}

/// Restores `power/control` on drop. vfio-pci lets an idle GPU suspend to D3
/// (`auto`), where its BAR0 reads all ones; the scrub holds it in D0 (`on`).
struct PowerRestore {
    path: PathBuf,
    original: String,
}

impl Drop for PowerRestore {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.original);
    }
}

fn hold_in_d0(device: &Path) -> Result<PowerRestore> {
    let path = device.join("power/control");
    let original = read_trimmed(&path)?;
    std::fs::write(&path, "on").with_context(|| format!("writing 'on' to {}", path.display()))?;
    Ok(PowerRestore { path, original })
}

fn enable_memory_decoding(config_path: &Path) -> Result<CommandRestore> {
    let mut config = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(config_path)
        .with_context(|| format!("opening {} (needs root)", config_path.display()))?;
    let mut bytes = [0u8; 2];
    config.seek(SeekFrom::Start(PCI_COMMAND))?;
    config
        .read_exact(&mut bytes)
        .with_context(|| format!("reading the command register of {}", config_path.display()))?;
    let command = u16::from_le_bytes(bytes);
    if command & PCI_COMMAND_MEMORY != 0 {
        return Ok(CommandRestore {
            config,
            original: None,
        });
    }
    config
        .write_all_at(&(command | PCI_COMMAND_MEMORY).to_le_bytes(), PCI_COMMAND)
        .with_context(|| format!("enabling memory decoding in {}", config_path.display()))?;
    Ok(CommandRestore {
        config,
        original: Some(command),
    })
}

fn read_trimmed(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_string())
}

/// Reset the GPU at `sbdf` and wait until it reports its memory cleared.
///
/// `sysfs` and `dev` are normally `/sys` and `/dev`. Fails with [`GpuBusy`]
/// when the GPU's VFIO group is open, and with an error naming the step for
/// anything else.
pub fn scrub_device(
    sysfs: &Path,
    dev: &Path,
    sbdf: &str,
    timeout: Duration,
) -> Result<ScrubTimings> {
    scrub_device_with(sysfs, dev, sbdf, timeout, &sysfs_reset)
}

/// The real reset: the kernel's function reset, a bus reset for these GPUs.
fn sysfs_reset(device: &Path) -> Result<()> {
    let reset = device.join("reset");
    std::fs::write(&reset, "1").with_context(|| format!("writing {}", reset.display()))
}

/// [`scrub_device`] with the reset supplied, so a test can stand in for the
/// hardware.
pub fn scrub_device_with(
    sysfs: &Path,
    dev: &Path,
    sbdf: &str,
    timeout: Duration,
    reset: &dyn Fn(&Path) -> Result<()>,
) -> Result<ScrubTimings> {
    scrub_device_marked(sysfs, dev, sbdf, timeout, reset, &|| Ok(()))
}

/// The scrub, calling `before_touching` once the checks have passed and the
/// VFIO group is held, before the GPU is changed in any way. An error from it
/// ends the scrub with the GPU untouched.
fn scrub_device_marked(
    sysfs: &Path,
    dev: &Path,
    sbdf: &str,
    timeout: Duration,
    reset: &dyn Fn(&Path) -> Result<()>,
    before_touching: &dyn Fn() -> Result<()>,
) -> Result<ScrubTimings> {
    let started = Instant::now();
    let device = sysfs.join("bus/pci/devices").join(sbdf);
    if !device.exists() {
        bail!("no PCI device {sbdf} on this host");
    }
    let vendor = read_trimmed(&device.join("vendor"))?;
    if vendor != NVIDIA_VENDOR {
        bail!("PCI device {sbdf} has vendor {vendor}, not NVIDIA ({NVIDIA_VENDOR})");
    }
    let driver = std::fs::read_link(device.join("driver"))
        .ok()
        .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()));
    if driver.as_deref() != Some("vfio-pci") {
        bail!(
            "GPU {sbdf} is bound to {}, not vfio-pci; only a GPU on vfio-pci is scrubbed",
            driver.as_deref().unwrap_or("no driver")
        );
    }
    let method = read_trimmed(&device.join("reset_method"))?;
    if method != "bus" {
        bail!(
            "GPU {sbdf} resets by '{method}'; its memory clear runs after a secondary bus \
             reset, so reset_method must be 'bus' (echo bus > {})",
            device.join("reset_method").display()
        );
    }

    // Hold the group: while it is open no VM can open the GPU, and if a VM has
    // it open the kernel refuses this open with EBUSY.
    let group = std::fs::read_link(device.join("iommu_group"))
        .with_context(|| format!("GPU {sbdf} has no IOMMU group"))?;
    let group = group
        .file_name()
        .with_context(|| format!("GPU {sbdf}: unreadable IOMMU group link"))?;
    let group_node = dev.join("vfio").join(group);
    let _group = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&group_node)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
            return Err(GpuBusy(format!(
                "GPU {sbdf} is in use: {} is open (a VM still holds it)",
                group_node.display()
            ))
            .into())
        }
        Err(error) => {
            return Err(error).with_context(|| format!("opening {}", group_node.display()))
        }
    };

    before_touching()?;
    // Dropped in reverse order: memory decoding, then power control.
    let _power = hold_in_d0(&device)?;
    let _command = enable_memory_decoding(&device.join("config"))?;
    let bar0 = Bar0::open(&device.join("resource0"))?;
    let boot0 = bar0.read32(PMC_BOOT_0);
    if unreadable(boot0) {
        bail!("GPU {sbdf} does not answer on BAR0 (PMC_BOOT_0 = {boot0:#010x})");
    }
    let Some(architecture) = architecture(boot0) else {
        bail!(
            "GPU {sbdf} (PMC_BOOT_0 = {boot0:#010x}, architecture {:#x}) is not Turing, Ampere \
             or Ada, the architectures whose memory clear this scrub can wait for",
            (boot0 >> 24) & 0x1f
        );
    };
    bar0.write32(RESET_SCRATCH, 1);
    let scratch = bar0.read32(RESET_SCRATCH);
    if scratch != 1 {
        bail!(
            "GPU {sbdf}: the reset scratch register ({RESET_SCRATCH:#x}) reads {scratch:#x} \
             after writing 1, so the reset could not be confirmed"
        );
    }

    let reset_started = Instant::now();
    reset(&device).with_context(|| format!("resetting GPU {sbdf}"))?;
    let reset_ms = ms(reset_started);
    let scratch = bar0.wait_readable(RESET_SCRATCH, timeout);
    if scratch != 0 {
        bail!(
            "GPU {sbdf} was not reset: the scratch register ({RESET_SCRATCH:#x}) set before the \
             reset still reads {scratch:#x}"
        );
    }
    let boot_ms = bar0.poll("boot complete", BOOT_COMPLETE, BOOT_COMPLETE_MASK, timeout)?;
    let clear_ms = bar0.poll("memory clear finished", MEMORY_CLEAR_FINISHED, 1, timeout)?;
    Ok(ScrubTimings {
        architecture: architecture.to_string(),
        reset_ms,
        boot_ms,
        clear_ms,
        total_ms: ms(started),
    })
}

/// One GPU's record: what the fleet agent counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuRecord {
    pub sbdf: String,
    /// Successful scrubs.
    pub scrubs: u64,
    /// Failed scrubs (each one quarantined the GPU).
    pub failures: u64,
    /// Set by a failed scrub, cleared by the next successful one.
    pub quarantined: bool,
    pub last_error: Option<String>,
    pub last_scrub: Option<ScrubTimings>,
    /// RFC 3339 time of the last scrub, either outcome.
    pub updated: Option<String>,
}

fn record_path(state_dir: &Path, sbdf: &str) -> PathBuf {
    state_dir.join(format!("{sbdf}.json"))
}

/// A GPU's record, or None if it was never scrubbed. An unreadable record is
/// an error: it may hold a quarantine.
pub fn read_record(state_dir: &Path, sbdf: &str) -> Result<Option<GpuRecord>> {
    let path = record_path(state_dir, sbdf);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("GPU record {} is not valid", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Every record in `state_dir`, sorted by address.
pub fn read_records(state_dir: &Path) -> Result<Vec<GpuRecord>> {
    let mut records = Vec::new();
    let entries = match std::fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(records),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", state_dir.display()))
        }
    };
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(sbdf) = name.strip_suffix(".json") {
            if let Some(record) = read_record(state_dir, sbdf)? {
                records.push(record);
            }
        }
    }
    records.sort_by(|a, b| a.sbdf.cmp(&b.sbdf));
    Ok(records)
}

/// Take the GPU's record lock, `<state_dir>/<sbdf>.lock`. A scrub holds it
/// from before it touches the GPU until its outcome is written.
fn lock_gpu(state_dir: &Path, sbdf: &str) -> Result<std::fs::File> {
    use fs2::FileExt as _;
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    // Root writes the records and rootless launches read them, whatever
    // root's umask.
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} readable", state_dir.display()))?;
    let path = state_dir.join(format!("{sbdf}.lock"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(lock)
}

/// Publish `record` atomically and durably: a synced temporary file renamed
/// over the old one, then the directory synced so the rename itself survives
/// a crash.
fn write_record(state_dir: &Path, record: &GpuRecord) -> Result<()> {
    let path = record_path(state_dir, &record.sbdf);
    let tmp = state_dir.join(format!(".{}.json.{}", record.sbdf, uuid::Uuid::new_v4()));
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(serde_json::to_string_pretty(record)?.as_bytes())?;
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path).with_context(|| format!("publishing {}", path.display()))?;
    std::fs::File::open(state_dir)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("syncing {}", state_dir.display()))
}

/// Read, change and write a GPU's record. The caller holds its lock.
fn update_record(
    state_dir: &Path,
    sbdf: &str,
    change: impl FnOnce(&mut GpuRecord),
) -> Result<GpuRecord> {
    let mut record = read_record(state_dir, sbdf)?.unwrap_or_else(|| GpuRecord {
        sbdf: sbdf.to_string(),
        ..Default::default()
    });
    change(&mut record);
    record.updated = Some(chrono::Utc::now().to_rfc3339());
    write_record(state_dir, &record)?;
    Ok(record)
}

/// Apply a scrub's outcome to a record: a failure quarantines the GPU, a
/// success lifts the quarantine.
fn apply_outcome(record: &mut GpuRecord, outcome: &std::result::Result<ScrubTimings, String>) {
    match outcome {
        Ok(timings) => {
            record.scrubs += 1;
            record.quarantined = false;
            record.last_error = None;
            record.last_scrub = Some(timings.clone());
        }
        Err(error) => {
            record.failures += 1;
            record.quarantined = true;
            record.last_error = Some(error.clone());
            record.last_scrub = None;
        }
    }
}

/// Record a scrub's outcome under the GPU's lock and return the record.
pub fn record_outcome(
    state_dir: &Path,
    sbdf: &str,
    outcome: &std::result::Result<ScrubTimings, String>,
) -> Result<GpuRecord> {
    let _lock = lock_gpu(state_dir, sbdf)?;
    update_record(state_dir, sbdf, |record| apply_outcome(record, outcome))
}

/// What one `fcvm gpu scrub` did to one GPU.
#[derive(Debug, Serialize)]
pub struct ScrubReport {
    pub sbdf: String,
    /// `scrubbed`, `failed` (now quarantined) or `busy` (a VM holds the GPU;
    /// nothing was done and nothing recorded).
    pub outcome: &'static str,
    pub error: Option<String>,
    pub timings: Option<ScrubTimings>,
    /// The GPU's record after this scrub; absent when busy.
    pub record: Option<GpuRecord>,
}

/// Scrub one GPU and record the outcome under `state_dir`: the call the
/// fleet agent makes, and what `fcvm gpu scrub` runs for each GPU.
pub fn scrub_and_record(
    sysfs: &Path,
    dev: &Path,
    state_dir: &Path,
    sbdf: &str,
    timeout: Duration,
) -> ScrubReport {
    scrub_and_record_with(sysfs, dev, state_dir, sbdf, timeout, &sysfs_reset)
}

/// [`scrub_and_record`] with the reset supplied, for tests.
///
/// The GPU's lock is held from before the GPU is touched until the outcome is
/// written, and before the GPU is touched its record is marked quarantined
/// ("a scrub started and has not recorded its outcome"). So the record always
/// holds the latest scrub's outcome, and a scrub that dies, or cannot write its
/// outcome, leaves the GPU refused. A scrub that cannot write the mark does not
/// touch the GPU.
pub fn scrub_and_record_with(
    sysfs: &Path,
    dev: &Path,
    state_dir: &Path,
    sbdf: &str,
    timeout: Duration,
    reset: &dyn Fn(&Path) -> Result<()>,
) -> ScrubReport {
    let failed = |error: String| ScrubReport {
        sbdf: sbdf.to_string(),
        outcome: "failed",
        error: Some(error),
        timings: None,
        record: None,
    };
    let _lock = match lock_gpu(state_dir, sbdf) {
        Ok(lock) => lock,
        Err(error) => return failed(format!("{error:#}; the GPU was not touched")),
    };
    let mark = || {
        update_record(state_dir, sbdf, |record| {
            record.quarantined = true;
            record.last_error = Some(format!(
                "a scrub started at {} and has not recorded its outcome",
                chrono::Utc::now().to_rfc3339()
            ));
        })
        .map(|_| ())
        .context("marking the GPU as being scrubbed; the GPU was not touched")
    };
    let result = scrub_device_marked(sysfs, dev, sbdf, timeout, reset, &mark);
    if let Err(error) = &result {
        if let Some(busy) = error.downcast_ref::<GpuBusy>() {
            return ScrubReport {
                sbdf: sbdf.to_string(),
                outcome: "busy",
                error: Some(busy.to_string()),
                timings: None,
                record: None,
            };
        }
    }
    let outcome = result.map_err(|error| format!("{error:#}"));
    match update_record(state_dir, sbdf, |record| apply_outcome(record, &outcome)) {
        Ok(record) => ScrubReport {
            sbdf: sbdf.to_string(),
            outcome: if outcome.is_ok() {
                "scrubbed"
            } else {
                "failed"
            },
            error: outcome.as_ref().err().cloned(),
            timings: outcome.ok(),
            record: Some(record),
        },
        // The mark written before the GPU was touched still quarantines it.
        Err(record_error) => failed(format!(
            "{}recording the outcome failed: {record_error:#}",
            outcome.err().map(|e| format!("{e}; ")).unwrap_or_default()
        )),
    }
}

/// Refuse a quarantined GPU. Called for every `--gpu` before the VM starts.
pub fn check_not_quarantined(state_dir: &Path, sbdf: &str) -> Result<()> {
    match read_record(state_dir, sbdf)? {
        Some(record) if record.quarantined => bail!(
            "GPU {sbdf} is quarantined: its last host scrub failed ({}) at {}. It is not \
             given to a VM until `sudo fcvm gpu scrub {sbdf}` succeeds",
            record.last_error.as_deref().unwrap_or("no error recorded"),
            record.updated.as_deref().unwrap_or("an unknown time")
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T4_BOOT0: u32 = 0x164000a1;

    /// A fake sysfs and /dev for one GPU: config space, a sparse BAR0 file
    /// with the given register values, and a VFIO group node.
    struct Fake {
        root: tempfile::TempDir,
    }

    impl Fake {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let device = root.path().join("sys/bus/pci/devices/0000:18:00.0");
            std::fs::create_dir_all(&device).unwrap();
            let driver = root.path().join("sys/bus/pci/drivers/vfio-pci");
            std::fs::create_dir_all(&driver).unwrap();
            std::os::unix::fs::symlink(&driver, device.join("driver")).unwrap();
            let group = root.path().join("sys/kernel/iommu_groups/81");
            std::fs::create_dir_all(&group).unwrap();
            std::os::unix::fs::symlink(&group, device.join("iommu_group")).unwrap();
            std::fs::create_dir_all(root.path().join("dev/vfio")).unwrap();
            std::fs::write(root.path().join("dev/vfio/81"), b"").unwrap();
            std::fs::write(device.join("vendor"), "0x10de\n").unwrap();
            std::fs::write(device.join("reset_method"), "bus\n").unwrap();
            std::fs::create_dir_all(device.join("power")).unwrap();
            std::fs::write(device.join("power/control"), "auto\n").unwrap();
            std::fs::write(device.join("reset"), "").unwrap();
            // Command register with memory decoding off, as vfio-pci leaves an
            // idle device.
            std::fs::write(device.join("config"), [0u8; 64]).unwrap();
            let bar = std::fs::File::create(device.join("resource0")).unwrap();
            bar.set_len(16 << 20).unwrap();
            let fake = Self { root };
            fake.set(PMC_BOOT_0, T4_BOOT0);
            fake.set(BOOT_COMPLETE, 0x3ff);
            fake.set(MEMORY_CLEAR_FINISHED, 1);
            fake
        }

        fn device(&self) -> PathBuf {
            self.root.path().join("sys/bus/pci/devices/0000:18:00.0")
        }

        fn set(&self, offset: u64, value: u32) {
            let bar = std::fs::OpenOptions::new()
                .write(true)
                .open(self.device().join("resource0"))
                .unwrap();
            bar.write_all_at(&value.to_le_bytes(), offset).unwrap();
        }

        /// Scrub with a reset that, like the hardware, clears the scratch
        /// register, unless `clears_scratch` is false.
        fn scrub_resetting(&self, clears_scratch: bool) -> Result<ScrubTimings> {
            scrub_device_with(
                &self.root.path().join("sys"),
                &self.root.path().join("dev"),
                "0000:18:00.0",
                Duration::from_millis(50),
                &|device: &Path| {
                    std::fs::write(device.join("reset"), "1")?;
                    if clears_scratch {
                        self.set(RESET_SCRATCH, 0);
                    }
                    Ok(())
                },
            )
        }

        fn scrub(&self) -> Result<ScrubTimings> {
            self.scrub_resetting(true)
        }
    }

    #[test]
    fn a_t4_that_reports_its_clear_is_scrubbed() {
        let fake = Fake::new();
        let timings = fake.scrub().unwrap();
        assert_eq!(timings.architecture, "turing");
        assert_eq!(
            std::fs::read_to_string(fake.device().join("reset")).unwrap(),
            "1",
            "the scrub must reset the GPU"
        );
        let config = std::fs::read(fake.device().join("config")).unwrap();
        assert_eq!(
            config[4] & PCI_COMMAND_MEMORY as u8,
            0,
            "the command register must be restored after the scrub"
        );
        assert_eq!(
            std::fs::read_to_string(fake.device().join("power/control")).unwrap(),
            "auto",
            "power control must be restored after the scrub"
        );
    }

    /// A reset that did not happen leaves the scratch register set, and the
    /// scrub must not then report the GPU's stale "cleared" bit as a clear.
    #[test]
    fn a_reset_that_did_not_happen_fails_the_scrub() {
        let fake = Fake::new();
        let error = format!("{:#}", fake.scrub_resetting(false).unwrap_err());
        assert!(error.contains("was not reset"), "{error}");
    }

    /// The scrub holds the GPU in D0 while it reads BAR0: in D3 every read is
    /// all ones.
    #[test]
    fn the_scrub_holds_the_gpu_in_d0() {
        let fake = Fake::new();
        let control = fake.device().join("power/control");
        let seen = std::cell::RefCell::new(String::new());
        scrub_device_with(
            &fake.root.path().join("sys"),
            &fake.root.path().join("dev"),
            "0000:18:00.0",
            Duration::from_millis(50),
            &|device: &Path| {
                *seen.borrow_mut() = std::fs::read_to_string(&control)?;
                std::fs::write(device.join("reset"), "1")?;
                fake.set(RESET_SCRATCH, 0);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*seen.borrow(), "on");
    }

    #[test]
    fn a_clear_that_never_finishes_fails_the_scrub() {
        let fake = Fake::new();
        fake.set(MEMORY_CLEAR_FINISHED, 0);
        let error = format!("{:#}", fake.scrub().unwrap_err());
        assert!(error.contains("memory clear finished"), "{error}");
    }

    #[test]
    fn a_gpu_that_never_boots_fails_the_scrub() {
        let fake = Fake::new();
        fake.set(BOOT_COMPLETE, 0x1ff);
        let error = format!("{:#}", fake.scrub().unwrap_err());
        assert!(error.contains("boot complete"), "{error}");
    }

    /// All ones is a GPU that does not answer, not a GPU that reports done.
    #[test]
    fn an_unanswered_read_does_not_count_as_done() {
        let fake = Fake::new();
        fake.set(BOOT_COMPLETE, 0xffff_ffff);
        assert!(fake.scrub().is_err());
        let fake = Fake::new();
        fake.set(MEMORY_CLEAR_FINISHED, 0xbadf_1001);
        assert!(fake.scrub().is_err());
        let fake = Fake::new();
        fake.set(PMC_BOOT_0, 0xffff_ffff);
        let error = format!("{:#}", fake.scrub().unwrap_err());
        assert!(error.contains("does not answer"), "{error}");
    }

    #[test]
    fn an_architecture_without_this_clear_is_refused() {
        let fake = Fake::new();
        fake.set(PMC_BOOT_0, 0x180000a1); // Hopper
        let error = format!("{:#}", fake.scrub().unwrap_err());
        assert!(error.contains("not Turing, Ampere or Ada"), "{error}");
    }

    #[test]
    fn a_gpu_without_a_bus_reset_is_refused() {
        let fake = Fake::new();
        std::fs::write(fake.device().join("reset_method"), "flr bus\n").unwrap();
        let error = format!("{:#}", fake.scrub().unwrap_err());
        assert!(error.contains("reset_method must be 'bus'"), "{error}");
        assert_eq!(
            std::fs::read_to_string(fake.device().join("reset")).unwrap(),
            "",
            "nothing may be reset when the checks fail"
        );
    }

    #[test]
    fn a_gpu_on_a_host_driver_is_refused() {
        let fake = Fake::new();
        std::fs::remove_file(fake.device().join("driver")).unwrap();
        let error = format!("{:#}", fake.scrub().unwrap_err());
        assert!(error.contains("not vfio-pci"), "{error}");
    }

    /// The scrub runs under the GPU's record lock from before the reset until
    /// its outcome is written, so a later scrub's outcome cannot be overwritten
    /// by an earlier one's; and from before the reset the record says the GPU
    /// is not clean, so a scrub that dies or cannot record leaves it refused.
    #[test]
    fn the_scrub_marks_the_gpu_and_holds_its_lock_until_the_outcome_is_recorded() {
        use fs2::FileExt as _;
        let fake = Fake::new();
        let state = fake.root.path().join("gpu-state");
        let sbdf = "0000:18:00.0";
        record_outcome(&state, sbdf, &Ok(ScrubTimings::default())).unwrap();
        let during = std::cell::RefCell::new(None);
        let report = scrub_and_record_with(
            &fake.root.path().join("sys"),
            &fake.root.path().join("dev"),
            &state,
            sbdf,
            Duration::from_millis(50),
            &|device: &Path| {
                let lock = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(state.join(format!("{sbdf}.lock")))?;
                let held = lock.try_lock_exclusive().is_err();
                let marked = read_record(&state, sbdf)?.map(|r| r.quarantined);
                *during.borrow_mut() = Some((held, marked));
                std::fs::write(device.join("reset"), "1")?;
                fake.set(RESET_SCRATCH, 0);
                Ok(())
            },
        );
        assert_eq!(report.outcome, "scrubbed", "{:?}", report.error);
        assert_eq!(
            *during.borrow(),
            Some((true, Some(true))),
            "during the reset the lock must be held and the GPU marked unclean"
        );
        let after = read_record(&state, sbdf).unwrap().unwrap();
        assert!(!after.quarantined);
        assert_eq!((after.scrubs, after.failures), (2, 0));
    }

    /// A scrub that cannot record that it started does not touch the GPU.
    #[test]
    fn a_scrub_that_cannot_mark_the_gpu_does_not_reset_it() {
        let fake = Fake::new();
        let state = fake.root.path().join("not-a-directory");
        std::fs::write(&state, "").unwrap();
        let reset = std::cell::Cell::new(false);
        let report = scrub_and_record_with(
            &fake.root.path().join("sys"),
            &fake.root.path().join("dev"),
            &state,
            "0000:18:00.0",
            Duration::from_millis(50),
            &|_: &Path| {
                reset.set(true);
                Ok(())
            },
        );
        assert_eq!(report.outcome, "failed");
        assert!(
            !reset.get(),
            "the GPU was reset with no record saying a scrub had started"
        );
    }

    /// Root writes the records and rootless launches read them, so a record
    /// and its directory are readable by everyone whatever root's umask.
    #[test]
    fn records_are_readable_by_rootless_launches_under_any_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("gpu");
        // nextest runs each test in its own process, so the umask is ours.
        let old = unsafe { libc::umask(0o077) };
        let recorded = record_outcome(&state, "0000:18:00.0", &Ok(ScrubTimings::default()));
        unsafe { libc::umask(old) };
        recorded.unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&state), 0o755, "state directory");
        assert_eq!(mode(&state.join("0000:18:00.0.json")), 0o644, "record");
    }

    /// A record is published by rename, and the directory is synced after
    /// it, or a crash can bring back the record the rename replaced.
    #[test]
    fn a_published_record_is_synced_with_its_directory() {
        let code = include_str!("scrub.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        let body = &code[code
            .find("fn write_record(")
            .expect("records are not written by write_record")..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let rename = body.find("std::fs::rename(").expect("no rename");
        let dir_sync = body[rename..]
            .find("File::open(state_dir)")
            .expect("the directory is not synced after the rename");
        assert!(body[rename + dir_sync..].contains(".sync_all()"));
    }

    #[test]
    fn a_failed_scrub_quarantines_the_gpu_until_one_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let sbdf = "0000:18:00.0";
        assert!(check_not_quarantined(dir.path(), sbdf).is_ok());

        let failed = record_outcome(dir.path(), sbdf, &Err("boot never completed".into())).unwrap();
        assert!(failed.quarantined);
        assert_eq!((failed.scrubs, failed.failures), (0, 1));
        let refused = format!("{:#}", check_not_quarantined(dir.path(), sbdf).unwrap_err());
        assert!(
            refused.contains("quarantined") && refused.contains("boot never completed"),
            "{refused}"
        );

        let ok = record_outcome(dir.path(), sbdf, &Ok(ScrubTimings::default())).unwrap();
        assert!(!ok.quarantined);
        assert_eq!((ok.scrubs, ok.failures), (1, 1));
        assert!(check_not_quarantined(dir.path(), sbdf).is_ok());
        assert_eq!(read_records(dir.path()).unwrap(), vec![ok]);
    }

    /// A record that cannot be read may hold a quarantine, so it refuses.
    #[test]
    fn an_unreadable_record_refuses_the_gpu() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0000:18:00.0.json"), "{not json").unwrap();
        assert!(check_not_quarantined(dir.path(), "0000:18:00.0").is_err());
    }
}
