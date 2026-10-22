//! Running an aggregator as a child process: the agent runs one for its host's band, the
//! coordinator one for the EC2 half. The child dies with its parent (PR_SET_PDEATHSIG); if
//! it exits it is restarted after 1 s, doubling to 30 s, and given the layout again.

use super::Layout;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Keep `argv` (an `fcvm fleet aggregate ...` command; `--listen <listen>` is added)
/// running and feed it every layout from `layouts`. `on_ready` runs each time a started
/// aggregator answers `/healthz`.
pub async fn supervise(
    argv: Vec<String>,
    listen: String,
    mut layouts: watch::Receiver<Option<Layout>>,
    on_ready: impl Fn() + Send + 'static,
) {
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let base = format!("http://{}", connect_addr(&listen));
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .args(["--listen", &listen])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        arm_pdeathsig(cmd.as_std_mut());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(cmd = ?argv, error = %e, "cannot start the aggregator");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        // Wait for it to listen.
        let mut ready = false;
        for _ in 0..200 {
            if http
                .get(format!("{base}/healthz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                ready = true;
                break;
            }
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if ready {
            on_ready();
            layouts.mark_changed();
            loop {
                tokio::select! {
                    status = child.wait() => {
                        tracing::warn!(?status, "aggregator exited");
                        break;
                    }
                    changed = layouts.changed() => {
                        if changed.is_err() {
                            let _ = child.kill().await;
                            return;
                        }
                        let layout = layouts.borrow_and_update().clone();
                        if let Some(l) = layout {
                            if let Err(e) = http.post(format!("{base}/layout")).json(&l).send().await {
                                tracing::warn!(error = %e, "layout not posted to the aggregator");
                            }
                        }
                    }
                }
            }
        } else {
            let _ = child.kill().await;
        }
        if started.elapsed() >= Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Where to reach a listen address from this host: an unspecified address means loopback.
pub fn connect_addr(listen: &str) -> String {
    if let Some(port) = listen.strip_prefix("0.0.0.0:") {
        format!("127.0.0.1:{port}")
    } else if let Some(port) = listen.strip_prefix("[::]:") {
        format!("[::1]:{port}")
    } else {
        listen.to_string()
    }
}

fn arm_pdeathsig(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: two async-signal-safe calls in the forked child; no allocation.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}
