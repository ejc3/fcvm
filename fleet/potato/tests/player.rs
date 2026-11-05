//! The player program as host processes: the ball between two players, the report
//! connection a wake retires, and a throw nothing accepts.

use fleet_potato::{chain_from_hex, chain_hex, chain_step, Ball, Inbound, Report, DEFAULT_COLOURS};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;

struct Player(Child);

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A port the kernel picks, released for the player to bind.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn player(n: u32, ball_port: u16, report: u16) -> Player {
    spawn(Some(n), ball_port, report)
}

fn spawn(n: Option<u32>, ball_port: u16, report: u16) -> Player {
    let mut c = Command::new(env!("CARGO_BIN_EXE_potato"));
    if let Some(n) = n {
        c.env("POTATO_PLAYER", n.to_string());
    }
    Player(
        c.env("POTATO_REPORT", format!("127.0.0.1:{report}"))
            .env("POTATO_BALL_PORT", ball_port.to_string())
            .spawn()
            .expect("starting the player"),
    )
}

/// One message on a player's ball port, and its one-line answer.
fn ask(port: u16, msg: &Inbound) -> String {
    let mut c = connect(port);
    writeln!(c, "{}", serde_json::to_string(msg).unwrap()).unwrap();
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line).unwrap();
    line
}

/// The host side of the report connections: every report, the open connections in the
/// order they came (a test closes one the way a wake retires it), and each player's latest.
type Latest = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u32, TcpStream>>>;

fn report_server() -> (u16, Receiver<Report>, Receiver<TcpStream>, Latest) {
    report_server_on(0)
}

fn report_server_on(port: u16) -> (u16, Receiver<Report>, Receiver<TcpStream>, Latest) {
    let l = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = channel();
    let (ctx, crx) = channel();
    let latest: Latest = Default::default();
    let by_player = latest.clone();
    thread::spawn(move || {
        for c in l.incoming() {
            let c = c.unwrap();
            ctx.send(c.try_clone().unwrap()).unwrap();
            let tx: Sender<Report> = tx.clone();
            let by_player = by_player.clone();
            let keep = c.try_clone().unwrap();
            thread::spawn(move || {
                for line in BufReader::new(c).lines() {
                    let Ok(line) = line else { return };
                    let r: Report = serde_json::from_str(&line).expect("a report is JSON");
                    if let Report::Hello { player, .. } = &r {
                        by_player
                            .lock()
                            .unwrap()
                            .insert(*player, keep.try_clone().unwrap());
                    }
                    let _ = tx.send(r);
                }
            });
        }
    });
    (port, rx, crx, latest)
}

/// What the agent does after a throw, as far as the player can tell: it is put to sleep, and
/// once restored it is told it woke.
fn wake(port: u16) {
    let ok = ask(port, &Inbound::Wake { round: None });
    assert!(ok.contains("\"ok\""), "{ok}");
}

fn next(rx: &Receiver<Report>) -> Report {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("a report within 10 s")
}

