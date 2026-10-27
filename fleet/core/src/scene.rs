//! The shared scene every renderer draws: camera path, time of day, viewers' words and
//! lights. Scene time `t` is seconds since `epoch_unix_ms`.

use serde::{Deserialize, Serialize};

/// One point on the camera path. `pos` is the camera position and `look` the world point it
/// looks at, `[x, altitude, z]` in kilometres (the renderer's world units); `t` is scene
/// seconds. `intent` is the id of the camera intent that created the keyframe, so a page
/// can tell when the frames reflect its own input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    pub t: f64,
    pub pos: [f64; 3],
    pub look: [f64; 3],
    pub fov_deg: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
}

/// How the renderer draws a viewer's message (the renderer's PROTOCOL.md, #1178).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStyle {
    #[default]
    Lockup,
    Skywriting,
    Landmark,
}

impl TextStyle {
    pub const NAMES: &'static str = "lockup, skywriting, landmark";

    pub fn parse(s: &str) -> Option<TextStyle> {
        match s {
            "lockup" => Some(TextStyle::Lockup),
            "skywriting" => Some(TextStyle::Skywriting),
            "landmark" => Some(TextStyle::Landmark),
            _ => None,
        }
    }
}

/// A word placed at scene time `t`, drawn in `text_style`: a viewer's, or a system word
/// (a line of GPU history the wall wrote itself, `system: true`; see `crate::history`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub t: f64,
    #[serde(default)]
    pub text_style: TextStyle,
    /// Written by the wall, not by a viewer. Absent from the wire when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
}

/// A light a viewer dropped: world position, linear RGB in [0, 1], scene time placed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Light {
    pub pos: [f64; 3],
    pub color: [f64; 3],
    pub t: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub epoch_unix_ms: u64,
    pub camera: Vec<Keyframe>,
    /// Fraction of the day in [0, 1]: 0.25 sunrise, 0.5 noon, 0.75 sunset.
    pub time_of_day: f64,
    pub words: Vec<Word>,
    pub lights: Vec<Light>,
}

/// Bounds on what viewers can put into the scene.
pub const MAX_KEYFRAMES: usize = 64;
pub const MAX_WORDS: usize = 16;
pub const MAX_LIGHTS: usize = 32;
pub const MAX_WORD_CHARS: usize = 28;
/// Coordinates beyond this are refused, so a viewer cannot send the camera to infinity.
pub const MAX_COORD: f64 = 1.0e6;
/// How far ahead of now a drag places its keyframe, in scene seconds.
pub const DRAG_EASE_SECS: f64 = 0.3;
/// Keyframes older than this, before the newest one at or before now, are dropped.
pub const KEYFRAME_HISTORY_SECS: f64 = 10.0;

impl Scene {
    pub fn new(epoch_unix_ms: u64) -> Self {
        Scene {
            epoch_unix_ms,
            camera: vec![Keyframe {
                t: 0.0,
                pos: [-32.0, 1.9, -55.0],
                look: [-60.0, 1.9, -100.0],
                fov_deg: 62.0,
                intent: None,
            }],
            time_of_day: 0.30,
            words: Vec::new(),
            lights: Vec::new(),
        }
    }

    /// Scene seconds at a Unix time.
    pub fn t_at(&self, unix_ms: u64) -> f64 {
        (unix_ms as f64 - self.epoch_unix_ms as f64) / 1000.0
    }

    /// The camera at scene time `t`, as the renderer draws it (fleet/renderer/src/scene.cpp
    /// `camera_at`): Catmull-Rom on `pos` and `look` with tangents scaled to each segment
    /// and zero next to a hold (two equal consecutive keyframes) or an end, `fov_deg`
    /// linear, held before the first keyframe and after the last. A steer starts from
    /// this pose, so taking over a moving camera starts where its frames are.
    pub fn camera_at(&self, t: f64) -> Keyframe {
        let k = &self.camera;
        if k.is_empty() {
            return Scene::new(0).camera[0].clone();
        }
        let held = |kf: &Keyframe| Keyframe {
            t,
            intent: None,
            ..kf.clone()
        };
        let n = k.len();
        if t <= k[0].t {
            return held(&k[0]);
        }
        if t >= k[n - 1].t {
            return held(&k[n - 1]);
        }
        let i = k.partition_point(|x| x.t <= t) - 1;
        let (k1, k2) = (&k[i], &k[i + 1]);
        if same_pose(k1, k2) {
            return held(k1);
        }
        let dt = k2.t - k1.t;
        let u = (t - k1.t) / dt;
        let ease_in = i == 0 || same_pose(&k[i - 1], k1);
        let ease_out = i + 2 >= n || same_pose(k2, &k[i + 2]);
        let h00 = 2.0 * u * u * u - 3.0 * u * u + 1.0;
        let h10 = u * u * u - 2.0 * u * u + u;
        let h01 = -2.0 * u * u * u + 3.0 * u * u;
        let h11 = u * u * u - u * u;
        let interp = |get: fn(&Keyframe) -> [f64; 3]| {
            let mut o = [0.0; 3];
            for (c, out) in o.iter_mut().enumerate() {
                let (p1, p2) = (get(k1)[c], get(k2)[c]);
                let m1 = if ease_in {
                    0.0
                } else {
                    let k0 = &k[i - 1];
                    (p2 - get(k0)[c]) / (k2.t - k0.t) * dt
                };
                let m2 = if ease_out {
                    0.0
                } else {
                    let k3 = &k[i + 2];
                    (get(k3)[c] - p1) / (k3.t - k1.t) * dt
                };
                *out = h00 * p1 + h10 * m1 + h01 * p2 + h11 * m2;
            }
            o
        };
        Keyframe {
            t,
            pos: interp(|x| x.pos),
            look: interp(|x| x.look),
            fov_deg: k1.fov_deg + (k2.fov_deg - k1.fov_deg) * u,
            intent: None,
        }
    }

