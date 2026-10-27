//! The autopilot of a viewer's own GPU: a looping flight its camera flies from the first
//! frame until the viewer pauses it or takes the camera.
//!
//! The loop is a periodic uniform cubic B-spline through `CONTROL`, so position, velocity
//! and acceleration are continuous everywhere, the loop point included. It starts low over
//! the long lake west of the main peak, pops up the ridge face at its north end, runs along
//! the peak's west flank looking at it, climbs north-east up through the cloud layer to 7
//! km, turns over the top heading south, dives back down through the clouds, skims the
//! water to the south, hops the hills west of it and banks north onto the lake again. The
//! eye stays more than 150 m above the renderer's surface (`surface_h`, 9 octaves), clear
//! of the renderer's own 60 m clamp, which would otherwise move the eye off the path:
//! fleet/renderer/tests/test_host.cpp checks every quarter second of the lap in
//! `tests/autopilot_lap.json`, which `tests/autopilot.rs` keeps equal to this code.
//!
//! The renderer follows keyframes, not this function: the claim's camera holds keyframes
//! sampled every `KEY_SECS` on a grid fixed by the flight's start (`flight_t0`), appended
//! in batches as time passes (`extend`), so a renderer is re-posted a scene every
//! `AHEAD_SECS - AHEAD_MIN_SECS` seconds and the frame being drawn always lies at least two
//! keyframes before the last one, where appending more cannot change it.

use crate::scene::{Keyframe, Scene};
use crate::state::Claim;

/// One lap of the loop, scene seconds.
pub const LOOP_SECS: f64 = 56.0;
/// Spacing of the keyframes the renderer is given.
pub const KEY_SECS: f64 = 1.0;
/// The path is extended once it ends sooner than this after now...
pub const AHEAD_MIN_SECS: f64 = 8.0;
/// ...to this far after now.
pub const AHEAD_SECS: f64 = 16.0;
/// The flight leaves its start pose this long after the first frame is reported, so the
/// keyframes reach the renderer before the camera starts to move.
pub const START_LEAD_SECS: f64 = 0.5;
/// Pausing brings the camera to rest over this long, along the path it was flying.
pub const PAUSE_SECS: f64 = 1.0;
/// A viewer's drag takes the camera from the autopilot over this long.
pub const TAKEOVER_SECS: f64 = 0.5;
/// Resuming flies back to the loop at about this speed (km/s), in 2 to 8 s.
const JOIN_KM_PER_S: f64 = 1.2;

/// `[x, altitude, z, yaw offset deg, pitch offset deg, fov deg]` per control point, in
/// the renderer's world (km). The view looks along the direction of travel turned by the
/// yaw offset (positive turns right), pitched by 0.6 of the climb angle plus the pitch
/// offset; the yaw offsets around the eighth point keep the main peak in view on the right.
const CONTROL: [[f64; 6]; 24] = [
    [-62.7, 0.436, -95.6, 0.0, -4.0, 68.0],
    [-62.9, 0.186, -98.2, 0.0, -3.0, 68.0],
    [-63.0, 0.2, -100.8, 0.0, -1.0, 66.0],
    [-62.9, 0.852, -103.2, 0.0, 4.0, 64.0],
    [-62.4, 2.405, -105.4, 0.0, 6.0, 62.0],
    [-61.6, 2.694, -107.6, 10.0, 0.0, 60.0],
    [-60.6, 2.6, -110.0, 35.0, -2.0, 58.0],
    [-59.8, 2.85, -112.6, 60.0, -3.0, 56.0],
    [-58.8, 3.2, -115.5, 55.0, 0.0, 56.0],
    [-56.6, 4.0, -118.4, 20.0, -2.0, 60.0],
    [-53.4, 5.1, -120.4, 5.0, -8.0, 64.0],
    [-49.8, 6.2, -120.2, 0.0, -13.0, 66.0],
    [-46.8, 6.9, -117.4, 0.0, -16.0, 70.0],
    [-45.6, 7.0, -113.0, 0.0, -16.0, 72.0],
    [-46.4, 6.1, -108.4, 0.0, -18.0, 72.0],
    [-47.6, 4.6, -104.0, 0.0, -24.0, 70.0],
    [-48.6, 3.1, -99.8, 0.0, -22.0, 68.0],
    [-49.6, 1.747, -95.6, 0.0, -12.0, 68.0],
    [-51.0, 0.939, -91.6, 0.0, -4.0, 68.0],
    [-53.2, 0.464, -89.0, 0.0, -3.0, 68.0],
    [-55.8, 1.453, -88.4, 0.0, -4.0, 68.0],
    [-58.6, 1.85, -89.4, 0.0, -6.0, 68.0],
    [-61.0, 1.134, -91.6, 0.0, -6.0, 68.0],
    [-62.4, 0.433, -93.6, 0.0, -5.0, 68.0],
];

