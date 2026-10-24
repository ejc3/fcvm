//! The renderer's frame protocol (fleet/PROTOCOL.md, "Renderer"), which the aggregator both
//! reads (from tiles) and writes (to viewers): a text header, then one binary FCVF message
//! per H.264 access unit.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Crop {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

/// The first (text) message of a frame stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub codec: String,
    /// The delivered (decoded) picture, crop margins included.
    pub width: u32,
    pub height: u32,
    /// The whole image's pixel grid, and where the inner rectangle starts in it.
    #[serde(default)]
    pub full_width: u32,
    #[serde(default)]
    pub full_height: u32,
    #[serde(default)]
    pub x0: u32,
    #[serde(default)]
    pub y0: u32,
    #[serde(default = "thirty")]
    pub fps: u32,
    #[serde(default)]
    pub crop: Crop,
}

fn thirty() -> u32 {
    30
}

/// One FCVF message.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub key: bool,
    pub width: u16,
    pub height: u16,
    /// Scene seconds; frames are drawn at t = k / fps.
    pub t: f64,
    pub number: u32,
    /// One Annex-B access unit.
    pub au: Vec<u8>,
}

pub const FCVF_HEADER: usize = 22;

impl Frame {
    pub fn parse(b: &[u8]) -> Result<Frame> {
        if b.len() < FCVF_HEADER || &b[..4] != b"FCVF" {
            bail!("not an FCVF frame");
        }
        if b[4] != 1 {
            bail!("FCVF version {} is not 1", b[4]);
        }
        Ok(Frame {
            key: b[5] & 1 == 1,
            width: u16::from_le_bytes([b[6], b[7]]),
            height: u16::from_le_bytes([b[8], b[9]]),
            t: f64::from_le_bytes(b[10..18].try_into().unwrap()),
            number: u32::from_le_bytes(b[18..22].try_into().unwrap()),
            au: b[FCVF_HEADER..].to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FCVF_HEADER + self.au.len());
        out.extend_from_slice(b"FCVF");
        out.push(1);
        out.push(self.key as u8);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.t.to_le_bytes());
        out.extend_from_slice(&self.number.to_le_bytes());
        out.extend_from_slice(&self.au);
        out
    }
}

/// The frame slot of a scene time: t = k / fps.
pub fn slot(t: f64, fps: u32) -> i64 {
    (t * fps as f64).round() as i64
}

/// The WebCodecs codec string (`avc1.PPCCLL`) from the first SPS in an Annex-B unit.
pub fn codec_string(au: &[u8]) -> Option<String> {
    nal_units(au)
        .find(|n| !n.is_empty() && n[0] & 0x1f == 7 && n.len() >= 4)
        .map(|sps| format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// The NAL units of an Annex-B byte stream (start codes removed).
pub fn nal_units(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // The next unit's start code, with a leading zero byte if it has one.
            let mut e = s - 3;
            while e > 0 && b[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(b.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .map(move |(s, e)| &b[s..e.max(s)])
}

/// Whether an Annex-B unit holds an IDR slice.
pub fn is_keyframe(au: &[u8]) -> bool {
    nal_units(au).any(|n| !n.is_empty() && n[0] & 0x1f == 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fcvf_round_trips() {
        let f = Frame {
            key: true,
            width: 512,
            height: 556,
            t: 12.0 + 1.0 / 30.0,
            number: 99,
            au: vec![0, 0, 0, 1, 0x65, 1, 2],
        };
        let b = f.encode();
        assert_eq!(&b[..5], b"FCVF\x01");
        assert_eq!(Frame::parse(&b).unwrap(), f);
        assert!(Frame::parse(b"FCVX").is_err());
    }

    #[test]
    fn header_reads_the_renderer_shape() {
        let h: Header = serde_json::from_str(
            r#"{"codec":"avc1.64001f","width":512,"height":556,"full_width":1920,"full_height":1080,"x0":480,"y0":0,"fps":30,"crop":{"left":16,"top":0,"right":16,"bottom":16}}"#,
        )
        .unwrap();
        assert_eq!(h.crop.right, 16);
        assert_eq!(h.x0, 480);
    }

    #[test]
    fn nal_units_and_codec_string() {
        let au = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAA, // SPS: High, level 4.0
            0, 0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 1, 0x65, 0x88, 0x80, // IDR slice
        ];
        let units: Vec<&[u8]> = nal_units(&au).collect();
        assert_eq!(units.len(), 3);
        assert_eq!(units[1], &[0x68, 0xBB]);
        assert_eq!(codec_string(&au).as_deref(), Some("avc1.640028"));
        assert!(is_keyframe(&au));
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 0x9a]));
    }

    #[test]
    fn slots_follow_the_thirtieth_grid() {
        assert_eq!(slot(286.6, 30), 8598);
        assert_eq!(slot(1.0 / 30.0, 30), 1);
    }
}
