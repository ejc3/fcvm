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
//! Layout generations do not move the budget. Every tile of the wall covers the same share
//! of the image (each side is as wide as its share of the tiles), so one budget gives every
//! tile the same samples per pixel, and the same path, in any layout, and a re-tile changes
//! them for all tiles at the layout's frame. A tile's fit is a cost model of its GPU (a fixed
//! part per frame and a cost per sample on each path), which a tile's size changes little;
//! a re-tile that does is answered at the next round.

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
}

impl Budget {
    /// The budget in force and its start, as posted to renderers.
    pub fn body(&self) -> Option<crate::protocol::BudgetBody> {
        Some(crate::protocol::BudgetBody {
            budget: self.current?,
            t: self.from_t?,
        })
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
