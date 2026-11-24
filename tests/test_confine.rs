//! `--confine` end to end: a confined rootless VM boots, restores from its pre-start
//! snapshot, runs a command, and its Firecracker holds no capabilities, while a host below
//! the Landlock floor refuses the start naming the ABI it has.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Every thread of `pid`: its name, then CapEff, CapPrm, CapInh, CapBnd, CapAmb and
/// NoNewPrivs, from its status.
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
        ] {
            values.push(field(name)?);
        }
        threads.push((field("Name")?, values));
    }
    Ok(threads)
}

/// The firecracker processes whose parent is `parent`. The binary is content-addressed
/// (`firecracker-default-<sha>.bin`), so its comm is a prefix of that name.
fn firecracker_children(parent: u32) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // comm is in parentheses and may hold spaces; the fields after it are plain.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let comm = &stat[stat.find('(').map_or(0, |o| o + 1)..close];
        let ppid = stat[close + 2..].split_whitespace().nth(1);
        if comm.starts_with("firecracker") && ppid == Some(parent.to_string().as_str()) {
            found.push(pid);
        }
    }
    found
}

#[tokio::test]
async fn test_confine_rootless_vm() -> Result<()> {
    let floor = fcvm::confine::LANDLOCK_ABI_FLOOR;
    let abi = fcvm::confine::landlock_abi();
    // Logged so a CI job says which branch ran and why.
    println!("landlock ABI on this host: {abi:?}; confinement needs {floor}");
    let (vm_name, _, _, _) = common::unique_names("confine");

    match abi {
        Ok(abi) if abi >= floor => {}
        _ => {
            // Below the floor the start fails closed, before anything is created.
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                tokio::process::Command::new(common::find_fcvm_binary()?)
                    .args([
                        "podman",
                        "run",
                        "--name",
                        &vm_name,
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
            let expected = match abi {
                Ok(abi) => format!("landlock: the kernel's Landlock ABI is {abi};"),
                Err(_) => "landlock: Landlock is unavailable".to_string(),
            };
            assert!(
                stderr.contains(&expected) && stderr.contains("nothing was started"),
                "expected {expected:?} in: {stderr}"
            );
            println!("refused below the floor, as required: {expected}");
            return Ok(());
        }
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

        let firecrackers = firecracker_children(fcvm_pid);
        anyhow::ensure!(
            firecrackers.len() == 1,
            "firecracker children of {fcvm_pid}: {firecrackers:?}"
        );
        let threads = thread_credentials(firecrackers[0])?;
        println!("firecracker threads: {threads:?}");
        let zero = "0000000000000000".to_string();
        let expected = vec![
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero,
            "1".to_string(),
        ];
        for (name, values) in &threads {
            anyhow::ensure!(
                values == &expected,
                "thread {name}: CapEff, CapPrm, CapInh, CapBnd, CapAmb, NoNewPrivs = {values:?}"
            );
        }
        let limits = std::fs::read_to_string(format!("/proc/{}/limits", firecrackers[0]))?;
        let core = limits
            .lines()
            .find(|l| l.starts_with("Max core file size"))
            .context("no core limit in /proc/<pid>/limits")?;
        anyhow::ensure!(
            core.split_whitespace().skip(4).take(2).eq(["1", "1"]),
            "core limit: {core}"
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
            vm["confinement"]["layers"] == serde_json::json!(["process"]),
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
