//! Pages mode: a side's search for the number of renders in flight its CPUs carry.
//!
//! A pure function of what it is fed: completed renders (which page, how long, whether it
//! succeeded, and the level it started at), the side cgroup's cumulative CPU pressure, and
//! the time. It reads no clock and no file, so the same inputs give the same decisions, a
//! recorded run can be replayed in a test, and the live wall and the benchmark run one
//! rule. Both sides use it with the same [`SearchParams`].
//!
//! The rule:
//!
//! - **Baseline.** At one in flight, each page's median render time.
//! - **Step.** One more render in flight. Pages are taken in rotation by the caller, so
//!   every page has its turn at every level.
//! - **Dwell.** A level is judged once every page has `min_renders` renders that started at
//!   it (the slowest page sets the time), or after `dwell_max_ms`.
//! - **Spike.** For each page, its median at the level over its own baseline; the level's
//!   figure is the median of those over the pages. The level spiked when that is above
//!   `ratio`, or when the share of the dwell in which some task of the side waited for a CPU
//!   is above `pressure`.
//! - **Back-off.** To the level below, which held: it becomes the limit.
//! - **Hold.** After a back-off a side holds its level for `hold_ms` (five minutes), then
//!   tries one more render in flight: one probe per hold. A probe that spikes goes back and
//!   holds again; one that does not becomes the limit and is held. At the end of each hold
//!   the held level is judged over the whole hold first, and steps down if it no longer
//!   holds (a level that held alone may not once the other side is at its own limit).
//! - **Failure.** A failed render at the level backs off at once, to half.
//!
//! The other side's load makes this side's renders slower without sharing a core with it, so
//! a baseline taken while the other side was idle reads its load as this side's
//! oversubscription. [`Pair`] has each side take its baseline again, one at a time, once
//! both have found a limit.

use crate::renders::{BackOff, CurvePoint, Search, SearchPhase, SpikeCause};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchParams {
    /// Render time at a level over the time at one in flight, above which the level spiked.
    pub ratio: f64,
    /// The share of a dwell with some task of the side waiting for a CPU, above which the
    /// level spiked.
    pub pressure: f64,
    /// Renders every page completes at a level before it is judged.
    pub min_renders: u32,
    /// The longest a level waits for them.
    pub dwell_max_ms: u64,
    /// How long a side holds its level after a back-off, or after a probe that held, before
    /// it tries one more render in flight.
    pub hold_ms: u64,
    /// The most renders in flight the side may run (its engines, its memory).
    pub max_in_flight: u32,
}

impl Default for SearchParams {
    fn default() -> Self {
        SearchParams {
            ratio: 1.25,
            pressure: 0.15,
            min_renders: 3,
            dwell_max_ms: 90_000,
            hold_ms: 300_000,
            max_in_flight: 64,
        }
    }
}

/// One completed render, as the search needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Render {
    /// The page's index in the wall's order.
    pub page: usize,
    pub ms: f64,
    pub ok: bool,
    /// The level the side ran (the renders in flight it was asked to keep) when this one
    /// started. A side kept below its level by something else (an engine being started
    /// again) still tags its renders with the level.
    pub level: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Baseline,
    Adding,
    Holding,
}

/// What a level measured over its dwell.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Measured {
    ratio: Option<f64>,
    pressure: Option<f64>,
    renders_per_s: f64,
    render_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LimitSearch {
    params: SearchParams,
    pages: usize,
    level: u32,
    phase: Phase,
    baseline: Vec<Option<f64>>,
    /// Render times of the renders that started at the current level, by page.
    at_level: Vec<Vec<f64>>,
    failed_at_level: u32,
    /// Renders completed since the current level began, whatever level they started at:
    /// the level's rate.
    completed: u32,
    /// When the current level began, and the pressure total then; none before the first step.
    since_ms: Option<u64>,
    pressure_at: Option<u64>,
    hold_until_ms: u64,
    limit: Option<u32>,
    backed_off: Option<BackOff>,
    curve: BTreeMap<u32, (f64, f64)>,
    /// The level to go back to once a baseline taken again is done.
    return_to: Option<u32>,
}

