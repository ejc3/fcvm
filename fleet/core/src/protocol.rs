//! Wire types: browser <-> coordinator (WebSocket `/live`), agent <-> coordinator
//! (newline-delimited JSON), and the body the agent posts to each renderer's `/scene`.
//! `fleet/PROTOCOL.md` documents them.

use crate::scene::{Keyframe, Light, Scene, Word};
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
    /// First message on every connection: this viewer's id, as `actions[].viewer` names it.
    Hello { viewer: String },
    /// A message the coordinator could not read; nothing was applied.
    Error { reason: String },
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthReport {
    pub id: String,
    pub fps: f64,
    #[serde(default)]
    pub gpu: serde_json::Value,
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
    },
    Slot(SlotReport),
    Health(HealthReport),
    /// The host got a spot interruption notice (or is otherwise going away).
    Draining {
        reason: String,
    },
    /// The host's aggregator is serving the host's region at this frame WebSocket.
    Aggregate {
        frame_url: String,
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
        }
    }

    pub fn for_renderer(&self, rect: Rect) -> RendererScene {
        RendererScene {
            epoch_unix_ms: self.epoch_unix_ms,
            rect,
            image_aspect: self.image_aspect,
            camera: self.camera.clone(),
            time_of_day: self.time_of_day,
            words: self.words.clone(),
            lights: self.lights.clone(),
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
}

/// The body of a renderer's `POST /scene`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RendererScene {
    pub epoch_unix_ms: u64,
    pub rect: Rect,
    pub image_aspect: f64,
    pub camera: Vec<Keyframe>,
    pub time_of_day: f64,
    pub words: Vec<Word>,
    pub lights: Vec<Light>,
}

#[cfg(test)]
mod tests {
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
            }],
            region: None,
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            json!({"type":"assign","slots":[{"id":"slot-1","sbdf":"0000:01:00.0","generation":1,"rect":null}],"region":null})
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
