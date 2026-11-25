//! Pages mode in the fleet agent (`--renders`): runs pages mode's host module
//! (`crate::fleet::renders`, the one `fcvm fleet renders-bench` runs) while the coordinator's
//! order says run, and sends the host's summary (fleet/PROTOCOL.md, "Pages mode").
//!
//! One module runs at a time, as a future of this task (never spawned on its own): an order
//! to run starts one, and the module itself follows the order from then on (an overlay change
//! is applied by it, an order to stop ends it, and so does the end of the connection, when
//! `serve` drops the order's sender). Each module starts its own reaper before it makes
//! anything, so an agent killed outright leaves nothing of it behind.
//!
//! Nothing is sent before the coordinator's first order: a coordinator built before the mode
//! sends none, and closes the connection on a message it cannot parse. After it, a summary
//! goes about every [`REPORT_EVERY`]: the module's own while its loop runs (every fifth tick
//! of its one-second loop), and otherwise the agent's, with nothing in flight (stopped,
//! starting, sweeping after its loop, or `withheld` with the error of a module that failed). A
//! module that failed is started again after [`RETRY_AFTER`] while the order says run, or at
//! the next order; the agent and its connection go on.

use crate::cli::FleetRendersBenchArgs;
use crate::fleet::renders::{Bounds, Signals};
use fleet_core::protocol::AgentMsg;
use fleet_core::renders::{RendersHost, RendersOrder, RendersReport};
use fleet_core::renders_search::SearchParams;
use fleet_core::renders_wall::Wall;
use futures_util::future::BoxFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// The module under the agent registers no signal handler: one stays registered for the life
/// of the process, and the agent's SIGTERM must end the agent (the reaper then removes what the
/// module made).
pub const AGENT_SIGNALS: Signals = Signals::LeaveToTheProcess;

/// What a module keeps of its records under the agent, which runs for as long as its host:
/// `renders.jsonl` rotated at 64 MiB and `samples.jsonl` at 16 MiB, each with its previous
/// file, and the newest 50 failed clones' logs.
pub const AGENT_BOUNDS: Bounds = Bounds {
    renders_bytes: 64 << 20,
    samples_bytes: 16 << 20,
    failed_clone_logs: 50,
};

/// The time between two of the agent's own summaries.
pub const REPORT_EVERY: Duration = Duration::from_secs(5);
/// How long after a module that failed the next one starts, while the order says run.
pub const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Starts one module: it renders while the order says run and sends its reports, and returns
/// once the order says stop or the order's sender is gone (`Ok`), or on an error.
pub type StartModule = Arc<
    dyn Fn(
            watch::Receiver<Option<RendersOrder>>,
            mpsc::UnboundedSender<RendersReport>,
        ) -> BoxFuture<'static, anyhow::Result<()>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct RendersCfg {
    /// What the hello says of the mode.
    pub host: RendersHost,
    /// The trailing window of the summaries.
    pub window_ms: u64,
    pub start: StartModule,
    pub report_every: Duration,
    pub retry_after: Duration,
}

impl RendersCfg {
    /// The module run with `args` (renders-bench's options) and the fcvm binary `exe`, with no
    /// signal handler: the agent's SIGTERM must end the agent, and the reaper then removes
    /// what the module made.
    pub fn module(args: FleetRendersBenchArgs, exe: PathBuf, host: RendersHost) -> Self {
        let window_ms = u64::from(args.window_s) * 1000;
        let args = Arc::new(args);
        let start: StartModule = Arc::new(move |orders, reports| {
            let (args, exe) = (args.clone(), exe.clone());
            Box::pin(async move {
                let (outcome, reaped) = crate::fleet::renders::session(
                    &args,
                    &exe,
                    orders,
                    Some(reports),
                    AGENT_SIGNALS,
                    Some(AGENT_BOUNDS),
                )
                .await?;
                outcome?;
                anyhow::ensure!(
                    reaped.clean(),
                    "the module left something behind: {reaped:?}"
                );
                Ok(())
            })
        });
        RendersCfg {
            host,
            window_ms,
            start,
            report_every: REPORT_EVERY,
            retry_after: RETRY_AFTER,
        }
    }

    /// The summary while no module renders: every page waiting, nothing in flight, and why
    /// when a module failed.
    fn idle_report(&self, withheld: Option<&str>) -> RendersReport {
        let now = crate::fleet::now_ms();
        let mut wall = Wall::new(
            self.host.pages.clone(),
            SearchParams::default(),
            self.window_ms,
            now,
        );
        wall.withhold(withheld.map(str::to_string));
        wall.report(now, None, None)
    }
}

