//! GPU passthrough, guest side.
//!
//! The host attaches an NVIDIA GPU to a restored, still-paused clone with
//! Firecracker's device-passthrough API and names it in the restore metadata
//! (`gpus`). The guest kernel is not told: Firecracker's PCI transport has no
//! hotplug notification, so [`attach`] rescans the PCI bus itself, waits for
//! the announced number of NVIDIA display functions, loads the NVIDIA kernel
//! modules from the read-only driver image, creates the device nodes, checks
//! that the driver initialised every GPU, and writes a CDI spec. The container
//! starts only after that, with `--device nvidia.com/gpu=all`.
//!
//! The driver image (kernel modules for this exact guest kernel, GSP firmware,
//! the user-space driver and nvidia-container-toolkit) is built by `fcvm setup
//! --kernel-profile gpu` (src/setup/nvidia.rs) and attached by the host as a
//! read-only block device named in the boot plan's `gpu_driver`. It is mounted
//! at [`DRIVER_ROOT`] at boot, before the snapshot, so every clone has it.
//!
//! Every step fails closed with an error naming what was expected and what
//! the guest saw. The caller shuts the clone down on error, before the restore
//! is acknowledged, so a clone without a working GPU is never published.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the NVIDIA driver image is mounted (read-only).
pub const DRIVER_ROOT: &str = "/run/fcvm-gpu/driver";

/// The CDI spec fc-agent writes. Podman reads `/etc/cdi` and `/var/run/cdi`.
pub const CDI_SPEC: &str = "/var/run/cdi/nvidia.yaml";

/// The CDI device the container is given: every GPU the spec describes.
pub const CDI_ALL_GPUS: &str = "nvidia.com/gpu=all";

/// The kernel modules loaded, in dependency order. nvidia-modeset and
/// nvidia-drm are display-only and the guest kernel has no DRM.
const MODULES: [&str; 2] = ["nvidia", "nvidia-uvm"];

const NVIDIA_VENDOR: u16 = 0x10de;
/// PCI base class 0x03: display controller (0x0300 VGA, 0x0302 3D controller).
const PCI_BASE_CLASS_DISPLAY: u32 = 0x03;
/// The NVIDIA character major (`NV_MAJOR_DEVICE_NUMBER`), fixed by the driver.
const NVIDIA_MAJOR: u32 = 195;
const NVIDIACTL_MINOR: u32 = 255;

/// How long the rescanned bus may take to show the announced GPUs.
const PCI_APPEAR_TIMEOUT: Duration = Duration::from_secs(10);
/// insmod of nvidia.ko boots the GSP; the healthy case is seconds.
const INSMOD_TIMEOUT: Duration = Duration::from_secs(120);
/// nvidia-smi initialises the GPU on first open when nothing holds it open.
const SMI_TIMEOUT: Duration = Duration::from_secs(120);
const CDI_TIMEOUT: Duration = Duration::from_secs(60);
/// nvidia-persistenced initialises the GPU and then detaches; its parent exits.
const PERSISTENCED_TIMEOUT: Duration = Duration::from_secs(120);

/// Orders GPU attachment against the container launch. An attach holds the gate
/// from its check that no container is running until it has published the CDI
/// devices, and the launch takes the gate to read them, so a launch that starts
/// while an attach is loading the driver waits for it instead of creating the
/// container without the GPU.
pub struct LaunchGate {
    state: tokio::sync::Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    /// Set when the container is launched. A GPU attached after that is refused:
    /// the container's device list was fixed when it was created.
    launched: bool,
    /// CDI devices for `podman run`, set by a successful attach.
    devices: Vec<String>,
}

impl LaunchGate {
    pub fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(GateState::default()),
        }
    }

    /// Run `work`, which makes `count` GPUs usable, unless the container already
    /// launched, and publish the CDI devices when it succeeds.
    pub async fn attach<F, Fut>(&self, count: usize, work: F) -> Result<AttachReport>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<AttachReport>>,
    {
        let mut state = self.state.lock().await;
        if state.launched {
            bail!(
                "{count} GPU(s) were attached to a clone whose container is already running; \
                 a container's devices are fixed when it is created, so GPUs can only be \
                 attached to clones of the pre-start snapshot"
            );
        }
        let initialised = work().await?;
        state.devices = vec![CDI_ALL_GPUS.to_string()];
        Ok(initialised)
    }

    /// Mark the container launched and return its `--device` values: empty unless
    /// a GPU was attached. Waits for an attach in progress.
    pub async fn launch(&self) -> Vec<String> {
        let mut state = self.state.lock().await;
        state.launched = true;
        state.devices.clone()
    }
}