    /// Replace the path from `now` on with a move from the current camera to `to`, easing
    /// over `DRAG_EASE_SECS`, and drop history older than `KEYFRAME_HISTORY_SECS`.
    pub fn steer(&mut self, now: f64, to: Keyframe) {
        self.steer_over(now, to, DRAG_EASE_SECS);
    }

    /// `steer`, easing over `ease` seconds.
    pub fn steer_over(&mut self, now: f64, to: Keyframe, ease: f64) {
        let here = self.camera_at(now);
        self.camera.retain(|k| k.t < now);
        self.camera.push(here);
        self.camera.push(Keyframe {
            t: now + ease,
            ..to
        });
        self.trim_history(now);
    }

    /// Drop keyframes that can no longer affect any frame at or after `now - history`,
    /// keeping the last one before that point as the path's start.
    pub fn trim_history(&mut self, now: f64) {
        let cutoff = now - KEYFRAME_HISTORY_SECS;
        let keep_from = self.camera.iter().rposition(|k| k.t <= cutoff).unwrap_or(0);
        self.camera.drain(..keep_from);
        while self.camera.len() > MAX_KEYFRAMES {
            self.camera.remove(0);
        }
    }
}

/// The renderer's hold test: the same position, look point and field of view.
pub fn same_pose(a: &Keyframe, b: &Keyframe) -> bool {
    a.pos == b.pos && a.look == b.look && a.fov_deg == b.fov_deg
}

/// Turn the camera by `yaw_deg` about the world up axis and `pitch_deg` up or down, then
/// move it `dive` world units along its view direction. Pitch is clamped short of straight
/// up or down so the look direction never degenerates.
pub fn drag(from: &Keyframe, yaw_deg: f64, pitch_deg: f64, dive: f64) -> Keyframe {
    let d = [
        from.look[0] - from.pos[0],
        from.look[1] - from.pos[1],
        from.look[2] - from.pos[2],
    ];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-9);
    let horiz = (d[0] * d[0] + d[2] * d[2]).sqrt();
    let yaw = d[0].atan2(-d[2]) + yaw_deg.to_radians();
    let max_pitch = 85f64.to_radians();
    let pitch = (d[1].atan2(horiz) + pitch_deg.to_radians()).clamp(-max_pitch, max_pitch);
    let dir = [
        pitch.cos() * yaw.sin(),
        pitch.sin(),
        -pitch.cos() * yaw.cos(),
    ];
    let pos = [
        from.pos[0] + dir[0] * dive,
        from.pos[1] + dir[1] * dive,
        from.pos[2] + dir[2] * dive,
    ];
    Keyframe {
        t: from.t,
        pos,
        look: [
            pos[0] + dir[0] * len,
            pos[1] + dir[1] * len,
            pos[2] + dir[2] * len,
        ],
        fov_deg: from.fov_deg,
        intent: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_interpolates_and_holds_at_the_ends() {
        let mut s = Scene::new(0);
        s.camera = vec![
            Keyframe {
                t: 0.0,
                pos: [0.0; 3],
                look: [0.0, 0.0, -1.0],
                fov_deg: 60.0,
                intent: None,
            },
            Keyframe {
                t: 2.0,
                pos: [2.0, 0.0, 0.0],
                look: [2.0, 0.0, -1.0],
                fov_deg: 80.0,
                intent: None,
            },
        ];
        assert_eq!(s.camera_at(1.0).pos, [1.0, 0.0, 0.0]);
        assert_eq!(s.camera_at(1.0).fov_deg, 70.0);
        assert_eq!(s.camera_at(-5.0).pos, [0.0; 3]);
        assert_eq!(s.camera_at(9.0).pos, [2.0, 0.0, 0.0]);
    }

    #[test]
    fn drag_with_no_turn_moves_along_the_view() {
        let k = Keyframe {
            t: 0.0,
            pos: [0.0; 3],
            look: [0.0, 0.0, -10.0],
            fov_deg: 60.0,
            intent: None,
        };
        let m = drag(&k, 0.0, 0.0, 5.0);
        assert!((m.pos[2] + 5.0).abs() < 1e-9 && m.pos[0].abs() < 1e-9);
        let turned = drag(&k, 90.0, 0.0, 0.0);
        assert!((turned.look[0] - 10.0).abs() < 1e-9, "{turned:?}");
    }

    #[test]
    fn steering_keeps_the_path_bounded() {
        let mut s = Scene::new(0);
        for i in 0..1000 {
            let now = i as f64 * 0.05;
            let to = drag(&s.camera_at(now), 1.0, 0.0, 0.1);
            s.steer(now, to);
            assert!(s.camera.len() <= MAX_KEYFRAMES);
        }
        assert!(s.camera.windows(2).all(|w| w[0].t <= w[1].t));
    }
}
