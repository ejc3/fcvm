//! Readiness probes of a VM that is being restored.
//!
//! A wait for a restored VM starts probing at spawn, before anything in the guest listens. A
//! SYN sent then is dropped, not refused (nothing reads the tap yet), so the attempt's connect
//! never completes and the attempt ends only at its own timeout. A loop that awaits one long
//! attempt before it sends the next cannot see the guest before that timeout, however early the
//! guest is ready: with a 250 ms attempt timeout, every restore that was ready sooner was seen
//! at 250 ms plus one poll.
//!
//! What is lost is a connect, so a short timeout bounds the connect alone ([`Budget`]). An
//! attempt whose connect completed has reached a listener: it says so ([`Connected::mark`]) and
//! keeps the answer timeout the wait had before, however long the wait has run. A just-restored
//! guest that listens can take longer to answer than a connect is given.
//!
//! [`Probes`] runs a wait in two phases:
//!
//! - **The first second** ([`FAST_FOR`]) of a wait: an attempt starts every [`Pace::every`]
//!   whether or not earlier ones have ended, each with [`Pace::fast_connect`] for its connect.
//!   A lost connect is over within that, and a guest that starts listening is reached within
//!   one `every`. Once an attempt has connected, no further attempt starts while it waits for
//!   its answer.
//! - **After it**: one attempt at a time, the next at the first tick after the last one ended.
//!   The connect timeout doubles from `fast_connect` with each attempt up to [`Pace::answer`],
//!   the timeout these waits had before. A guest that has not been reached within a second (a
//!   slow boot, a wedged VM) is then probed as it was before this module.
//!
//! What a VM costs, in sockets open at once and SYNs sent:
//!
//! | the guest | in the first second | after it |
//! |---|---|---|
//! | does not answer a SYN | `fast_connect / every` sockets, rounded up; `1 s / every` SYNs | 1 socket; at most `1 s / answer` SYNs a second, once the connect timeout has grown |
//! | accepts and does not answer | the attempts that had started when the first one connected (as many sockets at most as the row above), each kept for `answer`; then one at a time | 1 socket, one attempt per `answer` |
//!
//! With the callers' paces (`every`, `fast_connect`, `answer`), for a guest that does not
//! answer a SYN: a Minecraft tile (20 ms, 30 ms, 250 ms) 2 sockets and 50 SYNs, then 4 a second;
//! a potato player (10 ms, 25 ms, 250 ms) 3 sockets and 100 SYNs, then 4 a second; a renderer
//! or a Windows bridge (100 ms, 100 ms, 2 s) 1 socket and 10 SYNs, then one every 2 s. The
//! tests pin these over ten seconds. An HTTP wait makes its connect with [`reach`] and then its
//! request on a connection of the client's own, so an attempt that reaches a listener opens
//! two connections, one after the other.
//!
//! Answers are yielded in the order their attempts were sent: one that arrives after a later
//! attempt's answer was yielded is dropped, so a caller never acts on older news after newer.

use futures_util::stream::{FuturesUnordered, StreamExt};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    /// An attempt's connect timeout during the first [`FAST_FOR`] of the wait.
    pub fast_connect: Duration,
    /// How long a connected attempt may wait for its answer, and the longest a connect's
    /// timeout grows to after the first second.
    pub answer: Duration,
}

/// What one attempt may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// On its connect.
    pub connect: Duration,
    /// On its answer, from the moment it is connected.
    pub answer: Duration,
}

/// An attempt's word that its connect completed.
#[derive(Debug, Clone, Default)]
pub struct Connected(Arc<AtomicBool>);

