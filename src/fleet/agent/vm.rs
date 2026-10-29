//! One fcvm child process running one slot's GPU VM.
//!
//! The agent starts `fcvm podman run ...` and owns the child: it reads both pipes to the
//! end (an unread pipe stalls fcvm once 64 KiB are buffered), takes the restore timings
//! from fcvm's one-line `restored` record on stdout, and is the only one to signal or reap
//! it. The child is armed with `PR_SET_PDEATHSIG`, so if the agent dies the kernel sends
//! fcvm SIGTERM and fcvm tears its VM down.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// How one slot's VM is started.
#[derive(Debug, Clone)]
pub struct VmSpec {
    pub fcvm: PathBuf,
    pub name: String,
    /// The kernel profile: `gpu` for a renderer, `phone` for a phone.
    pub profile: String,
    pub sbdf: String,
    /// `<ip>:<port>:8080`
    pub publish: String,
    pub extra_args: Vec<String>,
    pub image: String,
    /// The container's command, after the image (none: the image's own).
    pub command: Vec<String>,
    /// Restore a clone of this snapshot (`fcvm snapshot run`) instead of `podman run`.
    pub clone_of: Option<String>,
}

impl VmSpec {
    /// A slot's VM: its renderer published on `port` (HTTP, guest 8080) and `tls_port` (TLS,
    /// guest 443) at `publish_ip`, routed at `ipv6` when the host has a prefix.
    /// fleet/ami/host/warm-root.sh launches the AMI's warm-up VM the same way, so a woken
    /// host's first restores find its pre-start snapshot
    /// (`the_ami_warm_up_launches_its_vm_as_a_slot_is_launched`).
    #[allow(clippy::too_many_arguments)]
    pub fn for_slot(
        fcvm: PathBuf,
        name: String,
        sbdf: String,
        publish_ip: &str,
        port: u16,
        tls_port: Option<u16>,
        ipv6: Option<(&super::addr::Prefix, std::net::Ipv6Addr)>,
        extra: &[String],
        image: String,
    ) -> VmSpec {
        let mut extra_args = match ipv6 {
            Some((p, a)) => super::addr::vm_args(p, a),
            None => Vec::new(),
        };
        if let Some(p) = tls_port {
            extra_args.push("--publish".into());
            extra_args.push(format!("{publish_ip}:{p}:443"));
        }
        extra_args.extend(extra.iter().cloned());
        VmSpec {
            fcvm,
            name,
            profile: "gpu".into(),
            sbdf,
            publish: format!("{publish_ip}:{port}:8080"),
            extra_args,
            image,
            command: Vec::new(),
            clone_of: None,
        }
    }

    /// A phone slot's VM: the slot's networking as for a renderer, the phone profile (KVM
    /// inside, the Firecracker that keeps nested state across a snapshot), and the phone
    /// container running `phone.sh serve` with the AOSP build, the emulator and the app
    /// mapped (portable volumes, so a warm snapshot of it restores on any host with the
    /// same files). Nothing in it names the slot or the GPU, so every phone restores from
    /// one pre-start snapshot, and fleet/ami/host/warm-phone.sh launches the phone its warm
    /// snapshot is taken of the same way.
    #[allow(clippy::too_many_arguments)]
    pub fn for_phone(
        fcvm: PathBuf,
        name: String,
        sbdf: String,
        publish_ip: &str,
        port: u16,
        tls_port: Option<u16>,
        ipv6: Option<(&super::addr::Prefix, std::net::Ipv6Addr)>,
        extra: &[String],
        phone: &PhoneCfg,
    ) -> VmSpec {
        let apk = phone
            .apk
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let work = phone
            .apk
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".into());
        let mut own: Vec<String> = vec![
            "--privileged".into(),
            "--cpu".into(),
            PHONE_CPUS.into(),
            "--mem".into(),
            PHONE_MEM_MIB.into(),
            "--portable-volumes".into(),
            "--map".into(),
            format!("{}:/android:ro", phone.aosp.display()),
            "--map".into(),
            format!("{}:/emu:ro", phone.emulator.display()),
            "--map".into(),
            format!("{work}:/work:ro"),
            "--env".into(),
            format!("PHONE_APK=/work/{apk}"),
        ];
        own.extend(extra.iter().cloned());
        let mut spec = VmSpec::for_slot(
            fcvm,
            name,
            sbdf,
            publish_ip,
            port,
            tls_port,
            ipv6,
            &own,
            phone.image.clone(),
        );
        spec.profile = "phone".into();
        spec.command = vec!["/usr/local/bin/phone.sh".into(), "serve".into()];
        spec
    }

