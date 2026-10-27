//! The directed camera: a program of stops joined by flights that never stops moving.
//!
//! At a stop the camera makes a slow move (an orbit, a push-in, a crane, a tracking drift)
//! for its `stay_secs`; a flight then carries it to the next stop in one of a few styles (a
//! banking glide, a high arc over the cloud deck, a dive through the cloud layer, a skim low
//! over the ground). Every leg is sampled into keyframes one second apart, and each leg
//! starts with the velocity the previous one ended with, so the renderer's Catmull-Rom
//! spline (fleet/PROTOCOL.md, "Camera interpolation and holds") carries the motion through
//! with no stop and no jolt. No two consecutive keyframes are equal, so the path has no
//! holds.
//!
//! The time of day follows a day cycle by stop (golden hour, sunset, dusk, dawn, then the
//! day), and moves on only during flights, a step per coordinator tick; the night passes
//! within one flight.
//!
//! Which stop comes next: those visited longest ago first, shuffled a little by a draw from
//! the seed and the step, never one of the last few, so every stop comes round and the
//! sequence does not repeat in a short loop. Viewers vote among a few offered stops; the vote is always open, for the
//! first stop not yet decided. `DECIDE_LEAD_SECS` before a stop ends the open vote closes
//! and decides the next stop: its flight and keyframes are committed then, so every
//! renderer has them before the flight starts. Committed keyframes are never changed, so a
//! late vote or a renderer that gets the scene late never sees the path move under it.
//! Times are scene seconds; legs last whole seconds so the timeline is exact.

use crate::scene::Keyframe;
use crate::terrain::ground_h;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DECIDE_LEAD_SECS: f64 = 2.0;
pub const OFFER: usize = 3;
/// Keyframes kept from before the current stop, so the spline segment being drawn when the
/// camera arrives keeps its neighbours.
pub const HISTORY_SECS: f64 = 4.0;
/// How far above the ground or water a flight keeps the eye (the renderer clamps at 60 m).
pub const CLEARANCE_KM: f64 = 0.25;
/// The cumulus layer (fleet/renderer/README.md, "Clouds").
pub const CLOUD_BASE_KM: f64 = 2.7;
pub const CLOUD_TOP_KM: f64 = 5.6;
/// Stops not offered again until this many others have been visited.
pub const RECENT: usize = 6;
/// Visits remembered in order (about 20 minutes of the program).
pub const HISTORY_STOPS: usize = 48;

/// A stop. Positions are `[x, altitude, z]` km in the renderer's world. `pos`, `look` and
/// `fov_deg` are the pose at the middle of the stay; the slow move goes through it.
#[derive(Debug, Clone, PartialEq)]
pub struct Viewpoint {
    pub name: &'static str,
    /// What the page shows for it.
    pub label: &'static str,
    pub pos: [f64; 3],
    pub look: [f64; 3],
    pub fov_deg: f64,
    /// Degrees the camera circles the look point over the stay, counterclockwise seen from
    /// above.
    pub orbit_deg: f64,
    /// Fraction of the distance to the look point closed over the stay (negative pulls back).
    pub push: f64,
    /// Altitude gained over the stay.
    pub rise_km: f64,
    /// Moves camera and look point together over the stay: a tracking shot.
    pub drift: [f64; 3],
    /// Field of view change over the stay.
    pub zoom_deg: f64,
    /// Degrees the view turns about the camera over the stay (a pan), counterclockwise seen
    /// from above.
    pub pan_deg: f64,
    /// Whole seconds.
    pub stay_secs: f64,
}

/// The main peak of the terrain near the default flight (3.01 km), found by sampling the
/// renderer's `terrain_h` (fleet/renderer/src/world.h) on a 250 m grid.
pub const MAIN_PEAK: [f64; 3] = [-56.0, 3.01, -114.75];