/// The mode's task. Returns once the order's sender is gone and the module it ran has ended.
pub async fn run(
    cfg: RendersCfg,
    mut orders: watch::Receiver<Option<RendersOrder>>,
    out: mpsc::UnboundedSender<AgentMsg>,
) {
    loop {
        if orders.borrow_and_update().is_some() {
            break;
        }
        if orders.changed().await.is_err() {
            return;
        }
    }
    let send = |report: RendersReport| {
        let _ = out.send(AgentMsg::Renders(report));
    };
    let mut withheld: Option<String> = None;
    let mut retry_at: Option<Instant> = None;
    let mut next_report = Instant::now();
    loop {
        let run = orders.borrow_and_update().as_ref().is_some_and(|o| o.run);
        if !run {
            // Stopped is not withheld: a failed module's error goes with the order to stop.
            withheld = None;
            retry_at = None;
        }
        if run && retry_at.is_none_or(|t| Instant::now() >= t) {
            // An order followed at once by the end of the connection starts nothing.
            if orders.has_changed().is_err() {
                return;
            }
            let (tx, mut rx) = mpsc::unbounded_channel();
            let module = (cfg.start)(orders.clone(), tx);
            tokio::pin!(module);
            next_report = Instant::now() + cfg.report_every;
            // The module reports from its first report until its loop ends, when it drops its
            // sender. Before (it is starting) and after (its reaper sweeps) the agent reports.
            let mut reporting = false;
            let mut open = true;
            let ended = loop {
                tokio::select! {
                    ended = &mut module => break ended,
                    report = rx.recv(), if open => match report {
                        Some(report) => {
                            send(report);
                            reporting = true;
                            next_report = Instant::now() + cfg.report_every;
                        }
                        None => {
                            open = false;
                            reporting = false;
                        }
                    },
                    _ = tokio::time::sleep_until(next_report), if !reporting => {
                        send(cfg.idle_report(None));
                        next_report = Instant::now() + cfg.report_every;
                    }
                }
            };
            while let Ok(report) = rx.try_recv() {
                send(report);
            }
            match ended {
                Ok(()) => {
                    withheld = None;
                    retry_at = None;
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::error!(target: "fcvm::fleet::renders", error = %why, retry_in = ?cfg.retry_after, "pages mode: the module ended on an error");
                    withheld = Some(why);
                    retry_at = Some(Instant::now() + cfg.retry_after);
                }
            }
            // The stopped or failed state at once, not at the next tick.
            next_report = Instant::now();
            if orders.has_changed().is_err() {
                return;
            }
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(next_report) => {
                send(cfg.idle_report(withheld.as_deref()));
                next_report = Instant::now() + cfg.report_every;
            }
            changed = orders.changed() => {
                if changed.is_err() {
                    return;
                }
                // Any new order ends a failed module's wait: a stop and a run that arrive
                // together read as one change.
                retry_at = None;
            }
            _ = tokio::time::sleep_until(retry_at.unwrap_or(next_report)), if retry_at.is_some() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::renders::{OrderOverlay, RendersPage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn host() -> RendersHost {
        RendersHost {
            pages: vec![RendersPage {
                id: "a.test".into(),
                url: "https://a.test/".into(),
            }],
            cpus: 40,
            confined: true,
            build: Default::default(),
        }
    }

    fn order(run: bool, seq: Option<u64>) -> Option<RendersOrder> {
        Some(RendersOrder {
            run,
            overlay: seq.map(|seq| OrderOverlay {
                text: Some("hi".into()),
                transform: None,
                chromium_only: false,
                seq,
            }),
        })
    }

    /// A module as the agent sees one: it sends one report (`at` is 1000 plus its start's
    /// number), records every order it is given, and ends as the module does, at an order to
    /// stop or when the sender is gone. The first `fail` starts fail at once.
    struct Fake {
        starts: Arc<AtomicUsize>,
        running: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<Option<RendersOrder>>>>,
    }

    fn fake(fail: usize) -> (Fake, RendersCfg) {
        let f = Fake {
            starts: Default::default(),
            running: Default::default(),
            seen: Default::default(),
        };
        let (starts, running, seen) = (f.starts.clone(), f.running.clone(), f.seen.clone());
        let start: StartModule = Arc::new(move |mut orders, reports| {
            let n = starts.fetch_add(1, Ordering::SeqCst);
            let (running, seen) = (running.clone(), seen.clone());
            Box::pin(async move {
                if n < fail {
                    anyhow::bail!("kernel test-1 lacks the bridge teardown fix");
                }
                running.fetch_add(1, Ordering::SeqCst);
                let _ = reports.send(RendersReport {
                    at: 1000 + n as u64,
                    ..Default::default()
                });
                loop {
                    let order = orders.borrow_and_update().clone();
                    seen.lock().unwrap().push(order.clone());
                    if !order.is_some_and(|o| o.run) {
                        break;
                    }
                    if orders.changed().await.is_err() {
                        break;
                    }
                }
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
        });
        let cfg = RendersCfg {
            host: host(),
            window_ms: 30_000,
            start,
            report_every: REPORT_EVERY,
            retry_after: RETRY_AFTER,
        };
        (f, cfg)
    }

    /// The next report that `want` accepts, skipping the ones before it.
    async fn until(
        rx: &mut mpsc::UnboundedReceiver<AgentMsg>,
        want: impl Fn(&RendersReport) -> bool,
    ) -> RendersReport {
        loop {
            let r = next(rx).await;
            if want(&r) {
                return r;
            }
        }
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<AgentMsg>) -> RendersReport {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(AgentMsg::Renders(r))) => r,
            other => panic!("no report: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_reported_before_the_first_order() {
        let (f, cfg) = fake(0);
        let (tx, rx) = watch::channel(None);
        let (out, mut msgs) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, rx, out));
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(msgs.try_recv().is_err(), "a report before any order");
        assert_eq!(f.starts.load(Ordering::SeqCst), 0);
        // An order to stop is an order: the host says it is stopped, and starts nothing.
        tx.send_replace(order(false, None));
        let r = next(&mut msgs).await;
        assert_eq!(r.withheld, None);
        assert_eq!(r.tiles.len(), 2);
        assert!(r.tiles.iter().all(|t| t.state == "waiting"));
        assert_eq!(f.starts.load(Ordering::SeqCst), 0);
        tx.send_replace(order(true, None));
        until(&mut msgs, |r| r.at == 1000).await;
        assert_eq!(f.starts.load(Ordering::SeqCst), 1);
        drop(tx);
        task.await.unwrap();
    }

    /// The module follows the order itself: an overlay change reaches it without a new
    /// start, an order to stop ends it, and the host then reports every few seconds that
    /// nothing is in flight. An order to run starts the next module.
    #[tokio::test(start_paused = true)]
    async fn an_order_to_stop_ends_the_module_and_one_to_run_starts_the_next() {
        let (f, cfg) = fake(0);
        let (tx, rx) = watch::channel(order(true, None));
        let (out, mut msgs) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, rx, out));
        assert_eq!(next(&mut msgs).await.at, 1000);
        tx.send_replace(order(true, Some(4)));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(f.starts.load(Ordering::SeqCst), 1);
        assert_eq!(f.running.load(Ordering::SeqCst), 1);
        assert!(f.seen.lock().unwrap().contains(&order(true, Some(4))));

        tx.send_replace(order(false, Some(4)));
        let stopped = next(&mut msgs).await;
        assert_eq!(f.running.load(Ordering::SeqCst), 0);
        assert_eq!(stopped.withheld, None);
        assert!(stopped.sides.values().all(|s| s.search.in_flight == 0));
        let t0 = Instant::now();
        next(&mut msgs).await;
        assert_eq!(Instant::now() - t0, REPORT_EVERY);

        tx.send_replace(order(true, Some(4)));
        until(&mut msgs, |r| r.at == 1001).await;
        assert_eq!(f.starts.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished());
        drop(tx);
        task.await.unwrap();
        assert_eq!(f.running.load(Ordering::SeqCst), 0);
    }

    /// A module that fails leaves the agent up: the host reports why it renders nothing
    /// (`withheld`) every few seconds, and starts the module again after the retry time, or
    /// at once after an order to stop and one to run.
    #[tokio::test(start_paused = true)]
    async fn a_module_that_fails_is_reported_withheld_and_started_again() {
        let (f, cfg) = fake(2);
        let (tx, rx) = watch::channel(order(true, None));
        let (out, mut msgs) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, rx, out));
        let failed = next(&mut msgs).await;
        let why = failed.withheld.expect("the error is reported");
        assert!(why.contains("bridge teardown fix"), "{why}");
        let t0 = Instant::now();
        let again = next(&mut msgs).await;
        assert_eq!(Instant::now() - t0, REPORT_EVERY);
        assert!(again.withheld.is_some());
        assert_eq!(f.starts.load(Ordering::SeqCst), 1);
        assert!(!task.is_finished());
        // the retry time: the second start fails too
        while f.starts.load(Ordering::SeqCst) < 2 {
            next(&mut msgs).await;
        }
        assert!(Instant::now() - t0 >= RETRY_AFTER - REPORT_EVERY);
        // stop and run: no wait for the retry time
        tx.send_replace(order(false, None));
        until(&mut msgs, |r| r.withheld.is_none()).await;
        let t1 = Instant::now();
        tx.send_replace(order(true, None));
        until(&mut msgs, |r| r.at == 1002).await;
        assert!(Instant::now() - t1 < REPORT_EVERY);
        assert_eq!(f.running.load(Ordering::SeqCst), 1);
        drop(tx);
        task.await.unwrap();
        assert_eq!(f.running.load(Ordering::SeqCst), 0);
    }

    /// A module that has ended its renders sends nothing while its reaper sweeps (it drops its
    /// report sender with its loop): the agent reports at the cadence meanwhile.
    #[tokio::test(start_paused = true)]
    async fn a_module_ending_its_sweep_is_reported_at_the_cadence() {
        let sweeping = Arc::new(AtomicUsize::new(0));
        let s = sweeping.clone();
        let start: StartModule = Arc::new(move |mut orders, reports| {
            let s = s.clone();
            Box::pin(async move {
                let _ = reports.send(RendersReport {
                    at: 1000,
                    ..Default::default()
                });
                while orders.borrow_and_update().as_ref().is_some_and(|o| o.run) {
                    if orders.changed().await.is_err() {
                        break;
                    }
                }
                drop(reports);
                s.store(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(12)).await;
                s.store(2, Ordering::SeqCst);
                Ok(())
            })
        });
        let cfg = RendersCfg { start, ..fake(0).1 };
        let (tx, rx) = watch::channel(order(true, None));
        let (out, mut msgs) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, rx, out));
        until(&mut msgs, |r| r.at == 1000).await;
        tx.send_replace(order(false, None));
        let t0 = Instant::now();
        next(&mut msgs).await;
        assert_eq!(Instant::now() - t0, REPORT_EVERY);
        assert_eq!(sweeping.load(Ordering::SeqCst), 1, "still sweeping");
        next(&mut msgs).await;
        assert_eq!(Instant::now() - t0, 2 * REPORT_EVERY);
        drop(tx);
        task.await.unwrap();
    }

    /// An order followed at once by the end of the connection starts no module.
    #[tokio::test(start_paused = true)]
    async fn an_order_on_a_connection_already_ended_starts_nothing() {
        let (f, cfg) = fake(0);
        let (tx, rx) = watch::channel(order(true, None));
        drop(tx);
        let (out, _msgs) = mpsc::unbounded_channel();
        run(cfg, rx, out).await;
        assert_eq!(f.starts.load(Ordering::SeqCst), 0);
    }

    /// A stop and a run that arrive together while a failed module waits for its retry still
    /// start the module at once: any new order ends the wait.
    #[tokio::test(start_paused = true)]
    async fn a_new_order_ends_the_wait_after_a_failure() {
        let (_f, cfg) = fake(1);
        let (tx, rx) = watch::channel(order(true, None));
        let (out, mut msgs) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, rx, out));
        assert!(next(&mut msgs).await.withheld.is_some());
        let t0 = Instant::now();
        tx.send_replace(order(false, None));
        tx.send_replace(order(true, None));
        until(&mut msgs, |r| r.at == 1001).await;
        assert!(Instant::now() - t0 < REPORT_EVERY);
        drop(tx);
        task.await.unwrap();
    }

    /// A module that is still starting has no report of its own: the host says it renders
    /// nothing yet, at the same cadence.
    #[tokio::test(start_paused = true)]
    async fn a_module_still_starting_is_reported_at_the_cadence() {
        let started = Arc::new(AtomicUsize::new(0));
        let s = started.clone();
        let start: StartModule = Arc::new(move |mut orders, _reports| {
            s.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                while orders.changed().await.is_ok() {}
                Ok(())
            })
        });
        let cfg = RendersCfg { start, ..fake(0).1 };
        let (tx, rx) = watch::channel(order(true, None));
        let (out, mut msgs) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, rx, out));
        let first = next(&mut msgs).await;
        assert!(first.sides.values().all(|s| s.search.in_flight == 0));
        let t0 = Instant::now();
        next(&mut msgs).await;
        assert_eq!(Instant::now() - t0, REPORT_EVERY);
        assert_eq!(started.load(Ordering::SeqCst), 1);
        drop(tx);
        task.await.unwrap();
    }

    /// The messages `bench/chromium/renders_agent_check.py` reads and sends, in the file its
    /// own test drives a stand-in agent with: the hello and a report are this build's
    /// serialisation, and the orders are what this build parses.
    #[test]
    fn the_host_checks_recorded_messages_are_this_builds() {
        use fleet_core::protocol::CoordMsg;
        use fleet_core::renders_wall::Completed;
        let args = crate::fleet::renders::agent_args(
            &[
                "--out=o",
                "--run-dir=r",
                "--bench=b",
                "--corpus=c",
                "--urls=https://example.com/,https://news.example.org/front",
                "--snapshot-tag=t",
            ]
            .map(String::from),
        )
        .unwrap();
        let all: Vec<u32> = (0..96).collect();
        let host = crate::fleet::renders::hello(
            &args,
            &crate::fleet::renders::HostFacts {
                unit_cpus: Some(&all),
                kernel: "7.3.3-fcvm-test",
                instance_type: None,
                snapshot: None,
                bundle_sha256: Some("feed".into()),
            },
        )
        .unwrap();
        let hello = AgentMsg::Hello {
            host: "renders-agent-check".into(),
            arch: "aarch64".into(),
            gpus: vec![],
            capacity: 0,
            store_ready_at: None,
            boot: Default::default(),
            phones: vec![],
            phone_actions: vec![],
            potato_players: 0,
            potato_cpus: 0,
            potato_confined: false,
            windows: 0,
            windows_actions: vec![],
            minecraft: 0,
            minecraft_server: None,
            renders: Some(host.clone()),
        };
        let mut wall = Wall::new(host.pages.clone(), SearchParams::default(), 30_000, 1_000);
        wall.order(
            &RendersOrder {
                run: true,
                overlay: None,
            },
            1_000,
        );
        wall.serving(Some(1));
        for side in 0..2 {
            wall.render(&Completed {
                side,
                page: 0,
                at: 2_000,
                ms: 300.0,
                level: 1,
                error: None,
            });
        }
        let report = AgentMsg::Renders(wall.report(3_000, None, None));
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/bench/chromium/renders_agent_protocol.jsonl"
        );
        let recorded = std::fs::read_to_string(path).unwrap();
        let lines: Vec<serde_json::Value> = recorded
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let ours = [
            serde_json::to_value(&hello).unwrap(),
            serde_json::to_value(&report).unwrap(),
        ];
        assert_eq!(
            lines[..2],
            ours,
            "{path} is not this build's; its first two lines are:\n{}\n{}",
            ours[0],
            ours[1]
        );
        assert!(lines.len() > 2, "the orders follow");
        for order in &lines[2..] {
            let parsed: CoordMsg = serde_json::from_value(order.clone()).unwrap();
            assert!(matches!(parsed, CoordMsg::Renders { .. }), "{order}");
        }
    }

    /// The agent's module leaves SIGTERM and SIGINT to the process (the session's source
    /// test pins that the agent passes this constant).
    #[test]
    fn the_agents_module_registers_no_signal_handler() {
        assert_eq!(AGENT_SIGNALS, Signals::LeaveToTheProcess);
    }

    /// The agent's module keeps bounded records (the sizes the docs state).
    #[test]
    fn the_agents_module_keeps_bounded_records() {
        assert_eq!(
            AGENT_BOUNDS,
            Bounds {
                renders_bytes: 64 << 20,
                samples_bytes: 16 << 20,
                failed_clone_logs: 50,
            }
        );
    }

    /// `--renders` turns the mode on and takes renders-bench's options through
    /// `--renders-arg`, which the module then parses as renders-bench does; neither goes
    /// without the other, and an agent started without them has no mode.
    #[test]
    fn the_agent_takes_the_mode_and_its_options_together() {
        use clap::Parser;
        let parse = |extra: &[&str]| {
            let base = [
                "fcvm",
                "fleet",
                "agent",
                "--stdio",
                "--image=x",
                "--base-port=1",
            ];
            let cli = crate::cli::Cli::try_parse_from(base.iter().chain(extra))?;
            let crate::cli::Commands::Fleet(fleet) = cli.cmd else {
                panic!("not the fleet command");
            };
            let crate::cli::FleetCommand::Agent(args) = fleet.cmd else {
                panic!("not the agent");
            };
            Ok::<_, clap::Error>(args)
        };
        let args = parse(&[]).unwrap();
        assert!(!args.renders && args.renders_args.is_empty());
        let args = parse(&[
            "--renders",
            "--renders-arg=--out=o",
            "--renders-arg=--run-dir=r",
            "--renders-arg",
            "--bench=b",
            "--renders-arg=--corpus=c",
            "--renders-arg=--urls=https://a.test/",
            "--renders-arg=--snapshot-tag=t",
            "--renders-arg=--max-engines=36",
        ])
        .unwrap();
        assert!(args.renders);
        let module = crate::fleet::renders::agent_args(&args.renders_args).unwrap();
        assert_eq!(module.max_engines, 36);
        assert_eq!(module.bench, PathBuf::from("b"));
        assert!(parse(&["--renders"]).is_err());
        assert!(parse(&["--renders-arg=--out=o"]).is_err());
    }

    /// The agent over one connection: the hello says the host runs the mode, the order
    /// reaches the module, the module's report reaches the coordinator, and the end of the
    /// connection ends the module before `serve` returns.
    #[tokio::test]
    async fn the_agent_says_it_runs_the_mode_and_passes_orders_and_reports() {
        use crate::fleet::lines::{spawn_writer, LineReader};
        use fleet_core::protocol::CoordMsg;
        let (f, cfg) = fake(0);
        let agent = super::super::Agent {
            host: "renders-host".into(),
            gpus: vec![],
            phones: vec![],
            capacity: 0,
            base_port: 20000,
            cfg: super::super::slot::AgentCfg {
                fcvm: "/nonexistent/fcvm".into(),
                image: "unused".into(),
                extra_args: vec![],
                publish_ip: "127.0.0.1".into(),
                frame_host: "127.0.0.1".into(),
                ipv6_prefix: None,
                frame_zone: None,
                tls_publish_base: None,
                public_ipv4: None,
                place: None,
                sysfs_root: "/nonexistent-sysfs".into(),
                start_timeout: Duration::from_secs(1),
                stop_grace: Duration::from_secs(1),
                gpu_free_timeout: Duration::from_secs(1),
                health_interval: Duration::from_secs(1),
                backoff_min: Duration::from_secs(1),
                backoff_max: Duration::from_secs(1),
                stable_after: Duration::from_secs(1),
                scrub: false,
                scrub_timeout: Duration::from_secs(1),
                quarantine_after: 3,
                phone: None,
                scene_token: Default::default(),
            },
            spot_notice_url: None,
            spot_poll: Duration::from_secs(5),
            aggregate: None,
            store_ready_file: None,
            boot: Default::default(),
            potato: None,
            windows: None,
            minecraft: None,
            renders: Some(cfg),
            ipv4_front: None,
        };
        let (coord, agent_side) = tokio::io::duplex(1 << 16);
        let (agent_in, agent_out) = tokio::io::split(agent_side);
        let (coord_in, coord_out) = tokio::io::split(coord);
        let serving = tokio::spawn(super::super::serve(agent, agent_in, agent_out));
        let (to_agent, writer) = spawn_writer::<CoordMsg, _>(coord_out, "agent");
        let mut from_agent = LineReader::new(coord_in, "agent");
        async fn recv<R: tokio::io::AsyncRead + Unpin>(from: &mut LineReader<R>) -> AgentMsg {
            tokio::time::timeout(Duration::from_secs(10), from.next::<AgentMsg>())
                .await
                .expect("a message within 10 s")
                .unwrap()
                .expect("the connection is open")
        }
        let AgentMsg::Hello { renders, .. } = recv(&mut from_agent).await else {
            panic!("the hello first");
        };
        assert_eq!(renders, Some(host()));
        to_agent
            .send(CoordMsg::Renders {
                order: order(true, Some(2)).unwrap(),
            })
            .unwrap();
        loop {
            if let AgentMsg::Renders(r) = recv(&mut from_agent).await {
                assert_eq!(r.at, 1000);
                break;
            }
        }
        assert!(f.seen.lock().unwrap().contains(&order(true, Some(2))));
        assert_eq!(f.running.load(Ordering::SeqCst), 1);
        drop(to_agent);
        let _ = writer.await;
        tokio::time::timeout(Duration::from_secs(10), serving)
            .await
            .expect("serve returns once its input closes")
            .unwrap()
            .unwrap();
        assert_eq!(
            f.running.load(Ordering::SeqCst),
            0,
            "the module ended first"
        );
    }
}