impl Default for LaunchGate {
    fn default() -> Self {
        Self::new()
    }
}

static GATE: std::sync::LazyLock<LaunchGate> = std::sync::LazyLock::new(LaunchGate::new);

/// The `--device` values for the container about to launch. Called just before
/// `podman run`; from then on a GPU attached by a later restore is refused.
pub async fn launch_devices() -> Vec<String> {
    GATE.launch().await
}

/// A run whose GPUs are attached by a restore (the plan's `await_gpu_restore`)
/// must not launch its container in a VM that was not restored: the GPUs are
/// attached to the restore of the pre-start snapshot, never to the cold VM that
/// made it. A failed cache handshake would otherwise launch it without them.
pub fn check_gpu_restore(await_gpu_restore: bool, restored: bool) -> Result<()> {
    if await_gpu_restore && !restored {
        bail!(
            "this run's GPUs are attached when the host restores the pre-start snapshot, and \
             this VM was not restored (the cache handshake did not end in a restore); \
             refusing to start the container without them"
        );
    }
    Ok(())
}

/// One PCI function as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciFunction {
    pub address: String,
    pub vendor: u16,
    pub device: u16,
    pub class: u32,
}

impl PciFunction {
    fn is_nvidia_gpu(&self) -> bool {
        self.vendor == NVIDIA_VENDOR && (self.class >> 16) == PCI_BASE_CLASS_DISPLAY
    }
}

impl std::fmt::Display for PciFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} [{:04x}:{:04x} class {:06x}]",
            self.address, self.vendor, self.device, self.class
        )
    }
}

fn parse_hex_u32(text: &str) -> Option<u32> {
    let text = text.trim();
    u32::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).ok()
}

/// Read every function under `<sysfs>/bus/pci/devices`, sorted by address.
pub fn read_pci_functions(sysfs: &Path) -> Result<Vec<PciFunction>> {
    let dir = sysfs.join("bus/pci/devices");
    let mut functions = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let read = |name: &str| -> Result<u32> {
            let file = path.join(name);
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            parse_hex_u32(&text)
                .with_context(|| format!("{} is not hexadecimal: {:?}", file.display(), text))
        };
        functions.push(PciFunction {
            address: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            vendor: read("vendor")? as u16,
            device: read("device")? as u16,
            class: read("class")?,
        });
    }
    functions.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(functions)
}

/// The NVIDIA display functions among `functions`.
pub fn nvidia_gpus(functions: &[PciFunction]) -> Vec<PciFunction> {
    functions
        .iter()
        .filter(|f| f.is_nvidia_gpu())
        .cloned()
        .collect()
}

/// The error for a bus that does not show the announced GPUs. It lists every
/// function the guest does see, so a missing device, a wrong vendor or a
/// non-display class function is visible in the message itself.
fn missing_gpus_error(announced: &[String], functions: &[PciFunction]) -> anyhow::Error {
    let seen: Vec<String> = functions.iter().map(ToString::to_string).collect();
    anyhow::anyhow!(
        "the host attached {} GPU(s) ({}) but after a PCI rescan the guest sees {} NVIDIA \
         display function(s). PCI functions present: {}. Check that the restore loaded with \
         the device_passthrough entry and that the guest kernel has PCI and MSI-X support",
        announced.len(),
        announced.join(", "),
        nvidia_gpus(functions).len(),
        if seen.is_empty() {
            "none".to_string()
        } else {
            seen.join(", ")
        }
    )
}

