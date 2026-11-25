//! Pages mode: the wall's state on a renders host, as a pure function of what happened.
//!
//! The host's module (and `fcvm fleet renders-bench`, which is that module without a
//! coordinator or viewers) feeds this every completed render, each side's cgroup CPU counter
//! and CPU pressure, and the coordinator's orders, and reads back two things: how many
//! renders each side runs in flight, and the report. Every figure the page shows for the
//! mode comes from here, so the live wall and the benchmark's reference row are computed by
//! one piece of code. Nothing here reads a clock, a file or a socket.

use crate::renders::{
    RendersOrder, RendersPage, RendersReport, SideReport, TileReport, SIDE_FCVM, SIDE_KITESURF,
};
use crate::renders_search::{Pair, Render, SearchParams};
use std::collections::{BTreeMap, VecDeque};

/// The sides in the order the wall indexes them.
pub const SIDES: [&str; 2] = [SIDE_FCVM, SIDE_KITESURF];

/// Failures in a row after which a tile says it stopped.
pub const STOPPED_AFTER: u32 = 5;

#[derive(Debug, Clone, PartialEq)]
struct Tile {
    done: u64,
    failed: u64,
    failed_in_a_row: u32,
    last_ms: Option<f64>,
    last_at: Option<u64>,
    last_error: Option<String>,
    /// The host does not render this page on this side, and why.
    unsupported: Option<String>,
}

/// One side's counts and its trailing window.
#[derive(Debug, Clone, PartialEq)]
struct Meter {
    window_ms: u64,
    since: u64,
    done: u64,
    failed: u64,
    /// Completed renders in the window: when, which page, how long.
    recent: VecDeque<(u64, usize, f64)>,
    /// The side cgroup's cumulative CPU time in the window: when, usec.
    cpu: VecDeque<(u64, u64)>,
    tiles: Vec<Tile>,
    engines: Option<u32>,
    recycles: Option<u64>,
    setup: BTreeMap<String, String>,
}

impl Meter {
    fn new(pages: usize, window_ms: u64, now: u64) -> Self {
        Meter {
            window_ms,
            since: now,
            done: 0,
            failed: 0,
            recent: VecDeque::new(),
            cpu: VecDeque::new(),
            tiles: vec![
                Tile {
                    done: 0,
                    failed: 0,
                    failed_in_a_row: 0,
                    last_ms: None,
                    last_at: None,
                    last_error: None,
                    unsupported: None,
                };
                pages
            ],
            engines: None,
            recycles: None,
            setup: BTreeMap::new(),
        }
    }

    fn trim(&mut self, now: u64) {
        let from = now.saturating_sub(self.window_ms);
        while self.recent.front().is_some_and(|r| r.0 < from) {
            self.recent.pop_front();
        }
        // Keep one sample at or before the window's start, to measure across the whole of it.
        while self.cpu.len() > 1 && self.cpu[1].0 <= from {
            self.cpu.pop_front();
        }
    }

    fn render(&mut self, at: u64, page: usize, ms: f64, error: Option<&str>) {
        let Some(tile) = self.tiles.get_mut(page) else {
            return;
        };
        match error {
            None => {
                self.done += 1;
                tile.done += 1;
                tile.failed_in_a_row = 0;
                tile.last_ms = Some(ms);
                tile.last_at = Some(at);
                tile.last_error = None;
                self.recent.push_back((at, page, ms));
            }
            Some(e) => {
                self.failed += 1;
                tile.failed += 1;
                tile.failed_in_a_row += 1;
                tile.last_error = Some(e.to_string());
            }
        }
        self.trim(at);
    }

    fn report(&self, now: u64, search: crate::renders::Search) -> SideReport {
        let from = now.saturating_sub(self.window_ms);
        let mut times: Vec<f64> = self
            .recent
            .iter()
            .filter(|r| r.0 >= from)
            .map(|r| r.2)
            .collect();
        let n = times.len();
        times.sort_by(|a, b| a.total_cmp(b));
        let render_ms = (n > 0).then(|| {
            if n % 2 == 1 {
                times[n / 2]
            } else {
                (times[n / 2 - 1] + times[n / 2]) / 2.0
            }
        });
        // The window a rate is over is no longer than the side has been counting.
        let window = self.window_ms.min(now.saturating_sub(self.since)).max(1);
        let (cpus_busy, cpu_ms_per_render) = match (self.cpu.front(), self.cpu.back()) {
            (Some(a), Some(b)) if b.0 > a.0 && b.1 >= a.1 => {
                let usec = (b.1 - a.1) as f64;
                let renders = self
                    .recent
                    .iter()
                    .filter(|r| r.0 > a.0 && r.0 <= b.0)
                    .count();
                (
                    usec / 1000.0 / (b.0 - a.0) as f64,
                    (renders > 0).then(|| usec / 1000.0 / renders as f64),
                )
            }
            _ => (0.0, None),
        };
        SideReport {
            search,
            renders_per_s: n as f64 * 1000.0 / window as f64,
            render_ms,
            cpus_busy,
            cpu_ms_per_render,
            done: self.done,
            failed: self.failed,
            since: self.since,
            engines: self.engines,
            recycles: self.recycles,
            setup: self.setup.clone(),
        }
    }