/// The stops. Each keeps the eye at least 0.2 km above the ground under it through its whole
/// move (`every_stop_clears_the_ground`). The clock is drawn 12 km ahead of the camera,
/// above the horizon, so it sits at the image's centre column, on the fcvm/EC2 seam; the
/// `peak-*` stops look at the main peak, which puts it on the seam under the clock.
pub fn viewpoints() -> Vec<Viewpoint> {
    let v = |name, label, pos, look, fov_deg| Viewpoint {
        name,
        label,
        pos,
        look,
        fov_deg,
        orbit_deg: 0.0,
        push: 0.0,
        rise_km: 0.0,
        drift: [0.0; 3],
        zoom_deg: 0.0,
        pan_deg: 0.0,
        stay_secs: 10.0,
    };
    vec![
        Viewpoint {
            orbit_deg: 16.0,
            push: 0.12,
            ..v(
                "peak-seam",
                "The peak across the seam",
                [-47.0, 2.6, -101.0],
                [MAIN_PEAK[0], 2.75, MAIN_PEAK[2]],
                55.0,
            )
        },
        Viewpoint {
            drift: [-2.4, 0.0, 1.8],
            ..v(
                "ridges",
                "Across the ridges",
                [-36.0, 2.4, -70.0],
                [-73.5, 2.4, -121.75],
                60.0,
            )
        },
        Viewpoint {
            push: 0.24,
            rise_km: -0.15,
            ..v(
                "valley",
                "Into the valley",
                [-18.0, 1.8, -92.0],
                [-10.75, 2.6, -105.5],
                62.0,
            )
        },
        Viewpoint {
            drift: [-6.0, 0.5, -4.0],
            zoom_deg: -4.0,
            pan_deg: 32.0,
            stay_secs: 12.0,
            ..v(
                "horizon",
                "High above: the curve of the horizon",
                [-30.0, 38.0, -60.0],
                [-75.0, 28.0, -170.0],
                70.0,
            )
        },
        Viewpoint {
            drift: [0.0, 0.0, -3.0],
            ..v(
                "coast",
                "Across the bay to the mountains",
                [-118.5, 0.3, -89.5],
                [-104.5, 1.4, -89.0],
                60.0,
            )
        },
        Viewpoint {
            orbit_deg: 32.0,
            ..v(
                "peak-close",
                "Circling the peak",
                [-62.0, 3.5, -105.0],
                [MAIN_PEAK[0], 2.9, MAIN_PEAK[2]],
                50.0,
            )
        },
        Viewpoint {
            orbit_deg: -22.0,
            ..v(
                "high-lakes",
                "The high lakes",
                [-25.0, 2.4, -140.0],
                [-21.25, 2.3, -150.0],
                65.0,
            )
        },
        Viewpoint {
            orbit_deg: -20.0,
            rise_km: 0.2,
            ..v(
                "peak-west",
                "The peak from the west",
                [-66.0, 2.5, -101.0],
                [MAIN_PEAK[0], 2.75, MAIN_PEAK[2]],
                55.0,
            )
        },
        Viewpoint {
            push: 0.13,
            stay_secs: 11.0,
            ..v(
                "peak-lake",
                "The peak over the lake",
                [-67.5, 0.42, -114.5],
                [MAIN_PEAK[0], 2.2, MAIN_PEAK[2]],
                55.0,
            )
        },
        Viewpoint {
            drift: [-4.0, 0.0, -2.0],
            pan_deg: 24.0,
            stay_secs: 11.0,
            ..v(
                "above-clouds",
                "Above the cloud deck",
                [-60.0, 7.0, -80.0],
                [-78.0, 5.2, -94.0],
                62.0,
            )
        },
        Viewpoint {
            rise_km: 2.4,
            pan_deg: 20.0,
            stay_secs: 11.0,
            ..v(
                "cloud-climb",
                "Up through the clouds",
                [-45.0, 4.0, -118.0],
                [-62.0, 4.6, -124.0],
                62.0,
            )
        },
        Viewpoint {
            drift: [0.0, 0.1, 4.0],
            pan_deg: 12.0,
            ..v(
                "plains",
                "Across the plains to the mountains",
                [3.0, 0.6, -88.0],
                [-14.0, 1.4, -98.0],
                60.0,
            )
        },
        Viewpoint {
            drift: [-5.0, 0.0, -0.2],
            stay_secs: 9.0,
            ..v(
                "lake-skim",
                "Low over the western lakes",
                [-102.5, 0.3, -123.0],
                [-114.0, 0.35, -123.6],
                64.0,
            )
        },
        Viewpoint {
            drift: [0.0, 0.0, -4.0],
            ..v(
                "ridgeline",
                "Along the ridgeline",
                [-35.2, 3.0, -69.5],
                [-33.8, 2.3, -82.0],
                60.0,
            )
        },
        Viewpoint {
            drift: [0.0, 0.0, -3.0],
            rise_km: 0.3,
            stay_secs: 11.0,
            ..v(
                "western-sea",
                "Over the western sea",
                [-118.0, 1.0, -90.0],
                [-160.0, 0.6, -98.0],
                62.0,
            )
        },
    ]
}

/// The time of day at each stop of a cycle: the day comes round about every seven minutes,
/// slowest through golden hour and twilight. No stop is at night: the renderer draws the
/// ground black once the sun is 6 degrees down, so the night passes during the one flight
/// from the last dusk stop to the first dawn stop.
pub const DAY_CYCLE: [f64; 18] = [
    0.205, 0.215, 0.228, 0.245, 0.265, 0.300, 0.370, 0.460, 0.550, 0.620, 0.670, 0.710, 0.735,
    0.755, 0.770, 0.782, 0.790, 0.797,
];
/// The sky is dark and the renderer's ground black (the sun 6 degrees down and more) from
/// `DUSK_ENDS` to `DAWN_STARTS`.
pub const DUSK_ENDS: f64 = 0.815;
pub const DAWN_STARTS: f64 = 0.185;
/// The program starts in late afternoon.
pub const DAY_START: usize = 11;

/// The time of day at the stop held at `step`.
pub fn tod_of_step(step: u64) -> f64 {
    DAY_CYCLE[(step as usize + DAY_START) % DAY_CYCLE.len()]
}

/// How a flight gets from one stop to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flight {
    /// A banking curve at about the stops' height.
    Glide,
    /// Up over the cloud deck and down again.
    Arc,
    /// Up over the deck, then down through the cloud layer into a stop below it.
    Dive,
    /// Down low over the ground for most of the way.
    Skim,
}

impl Flight {
    pub fn name(self) -> &'static str {
        match self {
            Flight::Glide => "glide",
            Flight::Arc => "arc",
            Flight::Dive => "dive",
            Flight::Skim => "skim",
        }
    }
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn norm(a: [f64; 3]) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

/// splitmix64: the director's only source of chance, from the seed.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn draw(seed: u64, step: u64, salt: u64) -> u64 {
    let mut s =
        seed ^ step.wrapping_mul(0xD6E8_FEB8_6659_FD93) ^ salt.wrapping_mul(0xA076_1D64_78BD_642F);
    splitmix(&mut s);
    splitmix(&mut s)
}

