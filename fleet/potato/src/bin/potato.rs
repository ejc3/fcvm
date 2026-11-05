//! The hot potato player: the one program in a player VM's container.
//!
//! It accepts the ball on `BALL_PORT`, steps its colour, counter and chain, holds the ball,
//! throws it to the next player in the ball's ring, and reports each step to the host agent
//! on a TCP connection (`POTATO_REPORT`, default 127.0.0.1:7071, which the VM forwards to the
//! host). The agent puts the VM to sleep after a throw. Every wake restores this process
//! from the snapshot with the report connection retired, so the reporter sees its socket
//! fail, connects again and says hello with the state it woke with.
//!
//! A player started without POTATO_PLAYER is unassigned: it waits for an `assign` on its
//! ball port, which gives it its number, and only then chooses its colour and starts
//! reporting. The fleet's agent snapshots one unassigned player and clones every player from
//! it.
//!
//! Environment: POTATO_PLAYER (this player's number), POTATO_REPORT, POTATO_COLOURS, and
//! POTATO_BALL_PORT (tests only; a VM always uses `BALL_PORT`).

use fleet_potato::{
    chain_hex, chain_step, next_colour, Ball, Chain, Inbound, Proof, Reply, Report, BALL_PORT,
    CHAIN_ZERO, DEFAULT_COLOURS,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct State {
    /// None until assigned.
    player: Option<u32>,
    colour: u32,
    colours: u32,
    counter: u64,
    chain: Chain,
    last: Option<(u64, u64)>,
    /// A hold asked for after a round started: (round, hold_ms).
    pace: Option<(u64, u64)>,
    /// False from the moment the player throws, or is told its round is over, until the
    /// agent wakes it: either way it is going to sleep, and a ball it caught before the snapshot paused it would be
    /// held by a sleeping VM nobody wakes for it. A ball that arrives meanwhile is refused, so
    /// the thrower's flight runs out and the ball drops, as it does for a player asleep.
    accepting: bool,
    /// Balls of rounds below this are refused (`stop`, `wake`).
    floor: u64,
}

impl State {
    fn proof(&self) -> Proof {
        Proof {
            colour: self.colour,
            counter: self.counter,
            chain: chain_hex(&self.chain),
        }
    }
}

enum Msg {
    Report(String),
    /// The reader of report connection `n` saw it end.
    Dead(u64),
}

fn env_u32(name: &str, default: Option<u32>) -> u32 {
    match std::env::var(name) {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{name}={v:?} is not a number")),
        Err(_) => default.unwrap_or_else(|| panic!("{name} is not set")),
    }
}

/// A colour index from the kernel's random pool, read once, when the player gets its number.
fn first_colour(colours: u32) -> u32 {
    let mut b = [0u8; 4];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("reading /dev/urandom");
    u32::from_le_bytes(b) % colours.max(1)
}

fn send(tx: &Sender<Msg>, r: &Report) {
    let line = serde_json::to_string(r).expect("a report serializes");
    let _ = tx.send(Msg::Report(line));
}

/// Keep one report connection open, opening a new one whenever it fails, and say hello first
/// on each. A report that could not be written is written again on the next connection.
fn reporter(
    addr: String,
    player: u32,
    state: Arc<Mutex<State>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    mut first: Option<Report>,
) {
    let mut connections = 0u64;
    let mut pending: Option<String> = None;
    loop {
        let mut backoff = Duration::from_millis(2);
        let mut conn = loop {
            match TcpStream::connect(&addr) {
                Ok(c) => break c,
                Err(_) => {
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_millis(100));
                }
            }
        };
        let _ = conn.set_nodelay(true);
        connections += 1;
        let gen = connections;
        // The first hello is the state at the assignment, taken before any ball could be
        // caught, so it always comes before the first catch's report.
        let hello = first
            .take()
            .unwrap_or_else(|| hello(player, &state.lock().unwrap(), connections));
        let mut ok = writeln!(conn, "{}", serde_json::to_string(&hello).unwrap()).is_ok();
        if ok {
            if let Some(p) = pending.take() {
                ok = writeln!(conn, "{p}").is_ok();
                if !ok {
                    pending = Some(p);
                }
            }
        }
        if let Ok(mut r) = conn.try_clone() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 256];
                while matches!(r.read(&mut buf), Ok(n) if n > 0) {}
                let _ = tx.send(Msg::Dead(gen));
            });
        } else {
            ok = false;
        }
        while ok {
            match rx.recv().expect("the reporter's channel never closes") {
                Msg::Report(line) => {
                    if writeln!(conn, "{line}").is_err() {
                        pending = Some(line);
                        ok = false;
                    }
                }
                Msg::Dead(g) if g == gen => ok = false,
                Msg::Dead(_) => {}
            }
        }
        let _ = conn.shutdown(Shutdown::Both);
    }
}