    fn tiles(&self, now: u64, side: &str, pages: &[RendersPage]) -> Vec<TileReport> {
        let from = now.saturating_sub(self.window_ms);
        let window = self.window_ms.min(now.saturating_sub(self.since)).max(1);
        self.tiles
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let n = self
                    .recent
                    .iter()
                    .filter(|r| r.1 == i && r.0 >= from)
                    .count();
                let (state, reason) = if let Some(why) = &t.unsupported {
                    ("unsupported", Some(why.clone()))
                } else if t.failed_in_a_row >= STOPPED_AFTER {
                    ("stopped", t.last_error.clone())
                } else if t.done == 0 {
                    ("waiting", None)
                } else {
                    ("rendering", None)
                };
                TileReport {
                    page: pages[i].id.clone(),
                    side: side.to_string(),
                    state: state.to_string(),
                    reason,
                    renders_per_s: n as f64 * 1000.0 / window as f64,
                    last_ms: t.last_ms,
                    last_at: t.last_at,
                    done: t.done,
                    failed: t.failed,
                }
            })
            .collect()
    }
}

/// A completed render, as the host's drivers hand it over.
#[derive(Debug, Clone, PartialEq)]
pub struct Completed {
    /// 0 for `fcvm`, 1 for `kitesurf` ([`SIDES`]).
    pub side: usize,
    pub page: usize,
    /// When it completed (Unix ms) and how long it took.
    pub at: u64,
    pub ms: f64,
    /// Renders in flight on its side when it started.
    pub level: u32,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Wall {
    pages: Vec<RendersPage>,
    params: SearchParams,
    pair: Pair,
    meters: [Meter; 2],
    run: bool,
    /// The overlay order the host was last given, and the one its replays serve.
    ordered_seq: Option<u64>,
    serving_seq: Option<u64>,
    withheld: Option<String>,
}

impl Wall {
    pub fn new(pages: Vec<RendersPage>, params: SearchParams, window_ms: u64, now: u64) -> Self {
        let n = pages.len();
        Wall {
            pair: Pair::new(params.clone(), n),
            meters: [Meter::new(n, window_ms, now), Meter::new(n, window_ms, now)],
            pages,
            params,
            run: false,
            ordered_seq: None,
            serving_seq: None,
            withheld: None,
        }
    }

    /// The most renders in flight `side` may run on this host.
    pub fn cap(&mut self, side: usize, most: u32) {
        if let Some(s) = self.pair.sides.get_mut(side) {
            s.set_max_in_flight(most);
        }
    }

    /// The coordinator's order. A new overlay changes the work of every render, so both
    /// sides' searches start again and the windows are emptied; the counts go on.
    pub fn order(&mut self, order: &RendersOrder, now: u64) {
        let seq = order.overlay.as_ref().map(|o| o.seq);
        if seq != self.ordered_seq {
            self.ordered_seq = seq;
            self.changed(now);
        }
        if order.run != self.run {
            self.run = order.run;
            self.changed(now);
        }
    }

    fn changed(&mut self, now: u64) {
        self.pair.restart();
        for m in &mut self.meters {
            m.recent.clear();
            m.cpu.clear();
            m.since = now;
        }
    }

    /// The replays serve this overlay now (none: the pages as captured).
    pub fn serving(&mut self, seq: Option<u64>) {
        self.serving_seq = seq;
    }

    /// The host renders nothing, and why (no snapshot yet, a kernel without the bridge
    /// teardown fix, a seal that does not match); `None` clears it.
    pub fn withhold(&mut self, why: Option<String>) {
        self.withheld = why;
    }

    /// The host does not render `page` on `side`, and why.
    pub fn unsupported(&mut self, side: usize, page: usize, why: String) {
        if let Some(t) = self.meters[side].tiles.get_mut(page) {
            t.unsupported = Some(why);
        }
    }

    pub fn render(&mut self, r: &Completed) {
        if r.side > 1 {
            return;
        }
        self.pair.sides[r.side].render(Render {
            page: r.page,
            ms: r.ms,
            ok: r.error.is_none(),
            level: r.level,
        });
        self.meters[r.side].render(r.at, r.page, r.ms, r.error.as_deref());
    }

