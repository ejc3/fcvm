//! Print how large the coordinator's snapshot is with N Minecraft servers on one host, and how
//! long one encode of it takes (docs/fleet/minecraft.md, "Many servers on one host").
//!
//!     cargo run --release -p fleet-core --example snapshot_size -- 8 64 256 512

use fleet_core::{apply, view, Config, Event, State};
use serde_json::json;
use std::time::Instant;

fn state(n: u64) -> State {
    let mut events = vec![json!({"type": "agent", "at": 1000, "host": "h1", "msg": {
        "type": "hello", "host": "h1", "arch": "aarch64", "gpus": [], "capacity": 0,
        "minecraft": n, "minecraft_server": "Pumpkin 3ea7d281 (Java Edition 26.3)"}})];
    for k in 1..=n {
        events.push(
            json!({"type": "agent", "at": 2000 + k, "host": "h1", "msg": {
            "type": "slot", "id": format!("mc-{k}"), "phase": "running", "generation": 1,
            "vm": format!("fleet-mc-{k}"),
            "frame_url": format!("ws://[2001:db8:1234:5678:9abc::{k:x}]:8080/view"),
            "ipv6": format!("2001:db8:1234:5678:9abc::{k:x}"),
            "timings": {"restore_ms": 410, "first_chunk_ms": 650, "total_ms": 650},
            "first_chunk_at": 2000 + k}}),
        );
    }
    let events: Vec<Event> = serde_json::from_value(json!(events)).expect("events parse");
    events.iter().fold(State::new(Config::default(), 0), apply)
}

fn main() {
    let counts: Vec<u64> = std::env::args()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    println!("servers\tsnapshot_bytes\tminecraft_bytes\tper_server_bytes\tencode_ms_median");
    for n in if counts.is_empty() {
        vec![8, 64, 256, 512]
    } else {
        counts
    } {
        let s = state(n);
        let v = view(&s);
        let whole = serde_json::to_string(&v).expect("the view serializes");
        let mc = serde_json::to_string(&v.minecraft)
            .expect("the view serializes")
            .len();
        let mut times: Vec<f64> = (0..201)
            .map(|_| {
                let t = Instant::now();
                let text = serde_json::to_string(&view(&s)).expect("the view serializes");
                std::hint::black_box(text);
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(|a, b| a.total_cmp(b));
        println!(
            "{n}\t{}\t{mc}\t{}\t{:.3}",
            whole.len(),
            mc / n.max(1) as usize,
            times[100]
        );
    }
}
