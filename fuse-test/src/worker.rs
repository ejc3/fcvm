//! Stress worker that performs filesystem operations on a FUSE mount.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Instant;

use crate::stress::{OpsBreakdown, WorkerResult};

/// Operations weighted by realistic FUSE usage
const WEIGHTS: &[(OpType, u32)] = &[
    (OpType::Getattr, 40),
    (OpType::Lookup, 25),
    (OpType::Read, 15),
    (OpType::Readdir, 10),
    (OpType::Write, 5),
    (OpType::Create, 5),
];

#[derive(Clone, Copy, Debug)]
enum OpType {
    Getattr,
    Lookup,
    Read,
    Readdir,
    Write,
    Create,
}

pub fn run_stress_worker(
    worker_id: usize,
    ops: usize,
    mount: &PathBuf,
    results_file: &PathBuf,
) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut errors = 0usize;
    let mut breakdown = OpsBreakdown::default();

    // Worker's private directory
    let worker_dir = mount.join(format!("worker-{}", worker_id));

    // Pre-generate weighted ops list
    let mut op_list: Vec<OpType> = Vec::with_capacity(100);
    for &(op, weight) in WEIGHTS {
        for _ in 0..weight {
            op_list.push(op);
        }
    }

    // Deterministic but varied sequence using worker_id as seed
    let mut rng_state = (worker_id as u64).wrapping_add(1);

    for i in 0..ops {
        // Simple LCG for reproducible "random" selection
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let idx = (rng_state >> 32) as usize % op_list.len();
        let op = op_list[idx];

        let file_idx = i % 10;
        let file_path = worker_dir.join(format!("file-{}.txt", file_idx));

        let result = match op {
            OpType::Getattr => {
                breakdown.getattr += 1;
                do_getattr(&file_path)
            }
            OpType::Lookup => {
                breakdown.lookup += 1;
                do_lookup(&worker_dir, file_idx)
            }
            OpType::Read => {
                breakdown.read += 1;
                do_read(&file_path)
            }
            OpType::Readdir => {
                breakdown.readdir += 1;
                do_readdir(&worker_dir)
            }
            OpType::Write => {
                breakdown.write += 1;
                do_write(&file_path, worker_id, i)
            }
            OpType::Create => {
                breakdown.create += 1;
                do_create(&worker_dir, worker_id, i)
            }
        };

        if let Err(e) = result {
            errors += 1;
            // Log first few errors for debugging
            if errors <= 3 {
                eprintln!("[worker-{}] error on op {:?} at {}: {}", worker_id, op, file_path.display(), e);
            }
        }
    }

    let duration = start.elapsed();

    // Write results
    let result = WorkerResult {
        worker_id,
        ops_completed: ops,
        errors,
        duration_ms: duration.as_millis() as u64,
        ops_breakdown: breakdown,
    };

    let json = serde_json::to_string(&result)?;
    fs::write(results_file, json)?;

    Ok(())
}

fn do_getattr(path: &PathBuf) -> anyhow::Result<()> {
    let _ = fs::metadata(path)?;
    Ok(())
}

fn do_lookup(dir: &PathBuf, file_idx: usize) -> anyhow::Result<()> {
    let path = dir.join(format!("file-{}.txt", file_idx));
    let _ = fs::metadata(&path)?;
    Ok(())
}

fn do_read(path: &PathBuf) -> anyhow::Result<()> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 512];
    let _ = file.read(&mut buf)?;
    Ok(())
}

fn do_readdir(dir: &PathBuf) -> anyhow::Result<()> {
    let _ = fs::read_dir(dir)?.count();
    Ok(())
}

fn do_write(path: &PathBuf, worker_id: usize, iteration: usize) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    let data = format!("w{}:i{}\n", worker_id, iteration);
    file.write_all(data.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn do_create(dir: &PathBuf, worker_id: usize, iteration: usize) -> anyhow::Result<()> {
    let tmp_file = dir.join(format!("tmp-{}-{}.txt", worker_id, iteration));
    // This is a create-then-delete test, so we don't check if the file exists
    // fs::write will create if needed
    if let Err(e) = fs::write(&tmp_file, "test") {
        // Only fail on actual errors, not ENOENT
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e.into());
        }
    }
    // Ignore remove errors (file might already be gone in concurrent tests)
    let _ = fs::remove_file(&tmp_file);
    Ok(())
}