/// Rescan the PCI bus until `announced.len()` NVIDIA GPUs are present.
///
/// The rescan write runs the bus scan synchronously, so the first check
/// normally succeeds; the loop only covers a device that is not yet
/// responding to config reads when the first scan runs.
pub async fn rescan_for_gpus(sysfs: &Path, announced: &[String]) -> Result<Vec<PciFunction>> {
    let deadline = tokio::time::Instant::now() + PCI_APPEAR_TIMEOUT;
    let rescan = sysfs.join("bus/pci/rescan");
    loop {
        std::fs::write(&rescan, "1").with_context(|| format!("writing {}", rescan.display()))?;
        let functions = read_pci_functions(sysfs)?;
        let gpus = nvidia_gpus(&functions);
        if gpus.len() >= announced.len() {
            return Ok(gpus);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(missing_gpus_error(announced, &functions));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Character majors from `/proc/devices`, by name.
pub fn parse_char_majors(proc_devices: &str) -> BTreeMap<String, u32> {
    let mut majors = BTreeMap::new();
    let mut in_char = false;
    for line in proc_devices.lines() {
        let line = line.trim();
        if line.starts_with("Character devices") {
            in_char = true;
            continue;
        }
        if line.starts_with("Block devices") {
            in_char = false;
            continue;
        }
        if !in_char {
            continue;
        }
        if let Some((major, name)) = line.split_once(char::is_whitespace) {
            if let Ok(major) = major.trim().parse() {
                majors.insert(name.trim().to_string(), major);
            }
        }
    }
    majors
}

/// The `Device Minor:` field of `/proc/driver/nvidia/gpus/<addr>/information`.
pub fn parse_device_minor(information: &str) -> Option<u32> {
    information.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "Device Minor").then(|| value.trim().parse().ok())?
    })
}

/// A device node fc-agent creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceNode {
    pub path: String,
    pub major: u32,
    pub minor: u32,
}

/// The device nodes the driver needs, from `/proc/devices` and the per-GPU
/// minors. The NVIDIA modules register their majors but create no nodes;
/// nvidia-modprobe does that on a normal install, and it is not run here.
pub fn device_nodes(majors: &BTreeMap<String, u32>, gpu_minors: &[u32]) -> Result<Vec<DeviceNode>> {
    let mut nodes = vec![DeviceNode {
        path: "/dev/nvidiactl".to_string(),
        major: NVIDIA_MAJOR,
        minor: NVIDIACTL_MINOR,
    }];
    for &minor in gpu_minors {
        nodes.push(DeviceNode {
            path: format!("/dev/nvidia{minor}"),
            major: NVIDIA_MAJOR,
            minor,
        });
    }
    let uvm = *majors
        .get("nvidia-uvm")
        .context("nvidia-uvm is loaded but /proc/devices lists no nvidia-uvm character major")?;
    nodes.push(DeviceNode {
        path: "/dev/nvidia-uvm".to_string(),
        major: uvm,
        minor: 0,
    });
    nodes.push(DeviceNode {
        path: "/dev/nvidia-uvm-tools".to_string(),
        major: uvm,
        minor: 1,
    });
    Ok(nodes)
}

/// Where the driver image keeps this kernel's modules.
pub fn module_dir(driver_root: &Path, kernel_release: &str) -> PathBuf {
    driver_root
        .join("lib/modules")
        .join(kernel_release)
        .join("extra")
}

/// The driver image's user-space library directory for this architecture.
pub fn library_dir(driver_root: &Path) -> PathBuf {
    let triplet = if cfg!(target_arch = "aarch64") {
        "aarch64-linux-gnu"
    } else {
        "x86_64-linux-gnu"
    };
    driver_root.join("usr/lib").join(triplet)
}

/// Refuse a driver image built for another kernel, naming both releases.
pub fn check_module_dir(driver_root: &Path, kernel_release: &str) -> Result<PathBuf> {
    let dir = module_dir(driver_root, kernel_release);
    if dir.is_dir() {
        return Ok(dir);
    }
    let modules = driver_root.join("lib/modules");
    let built_for: Vec<String> = std::fs::read_dir(&modules)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if built_for.is_empty() {
        bail!(
            "no NVIDIA driver image at {}: {} holds no kernel modules. The boot plan must \
             name the driver image (gpu_driver) for a GPU to be attached",
            driver_root.display(),
            modules.display()
        );
    }
    bail!(
        "the NVIDIA driver image holds modules for kernel {} but the guest runs {}. \
         Rebuild it with `fcvm setup --kernel-profile gpu` for this kernel",
        built_for.join(", "),
        kernel_release
    )
}

fn kernel_release() -> Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .context("reading /proc/sys/kernel/osrelease")?
        .trim()
        .to_string())
}

/// The last NVRM lines of the kernel log, for an error message.
async fn nvrm_log_tail() -> String {
    let output = tokio::process::Command::new("dmesg").output().await;
    let Ok(output) = output else {
        return "(dmesg could not run)".to_string();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("NVRM") || l.contains("nvidia"))
        .collect();
    let tail = &lines[lines.len().saturating_sub(15)..];
    if tail.is_empty() {
        "(no NVRM lines in the kernel log)".to_string()
    } else {
        tail.join("\n")
    }
}

