//! The grid-wide render budget (fleet/PROTOCOL.md, "Budget"; the renderer's reference
//! controller is fleet/renderer/tools/budgetctl.py on fleet-renderer-adapt). One budget, in
//! pixel-samples per frame per tile, for every tile of the wall, so tiles draw the same
//! samples per pixel and moving frames match across their edges:
//!
//! - every round (2 s) it takes the smallest `budget_fit` the placed tiles report (the
//!   slowest tile governs) and rounds it down to the ladder 250,000 x 2^(n/4), n -8..40;
//! - a lower rung is taken at once, a higher one only after two rounds in a row ask for at
//!   least that much (then the lower of the two asks);
//! - nothing is decided until every placed tile has reported a fit;
//! - a change takes effect for every tile at one scene time `t`, a second past the newest
//!   frame any tile has drawn, rounded up to a frame.
//!
//! Layout generations do not move the budget. Every tile of the wall covers the same share
//! of the image (each side is as wide as its share of the tiles), so one budget gives every
//! tile the same samples per pixel in any layout, and a re-tile changes them for all tiles
//! at the layout's frame. A tile's fit measures its GPU's pixel-samples per frame, which a
//! tile's size changes little; a re-tile that does is answered at the next round.

use serde::{Deserialize, Serialize};

/// Seconds between rounds, and how far past the newest drawn frame a change starts.
pub const ROUND_MS: u64 = 2000;
pub const LEAD_SECS: f64 = 1.0;

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
    /// A higher rung asked for in the previous round.
    pub want_up: Option<u64>,
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
    /// reported); `newest_t` the newest frame any tile has drawn. Returns whether the budget
    /// changed.
    pub fn round(&mut self, fits: &[Option<f64>], newest_t: f64) -> bool {
        if fits.is_empty()
            || fits
                .iter()
                .any(|f| f.is_none_or(|v| !v.is_finite() || v <= 0.0))
        {
            return false;
        }
        let slowest = fits.iter().flatten().copied().fold(f64::INFINITY, f64::min);
        let b = ladder_floor(slowest);
        let next = match self.current {
            None => Some(b),
            Some(cur) if b < cur => {
                self.want_up = None;
                Some(b)
            }
            Some(cur) if b > cur => match self.want_up.take() {
                Some(asked) => Some(b.min(asked)),
                None => {
                    self.want_up = Some(b);
                    None
                }
            },
            Some(_) => {
                self.want_up = None;
                None
            }
        };
        match next {
            Some(b) => {
                self.current = Some(b);
                self.from_t = Some(((newest_t + LEAD_SECS) * 30.0).ceil() / 30.0);
                true
            }
            None => false,
        }
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
    fn the_reference_rounds() {
        // budgetctl.py's self-test, round for round.
        let mut b = Budget::default();
        assert!(b.round(&[Some(1e6), Some(5e5)], 10.0));
        assert_eq!(
            b.current,
            Some(ladder_floor(5e5)),
            "the slowest tile governs"
        );
        assert!(
            !b.round(&[Some(2e6), Some(2e6)], 12.0),
            "up only after two rounds"
        );
        assert!(b.round(&[Some(2e6), Some(1.5e6)], 14.0));
        assert_eq!(
            b.current,
            Some(ladder_floor(1.5e6)),
            "the lower of the two asks"
        );
        assert!(b.round(&[Some(3e5), Some(2e6)], 16.0));
        assert_eq!(b.current, Some(ladder_floor(3e5)), "down at once");
    }
}