/// A render time for a set of renders of several pages: the mean over the pages of each
/// page's median, which is the time to render one of each page divided by the pages. Pages
/// with no render are left out; none with no render at all.
///
/// Not the median over all the renders. The pages differ by an order of magnitude, the
/// median over all of them falls between two groups of pages, and one render more of a
/// fast or of a slow page moves it from one group to the other.
pub fn page_mean_ms<'a>(by_page: impl IntoIterator<Item = &'a [f64]>) -> Option<f64> {
    let medians: Vec<f64> = by_page.into_iter().filter_map(median).collect();
    (!medians.is_empty()).then(|| medians.iter().sum::<f64>() / medians.len() as f64)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

impl LimitSearch {
    pub fn new(params: SearchParams, pages: usize) -> Self {
        LimitSearch {
            params,
            pages,
            level: 1,
            phase: Phase::Baseline,
            baseline: vec![None; pages],
            at_level: vec![Vec::new(); pages],
            failed_at_level: 0,
            completed: 0,
            since_ms: None,
            pressure_at: None,
            hold_until_ms: 0,
            limit: None,
            backed_off: None,
            curve: BTreeMap::new(),
            return_to: None,
        }
    }

    /// The most renders in flight this side may run (its workers, its engines, its memory).
    /// The sides share every other parameter; this one is each side's own, and a restart
    /// keeps it. A level above the new most comes down at the next decision.
    pub fn set_max_in_flight(&mut self, most: u32) {
        self.params.max_in_flight = most.max(1);
        if self.level > self.params.max_in_flight {
            self.level = self.params.max_in_flight;
            for v in &mut self.at_level {
                v.clear();
            }
            self.failed_at_level = 0;
            self.completed = 0;
        }
    }

    /// Renders in flight the side should run now.
    pub fn in_flight(&self) -> u32 {
        self.level
    }

    /// The highest level that held, once the side has stepped back from a spike.
    pub fn limit(&self) -> Option<u32> {
        self.limit
    }

    /// The side found a limit (it stepped back from a spike, or reached the most it may
    /// run) and holds a level.
    pub fn settled(&self) -> bool {
        self.limit.is_some() && self.phase == Phase::Holding
    }

    pub fn taking_baseline(&self) -> bool {
        self.phase == Phase::Baseline
    }

    /// A render completed. It counts toward the rate of the level the side runs now. Its
    /// time and its failure say something about the level it started at only, so one that
    /// started at another level is not kept beyond that.
    pub fn render(&mut self, r: Render) {
        if r.page >= self.pages {
            return;
        }
        if r.ok {
            self.completed += 1;
        }
        if r.level != self.level {
            return;
        }
        if !r.ok {
            self.failed_at_level += 1;
        } else if r.ms.is_finite() && r.ms >= 0.0 {
            self.at_level[r.page].push(r.ms);
        }
    }

    fn begin(&mut self, now_ms: u64, pressure_usec: Option<u64>) {
        for v in &mut self.at_level {
            v.clear();
        }
        self.failed_at_level = 0;
        self.completed = 0;
        self.since_ms = Some(now_ms);
        self.pressure_at = pressure_usec;
    }

    fn dwell_done(&self, now_ms: u64) -> bool {
        let since = self.since_ms.unwrap_or(now_ms);
        now_ms.saturating_sub(since) >= self.params.dwell_max_ms
            || self
                .at_level
                .iter()
                .all(|v| v.len() >= self.params.min_renders as usize)
    }

    fn measure(&self, now_ms: u64, pressure_usec: Option<u64>) -> Measured {
        let dwell_ms = now_ms
            .saturating_sub(self.since_ms.unwrap_or(now_ms))
            .max(1);
        let mut ratios = Vec::new();
        for (page, times) in self.at_level.iter().enumerate() {
            if let (Some(m), Some(base)) = (median(times), self.baseline[page]) {
                if base > 0.0 {
                    ratios.push(m / base);
                }
            }
        }
        // A figure from fewer than half the pages is not the level's.
        let ratio = (ratios.len() * 2 >= self.pages.max(1))
            .then(|| median(&ratios))
            .flatten();
        let pressure = match (self.pressure_at, pressure_usec) {
            (Some(a), Some(b)) if b >= a => Some((b - a) as f64 / 1000.0 / dwell_ms as f64),
            _ => None,
        };
        Measured {
            ratio,
            pressure,
            renders_per_s: f64::from(self.completed) * 1000.0 / dwell_ms as f64,
            render_ms: page_mean_ms(self.at_level.iter().map(Vec::as_slice)),
        }
    }

    fn record(&mut self, m: &Measured) {
        if let Some(ms) = m.render_ms {
            self.curve.insert(self.level, (m.renders_per_s, ms));
        }
    }

    fn spiked(&self, m: &Measured) -> Option<SpikeCause> {
        if m.ratio.is_some_and(|r| r > self.params.ratio) {
            Some(SpikeCause::RenderTime)
        } else if m.pressure.is_some_and(|p| p > self.params.pressure) {
            Some(SpikeCause::CpuPressure)
        } else {
            None
        }
    }

    fn back_off(&mut self, cause: SpikeCause, m: &Measured, now_ms: u64, pressure: Option<u64>) {
        let from = self.level;
        self.level = if cause == SpikeCause::Failed {
            (from / 2).max(1)
        } else {
            from.saturating_sub(1).max(1)
        };
        self.limit = Some(self.level);
        self.backed_off = Some(BackOff {
            from,
            cause,
            ratio: m.ratio,
            pressure: m.pressure,
            at: now_ms,
        });
        self.phase = Phase::Holding;
        self.hold_until_ms = now_ms + self.params.hold_ms;
        self.begin(now_ms, pressure);
    }

    fn hold(&mut self, now_ms: u64, pressure: Option<u64>) {
        self.phase = Phase::Holding;
        self.hold_until_ms = now_ms + self.params.hold_ms;
        self.begin(now_ms, pressure);
    }

    /// One decision, to be made about once a second. `pressure_usec` is the side cgroup's
    /// cumulative `some` time from `cpu.pressure`, when it can be read. Returns the renders
    /// in flight the side should run from now.
    pub fn step(&mut self, now_ms: u64, pressure_usec: Option<u64>) -> u32 {
        if self.since_ms.is_none() {
            self.begin(now_ms, pressure_usec);
            return self.level;
        }
        if self.failed_at_level > 0 && self.phase != Phase::Baseline && self.level > 1 {
            let m = self.measure(now_ms, pressure_usec);
            self.back_off(SpikeCause::Failed, &m, now_ms, pressure_usec);
            return self.level;
        }
        match self.phase {
            Phase::Baseline => {
                if !self.dwell_done(now_ms) {
                    return self.level;
                }
                for page in 0..self.pages {
                    if let Some(m) = median(&self.at_level[page]) {
                        self.baseline[page] = Some(m);
                    }
                }
                let m = self.measure(now_ms, pressure_usec);
                self.record(&m);
                match self.return_to.take() {
                    Some(level) => {
                        self.level = level;
                        self.hold(now_ms, pressure_usec);
                    }
                    None => {
                        self.phase = Phase::Adding;
                        self.level = 2.min(self.params.max_in_flight.max(1));
                        self.begin(now_ms, pressure_usec);
                    }
                }
            }
            Phase::Adding => {
                if !self.dwell_done(now_ms) {
                    return self.level;
                }
                let m = self.measure(now_ms, pressure_usec);
                self.record(&m);
                if let Some(cause) = self.spiked(&m) {
                    self.back_off(cause, &m, now_ms, pressure_usec);
                } else if self.level >= self.params.max_in_flight || self.backed_off.is_some() {
                    // The most the side may run, or a probe that held: this is the limit now,
                    // held for a whole hold before the next probe.
                    self.limit = Some(self.level);
                    self.hold(now_ms, pressure_usec);
                } else {
                    self.level += 1;
                    self.begin(now_ms, pressure_usec);
                }
            }
            Phase::Holding => {
                if now_ms < self.hold_until_ms || !self.dwell_done(now_ms) {
                    return self.level;
                }
                let m = self.measure(now_ms, pressure_usec);
                self.record(&m);
                if let Some(cause) = self.spiked(&m).filter(|_| self.level > 1) {
                    self.back_off(cause, &m, now_ms, pressure_usec);
                } else if self.level >= self.params.max_in_flight {
                    self.hold(now_ms, pressure_usec);
                } else {
                    self.phase = Phase::Adding;
                    self.level += 1;
                    self.begin(now_ms, pressure_usec);
                }
            }
        }
        self.level
    }

    /// Take the baseline again at one in flight, then go back to the level held now.
    pub fn rebaseline(&mut self, now_ms: u64, pressure_usec: Option<u64>) {
        if self.phase == Phase::Baseline {
            return;
        }
        self.return_to = Some(self.limit.unwrap_or(self.level));
        self.level = 1;
        self.phase = Phase::Baseline;
        self.begin(now_ms, pressure_usec);
    }

    /// The work changed (an overlay, a transform, a new snapshot or engine build): what was
    /// measured no longer describes it. From one in flight, with nothing known.
    pub fn restart(&mut self) {
        *self = LimitSearch::new(self.params.clone(), self.pages);
    }

    /// The search as the page shows it.
    pub fn view(&self) -> Search {
        Search {
            in_flight: self.level,
            phase: match self.phase {
                Phase::Baseline => SearchPhase::Starting,
                Phase::Adding => SearchPhase::Adding,
                Phase::Holding => SearchPhase::Holding,
            },
            backed_off: self.backed_off.clone(),
            limit: self.limit,
            curve: self
                .curve
                .iter()
                .map(|(level, (rps, ms))| CurvePoint {
                    in_flight: *level,
                    renders_per_s: *rps,
                    render_ms: *ms,
                })
                .collect(),
        }
    }
}

/// The two sides' searches. Each runs the same rule on its own inputs; the only thing they
/// share is when each takes its baseline again.
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub sides: [LimitSearch; 2],
    rebased: [bool; 2],
}

