//! The search for a side's limit, run on the ladders a measurement spike recorded
//! (`tests/fixtures/renders/ladder-*.json`: per page, the render time's quantiles at each
//! recorded number of renders in flight, and the side's CPU pressure there).
//!
//! The simulation: `in_flight` workers take the pages in rotation; a render's time is drawn
//! from the nearer recorded level's quantiles and scaled by the page's median interpolated
//! between the two levels around the current one; pressure is interpolated the same way. A
//! decision is made once a simulated second. Nothing here reads a clock.

use fleet_core::renders::{SearchPhase, SpikeCause};
use fleet_core::renders_search::{LimitSearch, Render, SearchParams};
use serde_json::Value;

struct Ladder {
    levels: Vec<u32>,
    /// By level index, by page: the quantiles (sorted).
    pages: Vec<Vec<Vec<f64>>>,
    pressure: Vec<f64>,
    /// Time a worker spends after a render before it starts the next. From the recording:
    /// the workers' cycle at the level before the bend less the mean render time there, 85 ms
    /// for fcvm (a clone's teardown is 73 of it) and 46 ms for Kitesurf.
    gap_ms: f64,
}

fn ladder(name: &str, gap_ms: f64) -> Ladder {
    let path = format!(
        "{}/tests/fixtures/renders/ladder-{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let names: Vec<String> = serde_json::from_value(doc["pages"].clone()).unwrap();
    let mut levels: Vec<u32> = doc["levels"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.parse().unwrap())
        .collect();
    levels.sort();
    let mut pages = Vec::new();
    let mut pressure = Vec::new();
    for level in &levels {
        let l = &doc["levels"][level.to_string()];
        pressure.push(l["cpu_pressure"].as_f64().unwrap());
        pages.push(
            names
                .iter()
                .map(|n| serde_json::from_value(l["pages"][n].clone()).unwrap())
                .collect(),
        );
    }
    Ladder {
        levels,
        pages,
        pressure,
        gap_ms,
    }
}

fn median(q: &[f64]) -> f64 {
    (q[q.len() / 2 - 1] + q[q.len() / 2]) / 2.0
}

impl Ladder {
    /// The recorded levels around `level` and where it sits between them (0 to 1).
    fn around(&self, level: u32) -> (usize, usize, f64) {
        let hi = self
            .levels
            .iter()
            .position(|l| *l >= level)
            .unwrap_or(self.levels.len() - 1);
        let lo = if self.levels[hi] == level || hi == 0 {
            hi
        } else {
            hi - 1
        };
        let w = if hi == lo {
            0.0
        } else {
            (level - self.levels[lo]) as f64 / (self.levels[hi] - self.levels[lo]) as f64
        };
        (lo, hi, w.clamp(0.0, 1.0))
    }

    fn render_ms(&self, level: u32, page: usize, draw: u64) -> f64 {
        let (lo, hi, w) = self.around(level);
        let near = if w < 0.5 { lo } else { hi };
        let q = &self.pages[near][page];
        let at = (1.0 - w) * median(&self.pages[lo][page]) + w * median(&self.pages[hi][page]);
        q[(draw % q.len() as u64) as usize] * at / median(q)
    }

    fn pressure(&self, level: u32) -> f64 {
        let (lo, hi, w) = self.around(level);
        (1.0 - w) * self.pressure[lo] + w * self.pressure[hi]
    }
}

/// A worker: when it is free again, and the render it carries (page, ms, level, done at).
type Worker = (f64, Option<(usize, f64, u32, f64)>);

struct Run {
    /// The level after each simulated second.
    levels: Vec<u32>,
    /// Successful renders completed in each simulated second.
    done: Vec<u32>,
    search: LimitSearch,
}

fn run(l: &Ladder, params: SearchParams, seed: u64, seconds: u64) -> Run {
    let n_pages = l.pages[0].len();
    let mut search = LimitSearch::new(params, n_pages);
    let mut rng = seed;
    let mut draw = move || {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        rng >> 33
    };
    let mut workers: Vec<Worker> = Vec::new();
    let mut cursor = 0usize;
    let mut psi_usec = 0.0f64;
    let mut out = Run {
        levels: Vec::new(),
        done: Vec::new(),
        search: search.clone(),
    };
    search.step(0, Some(0));
    let tick = 20.0;
    let mut now = 0.0f64;
    for _ in 0..seconds {
        let mut done = 0;
        for _ in 0..(1000.0 / tick) as usize {
            now += tick;
            let level = search.in_flight();
            while workers.len() < level as usize {
                workers.push((0.0, None));
            }
            for (i, w) in workers.iter_mut().enumerate() {
                if let Some((page, ms, at, done_at)) = w.1 {
                    if done_at <= now {
                        search.render(Render {
                            page,
                            ms,
                            ok: true,
                            level: at,
                        });
                        done += 1;
                        w.1 = None;
                    }
                }
                if w.1.is_none() && w.0 <= now && (i as u32) < level {
                    let page = cursor % n_pages;
                    cursor += 1;
                    let ms = l.render_ms(level, page, draw());
                    w.1 = Some((page, ms, level, now + ms));
                    w.0 = now + ms + l.gap_ms;
                }
            }
            psi_usec += l.pressure(level) * tick * 1000.0;
        }
        search.step(now as u64, Some(psi_usec as u64));
        out.levels.push(search.in_flight());
        out.done.push(done);
    }
    out.search = search;
    out
}

fn changes(levels: &[u32]) -> usize {
    levels.windows(2).filter(|w| w[0] != w[1]).count()
}

fn rate(done: &[u32]) -> f64 {
    done.iter().sum::<u32>() as f64 / done.len() as f64
}

/// fcvm's recorded ladder bends between 16 and 20 in flight (render time 1.19 then 1.47
/// times its time alone; pressure 0.05 then 0.24). The search stops there from any seed,
/// holds the level below its spike, and goes on completing what the ladder recorded there.
#[test]
fn the_search_finds_where_the_fcvm_ladder_bends() {
    let l = ladder("fcvm", 85.0);
    for seed in [1, 2, 3, 4, 5] {
        let r = run(&l, SearchParams::default(), seed, 1200);
        let v = r.search.view();
        let back = v.backed_off.expect("it stepped back");
        assert!((17..=20).contains(&back.from), "seed {seed}: {back:?}");
        let limit = v.limit.unwrap();
        assert!((16..=19).contains(&limit), "seed {seed}: limit {limit}");
        assert!(
            r.levels.iter().all(|l| *l <= 20),
            "seed {seed}: went past the bend"
        );
        let late = &r.levels[600..];
        assert!(
            late.iter().all(|l| (15..=20).contains(l)),
            "seed {seed}: left the limit: {:?}",
            late.iter().min()
        );
        // a hold, a probe, a step back: a few changes in ten minutes, not one every few seconds
        assert!(
            changes(late) <= 12,
            "seed {seed}: {} changes",
            changes(late)
        );
        // the ladder recorded 25.4 renders/s at 16 in flight and 26.0 at 20
        let rps = rate(&r.done[600..]);
        assert!((23.0..=27.5).contains(&rps), "seed {seed}: {rps} renders/s");
    }
}

/// Kitesurf's recorded ladder bends between 36 and 40 (1.08 then 1.23; pressure 0.05 then
/// 0.15) and is past both thresholds at 48.
#[test]
fn the_search_finds_where_the_kitesurf_ladder_bends() {
    let l = ladder("kitesurf", 46.0);
    for seed in [1, 2, 3, 4, 5] {
        let r = run(&l, SearchParams::default(), seed, 1800);
        let v = r.search.view();
        let back = v.backed_off.expect("it stepped back");
        assert!((38..=44).contains(&back.from), "seed {seed}: {back:?}");
        let limit = v.limit.unwrap();
        assert!((36..=43).contains(&limit), "seed {seed}: limit {limit}");
        assert!(
            r.levels.iter().all(|l| *l <= 45),
            "seed {seed}: went past the bend"
        );
        let late = &r.levels[1200..];
        assert!(
            changes(late) <= 12,
            "seed {seed}: {} changes",
            changes(late)
        );
        // the ladder recorded 31.3 renders/s at 36 in flight and 31.7 at 40
        let rps = rate(&r.done[1200..]);
        assert!((29.0..=33.5).contains(&rps), "seed {seed}: {rps} renders/s");
    }
}

/// The curve the page draws is what each level measured, and it follows the ladder: more
/// renders a second with every render added until the bend.
#[test]
fn the_curve_follows_the_ladder_up_to_the_bend() {
    let l = ladder("fcvm", 85.0);
    let r = run(&l, SearchParams::default(), 7, 600);
    let curve = r.search.view().curve;
    assert_eq!(curve[0].in_flight, 1);
    assert!(curve.len() >= 16);
    for w in curve.windows(2).take_while(|w| w[1].in_flight <= 14) {
        assert!(
            w[1].renders_per_s > w[0].renders_per_s * 0.97,
            "{:?} then {:?}",
            w[0],
            w[1]
        );
    }
    let at = |n: u32| curve.iter().find(|c| c.in_flight == n).unwrap();
    assert!(at(16).renders_per_s > 12.0 * at(1).renders_per_s);
    assert!(at(16).render_ms < 1.3 * at(1).render_ms);
}

/// With the pressure signal off the ratio alone finds the bend, and with the ratio off
/// the pressure alone does: either signal is enough, and the cause says which fired.
#[test]
fn either_signal_alone_finds_the_bend() {
    let l = ladder("kitesurf", 46.0);
    let ratio_only = SearchParams {
        pressure: f64::INFINITY,
        ..Default::default()
    };
    let r = run(&l, ratio_only, 1, 1200);
    let back = r.search.view().backed_off.unwrap();
    assert_eq!(back.cause, SpikeCause::RenderTime);
    assert!((39..=46).contains(&back.from), "{back:?}");
    let pressure_only = SearchParams {
        ratio: f64::INFINITY,
        ..Default::default()
    };
    let r = run(&l, pressure_only, 1, 1200);
    let back = r.search.view().backed_off.unwrap();
    assert_eq!(back.cause, SpikeCause::CpuPressure);
    assert!((40..=44).contains(&back.from), "{back:?}");
}

/// A side whose most renders in flight is under its bend holds at that most and says so by
/// holding, without a back-off.
#[test]
fn a_side_capped_below_its_bend_holds_at_the_cap() {
    let l = ladder("kitesurf", 46.0);
    let capped = SearchParams {
        max_in_flight: 24,
        ..Default::default()
    };
    let r = run(&l, capped, 1, 900);
    let v = r.search.view();
    assert_eq!((v.in_flight, v.phase), (24, SearchPhase::Holding));
    assert!(v.backed_off.is_none());
    assert_eq!(v.limit, Some(24));
}

/// One seed, twice: the same levels second by second.
#[test]
fn a_run_replays_exactly() {
    let l = ladder("fcvm", 85.0);
    let a = run(&l, SearchParams::default(), 9, 900);
    let b = run(&l, SearchParams::default(), 9, 900);
    assert_eq!(a.levels, b.levels);
    assert_eq!(a.search, b.search);
}