/// The style of the flight into the stop held at `step`, from `a` to `b`.
pub fn flight_style(seed: u64, step: u64, a: &Viewpoint, b: &Viewpoint) -> Flight {
    let km = dist(a.pos, b.pos);
    let low = |v: &Viewpoint| v.pos[1] < 3.6;
    let high = a.pos[1].max(b.pos[1]) > 9.0;
    // a long leg low down would cross the terrain faster than the image can follow: it
    // goes over the clouds
    if km > 45.0 && !high {
        return if b.pos[1] < CLOUD_BASE_KM && draw(seed, step, 1).is_multiple_of(2) {
            Flight::Dive
        } else {
            Flight::Arc
        };
    }
    // a glide over 20 km climbs toward the cloud base (`flight_keyframes`)
    let mut options = vec![(Flight::Glide, 3)];
    if km >= 15.0 && !high {
        options.push((Flight::Arc, 3));
    }
    if km >= 15.0 && b.pos[1] < CLOUD_BASE_KM && a.pos[1] < 9.0 {
        options.push((Flight::Dive, 3));
    }
    if low(a) && low(b) && km <= 45.0 {
        options.push((Flight::Skim, 4));
    }
    let total: u64 = options.iter().map(|(_, w)| *w).sum();
    let mut r = draw(seed, step, 1) % total;
    for (f, w) in options {
        if r < w {
            return f;
        }
        r -= w;
    }
    Flight::Glide
}

/// Whole seconds from leaving `a` to arriving at `b`: the distance at the style's speed,
/// and long enough that turning from `a`'s view to `b`'s averages at most 10 degrees a
/// second.
pub fn flight_secs(a: &Viewpoint, b: &Viewpoint, style: Flight) -> f64 {
    let km = dist(a.pos, b.pos);
    let (lo, hi, speed): (f64, f64, f64) = match style {
        Flight::Glide => (8.0, 18.0, 2.5),
        Flight::Arc => (12.0, 24.0, 2.6),
        Flight::Dive => (12.0, 22.0, 2.4),
        Flight::Skim => (10.0, 24.0, 1.8),
    };
    // to or from high above, the climb or the descent sets the time
    let (lo, hi) = if a.pos[1].max(b.pos[1]) > 20.0 {
        (16.0, 24.0)
    } else {
        (lo, hi)
    };
    let (pa, la, _) = stop_pose(a, 1.0);
    let (pb, lb, _) = stop_pose(b, 0.0);
    let turn = wrap(yaw_of(sub(lb, pb)) - yaw_of(sub(la, pa))).abs();
    (km / speed)
        .max(turn / 10.0)
        .clamp(lo, hi.max(turn / 10.0))
        .round()
}

/// The pose at fraction `s` (0 to 1) of a stop's stay.
pub fn stop_pose(v: &Viewpoint, s: f64) -> ([f64; 3], [f64; 3], f64) {
    let c = s - 0.5;
    let rel = sub(v.pos, v.look);
    let a = (v.orbit_deg * c).to_radians();
    let (sa, ca) = a.sin_cos();
    // counterclockwise seen from above (+y): x toward -z
    let rel = [
        rel[0] * ca + rel[2] * sa,
        rel[1],
        -rel[0] * sa + rel[2] * ca,
    ];
    let rel = scale(rel, 1.0 - v.push * c);
    let d = scale(v.drift, c);
    let pos = add(add(v.look, rel), add(d, [0.0, v.rise_km * c, 0.0]));
    let look = sub(add(v.look, d), pos);
    let (sp, cp) = (v.pan_deg * c).to_radians().sin_cos();
    let look = [
        look[0] * cp + look[2] * sp,
        look[1],
        -look[0] * sp + look[2] * cp,
    ];
    (pos, add(pos, look), v.fov_deg + v.zoom_deg * c)
}

/// The velocity of a stop's camera and look point (km/s) at fraction `s` of its stay.
pub fn stop_velocity(v: &Viewpoint, s: f64) -> ([f64; 3], [f64; 3]) {
    let h = 1e-4;
    let (p0, l0, _) = stop_pose(v, s - h);
    let (p1, l1, _) = stop_pose(v, s + h);
    let k = 1.0 / (2.0 * h * v.stay_secs);
    (scale(sub(p1, p0), k), scale(sub(l1, l0), k))
}

fn key(t: f64, pos: [f64; 3], look: [f64; 3], fov_deg: f64, intent: &Option<String>) -> Keyframe {
    let r = |x: f64| (x * 1e6).round() / 1e6;
    Keyframe {
        t,
        pos: pos.map(r),
        look: look.map(r),
        fov_deg: r(fov_deg),
        intent: intent.clone(),
    }
}

/// A stop's keyframes from `arrive`, one a second through its stay, then the launch: one
/// second further along the stop's own velocity, the same whatever comes next, so the
/// spline segment ending the stay is fixed before the next stop is decided.
pub fn stop_keyframes(v: &Viewpoint, arrive: f64, intent: &Option<String>) -> Vec<Keyframe> {
    let n = v.stay_secs as usize;
    let mut out: Vec<Keyframe> = (0..=n)
        .map(|i| {
            let (p, l, f) = stop_pose(v, i as f64 / n as f64);
            key(arrive + i as f64, p, l, f, intent)
        })
        .collect();
    let (p, l, f) = stop_pose(v, 1.0);
    let (vp, vl) = stop_velocity(v, 1.0);
    out.push(key(
        arrive + n as f64 + 1.0,
        add(p, vp),
        add(l, vl),
        f + v.zoom_deg / v.stay_secs,
        intent,
    ));
    out
}

fn hermite(p0: [f64; 3], m0: [f64; 3], p1: [f64; 3], m1: [f64; 3], u: f64) -> [f64; 3] {
    let (u2, u3) = (u * u, u * u * u);
    let h00 = 2.0 * u3 - 3.0 * u2 + 1.0;
    let h10 = u3 - 2.0 * u2 + u;
    let h01 = -2.0 * u3 + 3.0 * u2;
    let h11 = u3 - u2;
    add(
        add(scale(p0, h00), scale(m0, h10)),
        add(scale(p1, h01), scale(m1, h11)),
    )
}

