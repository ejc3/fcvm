//! Windows mode: disposable Windows sandboxes on x86 hosts (`docs/fleet/windows-sandboxes.md`).
//! Each sandbox is a slot of `Fleet::windows` (`win-<n>`), apart from the wall's slots and the
//! phones: a Cloud Hypervisor clone of its host's gold snapshot, an idle logged-on desktop. A
//! viewer borrows one with `claim_windows` and sends it the fixed actions below; nothing else a
//! viewer sends reaches Windows. There is no free input of any kind.

use crate::tiling::Rect;
use serde::{Deserialize, Serialize};

/// The line every tile and the take-over panel show (docs/fleet/windows-sandboxes.md,
/// "Edition and licensing").
pub const LABEL: &str = "Windows Server 2025 evaluation, for demonstration only";

/// The take-over actions, the only input a viewer gives a sandbox. The coordinator refuses any
/// other name; the agent posts each to the sandbox's bridge, which sends its fixed input
/// sequence over Remote Desktop, except `reset`, which is a relaunch of the sandbox's VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsAction {
    /// Opens the Start menu.
    StartMenu,
    /// Opens Notepad with a fixed line naming the sandbox and its restore time.
    Notepad,
    /// Opens File Explorer on This PC.
    Explorer,
    /// Opens Task Manager on its Performance tab.
    TaskManager,
    /// Opens `winver`: the edition, the build and the evaluation's expiry.
    About,
    /// Opens PowerShell running `Get-ComputerInfo` for the OS fields.
    Terminal,
    /// Closes every window the actions opened.
    CloseAll,
    /// Restores the sandbox from the gold snapshot.
    Reset,
    /// An action this build does not know, from a newer coordinator: carried out as nothing,
    /// so the rest of the assignment it came in still reaches the agent. Never in `ALL`, so
    /// never accepted from a viewer.
    #[serde(other)]
    Unknown,
}

impl WindowsAction {
    pub const ALL: [WindowsAction; 8] = [
        WindowsAction::StartMenu,
        WindowsAction::Notepad,
        WindowsAction::Explorer,
        WindowsAction::TaskManager,
        WindowsAction::About,
        WindowsAction::Terminal,
        WindowsAction::CloseAll,
        WindowsAction::Reset,
    ];
    pub const NAMES: &'static str =
        "start_menu, notepad, explorer, task_manager, about, terminal, close_all, reset";

    pub fn as_str(self) -> &'static str {
        match self {
            WindowsAction::StartMenu => "start_menu",
            WindowsAction::Notepad => "notepad",
            WindowsAction::Explorer => "explorer",
            WindowsAction::TaskManager => "task_manager",
            WindowsAction::About => "about",
            WindowsAction::Terminal => "terminal",
            WindowsAction::CloseAll => "close_all",
            WindowsAction::Reset => "reset",
            WindowsAction::Unknown => "unknown",
        }
    }

    /// The action of exactly this name; `None` for anything else.
    pub fn parse(name: &str) -> Option<WindowsAction> {
        WindowsAction::ALL.into_iter().find(|a| a.as_str() == name)
    }

    /// The actions a sandbox offers, in `ALL`'s order: those its host's hello lists (names
    /// this build does not know are dropped), and `reset`, the coordinator's own, always.
    pub fn offered(listed: &[String]) -> Vec<WindowsAction> {
        WindowsAction::ALL
            .into_iter()
            .filter(|a| *a == WindowsAction::Reset || listed.iter().any(|n| n == a.as_str()))
            .collect()
    }
}

/// What a sandbox has that a wall slot does not.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WindowsSlot {
    /// Its place among its host's sandboxes (0-based, below the hello's `windows`): the
    /// agent's ports and address for it.
    pub index: u32,
    pub claim: Option<WindowsClaim>,
    /// Actions for the agent to carry out, newest last, kept for `ACTION_KEEP_MS`; the agent
    /// carries out each `seq` once.
    #[serde(default)]
    pub actions: Vec<WindowsActionReq>,
}

/// A viewer's borrowed sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowsClaim {
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
pub struct WindowsActionReq {
    /// Fleet-wide, increasing.
    pub seq: u64,
    pub action: WindowsAction,
    pub at: u64,
    /// The sandbox's generation it is for: a reset sandbox drops the actions of the one before.
    pub generation: u64,
}

/// The gold snapshot's state on a host: being made, made, or not made (`gold_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gold {
    Building,
    Ready,
    Failed,
}

