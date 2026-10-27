//! The directed camera: a choreographed loop of flights and holds over named viewpoints.
//!
//! Each step holds at a viewpoint for `HOLD_SECS` (long enough for the path-traced image to
//! converge), then flies to the next. The default order is a permutation of the viewpoints
//! drawn from a seed, so the path is the same on every coordinator with that seed. Viewers
//! vote (`nudge`) among a few offered viewpoints for the next hold; the vote closes
//! `DECIDE_LEAD_SECS` before the hold ends, the winner (or the default when nobody voted)
//! becomes the next hold, and its keyframes are published so every renderer has them before
//! the flight starts. The vote never closes for a viewer: the moment one hold is decided, the
//! vote for the hold after it opens, so a nudge always counts for the first hold not yet
//! decided. Times are scene seconds; flight times are whole seconds so the timeline is exact.

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
    /// When the client first picked this target (Unix ms): of tied targets, the one picked
    /// first wins, so a later viewer cannot overturn an earlier pick on their own.
    #[serde(default)]
    pub at: u64,
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
    /// The viewpoints the open vote is among: for `next` while it is undecided, else for the
    /// hold after it.
    pub offer: Vec<usize>,
    /// The open vote, one per client address.
    pub votes: BTreeMap<String, Vote>,
    /// Scene time the open vote opened (the previous decision, or the director's start).
    #[serde(default)]
    pub vote_opened_t: f64,
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
            vote_opened_t: now_t,
            flying: false,
        };
        d.offer = d.make_offer(d.step, d.current);
        d
    }

    /// The offer for the hold that follows `from`, held at step `step`: the default (the next
    /// in the order) and the ones after it, never `from` itself.
    fn make_offer(&self, step: u64, from: usize) -> Vec<usize> {
        let n = self.order.len() as u64;
        let mut out = Vec::new();
        let mut k = 1;
        while out.len() < OFFER.min(self.order.len() - 1) {
            let v = self.order[((step + k) % n) as usize];
            if v != from && !out.contains(&v) {
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

    /// The hold the open vote decides follows this viewpoint, and the flight to it starts at
    /// this time: the current hold and its end while `next` is undecided, else `next` and
    /// the end of its hold.
    fn vote_from(&self) -> (usize, f64) {
        match (self.next, self.arrive_next_at()) {
            (Some(n), Some(at)) => (n, at + HOLD_SECS),
            _ => (self.current, self.hold_until()),
        }
    }

    /// Start the open vote again at scene time `now_t`, with no votes: what a coordinator
    /// does when it resumes a stored state, since the process that counted the votes is gone.
    pub fn restart_vote(&mut self, now_t: f64) {
        let (from, _) = self.vote_from();
        let step = if self.next.is_some() {
            self.step + 1
        } else {
            self.step
        };
        self.offer = self.make_offer(step, from);
        self.votes.clear();
        self.vote_opened_t = now_t;
    }

    /// When the open vote closes.
    pub fn vote_closes_at(&self) -> f64 {
        self.vote_from().1 - DECIDE_LEAD_SECS
    }

    /// When the camera reaches offered viewpoint `i` if it wins the open vote.
    pub fn arrive_offered_at(&self, i: usize) -> f64 {
        let vps = viewpoints();
        let (from, leave) = self.vote_from();
        leave + flight_secs(&vps[from], &vps[i])
    }

    /// The offered viewpoint that wins if the open vote closed now, and the nudge it stamps
    /// on its keyframes. Most votes wins; of tied targets, the one picked first; no votes:
    /// the default.
    pub fn leading(&self) -> (usize, Option<String>) {
        let mut best: Option<(usize, usize, u64, &str)> = None;
        for i in &self.offer {
            let picks: Vec<&Vote> = self.votes.values().filter(|v| v.target == *i).collect();
            let Some(first) = picks
                .iter()
                .min_by(|a, b| (a.at, &a.intent).cmp(&(b.at, &b.intent)))
            else {
                continue;
            };
            let better = match best {
                None => true,
                Some((_, c, at, intent)) => {
                    picks.len() > c
                        || (picks.len() == c && (first.at, first.intent.as_str()) < (at, intent))
                }
            };
            if better {
                best = Some((*i, picks.len(), first.at, first.intent.as_str()));
            }
        }
        match best {
            Some((i, _, _, intent)) => (i, Some(intent.to_string())),
            None => (self.offer[0], None),
        }
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

    /// Record a vote in the open vote; `Err` names why it does not count. A client's later
    /// vote replaces its earlier one; picking the same target again keeps its first time.
    pub fn vote(
        &mut self,
        client: &str,
        target: &str,
        intent: &str,
        at: u64,
    ) -> Result<(), String> {
        let vps = viewpoints();
        // A pick of the hold just decided (the offer moved on while it was on its way) is
        // already what happens: nothing to record.
        if self.next.is_some_and(|n| vps[n].name == target) {
            return Ok(());
        }
        let Some(i) = self.offer.iter().copied().find(|i| vps[*i].name == target) else {
            let names: Vec<&str> = self.offer.iter().map(|i| vps[*i].name).collect();
            return Err(format!(
                "{target:?} is not offered; the choices are {names:?}"
            ));
        };
        if self.votes.get(client).is_some_and(|v| v.target == i) {
            // The same pick again keeps its first time and intent, so its place in a tie.
            return Ok(());
        }
        self.votes.insert(
            client.to_string(),
            Vote {
                target: i,
                intent: intent.to_string(),
                at,
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
                let (pick, intent) = self.leading();
                self.next = Some(pick);
                self.next_intent = intent;
                // The vote for the hold after it opens now.
                self.votes.clear();
                self.offer = self.make_offer(self.step + 1, pick);
                self.vote_opened_t = now_t;
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

    #[test]
    fn a_director_stored_before_the_open_vote_fields_still_loads() {
        // The coordinator refuses to start on a stored state it cannot read.
        let mut v = serde_json::to_value(Director::new(3, 0.0)).unwrap();
        v.as_object_mut().unwrap().remove("vote_opened_t");
        v["votes"] = serde_json::json!({"192.0.2.1": {"target": v["offer"][0], "intent": "n1"}});
        let d: Director = serde_json::from_value(v).unwrap();
        assert_eq!(d.vote_opened_t, 0.0);
        assert_eq!(d.votes["192.0.2.1"].at, 0);
    }

    #[test]
    fn picking_the_same_stop_again_keeps_its_place_in_a_tie() {
        let mut d = Director::new(3, 0.0);
        let vps = viewpoints();
        let (t, u) = (d.offer[1], d.offer[2]);
        d.vote("192.0.2.1", vps[u].name, "n-b", 1000).unwrap();
        d.vote("192.0.2.2", vps[t].name, "n-a", 1000).unwrap();
        assert_eq!(d.leading().0, t, "same millisecond: the lower intent id");
        d.vote("192.0.2.1", vps[u].name, "n-0", 2000).unwrap();
        assert_eq!(d.leading().0, t, "picking again does not move a pick ahead");
    }

    #[test]
    fn the_vote_after_a_decision_opens_when_the_decision_is_made() {
        // A vote applied between the close and the tick that decides counts in the closing
        // vote, so the new one opens at the tick, not at the close.
        let mut d = Director::new(3, 0.0);
        d.tick(5.007);
        assert_eq!(d.vote_opened_t, 5.007);
    }
}
