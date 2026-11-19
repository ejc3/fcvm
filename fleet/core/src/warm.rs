//! Warm hours: weekly UTC windows in which the fleet stays awake with no viewers
//! (`Config::warm_hours`, the coordinator's `--warm-hours`).
//!
//! Inside a window a sleeping fleet wakes at once and an awake one does not sleep for want of
//! viewers or arrivals, so a viewer who comes during announced hours skips the first wake.
//! Outside every window the viewer rules alone decide. Whether a window holds is read from the
//! clock and the configured windows, so nothing about it is stored: a restarted coordinator
//! decides from its own flag at its first event.
//!
//! The time a window keeps hosts up is charged like any other (`crate::spend`): the meter
//! prices every instance the fleet runs. The day's budget wins: while [`crate::spend::resting`]
//! holds, the warm rule stands down until 00:00 UTC.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::spend::DAY_MS;

const MINUTE_MS: u64 = 60_000;
const WEEK_MS: u64 = 7 * DAY_MS;
/// 1970-01-01, day 0, was a Thursday: day `d` is weekday `(d + 3) % 7`, Monday 0.
const EPOCH_WEEKDAY: u64 = 3;
const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// `power.reason` while a warm window holds the fleet up.
pub const WARM: &str = "warm";

/// One weekly window, `<day> HH:MM-HH:MM` in UTC (`Sat 17:00-23:00`). It starts on `day` at
/// the first time and ends at the second; an end at or before the start is on the next day
/// (`Sat 22:00-02:00` ends Sunday 02:00). `24:00` is accepted as an end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Window {
    /// Monday 0 through Sunday 6.
    day: u8,
    /// Minutes after the day's 00:00 UTC.
    start: u16,
    /// Minutes the window lasts, 1 through 1440.
    minutes: u16,
}

impl Window {
    /// Whether Unix ms `at` falls in the window, start included and end excluded.
    pub fn contains(&self, at: u64) -> bool {
        let in_week = (at + EPOCH_WEEKDAY * DAY_MS) % WEEK_MS;
        let start = self.day as u64 * DAY_MS + self.start as u64 * MINUTE_MS;
        (in_week + WEEK_MS - start) % WEEK_MS < self.minutes as u64 * MINUTE_MS
    }
}

fn clock(s: &str, end: bool) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    match (h, m) {
        (24, 0) if end => Some(1440),
        (0..=23, 0..=59) => Some(h * 60 + m),
        _ => None,
    }
}

impl FromStr for Window {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("warm window {s:?} is not `<Mon..Sun> HH:MM-HH:MM` (UTC)");
        let (day, times) = s.trim().split_once(' ').ok_or_else(bad)?;
        let day = DAYS
            .iter()
            .position(|d| d.eq_ignore_ascii_case(day))
            .ok_or_else(bad)? as u8;
        let (from, to) = times.trim().split_once('-').ok_or_else(bad)?;
        let start = clock(from, false).ok_or_else(bad)?;
        let end = clock(to, true).ok_or_else(bad)?;
        if end == start {
            return Err(format!("warm window {s:?} starts where it ends"));
        }
        let minutes = if end > start {
            end - start
        } else {
            end + 1440 - start
        };
        Ok(Window {
            day,
            start,
            minutes,
        })
    }
}

impl TryFrom<String> for Window {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<Window> for String {
    fn from(w: Window) -> String {
        w.to_string()
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A window that ends at the day's end is written `24:00`.
        let end = match self.start + self.minutes {
            1440 => 1440,
            end => end % 1440,
        };
        write!(
            f,
            "{} {:02}:{:02}-{:02}:{:02}",
            DAYS[self.day as usize],
            self.start / 60,
            self.start % 60,
            end / 60,
            end % 60
        )
    }
}

/// The warm rule holds at `at`: the fleet is power managed, `at` is inside a configured
/// window, and the day's budget is not spent.
pub fn holds(state: &crate::State, at: u64) -> bool {
    state.config.power_managed
        && !crate::spend::resting(state, at)
        && state.config.warm_hours.iter().any(|w| w.contains(at))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;
    /// 1970-01-03, a Saturday.
    const SAT: u64 = 2 * DAY_MS;

    #[test]
    fn a_window_covers_its_start_and_not_its_end() {
        let w: Window = "Sat 17:00-23:00".parse().unwrap();
        assert!(!w.contains(SAT + 17 * HOUR - 1));
        assert!(w.contains(SAT + 17 * HOUR));
        assert!(w.contains(SAT + 23 * HOUR - 1));
        assert!(!w.contains(SAT + 23 * HOUR));
        // The same hours a week later, and not on another day.
        assert!(w.contains(SAT + 7 * DAY_MS + 18 * HOUR));
        assert!(!w.contains(SAT + DAY_MS + 18 * HOUR));
    }

    #[test]
    fn a_window_that_ends_before_it_starts_runs_into_the_next_day() {
        let w: Window = "sun 22:30-01:00".parse().unwrap();
        let sun = SAT + DAY_MS;
        assert!(w.contains(sun + 22 * HOUR + 30 * 60_000));
        assert!(w.contains(sun + DAY_MS + 59 * 60_000));
        assert!(!w.contains(sun + DAY_MS + HOUR));
        assert_eq!(w.to_string(), "Sun 22:30-01:00");
        let all: Window = "Thu 00:00-24:00".parse().unwrap();
        assert!(all.contains(0) && all.contains(DAY_MS - 1) && !all.contains(DAY_MS));
        assert_eq!(all.to_string(), "Thu 00:00-24:00");
    }

    #[test]
    fn nonsense_is_refused() {
        for s in [
            "",
            "Sat",
            "Sat 17:00",
            "Sat 17-23",
            "Sat 7:00-23:00",
            "Sat 17:00-23:60",
            "Sat 24:00-02:00",
            "Sat 17:00-17:00",
            "Saturday 17:00-23:00",
            "Sat 17:00-25:00",
        ] {
            assert!(s.parse::<Window>().is_err(), "{s:?} parsed");
        }
    }
}