fn connect(port: u16) -> TcpStream {
    for _ in 0..500 {
        if let Ok(c) = TcpStream::connect(("127.0.0.1", port)) {
            return c;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("nothing listens on {port}");
}

fn send_ball(port: u16, ball: &Ball) {
    let mut c = connect(port);
    writeln!(
        c,
        "{}",
        serde_json::to_string(&Inbound::Ball(ball.clone())).unwrap()
    )
    .unwrap();
    let mut ack = String::new();
    BufReader::new(c).read_line(&mut ack).unwrap();
    assert!(ack.contains("\"ok\""), "{ack}");
}

fn hello_of(r: Report) -> (u32, u32, u64, String) {
    match r {
        Report::Hello {
            player,
            proof,
            connections: _,
            ..
        } => (player, proof.colour, proof.counter, proof.chain),
        other => panic!("expected a hello, got {other:?}"),
    }
}

#[test]
fn the_ball_goes_round_and_drops_when_nobody_catches() {
    let (report, reports, _conns, _latest) = report_server();
    let (pa, pb) = (free_port(), free_port());
    let _a = player(0, pa, report);
    let mut b = Some(player(1, pb, report));
    let mut hellos = std::collections::BTreeMap::new();
    for _ in 0..2 {
        let (n, colour, counter, chain) = hello_of(next(&reports));
        assert_eq!((counter, chain.as_str()), (0, chain_hex(&[0; 32]).as_str()));
        assert!(colour < DEFAULT_COLOURS);
        hellos.insert(n, (colour, chain_from_hex(&chain).unwrap()));
    }
    let ball = Ball {
        round: 7,
        seq: 0,
        ring: vec![format!("127.0.0.1:{pa}"), format!("127.0.0.1:{pb}")],
        hold_ms: 300,
        shrink_permille: 0,
        min_hold_ms: 300,
        flight_ms: 300,
        trail: vec![],
    };
    send_ball(pa, &ball);
    // four catches, alternating, each the step of the catcher's last state
    let mut state = hellos.clone();
    let mut caught = 0;
    while caught < 4 {
        match next(&reports) {
            Report::Caught {
                player,
                round,
                seq,
                proof,
            } => {
                assert_eq!((round, seq), (7, caught));
                assert_eq!(player as u64, seq % 2, "the ring's order");
                let (colour, chain) = state[&player];
                let colour = (colour + 1) % DEFAULT_COLOURS;
                let chain = chain_step(&chain, round, seq, colour);
                assert_eq!(proof.colour, colour);
                assert_eq!(proof.counter, seq / 2 + 1);
                assert_eq!(proof.chain, chain_hex(&chain));
                state.insert(player, (colour, chain));
                caught += 1;
            }
            Report::Threw {
                player, attempts, ..
            } => {
                assert!(attempts >= 1);
                wake(if player == 0 { pa } else { pb });
            }
            Report::Hello { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    // player 1 goes away while player 0 holds the ball (a 300 ms hold): player 0's throw to it
    // drops once its flight window has passed
    loop {
        match next(&reports) {
            Report::Caught { player: 0, .. } => break,
            Report::Threw { player, .. } => wake(if player == 0 { pa } else { pb }),
            Report::Caught { .. } | Report::Hello { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    drop(b.take());
    // reports come on each player's own connection, so player 1's throw can be read after
    // player 0's catch
    let dropped = loop {
        match next(&reports) {
            Report::Dropped {
                player, attempts, ..
            } => break (player, attempts),
            Report::Threw { .. } | Report::Caught { .. } | Report::Hello { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(dropped.0, 0, "player 0 throws to the missing player 1");
    assert!(dropped.1 > 1, "it kept trying within the flight window");
}

#[test]
fn a_retired_report_connection_comes_back_with_the_state() {
    let (report, reports, conns, _latest) = report_server();
    let pa = free_port();
    let _a = player(3, pa, report);
    let (_, colour, _, _) = hello_of(next(&reports));
    let first = conns.recv_timeout(Duration::from_secs(10)).unwrap();
    // a catch, then the throw to nobody drops
    let ball = Ball {
        round: 2,
        seq: 5,
        ring: vec!["127.0.0.1:1".into(), format!("127.0.0.1:{pa}")],
        hold_ms: 0,
        shrink_permille: 0,
        min_hold_ms: 0,
        flight_ms: 50,
        trail: vec![],
    };
    send_ball(pa, &ball);
    let Report::Caught { proof, .. } = next(&reports) else {
        panic!("expected the catch")
    };
    assert_eq!(proof.colour, (colour + 1) % DEFAULT_COLOURS);
    assert!(matches!(next(&reports), Report::Dropped { .. }));
    // what a restore does to the connection the snapshot held
    first.shutdown(std::net::Shutdown::Both).unwrap();
    match next(&reports) {
        Report::Hello {
            player,
            proof: woke,
            connections,
            last_round,
            last_seq,
            ..
        } => {
            assert_eq!(player, 3);
            assert_eq!(connections, 2);
            assert_eq!(woke, proof, "the hello after a reconnect holds the state");
            assert_eq!((last_round, last_seq), (Some(2), Some(5)));
        }
        other => panic!("expected a hello, got {other:?}"),
    }
}

#[test]
fn a_probe_answers_with_the_state() {
    let (report, reports, _conns, _latest) = report_server();
    let pa = free_port();
    let _a = player(4, pa, report);
    let (_, colour, counter, chain) = hello_of(next(&reports));
    let mut c = connect(pa);
    writeln!(c, "{}", serde_json::to_string(&Inbound::Probe).unwrap()).unwrap();
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line).unwrap();
    match serde_json::from_str::<Report>(&line).unwrap() {
        Report::Probe { player, proof, .. } => {
            assert_eq!(player, Some(4));
            assert_eq!(
                (proof.colour, proof.counter, proof.chain),
                (colour, counter, chain)
            );
        }
        other => panic!("expected a probe answer, got {other:?}"),
    }
}

#[test]
fn an_unassigned_player_waits_for_its_number() {
    let (report, reports, _conns, _latest) = report_server();
    let pa = free_port();
    let _a = spawn(None, pa, report);
    let ball = Ball {
        round: 1,
        seq: 0,
        ring: vec![format!("127.0.0.1:{pa}")],
        hold_ms: 0,
        shrink_permille: 0,
        min_hold_ms: 0,
        flight_ms: 10,
        trail: vec![],
    };
    let refused = ask(pa, &Inbound::Ball(ball));
    assert!(refused.contains("not assigned"), "{refused}");
    let probe: Report = serde_json::from_str(&ask(pa, &Inbound::Probe)).unwrap();
    assert!(
        matches!(probe, Report::Probe { player: None, .. }),
        "{probe:?}"
    );
    assert!(
        reports.recv_timeout(Duration::from_millis(300)).is_err(),
        "an unassigned player reports nothing"
    );
    let ok = ask(
        pa,
        &Inbound::Assign {
            player: 9,
            colours: Some(5),
        },
    );
    assert!(ok.contains("\"ok\""), "{ok}");
    match next(&reports) {
        Report::Hello {
            player,
            colours,
            proof,
            connections,
            ..
        } => {
            assert_eq!((player, colours, connections), (9, 5, 1));
            assert_eq!(proof.counter, 0);
            assert!(proof.colour < 5);
        }
        other => panic!("expected a hello, got {other:?}"),
    }
    let again = ask(
        pa,
        &Inbound::Assign {
            player: 3,
            colours: None,
        },
    );
    assert!(again.contains("already assigned"), "{again}");
}

#[test]
fn a_pace_shortens_the_hold_of_its_round_only() {
    let (report, reports, _conns, _latest) = report_server();
    let (pa, sink) = (free_port(), TcpListener::bind("127.0.0.1:0").unwrap());
    let sink_port = sink.local_addr().unwrap().port();
    let (balls_tx, balls) = channel::<Ball>();
    thread::spawn(move || {
        for c in sink.incoming() {
            let mut c = c.unwrap();
            let mut line = String::new();
            BufReader::new(c.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            writeln!(c, "{{\"type\":\"ok\"}}").unwrap();
            if let Ok(Inbound::Ball(b)) = serde_json::from_str(&line) {
                let _ = balls_tx.send(b);
            }
        }
    });
    let _a = player(1, pa, report);
    let _ = next(&reports); // hello
    assert!(ask(
        pa,
        &Inbound::Pace {
            round: 4,
            hold_ms: 30
        }
    )
    .contains("ok"));
    let ball = |round| Ball {
        round,
        seq: 0,
        ring: vec![format!("127.0.0.1:{pa}"), format!("127.0.0.1:{sink_port}")],
        hold_ms: 3000,
        shrink_permille: 0,
        min_hold_ms: 10,
        flight_ms: 500,
        trail: vec![],
    };
    // round 4 holds 30 ms, not 3 s
    let started = std::time::Instant::now();
    send_ball(pa, &ball(4));
    assert!(matches!(next(&reports), Report::Caught { round: 4, .. }));
    assert!(matches!(next(&reports), Report::Threw { round: 4, .. }));
    let thrown = balls.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        thrown.hold_ms, 3000,
        "the pace caps the hold, the ball keeps its schedule"
    );
    wake(pa);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    // another round keeps its own hold
    let started = std::time::Instant::now();
    send_ball(pa, &ball(5));
    assert!(matches!(next(&reports), Report::Caught { round: 5, .. }));
    assert!(matches!(next(&reports), Report::Threw { round: 5, .. }));
    assert!(
        started.elapsed() >= Duration::from_millis(3000),
        "{:?}",
        started.elapsed()
    );
}

/// A connection that sends nothing (a probe abandoned while the player was waking) must not
/// hold up the ball, which arrives on a connection of its own.
#[test]
fn a_silent_connection_does_not_hold_up_the_ball() {
    let (report, reports, _conns, _latest) = report_server();
    let pa = free_port();
    let _a = player(2, pa, report);
    let _ = next(&reports); // hello
    let _silent = connect(pa);
    thread::sleep(Duration::from_millis(50));
    let ball = Ball {
        round: 1,
        seq: 0,
        ring: vec![format!("127.0.0.1:{pa}"), "127.0.0.1:1".into()],
        hold_ms: 0,
        shrink_permille: 0,
        min_hold_ms: 0,
        flight_ms: 10,
        trail: vec![],
    };
    let started = std::time::Instant::now();
    send_ball(pa, &ball);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the ball waited {:?} behind a silent connection",
        started.elapsed()
    );
}

/// A player that has thrown is going to sleep: it refuses the ball, and says so to a probe,
/// until the agent tells it it woke. A report connection that fails and comes back is not a
/// wake.
#[test]
fn a_player_that_has_thrown_refuses_the_ball_until_it_is_woken() {
    let (report, reports, conns, _latest) = report_server();
    let pa = free_port();
    let _a = player(5, pa, report);
    let _ = next(&reports); // hello
    let first = conns.recv_timeout(Duration::from_secs(10)).unwrap();
    let ball = |seq| Ball {
        round: 1,
        seq,
        ring: vec![format!("127.0.0.1:{pa}"), "127.0.0.1:1".into()],
        hold_ms: 0,
        shrink_permille: 0,
        min_hold_ms: 0,
        flight_ms: 20,
        trail: vec![],
    };
    let accepting =
        |port| match serde_json::from_str::<Report>(&ask(port, &Inbound::Probe)).unwrap() {
            Report::Probe { accepting, .. } => accepting,
            other => panic!("{other:?}"),
        };
    assert!(accepting(pa));
    send_ball(pa, &ball(0));
    assert!(matches!(next(&reports), Report::Caught { seq: 0, .. }));
    assert!(matches!(next(&reports), Report::Dropped { .. }));
    let refused = ask(pa, &Inbound::Ball(ball(2)));
    assert!(refused.contains("asleep"), "{refused}");
    assert!(!accepting(pa), "a probe says it does not catch yet");
    // the report connection failing and coming back is not a wake
    first.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(matches!(
        next(&reports),
        Report::Hello { connections: 2, .. }
    ));
    assert!(ask(pa, &Inbound::Ball(ball(2))).contains("asleep"));
    wake(pa);
    assert!(accepting(pa));
    send_ball(pa, &ball(2));
    assert!(matches!(next(&reports), Report::Caught { seq: 2, .. }));
}

/// A thrower that missed the acknowledgement sends the same ball again; the catch counts once.
#[test]
fn a_ball_sent_twice_is_caught_once() {
    let (report, reports, _conns, _latest) = report_server();
    let pa = free_port();
    let _a = player(6, pa, report);
    let _ = next(&reports); // hello
    let ball = Ball {
        round: 3,
        seq: 4,
        ring: vec![
            "127.0.0.1:1".into(),
            "127.0.0.1:1".into(),
            "127.0.0.1:1".into(),
            "127.0.0.1:1".into(),
            format!("127.0.0.1:{pa}"),
        ],
        hold_ms: 300,
        shrink_permille: 0,
        min_hold_ms: 0,
        flight_ms: 20,
        trail: vec![],
    };
    send_ball(pa, &ball);
    send_ball(pa, &ball);
    assert!(matches!(
        next(&reports),
        Report::Caught {
            seq: 4,
            proof: fleet_potato::Proof { counter: 1, .. },
            ..
        }
    ));
    assert!(
        matches!(next(&reports), Report::Dropped { .. }),
        "one throw, not two"
    );
    assert!(
        reports.recv_timeout(Duration::from_millis(500)).is_err(),
        "and nothing else"
    );
}

/// A round's end reaches a player holding the ball: its throw is abandoned, and it refuses
/// that round's balls from then on, so a snapshot taken now holds no throw to resume later.
#[test]
fn a_stopped_round_abandons_the_throw_and_refuses_its_balls() {
    let (report, reports, _conns, _latest) = report_server();
    let (pa, pb) = (free_port(), free_port());
    let _a = player(7, pa, report);
    let _b = player(8, pb, report);
    let _ = next(&reports);
    let _ = next(&reports);
    let ball = Ball {
        round: 9,
        seq: 0,
        ring: vec![format!("127.0.0.1:{pa}"), format!("127.0.0.1:{pb}")],
        hold_ms: 400,
        shrink_permille: 0,
        min_hold_ms: 0,
        flight_ms: 100,
        trail: vec![],
    };
    send_ball(pa, &ball);
    assert!(matches!(next(&reports), Report::Caught { player: 7, .. }));
    assert!(ask(pa, &Inbound::Stop { round: 9 }).contains("ok"));
    match next(&reports) {
        Report::Dropped {
            player,
            reason,
            attempts,
            ..
        } => {
            assert_eq!((player, attempts), (7, 0));
            assert!(reason.contains("round is over"), "{reason}");
        }
        other => panic!("expected the abandoned throw, got {other:?}"),
    }
    wake(pa);
    let refused = ask(
        pa,
        &Inbound::Ball(Ball {
            seq: 2,
            ..ball.clone()
        }),
    );
    assert!(refused.contains("round is over"), "{refused}");
    let ok = ask(
        pa,
        &Inbound::Ball(Ball {
            round: 10,
            seq: 0,
            hold_ms: 0,
            flight_ms: 10,
            ..ball
        }),
    );
    assert!(ok.contains("\"ok\""), "a later round is caught: {ok}");
}

/// The hello a new player sends is its state when it got its number, so the report of a ball
/// it catches at once comes after it, even when the report connection opens only after the
/// catch.
#[test]
fn the_first_hello_comes_before_the_first_catch() {
    let pa = free_port();
    let report = free_port();
    let _a = spawn(None, pa, report);
    assert!(ask(
        pa,
        &Inbound::Assign {
            player: 100,
            colours: None
        }
    )
    .contains("ok"));
    let ball = Ball {
        round: 1,
        seq: 0,
        ring: vec![format!("127.0.0.1:{pa}"), "127.0.0.1:1".into()],
        hold_ms: 0,
        shrink_permille: 0,
        min_hold_ms: 0,
        flight_ms: 10,
        trail: vec![],
    };
    send_ball(pa, &ball);
    // nothing listened for reports until now
    let (_, reports, _conns, _latest) = report_server_on(report);
    match next(&reports) {
        Report::Hello { proof, .. } => assert_eq!(proof.counter, 0),
        other => panic!("the first report is {other:?}"),
    }
    assert!(matches!(next(&reports), Report::Caught { .. }));
}
