//! `--confine` end to end: a confined rootless VM boots, restores from its pre-start
//! snapshot, runs a command, and its Firecracker holds no capabilities, while a host below
//! the Landlock floor refuses the start naming the ABI it has.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Every thread of `pid`: its name, then CapEff, CapPrm, CapInh, CapBnd, CapAmb,
/// NoNewPrivs and Seccomp, from its status.
fn thread_credentials(pid: u32) -> Result<Vec<(String, Vec<String>)>> {
    let mut threads = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        let status = std::fs::read_to_string(task?.path().join("status"))?;
        let field = |name: &str| -> Result<String> {
            status
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name}:")))
                .map(|v| v.trim().to_string())
                .with_context(|| format!("{name} in the status of a thread of {pid}"))
        };
        let mut values = Vec::new();
        for name in [
            "CapEff",
            "CapPrm",
            "CapInh",
            "CapBnd",
            "CapAmb",
            "NoNewPrivs",
            "Seccomp",
        ] {
            values.push(field(name)?);
        }
        threads.push((field("Name")?, values));
    }
    Ok(threads)
}

#[tokio::test]
async fn test_confine_rootless_vm() -> Result<()> {
    let floor = fcvm::confine::LANDLOCK_ABI_FLOOR;
    let abi = fcvm::confine::landlock_abi();
    // Logged so a CI job says which branch ran and why.
    println!("landlock ABI on this host: {abi:?}; confinement needs {floor}");
    // The core dump settings decide the VMM's core limit, or a refusal, as the preflight
    // and the process layer do; logged so a CI job says what its runner has.
    let core_pattern = std::fs::read_to_string("/proc/sys/kernel/core_pattern")
        .map(|p| p.trim_end().to_string())
        .ok();
    let core_hard = fcvm::confine::core_hard_limit();
    println!("core_pattern: {core_pattern:?}; hard core limit: {core_hard}");
    let (vm_name, _, _, _) = common::unique_names("confine");

    let core_refusal = match core_pattern.as_deref() {
        None => Some("process: the host's core_pattern cannot be read".to_string()),
        Some(p) if p.starts_with('@') => Some(format!("process: the host's core_pattern ({p})")),
        Some(p) if p.starts_with('|') && core_hard < 1 => Some(format!(
            "process: the host's core_pattern ({p}) pipes core dumps"
        )),
        Some(_) => None,
    };
    let refusal = match abi {
        Ok(abi) if abi >= floor => core_refusal,
        Ok(abi) => Some(format!("landlock: the kernel's Landlock ABI is {abi};")),
        Err(_) => Some("landlock: Landlock is unavailable".to_string()),
    };

    if let Some(expected) = refusal {
        return assert_a_confined_start_is_refused(&vm_name, &expected).await;
    }

    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--confine",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run --confine")?;
    let result = async {
        common::poll_health_by_pid(fcvm_pid, 300).await?;
        let out = common::exec_in_container(fcvm_pid, &["echo", "confined-ok"]).await?;
        anyhow::ensure!(out.contains("confined-ok"), "exec output: {out}");

        let firecrackers = common::firecracker_children(fcvm_pid);
        anyhow::ensure!(
            firecrackers.len() == 1,
            "firecracker children of {fcvm_pid}: {firecrackers:?}"
        );
        let threads = thread_credentials(firecrackers[0])?;
        println!("firecracker threads: {threads:?}");
        // The VMM was given the confined filter as bytes fcvm verified (a descriptor, not
        // its default policy, whose filter would also read Seccomp 2), and every thread
        // runs under a filter and holds nothing.
        let failures = common::vmm_confinement_failures(firecrackers[0]);
        anyhow::ensure!(failures.is_empty(), "not confined: {failures:?}");
        let zero = "0000000000000000".to_string();
        let expected = vec![
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero,
            "1".to_string(),
            "2".to_string(),
        ];
        for (name, values) in &threads {
            anyhow::ensure!(
                values == &expected,
                "thread {name}: CapEff, CapPrm, CapInh, CapBnd, CapAmb, NoNewPrivs, Seccomp = {values:?}"
            );
        }
        let limits = std::fs::read_to_string(format!("/proc/{}/limits", firecrackers[0]))?;
        let core = limits
            .lines()
            .find(|l| l.starts_with("Max core file size"))
            .context("no core limit in /proc/<pid>/limits")?;
        let want = fcvm::confine::core_limit_for(core_hard).to_string();
        anyhow::ensure!(
            core.split_whitespace()
                .skip(4)
                .take(2)
                .eq([want.as_str(), want.as_str()]),
            "core limit: {core}, expected {want} from a hard limit of {core_hard}"
        );

        let ls = tokio::process::Command::new(common::find_fcvm_binary()?)
            .args(["ls", "--json"])
            .output()
            .await?;
        let vms: Vec<serde_json::Value> = serde_json::from_slice(&ls.stdout)
            .with_context(|| format!("fcvm ls --json: {}", String::from_utf8_lossy(&ls.stdout)))?;
        let vm = vms
            .iter()
            .find(|vm| vm["pid"] == serde_json::json!(fcvm_pid))
            .with_context(|| format!("no VM with pid {fcvm_pid} in fcvm ls --json"))?;
        anyhow::ensure!(
            vm["confinement"]["layers"] == serde_json::json!(["process", "seccomp"])
                && vm["confinement"]["seccomp"] == serde_json::json!("enforce"),
            "fcvm ls --json confinement: {vm}"
        );

        // The process layer runs before PR_SET_PDEATHSIG and changes no id, so the confined
        // VMM still dies with fcvm, even when fcvm is SIGKILLed.
        // SAFETY: kill with a pid this test spawned.
        anyhow::ensure!(unsafe { libc::kill(fcvm_pid as i32, libc::SIGKILL) } == 0);
        let firecracker = firecrackers[0];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::path::Path::new(&format!("/proc/{firecracker}")).exists()
            && !std::fs::read_to_string(format!("/proc/{firecracker}/stat"))
                .map(|s| s.contains(") Z "))
                .unwrap_or(true)
        {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "the confined firecracker {firecracker} outlived its SIGKILLed fcvm by 10 s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(())
    }
    .await;
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    result
}