    /// A phone slot's VM as a clone of the warm snapshot of a running phone (flow B):
    /// `fcvm snapshot run` onto this slot's GPU, with its own ports, address and this host's
    /// certificate directory (the read-only maps of `extra`); the rest is the snapshot's.
    #[allow(clippy::too_many_arguments)]
    pub fn for_phone_clone(
        fcvm: PathBuf,
        name: String,
        sbdf: String,
        publish_ip: &str,
        port: u16,
        tls_port: Option<u16>,
        ipv6: Option<(&super::addr::Prefix, std::net::Ipv6Addr)>,
        extra: &[String],
        snapshot: &str,
    ) -> VmSpec {
        let mut extra_args = Vec::new();
        if let Some((p, a)) = ipv6 {
            extra_args.extend([
                "--ipv6-prefix".into(),
                p.to_string(),
                "--ipv6-address".into(),
                a.to_string(),
            ]);
        }
        if let Some(p) = tls_port {
            extra_args.push("--publish".into());
            extra_args.push(format!("{publish_ip}:{p}:443"));
        }
        // a clone takes this host's read-only maps (the certificate); everything else, the
        // environment included, is the snapshot's
        let mut it = extra.iter();
        while let Some(a) = it.next() {
            if a == "--map" {
                if let Some(m) = it.next().filter(|m| m.ends_with(":ro")) {
                    extra_args.push("--map".into());
                    extra_args.push(m.clone());
                }
            }
        }
        VmSpec {
            fcvm,
            name,
            profile: "phone".into(),
            sbdf,
            publish: format!("{publish_ip}:{port}:8080"),
            extra_args,
            image: String::new(),
            command: Vec::new(),
            clone_of: Some(snapshot.to_string()),
        }
    }

