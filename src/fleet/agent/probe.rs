//! Readiness probes of a VM that is being restored.
//!
//! A wait for a restored VM starts probing at spawn, before anything in the guest listens. A
//! SYN sent then is dropped, not refused (nothing reads the tap yet), so the attempt gets no
//! answer and ends only at its own timeout. A loop that awaits one long attempt before it sends
//! the next cannot see the guest before that timeout, however early the guest is ready: with a
//! 250 ms attempt timeout, every restore that was ready sooner was seen at 250 ms plus one poll.
//!
//! [`Probes`] runs a wait in two phases:
//!
//! - **The first second** ([`FAST_FOR`]) of a wait: an attempt starts every [`Pace::every`]
//!   whether or not earlier ones have answered, each with the short [`Pace::fast_timeout`]. The
//!   short timeout is what removes the step: a lost attempt is over within it, and a guest that
//!   starts answering is seen within one `every` and one answer.
//! - **After it**: one attempt at a time, the next at the first tick after the last one ended.
//!   The attempt timeout doubles from `fast_timeout` with each attempt up to
//!   [`Pace::slow_timeout`], the timeout these waits had before. A guest that has not answered
//!   within a second (a slow boot, a wedged VM) is then probed as it was before this module,
//!   and is seen no later after it starts answering than the timeout then in force.
//!
//! What a silent VM costs, in sockets open at once and SYNs sent:
//!
//! | | in the first second | after it |
//! |---|---|---|
//! | sockets | `fast_timeout / every`, rounded up | 1 |
//! | SYNs a second | `1 s / every` | at most `1 s / slow_timeout`, once the timeout has grown |
//!
//! With the callers' paces: a Minecraft tile (20 ms, 30 ms, 250 ms) 2 sockets and 50 SYNs, then
//! 4 a second; a potato player (10 ms, 25 ms, 250 ms) 3 sockets and 100 SYNs, then 4 a second; a
//! renderer or a Windows bridge (100 ms, 100 ms, 2 s) 1 socket and 10 SYNs, then one every 2 s.
//! The tests pin these over ten seconds of silence.
//!
//! Answers are yielded in the order their attempts were sent: one that arrives after a later
//! attempt's answer was yielded is dropped, so a caller never acts on older news after newer.

use futures_util::stream::{FuturesUnordered, StreamExt};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Instant, Interval, MissedTickBehavior};

/// How long a wait probes at its fast pace.
pub const FAST_FOR: Duration = Duration::from_secs(1);

/// How one wait probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    /// The time between the starts of two attempts.
    pub every: Duration,
    /// An attempt's timeout during the first [`FAST_FOR`] of the wait.
    pub fast_timeout: Duration,
    /// The longest an attempt's timeout grows to after it.
    pub slow_timeout: Duration,
}

/// One attempt and the order it was sent in. `Unpin` whatever `Fut` is.
struct Sent<Fut> {
    seq: u64,
    attempt: Pin<Box<Fut>>,
}

impl<Fut: Future> Future for Sent<Fut> {
    type Output = (u64, Fut::Output);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let seq = self.seq;
        self.attempt.as_mut().poll(cx).map(|answer| (seq, answer))
    }
}

/// The attempts of one wait.
pub struct Probes<F, Fut> {
    pace: Pace,
    tick: Interval,
    attempt: F,
    in_flight: FuturesUnordered<Sent<Fut>>,
    /// When the first attempt started.
    began: Option<Instant>,
    sent: u64,
    /// The newest attempt whose answer was yielded.
    answered: u64,
    /// The timeout of the next attempt of the slow phase.
    slow_next: Duration,
}

