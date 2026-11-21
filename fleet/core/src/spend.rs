//! The fleet's daily spend: a ledger, per UTC day, of what the fleet's EC2 instances cost,
//! and the forced sleep at `Config::daily_spend_usd`.
//!
//! The coordinator's meter prices every instance the fleet runs (spot at its spot price, on
//! demand at its on-demand price) and reports the sum as a burn rate (`Event::Burn`). The
//! ledger integrates that rate over time: `usd` is what `day` cost through `at`, and the rate
//! holds from `at` until the next report. The ledger is part of the stored state, so a
//! restarted coordinator keeps the day's spend, and the time it was down is charged at the
//! last rate it knew (the hosts keep running while it is down).
//!
//! The fleet trips when today's spend plus [`TRIP_LEAD_MS`] at the current rate reaches the
//! ceiling: it sleeps at once, and until 00:00 UTC no wake, arrival or claim wakes it
//! (`power.reason` is `budget`). The next UTC day starts a new ledger.
//!
//! Neither half assumes the fleet is awake only for viewers. The meter charges every instance
//! the fleet runs, whatever keeps it up, and the trip overrides every reason to be awake: a
//! viewer, a claim, a race, or any rule that keeps hosts up on a schedule. Such a rule must
//! check [`resting`] and stand down while it holds, as `power_step` does and as the warm
//! windows do (`crate::warm::holds`).

use serde::{Deserialize, Serialize};

pub const DAY_MS: u64 = 86_400_000;
const HOUR_MS: f64 = 3_600_000.0;

/// The fleet trips this long before the ceiling, at the current burn rate.
pub const TRIP_LEAD_MS: u64 = 15 * 60_000;

/// `power.reason` while the day's budget is spent.
pub const BUDGET: &str = "budget";

/// The refusal a wake, a claim or a race gets while the day's budget is spent.
pub const SPENT: &str = "today's GPU budget is spent; the fleet rests until 00:00 UTC";

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Spend {
    /// The UTC day `usd` is for, in days since the Unix epoch.
    pub day: u64,
    /// Spent on `day` through `at`, in USD.
    pub usd: f64,
    /// The burn rate from `at` on: USD per hour of every instance the meter priced.
    pub usd_per_hour: f64,
    /// Instances the last report priced.
    #[serde(default)]
    pub instances: u32,
    /// Unix ms the ledger was brought forward to. 0: never.
    pub at: u64,
    /// The ceiling was reached on `day`.
    pub tripped: bool,
    /// The UTC day for which the operator requested the $1 drill ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drill_day: Option<u64>,
}

impl Spend {
    pub fn ceiling(&self, configured: Option<f64>, day: u64) -> Option<f64> {
        configured.map(|usd| {
            if self.drill_day == Some(day) {
                usd.min(1.0)
            } else {
                usd
            }
        })
    }

    /// The day and what it cost through `at`, with the current rate since `self.at`. Only
    /// the part of that time inside `at`'s day counts toward it.
    pub fn spent_at(&self, at: u64) -> (u64, f64) {
        let day = at / DAY_MS;
        if self.at == 0 {
            return (day, 0.0);
        }
        let at = at.max(self.at);
        let rate = self.usd_per_hour.max(0.0);
        if day == self.day {
            (day, self.usd + rate * (at - self.at) as f64 / HOUR_MS)
        } else {
            let from = self.at.max(day * DAY_MS);
            (day, rate * (at - from) as f64 / HOUR_MS)
        }
    }

    /// Bring the ledger forward to `at`. A new day starts a new ledger, not tripped.
    pub fn bring_forward(&mut self, at: u64) {
        let (day, usd) = self.spent_at(at);
        if day != self.day {
            self.tripped = false;
        }
        self.day = day;
        self.usd = usd;
        self.at = self.at.max(at);
    }

    /// Whether, at `at`, today's spend plus [`TRIP_LEAD_MS`] at the current rate reaches
    /// `ceiling`.
    pub fn reaches(&self, at: u64, ceiling: f64) -> bool {
        let (_, usd) = self.spent_at(at);
        usd + self.usd_per_hour.max(0.0) * TRIP_LEAD_MS as f64 / HOUR_MS >= ceiling
    }

    /// Tripped for the day `at` falls in.
    pub fn tripped_on(&self, at: u64) -> bool {
        self.tripped && self.day == at / DAY_MS
    }
}

/// The fleet rests at `at`: a ceiling is set and the day's budget is spent.
pub fn resting(state: &crate::State, at: u64) -> bool {
    state.config.daily_spend_usd.is_some() && state.fleet.spend.tripped_on(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u64, hours: f64) -> u64 {
        day * DAY_MS + (hours * HOUR_MS) as u64
    }

    #[test]
    fn the_rate_accrues_from_the_last_report() {
        let mut s = Spend::default();
        s.bring_forward(at(10, 1.0));
        s.usd_per_hour = 20.0;
        assert_eq!(s.spent_at(at(10, 3.0)), (10, 40.0));
        s.bring_forward(at(10, 2.5));
        assert_eq!((s.day, s.usd), (10, 30.0));
    }

    #[test]
    fn a_new_utc_day_counts_only_its_own_hours() {
        let mut s = Spend::default();
        s.bring_forward(at(10, 23.0));
        s.usd_per_hour = 10.0;
        s.tripped = true;
        assert_eq!(s.spent_at(at(11, 0.5)), (11, 5.0));
        s.bring_forward(at(11, 0.5));
        assert_eq!((s.day, s.usd, s.tripped), (11, 5.0, false));
    }

    #[test]
    fn it_reaches_the_ceiling_fifteen_minutes_early_at_the_current_rate() {
        let mut s = Spend::default();
        s.bring_forward(at(3, 0.0));
        s.usd = 740.0;
        s.usd_per_hour = 40.0;
        // 740 + 40/4 = 750
        assert!(s.reaches(at(3, 0.0), 750.0));
        s.usd_per_hour = 39.0;
        assert!(!s.reaches(at(3, 0.0), 750.0));
        assert!(s.reaches(at(3, 0.01), 750.0));
    }
}
