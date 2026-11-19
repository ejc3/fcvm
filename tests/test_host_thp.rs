//! `podman run --host-thp`: a cold-booted VM's memory is on host transparent huge pages,
//! and a clone of its snapshot restores with 4 KiB pages, as every restore does.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{bail, Context, Result};

/// Guest memory for the test VM (MiB). `--host-thp` needs a whole number of 2 MiB pages.
const MEM_MIB: &str = "1024";

/// The host's transparent huge page mode, the bracketed word of
/// /sys/kernel/mm/transparent_hugepage/enabled.
fn host_thp_mode() -> Result<String> {
    let text = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .context("reading the host's transparent huge page mode")?;
    text.split_whitespace()
        .find_map(|w| w.strip_prefix('[').and_then(|w| w.strip_suffix(']')))
        .map(str::to_string)
        .with_context(|| format!("no selected mode in {text:?}"))
}

/// The Firecracker process under an fcvm process.
fn firecracker_under(fcvm_pid: u32) -> Result<u32> {
    let mut parents = vec![fcvm_pid];
    while let Some(parent) = parents.pop() {
        for entry in std::fs::read_dir("/proc")? {
            let Ok(pid) = entry?.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // The name sits in parentheses and may hold spaces; ppid is the
            // second field after it.
            let Some((name, rest)) = stat
                .split_once(" (")
                .and_then(|(_, rest)| rest.rsplit_once(") "))
            else {
                continue;
            };
            if rest.split_whitespace().nth(1) != Some(&parent.to_string()) {
                continue;
            }
            if name.starts_with("firecracker") {
                return Ok(pid);
            }
            parents.push(pid);
        }
    }
    bail!("no firecracker process under fcvm {fcvm_pid}")
}

/// AnonHugePages of a process, in KiB.
fn anon_huge_kib(pid: u32) -> Result<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .with_context(|| format!("reading /proc/{pid}/smaps_rollup"))?;
    text.lines()
        .find_map(|l| l.strip_prefix("AnonHugePages:"))
        .and_then(|v| v.split_whitespace().next())
        .context("no AnonHugePages line")?
        .parse()
        .context("parsing AnonHugePages")
}

/// Whether a Firecracker's guest memory is advised for transparent huge pages: the `hg`
/// flag (MADV_HUGEPAGE) on its largest anonymous mapping, which is the guest's memory.
fn guest_memory_advised(vmm_pid: u32) -> Result<bool> {
    let text = std::fs::read_to_string(format!("/proc/{vmm_pid}/smaps"))
        .with_context(|| format!("reading /proc/{vmm_pid}/smaps"))?;
    // (size in KiB, VmFlags) of each anonymous mapping. A mapping's header line is the only
    // line whose first field is an address range.
    let mut mappings: Vec<(u64, String)> = Vec::new();
    let mut current: Option<(bool, u64)> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields
            .first()
            .is_some_and(|f| f.contains('-') && !f.ends_with(':'))
        {
            // address perms offset dev inode [path]: anonymous when inode is 0 and no path.
            current = Some((fields.get(4) == Some(&"0") && fields.len() == 5, 0));
        } else if let Some(size) = line.strip_prefix("Size:") {
            if let Some((_, kib)) = current.as_mut() {
                *kib = size
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse()
                    .unwrap_or(0);
            }
        } else if let Some(flags) = line.strip_prefix("VmFlags:") {
            if let Some((true, kib)) = current.take() {
                mappings.push((kib, flags.trim().to_string()));
            }
        }
    }
    let (kib, flags) = mappings
        .into_iter()
        .max_by_key(|(kib, _)| *kib)
        .context("no anonymous mapping")?;
    anyhow::ensure!(
        kib >= 512 * 1024,
        "the largest anonymous mapping of Firecracker {vmm_pid} is {kib} KiB, too small to be \
         the guest's memory"
    );
    Ok(flags.split_whitespace().any(|f| f == "hg"))
}

/// A cold boot with `--host-thp` advises the guest's memory with MADV_HUGEPAGE, and a
/// clone of its snapshot does not: Firecracker records `Transparent` in the snapshot and a
/// load that reused it would advise the clone's memory too.
#[tokio::test]
async fn test_host_thp_cold_boot_uses_huge_pages_and_its_clone_does_not() -> Result<()> {
    let mode = host_thp_mode()?;
    anyhow::ensure!(
        mode == "madvise" || mode == "always",
        "this test needs a host transparent huge page mode of madvise or always, found {mode}"
    );
    let (base_name, clone_name, snap_name, serve_name) = common::unique_names("host-thp");
    let fcvm_path = common::find_fcvm_binary()?;

    let (mut base, base_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &base_name,
        "--host-thp",
        "--no-snapshot",
        "--mem",
        MEM_MIB,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning the --host-thp VM")?;
    let mut serve: Option<(tokio::process::Child, u32)> = None;
    let mut clone: Option<(tokio::process::Child, u32)> = None;

    let result = async {
        common::poll_health_by_pid(base_pid, 300).await?;
        let vmm = firecracker_under(base_pid)?;
        anyhow::ensure!(
            guest_memory_advised(vmm)?,
            "the --host-thp VM's guest memory has no MADV_HUGEPAGE (hg) flag"
        );
        let before = anon_huge_kib(vmm)?;
        // 256 MiB of first writes into guest memory.
        common::exec_in_vm(
            base_pid,
            &["dd if=/dev/zero of=/dev/shm/thp bs=1M count=256 status=none && sync"],
        )
        .await?;
        let after = anon_huge_kib(vmm)?;
        // Reported, not asserted: on a fragmented host the kernel falls back to 4 KiB pages.
        println!("  firecracker AnonHugePages: {before} KiB before the writes, {after} KiB after");

        let out = tokio::process::Command::new(&fcvm_path)
            .args([
                "snapshot",
                "create",
                "--pid",
                &base_pid.to_string(),
                "--tag",
                &snap_name,
            ])
            .output()
            .await
            .context("running snapshot create")?;
        anyhow::ensure!(
            out.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let (serve_child, serve_pid, _serve_log) =
            common::spawn_fcvm_with_log_path(&["snapshot", "serve", &snap_name], &serve_name)
                .await
                .context("starting the memory server")?;
        serve = Some((serve_child, serve_pid));
        common::poll_serve_ready(&snap_name, serve_pid, 60).await?;
        let (clone_child, clone_pid) = common::spawn_fcvm(&[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone_name,
        ])
        .await
        .context("spawning the clone")?;
        clone = Some((clone_child, clone_pid));
        common::poll_health_by_pid(clone_pid, 120).await?;
        let echoed = common::exec_in_vm(clone_pid, &["echo host-thp-clone-ok"]).await?;
        anyhow::ensure!(
            echoed.contains("host-thp-clone-ok"),
            "the clone did not answer an exec: {echoed:?}"
        );

        let clone_vmm = firecracker_under(clone_pid)?;
        anyhow::ensure!(
            !guest_memory_advised(clone_vmm)?,
            "the clone's guest memory carries MADV_HUGEPAGE (hg): its load reused the \
             snapshot's Transparent setting"
        );
        Ok(())
    }
    .await;

    for (child, pid) in [clone.take(), serve.take()].into_iter().flatten() {
        common::kill_process(pid).await;
        let mut child = child;
        let _ = child.wait().await;
    }
    common::kill_process(base_pid).await;
    let _ = base.wait().await;
    let _ = tokio::process::Command::new(&fcvm_path)
        .args(["snapshots", "delete", "--force", &snap_name])
        .output()
        .await;
    result
}