fn smooth(a: f64, b: f64, x: f64) -> f64 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 1 in the middle, falling to 0 with zero slope at both ends over `edge` of the span.
fn plateau(u: f64, edge: f64) -> f64 {
    smooth(0.0, edge, u) * smooth(1.0, 1.0 - edge, u)
}

/// A flight's keyframes: from the launch of `a` (one second after leaving it at `leave`)
/// to `b` at `arrive`, one a second, neither end included.
pub fn flight_keyframes(
    a: &Viewpoint,
    leave: f64,
    b: &Viewpoint,
    arrive: f64,
    style: Flight,
    seed: u64,
    step: u64,
) -> Vec<Keyframe> {
    let t0 = leave + 1.0;
    let d = arrive - t0;
    if d < 2.0 {
        return Vec::new();
    }
    let (pa, la, fa) = stop_pose(a, 1.0);
    let (va, vla) = stop_velocity(a, 1.0);
    let (p0, l0, f0) = (add(pa, va), add(la, vla), fa + a.zoom_deg / a.stay_secs);
    let (p1, l1, f1) = stop_pose(b, 0.0);
    let (v1, _) = stop_velocity(b, 0.0);

    // The path on a grid of eighths of a second.
    let per = 8usize;
    let n = (d as usize) * per;
    let us: Vec<f64> = (0..=n).map(|i| i as f64 / n as f64).collect();
    let base: Vec<[f64; 3]> = us
        .iter()
        .map(|&u| hermite(p0, scale(va, d), p1, scale(v1, d), u))
        .collect();
    let span = sub(p1, p0);
    let horiz = (span[0] * span[0] + span[2] * span[2]).sqrt().max(1e-6);
    let side = [-span[2] / horiz, 0.0, span[0] / horiz];
    let sign = if draw(seed, step, 2).is_multiple_of(2) {
        1.0
    } else {
        -1.0
    };
    let swing = match style {
        Flight::Glide => 0.18,
        _ => 0.06,
    } * horiz
        * sign;
    let top = p0[1].max(p1[1]).max(CLOUD_TOP_KM);
    let mut path: Vec<[f64; 3]> = Vec::with_capacity(n + 1);
    for (i, &u) in us.iter().enumerate() {
        let mut p = add(
            base[i],
            scale(side, swing * 16.0 * u * u * (1.0 - u) * (1.0 - u)),
        );
        match style {
            Flight::Glide => {
                // a long glide climbs to just under the cloud base, clear of the peaks
                let floor =
                    (CLOUD_BASE_KM - 0.2).min(p0[1].max(p1[1]) + 0.04 * (horiz - 20.0).max(0.0));
                p[1] += (floor - p[1]).max(0.0) * plateau(u, 0.35);
            }
            Flight::Arc => {
                let cruise = (top + 3.0).min(p0[1].max(p1[1]) + 0.12 * horiz);
                p[1] += (cruise - p[1]).max(0.0) * plateau(u, 0.4)
            }
            Flight::Dive => {
                let w = smooth(0.0, 0.3, u) * (1.0 - smooth(0.4, 1.0, u));
                let cruise = (top + 3.5)
                    .min(p0[1].max(p1[1]) + 0.15 * horiz)
                    .max(CLOUD_TOP_KM + 1.0);
                p[1] += (cruise - p[1]).max(0.0) * w;
            }
            Flight::Skim => {}
        }
        path.push(p);
    }
    let window: Vec<f64> = us.iter().map(|&u| plateau(u, (1.5 / d).min(0.3))).collect();
    if style == Flight::Skim {
        // Down toward the ground: a smoothed clearance profile under the path.
        let low: Vec<f64> = path.iter().map(|p| ground_h(p[0], p[2]) + 0.45).collect();
        let low = blur(&max_filter(&low, 2 * per), 2 * per);
        for (i, &u) in us.iter().enumerate() {
            let target = low[i];
            if target < path[i][1] {
                path[i][1] += (target - path[i][1]) * plateau(u, 0.3);
            }
        }
    }
    // Lift over whatever ground the path would come near.
    let deficit: Vec<f64> = path
        .iter()
        .map(|p| (ground_h(p[0], p[2]) + CLEARANCE_KM - p[1]).max(0.0))
        .collect();
    let lift = blur(&max_filter(&deficit, 5 * per / 4), 5 * per / 4);
    for i in 0..=n {
        path[i][1] += lift[i] * window[i];
    }

    let look_w = match style {
        Flight::Glide => 0.55,
        Flight::Arc => 0.65,
        Flight::Dive => 0.8,
        Flight::Skim => 0.75,
    };
    // The view turns from where the last stop looked to where the next one will, and leans
    // toward the direction of travel in between. Blending directions (not look points)
    // keeps it from whipping round when a look point passes near the camera.
    let unit = |v: [f64; 3]| scale(v, 1.0 / norm(v).max(1e-9));
    let (d0, d1) = (unit(sub(l0, p0)), unit(sub(l1, p1)));
    let (len0, len1) = (norm(sub(l0, p0)), norm(sub(l1, p1)));
    // the direction of travel, smoothed over a second either side and kept near level
    let fwd: Vec<[f64; 3]> = (0..=n)
        .map(|i| {
            let (a, b) = (i.saturating_sub(per), (i + per).min(n));
            let mut f = unit(sub(path[b], path[a]));
            f[1] = f[1].clamp(-0.4, 0.15) - 0.06;
            unit(f)
        })
        .collect();
    // yaw and pitch are blended apart, so a turn round to face the other way swings
    // across the horizon instead of over the top
    let (y0, y1) = (yaw_of(d0), yaw_of(d0) + wrap(yaw_of(d1) - yaw_of(d0)));
    let (q0, q1) = (pitch_of(d0), pitch_of(d1));
    let base: Vec<(f64, f64)> = us
        .iter()
        .map(|&u| {
            let s = smooth(0.0, 1.0, u);
            (y0 + (y1 - y0) * s, q0 + (q1 - q0) * s)
        })
        .collect();
    // lean toward the travel only on a flight that mostly heads where the camera looks
    let mid: Vec<usize> = (0..=n).filter(|i| (0.3..=0.7).contains(&us[*i])).collect();
    let mean_off = mid
        .iter()
        .map(|i| wrap(yaw_of(fwd[*i]) - base[*i].0).abs())
        .sum::<f64>()
        / mid.len().max(1) as f64;
    let lean = look_w * smooth(110.0, 50.0, mean_off);
    let mut out = Vec::new();
    for k in 1..(d as usize) {
        let i = k * per;
        let u = us[i];
        let s = smooth(0.0, 1.0, u);
        let (by, bq) = base[i];
        let off = wrap(yaw_of(fwd[i]) - by);
        // no lean toward a heading far behind the view (and none across the wrap at 180)
        let w = lean * plateau(u, 0.3) * smooth(150.0, 90.0, off.abs());
        let yaw = by + off * w;
        let pitch = bq + (pitch_of(fwd[i]) - bq) * w;
        let dir = from_yaw_pitch(yaw, pitch);
        let len = (len0 + (len1 - len0) * s).max(6.0);
        let look = add(path[i], scale(dir, len));
        let fov = f0 + (f1 - f0) * s + 8.0 * 16.0 * u * u * (1.0 - u) * (1.0 - u);
        out.push(key(t0 + k as f64, path[i], look, fov, &None));
    }
    out
}

