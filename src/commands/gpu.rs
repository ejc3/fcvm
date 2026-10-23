//! `fcvm gpu`: the host side of a GPU pool. `scrub` resets GPUs and waits for
//! their memory clear before they go to another VM (src/gpu/scrub.rs); `status`
//! prints what has been recorded about each GPU. Both print one JSON line per
//! GPU for the fleet agent to count.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::cli::{GpuArgs, GpuCommands};
use crate::gpu::scrub::{self, GpuRecord};

pub async fn cmd_gpu(args: GpuArgs) -> Result<()> {
    let state_dir = scrub::state_dir();
    match args.cmd {
        GpuCommands::Scrub(args) => {
            // The reset, the config space and BAR0 need root, and only root may
            // write the records a quarantine lives in.
            anyhow::ensure!(
                nix::unistd::geteuid().is_root(),
                "fcvm gpu scrub must run as root: it resets the GPU and records the outcome in {}",
                state_dir.display()
            );
            let sbdfs = crate::gpu::canonical_sbdfs(&args.gpus)?;
            let timeout = Duration::from_secs(args.timeout_secs);
            let reports = tokio::task::spawn_blocking(move || {
                std::thread::scope(|scope| {
                    let workers: Vec<_> = sbdfs
                        .iter()
                        .map(|sbdf| {
                            let state_dir = &state_dir;
                            scope.spawn(move || {
                                scrub::scrub_and_record(
                                    Path::new("/sys"),
                                    Path::new("/dev"),
                                    state_dir,
                                    sbdf,
                                    timeout,
                                )
                            })
                        })
                        .collect();
                    workers
                        .into_iter()
                        .map(|w| w.join().expect("a scrub thread panicked"))
                        .collect::<Vec<_>>()
                })
            })
            .await?;
            let mut all_scrubbed = true;
            for report in &reports {
                all_scrubbed &= report.outcome == "scrubbed";
                println!("{}", serde_json::to_string(report)?);
            }
            if !all_scrubbed {
                std::process::exit(1);
            }
            Ok(())
        }
        GpuCommands::Status(args) => {
            let records = if args.gpus.is_empty() {
                scrub::read_records(&state_dir)?
            } else {
                let mut records = Vec::new();
                for sbdf in crate::gpu::canonical_sbdfs(&args.gpus)? {
                    records.push(scrub::read_record(&state_dir, &sbdf)?.unwrap_or(GpuRecord {
                        sbdf,
                        ..Default::default()
                    }));
                }
                records
            };
            for record in records {
                println!("{}", serde_json::to_string(&record)?);
            }
            Ok(())
        }
    }
}