    pub fn args(&self) -> Vec<String> {
        if let Some(snapshot) = &self.clone_of {
            let mut a: Vec<String> = [
                "snapshot",
                "run",
                "--snapshot",
                snapshot,
                "--name",
                &self.name,
                "--gpu",
                &self.sbdf,
                "--publish",
                &self.publish,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            a.extend(self.extra_args.iter().cloned());
            return a;
        }
        let mut a: Vec<String> = [
            "podman",
            "run",
            "--name",
            &self.name,
            "--kernel-profile",
            &self.profile,
            "--gpu",
            &self.sbdf,
            "--publish",
            &self.publish,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        a.extend(self.extra_args.iter().cloned());
        a.push(self.image.clone());
        a.extend(self.command.iter().cloned());
        a
    }
}

/// A phone VM's size: the spike's (docs/fleet/ARCHITECTURE.md, "The phone"), a 6 GiB VM for a
/// 2 GiB phone with 4 cores, which keeps the VFIO pin at restore short.
pub const PHONE_CPUS: &str = "6";
pub const PHONE_MEM_MIB: &str = "6144";

/// What a phone slot runs.
#[derive(Debug, Clone, PartialEq)]
pub struct PhoneCfg {
    pub image: String,
    /// scripts/phone/aosp.sh --root-path: the system image and adb, at /android
    pub aosp: PathBuf,
    /// scripts/phone/emulator.sh --path: the emulator, at /emu
    pub emulator: PathBuf,
    /// Robot Park (scripts/phone/app.sh); its directory at /work
    pub apk: PathBuf,
    /// The warm snapshot of a running phone every phone is a clone of (flow B); none: each
    /// phone boots cold
    pub snapshot: Option<String>,
}

/// The command that carries out a take-over action in a running phone: `phone.sh action
/// <name>` in its container, through `fcvm exec`. Each action is one of the enumerated
/// names, never text from a viewer. `None` for `reboot_phone`, which the coordinator turns
/// into a relaunch of the VM.
pub fn phone_action_argv(
    vm_pid: u32,
    action: fleet_core::phone::PhoneAction,
) -> Option<Vec<String>> {
    use fleet_core::phone::PhoneAction;
    if action == PhoneAction::RebootPhone {
        return None;
    }
    Some(
        [
            "exec",
            "--pid",
            &vm_pid.to_string(),
            "--",
            "/usr/local/bin/phone.sh",
            "action",
            action.as_str(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    )
}

/// What fcvm's `restored` record says about the restore.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Restored {
    pub at: Instant,
    pub ack_ms: Option<u64>,
    pub gpu_ms: Option<u64>,
}

/// Parse a stdout line; `Some` for fcvm's `{"status":"restored",...}` record.
pub fn parse_restored(line: &str, at: Instant) -> Option<Restored> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("status")?.as_str()? != "restored" {
        return None;
    }
    let ms = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_f64()).map(|x| x.round() as u64);
    Some(Restored {
        at,
        ack_ms: ms(v.get("ack_ms")),
        gpu_ms: ms(v.pointer("/guest_phases/gpu_ms")),
    })
}

const TAIL_LINES: usize = 20;

/// Resolves when fcvm prints its restore record; pending forever if it never does.
pub async fn wait_restored(rx: &mut watch::Receiver<Option<Restored>>) -> Restored {
    loop {
        if let Some(r) = *rx.borrow_and_update() {
            return r;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub struct Vm {
    child: Child,
    pub pid: u32,
    pub started: Instant,
    restored: watch::Receiver<Option<Restored>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    exit: Option<ExitStatus>,
}

impl Vm {
    pub fn spawn(spec: &VmSpec, slot: &str) -> Result<Vm> {
        let mut cmd = Command::new(&spec.fcvm);
        cmd.args(spec.args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        arm_pdeathsig(cmd.as_std_mut());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", spec.fcvm.display()))?;
        let pid = child.id().context("fcvm exited before its pid was read")?;
        let (tx, restored) = watch::channel(None);
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let stdout = child.stdout.take().context("fcvm stdout")?;
        let stderr = child.stderr.take().context("fcvm stderr")?;
        let slot_out = slot.to_string();
        let tail_out = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(r) = parse_restored(&line, Instant::now()) {
                    let _ = tx.send(Some(r));
                }
                tracing::debug!(target: "fcvm::fleet::vm", slot = %slot_out, "stdout: {line}");
                push_tail(&tail_out, line);
            }
        });
        let slot_err = slot.to_string();
        let tail_err = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "fcvm::fleet::vm", slot = %slot_err, "{line}");
                push_tail(&tail_err, line);
            }
        });
        Ok(Vm {
            child,
            pid,
            started: Instant::now(),
            restored,
            tail,
            exit: None,
        })
    }

    /// The restore record, once fcvm has printed it.
    pub fn restored(&self) -> Option<Restored> {
        *self.restored.borrow()
    }

    /// A receiver of the restore record, for [`wait_restored`].
    pub fn restored_watch(&self) -> watch::Receiver<Option<Restored>> {
        self.restored.clone()
    }

    /// Resolves when the fcvm process exits. Cancel safe.
    pub async fn exited(&mut self) -> ExitStatus {
        if let Some(s) = self.exit {
            return s;
        }
        let s = match self.child.wait().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(pid = self.pid, error = %e, "waiting for fcvm failed");
                std::future::pending().await
            }
        };
        self.exit = Some(s);
        s
    }

    /// The last lines fcvm printed, for a failure report.
    pub fn tail(&self) -> String {
        let t = self.tail.lock().unwrap();
        t.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// SIGTERM, wait up to `grace`, then SIGKILL; returns once the process has exited and
    /// been reaped.
    pub async fn stop(&mut self, grace: Duration) -> ExitStatus {
        if let Some(s) = self.exit {
            return s;
        }
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        match tokio::time::timeout(grace, self.exited()).await {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!(
                    pid = self.pid,
                    "fcvm ignored SIGTERM for {grace:?}; killing it"
                );
                let _ = self.child.start_kill();
                self.exited().await
            }
        }
    }
}