/// Heading in degrees, 0 toward -z, 90 toward +x (the renderer's `drag` convention).
fn yaw_of(d: [f64; 3]) -> f64 {
    d[0].atan2(-d[2]).to_degrees()
}

fn pitch_of(d: [f64; 3]) -> f64 {
    let h = (d[0] * d[0] + d[2] * d[2]).sqrt();
    d[1].atan2(h).to_degrees()
}

fn from_yaw_pitch(yaw: f64, pitch: f64) -> [f64; 3] {
    let (y, p) = (yaw.to_radians(), pitch.to_radians());
    [p.cos() * y.sin(), p.sin(), -p.cos() * y.cos()]
}

/// An angle difference in (-180, 180].
fn wrap(a: f64) -> f64 {
    let r = a.rem_euclid(360.0);
    if r > 180.0 {
        r - 360.0
    } else {
        r
    }
}

fn max_filter(v: &[f64], r: usize) -> Vec<f64> {
    (0..v.len())
        .map(|i| {
            v[i.saturating_sub(r)..(i + r + 1).min(v.len())]
                .iter()
                .cloned()
                .fold(f64::MIN, f64::max)
        })
        .collect()
}

/// A triangle filter of half-width `r`.
fn blur(v: &[f64], r: usize) -> Vec<f64> {
    (0..v.len())
        .map(|i| {
            let lo = i.saturating_sub(r);
            let (mut s, mut w) = (0.0, 0.0);
            for (j, x) in v.iter().enumerate().take(i + r + 1).skip(lo) {
                let k = (r + 1 - i.abs_diff(j)) as f64;
                s += x * k;
                w += k;
            }
            s / w
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    pub target: usize,
    /// The nudge intent's id, stamped on the stop's keyframes when its target wins.
    pub intent: String,
    /// When the client first picked this target (Unix ms): of tied targets, the one picked
    /// first wins, so a later viewer cannot overturn an earlier pick on their own.
    #[serde(default)]
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Director {
    pub seed: u64,
    /// Steps taken; the current stop is `current`.
    pub step: u64,
    pub current: usize,
    /// Scene time the camera arrives at the current stop.
    pub arrive_t: f64,
    /// The next stop, once decided.
    pub next: Option<usize>,
    /// The nudge that chose `next`, if one did.
    pub next_intent: Option<String>,
    /// The viewpoints the open vote is among: for `next` while it is undecided, else for the
    /// stop after it.
    pub offer: Vec<usize>,
    /// The open vote, one per client address.
    pub votes: BTreeMap<String, Vote>,
    /// Scene time the open vote opened (the previous decision, or the director's start).
    #[serde(default)]
    pub vote_opened_t: f64,
    /// The flight to `next` has begun.
    pub flying: bool,
    /// Every stop visited, each once, the most recent last (`current` and a decided `next`
    /// included): the last `RECENT` are not offered again, and the rest come round in turn.
    #[serde(default)]
    pub recent: Vec<usize>,
    /// The stops visited in order, the last `HISTORY_STOPS`, so the default next stop never
    /// repeats a run of four stops flown in that time.
    #[serde(default)]
    pub history: Vec<usize>,
    /// The committed keyframes: from a little before the current stop through the decided
    /// next stop. Never changed once committed, only trimmed from the front.
    #[serde(default)]
    pub path: Vec<Keyframe>,
}

impl Director {
    pub fn new(seed: u64, now_t: f64) -> Director {
        let first = (0..viewpoints().len())
            .min_by_key(|i| draw(seed, 0, 3 + *i as u64))
            .unwrap_or(0);
        let mut d = Director {
            seed,
            current: first,
            step: 0,
            arrive_t: now_t,
            next: None,
            next_intent: None,
            offer: Vec::new(),
            votes: BTreeMap::new(),
            vote_opened_t: now_t,
            flying: false,
            recent: vec![first],
            history: vec![first],
            path: Vec::new(),
        };
        d.offer = d.make_offer(d.step, d.current);
        d.path = stop_keyframes(&viewpoints()[first], now_t, &None);
        d
    }

    /// The offer for the stop that follows `from`, held at step `step`: the stops not among
    /// the last `RECENT` visited, the ones visited
    /// longest ago (or never) first, each moved up to seven places by a draw from the seed,
    /// the step and `from`, so every stop comes round and the order still varies; a stop
    /// that would repeat a run of four in `history` goes after the others. The first is the
    /// default.
    fn make_offer(&self, step: u64, from: usize) -> Vec<usize> {
        let vps = viewpoints();
        // the stops that, after the last three, would fly a run of four flown before
        let n = self.history.len();
        let repeats: Vec<usize> = if n >= 3 {
            let tail = &self.history[n - 3..];
            (0..n - 3)
                .filter(|k| &self.history[*k..*k + 3] == tail)
                .map(|k| self.history[k + 3])
                .collect()
        } else {
            Vec::new()
        };
        let mut c: Vec<(usize, (bool, bool, i64, u64))> = (0..vps.len())
            .filter(|i| *i != from)
            .map(|i| {
                // stops since the last visit, newest 0; a stop never visited counts as long ago
                let age = self
                    .recent
                    .iter()
                    .rev()
                    .position(|r| *r == i)
                    .unwrap_or(2 * vps.len()) as i64;
                let h = draw(self.seed, step + 1, 10 + (i + 100 * from) as u64);
                let key = (
                    age < RECENT as i64,
                    repeats.contains(&i),
                    -(age + (h % 8) as i64),
                    h,
                );
                (i, key)
            })
            .collect();
        c.sort_by_key(|(_, k)| *k);
        c.into_iter().take(OFFER).map(|(i, _)| i).collect()
    }

    /// Record a visit: `recent` holds each stop once, newest last; `history` every visit.
    fn visit(&mut self, i: usize) {
        self.recent.retain(|r| *r != i);
        self.recent.push(i);
        self.history.push(i);
        if self.history.len() > HISTORY_STOPS {
            self.history.remove(0);
        }
    }

    pub fn hold_until(&self) -> f64 {
        self.arrive_t + viewpoints()[self.current].stay_secs
    }

    pub fn decide_at(&self) -> f64 {
        self.hold_until() - DECIDE_LEAD_SECS
    }

    fn flight_to(&self, step: u64, from: usize, to: usize) -> (Flight, f64) {
        let vps = viewpoints();
        let style = flight_style(self.seed, step, &vps[from], &vps[to]);
        (style, flight_secs(&vps[from], &vps[to], style))
    }

    pub fn arrive_next_at(&self) -> Option<f64> {
        self.next
            .map(|n| self.hold_until() + self.flight_to(self.step + 1, self.current, n).1)
    }

    /// The stop the open vote decides follows this viewpoint, held at this step, and the
    /// flight to it starts at this time: the current stop and its end while `next` is
    /// undecided, else `next` and the end of its stay.
    fn vote_from(&self) -> (usize, u64, f64) {
        match (self.next, self.arrive_next_at()) {
            (Some(n), Some(at)) => (n, self.step + 1, at + viewpoints()[n].stay_secs),
            _ => (self.current, self.step, self.hold_until()),
        }
    }

    /// Start the open vote again at scene time `now_t`, with no votes: what a coordinator
    /// does when it resumes a stored state, since the process that counted the votes is gone.
    pub fn restart_vote(&mut self, now_t: f64) {
        let (from, step, _) = self.vote_from();
        self.offer = self.make_offer(step, from);
        self.votes.clear();
        self.vote_opened_t = now_t;
    }

    /// When the open vote closes.
    pub fn vote_closes_at(&self) -> f64 {
        self.vote_from().2 - DECIDE_LEAD_SECS
    }

    /// When the camera reaches offered viewpoint `i` if it wins the open vote.
    pub fn arrive_offered_at(&self, i: usize) -> f64 {
        let (from, step, leave) = self.vote_from();
        leave + self.flight_to(step + 1, from, i).1
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

    /// The committed keyframes: the current stop and, once decided, the flight and the next
    /// stop.
    pub fn camera(&self) -> Vec<Keyframe> {
        self.path.clone()
    }

    /// The flight into the decided next stop, if any.
    pub fn next_flight(&self) -> Option<Flight> {
        self.next
            .map(|n| self.flight_to(self.step + 1, self.current, n).0)
    }

    /// The time of day at scene time `now_t`: the current stop's, moving on toward the next
    /// stop's (always forward through the day) while the camera flies there. Evenly, except
    /// through the night, which passes quickly in the middle of its flight.
    pub fn time_of_day(&self, now_t: f64) -> f64 {
        let here = tod_of_step(self.step);
        let tod = match self.arrive_next_at() {
            Some(at) if now_t >= self.hold_until() => {
                let u = ((now_t - self.hold_until()) / (at - self.hold_until())).clamp(0.0, 1.0);
                let ahead = (tod_of_step(self.step + 1) - here).rem_euclid(1.0);
                if ahead > 0.2 {
                    // through the night: the last of dusk over the first 35% of the
                    // flight, the dark middle over 30%, the start of dawn over the rest
                    let a = (DUSK_ENDS - here).max(0.0);
                    let c = (tod_of_step(self.step + 1) - DAWN_STARTS)
                        .rem_euclid(1.0)
                        .min(ahead - a);
                    let b = ahead - a - c;
                    let f = if u < 0.35 {
                        a * u / 0.35
                    } else if u < 0.65 {
                        a + b * (u - 0.35) / 0.3
                    } else {
                        a + b + c * (u - 0.65) / 0.35
                    };
                    (here + f).rem_euclid(1.0)
                } else {
                    (here + ahead * u).rem_euclid(1.0)
                }
            }
            _ => here,
        };
        (tod * 1e4).round() / 1e4
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
        // A pick of the stop just decided (the offer moved on while it was on its way) is
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

    /// Commit the flight to the decided next stop and the next stop's keyframes.
    fn commit_next(&mut self) {
        let vps = viewpoints();
        let (Some(n), Some(at)) = (self.next, self.arrive_next_at()) else {
            return;
        };
        let (style, _) = self.flight_to(self.step + 1, self.current, n);
        let (a, b) = (&vps[self.current], &vps[n]);
        self.path.extend(flight_keyframes(
            a,
            self.hold_until(),
            b,
            at,
            style,
            self.seed,
            self.step + 1,
        ));
        self.path.extend(stop_keyframes(b, at, &self.next_intent));
    }

    /// Pick up again at `now_t` after the path ran out (a coordinator that was away, or a
    /// director stored before the path was kept): the current stop is laid out so that its
    /// launch, the last keyframe renderers were holding, falls at `now_t`, so the vote closes
    /// at once and the flight to the next stop leaves from where the camera stands.
    fn restart_at(&mut self, now_t: f64) {
        let stay = viewpoints()[self.current].stay_secs;
        self.arrive_t = now_t - stay - 1.0;
        self.flying = false;
        self.path = stop_keyframes(&viewpoints()[self.current], self.arrive_t, &None);
        if self.next.is_some() {
            self.commit_next();
        }
    }

    /// Advance to scene time `now_t`.
    pub fn tick(&mut self, now_t: f64) {
        // A coordinator that was away past the end of the committed path picks up from where
        // the camera stands; so does a director stored before the path was kept.
        if (now_t > self.hold_until() + 60.0 && self.next.is_none()) || self.path.is_empty() {
            self.restart_at(now_t);
        }
        for _ in 0..4 {
            if self.next.is_none() && now_t >= self.decide_at() {
                let (pick, intent) = self.leading();
                self.next = Some(pick);
                self.next_intent = intent;
                self.visit(pick);
                self.commit_next();
                // The vote for the stop after it opens now.
                self.votes.clear();
                self.offer = self.make_offer(self.step + 1, pick);
                self.vote_opened_t = now_t;
            }
            if let Some(n) = self.next {
                if !self.flying && now_t >= self.hold_until() {
                    self.flying = true;
                }
                let at = self.arrive_next_at().unwrap();
                if now_t >= at {
                    self.step += 1;
                    self.current = n;
                    self.arrive_t = at;
                    self.next = None;
                    self.next_intent = None;
                    self.flying = false;
                    let keep = at - HISTORY_SECS;
                    let from = self.path.iter().position(|k| k.t >= keep).unwrap_or(0);
                    self.path.drain(..from);
                    continue;
                }
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;

    /// Run the director with no votes, a tick a second, for `secs`; returns the union of the
    /// keyframes it committed and, per tick, (t, time of day, camera as published).
    /// The union of the committed keyframes, the time of day per tick, and the camera as
    /// published at each tick.
    type Run = (Vec<Keyframe>, Vec<(f64, f64)>, Vec<(f64, Vec<Keyframe>)>);

    fn run(seed: u64, secs: u64) -> Run {
        let mut d = Director::new(seed, 0.0);
        let mut all: Vec<Keyframe> = Vec::new();
        let mut tods = Vec::new();
        let mut published = Vec::new();
        for s in 0..=secs {
            let t = s as f64;
            d.tick(t);
            for k in d.camera() {
                if all.last().is_none_or(|l| k.t > l.t) {
                    all.push(k);
                }
            }
            tods.push((t, d.time_of_day(t)));
            published.push((t, d.camera()));
        }
        (all, tods, published)
    }

    fn scene_of(camera: Vec<Keyframe>) -> Scene {
        let mut s = Scene::new(0);
        s.camera = camera;
        s
    }

    #[test]
    fn committed_keyframes_never_change() {
        let (_, _, published) = run(3, 900);
        let mut seen: BTreeMap<u64, Keyframe> = BTreeMap::new();
        for (_, cam) in &published {
            for k in cam {
                let id = (k.t * 1000.0).round() as u64;
                if let Some(old) = seen.get(&id) {
                    assert_eq!(old, k, "the keyframe at t={} changed", k.t);
                }
                seen.insert(id, k.clone());
            }
        }
    }

    #[test]
    fn the_next_leg_is_committed_ahead_of_the_segment_that_needs_it() {
        // A spline segment depends on the keyframes on both sides of it; the one being drawn
        // at t must have a keyframe after its end, published at least a second before t.
        let (_, _, published) = run(4, 900);
        for (t, cam) in &published {
            let last = cam.last().unwrap().t;
            let end = cam
                .iter()
                .map(|k| k.t)
                .find(|kt| *kt > *t + 1.0)
                .unwrap_or(f64::MAX);
            assert!(last > end, "at t={t}: the path ends at {last}");
        }
    }

    #[test]
    fn the_program_does_not_repeat_within_ten_minutes() {
        // On the hold-based director the order was one permutation of the viewpoints, so
        // the same stops came round in the same order about every two and a half minutes.
        for seed in 1..=8 {
            let mut d = Director::new(seed, 0.0);
            let mut stops: Vec<(f64, usize, Option<Flight>)> = vec![(0.0, d.current, None)];
            for s in 0..=3600 {
                let step = d.step;
                d.tick(s as f64);
                if d.step != step {
                    stops.push((d.arrive_t, d.current, None));
                }
                if let (Some(f), Some(l)) = (d.next_flight(), stops.last_mut()) {
                    l.2 = Some(f);
                }
            }
            // No stop comes back within RECENT stops, and every stop comes round.
            for (i, a) in stops.iter().enumerate() {
                for b in &stops[i + 1..(i + 1 + RECENT).min(stops.len())] {
                    assert_ne!(
                        a.1,
                        b.1,
                        "seed {seed}: {} at {} and again at {}",
                        viewpoints()[a.1].name,
                        a.0,
                        b.0
                    );
                }
            }
            let seen: std::collections::BTreeSet<usize> = stops.iter().map(|s| s.1).collect();
            assert_eq!(
                seen.len(),
                viewpoints().len(),
                "seed {seed}: every stop in an hour"
            );
            // No run of four stops and the flights between them recurs within ten minutes.
            let key = |w: &[(f64, usize, Option<Flight>)]| {
                w.iter().map(|s| (s.1, s.2)).collect::<Vec<_>>()
            };
            for i in 0..stops.len() {
                for j in i + 4..stops.len().saturating_sub(3) {
                    if stops[j].0 - stops[i].0 <= 600.0 {
                        assert_ne!(
                            key(&stops[i..i + 4]),
                            key(&stops[j..j + 4]),
                            "seed {seed}: stops {i} and {j} start the same run"
                        );
                    }
                }
            }
            let styles: std::collections::BTreeSet<&str> =
                stops.iter().filter_map(|s| s.2.map(|f| f.name())).collect();
            assert_eq!(
                styles.len(),
                4,
                "seed {seed}: every flight style flies in an hour: {styles:?}"
            );
        }
    }

    #[test]
    fn a_coordinator_back_from_a_pause_flies_on_from_where_the_camera_was() {
        // Away before the vote closed: renderers hold the last committed keyframe (the
        // stop's launch). Restarting the stop from its beginning moved the camera 4.4 km in
        // one frame on seed 1; it flies on from the held pose instead.
        let mut d = Director::new(1, 0.0);
        d.tick(5.0);
        let held = d.camera().last().unwrap().clone();
        let back = d.hold_until() + 61.0;
        d.tick(back);
        let s = scene_of(d.camera());
        let now = s.camera_at(back);
        assert!(
            dist(now.pos, held.pos) < 1e-6,
            "jumped {:.3} km",
            dist(now.pos, held.pos)
        );
        assert!(dist(now.look, held.look) < 1e-6);
        let later = s.camera_at(back + 1.0);
        assert!(dist(later.pos, now.pos) > 0.01, "and moves on");
        assert!(d.next.is_some());
    }

    #[test]
    fn every_stop_clears_the_ground() {
        for v in viewpoints() {
            for i in 0..=20 {
                let (p, _, _) = stop_pose(&v, i as f64 / 20.0);
                let g = ground_h(p[0], p[2]);
                assert!(
                    p[1] - g >= 0.2,
                    "{} at {i}/20: eye {:.3} over ground {g:.3}",
                    v.name,
                    p[1]
                );
            }
        }
    }

    #[test]
    fn the_path_clears_the_ground() {
        let (all, _, _) = run(5, 1800);
        let s = scene_of(all);
        let mut t = 1.0;
        while t < 1790.0 {
            let p = s.camera_at(t).pos;
            let g = ground_h(p[0], p[2]);
            assert!(
                p[1] - g >= 0.1,
                "at t={t:.1}: eye {:.3} over ground {g:.3}",
                p[1]
            );
            t += 0.1;
        }
    }

    #[test]
    fn the_day_comes_round_and_moves_only_in_flight() {
        let (_, tods, _) = run(6, 1200);
        // the sun high, golden hour (under 15 degrees up) and twilight all show
        let shows = |lo: f64, hi: f64| tods.iter().any(|(_, t)| (lo..hi).contains(t));
        assert!(shows(0.3, 0.7) && shows(0.73, 0.78) && shows(0.78, 0.81) && shows(0.19, 0.27));
        assert!(
            tods.iter().any(|(_, t)| *t > 0.9 || *t < 0.1),
            "the night passes"
        );
        for w in tods.windows(2) {
            let step = (w[1].1 - w[0].1).rem_euclid(1.0);
            // in twilight and day the light moves in small steps; deep night shows nothing
            let night = |t: f64| !(DAWN_STARTS..=DUSK_ENDS).contains(&t);
            let day = |t: f64| (0.27..=0.73).contains(&t);
            let limit = if night(w[0].1) || night(w[1].1) {
                0.15
            } else if day(w[0].1) && day(w[1].1) {
                0.015
            } else {
                0.006
            };
            assert!(
                step < limit,
                "the time of day jumped {step} at t={}",
                w[1].0
            );
        }
    }

    #[test]
    fn a_director_stored_before_the_open_vote_fields_still_loads() {
        // The coordinator refuses to start on a stored state it cannot read.
        let mut v = serde_json::to_value(Director::new(3, 0.0)).unwrap();
        v.as_object_mut().unwrap().remove("vote_opened_t");
        v.as_object_mut().unwrap().remove("path");
        v.as_object_mut().unwrap().remove("recent");
        v.as_object_mut().unwrap().remove("history");
        v["order"] = serde_json::json!([0, 1, 2]);
        v["votes"] = serde_json::json!({"192.0.2.1": {"target": v["offer"][0], "intent": "n1"}});
        let mut d: Director = serde_json::from_value(v).unwrap();
        assert_eq!(d.vote_opened_t, 0.0);
        assert_eq!(d.votes["192.0.2.1"].at, 0);
        d.tick(1.0);
        assert!(!d.camera().is_empty());
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
        let at = d.decide_at() + 0.007;
        d.tick(at);
        assert!(d.next.is_some());
        assert_eq!(d.vote_opened_t, at);
    }
}