impl Pair {
    pub fn new(params: SearchParams, pages: usize) -> Self {
        Pair {
            sides: [
                LimitSearch::new(params.clone(), pages),
                LimitSearch::new(params, pages),
            ],
            rebased: [false; 2],
        }
    }

    /// One decision for both sides. Once both have found a limit and hold it, each takes its
    /// baseline again with the other at its limit, one side at a time.
    pub fn step(&mut self, now_ms: u64, pressure_usec: [Option<u64>; 2]) -> [u32; 2] {
        if self.sides.iter().all(|s| s.settled()) {
            if let Some(i) = (0..2).find(|i| !self.rebased[*i]) {
                self.sides[i].rebaseline(now_ms, pressure_usec[i]);
                self.rebased[i] = true;
            }
        }
        [
            self.sides[0].step(now_ms, pressure_usec[0]),
            self.sides[1].step(now_ms, pressure_usec[1]),
        ]
    }

    /// The work changed on both sides.
    pub fn restart(&mut self) {
        for s in &mut self.sides {
            s.restart();
        }
        self.rebased = [false; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each side has its own most renders in flight, and starting again keeps it.
    #[test]
    fn a_sides_own_most_survives_a_restart_and_caps_its_level() {
        let mut pair = Pair::new(SearchParams::default(), 2);
        pair.sides[0].set_max_in_flight(3);
        pair.sides[1].set_max_in_flight(5);
        pair.restart();
        let mut now = 0;
        pair.step(now, [Some(0), Some(0)]);
        for _ in 0..200 {
            for side in 0..2 {
                let level = pair.sides[side].in_flight();
                for page in 0..2 {
                    for _ in 0..3 {
                        pair.sides[side].render(Render {
                            page,
                            ms: 100.0,
                            ok: true,
                            level,
                        });
                    }
                }
            }
            now += 1000;
            let levels = pair.step(now, [Some(0), Some(0)]);
            assert!(levels[0] <= 3 && levels[1] <= 5, "{levels:?}");
        }
        assert_eq!(pair.sides[0].limit(), Some(3));
        assert_eq!(pair.sides[1].limit(), Some(5));
        // lowered while running: the level comes down with it
        pair.sides[1].set_max_in_flight(2);
        assert_eq!(pair.sides[1].in_flight(), 2);
    }

    fn params() -> SearchParams {
        SearchParams {
            max_in_flight: 12,
            ..Default::default()
        }
    }

    /// Feed `n` renders of `ms` for each of `pages` pages at the search's current level.
    fn feed(s: &mut LimitSearch, pages: usize, n: usize, ms: f64) {
        let level = s.in_flight();
        for page in 0..pages {
            for _ in 0..n {
                s.render(Render {
                    page,
                    ms,
                    ok: true,
                    level,
                });
            }
        }
    }

    /// Climb with render time `f(level)` and pressure share `p(level)` until the first
    /// back-off; returns the time.
    fn climb(
        s: &mut LimitSearch,
        f: impl Fn(u32) -> f64,
        p: impl Fn(u32) -> f64,
        now: &mut u64,
        psi: &mut u64,
    ) {
        s.step(*now, Some(*psi));
        for _ in 0..200 {
            if s.view().backed_off.is_some() {
                return;
            }
            let level = s.in_flight();
            feed(s, 2, 3, f(level));
            *now += 1000;
            *psi += (p(level) * 1_000_000.0) as u64;
            s.step(*now, Some(*psi));
        }
        panic!("no back-off in 200 steps");
    }

    /// A level's rate is every render the side completed while it ran the level. Renders
    /// that were in flight when the level began complete in its first second or so; left
    /// out, a short dwell reads low by the mean render time over the dwell (on a host, a
    /// 5 s probe of 16 in flight read 20.8 renders a second while the side completed 24.2).
    /// The level's render time is still of the renders that started at it.
    #[test]
    fn a_levels_rate_counts_the_renders_that_complete_during_it() {
        let mut s = LimitSearch::new(params(), 2);
        s.step(0, Some(0));
        // the baseline at one in flight: three renders a page, 1,000 ms each, in 6 s
        feed(&mut s, 2, 3, 1000.0);
        assert_eq!(s.step(6_000, Some(0)), 2);
        // Level 2 for four seconds, completing two renders a second throughout. Those of
        // the first second started before the level began.
        for page in 0..2 {
            s.render(Render {
                page,
                ms: 1000.0,
                ok: true,
                level: 1,
            });
        }
        assert_eq!(s.step(7_000, Some(0)), 2, "no render of this level yet");
        feed(&mut s, 2, 3, 1100.0);
        assert_eq!(s.step(10_000, Some(0)), 3);
        let at = s.view().curve[1].clone();
        assert_eq!(at.in_flight, 2);
        assert!((at.renders_per_s - 2.0).abs() < 1e-9, "{at:?}");
        assert_eq!(at.render_ms, 1100.0);
    }

    /// A side that reaches its most renders in flight without a spike has found its limit
    /// as much as one that stepped back, so the pair takes both baselines again. Without
    /// that, the other side keeps a baseline taken while this one was idle.
    #[test]
    fn a_side_held_at_its_cap_counts_as_settled_for_the_baselines() {
        let mut pair = Pair::new(params(), 2);
        pair.sides[1].set_max_in_flight(3);
        let mut now = 0;
        pair.step(now, [Some(0), Some(0)]);
        let mut again = [false; 2];
        let mut held = [false; 2];
        for _ in 0..400 {
            for side in 0..2 {
                let level = pair.sides[side].in_flight();
                // side 0 spikes at 5 in flight; side 1 never does, and is capped at 3
                let ms = if side == 0 && level >= 5 {
                    140.0
                } else {
                    100.0
                };
                feed(&mut pair.sides[side], 2, 3, ms);
                if pair.sides[side].limit().is_some() {
                    held[side] = true;
                }
                if held[side] && pair.sides[side].taking_baseline() {
                    again[side] = true;
                }
            }
            now += 1000;
            pair.step(now, [Some(0), Some(0)]);
        }
        assert!(pair.sides[1].settled(), "held at its cap of 3");
        assert_eq!(again, [true, true], "both baselines taken again");
        assert_eq!(pair.sides[1].limit(), Some(3));
    }

    #[test]
    fn a_level_waits_until_every_page_has_its_renders() {
        let mut s = LimitSearch::new(params(), 2);
        s.step(0, Some(0));
        // page 0 has its three, page 1 has two
        for _ in 0..3 {
            s.render(Render {
                page: 0,
                ms: 100.0,
                ok: true,
                level: 1,
            });
        }
        for _ in 0..2 {
            s.render(Render {
                page: 1,
                ms: 900.0,
                ok: true,
                level: 1,
            });
        }
        assert_eq!(s.step(1000, Some(0)), 1);
        assert_eq!(s.view().phase, SearchPhase::Starting);
        s.render(Render {
            page: 1,
            ms: 900.0,
            ok: true,
            level: 1,
        });
        assert_eq!(s.step(2000, Some(0)), 2);
        assert_eq!(s.view().phase, SearchPhase::Adding);
        // and no longer than the longest dwell
        assert_eq!(s.step(2000 + 89_999, Some(0)), 2);
        assert_eq!(s.step(2000 + 90_000, Some(0)), 3);
    }

    #[test]
    fn render_time_past_the_ratio_backs_off_to_the_level_below() {
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0, 0);
        // flat to 5 in flight, then 30% slower
        climb(
            &mut s,
            |l| if l <= 5 { 100.0 } else { 130.0 },
            |_| 0.0,
            &mut now,
            &mut psi,
        );
        let v = s.view();
        let back = v.backed_off.unwrap();
        assert_eq!((back.from, back.cause), (6, SpikeCause::RenderTime));
        assert!((back.ratio.unwrap() - 1.3).abs() < 1e-9);
        assert_eq!(
            (v.in_flight, v.limit, v.phase),
            (5, Some(5), SearchPhase::Holding)
        );
    }

    #[test]
    fn pressure_alone_backs_off_and_says_so() {
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0, 0);
        climb(
            &mut s,
            |_| 100.0,
            |l| if l <= 7 { 0.02 } else { 0.2 },
            &mut now,
            &mut psi,
        );
        let back = s.view().backed_off.unwrap();
        assert_eq!((back.from, back.cause), (8, SpikeCause::CpuPressure));
        assert!((back.pressure.unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(s.in_flight(), 7);
    }

    #[test]
    fn each_page_is_held_to_its_own_time_at_one_in_flight() {
        // A page that takes 6 s alone is not a spike; the level's figure is the median of
        // the pages' own ratios.
        let mut s = LimitSearch::new(params(), 3);
        s.step(0, Some(0));
        let times = [100.0, 300.0, 6000.0];
        for now in 1..=4u64 {
            let level = s.in_flight();
            for (page, ms) in times.iter().enumerate() {
                for _ in 0..3 {
                    s.render(Render {
                        page,
                        ms: *ms,
                        ok: true,
                        level,
                    });
                }
            }
            s.step(now * 1000, Some(0));
        }
        assert!(s.view().backed_off.is_none());
        assert_eq!(s.in_flight(), 5);
    }

    #[test]
    fn a_failed_render_backs_off_at_once_to_half() {
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0u64, 0u64);
        s.step(now, Some(psi));
        while s.in_flight() < 8 {
            feed(&mut s, 2, 3, 100.0);
            now += 1000;
            s.step(now, Some(psi));
        }
        s.render(Render {
            page: 0,
            ms: 5.0,
            ok: false,
            level: 8,
        });
        psi += 1;
        assert_eq!(s.step(now + 1, Some(psi)), 4);
        let back = s.view().backed_off.unwrap();
        assert_eq!((back.from, back.cause), (8, SpikeCause::Failed));
        // a failure at one in flight has nowhere to go
        let mut one = LimitSearch::new(params(), 1);
        one.step(0, None);
        one.render(Render {
            page: 0,
            ms: 1.0,
            ok: false,
            level: 1,
        });
        assert_eq!(one.step(1000, None), 1);
    }

    #[test]
    fn after_a_back_off_a_side_probes_once_per_hold() {
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0, 0);
        let f = |l: u32| if l <= 5 { 100.0 } else { 140.0 };
        climb(&mut s, f, |_| 0.0, &mut now, &mut psi);
        let mut probes = Vec::new();
        let start = now;
        // An hour of decisions: every probe is one step up from 5, five minutes after the
        // last step back, and comes back at the next decision.
        for _ in 0..3600 {
            let before = s.in_flight();
            feed(&mut s, 2, 3, f(before));
            now += 1000;
            let after = s.step(now, Some(psi));
            if after > before {
                probes.push(now - start);
                assert_eq!((before, after), (5, 6));
            }
            assert!(after == 5 || after == 6, "left the limit: {after}");
        }
        assert_eq!(probes.len(), 11);
        assert!(
            probes.windows(2).all(|w| w[1] - w[0] == 301_000),
            "{probes:?}"
        );
        assert_eq!(s.limit(), Some(5));
    }

