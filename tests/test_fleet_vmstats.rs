//! The fleet agent's VM CPU sampler (`fcvm::fleet::agent::vmstats`) against one real rootless
//! VM: it must find the VM's Firecracker from the fcvm process (fcvm runs a content-addressed
//! `firecracker-<profile>-<sha>.bin`, whose command name is not `firecracker`), count the
//! VM's vCPUs from its threads, and read CPU use that follows what the guest does.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use fcvm::fleet::agent::vmstats::Sampler;
use std::time::Duration;

const VCPUS: u32 = 2;

#[tokio::test]
async fn test_vmstats_follow_a_real_vms_cpu() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("vmstats");
    let (_child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--cpu",
        &VCPUS.to_string(),
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run")?;
    let result = sample_idle_then_busy(fcvm_pid).await;
    common::kill_process(fcvm_pid).await;
    result
}

async fn sample_idle_then_busy(fcvm_pid: u32) -> Result<()> {
    common::poll_health_by_pid(fcvm_pid, 300).await?;
    // Let the guest's boot work settle before the idle reading.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut sampler = Sampler::default();
    assert!(
        sampler.poll(fcvm_pid).is_none(),
        "the first read has no interval to report"
    );
    let mut idle = Vec::new();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let s = sampler
            .poll(fcvm_pid)
            .context("no reading from a running VM: the sampler did not find its Firecracker")?;
        assert_eq!(s.vcpus, VCPUS, "vcpus follows the VM's --cpu");
        assert!(s.cpu_pct >= 0.0 && s.cpu_pct <= 100.0, "{s:?}");
        idle.push(s.cpu_pct);
    }
    let idle_max = idle.iter().cloned().fold(0.0, f64::max);
    assert!(idle_max < 25.0, "an idle VM read {idle:?}");

    // One busy loop per vCPU, long enough to outlast a slow exec start. Poll until a window
    // shows the load, with a deadline, instead of sampling at fixed offsets.
    let busy = tokio::spawn(async move {
        common::exec_in_vm(
            fcvm_pid,
            &["for i in 1 2; do timeout 90 sh -c 'while :; do :; done' & done; wait"],
        )
        .await
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(75);
    let mut loaded = Vec::new();
    let loaded_max = loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(s) = sampler.poll(fcvm_pid) {
            assert_eq!(s.vcpus, VCPUS);
            assert!(s.cpu_pct <= 100.0, "{s:?}");
            loaded.push(s.cpu_pct);
            if s.cpu_pct > 50.0 && s.cpu_pct > idle_max + 25.0 {
                break s.cpu_pct;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{VCPUS} busy loops never showed: windows {loaded:?} against idle {idle:?} (exec finished: {})",
            busy.is_finished()
        );
    };
    println!("vmstats idle {idle:?} loaded {loaded:?} (peak {loaded_max})");
    busy.abort();
    Ok(())
}