/// A host's Windows image as its agent reports it (`windows_image`): the image's manifest
/// (fleet/windows/build-image.sh), its age, the evaluation rules' verdict and the gold
/// snapshot's state. The agent computes the age and the verdict on its own clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowsImage {
    pub id: String,
    pub edition: String,
    pub build: String,
    /// ISO 8601, as the manifest has them.
    pub installed_at: String,
    pub eval_expires: String,
    /// Whole days since `installed_at`.
    pub age_days: u64,
    /// From 150 days after the installation: the image needs rebuilding soon.
    #[serde(default)]
    pub warning: Option<String>,
    /// From 170 days, or within 7 days of `eval_expires`: no sandbox starts from it.
    #[serde(default)]
    pub refused: Option<String>,
    pub gold: Gold,
    #[serde(default)]
    pub gold_error: Option<String>,
}

/// The last `windows_reset` of every sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetAll {
    pub clicked_at: u64,
    pub by: String,
    /// The sandboxes it reset.
    pub sandboxes: usize,
}

/// A sandbox's name across connections and coordinator restarts: its host and its index there.
pub fn key(host: &str, index: u32) -> String {
    format!("{host}/{index}")
}

/// How long an action stays in the assignment (an agent that reconnects within it still
/// carries it out).
pub const ACTION_KEEP_MS: u64 = 30_000;
/// The least time between two actions of one borrowed sandbox.
pub const ACTION_INTERVAL_MS: u64 = 1_000;
/// The least time between two resets of every sandbox from one client.
pub const RESET_ALL_INTERVAL_MS: u64 = 10_000;
/// A sandbox's desktop, width / height (1280 x 720).
pub const TILE_ASPECT: f64 = 16.0 / 9.0;

/// Rows and columns for `n` sandboxes: one row of up to four, then rows of equal length.
pub fn grid(n: usize) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let rows = n.div_ceil(4);
    (rows, n.div_ceil(rows))
}

/// The sandboxes' image: each sandbox's rectangle, in order, and the image's width / height.
pub fn layout(n: usize) -> (Vec<Rect>, f64) {
    let (rows, cols) = grid(n);
    if n == 0 {
        return (Vec::new(), TILE_ASPECT);
    }
    let rects = (0..n)
        .map(|i| Rect {
            x: (i % cols) as f64 / cols as f64,
            y: (i / cols) as f64 / rows as f64,
            w: 1.0 / cols as f64,
            h: 1.0 / rows as f64,
        })
        .collect();
    (rects, cols as f64 * TILE_ASPECT / rows as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_one_name_and_nothing_else_parses() {
        for a in WindowsAction::ALL {
            assert_eq!(WindowsAction::parse(a.as_str()), Some(a));
            assert!(WindowsAction::NAMES.contains(a.as_str()));
            assert_eq!(
                serde_json::to_value(a).unwrap(),
                serde_json::json!(a.as_str())
            );
        }
        assert_eq!(
            WindowsAction::NAMES,
            WindowsAction::ALL.map(|a| a.as_str()).join(", ")
        );
        for bad in [
            "",
            "Notepad",
            "notepad ",
            "cmd",
            "type",
            "click",
            "tap",
            "notepad;calc",
            "unknown",
        ] {
            assert_eq!(WindowsAction::parse(bad), None, "{bad:?}");
        }
        assert!(!WindowsAction::ALL.contains(&WindowsAction::Unknown));
    }

    #[test]
    fn an_action_from_a_newer_coordinator_parses_as_unknown() {
        let req: WindowsActionReq = serde_json::from_value(serde_json::json!({
            "seq": 4, "action": "open_paint", "at": 1, "generation": 2
        }))
        .unwrap();
        assert_eq!(req.action, WindowsAction::Unknown);
    }

    #[test]
    fn a_sandbox_offers_what_its_host_lists_and_reset() {
        let listed: Vec<String> = ["notepad", "about", "open_paint"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            WindowsAction::offered(&listed),
            vec![
                WindowsAction::Notepad,
                WindowsAction::About,
                WindowsAction::Reset
            ]
        );
        assert_eq!(WindowsAction::offered(&[]), vec![WindowsAction::Reset]);
    }

    #[test]
    fn sandboxes_are_laid_out_in_rows_of_up_to_four() {
        assert_eq!(grid(0), (0, 0));
        assert_eq!(grid(1), (1, 1));
        assert_eq!(grid(4), (1, 4));
        assert_eq!(grid(5), (2, 3));
        assert_eq!(grid(8), (2, 4));
        assert_eq!(grid(9), (3, 3));
        let (rects, aspect) = layout(6);
        assert_eq!(rects.len(), 6);
        assert_eq!(aspect, 3.0 * TILE_ASPECT / 2.0);
        assert_eq!(
            rects[4],
            Rect {
                x: 1.0 / 3.0,
                y: 0.5,
                w: 1.0 / 3.0,
                h: 0.5
            }
        );
        // Each tile is 16:9 in the image: its share of the width over its share of the height.
        let r = rects[0];
        assert!((r.w * aspect / r.h - TILE_ASPECT).abs() < 1e-12);
    }
}
