//! Wire types: browser <-> coordinator (WebSocket `/live`), agent <-> coordinator
//! (newline-delimited JSON), and the body the agent posts to each renderer's `/scene`.
//! `fleet/PROTOCOL.md` documents them.

use crate::scene::{Keyframe, Light, Scene, WeatherKey, Word};
use crate::state::{Gpu, TextCheck};
use crate::tiling::Rect;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Browser <-> coordinator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Intent {
        id: String,
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    Ping {
        client_unix_ms: f64,
    },
    /// A Cloudflare Turnstile token, before the connection's first intent.
    Verify {
        token: String,
    },
    /// Whether the page is visible; a hidden page is not an active viewer.
    Visibility {
        visible: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Snapshot {
        version: u64,
        state: serde_json::Value,
    },
    Receipt {
        id: String,
        outcome: crate::state::Outcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        version: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        check: Option<TextCheck>,
    },
    Clock {
        server_unix_ms: u64,
        client_unix_ms: f64,
    },
    /// First message on every connection: this viewer's id, as `actions[].viewer` names it,
    /// and the Turnstile site key when intents need a `verify` first (null otherwise).
    Hello {
        viewer: String,
        turnstile: Option<String>,
    },
    /// The answer to `verify`.
    Verified { ok: bool, reason: Option<String> },
    /// The viewer's own GPU, to the claiming connection only, whenever it changes and once
    /// more when it ends.
    Claim { claim: ClaimMsg },
    /// A message the coordinator could not read; nothing was applied.
    Error { reason: String },
}

/// A viewer's own GPU as its viewer sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimMsg {
    /// The session (`claim-3`), what `release_gpu` names as `claim`.
    pub id: String,
    /// The wall tile whose GPU it borrowed (`fcvm:3`); null for a standby GPU.
    pub tile: Option<String>,
    /// `restoring` until the private VM's first frame, `running`, then `ended`.
    pub phase: String,
    /// The private renderer's frame WebSocket over IPv6 and over IPv4, once it runs (with
    /// published names, once the launch's names answer, as for a tile).
    pub frame_url: Option<String>,
    #[serde(default)]
    pub frame_url_v4: Option<String>,
    /// The session token for this viewer alone.
    pub token: String,
    pub clicked_at: u64,
    pub first_frame_at: Option<u64>,
    pub expires_at: u64,
    /// The GPU's name.
    pub gpu: Option<String>,
    /// The agent's timings of the private VM's restore.
    pub phases: BTreeMap<String, u64>,
    pub camera: Vec<crate::scene::Keyframe>,
    /// The camera flies the autopilot's loop (`claim_autopilot`); false once the viewer
    /// paused it or took the camera.
    pub autopilot: bool,
    /// Why it ended: `expired` (its time ran out) or `released` (`release_gpu`, or the GPU
    /// left the fleet); null while it lasts.
    pub ended: Option<String>,
}