/// On a host that cannot confine, a start that must be confined fails closed, before
/// anything is created, naming what is missing.
async fn assert_a_confined_start_is_refused(vm_name: &str, expected: &str) -> Result<()> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new(common::find_fcvm_binary()?)
            .args([
                "podman",
                "run",
                "--name",
                vm_name,
                "--network",
                "rootless",
                "--confine",
                common::TEST_IMAGE,
            ])
            .output(),
    )
    .await
    .context("a refused --confine start did not exit")??;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains(expected) && stderr.contains("nothing was started"),
        "expected {expected:?} in: {stderr}"
    );
    println!("refused, as required: {expected}");
    Ok(())
}

/// What a host that cannot confine says when asked to, or `None` on one that can (see
/// `test_confine_rootless_vm`, which asserts the refusal itself).
fn host_refusal() -> Option<String> {
    let floor = fcvm::confine::LANDLOCK_ABI_FLOOR;
    let core_pattern = std::fs::read_to_string("/proc/sys/kernel/core_pattern")
        .map(|p| p.trim_end().to_string())
        .ok();
    let core_hard = fcvm::confine::core_hard_limit();
    let core_refusal = match core_pattern.as_deref() {
        None => Some("process: the host's core_pattern cannot be read".to_string()),
        Some(p) if p.starts_with('@') => Some(format!("process: the host's core_pattern ({p})")),
        Some(p) if p.starts_with('|') && core_hard < 1 => Some(format!(
            "process: the host's core_pattern ({p}) pipes core dumps"
        )),
        Some(_) => None,
    };
    match fcvm::confine::landlock_abi() {
        Ok(abi) if abi >= floor => core_refusal,
        Ok(abi) => Some(format!("landlock: the kernel's Landlock ABI is {abi};")),
        Err(_) => Some("landlock: Landlock is unavailable".to_string()),
    }
}

