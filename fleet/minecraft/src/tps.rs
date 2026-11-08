//! Ticks per second, measured as a client measures it: the server's game time (its tick count,
//! in every `set_time` packet) against the client's own clock, over a sliding window.

use std::collections::VecDeque;

/// The window, in ms.
pub const WINDOW_MS: u64 = 10_000;

#[derive(Debug, Default)]
pub struct Tps {
    samples: VecDeque<(u64, u64)>,
}

impl Tps {
    /// A `set_time` packet's game time, received at `now_ms` on a monotonic clock.
    pub fn sample(&mut self, now_ms: u64, game_time: u64) {
        if let Some(&(_, last)) = self.samples.back()
            && game_time < last
        {
            // A new world or a restored server: start again.
            self.samples.clear();
        }
        self.samples.push_back((now_ms, game_time));
        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|&(t, _)| t + WINDOW_MS < now_ms)
        {
            self.samples.pop_front();
        }
    }

    /// Ticks per second over the window; `None` with under 2 s of samples.
    pub fn tps(&self) -> Option<f64> {
        let (&(t0, g0), &(t1, g1)) = (self.samples.front()?, self.samples.back()?);
        (t1 >= t0 + 2_000).then(|| (g1 - g0) as f64 * 1000.0 / (t1 - t0) as f64)
    }

    /// How long ago (ms, on the same clock as `sample`) the last game time arrived.
    pub fn age(&self, now_ms: u64) -> Option<u64> {
        self.samples.back().map(|&(t, _)| now_ms.saturating_sub(t))
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tps_is_ticks_over_seconds_in_the_window() {
        let mut t = Tps::default();
        assert_eq!(t.tps(), None);
        for s in 0..=20 {
            t.sample(s * 1000, s * 20);
        }
        assert!((t.tps().unwrap() - 20.0).abs() < 1e-9);
        // A slow second: 10 ticks.
        t.sample(21_000, 410);
        assert!(t.tps().unwrap() < 20.0);
        assert_eq!(t.age(21_500), Some(500));
        t.sample(22_000, 5);
        assert_eq!(t.tps(), None);
    }
}