impl ServerMsg {
    pub fn receipt(r: &crate::state::Receipt) -> Self {
        ServerMsg::Receipt {
            id: r.id.clone(),
            outcome: r.outcome,
            reason: r.reason.clone(),
            version: r.version,
            check: r.check.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Agent <-> coordinator
// ---------------------------------------------------------------------------

/// What an agent reports about one slot.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SlotReport {
    pub id: String,
    pub phase: String,
    pub generation: u64,
    #[serde(default)]
    pub vm: Option<String>,
    #[serde(default)]
    pub frame_url: Option<String>,
    #[serde(default)]
    pub timings: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// With `failed`: when the agent tries again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_in_ms: Option<u64>,
    /// The slot's public addresses, for its DNS names (`slot-<n>`, `ec2-<n>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<String>,
    /// The TCP port the renderer's TLS port is reached on at `ipv4` (a GPU host's public
    /// IPv4 address is shared by its slots, each forwarded on its own port); 443 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v4_port: Option<u16>,
    /// Where the slot runs, as a viewer may be told (`place_label`); never the instance id
    /// or an address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
}

/// The public label of where a slot runs: `EC2 in <region>`. The one place the wording is
/// made, for fcvm hosts and EC2 tiles alike. None for text that is not an AWS region name
/// (lowercase letters, digits and dashes), so nothing else reaches a viewer through it.
pub fn place_label(region: &str) -> Option<String> {
    let ok = !region.is_empty()
        && region.len() <= 32
        && region.starts_with(|c: char| c.is_ascii_lowercase())
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then(|| format!("EC2 in {region}"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthReport {
    pub id: String,
    pub fps: f64,
    #[serde(default)]
    pub gpu: serde_json::Value,
    /// The renderer's `budget_fit`, the scene time of its newest frame (`t`), and its
    /// slowest render over the last 2 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_fit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<f64>,
    /// The slowest render over its last 2 s (`frames_2s.render_ms_max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_ms_max: Option<f64>,
}

impl HealthReport {
    /// The report for a renderer's `/health` reply. A renderer that is not `ready` reports
    /// no fit, as budgetctl.py counts only ready tiles.
    pub fn from_renderer(id: &str, h: &serde_json::Value) -> HealthReport {
        let num = |v: Option<&serde_json::Value>| v.and_then(|f| f.as_f64());
        let ready = h.get("ready").and_then(|r| r.as_bool()) != Some(false);
        HealthReport {
            id: id.to_string(),
            fps: num(h.get("fps")).unwrap_or(0.0),
            gpu: h.get("gpu").cloned().unwrap_or(serde_json::Value::Null),
            budget_fit: num(h.get("budget_fit")).filter(|_| ready),
            t: num(h.get("t")),
            render_ms_max: num(h.get("frames_2s").and_then(|f| f.get("render_ms_max"))),
        }
    }
}

/// The renderer's process id in a `/health` reply (`boot_id`, random per process), when it
/// reports one. A different one than the last read means the renderer restarted and has
/// lost its scene.
pub fn boot_id(health: &serde_json::Value) -> Option<String> {
    health
        .get("boot_id")
        .and_then(|b| b.as_str())
        .map(str::to_string)
}

/// Agent -> coordinator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    Hello {
        host: String,
        arch: String,
        gpus: Vec<Gpu>,
        capacity: usize,
        /// When the host's store was ready (Unix ms), if the host marks it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        store_ready_at: Option<u64>,
        /// When the host's boot reached each of its steps (Unix ms, the host's clock):
        /// `kernel_started_at`, `store_started_at`, `store_mounted_at`, `agent_started_at`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        boot: BTreeMap<String, u64>,
    },
    Slot(SlotReport),
    Health(HealthReport),
    /// The host got a spot interruption notice (or is otherwise going away).
    Draining {
        reason: String,
    },
    /// The host's aggregator is serving the host's region at this frame WebSocket, and,
    /// when it serves browsers with TLS, at these public addresses: its own IPv6 address
    /// (port 443) and the host's IPv4 address on `v4_port`. The coordinator names them.
    Aggregate {
        frame_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ipv6: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ipv4: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        v4_port: Option<u16>,
    },
    /// A GPU scrub finished (`ok`), or failed with `error`.
    Scrub {
        slot: String,
        sbdf: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The agent took a GPU out of rotation: its reset, scrub or attach failed. `report`
    /// names this report (random per quarantine), so a replay of it is told apart from a
    /// new failure of the same GPU.
    Quarantine {
        slot: String,
        sbdf: String,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<String>,
    },
}

/// One slot an agent must run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignSlot {
    pub id: String,
    pub sbdf: String,
    pub generation: u64,
    /// `None` on standby or while the host drains: keep the VM, post no scene.
    pub rect: Option<Rect>,
    /// A private session's camera: the renderer draws the whole image with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_camera: Option<Vec<Keyframe>>,
    /// The layout generation of `rect`, the frame it takes effect at, and the slot's
    /// rectangle before (when this generation moved it).
    #[serde(default)]
    pub layout_generation: u64,
    #[serde(default)]
    pub rect_from_frame: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_rect: Option<Rect>,
    /// In the standby pool: run no VM, keep the GPU reset, scrubbed and bound, and report
    /// `standby` when it is ready.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub standby: bool,
    /// Warming before it joins the wall: run the VM and draw the whole image with the shared
    /// camera (not part of the host's band), then take `rect` when it is placed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warm: bool,
}

/// The scene shared by every renderer; the agent adds each slot's rect to make the
/// renderer's `POST /scene` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedScene {
    pub epoch_unix_ms: u64,
    pub image_aspect: f64,
    pub camera: Vec<Keyframe>,
    pub time_of_day: f64,
    pub words: Vec<Word>,
    pub lights: Vec<Light>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weather: Vec<WeatherKey>,
}

impl SharedScene {
    pub fn new(scene: &Scene, image_aspect: f64) -> Self {
        SharedScene {
            epoch_unix_ms: scene.epoch_unix_ms,
            image_aspect,
            camera: scene.camera.clone(),
            time_of_day: scene.time_of_day,
            words: scene.words.clone(),
            lights: scene.lights.clone(),
            weather: scene.weather.clone(),
        }
    }

    pub fn for_renderer(&self, rect: Rect) -> RendererScene {
        RendererScene {
            epoch_unix_ms: self.epoch_unix_ms,
            rect,
            layout_generation: 0,
            rect_from_frame: 0,
            previous_rect: None,
            image_aspect: self.image_aspect,
            camera: self.camera.clone(),
            time_of_day: self.time_of_day,
            words: self.words.clone(),
            lights: self.lights.clone(),
            weather: self.weather.clone(),
        }
    }
}

/// Coordinator -> agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordMsg {
    /// The complete set of slots the host must run (a slot missing from it is torn down),
    /// and the rectangle its placed slots cover together (its aggregator's region).
    Assign {
        slots: Vec<AssignSlot>,
        #[serde(default)]
        region: Option<Rect>,
    },
    Scene(SharedScene),
    /// The grid's budget: every renderer draws `budget` pixel-samples per frame from the
    /// first frame at or after scene time `t` (`POST /budget`, again after every scene).
    Budget(BudgetBody),
    /// A state that holds the agent's `quarantine` report `report` for this GPU (or an
    /// operator's release that came after it) is stored: the agent stops replaying that
    /// report.
    QuarantineStored {
        sbdf: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<String>,
    },
}

/// The body of a renderer's `POST /budget`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BudgetBody {
    pub budget: u64,
    pub t: f64,
}

