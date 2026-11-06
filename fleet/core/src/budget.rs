//! The grid-wide render budget (fleet/PROTOCOL.md, "Grid budget"; the renderer's reference
//! controller is fleet/renderer/tools/budgetctl.py, 55e81ad5 on fleet-renderer-adapt). One
//! budget, in pixel-samples per frame per tile, for every tile of the wall, so tiles draw the
//! same samples per pixel and take the same path (fast, or high quality from 8 samples per
//! drawn pixel) at the same slot, and moving frames match across their edges:
//!
//! - every round (2 s), once every placed tile has reported a fit, it takes the smallest
//!   `budget_fit` (the slowest tile governs) and smooths it, half the way each round; while a
//!   tile has no fit, only the two steps down below run, from the fits that are known;
//! - down, to the ladder rung under the fit (the lower of the smoothed fit and this round's),
//!   when the budget exceeds the smoothed fit by more than 5%; and at least one rung down at
//!   once when any tile's frame took over 90% of its slot (`render_ms_max`);
//! - up, to the rung under smoothed fit / 1.15, after three rounds in a row allow it;
//! - the first budget is the rung under smoothed fit / 1.15;
//! - a change takes effect for every tile at one scene time `t`, a second past the newest
//!   frame any tile has drawn, rounded up to a frame.
//!
//! A tile whose own fit reaches the budget at which it draws every pixel
//! (`budget_every_pixel`, one sample on each, stride 1) while the grid's budget gives it a
//! checkerboard is granted `every_pixel` after three rounds in a row allow it, at its own
//! scene time; the grant ends at once when its fit falls under that budget or its frames
//! overrun, and a granted tile's overrun does not move the grid. So every tile that can draw
//! every pixel does, and the others draw the grid's checkerboard. Each tile's pixels are
//! the whole image's at its own stride, so its edge with a neighbour at another stride is
//! the edge between two correct images (the renderer's seam test, per stride).
//!
//! Layout generations do not move the budget. Every tile of the wall covers the same share
//! of the image (each side is as wide as its share of the tiles), so one budget gives every
//! tile the same samples per pixel, and the same path, in any layout, and a re-tile changes
//! them for all tiles at the layout's frame. A tile's fit is a cost model of its GPU (a fixed
//! part per frame and a cost per sample on each path), which a tile's size changes little;
//! a re-tile that does is answered at the next round.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Seconds between rounds, and how far past the newest drawn frame a change starts.
pub const ROUND_MS: u64 = 2000;
pub const LEAD_SECS: f64 = 1.0;
/// A frame whose render takes more than this share of its slot forces a rung down.
pub const OVERLOAD_MS: f64 = 0.9 * 1000.0 / 30.0;

/// The largest ladder rung at or below `fit`; the lowest rung when `fit` is below it.
pub fn ladder_floor(fit: f64) -> u64 {
    let mut best = 250_000.0 * 2f64.powf(-8.0 / 4.0);
    for n in -8..=40 {
        let r = 250_000.0 * 2f64.powf(n as f64 / 4.0);
        if r <= fit * (1.0 + 1e-9) {
            best = r;
        }
    }
    best.round() as u64
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Budget {
    /// The budget in force, and the scene time it took effect at.
    pub current: Option<u64>,
    pub from_t: Option<f64>,
    /// The slowest fit, smoothed over rounds.
    #[serde(default)]
    pub smoothed: Option<f64>,
    /// Rounds in a row that allowed a raise.
    #[serde(default)]
    pub up_rounds: u32,
    pub next_round_at: u64,
    /// Every wall tile's every-pixel grant, by tile id.
    #[serde(default)]
    pub every_pixel: BTreeMap<String, TileGrant>,
}

/// A tile's every-pixel grant, the scene time of its last change, and the rounds in a row
/// that have allowed one.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TileGrant {
    pub on: bool,
    pub t: f64,
    #[serde(default)]
    pub up_rounds: u32,
}

/// One wall tile as a round sees it: its fit, the budget at which it draws every pixel, and
/// whether its slowest frame over the last 2 s overran 90% of a slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileFit<'a> {
    pub id: &'a str,
    pub fit: Option<f64>,
    pub every_pixel_budget: Option<f64>,
    pub overloaded: bool,
}