fn hello(player: u32, s: &State, connections: u64) -> Report {
    Report::Hello {
        player,
        colours: s.colours,
        proof: s.proof(),
        connections,
        last_round: s.last.map(|l| l.0),
        last_seq: s.last.map(|l| l.1),
    }
}

fn read_line(conn: &TcpStream) -> Option<String> {
    let mut line = String::new();
    BufReader::new(conn.take(64 * 1024))
        .read_line(&mut line)
        .ok()?;
    Some(line)
}

/// Throw `ball` to its next catcher, trying until `flight` has passed since the first try.
fn throw(player: u32, ball: Ball, flight: Duration, tx: &Sender<Msg>) {
    let to = ball.ring[(ball.seq % ball.ring.len() as u64) as usize].clone();
    let line = serde_json::to_string(&Inbound::Ball(ball.clone())).unwrap();
    let start = Instant::now();
    let deadline = start + flight;
    let mut attempts = 0u32;
    let mut last_err = String::from("no attempt");
    while Instant::now() < deadline {
        attempts += 1;
        let t = Instant::now();
        let left = deadline
            .saturating_duration_since(t)
            .max(Duration::from_millis(1));
        let result = (|| -> std::io::Result<()> {
            let sa = to
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| std::io::Error::other("no address"))?;
            let mut c = TcpStream::connect_timeout(&sa, left.min(Duration::from_millis(200)))?;
            c.set_nodelay(true)?;
            c.set_read_timeout(Some(left))?;
            writeln!(c, "{line}")?;
            let ack = read_line(&c).unwrap_or_default();
            Reply::ok(&ack).map_err(std::io::Error::other)
        })();
        match result {
            Ok(()) => {
                send(
                    tx,
                    &Report::Threw {
                        player,
                        round: ball.round,
                        seq: ball.seq,
                        to,
                        attempts,
                        wait_ms: (t - start).as_secs_f64() * 1000.0,
                        send_ms: t.elapsed().as_secs_f64() * 1000.0,
                    },
                );
                return;
            }
            Err(e) => {
                last_err = e.to_string();
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    send(
        tx,
        &Report::Dropped {
            player,
            round: ball.round,
            seq: ball.seq,
            to,
            attempts,
            reason: last_err,
        },
    );
}

/// What every connection's thread shares.
struct Shared {
    state: Arc<Mutex<State>>,
    tx: Sender<Msg>,
    /// The reporter's end of `tx`, until the player has a number and its reporter starts.
    rx: Mutex<Option<Receiver<Msg>>>,
    report: String,
    colours: u32,
}

impl Shared {
    /// Give the player its number and start its reporter, once.
    fn assign(&self, player: u32, colours: u32) -> bool {
        let Some(rx) = self.rx.lock().unwrap().take() else {
            return false;
        };
        let first = {
            let mut s = self.state.lock().unwrap();
            assign(&mut s, player, colours);
            hello(player, &s, 1)
        };
        let (state, tx, report) = (self.state.clone(), self.tx.clone(), self.report.clone());
        thread::spawn(move || reporter(report, player, state, tx, rx, Some(first)));
        true
    }
}

fn main() {
    let colours = env_u32("POTATO_COLOURS", Some(DEFAULT_COLOURS)).max(1);
    let player = std::env::var("POTATO_PLAYER")
        .ok()
        .map(|_| env_u32("POTATO_PLAYER", None));
    let (tx, rx) = channel();
    let shared = Arc::new(Shared {
        state: Arc::new(Mutex::new(State {
            player: None,
            colour: 0,
            colours,
            counter: 0,
            chain: CHAIN_ZERO,
            last: None,
            pace: None,
            accepting: true,
            floor: 0,
        })),
        tx,
        rx: Mutex::new(Some(rx)),
        report: std::env::var("POTATO_REPORT").unwrap_or_else(|_| "127.0.0.1:7071".into()),
        colours,
    });
    let port = env_u32("POTATO_BALL_PORT", Some(u32::from(BALL_PORT))) as u16;
    let listener = TcpListener::bind(("::", port))
        .or_else(|_| TcpListener::bind(("0.0.0.0", port)))
        .expect("binding the ball port");
    if let Some(p) = player {
        shared.assign(p, colours);
    }
    // tests bind port 0 and read the port the kernel chose from this line
    let port = listener.local_addr().map_or(port, |a| a.port());
    eprintln!("potato: player {player:?}, {colours} colours, ball port {port}");
    // One thread per connection: a connection that says nothing must not hold up the ball.
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let shared = shared.clone();
        thread::spawn(move || handle(conn, &shared));
    }
}

fn handle(mut conn: TcpStream, shared: &Shared) {
    let _ = conn.set_nodelay(true);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let Some(line) = read_line(&conn) else {
        return;
    };
    let refuse = |conn: &mut TcpStream, reason: &str| {
        let _ = writeln!(
            conn,
            "{{\"type\":\"refused\",\"reason\":{}}}",
            serde_json::to_string(reason).unwrap()
        );
    };
    let state = &shared.state;
    let me = state.lock().unwrap().player;
    match serde_json::from_str::<Inbound>(line.trim()) {
        Ok(Inbound::Probe) => {
            let (proof, accepting) = {
                let s = state.lock().unwrap();
                (s.proof(), s.accepting)
            };
            let r = Report::Probe {
                player: me,
                proof,
                accepting,
            };
            let _ = writeln!(conn, "{}", serde_json::to_string(&r).unwrap());
        }
        Ok(Inbound::Assign { player, colours }) => {
            if shared.assign(player, colours.unwrap_or(shared.colours)) {
                let _ = writeln!(conn, "{{\"type\":\"ok\"}}");
            } else {
                refuse(&mut conn, "already assigned");
            }
        }
        Ok(Inbound::Pace { round, hold_ms }) => {
            state.lock().unwrap().pace = Some((round, hold_ms));
            let _ = writeln!(conn, "{{\"type\":\"ok\"}}");
        }
        Ok(Inbound::Wake { round }) => {
            {
                let mut s = state.lock().unwrap();
                s.accepting = true;
                s.floor = s.floor.max(round.unwrap_or(0));
            }
            let _ = writeln!(conn, "{{\"type\":\"ok\"}}");
        }
        Ok(Inbound::Stop { round }) => {
            {
                let mut s = state.lock().unwrap();
                s.floor = s.floor.max(round.saturating_add(1));
                // a stop comes before a sleep: a ball of a later round caught now would be held
                // by a VM nobody wakes for it, so it is refused until the wake
                s.accepting = false;
            }
            let _ = writeln!(conn, "{{\"type\":\"ok\"}}");
        }
        Ok(Inbound::Ball(_)) if me.is_none() => refuse(&mut conn, "not assigned"),
        Ok(Inbound::Ball(ball)) if ball.ring.is_empty() => refuse(&mut conn, "empty ring"),
        Ok(Inbound::Ball(ball)) => {
            let player = me.expect("checked above");
            let caught_at = Instant::now();
            let stepped = {
                let mut s = state.lock().unwrap();
                if s.last == Some((ball.round, ball.seq)) {
                    // the thrower did not see the acknowledgement of this catch and sent it
                    // again: the catch stands, once
                    Err(None)
                } else if ball.round < s.floor {
                    Err(Some("that round is over"))
                } else if !s.accepting {
                    Err(Some("asleep: it has thrown and not woken since"))
                } else {
                    s.colour = next_colour(s.colour, s.colours);
                    s.counter += 1;
                    s.chain = chain_step(&s.chain, ball.round, ball.seq, s.colour);
                    s.last = Some((ball.round, ball.seq));
                    let pace = s.pace.filter(|p| p.0 == ball.round).map(|p| p.1);
                    Ok((s.colour, s.proof(), pace))
                }
            };
            let (colour, proof, pace) = match stepped {
                Ok(x) => x,
                Err(None) => {
                    let _ = writeln!(conn, "{{\"type\":\"ok\"}}");
                    return;
                }
                Err(Some(reason)) => {
                    refuse(&mut conn, reason);
                    return;
                }
            };
            let _ = writeln!(conn, "{{\"type\":\"ok\"}}");
            drop(conn);
            send(
                &shared.tx,
                &Report::Caught {
                    player,
                    round: ball.round,
                    seq: ball.seq,
                    proof,
                },
            );
            // a pace caps this hold only; the ball keeps its own schedule
            let hold_ms = match pace {
                Some(p) => ball.hold_ms.min(p.max(ball.min_hold_ms)),
                None => ball.hold_ms,
            };
            let next = ball.thrown(colour);
            let flight = Duration::from_millis(ball.flight_ms);
            thread::sleep(Duration::from_millis(hold_ms).saturating_sub(caught_at.elapsed()));
            // a throw of a round that is over is abandoned; only a throw makes the player go to
            // sleep, so an abandoned one leaves it catching
            let over = {
                let mut s = state.lock().unwrap();
                let over = ball.round < s.floor;
                if !over {
                    s.accepting = false;
                }
                over
            };
            if over {
                let to = next.ring[(next.seq % next.ring.len() as u64) as usize].clone();
                send(
                    &shared.tx,
                    &Report::Dropped {
                        player,
                        round: ball.round,
                        seq: next.seq,
                        to,
                        attempts: 0,
                        reason: "the round is over".into(),
                    },
                );
            } else {
                throw(player, next, flight, &shared.tx);
            }
        }
        Err(e) => refuse(&mut conn, &e.to_string()),
    }
}

/// Give the player its number and its first colour.
fn assign(s: &mut State, player: u32, colours: u32) {
    s.player = Some(player);
    s.colours = colours.max(1);
    s.colour = first_colour(s.colours);
}