/// The spline's value (`deriv` false) or its derivative per second (`deriv` true) at a
/// phase in seconds; any real phase, the loop repeating every `LOOP_SECS`.
fn spline(phase: f64, deriv: bool) -> [f64; 6] {
    let n = CONTROL.len();
    let dt = LOOP_SECS / n as f64;
    let s = (phase / dt).rem_euclid(n as f64);
    let i = s.floor() as usize % n;
    let u = s - s.floor();
    let b = if deriv {
        [
            -(1.0 - u).powi(2) / 2.0 / dt,
            (3.0 * u * u - 4.0 * u) / 2.0 / dt,
            (-3.0 * u * u + 2.0 * u + 1.0) / 2.0 / dt,
            u * u / 2.0 / dt,
        ]
    } else {
        [
            (1.0 - u).powi(3) / 6.0,
            (3.0 * u.powi(3) - 6.0 * u * u + 4.0) / 6.0,
            (-3.0 * u.powi(3) + 3.0 * u * u + 3.0 * u + 1.0) / 6.0,
            u.powi(3) / 6.0,
        ]
    };
    let mut out = [0.0; 6];
    for (j, w) in b.iter().enumerate() {
        let c = &CONTROL[(i + n + j - 1) % n];
        for (o, v) in out.iter_mut().zip(c) {
            *o += w * v;
        }
    }
    out
}

/// How far ahead of the eye the look point sits, km.
const LOOK_KM: f64 = 2.0;

/// The pose at a phase of the loop, as a keyframe at `t` = phase.
pub fn pose(phase: f64) -> Keyframe {
    let c = spline(phase, false);
    let v = spline(phase, true);
    let yaw = v[0].atan2(-v[2]) + c[3].to_radians();
    let pitch = 0.6 * v[1].atan2(v[0].hypot(v[2])) + c[4].to_radians();
    let dir = [
        pitch.cos() * yaw.sin(),
        pitch.sin(),
        -pitch.cos() * yaw.cos(),
    ];
    let pos = [c[0], c[1], c[2]];
    Keyframe {
        t: phase,
        pos,
        look: [
            pos[0] + dir[0] * LOOK_KM,
            pos[1] + dir[1] * LOOK_KM,
            pos[2] + dir[2] * LOOK_KM,
        ],
        fov_deg: c[5],
        intent: None,
    }
}

/// The camera velocity at a phase, km/s (for tests).
pub fn velocity(phase: f64) -> [f64; 3] {
    let v = spline(phase, true);
    [v[0], v[1], v[2]]
}

