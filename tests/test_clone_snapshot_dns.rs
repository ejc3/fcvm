//! A snapshot taken from a restored clone keeps the resolver its source baked.
//!
//! #1540: a rootless golden made with `--dns 10.0.2.2` records that resolver, and
//! its clones' pasta forwards the guest's DNS to the host loopback service the
//! golden was recorded against. A snapshot taken from one of those clones recorded
//! no resolver, because the clone took its network config from pasta's setup,
//! which reports none. Clones of that snapshot then ran pasta with its DNS special
//! case on, and a guest whose resolv.conf names the gateway resolved through the
//! host's own resolver: the corpus replay served live sites, which used 2.9 times
//! the memory and timed out on one page.
//!
//! The test names the host's own resolver with `--dns` rather than 10.0.2.2: the
//! defect is any resolver a rootless clone drops, and the guest has to pull its
//! image through the one it is given.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use fcvm::network::{nameservers_from_sources, ResolvSource, RESOLV_CONF_SOURCES};

fn recorded_resolver(tag: &str) -> Result<serde_json::Value> {
    let path = fcvm::paths::snapshot_dir().join(tag).join("config.json");
    let config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    Ok(config["metadata"]["network_config"]["dns_server"].clone())
}

#[tokio::test]
async fn test_a_snapshot_of_a_restored_clone_keeps_the_resolver_its_source_baked() -> Result<()> {
    let (vm, clone, snapshot, _) = common::unique_names("clone-snap-dns");
    let second = format!("{snapshot}-of-clone");
    let sources = RESOLV_CONF_SOURCES.map(ResolvSource::read);
    let resolver = nameservers_from_sources(&sources)
        .context("the guest needs a resolver it can reach to pull its image")?
        .remove(0);
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm,
        "--network",
        "rootless",
        "--dns",
        &resolver,
        common::TEST_IMAGE,
    ])
    .await?;
    common::poll_health_by_pid(pid, 180).await?;
    common::create_snapshot_by_pid(pid, &snapshot).await?;
    assert_eq!(
        recorded_resolver(&snapshot)?,
        resolver.as_str(),
        "the golden's snapshot"
    );

    let (mut serve, serve_pid) = common::start_memory_server(&snapshot).await?;
    let (mut c, c_pid) = common::spawn_clone(serve_pid, &clone).await?;
    common::poll_health_by_pid(c_pid, 120).await?;
    common::create_snapshot_by_pid(c_pid, &second).await?;
    let result = recorded_resolver(&second);

    for (child, pid) in [(&mut c, c_pid), (&mut serve, serve_pid), (&mut child, pid)] {
        common::kill_process(pid).await;
        let _ = child.wait().await;
    }
    assert_eq!(
        result?,
        resolver.as_str(),
        "the snapshot taken from the restored clone"
    );
    Ok(())
}