/// The Firecracker `fcvm setup` built for the default profile.
async fn default_firecracker() -> Result<std::path::PathBuf> {
    let profile = fcvm::setup::get_kernel_profile("default")?.context("no default profile")?;
    fcvm::setup::get_configured_firecracker_for_profile(&profile, "default")
        .await?
        .context("the default profile configures no Firecracker")
}

/// The shared assertion fails on a process that is not a confined VMM, naming each reason:
/// this test process runs under no filter, keeps `no_new_privs` off and was handed no
/// filter. A helper that cannot fail would let every test in the gate pass unconfined.
#[test]
fn the_gate_assertion_fails_on_an_unconfined_process() {
    let failures = common::vmm_confinement_failures(std::process::id());
    // A test runner inside a sandbox can itself run under a filter or with no_new_privs.
    // The command line is never a confined VMM's, and no thread here has a filter beyond
    // the ones this process itself runs under.
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let mut needles = vec![
        "no --seccomp-filter /proc/self/fd/<n>",
        "threads under no seccomp filter",
    ];
    if status.contains("\nSeccomp:\t0\n") {
        needles.push("Seccomp: 0, Seccomp_filters: 0");
    }
    if status.contains("\nNoNewPrivs:\t0\n") {
        needles.push("NoNewPrivs: 0");
    }
    for needle in needles {
        assert!(
            failures.iter().any(|f| f.contains(needle)),
            "missing {needle:?} in {failures:?}"
        );
    }
    // A process that is gone is a failure too, not an empty list.
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    assert!(!common::vmm_confinement_failures(pid).is_empty());
}

/// `fcvm snapshot run --confine --firecracker-bin <firecracker>` of `snapshot`, run to its
/// exit: its stderr when it exits unsuccessfully within two minutes. A clone that is still
/// running then was started, which is an error here, and it is stopped.
async fn refused_clone(
    snapshot: &str,
    name: &str,
    firecracker: &std::path::Path,
) -> Result<String> {
    let mut command = tokio::process::Command::new(common::find_fcvm_binary()?);
    command
        .args(["snapshot", "run", "--snapshot", snapshot, "--name", name])
        .args(["--confine", "--firecracker-bin"])
        .arg(firecracker);
    refused_start(command, name).await
}

/// Run an fcvm start that has to fail, and return what it wrote to stderr. A start that is
/// still running after two minutes is stopped and is an error.
async fn refused_start(mut command: tokio::process::Command, name: &str) -> Result<String> {
    // A refused start still launches a VMM before fcvm kills it. If the test dies first,
    // fcvm must die with it, or that VMM and its holder outlive the lane.
    common::set_test_pdeathsig(&mut command);
    let mut child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning fcvm")?;
    let pid = child.id().context("the fcvm start has no pid")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let read = tokio::spawn(async move {
        let mut text = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut text).await;
        text
    });
    let exited = tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await;
    if exited.is_err() {
        common::kill_process(pid).await;
        let _ = child.wait().await;
    }
    let stderr = read.await?;
    match exited {
        Ok(status) => {
            anyhow::ensure!(!status?.success(), "{name} exited successfully: {stderr}");
            Ok(stderr)
        }
        Err(_) => anyhow::bail!("{name} was started and ran for two minutes: {stderr}"),
    }
}

/// A copy of the default Firecracker in `dir`, with a recorded enforce-mode filter of
/// three empty programs beside it: Firecracker loads the file and installs nothing.
async fn firecracker_copy_with_an_empty_filter(
    dir: &std::path::Path,
) -> Result<std::path::PathBuf> {
    use fcvm::confine::{filter_bytes_digest, filter_digest_path, filter_path, SeccompMode};
    // Writable by its owner only, whatever the umask: a filter in a directory its group
    // can write is refused.
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let firecracker = dir.join("firecracker-copy");
    std::fs::copy(default_firecracker().await?, &firecracker)?;
    let empty: std::collections::HashMap<String, Vec<u64>> = ["vmm", "api", "vcpu"]
        .into_iter()
        .map(|class| (class.to_string(), Vec::new()))
        .collect();
    let bytes = bitcode::serialize(&empty)?;
    let filter = filter_path(&firecracker, SeccompMode::Enforce);
    fcvm::confine::write_store_file(&filter, &bytes)?;
    fcvm::confine::write_store_file(
        &filter_digest_path(&filter),
        filter_bytes_digest(&bytes).as_bytes(),
    )?;
    Ok(firecracker)
}

