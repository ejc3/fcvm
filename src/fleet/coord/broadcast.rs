//! The broadcast: one composite of the whole wall, encoded once and pushed to a live ingest
//! (Amazon IVS), for viewers beyond what direct streaming serves (docs/fleet/ARCHITECTURE.md,
//! "Broadcast mode").
//!
//! The coordinator runs an aggregator for it beside itself (`--broadcast-listen`), like the
//! EC2 half's, and gives it a layout of the whole image: every placed tile. That aggregator
//! pushes its output with `--push-url-env`, every frame stamped with its scene time.

use crate::fleet::aggregate::{Layout, TileSpec};
use anyhow::{Context, Result};
use fleet_core::state::State;
use fleet_core::tiling::Rect;
use serde::Deserialize;

/// The environment variable the broadcast aggregator reads its ingest URL from.
pub const PUSH_URL_ENV: &str = "FCVM_FLEET_BROADCAST_INGEST";

/// The broadcast aggregator's settings before `--broadcast-arg`, on a machine with fewer than
/// [`FULL_HD_CPUS`] CPUs: a 720-line image (H.264 High, a keyframe every 2 s, at most 4.5
/// Mbit/s), which IVS's standard channel transcodes into 720p (2.4 Mbit/s), 480p, 360p and
/// 160p renditions; tiles at their standard quality. Measured at 1.4 of a 2 vCPU Fargate
/// task's cores (CloudWatch, 2026-11-03, with the coordinator's own work).
pub const DEFAULT_ARGS_HD: &[&str] = &[
    "--image-height",
    "720",
    "--tile-query",
    "q=std",
    "--maxrate-kbps",
    "4500",
    "--crf",
    "21",
    "--preset",
    "veryfast",
];

/// With [`FULL_HD_CPUS`] CPUs or more: a 1080-line image at CRF 18 and at most 7.5 Mbit/s,
/// under IVS's 8.5 Mbit/s limit for a standard channel's input. IVS passes a 1080p input
/// through as its top rendition (a 720p input is transcoded to 2.4 Mbit/s), and adds 720p,
/// 480p, 360p and 160p. The encode and the 16 tile decodes take 1.96 times the CPU of the
/// 720-line set (76 against 39 ms a frame on one Graviton core), about 2.7 cores of the
/// coordinator's task, which a 2 vCPU task does not have. Measured in
/// docs/fleet/ARCHITECTURE.md, "The composite".
pub const DEFAULT_ARGS_FULL_HD: &[&str] = &[
    "--image-height",
    "1080",
    "--tile-query",
    "q=std",
    "--maxrate-kbps",
    "7500",
    "--crf",
    "18",
    "--preset",
    "veryfast",
];

/// The CPUs from which the broadcast is 1080 lines tall.
pub const FULL_HD_CPUS: usize = 4;

/// The broadcast's default settings for a machine with `cpus` CPUs.
pub fn default_args(cpus: usize) -> &'static [&'static str] {
    if cpus >= FULL_HD_CPUS {
        DEFAULT_ARGS_FULL_HD
    } else {
        DEFAULT_ARGS_HD
    }
}

/// The whole image.
pub const WHOLE: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1.0,
    h: 1.0,
};

/// The broadcast's settings: the ingest and the playback side of one IVS channel. Kept in
/// Secrets Manager (`--broadcast-secret`) or a file (`--broadcast-file`), never on a command
/// line.
#[derive(Debug, Clone, Deserialize)]
pub struct BroadcastSecret {
    /// `rtmps://<ingest endpoint>:443/app/<stream key>`.
    pub ingest_url: String,
    /// The channel's playback URL (`https://<...>.m3u8`).
    #[serde(default)]
    pub playback_url: Option<String>,
    /// A private channel's ARN and its playback key pair's private key (PKCS#8 PEM, P-384):
    /// every viewer's playback URL is then signed. Without them the URL is given as it is.
    #[serde(default)]
    pub channel_arn: Option<String>,
    #[serde(default)]
    pub playback_key_pem: Option<String>,
}

impl BroadcastSecret {
    /// How viewers play it: signed when the secret has the channel's key, open otherwise.
    pub fn playback(&self) -> Result<Option<super::admission::Playback>> {
        use super::admission::{Playback, PlaybackSigner};
        let Some(url) = &self.playback_url else {
            return Ok(None);
        };
        Ok(Some(match (&self.channel_arn, &self.playback_key_pem) {
            (Some(arn), Some(pem)) => Playback::Signed(Box::new(PlaybackSigner::new(
                arn.clone(),
                url.clone(),
                pem,
            )?)),
            (None, None) => Playback::Open(url.clone()),
            _ => anyhow::bail!(
                "the broadcast secret needs both channel_arn and playback_key_pem, or neither"
            ),
        }))
    }

    pub fn parse(json: &str) -> Result<Self> {
        let s: BroadcastSecret =
            serde_json::from_str(json).context("the broadcast secret is not the expected JSON")?;
        anyhow::ensure!(
            s.ingest_url.starts_with("rtmps://") || s.ingest_url.starts_with("rtmp://"),
            "the broadcast ingest URL is not rtmp(s)"
        );
        Ok(s)
    }
}