/// A Vm dropped while fcvm runs (the agent returning, a slot task ending) sends it
/// SIGTERM, which tears the VM down, and does not wait. Not SIGKILL (tokio's
/// `kill_on_drop`): a SIGKILLed fcvm leaves a routed VM's namespace, veth and host route
/// behind, and the next VM given that address is refused.
impl Drop for Vm {
    fn drop(&mut self) {
        // try_wait reaps an exited child, so the pid signalled below is still fcvm's.
        if self.exit.is_none() && matches!(self.child.try_wait(), Ok(None)) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}

fn push_tail(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut t = tail.lock().unwrap();
    t.push_back(line);
    while t.len() > TAIL_LINES {
        t.pop_front();
    }
}

/// The kernel sends the child SIGTERM when the agent thread that spawned it dies. The
/// spawning threads are the runtime's workers, which live as long as the agent.
fn arm_pdeathsig(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    let agent = std::process::id() as libc::pid_t;
    // SAFETY: runs in the forked child before exec and makes two async-signal-safe calls;
    // both error paths build an errno-only io::Error, which does not allocate.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The agent died between the fork and the prctl: start nothing unsupervised.
            if libc::getppid() != agent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_phone_action_maps_to_its_fixed_command_and_reboot_to_none() {
        use fleet_core::phone::PhoneAction;
        // The container's controller must handle every name the agent sends.
        let script = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fleet/phone/phone.sh"),
        )
        .unwrap();
        let controller = script
            .split("\naction)\n")
            .nth(1)
            .and_then(|s| s.split("\n  ;;\n").next())
            .expect("phone.sh has an action command");
        for a in PhoneAction::ALL {
            match super::phone_action_argv(42, a) {
                None => assert_eq!(a, PhoneAction::RebootPhone),
                Some(argv) => {
                    assert_eq!(
                        argv,
                        [
                            "exec",
                            "--pid",
                            "42",
                            "--",
                            "/usr/local/bin/phone.sh",
                            "action",
                            a.as_str()
                        ]
                    );
                    assert!(
                        controller.lines().any(|l| l
                            .trim_start()
                            .split(')')
                            .next()
                            .is_some_and(|labels| labels.split('|').any(|x| x == a.as_str()))),
                        "phone.sh action has no case for {}",
                        a.as_str()
                    );
                }
            }
        }
    }

    fn phone() -> super::PhoneCfg {
        super::PhoneCfg {
            image: "localhost/fcvm-phone:fleet".into(),
            aosp: "/store/android-aosp/abc".into(),
            emulator: "/store/android-emulator/def/emulator".into(),
            apk: "/store/android-aosp/abc/phone-work/robotpark-1.apk".into(),
            snapshot: None,
        }
    }
    const TLS: [&str; 4] = [
        "--map",
        "/etc/fleet-tls:/run/fleet-tls:ro",
        "--env",
        "TLS_CERT_FILE=/run/fleet-tls/tls.json",
    ];

    #[test]
    fn every_phone_slot_launches_the_same_vm_but_for_its_name_gpu_and_ports() {
        let tls: Vec<String> = TLS.iter().map(|s| s.to_string()).collect();
        let spec = |name: &str, sbdf: &str, port: u16| {
            super::VmSpec::for_phone(
                "/usr/local/bin/fcvm".into(),
                name.into(),
                sbdf.into(),
                "127.0.0.1",
                port,
                Some(port + 100),
                None,
                &tls,
                &phone(),
            )
            .args()
        };
        // fcvm keys the pre-start snapshot on everything but these, so one snapshot serves
        // every phone slot of every host
        let keyed = |a: Vec<String>| {
            let mut out = Vec::new();
            let mut it = a.into_iter();
            while let Some(x) = it.next() {
                if x == "--name" || x == "--gpu" || x == "--publish" {
                    it.next();
                    continue;
                }
                out.push(x);
            }
            out
        };
        let a = spec("fleet-phone-1", "0000:02:00.0", 20101);
        let b = spec("fleet-phone-3", "0000:04:00.0", 20103);
        assert_ne!(a, b);
        assert_eq!(keyed(a.clone()), keyed(b));
        let has = |pair: [&str; 2]| a.windows(2).any(|w| w == pair);
        assert!(has(["--kernel-profile", "phone"]), "{a:?}");
        assert!(
            has(["--map", "/store/android-aosp/abc:/android:ro"]),
            "{a:?}"
        );
        assert!(
            has(["--map", "/store/android-emulator/def/emulator:/emu:ro"]),
            "{a:?}"
        );
        assert!(
            has(["--map", "/store/android-aosp/abc/phone-work:/work:ro"]),
            "{a:?}"
        );
        assert!(has(["--env", "PHONE_APK=/work/robotpark-1.apk"]), "{a:?}");
        assert!(
            a.contains(&"--portable-volumes".to_string())
                && a.contains(&"--privileged".to_string())
        );
        assert_eq!(
            a[a.len() - 3..],
            [
                "localhost/fcvm-phone:fleet",
                "/usr/local/bin/phone.sh",
                "serve"
            ]
        );
    }