/// The pose at scene time `t` of a flight whose phase is 0 at `t0`.
fn at(t0: f64, t: f64) -> Keyframe {
    Keyframe { t, ..pose(t - t0) }
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// The pose a claim starts at: the loop's start, held until the flight begins.
pub fn start_pose(t: f64, intent: Option<String>) -> Keyframe {
    Keyframe {
        t,
        intent,
        ..pose(0.0)
    }
}

fn path(c: &Claim) -> Scene {
    Scene {
        epoch_unix_ms: 0,
        camera: c.camera.clone(),
        time_of_day: 0.0,
        words: vec![],
        lights: vec![],
        weather: vec![],
    }
}

/// The camera as the renderer draws it at `t`.
pub fn camera_at(c: &Claim, t: f64) -> Keyframe {
    path(c).camera_at(t)
}

/// The first frame showed: fly from `start_t` on, if the autopilot is on.
pub fn start(c: &mut Claim, start_t: f64, now: f64) {
    if !c.autopilot || c.flight_t0.is_some() {
        return;
    }
    c.flight_t0 = Some(start_t);
    c.camera.retain(|k| k.t < start_t);
    c.camera.push(start_pose(start_t, None));
    extend(c, now);
}

/// Keep the flight's keyframes at least `AHEAD_MIN_SECS` ahead of `now`.
pub fn extend(c: &mut Claim, now: f64) {
    let Some(t0) = c.flight_t0.filter(|_| c.autopilot) else {
        return;
    };
    let last = c.camera.last().map_or(f64::NEG_INFINITY, |k| k.t);
    if last >= now + AHEAD_MIN_SECS {
        return;
    }
    let from = last.max(now - KEY_SECS);
    let mut k = ((from - t0) / KEY_SECS).floor() + 1.0;
    while t0 + k * KEY_SECS <= last + 1e-6 {
        k += 1.0;
    }
    while t0 + k * KEY_SECS <= now + AHEAD_SECS {
        c.camera.push(at(t0, t0 + k * KEY_SECS));
        k += 1.0;
    }
    let mut s = path(c);
    s.trim_history(now);
    c.camera = s.camera;
}

/// Pause: the camera comes to rest over `ease` seconds along the path it was on, and the
/// viewer has it from there.
pub fn pause(c: &mut Claim, now: f64, ease: f64, intent: Option<String>) {
    let mut s = path(c);
    let to = Keyframe {
        intent,
        ..s.camera_at(now + ease / 2.0)
    };
    s.steer_over(now, to, ease);
    c.camera = s.camera;
    c.autopilot = false;
    c.flight_t0 = None;
}

/// Resume: fly from where the camera is to the nearest point of the loop and on around it.
/// Before the first frame this only arms the autopilot; the flight starts with the frame.
pub fn resume(c: &mut Claim, now: f64, intent: Option<String>) {
    c.autopilot = true;
    if c.first_frame_at.is_none() {
        return;
    }
    let here = camera_at(c, now);
    let steps = (LOOP_SECS / KEY_SECS).round() as usize;
    let phase = (0..steps)
        .map(|i| i as f64 * KEY_SECS)
        .min_by(|a, b| {
            dist(pose(*a).pos, here.pos)
                .partial_cmp(&dist(pose(*b).pos, here.pos))
                .unwrap()
        })
        .unwrap();
    let join = (dist(pose(phase).pos, here.pos) / JOIN_KM_PER_S + 1.5).clamp(2.0, 8.0);
    let t0 = now + join - phase;
    c.flight_t0 = Some(t0);
    c.camera.retain(|k| k.t < now);
    c.camera.push(here);
    c.camera.push(Keyframe {
        intent,
        ..at(t0, now + join)
    });
    extend(c, now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f64; 3], b: [f64; 3], tol: f64) -> bool {
        dist(a, b) <= tol
    }

    #[test]
    fn the_loop_closes_with_matching_position_and_velocity() {
        let eps = 1e-6;
        let end = pose(LOOP_SECS - eps);
        let start = pose(0.0);
        assert!(close(end.pos, start.pos, 1e-5), "{end:?} vs {start:?}");
        assert!(close(end.look, start.look, 1e-4), "{end:?} vs {start:?}");
        assert!((end.fov_deg - start.fov_deg).abs() < 1e-4);
        let (v0, v1) = (velocity(LOOP_SECS - eps), velocity(0.0));
        assert!(close(v0, v1, 1e-4), "{v0:?} vs {v1:?}");
        // and the keyframes sampled across the loop point are the same as one lap later
        for k in -3..=3 {
            let t = k as f64 * KEY_SECS;
            assert!(close(pose(t).pos, pose(t + LOOP_SECS).pos, 1e-9));
        }
    }
}