impl<F, Fut> Probes<F, Fut>
where
    F: FnMut(Duration) -> Fut,
    Fut: Future,
{
    /// `attempt` makes one attempt and gives up after the timeout it is handed. The first
    /// attempt starts at the first [`Probes::next`].
    pub fn new(pace: Pace, attempt: F) -> Self {
        let mut tick = tokio::time::interval(pace.every);
        // A caller that was busy between two `next` calls gets one attempt for the ticks it
        // missed, not a burst of them.
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            pace,
            tick,
            attempt,
            in_flight: FuturesUnordered::new(),
            began: None,
            sent: 0,
            answered: 0,
            slow_next: pace.fast_timeout.min(pace.slow_timeout),
        }
    }

    fn fast(&self) -> bool {
        self.began.is_none_or(|began| began.elapsed() < FAST_FOR)
    }

    fn start(&mut self) {
        self.began.get_or_insert_with(Instant::now);
        let timeout = if self.fast() {
            self.pace.fast_timeout.min(self.pace.slow_timeout)
        } else {
            self.slow_next = (self.slow_next * 2).min(self.pace.slow_timeout);
            self.slow_next
        };
        self.sent += 1;
        self.in_flight.push(Sent {
            seq: self.sent,
            attempt: Box::pin((self.attempt)(timeout)),
        });
    }

    /// The answer of the next attempt to finish, unless a later attempt's answer was already
    /// yielded. Cancel safe: attempts in flight stay in flight for the next call, and
    /// dropping `self` drops them.
    pub async fn next(&mut self) -> Fut::Output {
        loop {
            // Past the fast phase an attempt starts only when none is in flight.
            let may_start = self.fast() || self.in_flight.is_empty();
            tokio::select! {
                _ = self.tick.tick(), if may_start => self.start(),
                Some((seq, answer)) = self.in_flight.next() => {
                    if seq > self.answered {
                        self.answered = seq;
                        return answer;
                    }
                }
            }
        }
    }

    /// Drop the attempts in flight: their answers describe the guest as it was before
    /// something the caller has just done to it (a scene it posted), and must not be read
    /// as the guest's state after it.
    pub fn forget_in_flight(&mut self) {
        self.in_flight.clear();
        self.answered = self.sent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;

    const TILE: Pace = Pace {
        every: Duration::from_millis(20),
        fast_timeout: Duration::from_millis(30),
        slow_timeout: Duration::from_millis(250),
    };
    const PLAYER: Pace = Pace {
        every: Duration::from_millis(10),
        fast_timeout: Duration::from_millis(25),
        slow_timeout: Duration::from_millis(250),
    };
    const RENDERER: Pace = Pace {
        every: Duration::from_millis(100),
        fast_timeout: Duration::from_millis(100),
        slow_timeout: Duration::from_secs(2),
    };

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

    /// When a wait sees a guest that starts answering `ready_after` after the wait began.
    async fn seen_after(pace: Pace, ready_after: Duration) -> Duration {
        let start = Instant::now();
        let ready = start + ready_after;
        let rtt = Duration::from_millis(1);
        let mut probes = Probes::new(pace, |timeout| attempt(ready, timeout, rtt));
        loop {
            if let Some(at) = probes.next().await {
                return at - start;
            }
        }
    }

    /// In the first second of a wait, a guest that starts answering N ms after the wait began
    /// is seen within one probe interval (and one answer) of N. The first attempts are sent
    /// at once and lost every time.
    ///
    /// RED BEFORE THE FIX: with each 250 ms attempt awaited before the next was sent, every N
    /// under 250 ms was seen at 251 ms.
    #[tokio::test(start_paused = true)]
    async fn a_guest_is_seen_within_one_interval_of_answering() {
        for pace in [TILE, PLAYER] {
            for ready_ms in [1, 19, 20, 87, 130, 249, 250, 251, 400, 900] {
                let ready = Duration::from_millis(ready_ms);
                let seen = seen_after(pace, ready).await;
                assert!(
                    seen >= ready && seen <= ready + pace.every + Duration::from_millis(1),
                    "{pace:?}: a guest answering from {ready:?} was seen at {seen:?}"
                );
            }
        }
    }

    /// Past the first second a guest is seen no later after it starts answering than the slow
    /// timeout, and no later than it has been waited for.
    #[tokio::test(start_paused = true)]
    async fn a_late_guest_is_seen_within_the_slow_timeout() {
        for pace in [TILE, PLAYER, RENDERER] {
            for ready_ms in [1001, 1100, 1400, 2000, 3000, 7000, 30_000] {
                let ready = Duration::from_millis(ready_ms);
                let seen = seen_after(pace, ready).await;
                let late = seen - ready;
                assert!(
                    seen >= ready && late <= ready.min(pace.slow_timeout) + pace.every,
                    "{pace:?}: a guest answering from {ready:?} was seen {late:?} later"
                );
            }
        }
    }

    /// What ten seconds of a silent guest cost: the most sockets open at once and the
    /// attempts (SYNs) sent, in the first second and in the nine after it.
    async fn cost_of_silence(pace: Pace) -> (usize, usize, usize) {
        let live = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let fast_sent = Arc::new(AtomicUsize::new(0));
        let mut probes = Probes::new(pace, |timeout| {
            let (live, most) = (live.clone(), most.clone());
            sent.fetch_add(1, SeqCst);
            if start.elapsed() < FAST_FOR {
                fast_sent.fetch_add(1, SeqCst);
            }
            async move {
                most.fetch_max(live.fetch_add(1, SeqCst) + 1, SeqCst);
                tokio::time::sleep(timeout).await;
                live.fetch_sub(1, SeqCst);
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                probes.next().await;
            }
        })
        .await;
        let fast = fast_sent.load(SeqCst);
        (most.load(SeqCst), fast, sent.load(SeqCst) - fast)
    }

    /// The cost of a silent guest is what the module's table says: a few sockets and one
    /// attempt an interval for a second, then one socket and the old cadence.
    ///
    /// RED BEFORE THE FIX: with an attempt every interval for the whole wait and the old
    /// timeout on each, a silent tile held 13 sockets and was sent 500 SYNs in ten seconds, a
    /// player probed every 5 ms 50 sockets and 2,000.
    #[tokio::test(start_paused = true)]
    async fn a_silent_guest_costs_a_few_sockets_then_one() {
        assert_eq!(cost_of_silence(TILE).await, (2, 50, 38));
        assert_eq!(cost_of_silence(PLAYER).await, (3, 100, 38));
        assert_eq!(cost_of_silence(RENDERER).await, (1, 10, 8));
    }

    /// An answer that arrives after a later attempt's answer was yielded is dropped: the
    /// caller reads answers in the order their attempts were sent.
    ///
    /// RED BEFORE THE FIX: answers were yielded in the order they arrived.
    #[tokio::test(start_paused = true)]
    async fn an_answer_older_than_one_already_yielded_is_dropped() {
        // Attempt 1 takes 25 ms to answer, attempts 2 and later 1 ms.
        let mut n = 0u32;
        let mut probes = Probes::new(PLAYER, |_| {
            n += 1;
            let (n, rtt) = (n, if n == 1 { 25 } else { 1 });
            async move {
                tokio::time::sleep(Duration::from_millis(rtt)).await;
                n
            }
        });
        let mut read = Vec::new();
        while read.len() < 4 {
            read.push(probes.next().await);
        }
        assert_eq!(
            read,
            [2, 3, 4, 5],
            "attempt 1's answer was read after attempt 2's"
        );
    }

    /// Attempts in flight when the caller changes the guest are forgotten: the next answer
    /// is of an attempt sent after the change.
    #[tokio::test(start_paused = true)]
    async fn forgotten_attempts_are_not_answered() {
        let mut n = 0u32;
        let mut probes = Probes::new(PLAYER, |_| {
            n += 1;
            let n = n;
            async move {
                tokio::time::sleep(Duration::from_millis(22)).await;
                n
            }
        });
        assert_eq!(probes.next().await, 1);
        // Attempts 2 and 3 are in flight (sent at 10 and 20 ms).
        probes.forget_in_flight();
        let next = probes.next().await;
        assert!(next >= 4, "attempt {next} was sent before the change");
    }
}
