//! Events (the only inputs to [`crate::apply`]) and the intents viewers send.

use crate::protocol::{AgentMsg, SlotReport};
use crate::scene::{MAX_COORD, MAX_WORD_CHARS};
use crate::state::TextCheck;
use serde::{Deserialize, Serialize};

/// Every state change is one of these. `at` is Unix milliseconds, stamped by the
/// coordinator when it accepts the event; `apply` reads no clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A viewer's intent. `client` is the address the per-client limits are keyed by
    /// (the viewer id when absent). `check` is the text check's verdict for a `word`, made
    /// before the event was submitted; a `word` without one is refused. `secret` is a value
    /// the coordinator draws for an intent that needs one (a claim's session token).
    Intent {
        at: u64,
        viewer: String,
        #[serde(default)]
        client: Option<String>,
        id: String,
        name: String,
        #[serde(default)]
        args: serde_json::Value,
        #[serde(default)]
        check: Option<TextCheck>,
        #[serde(default)]
        secret: Option<String>,
    },
    ViewerJoined {
        at: u64,
        viewer: String,
        #[serde(default)]
        client: Option<String>,
    },
    ViewerLeft {
        at: u64,
        viewer: String,
    },
    /// Once a second; drives the race schedule, the director and claim expiry.
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
    /// A coordinator component degraded (`reason`) or recovered (`None`).
    Degraded {
        at: u64,
        component: String,
        #[serde(default)]
        reason: Option<String>,
    },
    /// An operator returns a quarantined GPU to rotation.
    ReleaseQuarantine {
        at: u64,
        host: String,
        sbdf: String,
    },
    /// A viewer's page reported whether it is visible.
    Visibility {
        at: u64,
        viewer: String,
        visible: bool,
    },
    /// An EC2 tile's renderer health, read by the coordinator.
    Ec2Health {
        at: u64,
        report: crate::protocol::HealthReport,
    },
    /// The GPU hosts' instances, listed while the fleet wakes.
    ScaleInstances {
        at: u64,
        instances: Vec<ScaleInstance>,
    },
    /// A launch's DNS names were written and answer at the zone's authoritative servers.
    Published {
        at: u64,
        /// The slot or tile id (`slot-<n>`, `ec2-<n>`).
        slot: String,
        names: crate::state::Published,
    },
}

/// One GPU host instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScaleInstance {
    pub id: String,
    /// Its lifecycle state: `Pending`, `InService` (EC2 `running`), `Terminating`, ...
    pub lifecycle: String,
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
            | Event::Ec2 { at, .. }
            | Event::Degraded { at, .. }
            | Event::ReleaseQuarantine { at, .. }
            | Event::Visibility { at, .. }
            | Event::Ec2Health { at, .. }
            | Event::ScaleInstances { at, .. }
            | Event::Published { at, .. } => *at,
        }
    }
}

/// A parsed intent.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Relaunch {
        tile: String,
    },
    /// Start a race now, outside the global cooldown.
    RestartNow,
    /// A vote for the next hold of the directed camera.
    Nudge {
        target: String,
    },
    Word {
        text: String,
        text_style: crate::scene::TextStyle,
    },
    Light {
        pos: [f64; 3],
        color: [f64; 3],
    },
    /// Take a standby GPU for a private session.
    ClaimGpu,
    /// End the viewer's claim; `claim`, when given, must name it.
    ReleaseGpu {
        claim: Option<String>,
    },
    /// Turn and move the private session's camera.
    ClaimCamera {
        yaw_deg: f64,
        pitch_deg: f64,
        dive: f64,
    },
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

fn vec3(v: &serde_json::Value, what: &str) -> Result<[f64; 3], String> {
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| format!("{what} must be an array of 3 numbers"))?;
    let mut out = [0.0; 3];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x
            .as_f64()
            .filter(|x| x.is_finite())
            .ok_or_else(|| format!("{what} must hold finite numbers"))?;
    }
    Ok(out)
}

/// Light colour as given, each component clamped to [0, 1], then scaled down so the three
/// add up to at most `MAX_LIGHT_SUM`: no light outshines the scene.
pub const MAX_LIGHT_SUM: f64 = 2.0;

pub fn clamp_color(c: [f64; 3]) -> [f64; 3] {
    let c = c.map(|v| v.clamp(0.0, 1.0));
    let sum: f64 = c.iter().sum();
    if sum > MAX_LIGHT_SUM {
        c.map(|v| v * MAX_LIGHT_SUM / sum)
    } else {
        c
    }
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
            "restart_now" => Ok(Intent::RestartNow),
            "nudge" => {
                let target = args
                    .get("target")
                    .and_then(|v| v.as_str())
                    .ok_or("args.target must be a viewpoint name")?;
                Ok(Intent::Nudge {
                    target: target.to_string(),
                })
            }
            "word" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or("args.text must be a string")?;
                let text_style = match args.get("text_style") {
                    None | Some(serde_json::Value::Null) => crate::scene::TextStyle::default(),
                    Some(v) => v
                        .as_str()
                        .and_then(crate::scene::TextStyle::parse)
                        .ok_or_else(|| {
                            format!(
                                "args.text_style must be one of {}",
                                crate::scene::TextStyle::NAMES
                            )
                        })?,
                };
                Ok(Intent::Word {
                    text: normalise_word(text)?,
                    text_style,
                })
            }
            "light" => {
                let pos = vec3(args.get("pos").unwrap_or(&serde_json::Value::Null), "pos")?;
                if pos.iter().any(|v| v.abs() > MAX_COORD) {
                    return Err(format!("pos must be within {MAX_COORD}"));
                }
                let color = vec3(
                    args.get("color").unwrap_or(&serde_json::Value::Null),
                    "color",
                )?;
                Ok(Intent::Light {
                    pos,
                    color: clamp_color(color),
                })
            }
            "claim_gpu" => Ok(Intent::ClaimGpu),
            "release_gpu" => Ok(Intent::ReleaseGpu {
                claim: match args.get("claim") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => Some(
                        v.as_str()
                            .ok_or("args.claim must be a claim id")?
                            .to_string(),
                    ),
                },
            }),
            "claim_camera" => {
                let d = args.get("drag").ok_or("claim_camera needs args.drag")?;
                let yaw_deg = num(d, "yaw_deg")?;
                let pitch_deg = num(d, "pitch_deg")?;
                let dive = d
                    .get("dive")
                    .map(|_| num(d, "dive"))
                    .transpose()?
                    .unwrap_or(0.0);
                if yaw_deg.abs() > 360.0 || pitch_deg.abs() > 180.0 || dive.abs() > 50.0 {
                    return Err("drag is out of range".into());
                }
                Ok(Intent::ClaimCamera {
                    yaw_deg,
                    pitch_deg,
                    dive,
                })
            }
            other => Err(format!("unknown intent {other:?}")),
        }
    }
}