/// Run a command with a deadline and return its combined output on success.
async fn run_checked(
    what: &str,
    mut command: tokio::process::Command,
    timeout: Duration,
) -> Result<String> {
    command.kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("{what} did not finish within {timeout:?}"))?
        .with_context(|| format!("starting {what}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        bail!("{what} failed ({}): {}", output.status, text.trim());
    }
    Ok(text)
}

/// Point the firmware loader at the image's firmware. It looks in this
/// directory first. Set before nvidia.ko probes a GPU: the GSP firmware is
/// requested from the probe.
fn set_firmware_path(firmware_dir: &Path) -> Result<()> {
    let firmware_param = "/sys/module/firmware_class/parameters/path";
    std::fs::write(firmware_param, firmware_dir.as_os_str().as_encoded_bytes())
        .with_context(|| format!("setting {firmware_param} to {}", firmware_dir.display()))
}

/// `insmod` one of [`MODULES`] from the image, unless it is already loaded.
async fn load_module(module_dir: &Path, module: &str) -> Result<()> {
    debug_assert!(MODULES.contains(&module));
    if Path::new("/sys/module")
        .join(module.replace('-', "_"))
        .exists()
    {
        eprintln!("[fc-agent] gpu: {module} already loaded");
        return Ok(());
    }
    let path = module_dir.join(format!("{module}.ko"));
    let mut insmod = tokio::process::Command::new("insmod");
    insmod.arg(&path);
    if let Err(error) = run_checked(
        &format!("insmod {}", path.display()),
        insmod,
        INSMOD_TIMEOUT,
    )
    .await
    {
        bail!("{error:#}\nkernel log:\n{}", nvrm_log_tail().await);
    }
    eprintln!("[fc-agent] gpu: loaded {module}");
    Ok(())
}

/// The minors of the GPUs the driver probed, from `/proc/driver/nvidia/gpus`.
fn probed_gpu_minors() -> Result<Vec<u32>> {
    let dir = Path::new("/proc/driver/nvidia/gpus");
    let mut minors = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries {
            let path = entry?.path().join("information");
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            minors.push(
                parse_device_minor(&text)
                    .with_context(|| format!("{} has no Device Minor", path.display()))?,
            );
        }
    }
    minors.sort_unstable();
    Ok(minors)
}

fn create_device_nodes(nodes: &[DeviceNode]) -> Result<()> {
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};
    for node in nodes {
        if Path::new(&node.path).exists() {
            continue;
        }
        mknod(
            node.path.as_str(),
            SFlag::S_IFCHR,
            Mode::from_bits_truncate(0o666),
            makedev(node.major as _, node.minor as _),
        )
        .with_context(|| format!("mknod {} c {} {}", node.path, node.major, node.minor))?;
        // mknod applies the umask; the driver's own nodes are 0666.
        std::fs::set_permissions(
            &node.path,
            std::os::unix::fs::PermissionsExt::from_mode(0o666),
        )
        .with_context(|| format!("chmod 0666 {}", node.path))?;
    }
    Ok(())
}

/// Load the NVIDIA modules at boot, with no GPU on the bus, so the pre-start
/// snapshot carries them: a restore that attaches a GPU then only probes it
/// (the PCI rescan binds it to the loaded driver). The driver image's
/// nvidia.ko is built to load without a GPU (src/setup/nvidia.rs). The
/// firmware path is set here too, because the probe of a hot-added GPU
/// requests the GSP firmware.
///
/// A failure here is not fatal: [`attach`] loads whatever is not loaded, and
/// fails closed if that fails, so this only moves work out of the restore.
pub async fn preload(driver_root: &Path) -> Result<()> {
    let release = kernel_release()?;
    let modules = check_module_dir(driver_root, &release)?;
    set_firmware_path(&driver_root.join("lib/firmware"))?;
    for module in MODULES {
        load_module(&modules, module).await?;
    }
    Ok(())
}

/// Make the GPUs the host attached usable by the container.
///
/// `announced` is the host's list of attached devices (host PCI addresses);
/// only its length is compared with what the guest sees, because the guest
/// address of a passed-through device is assigned by the VMM.
/// Returns the number of GPUs the driver initialised and each step's duration.
pub async fn attach(announced: &[String]) -> Result<AttachReport> {
    anyhow::ensure!(!announced.is_empty(), "attach called with no GPUs");
    GATE.attach(announced.len(), || attach_devices(announced))
        .await
}

/// What a successful attach reports: the GPUs the driver initialised and the
/// duration of each step, carried in the restore ACK's phase record.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct AttachReport {
    pub gpus: usize,
    pub steps: AttachSteps,
}

