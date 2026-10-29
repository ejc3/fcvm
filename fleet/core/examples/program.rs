//! Print the directed camera's program with no votes, as the coordinator would fly it:
//! every keyframe it commits, the time of day at each tick, and each stop with the flight
//! into it. For rendering a run offline (fleet/renderer/tools/pathrun.py, contact sheets).
//!
//!     cargo run -p fleet-core --example program -- SEED SECONDS > program.json

use fleet_core::director::{viewpoints, Director};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seed: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600);
    let vps = viewpoints();
    let mut d = Director::new(seed, 0.0);
    let mut camera: Vec<fleet_core::scene::Keyframe> = Vec::new();
    let mut tod = Vec::new();
    let mut stops = vec![json!({"t": 0.0, "stop": vps[d.current].name})];
    for s in 0..=secs {
        let t = s as f64;
        let step = d.step;
        d.tick(t);
        if d.step != step {
            stops.push(json!({"t": d.arrive_t, "stop": vps[d.current].name}));
        }
        if let Some(f) = d.next_flight() {
            let n = stops.len();
            stops[n - 1]["flight_out"] = json!(f.name());
        }
        for k in d.camera() {
            if camera.last().is_none_or(|l| k.t > l.t) {
                camera.push(k);
            }
        }
        tod.push(json!([t, d.time_of_day(t)]));
    }
    let out =
        json!({"seed": seed, "secs": secs, "camera": camera, "time_of_day": tod, "stops": stops});
    println!("{}", serde_json::to_string(&out).unwrap());
}