/// The broadcast aggregator's layout, or `None` while there is nothing to show (the fleet
/// is asleep: no tile is placed). Every placed tile is read directly, at the broadcast's own
/// height: each renderer then encodes a small stream for it, which one decoder thread keeps
/// up with. A region aggregator's stream is a near-lossless region a whole image tall, which
/// one decoder thread does not (a 1280x720 renderer stream at `q=hi` fell 3 s behind on a
/// g4dn.2xlarge).
pub fn layout(state: &State) -> Option<Layout> {
    let f = &state.fleet;
    let placed = |s: &&fleet_core::state::Slot| match s.half {
        fleet_core::tiling::Half::Fcvm => s
            .host
            .as_ref()
            .is_some_and(|h| f.hosts.get(h).is_some_and(|h| h.region.is_some())),
        fleet_core::tiling::Half::Ec2 => f.layout.ec2.is_some(),
    };
    let tiles: Vec<TileSpec> = f
        .slots
        .values()
        .chain(f.ec2.values())
        .filter(placed)
        .filter_map(|s| tile(state, s))
        .collect();
    if tiles.is_empty() {
        return None;
    }
    Some(Layout {
        epoch_unix_ms: f.scene.epoch_unix_ms,
        image_aspect: state.config.image_aspect,
        region: WHOLE,
        tiles,
        generation: f.layout.generation,
        from_frame: f.layout.from_frame,
        admit_keys: Vec::new(),
    })
}

/// One tile read directly: its published IPv6 name, which reaches the VM without a proxy
/// (the IPv4 name of an fcvm tile goes through Cloudflare). A tile lent to a visitor is
/// black: its GPU draws that visitor's private camera over the whole image.
fn tile(state: &State, s: &fleet_core::state::Slot) -> Option<TileSpec> {
    let (v6, v4) = fleet_core::view::frame_urls(state, s);
    Some(TileSpec {
        id: s.id.clone(),
        rect: s.rect?,
        url: if s.claim.is_some() { None } else { v6.or(v4) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_secret_needs_an_rtmp_ingest() {
        let s =
            BroadcastSecret::parse(r#"{"ingest_url":"rtmps://ingest.example:443/app/k"}"#).unwrap();
        assert!(s.ingest_url.ends_with("/app/k"));
        assert!(BroadcastSecret::parse(r#"{"ingest_url":"https://x"}"#).is_err());
        assert!(BroadcastSecret::parse("{}").is_err());
    }

    /// `--broadcast-arg` changes a default: the child parses the defaults then the overrides,
    /// and the last value of a repeated option wins (a conflict would stop the child at
    /// start, again and again).
    #[test]
    fn a_broadcast_arg_overrides_a_default() {
        use clap::Parser;
        let mut argv = vec!["fcvm", "fleet", "aggregate", "--listen", "127.0.0.1:1"];
        argv.extend(DEFAULT_ARGS_HD);
        argv.extend(["--crf", "23", "--image-height", "1080"]);
        let cli =
            crate::cli::Cli::try_parse_from(argv).expect("the broadcast's command line parses");
        let crate::cli::Commands::Fleet(f) = cli.cmd else {
            panic!("not a fleet command")
        };
        let crate::cli::FleetCommand::Aggregate(a) = f.cmd else {
            panic!("not the aggregator")
        };
        assert_eq!(a.crf, 23);
        assert_eq!(a.image_height, 1080);
        assert_eq!(a.maxrate_kbps, 4500, "a default left alone stays");
    }

    /// A task with 4 CPUs broadcasts 1080 lines, a smaller one 720 (a 1080-line broadcast
    /// takes about 2.7 cores, over a 2 vCPU task). Both sets parse, and the 1080 set stays
    /// under IVS's 8.5 Mbit/s input limit for a standard channel.
    #[test]
    fn the_broadcast_is_1080_lines_from_four_cpus() {
        use clap::Parser;
        let parse = |cpus: usize| {
            let mut argv = vec!["fcvm", "fleet", "aggregate", "--listen", "127.0.0.1:1"];
            argv.extend(default_args(cpus));
            let cli = crate::cli::Cli::try_parse_from(argv).expect("the defaults parse");
            let crate::cli::Commands::Fleet(f) = cli.cmd else {
                panic!("not a fleet command")
            };
            let crate::cli::FleetCommand::Aggregate(a) = f.cmd else {
                panic!("not the aggregator")
            };
            a
        };
        for cpus in [1, 2, 3] {
            let a = parse(cpus);
            assert_eq!(
                (a.image_height, a.maxrate_kbps, a.crf),
                (720, 4500, 21),
                "{cpus} CPUs"
            );
        }
        for cpus in [4, 8] {
            let a = parse(cpus);
            assert_eq!(
                (a.image_height, a.maxrate_kbps, a.crf),
                (1080, 7500, 18),
                "{cpus} CPUs"
            );
            assert!(a.maxrate_kbps < 8500, "over IVS's input limit");
        }
    }
}