/// Milliseconds spent in each step of [`attach_devices`].
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct AttachSteps {
    /// The driver image check for this kernel.
    pub check_ms: f64,
    /// PCI rescan until the announced GPUs are on the bus.
    pub rescan_ms: f64,
    /// `firmware_class.path` and `insmod nvidia.ko` (the PCI probe runs here).
    pub nvidia_ms: f64,
    /// `insmod nvidia-uvm.ko`.
    pub uvm_ms: f64,
    /// Reading the probed minors and creating the device nodes.
    pub nodes_ms: f64,
    /// `nvidia-persistenced`, which initialises the GPU and keeps it open.
    pub persistenced_ms: f64,
    /// `nvidia-smi -L`.
    pub smi_ms: f64,
    /// `nvidia-ctk cdi generate`.
    pub cdi_ms: f64,
}

/// Times one attach step and logs it, so a slow attach names the slow step in
/// the guest log as well as in the ACK.
struct StepClock(std::time::Instant);

impl StepClock {
    fn start() -> Self {
        Self(std::time::Instant::now())
    }

    /// Milliseconds since the last lap (or the start), logged under `step`.
    fn lap(&mut self, step: &str) -> f64 {
        let now = std::time::Instant::now();
        let ms = now.duration_since(self.0).as_secs_f64() * 1000.0;
        self.0 = now;
        eprintln!("[fc-agent] gpu: step={step} elapsed_ms={ms:.3}");
        ms
    }
}

async fn attach_devices(announced: &[String]) -> Result<AttachReport> {
    eprintln!(
        "[fc-agent] gpu: host attached {} device(s): {}",
        announced.len(),
        announced.join(", ")
    );
    let mut steps = AttachSteps::default();
    let mut clock = StepClock::start();

    let driver_root = Path::new(DRIVER_ROOT);
    let release = kernel_release()?;
    let modules = check_module_dir(driver_root, &release)?;
    steps.check_ms = clock.lap("check");

    let gpus = rescan_for_gpus(Path::new("/sys"), announced).await?;
    for gpu in &gpus {
        eprintln!("[fc-agent] gpu: found {gpu}");
    }
    steps.rescan_ms = clock.lap("rescan");

    set_firmware_path(&driver_root.join("lib/firmware"))?;
    load_module(&modules, "nvidia").await?;
    steps.nvidia_ms = clock.lap("insmod-nvidia");
    load_module(&modules, "nvidia-uvm").await?;
    steps.uvm_ms = clock.lap("insmod-nvidia-uvm");

    let minors = probed_gpu_minors()?;
    if minors.len() < announced.len() {
        bail!(
            "the NVIDIA driver loaded but probed {} of {} GPU(s); kernel log:\n{}",
            minors.len(),
            announced.len(),
            nvrm_log_tail().await
        );
    }
    let proc_devices = std::fs::read_to_string("/proc/devices").context("reading /proc/devices")?;
    create_device_nodes(&device_nodes(&parse_char_majors(&proc_devices), &minors)?)?;
    steps.nodes_ms = clock.lap("device-nodes");

    start_persistenced(driver_root).await?;
    steps.persistenced_ms = clock.lap("persistenced");

    // Both only read the GPUs the daemon has initialised, so they run together.
    // The spec is generated after the daemon starts so it mounts its socket.
    let ((smi_ms, listed), (cdi_ms, spec)) = tokio::join!(
        timed(list_gpus(driver_root)),
        timed(generate_cdi_spec(driver_root)),
    );
    steps.smi_ms = smi_ms;
    steps.cdi_ms = cdi_ms;
    eprintln!("[fc-agent] gpu: step=nvidia-smi elapsed_ms={smi_ms:.3}");
    eprintln!("[fc-agent] gpu: step=cdi elapsed_ms={cdi_ms:.3}");
    let listed = listed?;
    let initialised = listed.lines().filter(|l| l.starts_with("GPU ")).count();
    if initialised < announced.len() {
        bail!(
            "nvidia-smi lists {initialised} of {} GPU(s): {}",
            announced.len(),
            listed.trim()
        );
    }
    eprintln!("[fc-agent] gpu: {}", listed.trim().replace('\n', "; "));
    spec?;
    eprintln!("[fc-agent] gpu: wrote CDI spec {CDI_SPEC}");
    Ok(AttachReport {
        gpus: initialised,
        steps,
    })
}