/// The body of a renderer's `POST /scene`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RendererScene {
    pub epoch_unix_ms: u64,
    pub rect: Rect,
    /// The layout generation `rect` belongs to, and the frame it takes effect at; before
    /// that frame the renderer keeps drawing `previous_rect` (when the tile moved).
    #[serde(default)]
    pub layout_generation: u64,
    #[serde(default)]
    pub rect_from_frame: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_rect: Option<Rect>,
    pub image_aspect: f64,
    pub camera: Vec<Keyframe>,
    pub time_of_day: f64,
    pub words: Vec<Word>,
    pub lights: Vec<Light>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weather: Vec<WeatherKey>,
}

#[cfg(test)]
mod tests {
    use super::HealthReport;

    #[test]
    fn a_health_reply_gives_the_fit_only_when_ready() {
        let h = serde_json::json!({"fps": 30.0, "t": 12.5, "ready": true, "budget_fit": 4.0e5,
            "frames_2s": {"n": 60, "render_ms_max": 31.5}});
        let r = HealthReport::from_renderer("slot-1", &h);
        assert_eq!(
            (r.budget_fit, r.t, r.render_ms_max),
            (Some(4.0e5), Some(12.5), Some(31.5))
        );
        let mut h = h;
        h["ready"] = false.into();
        assert_eq!(HealthReport::from_renderer("slot-1", &h).budget_fit, None);
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn messages_have_the_documented_shapes() {
        let m: ClientMsg = serde_json::from_value(
            json!({"type":"intent","id":"a","name":"relaunch","args":{"tile":"slot-1"}}),
        )
        .unwrap();
        assert!(matches!(m, ClientMsg::Intent { .. }));
        let m: AgentMsg = serde_json::from_value(json!({
            "type":"slot","id":"slot-1","phase":"running","generation":2,
            "vm":"fleet-slot-1","frame_url":"ws://h:1/frames","timings":{"gpu_ms":3000}
        }))
        .unwrap();
        assert!(matches!(
            m,
            AgentMsg::Slot(SlotReport { generation: 2, .. })
        ));
        let a = CoordMsg::Assign {
            slots: vec![AssignSlot {
                id: "slot-1".into(),
                sbdf: "0000:01:00.0".into(),
                generation: 1,
                rect: None,
                private_camera: None,
                layout_generation: 0,
                rect_from_frame: 0,
                previous_rect: None,
                standby: false,
                warm: false,
            }],
            region: None,
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            json!({"type":"assign","slots":[{"id":"slot-1","sbdf":"0000:01:00.0","generation":1,"rect":null,
                "layout_generation":0,"rect_from_frame":0}],"region":null})
        );
        let s = CoordMsg::Scene(SharedScene::new(&Scene::new(5), 2.0));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "scene");
        assert_eq!(v["epoch_unix_ms"], 5);
        let r = serde_json::to_value(SharedScene::new(&Scene::new(5), 2.0).for_renderer(Rect {
            x: 0.0,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        }))
        .unwrap();
        for key in [
            "epoch_unix_ms",
            "rect",
            "image_aspect",
            "camera",
            "time_of_day",
            "words",
            "lights",
        ] {
            assert!(r.get(key).is_some(), "renderer body lacks {key}");
        }
    }
}
