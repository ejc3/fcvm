//! The directed camera: a choreographed loop of flights and holds over named viewpoints.
//!
//! Each step holds at a viewpoint for `HOLD_SECS` (long enough for the path-traced image to
//! converge), then flies to the next. The default order is a permutation of the viewpoints
//! drawn from a seed, so the path is the same on every coordinator with that seed. Viewers
//! vote (`nudge`) among a few offered viewpoints for the next hold; the vote closes
//! `DECIDE_LEAD_SECS` before the hold ends, the winner (or the default when nobody voted)
//! becomes the next hold, and its keyframes are published so every renderer has them before
//! the flight starts. Times are scene seconds; flight times are whole seconds so the
//! timeline is exact.

use crate::scene::Keyframe;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HOLD_SECS: f64 = 7.0;
pub const DECIDE_LEAD_SECS: f64 = 2.0;
pub const OFFER: usize = 3;

/// A place the camera holds. Positions are `[x, altitude, z]` km in the renderer's world;
/// `via` is a waypoint the flight into this viewpoint passes through.
#[derive(Debug, Clone, PartialEq)]
pub struct Viewpoint {
    pub name: &'static str,
    /// What the page shows for it.
    pub label: &'static str,
    pub pos: [f64; 3],
    pub look: [f64; 3],
    pub fov_deg: f64,
    pub time_of_day: f64,
    pub via: Option<[f64; 3]>,
}

/// The main peak of the terrain near the default flight (3.01 km), found by sampling the
/// renderer's `terrain_h` (fleet/renderer/src/world.h) on a 250 m grid.
pub const MAIN_PEAK: [f64; 3] = [-56.0, 3.01, -114.75];