/// Run `future` and return its milliseconds with its output.
async fn timed<T>(future: impl std::future::Future<Output = T>) -> (f64, T) {
    let started = std::time::Instant::now();
    let output = future.await;
    (started.elapsed().as_secs_f64() * 1000.0, output)
}

/// `nvidia-smi -L` from the image: one `GPU <n>:` line per GPU the driver
/// initialised. A failure carries the kernel log's NVRM lines.
async fn list_gpus(driver_root: &Path) -> Result<String> {
    let mut smi = tokio::process::Command::new(driver_root.join("usr/bin/nvidia-smi"));
    smi.arg("-L")
        .env("LD_LIBRARY_PATH", library_dir(driver_root));
    match run_checked("nvidia-smi -L", smi, SMI_TIMEOUT).await {
        Ok(text) => Ok(text),
        Err(error) => bail!("{error:#}\nkernel log:\n{}", nvrm_log_tail().await),
    }
}

/// Write the CDI spec with `nvidia-ctk cdi generate` from the image.
async fn generate_cdi_spec(driver_root: &Path) -> Result<()> {
    std::fs::create_dir_all(Path::new(CDI_SPEC).parent().unwrap())
        .context("creating the CDI spec directory")?;
    let mut ctk = tokio::process::Command::new(driver_root.join("usr/bin/nvidia-ctk"));
    ctk.args(cdi_generate_args(driver_root))
        .env("LD_LIBRARY_PATH", library_dir(driver_root));
    run_checked("nvidia-ctk cdi generate", ctk, CDI_TIMEOUT).await?;
    Ok(())
}

/// Start nvidia-persistenced from the driver image. Without it nothing holds the
/// GPU open between processes, and every process that opens it initialises it
/// again: `nvidia-smi -L` took 1.44 to 1.49 s each on a T4 in a gpu VM, and
/// 0.023 to 0.031 s with the daemon running. It starts before `nvidia-smi -L`,
/// so that check opens a GPU the daemon has initialised, and before the CDI spec
/// is generated, which then mounts the daemon's socket into the container.
async fn start_persistenced(driver_root: &Path) -> Result<()> {
    run_checked(
        "nvidia-persistenced",
        persistenced_command(driver_root),
        PERSISTENCED_TIMEOUT,
    )
    .await?;
    eprintln!("[fc-agent] gpu: nvidia-persistenced started");
    Ok(())
}

/// The nvidia-persistenced command: the image's binary, as root (its default),
/// with the image's libraries. It forks the daemon and exits.
pub fn persistenced_command(driver_root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(driver_root.join("usr/bin/nvidia-persistenced"));
    command.env("LD_LIBRARY_PATH", library_dir(driver_root));
    command
}

/// Arguments for `nvidia-ctk cdi generate` against the mounted driver image.
///
/// `--driver-root` makes the spec's host paths point into the image while the
/// container paths are the usual ones (`/usr/lib/<triplet>/libcuda.so.N`),
/// the layout NVIDIA's GPU Operator uses for a driver installed in its own
/// root. The hook binary is the image's, since the guest rootfs has none.
pub fn cdi_generate_args(driver_root: &Path) -> Vec<String> {
    vec![
        "cdi".to_string(),
        "generate".to_string(),
        format!("--driver-root={}", driver_root.display()),
        "--dev-root=/".to_string(),
        format!(
            "--nvidia-cdi-hook-path={}",
            driver_root.join("usr/bin/nvidia-cdi-hook").display()
        ),
        format!("--output={CDI_SPEC}"),
    ]
}

#[cfg(test)]
mod tests {
    /// The daemon is the image's binary, run with the image's libraries.
    #[test]
    fn persistenced_runs_the_images_binary_with_its_libraries() {
        let root = std::path::Path::new("/run/fcvm-gpu/driver");
        let command = super::persistenced_command(root);
        let command = command.as_std();
        assert_eq!(
            command.get_program(),
            root.join("usr/bin/nvidia-persistenced").as_os_str()
        );
        let libraries = command
            .get_envs()
            .find(|(key, _)| *key == "LD_LIBRARY_PATH")
            .and_then(|(_, value)| value);
        assert_eq!(libraries, Some(super::library_dir(root).as_os_str()));
    }