    #[test]
    fn a_phone_clone_restores_the_warm_snapshot_with_its_own_gpu_ports_address_and_certificate() {
        let tls: Vec<String> = TLS.iter().map(|s| s.to_string()).collect();
        let prefix = crate::fleet::agent::addr::Prefix::parse("2001:db8:1:2::/64").unwrap();
        let addr: std::net::Ipv6Addr = "2001:db8:1:2::5".parse().unwrap();
        let a = super::VmSpec::for_phone_clone(
            "/usr/local/bin/fcvm".into(),
            "fleet-phone-2".into(),
            "0000:03:00.0".into(),
            "127.0.0.1",
            20102,
            Some(20202),
            Some((&prefix, addr)),
            &tls,
            "phone-warm",
        )
        .args();
        let p = prefix.to_string();
        assert_eq!(
            a,
            [
                "snapshot",
                "run",
                "--snapshot",
                "phone-warm",
                "--name",
                "fleet-phone-2",
                "--gpu",
                "0000:03:00.0",
                "--publish",
                "127.0.0.1:20102:8080",
                "--ipv6-prefix",
                p.as_str(),
                "--ipv6-address",
                "2001:db8:1:2::5",
                "--publish",
                "127.0.0.1:20202:443",
                "--map",
                "/etc/fleet-tls:/run/fleet-tls:ro",
            ]
        );
    }

    use super::*;