/// The viewpoints, each checked against the renderer's terrain height (the eye at least
/// 0.3 km above the ground under it). The clock is drawn 12 km ahead of the camera, above the horizon, so it
/// sits at the image's centre column, on the fcvm/EC2 seam; the `peak-*` holds look straight
/// at the main peak, which puts it on the seam under the clock.
pub fn viewpoints() -> Vec<Viewpoint> {
    vec![
        Viewpoint {
            name: "peak-seam",
            label: "The peak across the seam",
            pos: [-47.0, 2.6, -101.0],
            look: [MAIN_PEAK[0], 2.75, MAIN_PEAK[2]],
            fov_deg: 55.0,
            time_of_day: 0.40,
            via: None,
        },
        Viewpoint {
            name: "sunset-ridge",
            label: "Sunset over the ridge",
            pos: [-36.0, 2.4, -70.0],
            look: [-73.5, 2.4, -121.75],
            fov_deg: 60.0,
            time_of_day: 0.735,
            via: None,
        },
        Viewpoint {
            name: "dawn-valley",
            label: "Dawn in the valley",
            pos: [-18.0, 1.8, -92.0],
            look: [-10.75, 2.6, -105.5],
            fov_deg: 62.0,
            time_of_day: 0.265,
            via: None,
        },
        Viewpoint {
            name: "horizon",
            label: "High above: the curve of the horizon",
            pos: [-30.0, 38.0, -60.0],
            look: [-75.0, 28.0, -170.0],
            fov_deg: 70.0,
            time_of_day: 0.45,
            via: Some([-40.0, 20.0, -80.0]),
        },
        Viewpoint {
            name: "coast",
            label: "From the coast",
            pos: [-100.0, 0.35, -90.0],
            look: [-63.5, 2.2, -87.0],
            fov_deg: 60.0,
            time_of_day: 0.33,
            via: None,
        },
        Viewpoint {
            name: "peak-close",
            label: "Close to the peak",
            pos: [-62.0, 3.5, -105.0],
            look: [MAIN_PEAK[0], 2.9, MAIN_PEAK[2]],
            fov_deg: 50.0,
            time_of_day: 0.70,
            via: None,
        },
        Viewpoint {
            name: "high-lakes",
            label: "The high lakes",
            pos: [-25.0, 2.4, -140.0],
            look: [-21.25, 2.3, -150.0],
            fov_deg: 65.0,
            time_of_day: 0.52,
            via: None,
        },
        Viewpoint {
            name: "peak-seam-low-sun",
            label: "The peak at a low sun",
            pos: [-66.0, 2.5, -101.0],
            look: [MAIN_PEAK[0], 2.75, MAIN_PEAK[2]],
            fov_deg: 55.0,
            time_of_day: 0.255,
            via: None,
        },
    ]
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Whole seconds to fly from `a` to `b` (through `b.via` when it has one).
pub fn flight_secs(a: &Viewpoint, b: &Viewpoint) -> f64 {
    let km = match b.via {
        Some(v) => dist(a.pos, v) + dist(v, b.pos),
        None => dist(a.pos, b.pos),
    };
    let (lo, hi, speed) = if b.via.is_some() {
        (14.0, 24.0, 4.0)
    } else {
        (8.0, 18.0, 2.5)
    };
    (km / speed).clamp(lo, hi).round()
}

/// splitmix64: the director's only source of order, from the seed.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The default order of the viewpoints for a seed (Fisher-Yates).
pub fn order(seed: u64, n: usize) -> Vec<usize> {
    let mut o: Vec<usize> = (0..n).collect();
    let mut s = seed;
    for i in (1..n).rev() {
        let j = (splitmix(&mut s) % (i as u64 + 1)) as usize;
        o.swap(i, j);
    }
    o
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    pub target: usize,
    /// The nudge intent's id, stamped on the hold's keyframes when its target wins.
    pub intent: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Director {
    pub seed: u64,
    pub order: Vec<usize>,
    /// Steps taken; the current hold is `current`.
    pub step: u64,
    pub current: usize,
    /// Scene time the camera arrives at the current hold.
    pub arrive_t: f64,
    /// The next hold, once decided.
    pub next: Option<usize>,
    /// The nudge that chose `next`, if one did.
    pub next_intent: Option<String>,
    pub offer: Vec<usize>,
    /// One vote per client address.
    pub votes: BTreeMap<String, Vote>,
    /// The flight to `next` has begun (its time of day is in the scene).
    pub flying: bool,
}

fn hold(v: &Viewpoint, t: f64, intent: Option<String>) -> [Keyframe; 2] {
    let k = Keyframe {
        t,
        pos: v.pos,
        look: v.look,
        fov_deg: v.fov_deg,
        intent,
    };
    [
        k.clone(),
        Keyframe {
            t: t + HOLD_SECS,
            ..k
        },
    ]
}

impl Director {
    pub fn new(seed: u64, now_t: f64) -> Director {
        let vps = viewpoints();
        let order = order(seed, vps.len());
        let mut d = Director {
            seed,
            current: order[0],
            order,
            step: 0,
            arrive_t: now_t,
            next: None,
            next_intent: None,
            offer: Vec::new(),
            votes: BTreeMap::new(),
            flying: false,
        };
        d.offer = d.make_offer();
        d
    }

    /// The default next hold and the next ones in the order, never the current one.
    fn make_offer(&self) -> Vec<usize> {
        let n = self.order.len() as u64;
        let mut out = Vec::new();
        let mut k = 1;
        while out.len() < OFFER.min(self.order.len() - 1) {
            let v = self.order[((self.step + k) % n) as usize];
            if v != self.current && !out.contains(&v) {
                out.push(v);
            }
            k += 1;
        }
        out
    }

    pub fn hold_until(&self) -> f64 {
        self.arrive_t + HOLD_SECS
    }

    pub fn decide_at(&self) -> f64 {
        self.hold_until() - DECIDE_LEAD_SECS
    }

    pub fn arrive_next_at(&self) -> Option<f64> {
        let vps = viewpoints();
        self.next
            .map(|n| self.hold_until() + flight_secs(&vps[self.current], &vps[n]))
    }

    /// The keyframes for the current hold and, once decided, the flight and the next hold.
    pub fn camera(&self) -> Vec<Keyframe> {
        let vps = viewpoints();
        let mut k: Vec<Keyframe> = hold(&vps[self.current], self.arrive_t, None).to_vec();
        if let (Some(n), Some(at)) = (self.next, self.arrive_next_at()) {
            if let Some(via) = vps[n].via {
                let mid = (self.hold_until() + at) / 2.0;
                k.push(Keyframe {
                    t: mid,
                    pos: via,
                    look: vps[n].look,
                    fov_deg: (vps[self.current].fov_deg + vps[n].fov_deg) / 2.0,
                    intent: None,
                });
            }
            k.extend(hold(&vps[n], at, self.next_intent.clone()));
        }
        k
    }

    /// Record a vote; `Err` names why it does not count.
    pub fn vote(&mut self, client: &str, target: &str, intent: &str) -> Result<(), String> {
        if self.next.is_some() {
            return Err("the next hold is already chosen; vote again during it".into());
        }
        let vps = viewpoints();
        let Some(i) = self.offer.iter().copied().find(|i| vps[*i].name == target) else {
            let names: Vec<&str> = self.offer.iter().map(|i| vps[*i].name).collect();
            return Err(format!(
                "{target:?} is not offered; the choices are {names:?}"
            ));
        };
        self.votes.insert(
            client.to_string(),
            Vote {
                target: i,
                intent: intent.to_string(),
            },
        );
        Ok(())
    }

    /// Votes per offered viewpoint.
    pub fn tally(&self) -> BTreeMap<String, usize> {
        let vps = viewpoints();
        let mut t: BTreeMap<String, usize> = self
            .offer
            .iter()
            .map(|i| (vps[*i].name.to_string(), 0))
            .collect();
        for v in self.votes.values() {
            *t.entry(vps[v.target].name.to_string()).or_default() += 1;
        }
        t
    }

    /// Advance to scene time `now_t`. Returns the time of day to set when a flight starts.
    pub fn tick(&mut self, now_t: f64) -> Option<f64> {
        let vps = viewpoints();
        let mut tod = None;
        // A coordinator that was away for longer than a whole step starts a fresh hold.
        if now_t > self.hold_until() + 60.0 && self.next.is_none() {
            self.arrive_t = now_t;
        }
        for _ in 0..4 {
            if self.next.is_none() && now_t >= self.decide_at() {
                // Most votes wins; ties go to the earlier offered; no votes: the default.
                let tally: Vec<(usize, usize)> = self
                    .offer
                    .iter()
                    .map(|i| (*i, self.votes.values().filter(|v| v.target == *i).count()))
                    .collect();
                let best = tally.iter().map(|(_, c)| *c).max().unwrap_or(0);
                let (pick, intent) = if best == 0 {
                    (self.offer[0], None)
                } else {
                    let pick = tally.iter().find(|(_, c)| *c == best).unwrap().0;
                    let first = self
                        .votes
                        .values()
                        .filter(|v| v.target == pick)
                        .map(|v| v.intent.clone())
                        .min();
                    (pick, first)
                };
                self.next = Some(pick);
                self.next_intent = intent;
            }
            if let Some(n) = self.next {
                if !self.flying && now_t >= self.hold_until() {
                    self.flying = true;
                    tod = Some(vps[n].time_of_day);
                }
                let at = self.arrive_next_at().unwrap();
                if now_t >= at {
                    self.step += 1;
                    self.current = n;
                    self.arrive_t = at;
                    self.next = None;
                    self.next_intent = None;
                    self.flying = false;
                    self.votes.clear();
                    self.offer = self.make_offer();
                    continue;
                }
            }
            break;
        }
        tod
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_order_is_a_permutation_fixed_by_the_seed() {
        let n = viewpoints().len();
        let a = order(7, n);
        let mut s = a.clone();
        s.sort();
        assert_eq!(s, (0..n).collect::<Vec<_>>());
        assert_eq!(a, order(7, n));
        assert_ne!(order(7, n), order(8, n));
    }

    #[test]
    fn a_loop_holds_long_enough_and_lasts_minutes() {
        let vps = viewpoints();
        let o = order(1, vps.len());
        let mut total = 0.0;
        for i in 0..o.len() {
            let (a, b) = (&vps[o[i]], &vps[o[(i + 1) % o.len()]]);
            let f = flight_secs(a, b);
            assert_eq!(f, f.round());
            assert!((8.0..=24.0).contains(&f));
            total += HOLD_SECS + f;
        }
        assert!(total >= 120.0, "a loop of {total} s");
        assert!(
            vps.iter().any(|v| v.pos[1] > 20.0),
            "a high leg shows the horizon"
        );
        assert!(
            vps.iter()
                .filter(|v| v.time_of_day < 0.28 || v.time_of_day > 0.72)
                .count()
                >= 2,
            "low sun"
        );
        assert!(
            vps.iter()
                .filter(|v| v.look[0] == MAIN_PEAK[0] && v.look[2] == MAIN_PEAK[2])
                .count()
                >= 2
        );
    }

    #[test]
    fn the_director_runs_the_default_loop_without_votes() {
        let mut d = Director::new(3, 0.0);
        let first = d.current;
        let mut seen = vec![first];
        let mut t = 0.0;
        while seen.len() < viewpoints().len() + 1 {
            t += 0.5;
            d.tick(t);
            if *seen.last().unwrap() != d.current {
                seen.push(d.current);
            }
        }
        assert_eq!(seen[1..viewpoints().len()].len(), viewpoints().len() - 1);
        assert_eq!(
            seen[viewpoints().len()],
            first,
            "the loop comes back to its start"
        );
    }
}