    /// The body of `attach_devices`, for the source-order tests below.
    fn attach_devices_body() -> &'static str {
        let source = include_str!("gpu.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let body = &code[code
            .find("async fn attach_devices(")
            .expect("no attach_devices")..];
        &body[..body.find("\n}\n").expect("attach_devices does not end")]
    }

    /// The daemon must start once the device nodes exist, and before the checks
    /// and the CDI spec: `nvidia-smi -L` then opens a GPU the daemon already
    /// initialised, and the spec mounts the daemon's socket into the container.
    #[test]
    fn persistenced_starts_after_the_device_nodes_and_before_the_cdi_spec() {
        let body = attach_devices_body();
        let nodes = body.find("create_device_nodes(").expect("no device nodes");
        let daemon = body
            .find("start_persistenced(driver_root)")
            .expect("attach_devices does not start nvidia-persistenced");
        let smi = body
            .find("list_gpus(driver_root)")
            .expect("no nvidia-smi -L");
        let cdi = body
            .find("generate_cdi_spec(driver_root)")
            .expect("no CDI generation");
        assert!(
            nodes < daemon && daemon < smi && daemon < cdi,
            "nvidia-persistenced must start after the device nodes ({nodes}) and before \
             nvidia-smi -L ({smi}) and the CDI spec ({cdi}); it starts at {daemon}"
        );
    }

    /// With the daemon up, `nvidia-smi -L` and the CDI generation only read the
    /// initialised GPUs, so they run together rather than one after the other.
    #[test]
    fn the_gpu_check_and_the_cdi_spec_run_together() {
        let body = attach_devices_body();
        let join = body
            .find("tokio::join!(")
            .expect("attach_devices no longer joins the GPU check and the CDI spec");
        let joined = &body[join..join + body[join..].find(");").unwrap()];
        assert!(
            joined.contains("list_gpus(driver_root)")
                && joined.contains("generate_cdi_spec(driver_root)"),
            "nvidia-smi -L and the CDI generation must be joined: {joined}"
        );
    }

    /// A launch that starts while an attach is loading the driver waits for the
    /// attach and gets its devices; one that comes first makes a later attach fail.
    #[tokio::test]
    async fn a_launch_during_an_attach_waits_for_its_devices() {
        let gate = std::sync::Arc::new(LaunchGate::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let attaching = {
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.attach(1, || async move {
                    started_tx.send(()).unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(AttachReport {
                        gpus: 1,
                        ..Default::default()
                    })
                })
                .await
            })
        };
        started_rx.await.unwrap();
        assert_eq!(gate.launch().await, vec![CDI_ALL_GPUS.to_string()]);
        assert_eq!(attaching.await.unwrap().unwrap().gpus, 1);

        let late = gate
            .attach(1, || async { Ok(AttachReport::default()) })
            .await
            .unwrap_err();
        assert!(format!("{late:#}").contains("already running"), "{late:#}");
    }

    #[test]
    fn a_gpu_run_refuses_to_launch_in_a_vm_that_was_not_restored() {
        assert!(check_gpu_restore(true, true).is_ok());
        assert!(check_gpu_restore(false, false).is_ok());
        let err = check_gpu_restore(true, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to start the container"),
            "{err:#}"
        );
    }

    use super::*;

    fn write_function(sysfs: &Path, address: &str, vendor: &str, device: &str, class: &str) {
        let dir = sysfs.join("bus/pci/devices").join(address);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vendor"), format!("{vendor}\n")).unwrap();
        std::fs::write(dir.join("device"), format!("{device}\n")).unwrap();
        std::fs::write(dir.join("class"), format!("{class}\n")).unwrap();
    }

    fn fake_sysfs() -> tempfile::TempDir {
        let sysfs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(sysfs.path().join("bus/pci/devices")).unwrap();
        std::fs::write(sysfs.path().join("bus/pci/rescan"), "").unwrap();
        // The host bridge and a virtio-net function, as Firecracker's PCI
        // transport presents them.
        write_function(sysfs.path(), "0000:00:00.0", "0x8086", "0x0d57", "0x060000");
        write_function(sysfs.path(), "0000:00:01.0", "0x1af4", "0x1041", "0x020000");
        sysfs
    }

    #[test]
    fn only_nvidia_display_functions_count_as_gpus() {
        let sysfs = fake_sysfs();
        // A T4 (3D controller) and an NVIDIA audio function, which is not a GPU.
        write_function(sysfs.path(), "0000:00:02.0", "0x10de", "0x1eb8", "0x030200");
        write_function(sysfs.path(), "0000:00:03.0", "0x10de", "0x10f8", "0x040300");
        let functions = read_pci_functions(sysfs.path()).unwrap();
        assert_eq!(functions.len(), 4);
        let gpus = nvidia_gpus(&functions);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].address, "0000:00:02.0");
        assert_eq!(gpus[0].device, 0x1eb8);
    }

    #[tokio::test]
    async fn rescan_returns_once_the_announced_gpus_are_present() {
        let sysfs = fake_sysfs();
        write_function(sysfs.path(), "0000:00:02.0", "0x10de", "0x1eb8", "0x030200");
        let gpus = rescan_for_gpus(sysfs.path(), &["0000:01:00.0".to_string()])
            .await
            .unwrap();
        assert_eq!(gpus.len(), 1);
        assert_eq!(
            std::fs::read_to_string(sysfs.path().join("bus/pci/rescan")).unwrap(),
            "1",
            "the rescan must be requested"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_gpu_that_never_appears_fails_and_names_what_the_bus_holds() {
        let sysfs = fake_sysfs();
        let error = rescan_for_gpus(sysfs.path(), &["0000:01:00.0".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("attached 1 GPU(s) (0000:01:00.0)"),
            "{error}"
        );
        assert!(error.contains("guest sees 0 NVIDIA"), "{error}");
        assert!(
            error.contains("0000:00:01.0 [1af4:1041 class 020000]"),
            "the error must list the functions present: {error}"
        );
    }

    #[test]
    fn char_majors_come_from_the_character_section_only() {
        let devices = "Character devices:\n  1 mem\n195 nvidia\n195 nvidiactl\n\
                       235 nvidia-uvm\n\nBlock devices:\n235 nvidia-uvm-fake\n259 blkext\n";
        let majors = parse_char_majors(devices);
        assert_eq!(majors.get("nvidia-uvm"), Some(&235));
        assert_eq!(majors.get("mem"), Some(&1));
        assert!(!majors.contains_key("blkext"));
    }

    #[test]
    fn device_minor_is_read_from_the_information_file() {
        let info = "Model: \t\t Tesla T4\nIRQ:   \t\t 45\nGPU UUID: \t GPU-0000\n\
                    Video BIOS: \t 90.04.96.00.02\nBus Type: \t PCIe\nDMA Size: \t 47 bits\n\
                    DMA Mask: \t 0x7fffffffffff\nBus Location: \t 0000:00:02.0\n\
                    Device Minor: \t 3\nGPU Excluded:\t No\n";
        assert_eq!(parse_device_minor(info), Some(3));
        assert_eq!(parse_device_minor("Model: x\n"), None);
    }

    #[test]
    fn device_nodes_cover_control_each_gpu_and_uvm() {
        let majors = parse_char_majors("Character devices:\n195 nvidia\n511 nvidia-uvm\n");
        let nodes = device_nodes(&majors, &[0, 1]).unwrap();
        let summary: Vec<(String, u32, u32)> = nodes
            .into_iter()
            .map(|n| (n.path, n.major, n.minor))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("/dev/nvidiactl".to_string(), 195, 255),
                ("/dev/nvidia0".to_string(), 195, 0),
                ("/dev/nvidia1".to_string(), 195, 1),
                ("/dev/nvidia-uvm".to_string(), 511, 0),
                ("/dev/nvidia-uvm-tools".to_string(), 511, 1),
            ]
        );
    }

    #[test]
    fn device_nodes_refuse_a_missing_uvm_major() {
        let error = device_nodes(&BTreeMap::new(), &[0])
            .unwrap_err()
            .to_string();
        assert!(error.contains("nvidia-uvm"), "{error}");
    }

    #[test]
    fn a_driver_image_for_another_kernel_names_both_releases() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(module_dir(root.path(), "6.18.50")).unwrap();
        assert!(check_module_dir(root.path(), "6.18.50").is_ok());
        let error = check_module_dir(root.path(), "6.18.51")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("modules for kernel 6.18.50 but the guest runs 6.18.51"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_driver_image_says_the_plan_must_name_it() {
        let root = tempfile::tempdir().unwrap();
        let error = check_module_dir(root.path(), "6.18.50")
            .unwrap_err()
            .to_string();
        assert!(error.contains("gpu_driver"), "{error}");
    }

    #[test]
    fn cdi_spec_points_into_the_driver_image() {
        let args = cdi_generate_args(Path::new(DRIVER_ROOT));
        assert!(args.contains(&format!("--driver-root={DRIVER_ROOT}")));
        assert!(args.contains(&format!(
            "--nvidia-cdi-hook-path={DRIVER_ROOT}/usr/bin/nvidia-cdi-hook"
        )));
        assert!(args.contains(&format!("--output={CDI_SPEC}")));
    }
}