    /// A side's cgroup CPU counter (`cpu.stat` `usage_usec`) at `at`.
    pub fn cpu(&mut self, side: usize, at: u64, usage_usec: u64) {
        let m = &mut self.meters[side];
        m.cpu.push_back((at, usage_usec));
        m.trim(at);
    }

    /// The `kitesurf` side's engine processes and recycles so far, and a side's `Setup`
    /// values.
    pub fn engines(&mut self, running: u32, recycles: u64) {
        self.meters[1].engines = Some(running);
        self.meters[1].recycles = Some(recycles);
    }

    pub fn setup(&mut self, side: usize, values: BTreeMap<String, String>) {
        self.meters[side].setup = values;
    }

    /// One decision, about once a second: the renders in flight each side runs from now.
    /// Nothing while the host is ordered to stop or renders are withheld.
    pub fn step(&mut self, now: u64, pressure_usec: [Option<u64>; 2]) -> [u32; 2] {
        if !self.run || self.withheld.is_some() {
            return [0, 0];
        }
        self.pair.step(now, pressure_usec)
    }

    /// The pages still rotate while a side waits for its turn; this is which ones a side
    /// renders (all but the unsupported).
    pub fn renders_page(&self, side: usize, page: usize) -> bool {
        self.meters[side]
            .tiles
            .get(page)
            .is_some_and(|t| t.unsupported.is_none())
    }

    pub fn params(&self) -> &SearchParams {
        &self.params
    }

