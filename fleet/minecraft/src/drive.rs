//! Take-over: the viewer holding a tile drives its first bot from the keyboard
//! (docs/fleet/minecraft.md, "Take over"). A drive message names the keys held now, where the
//! bot looks and at most one action; the keys are a lease, dropped `LEASE_MS` after the last
//! message, so a lost key-up never leaves the bot walking.

use serde::Deserialize;

pub const LEASE_MS: u64 = 1_000;
/// The most drive messages a socket may send a second; more are refused.
pub const MAX_PER_SEC: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Forward,
    Back,
    Left,
    Right,
    Jump,
    Sprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Place the bot's block on the block it looks at.
    Place,
    /// Break the block it looks at.
    Dig,
}

/// One drive message from the page, as received (`{"type": "drive", ...}`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveMsg {
    #[serde(rename = "type")]
    pub kind: String,
    pub seq: u64,
    #[serde(default)]
    pub keys: Vec<Key>,
    /// Degrees, the game's convention (0 faces +z, 90 faces -x).
    pub yaw: f32,
    #[serde(default = "default_pitch")]
    pub pitch: f32,
    #[serde(default)]
    pub action: Option<Action>,
}

fn default_pitch() -> f32 {
    45.0
}

impl DriveMsg {
    pub fn check(&self) -> Result<(), String> {
        if !self.yaw.is_finite() || self.yaw.abs() > 3600.0 {
            return Err("yaw must be a finite angle within 3600 degrees".into());
        }
        if !self.pitch.is_finite() || self.pitch.abs() > 90.0 {
            return Err("pitch must be within 90 degrees".into());
        }
        if self.keys.len() > 6 {
            return Err("at most 6 keys".into());
        }
        Ok(())
    }
}

/// What the driven bot does now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drive {
    /// The viewer driving, from the scene; `None` while nobody holds the tile.
    pub driver: Option<String>,
    /// When the borrow ends (unix ms), from the scene: nothing drives after it, even when no
    /// new scene comes (an agent that lost its coordinator).
    pub driver_until: Option<u64>,
    pub seq: u64,
    pub keys: Vec<Key>,
    pub yaw: f32,
    pub pitch: f32,
    /// Unix ms: the keys are dropped after it.
    pub until: u64,
    /// Actions not yet carried out, oldest first, with their seq.
    pub actions: Vec<(u64, Action)>,
    /// The last message's arrival (unix ms) and the first status after it that saw the bot
    /// move or a block change, for the page's time to the first visible change.
    pub received_at: u64,
}

impl Drive {
    /// Take a message from `viewer`; refused unless it is the driver's, and newer than the last.
    pub fn take(&mut self, viewer: &str, m: &DriveMsg, now: u64) -> Result<(), String> {
        m.check()?;
        match &self.driver {
            Some(d) if d == viewer => {}
            Some(_) => return Err("another viewer holds this tile".into()),
            None => return Err("nobody holds this tile; take it over first".into()),
        }
        if self.driver_until.is_some_and(|u| now > u) {
            return Err("the borrow has ended".into());
        }
        if m.seq <= self.seq {
            return Err(format!("seq {} is not after {}", m.seq, self.seq));
        }
        self.seq = m.seq;
        self.keys = m.keys.clone();
        self.yaw = m.yaw;
        self.pitch = m.pitch;
        self.until = now + LEASE_MS;
        self.received_at = now;
        if let Some(a) = m.action
            && self.actions.len() < 8
        {
            self.actions.push((m.seq, a));
        }
        Ok(())
    }

    /// The keys held at `now`: none once the lease ran out or nobody drives.
    pub fn held(&self, now: u64) -> &[Key] {
        if self.driver.is_some() && now <= self.until && self.driver_until.is_none_or(|u| now <= u)
        {
            &self.keys
        } else {
            &[]
        }
    }