/// The processes that run `binary`.
fn processes_running(binary: &std::path::Path) -> Result<Vec<u32>> {
    Ok(std::fs::read_dir("/proc")?
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe == binary))
        .collect())
}

/// What the thread check reports for a thread that installed no filter of its own: the
/// filters this test process runs under, which fcvm and its VMM inherit.
fn an_unfiltered_thread() -> Result<String> {
    let own = fcvm::confine::own_seccomp_filters()?;
    Ok(format!(
        "Seccomp: {}, Seccomp_filters: {own})",
        if own == 0 { 0 } else { 2 }
    ))
}

/// A cold boot (`podman run --no-snapshot`, the path through `boot()`) of a Firecracker
/// whose filter file installs nothing is killed and the run fails naming the unfiltered
/// threads. The restore path has the same case in
/// [`test_confine_refuses_a_filter_that_does_not_confine`]; before this test the cold boot's
/// check was pinned only by a search of the source for its call.
///
/// `podman run` takes its Firecracker from the config's `[firecracker]` table and has no
/// flag for another, so the run reads a copy of the config without that table and takes
/// the binary from `FCVM_FIRECRACKER_BIN`.
#[tokio::test]
async fn test_confine_cold_boot_kills_a_vmm_whose_filter_installs_nothing() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("confine-cold-empty");
    if let Some(expected) = host_refusal() {
        return assert_a_confined_start_is_refused(&vm_name, &expected).await;
    }
    let dir = tempfile::tempdir()?;
    let firecracker = firecracker_copy_with_an_empty_filter(dir.path()).await?;

    let config = std::fs::read_to_string(fcvm::setup::rootfs::find_config_file(None)?)?;
    let mut in_table = false;
    let without_firecracker: String = config
        .lines()
        .filter(|line| {
            if line.starts_with('[') {
                in_table = line.trim() == "[firecracker]";
            }
            !in_table
        })
        .map(|line| format!("{line}\n"))
        .collect();
    let plan: fcvm::setup::rootfs::Plan = toml::from_str(&without_firecracker)?;
    anyhow::ensure!(
        toml::from_str::<fcvm::setup::rootfs::Plan>(&config)?
            .firecracker
            .is_some()
            && plan.firecracker.is_none(),
        "the config copy still names a Firecracker build"
    );
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(config_dir.join("fcvm"))?;
    std::fs::write(
        config_dir.join("fcvm/rootfs-config.toml"),
        without_firecracker,
    )?;

    let mut command = tokio::process::Command::new(common::find_fcvm_binary()?);
    command
        .args(["podman", "run", "--name", &vm_name])
        .args(["--network", "rootless", "--confine", "--no-snapshot"])
        .arg(common::TEST_IMAGE)
        .env("FCVM_CONFIG_DIR", &config_dir)
        .env("FCVM_FIRECRACKER_BIN", &firecracker);
    let stderr = refused_start(command, &vm_name).await?;
    let unfiltered = an_unfiltered_thread()?;
    anyhow::ensure!(
        stderr.contains("run without a seccomp filter of their own")
            && stderr.contains(&unfiltered)
            && stderr.contains("the confined VMM was killed"),
        "a cold boot on a filter that installs nothing: {stderr}"
    );
    // Only the copy's filter installs nothing, so it was the copy that ran. None is left.
    let leftover = processes_running(&firecracker)?;
    anyhow::ensure!(
        leftover.is_empty(),
        "unfiltered VMMs still run: {leftover:?}"
    );
    Ok(())
}

