//! Events (the only inputs to [`crate::apply`]) and the intents viewers send.

use crate::protocol::{AgentMsg, SlotReport};
use crate::scene::{Keyframe, MAX_COORD, MAX_KEYFRAMES, MAX_WORD_CHARS};
use crate::state::TextCheck;
use serde::{Deserialize, Serialize};

/// Every state change is one of these. `at` is Unix milliseconds, stamped by the
/// coordinator when it accepts the event; `apply` reads no clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A viewer's intent. `check` is the text check's verdict for a `word`, made before the
    /// event was submitted; a `word` without one is refused.
    Intent {
        at: u64,
        viewer: String,
        id: String,
        name: String,
        #[serde(default)]
        args: serde_json::Value,
        #[serde(default)]
        check: Option<TextCheck>,
    },
    ViewerJoined {
        at: u64,
        viewer: String,
    },
    ViewerLeft {
        at: u64,
        viewer: String,
    },
    /// Once a second; drives auto-relaunch.
    Tick {
        at: u64,
    },
    /// A message from the agent on `host`.
    Agent {
        at: u64,
        host: String,
        msg: AgentMsg,
    },
    /// The agent on `host` disconnected or exited.
    AgentGone {
        at: u64,
        host: String,
    },
    /// The EC2 driver's report about one EC2 tile.
    Ec2 {
        at: u64,
        report: SlotReport,
    },
}

impl Event {
    pub fn at(&self) -> u64 {
        match self {
            Event::Intent { at, .. }
            | Event::ViewerJoined { at, .. }
            | Event::ViewerLeft { at, .. }
            | Event::Tick { at }
            | Event::Agent { at, .. }
            | Event::AgentGone { at, .. }
            | Event::Ec2 { at, .. } => *at,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CameraMove {
    /// Replace the path.
    Keyframes(Vec<Keyframe>),
    /// Turn and move from the current camera.
    Drag {
        yaw_deg: f64,
        pitch_deg: f64,
        dive: f64,
    },
}

/// A parsed intent.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Relaunch { tile: String },
    RelaunchAll,
    SetAutoRelaunch { secs: Option<u64> },
    Camera(CameraMove),
    TimeOfDay { v: f64 },
    Word { text: String },
    Light { pos: [f64; 3], color: [f64; 3] },
}

fn num(args: &serde_json::Value, key: &str) -> Result<f64, String> {
    let v = args
        .get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("args.{key} must be a number"))?;
    if !v.is_finite() {
        return Err(format!("args.{key} must be finite"));
    }
    Ok(v)
}

fn vec3(v: &serde_json::Value, what: &str, bound: f64) -> Result<[f64; 3], String> {
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| format!("{what} must be an array of 3 numbers"))?;
    let mut out = [0.0; 3];
    for (o, x) in out.iter_mut().zip(a) {
        let x = x
            .as_f64()
            .filter(|x| x.is_finite() && x.abs() <= bound)
            .ok_or_else(|| format!("{what} must hold finite numbers within {bound}"))?;
        *o = x;
    }
    Ok(out)
}

/// Normalise viewer text: collapse whitespace, trim; refuse control characters, empty text
/// and text longer than [`MAX_WORD_CHARS`].
pub fn normalise_word(text: &str) -> Result<String, String> {
    if text.chars().any(|c| c.is_control()) {
        return Err("text holds control characters".into());
    }
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = t.chars().count();
    if n == 0 {
        return Err("text is empty".into());
    }
    if n > MAX_WORD_CHARS {
        return Err(format!(
            "text is {n} characters; the limit is {MAX_WORD_CHARS}"
        ));
    }
    Ok(t)
}

impl Intent {
    pub fn parse(name: &str, args: &serde_json::Value) -> Result<Intent, String> {
        match name {
            "relaunch" => {
                let tile = args
                    .get("tile")
                    .and_then(|v| v.as_str())
                    .ok_or("args.tile must be a string")?;
                Ok(Intent::Relaunch {
                    tile: tile.to_string(),
                })
            }
            "relaunch_all" => Ok(Intent::RelaunchAll),
            "set_auto_relaunch" => match args.get("secs") {
                None | Some(serde_json::Value::Null) => Ok(Intent::SetAutoRelaunch { secs: None }),
                Some(v) => {
                    let secs = v
                        .as_u64()
                        .ok_or("args.secs must be a whole number or null")?;
                    Ok(Intent::SetAutoRelaunch { secs: Some(secs) })
                }
            },
            "camera" => {
                if let Some(k) = args.get("keyframes") {
                    let arr = k.as_array().ok_or("args.keyframes must be an array")?;
                    if arr.is_empty() || arr.len() > MAX_KEYFRAMES {
                        return Err(format!(
                            "args.keyframes must hold 1 to {MAX_KEYFRAMES} keyframes"
                        ));
                    }
                    let mut out = Vec::with_capacity(arr.len());
                    for k in arr {
                        let t = num(k, "t")?;
                        let fov = num(k, "fov_deg")?;
                        if !(10.0..=120.0).contains(&fov) {
                            return Err("fov_deg must be within 10 and 120".into());
                        }
                        out.push(Keyframe {
                            t,
                            pos: vec3(
                                k.get("pos").unwrap_or(&serde_json::Value::Null),
                                "pos",
                                MAX_COORD,
                            )?,
                            look: vec3(
                                k.get("look").unwrap_or(&serde_json::Value::Null),
                                "look",
                                MAX_COORD,
                            )?,
                            fov_deg: fov,
                        });
                    }
                    if out.windows(2).any(|w| w[1].t < w[0].t) {
                        return Err("keyframes must be in time order".into());
                    }
                    Ok(Intent::Camera(CameraMove::Keyframes(out)))
                } else if let Some(d) = args.get("drag") {
                    let yaw_deg = num(d, "yaw_deg")?;
                    let pitch_deg = num(d, "pitch_deg")?;
                    let dive = d
                        .get("dive")
                        .map(|_| num(d, "dive"))
                        .transpose()?
                        .unwrap_or(0.0);
                    if yaw_deg.abs() > 360.0 || pitch_deg.abs() > 180.0 || dive.abs() > 1000.0 {
                        return Err("drag is out of range".into());
                    }
                    Ok(Intent::Camera(CameraMove::Drag {
                        yaw_deg,
                        pitch_deg,
                        dive,
                    }))
                } else {
                    Err("camera needs args.keyframes or args.drag".into())
                }
            }
            "time_of_day" => {
                let v = num(args, "v")?;
                if !(0.0..24.0).contains(&v) {
                    return Err("args.v must be hours in [0, 24)".into());
                }
                Ok(Intent::TimeOfDay { v })
            }
            "word" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or("args.text must be a string")?;
                Ok(Intent::Word {
                    text: normalise_word(text)?,
                })
            }
            "light" => {
                let pos = vec3(
                    args.get("pos").unwrap_or(&serde_json::Value::Null),
                    "pos",
                    MAX_COORD,
                )?;
                let color = vec3(
                    args.get("color").unwrap_or(&serde_json::Value::Null),
                    "color",
                    1.0,
                )?;
                if color.iter().any(|c| *c < 0.0) {
                    return Err("color components must be within 0 and 1".into());
                }
                Ok(Intent::Light { pos, color })
            }
            other => Err(format!("unknown intent {other:?}")),
        }
    }
}