    /// The summary the host sends the coordinator and its frame server sends viewers.
    pub fn report(
        &self,
        now: u64,
        frame_url: Option<String>,
        frame_url_v4: Option<String>,
    ) -> RendersReport {
        let mut sides = BTreeMap::new();
        let mut tiles = Vec::new();
        for (i, name) in SIDES.iter().enumerate() {
            let mut search = self.pair.sides[i].view();
            if !self.run || self.withheld.is_some() {
                search.in_flight = 0;
            }
            sides.insert(name.to_string(), self.meters[i].report(now, search));
            tiles.extend(self.meters[i].tiles(now, name, &self.pages));
        }
        RendersReport {
            at: now,
            frame_url,
            frame_url_v4,
            sides,
            tiles,
            overlay_seq: self.serving_seq,
            harness: None,
            withheld: self.withheld.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renders::{OrderOverlay, SearchPhase};

    fn pages() -> Vec<RendersPage> {
        ["example", "hn"]
            .iter()
            .map(|id| RendersPage {
                id: id.to_string(),
                url: format!("https://{id}.test/"),
            })
            .collect()
    }

    fn running(now: u64) -> Wall {
        let mut w = Wall::new(pages(), SearchParams::default(), 20_000, now);
        w.order(
            &RendersOrder {
                run: true,
                overlay: None,
            },
            now,
        );
        w
    }

    fn ok(side: usize, page: usize, at: u64, ms: f64, level: u32) -> Completed {
        Completed {
            side,
            page,
            at,
            ms,
            level,
            error: None,
        }
    }

    #[test]
    fn a_wall_that_was_not_ordered_to_run_runs_nothing() {
        let mut w = Wall::new(pages(), SearchParams::default(), 20_000, 0);
        assert_eq!(w.step(1_000, [None, None]), [0, 0]);
        let r = w.report(1_000, None, None);
        assert_eq!(r.sides[SIDE_FCVM].search.in_flight, 0);
        assert_eq!(r.tiles.len(), 4);
        assert!(r.tiles.iter().all(|t| t.state == "waiting"));
        w.order(
            &RendersOrder {
                run: true,
                overlay: None,
            },
            2_000,
        );
        assert_eq!(w.step(2_000, [None, None]), [1, 1]);
        w.withhold(Some("no snapshot yet".into()));
        assert_eq!(w.step(3_000, [None, None]), [0, 0]);
        assert_eq!(
            w.report(3_000, None, None).withheld.as_deref(),
            Some("no snapshot yet")
        );
    }

    #[test]
    fn rates_times_and_cpu_come_from_completed_renders_in_the_window() {
        let mut w = running(0);
        w.step(0, [Some(0), Some(0)]);
        w.cpu(0, 0, 0);
        // 40 renders in 20 s on the fcvm side: 30 of page 0 at 200 ms, 10 of page 1 at 1,000
        for i in 0..40u64 {
            let (page, ms) = if i % 4 == 3 { (1, 1000.0) } else { (0, 200.0) };
            w.render(&ok(0, page, 500 * (i + 1), ms, 1));
        }
        // 26 CPU-seconds over those 20 s
        w.cpu(0, 20_000, 26_000_000);
        let r = w.report(20_000, Some("wss://h/frames".into()), None);
        let s = &r.sides[SIDE_FCVM];
        assert_eq!(s.renders_per_s, 2.0);
        assert_eq!(s.render_ms, Some(200.0));
        assert!((s.cpus_busy - 1.3).abs() < 1e-9);
        assert_eq!(s.cpu_ms_per_render, Some(650.0));
        assert_eq!((s.done, s.failed), (40, 0));
        let t: Vec<_> = r.tiles.iter().filter(|t| t.side == SIDE_FCVM).collect();
        assert_eq!((t[0].renders_per_s, t[0].last_ms), (1.5, Some(200.0)));
        assert_eq!((t[1].renders_per_s, t[1].done), (0.5, 10));
        assert_eq!(t[1].state, "rendering");
        // the other side rendered nothing: no time, no rate, tiles waiting
        let k = &r.sides[SIDE_KITESURF];
        assert_eq!(
            (k.renders_per_s, k.render_ms, k.cpu_ms_per_render),
            (0.0, None, None)
        );
        assert_eq!(r.frame_url.as_deref(), Some("wss://h/frames"));
        // 30 s later, with nothing more rendered, the window is empty and the counts stand
        let later = w.report(50_000, None, None);
        assert_eq!(later.sides[SIDE_FCVM].renders_per_s, 0.0);
        assert_eq!(later.sides[SIDE_FCVM].done, 40);
        assert_eq!(later.tiles[0].last_at, Some(19_500));
    }

    #[test]
    fn a_tile_that_keeps_failing_says_it_stopped_and_why_and_recovers() {
        let mut w = running(0);
        w.step(0, [None, None]);
        for i in 0..STOPPED_AFTER as u64 {
            w.render(&Completed {
                side: 1,
                page: 1,
                at: 1_000 + i,
                ms: 5.0,
                level: 1,
                error: Some("navigate: connection reset".into()),
            });
        }
        let r = w.report(2_000, None, None);
        let t = r
            .tiles
            .iter()
            .find(|t| t.side == SIDE_KITESURF && t.page == "hn")
            .unwrap();
        assert_eq!(
            (t.state.as_str(), t.reason.as_deref(), t.failed),
            ("stopped", Some("navigate: connection reset"), 5)
        );
        assert_eq!(r.sides[SIDE_KITESURF].failed, 5);
        w.render(&ok(1, 1, 3_000, 300.0, 1));
        let r = w.report(3_000, None, None);
        assert_eq!(r.tiles[3].state, "rendering");
        w.unsupported(1, 0, "Kitesurf doesn't support WebGL".into());
        assert!(!w.renders_page(1, 0));
        assert!(w.renders_page(0, 0));
        assert_eq!(w.report(3_000, None, None).tiles[2].state, "unsupported");
    }

    #[test]
    fn a_new_overlay_starts_both_searches_again_and_keeps_the_counts() {
        let mut w = running(0);
        w.step(0, [Some(0), Some(0)]);
        let mut now = 0;
        // both sides climb for a while
        for _ in 0..10 {
            for (side, name) in SIDES.iter().enumerate() {
                let level = w.report(now, None, None).sides[*name].search.in_flight;
                for page in 0..2 {
                    for _ in 0..3 {
                        w.render(&ok(side, page, now, 100.0, level));
                    }
                }
            }
            now += 1_000;
            w.step(now, [Some(0), Some(0)]);
        }
        let before = w.report(now, None, None);
        assert!(before.sides[SIDE_FCVM].search.in_flight > 5);
        assert_eq!(before.overlay_seq, None);
        let order = RendersOrder {
            run: true,
            overlay: Some(OrderOverlay {
                text: Some("hello".into()),
                transform: None,
                chromium_only: false,
                seq: 1,
            }),
        };
        w.order(&order, now);
        let after = w.report(now, None, None);
        for side in SIDES {
            let s = &after.sides[side];
            assert_eq!(
                (s.search.in_flight, s.search.phase),
                (1, SearchPhase::Starting)
            );
            assert!(s.search.curve.is_empty());
            assert_eq!(s.renders_per_s, 0.0);
            assert_eq!(s.done, before.sides[side].done);
        }
        // the report says which overlay the replays serve, once they do
        assert_eq!(after.overlay_seq, None);
        w.serving(Some(1));
        assert_eq!(w.report(now, None, None).overlay_seq, Some(1));
        // the same order again changes nothing
        w.step(now + 1_000, [Some(0), Some(0)]);
        let held = w.clone();
        w.order(&order, now + 1_000);
        assert_eq!(w, held);
    }
}