/// A confined start whose filter is not the one `fcvm setup` recorded, or whose filter
/// installs nothing, does not run a guest. With a copy of the default Firecracker in a
/// directory of its own and `--firecracker-bin`:
/// - no filter beside it: refused before anything is created;
/// - the real filter without its recorded digest: refused the same way;
/// - a filter of three empty programs with a matching digest, which Firecracker accepts
///   and installs nothing for: the VMM is killed before the guest resumes, and the start
///   fails naming its unfiltered threads;
/// - the real filter with its digest: the clone starts and is confined (the control).
#[tokio::test]
async fn test_confine_refuses_a_filter_that_does_not_confine() -> Result<()> {
    let (base_name, clone_name, snapshot, _) = common::unique_names("confine-filter");
    if let Some(expected) = host_refusal() {
        // No filter is handed over on a host that cannot confine: the start is refused.
        return assert_a_confined_start_is_refused(&base_name, &expected).await;
    }
    use fcvm::confine::{filter_bytes_digest, filter_digest_path, filter_path, SeccompMode};
    let store = default_firecracker().await?;
    let real = fcvm::confine::verified_filter_bytes(&store, SeccompMode::Enforce)
        .context("the default Firecracker's confined filter")?;
    let dir = tempfile::tempdir()?;
    // Writable by its owner only, whatever the umask: a filter in a directory its group
    // can write is refused.
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let firecracker = dir.path().join("firecracker-copy");
    std::fs::copy(&store, &firecracker)?;
    let filter = filter_path(&firecracker, SeccompMode::Enforce);
    let install = |bytes: &[u8], record: bool| -> Result<()> {
        fcvm::confine::write_store_file(&filter, bytes)?;
        let digest = filter_digest_path(&filter);
        if record {
            fcvm::confine::write_store_file(&digest, filter_bytes_digest(bytes).as_bytes())?;
        } else {
            let _ = std::fs::remove_file(&digest);
        }
        Ok(())
    };

    let (mut base, base_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &base_name,
        "--network",
        "rootless",
        "--confine",
        common::TEST_IMAGE,
    ])
    .await?;
    let mut clone_pid = None;
    let result = async {
        common::poll_health_by_pid(base_pid, 300).await?;
        let created = tokio::process::Command::new(common::find_fcvm_binary()?)
            .args(["snapshot", "create", "--pid", &base_pid.to_string()])
            .args(["--tag", &snapshot])
            .output()
            .await?;
        anyhow::ensure!(
            created.status.success(),
            "snapshot create: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let vms_named = |name: String| async move {
            let ls = tokio::process::Command::new(common::find_fcvm_binary()?)
                .args(["ls", "--json"])
                .output()
                .await?;
            let vms: Vec<serde_json::Value> = serde_json::from_slice(&ls.stdout)?;
            Ok::<_, anyhow::Error>(vms.iter().filter(|vm| vm["name"] == name.as_str()).count())
        };

        // No filter beside the binary.
        let name = format!("{clone_name}-none");
        let stderr = refused_clone(&snapshot, &name, &firecracker).await?;
        anyhow::ensure!(
            stderr.contains("nothing was started")
                && stderr.contains("the confined seccomp filter is unusable"),
            "no filter: {stderr}"
        );
        anyhow::ensure!(vms_named(name).await? == 0, "a refused clone has state");

        // The real filter under the right name, without the digest setup records.
        install(&real, false)?;
        let name = format!("{clone_name}-unrecorded");
        let stderr = refused_clone(&snapshot, &name, &firecracker).await?;
        anyhow::ensure!(
            stderr.contains("nothing was started")
                && stderr.contains("the digest fcvm setup records"),
            "unrecorded filter: {stderr}"
        );

        // Three empty programs, recorded: Firecracker loads the file and installs nothing.
        let empty: std::collections::HashMap<String, Vec<u64>> = ["vmm", "api", "vcpu"]
            .into_iter()
            .map(|class| (class.to_string(), Vec::new()))
            .collect();
        install(&bitcode::serialize(&empty)?, true)?;
        let name = format!("{clone_name}-empty");
        let stderr = refused_clone(&snapshot, &name, &firecracker).await?;
        anyhow::ensure!(
            stderr.contains("run without a seccomp filter of their own")
                && stderr.contains(&an_unfiltered_thread()?),
            "empty filter: {stderr}"
        );
        let leftover = processes_running(&firecracker)?;
        anyhow::ensure!(
            leftover.is_empty(),
            "unfiltered VMMs still run: {leftover:?}"
        );

        // The control: the real filter with its digest starts, confined.
        install(&real, true)?;
        let name = format!("{clone_name}-real");
        let firecracker_arg = firecracker.to_string_lossy().into_owned();
        let (mut clone, pid) = common::spawn_fcvm(&[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot,
            "--name",
            &name,
            "--confine",
            "--firecracker-bin",
            &firecracker_arg,
        ])
        .await?;
        clone_pid = Some(pid);
        let confined = async {
            common::poll_health_by_pid(pid, 300).await?;
            let vmms = common::firecracker_children(pid);
            anyhow::ensure!(vmms.len() == 1, "firecracker children of {pid}: {vmms:?}");
            let failures = common::vmm_confinement_failures(vmms[0]);
            anyhow::ensure!(
                failures.is_empty(),
                "the control is not confined: {failures:?}"
            );
            Ok(())
        }
        .await;
        common::kill_process(pid).await;
        let _ = clone.wait().await;
        confined
    }
    .await;
    if let Some(pid) = clone_pid {
        common::kill_process(pid).await;
    }
    common::kill_process(base_pid).await;
    let _ = base.wait().await;
    let _ = std::fs::remove_dir_all(fcvm::paths::snapshot_dir().join(&snapshot));
    result
}