impl Connected {
    /// The attempt's connect has completed: it reached a listener.
    pub fn mark(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_marked(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Connect to `addr` within the budget's connect timeout, say so, and close the connection:
/// the connect of an attempt whose client hides its own (an HTTP client). True when a listener
/// was reached; the caller then makes its request with the budget's answer timeout.
pub async fn reach(
    addr: impl tokio::net::ToSocketAddrs,
    budget: Budget,
    connected: &Connected,
) -> bool {
    let reached = matches!(
        tokio::time::timeout(budget.connect, tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    );
    if reached {
        connected.mark();
    }
    reached
}

/// One attempt and the order it was sent in. `Unpin` whatever `Fut` is.
struct Sent<Fut> {
    seq: u64,
    connected: Connected,
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
    /// The connect timeout of the next attempt of the slow phase.
    slow_next: Duration,
}

impl<F, Fut> Probes<F, Fut>
where
    F: FnMut(Budget, Connected) -> Fut,
    Fut: Future,
{
    /// `attempt` makes one attempt within the budget it is handed and marks the handle when
    /// its connect completes. The first attempt starts at the first [`Probes::next`].
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
            slow_next: pace.fast_connect.min(pace.answer),
        }
    }

    fn fast(&self) -> bool {
        self.began.is_none_or(|began| began.elapsed() < FAST_FOR)
    }

    /// Whether an attempt may start now: in the first second unless one that has connected
    /// is waiting for its answer, after it only when none is in flight.
    fn may_start(&self) -> bool {
        if self.fast() {
            !self.in_flight.iter().any(|sent| sent.connected.is_marked())
        } else {
            self.in_flight.is_empty()
        }
    }

    fn start(&mut self) {
        self.began.get_or_insert_with(Instant::now);
        let connect = if self.fast() {
            self.pace.fast_connect.min(self.pace.answer)
        } else {
            self.slow_next = (self.slow_next * 2).min(self.pace.answer);
            self.slow_next
        };
        self.sent += 1;
        let connected = Connected::default();
        let budget = Budget {
            connect,
            answer: self.pace.answer,
        };
        self.in_flight.push(Sent {
            seq: self.sent,
            attempt: Box::pin((self.attempt)(budget, connected.clone())),
            connected,
        });
    }

    /// The answer of the next attempt to finish, unless a later attempt's answer was already
    /// yielded. Cancel safe: attempts in flight stay in flight for the next call, and
    /// dropping `self` drops them.
    pub async fn next(&mut self) -> Fut::Output {
        loop {
            let may_start = self.may_start();
            tokio::select! {
                // An attempt that ends at the instant a tick is due is read first: read
                // second, it would still count as in flight when the tick asks whether an
                // attempt may start, and past the first second that tick would be spent
                // on nothing and the next attempt would start an interval late.
                biased;
                Some((seq, answer)) = self.in_flight.next() => {
                    if seq > self.answered {
                        self.answered = seq;
                        return answer;
                    }
                }
                // Checked again: an attempt may have connected while this tick was awaited.
                _ = self.tick.tick(), if may_start => if self.may_start() { self.start() },
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

    const TILE: Pace = Pace {
        every: Duration::from_millis(20),
        fast_connect: Duration::from_millis(30),
        answer: Duration::from_millis(250),
    };
    const PLAYER: Pace = Pace {
        every: Duration::from_millis(10),
        fast_connect: Duration::from_millis(25),
        answer: Duration::from_millis(250),
    };
    const RENDERER: Pace = Pace {
        every: Duration::from_millis(100),
        fast_connect: Duration::from_millis(100),
        answer: Duration::from_secs(2),
    };
    /// How long a connect to a listening guest takes in these tests.
    const CONNECT: Duration = Duration::from_millis(1);

    /// One attempt against a guest that listens from `listening` and answers a connected
    /// attempt after `answers_in` (`None`: never). A connect sent before the guest listens is
    /// lost and ends at the connect timeout; a connected attempt ends at its answer or at the
    /// answer timeout.
    async fn attempt(
        listening: Instant,
        answers_in: Option<Duration>,
        budget: Budget,
        connected: Connected,
    ) -> Option<Instant> {
        if Instant::now() < listening {
            tokio::time::sleep(budget.connect).await;
            return None;
        }
        tokio::time::sleep(CONNECT).await;
        connected.mark();
        match answers_in {
            Some(answer) if answer <= budget.answer => {
                tokio::time::sleep(answer).await;
                Some(Instant::now())
            }
            _ => {
                tokio::time::sleep(budget.answer).await;
                None
            }
        }
    }

    /// When a wait sees a guest that starts listening `listening_after` after the wait began
    /// and takes `answers_in` to answer.
    async fn seen_after(pace: Pace, listening_after: Duration, answers_in: Duration) -> Duration {
        let start = Instant::now();
        let listening = start + listening_after;
        let mut probes = Probes::new(pace, |budget, connected| {
            attempt(listening, Some(answers_in), budget, connected)
        });
        loop {
            if let Some(at) = probes.next().await {
                return at - start;
            }
        }
    }

    /// In the first second of a wait, a guest that starts listening N ms after the wait began
    /// is seen within one probe interval (and one connect and answer) of N. The first attempts
    /// are sent at once and lost every time.
    ///
    /// RED BEFORE THE FIX: with each 250 ms attempt awaited before the next was sent, every N
    /// under 250 ms was seen at 251 ms.
    #[tokio::test(start_paused = true)]
    async fn a_guest_is_seen_within_one_interval_of_listening() {
        let answer = Duration::from_millis(1);
        for pace in [TILE, PLAYER] {
            for ready_ms in [1, 19, 20, 87, 130, 249, 250, 251, 400, 900] {
                let ready = Duration::from_millis(ready_ms);
                let seen = seen_after(pace, ready, answer).await;
                assert!(
                    seen >= ready && seen <= ready + pace.every + CONNECT + answer,
                    "{pace:?}: a guest listening from {ready:?} was seen at {seen:?}"
                );
            }
        }
    }

    /// A guest that listens and takes longer to answer than a connect is given, as a
    /// just-restored one can, is seen when its answer arrives: the attempt that reached it is
    /// not cut short at the connect's timeout.
    ///
    /// RED BEFORE THE FIX: the short timeout bounded the whole attempt, so every attempt of
    /// the first second was abandoned before its answer and the guest was first seen in the
    /// slow phase, a second after the wait began.
    #[tokio::test(start_paused = true)]
    async fn a_slow_answer_is_seen_when_it_arrives() {
        for (pace, answers_ms) in [
            (TILE, [31, 40, 100, 249]),
            (PLAYER, [26, 40, 100, 249]),
            (RENDERER, [101, 400, 1000, 1999]),
        ] {
            for answers_ms in answers_ms {
                for listening_ms in [0, 100, 500] {
                    let listening = Duration::from_millis(listening_ms);
                    let answers_in = Duration::from_millis(answers_ms);
                    let seen = seen_after(pace, listening, answers_in).await;
                    assert!(
                        seen >= listening + answers_in
                            && seen <= listening + pace.every + CONNECT + answers_in,
                        "{pace:?}: listening from {listening:?} and answering in \
                         {answers_in:?}, the guest was seen at {seen:?}"
                    );
                }
            }
        }
    }

    /// Past the first second a guest is seen no later after it starts listening than the
    /// answer timeout, and no later than it has been waited for.
    #[tokio::test(start_paused = true)]
    async fn a_late_guest_is_seen_within_the_old_timeout() {
        let answer = Duration::from_millis(1);
        for pace in [TILE, PLAYER, RENDERER] {
            for ready_ms in [1001, 1100, 1400, 2000, 3000, 7000, 30_000] {
                let ready = Duration::from_millis(ready_ms);
                let seen = seen_after(pace, ready, answer).await;
                let late = seen - ready;
                assert!(
                    seen >= ready && late <= ready.min(pace.answer) + pace.every,
                    "{pace:?}: a guest listening from {ready:?} was seen {late:?} later"
                );
            }
        }
    }

    /// A renderer's attempts of the first second each end at the instant the next tick is
    /// due, and so does the last of them at the turn to the slow phase. Whichever the wait
    /// reads first, the slow phase's first attempt starts at that instant.
    ///
    /// RED BEFORE THE FIX: the wait read the two in an order drawn at random; reading the
    /// tick first spent it, and the first slow attempt started 100 ms late in about half
    /// of all runs (7 attempts in the nine seconds after the first, or 8).
    #[tokio::test(start_paused = true)]
    async fn an_attempt_that_ends_on_a_tick_does_not_cost_the_tick() {
        for _ in 0..200 {
            assert_eq!(cost_of_silence(RENDERER, false).await, (1, 10, 8));
        }
    }

    /// What ten seconds of a guest that never answers cost: the most sockets open at once
    /// and the attempts sent, in the first second and in the nine after it. `listens`: the
    /// guest accepts connections (and answers nothing); otherwise its SYNs are lost.
    async fn cost_of_silence(pace: Pace, listens: bool) -> (usize, usize, usize) {
        let live = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let listening = if listens {
            start
        } else {
            start + Duration::from_secs(3600)
        };
        let fast_sent = Arc::new(AtomicUsize::new(0));
        let mut probes = Probes::new(pace, |budget, connected| {
            let (live, most) = (live.clone(), most.clone());
            sent.fetch_add(1, SeqCst);
            if start.elapsed() < FAST_FOR {
                fast_sent.fetch_add(1, SeqCst);
            }
            async move {
                most.fetch_max(live.fetch_add(1, SeqCst) + 1, SeqCst);
                let answer = attempt(listening, None, budget, connected).await;
                live.fetch_sub(1, SeqCst);
                answer
            }
        });
        // Half a millisecond past the ten seconds: every attempt here starts and ends on a
        // whole millisecond, so none is tied with the end of the count.
        let window = Duration::from_secs(10) + Duration::from_micros(500);
        let _ = tokio::time::timeout(window, async {
            loop {
                probes.next().await;
            }
        })
        .await;
        let fast = fast_sent.load(SeqCst);
        (most.load(SeqCst), fast, sent.load(SeqCst) - fast)
    }

    /// The cost of a guest that answers no SYN is what the module's table says: a few
    /// sockets and one attempt an interval for a second, then one socket and the old cadence.
    ///
    /// RED BEFORE THE FIX: with an attempt every interval for the whole wait and the old
    /// timeout on each, a silent tile held 13 sockets and was sent 501 SYNs in ten seconds.
    #[tokio::test(start_paused = true)]
    async fn a_silent_guest_costs_a_few_sockets_then_one() {
        assert_eq!(cost_of_silence(TILE, false).await, (2, 50, 38));
        assert_eq!(cost_of_silence(PLAYER, false).await, (3, 100, 38));
        assert_eq!(cost_of_silence(RENDERER, false).await, (1, 10, 8));
    }

    /// A guest that accepts connections and never answers is given the old answer timeout by
    /// each attempt that reached it, one attempt at a time, and the wait goes on.
    #[tokio::test(start_paused = true)]
    async fn a_guest_that_accepts_and_never_answers_is_asked_one_at_a_time() {
        // 251 ms an attempt (the connect and the answer timeout), the next at the first tick
        // after it: 40 of them in ten seconds, 4 in the first.
        assert_eq!(cost_of_silence(TILE, true).await, (1, 4, 36));
        assert_eq!(cost_of_silence(PLAYER, true).await, (1, 4, 36));
        assert_eq!(cost_of_silence(RENDERER, true).await, (1, 1, 4));
    }

    /// An answer that arrives after a later attempt's answer was yielded is dropped: the
    /// caller reads answers in the order their attempts were sent.
    ///
    /// RED BEFORE THE FIX: answers were yielded in the order they arrived.
    #[tokio::test(start_paused = true)]
    async fn an_answer_older_than_one_already_yielded_is_dropped() {
        // Attempt 1 takes 25 ms to be refused, attempts 2 and later 1 ms; none connects.
        let mut n = 0u32;
        let mut probes = Probes::new(PLAYER, |_, _| {
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
        let mut probes = Probes::new(PLAYER, |_, _| {
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
