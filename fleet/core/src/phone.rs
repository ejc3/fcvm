//! Phones mode: Android phones on GPU hosts (`docs/fleet/ARCHITECTURE.md`, "Phones mode").
//! Each phone is a slot of `Fleet::phones` (`phone-<n>`), apart from the wall's slots: the
//! wall's layout, races, budget and claims never see one. A viewer borrows a phone with
//! `claim_phone` and sends it the fixed actions below; nothing else a viewer sends reaches
//! a phone.

use crate::tiling::Rect;
use serde::{Deserialize, Serialize};

/// The take-over actions, the only input a viewer gives a phone. The coordinator refuses
/// any other name; the agent maps each to one fixed command in the phone container
/// (`phone.sh action <name>`), except `reboot_phone`, which is a relaunch of the phone's VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneAction {
    /// The app's other screen: the scene, or its info screen.
    SwitchScreen,
    /// Turn the phone a quarter turn.
    Rotate,
    /// Android's dark theme on or off.
    DarkMode,
    /// The robots dance, or wave.
    Dance,
    Wave,
    /// A GPU benchmark inside the app.
    GpuBenchmark,
    /// Stop the app and start it again.
    RestartApp,
    /// Restore the phone's VM from its snapshot.
    RebootPhone,
}

impl PhoneAction {
    pub const ALL: [PhoneAction; 8] = [
        PhoneAction::SwitchScreen,
        PhoneAction::Rotate,
        PhoneAction::DarkMode,
        PhoneAction::Dance,
        PhoneAction::Wave,
        PhoneAction::GpuBenchmark,
        PhoneAction::RestartApp,
        PhoneAction::RebootPhone,
    ];
    pub const NAMES: &'static str =
        "switch_screen, rotate, dark_mode, dance, wave, gpu_benchmark, restart_app, reboot_phone";

    pub fn as_str(self) -> &'static str {
        match self {
            PhoneAction::SwitchScreen => "switch_screen",
            PhoneAction::Rotate => "rotate",
            PhoneAction::DarkMode => "dark_mode",
            PhoneAction::Dance => "dance",
            PhoneAction::Wave => "wave",
            PhoneAction::GpuBenchmark => "gpu_benchmark",
            PhoneAction::RestartApp => "restart_app",
            PhoneAction::RebootPhone => "reboot_phone",
        }
    }

    /// The action of exactly this name; `None` for anything else.
    pub fn parse(name: &str) -> Option<PhoneAction> {
        PhoneAction::ALL.into_iter().find(|a| a.as_str() == name)
    }
}

/// What a phone has that a wall slot does not.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PhoneSlot {
    pub claim: Option<PhoneClaim>,
    /// Actions for the agent to carry out, newest last, kept for `ACTION_KEEP_MS`; the agent
    /// carries out each `seq` once.
    #[serde(default)]
    pub actions: Vec<PhoneActionReq>,
}

/// A viewer's borrowed phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhoneClaim {
    pub session: String,
    pub viewer: String,
    pub client: String,
    pub claimed_at: u64,
    pub expires_at: u64,
    /// The last action it sent, for the rate limit.
    #[serde(default)]
    pub last_action_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhoneActionReq {
    /// Fleet-wide, increasing.
    pub seq: u64,
    pub action: PhoneAction,
    pub at: u64,
    /// The phone's generation it is for: a relaunched phone drops the actions of the one before.
    pub generation: u64,
}

/// Every phone shows its app's top and tab bars from scene time `t` (`on`) or hides them
/// (Robot Park's `chrome` keys, PROTOCOL.md "Phone app"): the phones zoomed out, together.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChromeKey {
    pub t: f64,
    pub on: bool,
}

/// The phones' zoom, shared by every viewer: zoomed out, every phone shows its app's bars and
/// the page draws each in a phone body; zoomed in, the stitched picture.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PhonesZoom {
    pub out: bool,
    /// When the current state took effect (Unix ms).
    pub at: Option<u64>,
    /// The keys every phone is sent, oldest first, at most ZOOM_KEYS.
    pub keys: Vec<ChromeKey>,
}

pub const ZOOM_KEYS: usize = 16;
/// A zoom's key is this far ahead of the scene clock, so every phone has it before its time.
pub const ZOOM_LEAD_SECS: f64 = 1.0;
/// The least time between two zoom changes from one client.
pub const ZOOM_INTERVAL_MS: u64 = 2_000;

/// How long an action stays in the assignment (an agent that reconnects within it still
/// carries it out).
pub const ACTION_KEEP_MS: u64 = 30_000;
/// The least time between two actions of one borrowed phone.
pub const ACTION_INTERVAL_MS: u64 = 1_000;

/// Rows and columns for `n` phones: one row of up to six, then rows of equal length.
pub fn grid(n: usize) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let rows = n.div_ceil(6);
    (rows, n.div_ceil(rows))
}

/// The phones' image: each phone's rectangle, in order, and the image's width / height,
/// for phones whose scene is `phone_aspect` (width / height) each.
pub fn layout(n: usize, phone_aspect: f64) -> (Vec<Rect>, f64) {
    let (rows, cols) = grid(n);
    if n == 0 {
        return (Vec::new(), phone_aspect);
    }
    let rects = (0..n)
        .map(|i| Rect {
            x: (i % cols) as f64 / cols as f64,
            y: (i / cols) as f64 / rows as f64,
            w: 1.0 / cols as f64,
            h: 1.0 / rows as f64,
        })
        .collect();
    (rects, cols as f64 * phone_aspect / rows as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_one_name_and_nothing_else_parses() {
        for a in PhoneAction::ALL {
            assert_eq!(PhoneAction::parse(a.as_str()), Some(a));
            assert!(PhoneAction::NAMES.contains(a.as_str()));
            assert_eq!(
                serde_json::to_value(a).unwrap(),
                serde_json::json!(a.as_str())
            );
        }
        for bad in [
            "",
            "Rotate",
            "rotate ",
            "shell",
            "reboot",
            "dark_mode;id",
            "am start",
        ] {
            assert_eq!(PhoneAction::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn phones_are_laid_out_in_rows_of_up_to_six() {
        assert_eq!(grid(1), (1, 1));
        assert_eq!(grid(6), (1, 6));
        assert_eq!(grid(7), (2, 4));
        assert_eq!(grid(8), (2, 4));
        assert_eq!(grid(13), (3, 5));
        let (rects, aspect) = layout(8, 0.5);
        assert_eq!(rects.len(), 8);
        assert_eq!(aspect, 4.0 * 0.5 / 2.0);
        assert_eq!(
            rects[5],
            Rect {
                x: 0.25,
                y: 0.5,
                w: 0.25,
                h: 0.5
            }
        );
    }
}