/// The instructions of a compiled seccomp program, as Firecracker loads them: one
/// `sock_filter` per word (code, jt, jf, k).
fn bpf_action(program: &[u64], nr: u32, arch: u32) -> u32 {
    // struct seccomp_data: nr, arch, instruction_pointer, args[6]; the arguments are zero.
    let mut data = [0u8; 64];
    data[0..4].copy_from_slice(&nr.to_le_bytes());
    data[4..8].copy_from_slice(&arch.to_le_bytes());
    let mut acc = 0u32;
    let mut pc = 0usize;
    loop {
        let insn = program[pc];
        let (code, jt, jf, k) = (
            (insn & 0xffff) as u16,
            ((insn >> 16) & 0xff) as usize,
            ((insn >> 24) & 0xff) as usize,
            (insn >> 32) as u32,
        );
        pc += 1;
        let jump = |taken: bool| if taken { jt } else { jf };
        match code {
            0x20 => {
                let at = k as usize;
                acc = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
            }
            0x00 => acc = k,
            0x54 => acc &= k,
            0x05 => pc += k as usize,
            0x15 => pc += jump(acc == k),
            0x25 => pc += jump(acc > k),
            0x35 => pc += jump(acc >= k),
            0x45 => pc += jump(acc & k != 0),
            0x06 => return k,
            other => panic!(
                "BPF instruction {other:#x} at {} is not one this reads",
                pc - 1
            ),
        }
    }
}

const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_KILL_THREAD: u32 = 0;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: (u32, u32) = (0xc000_00b7, 0xc000_003e);
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: (u32, u32) = (0xc000_003e, 0x4000_0003);

/// A child of this process that installs `program` on itself and then makes `syscall`
/// (with zero arguments beyond the three given), then exits 7. Only raw syscalls run in
/// the child. Returns its wait status.
fn status_under_filter(program: &[u64], syscall: libc::c_long, args: [libc::c_long; 3]) -> i32 {
    let fprog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr().cast_mut().cast(),
    };
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: the child makes only direct syscalls on memory this function owns, and exits.
    unsafe {
        let pid = libc::fork();
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            libc::setrlimit(libc::RLIMIT_CORE, &no_core);
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER as libc::c_ulong,
                    0 as libc::c_ulong,
                    &fprog as *const libc::sock_fprog,
                ) != 0
            {
                libc::_exit(9);
            }
            libc::syscall(syscall, args[0], args[1], args[2]);
            libc::syscall(libc::SYS_exit_group, 7);
            libc::_exit(8);
        }
        let mut status = 0;
        assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
        status
    }
}