    /// A new driver (or none): everything the last one held is dropped. The same driver keeps
    /// its keys and takes the new end.
    pub fn set_driver(&mut self, driver: Option<String>, until: Option<u64>) {
        if self.driver != driver {
            *self = Drive {
                driver,
                seq: 0,
                ..Default::default()
            };
        }
        self.driver_until = until;
    }
}

/// Forward/back and left/right from the held keys: each -1, 0 or 1.
pub fn axes(keys: &[Key]) -> (i8, i8) {
    let has = |k| keys.contains(&k);
    let f = has(Key::Forward) as i8 - has(Key::Back) as i8;
    let s = has(Key::Right) as i8 - has(Key::Left) as i8;
    (f, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(seq: u64, keys: &[Key]) -> DriveMsg {
        DriveMsg {
            kind: "drive".into(),
            seq,
            keys: keys.to_vec(),
            yaw: 90.0,
            pitch: 45.0,
            action: None,
        }
    }

    #[test]
    fn only_the_driver_drives_and_only_newer_messages_count() {
        let mut d = Drive::default();
        assert!(d.take("v-1", &msg(1, &[Key::Forward]), 0).is_err());
        d.set_driver(Some("v-1".into()), None);
        assert!(d.take("v-2", &msg(1, &[Key::Forward]), 0).is_err());
        d.take("v-1", &msg(1, &[Key::Forward]), 100).unwrap();
        assert_eq!(d.held(100), &[Key::Forward]);
        assert!(d.take("v-1", &msg(1, &[]), 150).is_err());
        assert_eq!(d.held(100), &[Key::Forward]);
    }

    #[test]
    fn keys_drop_when_the_lease_runs_out_or_the_driver_changes() {
        let mut d = Drive::default();
        d.set_driver(Some("v-1".into()), None);
        d.take("v-1", &msg(4, &[Key::Left, Key::Jump]), 1000)
            .unwrap();
        assert_eq!(d.held(1000 + LEASE_MS).len(), 2);
        assert!(d.held(1001 + LEASE_MS).is_empty());
        d.set_driver(Some("v-2".into()), None);
        assert!(d.held(1001).is_empty());
        assert_eq!(d.seq, 0);
    }

    #[test]
    fn nothing_drives_after_the_borrow_ends_without_a_new_scene() {
        let mut d = Drive::default();
        d.set_driver(Some("v-1".into()), Some(5_000));
        d.take("v-1", &msg(1, &[Key::Forward]), 4_500).unwrap();
        assert_eq!(d.held(4_900), &[Key::Forward]);
        assert!(d.held(5_001).is_empty(), "keys held past the borrow's end");
        assert_eq!(
            d.take("v-1", &msg(2, &[Key::Forward]), 5_001),
            Err("the borrow has ended".to_string())
        );
    }

    #[test]
    fn a_message_is_checked_and_unknown_fields_are_refused() {
        let ok: DriveMsg = serde_json::from_str(
            r#"{"type":"drive","seq":1,"keys":["forward","jump"],"yaw":10,"action":"place"}"#,
        )
        .unwrap();
        assert!(ok.check().is_ok());
        assert_eq!(ok.pitch, 45.0);
        assert!(
            serde_json::from_str::<DriveMsg>(r#"{"type":"drive","seq":1,"yaw":0,"text":"hi"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<DriveMsg>(r#"{"type":"drive","seq":1,"yaw":0,"keys":["chat"]}"#)
                .is_err()
        );
        let mut bad = ok.clone();
        bad.pitch = 120.0;
        assert!(bad.check().is_err());
        bad.pitch = 0.0;
        bad.yaw = f32::NAN;
        assert!(bad.check().is_err());
    }

    #[test]
    fn axes_cancel_opposite_keys() {
        assert_eq!(axes(&[Key::Forward, Key::Back]), (0, 0));
        assert_eq!(axes(&[Key::Forward, Key::Right]), (1, 1));
        assert_eq!(axes(&[Key::Back, Key::Left]), (-1, -1));
    }
}