    #[test]
    fn restored_record_gives_ack_and_gpu_times() {
        let now = Instant::now();
        let r = parse_restored(
            r#"{"status":"restored","name":"fleet-slot-1","ack_ms":4157,"guest_phases":{"gpu_ms":3337.4,"total_ms":4100}}"#,
            now,
        )
        .unwrap();
        assert_eq!((r.ack_ms, r.gpu_ms), (Some(4157), Some(3337)));
        let bare = parse_restored(
            r#"{"status":"restored","ack_ms":10,"guest_phases":null}"#,
            now,
        )
        .unwrap();
        assert_eq!((bare.ack_ms, bare.gpu_ms), (Some(10), None));
        assert!(parse_restored("✓ VM 'x' cloned from snapshot 'y'", now).is_none());
        assert!(parse_restored(r#"{"status":"ready"}"#, now).is_none());
    }

    /// A Vm dropped without `stop` (the agent returning, a slot task ending) must give
    /// fcvm SIGTERM, which tears the VM down, not SIGKILL: a SIGKILLed routed fcvm leaves
    /// its namespace, veth and host route, and the next VM at that address is refused.
    #[tokio::test]
    async fn dropping_a_vm_gives_fcvm_sigterm() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("term");
        let script = dir.path().join("fake-fcvm");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap 'echo term > {}; exit 0' TERM\necho ready\nwhile :; do sleep 0.05; done\n",
                mark.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = VmSpec {
            command: Vec::new(),
            clone_of: None,
            profile: "gpu".into(),
            fcvm: script,
            name: "fleet-slot-1".into(),
            sbdf: "0000:02:00.0".into(),
            publish: "127.0.0.1:20001:8080".into(),
            extra_args: vec![],
            image: "renderer:1".into(),
        };
        let vm = Vm::spawn(&spec, "slot-1").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !vm.tail().contains("ready") {
            assert!(Instant::now() < deadline, "the fake fcvm never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(vm);
        while !mark.exists() {
            assert!(
                Instant::now() < deadline,
                "fcvm got no SIGTERM when its Vm was dropped"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The shape of a `podman run` command: each flag and its value, except that a publish
    /// is its guest port and a name, GPU or address is `v` (those differ per VM and are in
    /// no snapshot key).
    fn shape(words: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < words.len() {
            let w = &words[i];
            if w.starts_with("--") && i + 1 < words.len() {
                out.push(w.clone());
                let v = &words[i + 1];
                out.push(match w.as_str() {
                    "--publish" => format!("guest:{}", v.rsplit(':').next().unwrap()),
                    "--name" | "--gpu" | "--ipv6-prefix" | "--ipv6-address" => "v".into(),
                    _ => v.clone(),
                });
                i += 2;
            } else {
                out.push(w.clone());
                i += 1;
            }
        }
        out
    }

    /// fleet/ami/host/warm-root.sh's VM leaves the pre-start snapshot a woken host's slots
    /// restore, so it must be launched as a slot is: the same flags in the same order, the
    /// same guest ports, the agent's image. The AMI's only warm-up used to be warm.sh, a
    /// rootless VM published on one port, whose snapshot key no slot ever had: every slot of
    /// every woken host cold-booted and took a snapshot before its first restore.
    #[test]
    fn the_ami_warm_up_launches_its_vm_as_a_slot_is_launched() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fleet/ami/host/warm-root.sh");
        let script = std::fs::read_to_string(path).unwrap();
        // The script's constants (`NAME=value` with no expansion), substituted into the command.
        let mut joined = script.replace("\\\n", " ");
        for (name, value) in script.lines().filter_map(|l| l.split_once('=')) {
            if name.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !value.contains('$') {
                joined = joined
                    .replace(&format!("\"${name}\""), value)
                    .replace(&format!("${name}"), value);
            }
        }
        assert!(
            joined.contains("localhost/fcvm-renderer:fleet"),
            "warm-root.sh sets IMG"
        );
        let line = joined
            .lines()
            .find(|l| l.contains("fcvm podman run"))
            .expect("warm-root.sh runs fcvm podman run");
        let words: Vec<String> = line[line.find("podman run").unwrap()..]
            .split_whitespace()
            .take_while(|w| !w.starts_with('>'))
            .map(str::to_string)
            .collect();
        let prefix = super::super::addr::Prefix::parse("2001:db8:1:2:3::/80").unwrap();
        let slot = VmSpec::for_slot(
            "fcvm".into(),
            "fleet-slot-3".into(),
            "0000:18:00.0".into(),
            "0.0.0.0",
            20103,
            Some(20203),
            Some((&prefix, "2001:db8:1:2:3::5".parse().unwrap())),
            // What --tls-secret adds to every slot's command, at the default --tls-dir.
            &super::super::tls::vm_args(std::path::Path::new(super::super::tls::HOST_DIR)),
            // The agent's --image on the fleet's hosts (ejc3/aws fcvm-fleet.tf).
            "localhost/fcvm-renderer:fleet".into(),
        );
        assert_eq!(shape(&words), shape(&slot.args()), "warm-root.sh: {line}");
    }

    #[test]
    fn spec_builds_the_documented_command() {
        let s = VmSpec {
            command: Vec::new(),
            clone_of: None,
            profile: "gpu".into(),
            fcvm: "fcvm".into(),
            name: "fleet-slot-2".into(),
            sbdf: "0000:02:00.0".into(),
            publish: "127.0.0.1:20001:8080".into(),
            extra_args: vec!["--cpu".into(), "4".into()],
            image: "renderer:1".into(),
        };
        assert_eq!(
            s.args().join(" "),
            "podman run --name fleet-slot-2 --kernel-profile gpu --gpu 0000:02:00.0 --publish 127.0.0.1:20001:8080 --cpu 4 renderer:1"
        );
    }
}
