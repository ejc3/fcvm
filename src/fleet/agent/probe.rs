//! Readiness probes whose attempts overlap.
//!
//! A wait for a restored VM starts probing at spawn, before anything in the guest listens. A
//! SYN sent then is dropped, not refused (nothing reads the tap yet, or a proxy accepted the
//! connection and has nobody to hand it to), so the attempt gets no answer and ends only at its
//! own timeout. A loop that awaits one attempt before it sends the next cannot see the guest
//! before that timeout, however early the guest is ready: with a 250 ms attempt timeout, every
//! restore that was ready sooner was seen at 250 ms plus one poll.
//!
//! [`Probes`] starts an attempt every `every` whether or not the earlier ones have answered, so
//! a guest is seen within one `every` (plus one answer) of the moment it starts answering. Each
//! attempt keeps its own timeout, which bounds how many are in flight: the attempt timeout
//! divided by `every`, rounded up. A cap below that would bring the step back: with every
//! slot held by a lost attempt, the next attempt could not start before the oldest timed out.

use futures_util::stream::{FuturesUnordered, StreamExt};
use std::future::Future;
use std::time::Duration;
use tokio::time::{Interval, MissedTickBehavior};

/// Attempts of one probe, started `every` apart and answered in whatever order they finish.
pub struct Probes<F, Fut> {
    every: Interval,
    attempt: F,
    in_flight: FuturesUnordered<Fut>,
}

impl<F, Fut> Probes<F, Fut>
where
    F: FnMut() -> Fut,
    Fut: Future,
{
    /// `attempt` makes one attempt and must bound it itself (a request timeout). The first
    /// attempt starts at the first [`Probes::next`].
    pub fn new(every: Duration, attempt: F) -> Self {
        let mut every = tokio::time::interval(every);
        // A caller that was busy between two `next` calls gets one attempt for the ticks it
        // missed, not a burst of them.
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            every,
            attempt,
            in_flight: FuturesUnordered::new(),
        }
    }

    /// The answer of the next attempt to finish. Answers can arrive out of the order their
    /// attempts started in. Cancel safe: attempts in flight stay in flight for the next call,
    /// and dropping `self` drops them.
    pub async fn next(&mut self) -> Fut::Output {
        loop {
            tokio::select! {
                _ = self.every.tick() => self.in_flight.push((self.attempt)()),
                Some(answer) = self.in_flight.next() => return answer,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Instant;

    /// One attempt against a guest that starts answering at `ready`: sent before then it is
    /// lost and ends at `timeout` with nothing; sent from then on it is answered after `rtt`.
    async fn attempt(ready: Instant, timeout: Duration, rtt: Duration) -> Option<Instant> {
        if Instant::now() < ready {
            tokio::time::sleep(timeout).await;
            return None;
        }
        tokio::time::sleep(rtt).await;
        Some(Instant::now())
    }

    /// When a wait that probes every `every` sees a guest that starts answering `ready_after`
    /// after the wait began.
    async fn seen_after(every: Duration, ready_after: Duration, timeout: Duration) -> Duration {
        let start = Instant::now();
        let ready = start + ready_after;
        let rtt = Duration::from_millis(1);
        let mut probes = Probes::new(every, || attempt(ready, timeout, rtt));
        loop {
            if let Some(at) = probes.next().await {
                return at - start;
            }
        }
    }

    /// A guest that starts answering N ms after the wait began is seen within one probe
    /// interval (and one answer) of N, for N across and beyond the attempt timeout. The first
    /// attempt is sent at once and lost every time.
    ///
    /// RED BEFORE THE FIX: with each attempt awaited before the next was sent, every N under
    /// the 250 ms attempt timeout was seen at 251 ms.
    #[tokio::test(start_paused = true)]
    async fn a_guest_is_seen_within_one_interval_of_answering() {
        let every = Duration::from_millis(20);
        let timeout = Duration::from_millis(250);
        for ready_ms in [1, 19, 20, 87, 130, 249, 250, 251, 400, 1000] {
            let ready = Duration::from_millis(ready_ms);
            let seen = seen_after(every, ready, timeout).await;
            assert!(
                seen >= ready && seen <= ready + every + Duration::from_millis(1),
                "a guest answering from {ready:?} was seen at {seen:?}, not within {every:?} of it"
            );
        }
    }

    /// No more attempts are in flight than the attempt timeout holds intervals, however long
    /// the guest stays silent.
    #[tokio::test(start_paused = true)]
    async fn attempts_in_flight_are_bounded_by_the_attempt_timeout() {
        let every = Duration::from_millis(20);
        let timeout = Duration::from_millis(250);
        let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let most = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut probes = Probes::new(every, || {
            let (live, most) = (live.clone(), most.clone());
            async move {
                use std::sync::atomic::Ordering::SeqCst;
                most.fetch_max(live.fetch_add(1, SeqCst) + 1, SeqCst);
                tokio::time::sleep(timeout).await;
                live.fetch_sub(1, SeqCst);
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                probes.next().await;
            }
        })
        .await;
        let most = most.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (12..=13).contains(&most),
            "{most} attempts in flight at once; 250 ms of 20 ms intervals hold 13"
        );
    }
}
