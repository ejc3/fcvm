use anyhow::{Context, Result};
use fs2::FileExt;
use std::io::{Read, Seek, SeekFrom, Write};
use tokio::time::{sleep, Duration};

/// Watch for lock test trigger file and run lock tests when it appears.
pub async fn watch_for_lock_test(clone_id: String) {
    let trigger_path = "/mnt/shared/run-lock-test";
    let counter_path = "/mnt/shared/counter.txt";
    let append_path = "/mnt/shared/append.log";

    eprintln!(
        "[fc-agent] watching for lock test trigger at {}",
        trigger_path
    );

    loop {
        sleep(Duration::from_millis(500)).await;

        if std::path::Path::new(trigger_path).exists() {
            let iterations: usize = match std::fs::read_to_string(trigger_path) {
                Ok(content) => content.trim().parse().unwrap_or(100),
                Err(_) => continue,
            };

            eprintln!(
                "[fc-agent] lock test triggered! clone={} iterations={}",
                clone_id, iterations
            );

            run_lock_tests(&clone_id, iterations, counter_path, append_path);

            let done_path = format!("/mnt/shared/done-{}", clone_id);
            if let Err(e) = std::fs::write(&done_path, "done") {
                eprintln!("[fc-agent] ERROR writing done file: {}", e);
            } else {
                eprintln!("[fc-agent] lock test complete, wrote {}", done_path);
            }

            break;
        }
    }
}

fn run_lock_tests(clone_id: &str, iterations: usize, counter_path: &str, append_path: &str) {
    eprintln!("[fc-agent] running {} lock iterations", iterations);

    for i in 0..iterations {
        if let Err(e) = increment_counter_with_lock(counter_path) {
            eprintln!("[fc-agent] ERROR incrementing counter (iter {}): {}", i, e);
        }
        if let Err(e) = append_with_lock(append_path, clone_id, i) {
            eprintln!("[fc-agent] ERROR appending to log (iter {}): {}", i, e);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    eprintln!("[fc-agent] completed {} lock iterations", iterations);
}

fn increment_counter_with_lock(path: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("opening counter file")?;

    file.lock_exclusive()
        .context("acquiring exclusive lock on counter")?;

    let mut content = String::new();
    file.read_to_string(&mut content)
        .context("reading counter")?;
    let current: i64 = content.trim().parse().unwrap_or(0);
    let new_value = current + 1;

    file.seek(SeekFrom::Start(0)).context("seeking to start")?;
    file.set_len(0).context("truncating file")?;
    write!(file, "{}", new_value).context("writing new counter value")?;
    file.sync_all().context("syncing counter file")?;

    Ok(())
}

fn append_with_lock(path: &str, clone_id: &str, iteration: usize) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("opening append file")?;

    file.lock_exclusive()
        .context("acquiring exclusive lock on append file")?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let line = format!("{}:{}:{}\n", clone_id, iteration, timestamp);

    let mut writer = std::io::BufWriter::new(&file);
    writer
        .write_all(line.as_bytes())
        .context("writing append line")?;
    writer.flush().context("flushing append file")?;

    Ok(())
}