    #[test]
    fn a_probe_that_holds_becomes_the_limit_and_is_held() {
        // The first spike at 6 was noise: 6 holds from then on, 7 does not.
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0, 0);
        climb(
            &mut s,
            |l| if l <= 5 { 100.0 } else { 140.0 },
            |_| 0.0,
            &mut now,
            &mut psi,
        );
        let f = |l: u32| if l <= 6 { 100.0 } else { 140.0 };
        let mut levels = Vec::new();
        for _ in 0..1000 {
            let level = s.in_flight();
            feed(&mut s, 2, 3, f(level));
            now += 1000;
            levels.push(s.step(now, Some(psi)));
        }
        // 300 s at 5, a probe to 6 that holds, 300 s at 6, a probe to 7 that does not
        assert_eq!(levels[298], 5);
        assert_eq!(levels[299], 6);
        assert!(levels[300..600].iter().all(|l| *l == 6));
        assert_eq!((levels[600], levels[601]), (7, 6));
        assert_eq!(s.limit(), Some(6));
    }

    #[test]
    fn a_held_level_that_no_longer_holds_steps_down() {
        // The other side reached its limit: everything here is 30% slower than at baseline.
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0, 0);
        climb(
            &mut s,
            |l| if l <= 5 { 100.0 } else { 140.0 },
            |_| 0.0,
            &mut now,
            &mut psi,
        );
        assert_eq!(s.in_flight(), 5);
        for _ in 0..301 {
            feed(&mut s, 2, 3, 130.0);
            now += 1000;
            s.step(now, Some(psi));
        }
        let v = s.view();
        assert_eq!((v.in_flight, v.limit), (4, Some(4)));
        assert_eq!(v.backed_off.unwrap().from, 5);
    }

    #[test]
    fn a_baseline_taken_again_returns_to_the_held_level() {
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0, 0);
        climb(
            &mut s,
            |l| if l <= 5 { 100.0 } else { 140.0 },
            |_| 0.0,
            &mut now,
            &mut psi,
        );
        s.rebaseline(now, Some(psi));
        assert_eq!((s.in_flight(), s.view().phase), (1, SearchPhase::Starting));
        // 10% slower at one in flight now: the other side is at its limit
        feed(&mut s, 2, 3, 110.0);
        now += 1000;
        assert_eq!(s.step(now, Some(psi)), 5);
        assert_eq!(s.view().phase, SearchPhase::Holding);
        // 130 ms at 6 was a spike against 100 and is not against 110
        for _ in 0..300 {
            feed(&mut s, 2, 3, 110.0);
            now += 1000;
            s.step(now, Some(psi));
        }
        assert_eq!(s.in_flight(), 6);
        feed(&mut s, 2, 3, 130.0);
        now += 1000;
        s.step(now, Some(psi));
        let v = s.view();
        assert_eq!(
            v.backed_off.unwrap().from,
            6,
            "the old spike, not a new one"
        );
        assert_eq!(
            (v.in_flight, v.limit, v.phase),
            (6, Some(6), SearchPhase::Holding)
        );
    }

    #[test]
    fn the_same_inputs_give_the_same_decisions() {
        let run = || {
            let mut s = LimitSearch::new(params(), 2);
            let (mut now, mut psi) = (0u64, 0u64);
            let mut trace = Vec::new();
            s.step(now, Some(psi));
            for i in 0..600u64 {
                let level = s.in_flight();
                feed(
                    &mut s,
                    2,
                    3,
                    100.0 + (level as f64).powi(2) + (i % 7) as f64,
                );
                now += 1000;
                psi += 1000 * level as u64;
                trace.push(s.step(now, Some(psi)));
            }
            (trace, s)
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn each_side_takes_its_baseline_again_once_both_have_settled_one_at_a_time() {
        let mut p = Pair::new(params(), 2);
        let (mut now, mut psi) = (0u64, [0u64; 2]);
        let knee = [4u32, 9u32];
        let mut baselines = [0u32; 2];
        let mut was = [false; 2];
        p.step(now, [Some(0), Some(0)]);
        for _ in 0..900 {
            for i in 0..2 {
                let level = p.sides[i].in_flight();
                let ms = if level <= knee[i] { 100.0 } else { 150.0 };
                feed(&mut p.sides[i], 2, 3, ms);
                psi[i] += 1;
            }
            now += 1000;
            p.step(now, [Some(psi[0]), Some(psi[1])]);
            let taking = [p.sides[0].taking_baseline(), p.sides[1].taking_baseline()];
            assert!(
                !(taking[0] && taking[1]) || now <= 2000,
                "both at one in flight"
            );
            for i in 0..2 {
                if taking[i] && !was[i] && now > 2000 {
                    baselines[i] += 1;
                    // the other side holds its limit meanwhile
                    assert_eq!(p.sides[1 - i].limit(), Some(knee[1 - i]));
                }
                was[i] = taking[i];
            }
        }
        assert_eq!(baselines, [1, 1]);
        assert_eq!([p.sides[0].limit(), p.sides[1].limit()], [Some(4), Some(9)]);
    }

    #[test]
    fn a_restart_knows_nothing() {
        let mut s = LimitSearch::new(params(), 2);
        let (mut now, mut psi) = (0, 0);
        climb(
            &mut s,
            |l| if l <= 5 { 100.0 } else { 140.0 },
            |_| 0.0,
            &mut now,
            &mut psi,
        );
        s.restart();
        assert_eq!(s.view(), LimitSearch::new(params(), 2).view());
        assert_eq!(s.in_flight(), 1);
    }
}