impl Budget {
    /// The budget in force and its start, with every tile's grant, as posted to renderers.
    pub fn body(&self) -> Option<crate::protocol::BudgetPlan> {
        Some(crate::protocol::BudgetPlan {
            budget: self.current?,
            t: self.from_t?,
            every_pixel: self
                .every_pixel
                .iter()
                .map(|(id, g)| (id.clone(), crate::protocol::EveryPixel { on: g.on, t: g.t }))
                .collect(),
        })
    }

    /// Whether tile `id` draws more than the grid's budget gives it: its grant is on and the
    /// grid's budget is under the one at which it draws every pixel. Such a tile's overrun
    /// ends its grant and does not move the grid.
    pub fn elevated(&self, id: &str, every_pixel_budget: Option<f64>) -> bool {
        let on = self.every_pixel.get(id).is_some_and(|g| g.on);
        match (self.current, every_pixel_budget) {
            (Some(cur), Some(e)) => on && (cur as f64) < e,
            _ => on,
        }
    }

    /// The every-pixel grants, after the grid's round: a tile the grid's budget would draw
    /// at a checkerboard whose fit reaches its `every_pixel_budget` is granted after three
    /// rounds in a row allow it; a grant ends when the fit falls under that budget or the
    /// tile's frames overrun. A tile with no fit keeps what it has, unless it overruns.
    /// Tiles no longer on the wall are forgotten. Returns whether a grant changed.
    pub fn strides(&mut self, tiles: &[TileFit], newest_t: f64) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        self.every_pixel
            .retain(|id, _| tiles.iter().any(|t| t.id == id.as_str()));
        let start = ((newest_t + LEAD_SECS) * 30.0).ceil() / 30.0;
        let mut changed = false;
        for t in tiles {
            let g = self.every_pixel.entry(t.id.to_string()).or_default();
            let affords = match (t.fit, t.every_pixel_budget) {
                (Some(f), Some(e)) => Some(f.is_finite() && f >= e),
                _ => None,
            };
            if g.on {
                if t.overloaded || affords == Some(false) {
                    g.on = false;
                    g.t = start;
                    g.up_rounds = 0;
                    changed = true;
                }
                continue;
            }
            let needed = t.every_pixel_budget.is_some_and(|e| (cur as f64) < e);
            if needed && affords == Some(true) && !t.overloaded {
                g.up_rounds += 1;
                if g.up_rounds >= 3 {
                    g.on = true;
                    g.t = start;
                    g.up_rounds = 0;
                    changed = true;
                }
            } else {
                g.up_rounds = 0;
            }
        }
        changed
    }

    /// One round. `fits` holds every placed tile's fit (`None` for a tile that has not
    /// reported); `overloaded` whether any tile's frame took over 90% of its slot;
    /// `newest_t` the newest frame any tile has drawn. Returns whether the budget changed.
    ///
    /// The first budget and every raise need every tile's fit. A tile without one (just
    /// started, or drawing at a new size, whose cost model starts again for 60 s) holds back
    /// neither an overloaded frame's rung down nor a step down to a fit that is known.
    pub fn round(&mut self, fits: &[Option<f64>], overloaded: bool, newest_t: f64) -> bool {
        if fits.is_empty() {
            return false;
        }
        let known: Vec<f64> = fits
            .iter()
            .flatten()
            .copied()
            .filter(|v| v.is_finite() && *v > 0.0)
            .collect();
        if known.len() < fits.len() {
            let Some(cur) = self.current else {
                return false;
            };
            let mut b = cur;
            if let Some(m) = known.iter().copied().reduce(f64::min) {
                if m < 0.95 * cur as f64 {
                    b = b.min(ladder_floor(m));
                }
            }
            if overloaded {
                b = b.min(ladder_floor(cur as f64 * 0.999));
            }
            if b < cur {
                self.up_rounds = 0;
            }
            return self.set(b < cur, b, newest_t);
        }
        let m = known.iter().copied().fold(f64::INFINITY, f64::min);
        let f = match self.smoothed {
            None => m,
            Some(f) => f + (m - f) * 0.5,
        };
        self.smoothed = Some(f);
        let next = match self.current {
            None => Some(ladder_floor(f / 1.15)),
            Some(cur) if overloaded || f < 0.95 * cur as f64 => {
                self.up_rounds = 0;
                let mut b = ladder_floor(f.min(m));
                if overloaded {
                    b = b.min(ladder_floor(cur as f64 * 0.999));
                }
                (b < cur).then_some(b)
            }
            Some(cur) => {
                let up = ladder_floor(f / 1.15);
                if up > cur {
                    self.up_rounds += 1;
                    if self.up_rounds >= 3 {
                        self.up_rounds = 0;
                        Some(up)
                    } else {
                        None
                    }
                } else {
                    self.up_rounds = 0;
                    None
                }
            }
        };
        match next {
            Some(b) => self.set(true, b, newest_t),
            None => false,
        }
    }

    /// Put budget `b` in force a second past the newest frame, when `change`.
    fn set(&mut self, change: bool, b: u64, newest_t: f64) -> bool {
        if change {
            self.current = Some(b);
            self.from_t = Some(((newest_t + LEAD_SECS) * 30.0).ceil() / 30.0);
        }
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile<'a>(id: &'a str, fit: f64, every: f64, overloaded: bool) -> TileFit<'a> {
        TileFit {
            id,
            fit: Some(fit),
            every_pixel_budget: Some(every),
            overloaded,
        }
    }

    #[test]
    fn a_tile_whose_fit_draws_every_pixel_draws_every_pixel() {
        // Staging, 2026-11-05: the grid sat at 250,000 (a checkerboard on every 432- and
        // 540-line tile) because five heavy tiles could not draw every pixel, while the
        // others reported fits that could (825,518 at 432 lines, 275,448 every-pixel
        // budget). Those draw every pixel now; the heavy ones keep the grid's checkerboard.
        let mut b = Budget::default();
        let light = |o| tile("ec2:1", 825_518.0, 275_448.0, o);
        let heavy = |o| tile("ec2:5", 275_173.0, 275_448.0, o);
        assert!(b.round(&[Some(825_518.0), Some(275_173.0)], false, 10.0));
        assert!(b.current.unwrap() < 275_448);
        for r in 0..2 {
            assert!(
                !b.strides(&[light(false), heavy(false)], 10.0 + r as f64),
                "three rounds first"
            );
        }
        assert!(b.strides(&[light(false), heavy(false)], 12.0));
        let plan = b.body().unwrap();
        let l = plan.for_tile("ec2:1");
        let h = plan.for_tile("ec2:5");
        assert!(l.every_pixel && !h.every_pixel);
        assert_eq!(
            (l.budget, h.budget),
            (b.current.unwrap(), b.current.unwrap())
        );
        assert_eq!(
            l.t,
            ((12.0 + LEAD_SECS) * 30.0f64).ceil() / 30.0,
            "at its own scene time"
        );
        assert!(b.elevated("ec2:1", Some(275_448.0)) && !b.elevated("ec2:5", Some(275_448.0)));

        // Its frames overrun: the grant ends at once, at a later time, and the grid stays.
        let grid = b.current;
        assert!(
            !b.round(&[Some(825_518.0), Some(275_173.0)], false, 20.0),
            "an elevated tile's overrun is not the grid's"
        );
        assert!(b.strides(&[light(true), heavy(false)], 20.0));
        assert_eq!(b.current, grid);
        let ended = b.body().unwrap().for_tile("ec2:1");
        assert!(
            !ended.every_pixel && ended.t > l.t,
            "the end is posted at its own time"
        );

        // A fit that falls under the every-pixel budget ends it too.
        for r in 0..3 {
            b.strides(&[light(false), heavy(false)], 30.0 + r as f64);
        }
        assert!(b.body().unwrap().for_tile("ec2:1").every_pixel);
        assert!(b.strides(
            &[tile("ec2:1", 275_173.0, 275_448.0, false), heavy(false)],
            40.0
        ));
        assert!(!b.body().unwrap().for_tile("ec2:1").every_pixel);

        // A tile that leaves the wall is forgotten; one with no fit keeps what it has.
        for r in 0..3 {
            b.strides(&[light(false)], 50.0 + r as f64);
        }
        let nofit = TileFit {
            id: "ec2:1",
            fit: None,
            every_pixel_budget: None,
            overloaded: false,
        };
        assert!(!b.strides(&[nofit], 60.0));
        assert!(b.body().unwrap().for_tile("ec2:1").every_pixel);
        assert!(!b.body().unwrap().every_pixel.contains_key("ec2:5"));

        // Where the grid's budget already draws every pixel there is nothing to grant.
        let mut c = Budget::default();
        c.round(&[Some(4e6)], false, 10.0);
        for r in 0..5 {
            assert!(!c.strides(&[tile("ec2:2", 4e6, 275_448.0, false)], 10.0 + r as f64));
        }
        assert!(!c.body().unwrap().for_tile("ec2:2").every_pixel);
    }

    #[test]
    fn the_ladder_matches_the_reference() {
        assert_eq!(ladder_floor(250_000.0), 250_000);
        assert_eq!(ladder_floor(297_302.0), 297_302);
        assert_eq!(ladder_floor(297_300.0), 250_000);
        assert_eq!(ladder_floor(1.0), 62_500);
    }

    #[test]
    fn a_tile_without_a_fit_does_not_hold_the_grid_up() {
        // Staging, 2026-10-27: a viewer opened slot-6, its renderer started drawing at a
        // new size and its cost model started again (`budget_fit` 0 for 60 s). Its frames
        // overran their slot, and no round ran until its fit came back, because a round
        // needed every tile's fit.
        let mut b = Budget::default();
        assert!(b.round(&[Some(4e5), Some(4e5)], false, 10.0));
        let top = b.current.unwrap();
        assert!(
            b.round(&[Some(4e5), None], true, 12.0),
            "an overloaded frame steps down with a fit missing"
        );
        assert_eq!(b.current, Some(ladder_floor(top as f64 * 0.999)));
        let cur = b.current.unwrap();
        assert!(
            b.round(&[Some(1.5e5), None], false, 14.0),
            "a reported fit under the budget steps down with another missing"
        );
        assert!(b.current.unwrap() < cur);
        let low = b.current;
        for _ in 0..5 {
            assert!(
                !b.round(&[Some(4e6), None], false, 16.0),
                "never up while a tile has not reported"
            );
        }
        assert_eq!(b.current, low);
        let mut fresh = Budget::default();
        assert!(
            !fresh.round(&[Some(4e5), None], true, 10.0),
            "no first budget until every tile reports"
        );
    }

    #[test]
    fn the_reference_rounds() {
        // budgetctl.py's self-test (55e81ad5), round for round.
        let mut b = Budget::default();
        assert!(b.round(&[Some(1e6), Some(5e5)], false, 10.0));
        assert_eq!(
            b.current,
            Some(ladder_floor(5e5 / 1.15)),
            "the slowest tile, with margin"
        );
        let cur = b.current;
        for f in [5.2e5, 4.6e5, 5.4e5, 4.7e5, 5.1e5] {
            assert!(
                !b.round(&[Some(2e6), Some(f)], false, 10.0),
                "+-8% around one load: {f}"
            );
        }
        assert_eq!(b.current, cur);
        for _ in 0..2 {
            assert!(
                !b.round(&[Some(2e6), Some(2e6)], false, 10.0),
                "up only after three rounds"
            );
        }
        assert!(b.round(&[Some(2e6), Some(2e6)], false, 10.0));
        assert!(b.current > cur);
        let hi = b.current;
        assert!(!b.round(&[Some(3e5), Some(2e6)], false, 10.0) || b.current < hi);
        let mut c = Budget::default();
        c.round(&[Some(1e6)], false, 10.0);
        let top = c.current.unwrap();
        assert!(c.round(&[Some(1e6)], true, 10.0), "overload: one rung down");
        assert_eq!(c.current, Some(ladder_floor(top as f64 * 0.999)));
    }
}