/// The compiled filter itself denies what the policy does not list, with no VMM involved:
/// the bytes `fcvm setup` compiled for the default Firecracker are decoded, each class's
/// program is evaluated for syscalls the policy must refuse, and the `vcpu` program is
/// loaded in a child, where `socket` (which no vCPU thread may make) ends it with SIGSYS
/// and `getpid` does not.
#[test]
fn the_compiled_filter_traps_a_syscall_the_policy_does_not_list() -> Result<()> {
    let store = tokio::runtime::Runtime::new()?.block_on(default_firecracker())?;
    let bytes = fcvm::confine::verified_filter_bytes(&store, fcvm::confine::SeccompMode::Enforce)
        .context("the default Firecracker's confined filter")?;
    let programs: std::collections::HashMap<String, Vec<u64>> = bitcode::deserialize(&bytes)?;
    let mut classes: Vec<&str> = programs.keys().map(String::as_str).collect();
    classes.sort();
    // Every category the pinned Firecracker has a filter for, `blk_worker` included: no VM of
    // fcvm's has such a thread, and its program still has to refuse what the others refuse.
    assert_eq!(classes, ["api", "blk_worker", "vcpu", "vmm"]);
    let (native, foreign) = AUDIT_ARCH;
    let denied = [
        ("clone", libc::SYS_clone),
        ("clone3", libc::SYS_clone3),
        ("execve", libc::SYS_execve),
        ("execveat", libc::SYS_execveat),
        ("io_uring_setup", libc::SYS_io_uring_setup),
        ("ptrace", libc::SYS_ptrace),
        ("landlock_restrict_self", libc::SYS_landlock_restrict_self),
    ];
    for (class, program) in &programs {
        assert!(!program.is_empty(), "the {class} program is empty");
        for (name, nr) in denied {
            assert_eq!(
                bpf_action(program, nr as u32, native),
                SECCOMP_RET_TRAP,
                "{class}: {name}"
            );
        }
        assert_eq!(
            bpf_action(program, libc::SYS_getpid as u32, native),
            SECCOMP_RET_ALLOW,
            "{class}: getpid"
        );
        // A syscall of another architecture (on x86_64, `int 0x80` reaches it) is never
        // allowed. What it gets is libseccomp's bad-architecture action, which the fork's
        // seccompiler does not set: with libseccomp 2.5.5 the calling thread is killed and
        // the process is not, as under Firecracker's own filters. Printed, so a run says
        // which this host's library compiled.
        let foreign_action = bpf_action(program, libc::SYS_getpid as u32, foreign);
        println!("{class}: a foreign-architecture syscall gets action {foreign_action:#x}");
        assert!(
            [
                SECCOMP_RET_KILL_THREAD,
                SECCOMP_RET_KILL_PROCESS,
                SECCOMP_RET_TRAP
            ]
            .contains(&foreign_action),
            "{class}: a foreign-architecture syscall gets action {foreign_action:#x}"
        );
    }

    let vcpu = &programs["vcpu"];
    assert_eq!(
        bpf_action(vcpu, libc::SYS_socket as u32, native),
        SECCOMP_RET_TRAP
    );
    let allowed = status_under_filter(vcpu, libc::SYS_getpid, [0, 0, 0]);
    assert!(
        libc::WIFEXITED(allowed) && libc::WEXITSTATUS(allowed) == 7,
        "getpid under the vcpu filter: wait status {allowed:#x}"
    );
    let trapped = status_under_filter(
        vcpu,
        libc::SYS_socket,
        [libc::AF_UNIX as _, libc::SOCK_STREAM as _, 0],
    );
    assert!(
        libc::WIFSIGNALED(trapped) && libc::WTERMSIG(trapped) == libc::SIGSYS,
        "socket under the vcpu filter: wait status {trapped:#x}"
    );
    Ok(())
}
