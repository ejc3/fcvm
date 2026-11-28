//! The tile's world as the page draws it: a square region of block columns around the world
//! spawn, each its top block's material (`palette`) and height. The page receives the whole
//! grid once (`map`) and then the columns that changed (`delta`). Beside each column's top block
//! the grid holds its ground: the highest block that is not leaves, a plant or water, which is
//! what the page draws under a canopy and a lake.

use base64::Engine;

/// A column: the material of its top block and that block's height, `y + 64` held to 0..=255
/// (the overworld starts at y = -64; a column above y = 191 reads 255).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    pub material: u8,
    pub height: u8,
}

impl Cell {
    pub fn new(material: u8, y: i32) -> Self {
        Cell {
            material,
            height: (y + 64).clamp(0, 255) as u8,
        }
    }
}

/// The region, in block coordinates: columns `x0..x0+w`, `z0..z0+h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Region {
    pub x0: i32,
    pub z0: i32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    /// The `side` x `side` columns centred on the chunk holding `(x, z)`, aligned to chunks.
    pub fn around(x: i32, z: i32, side: u32) -> Region {
        let half = (side / 2) as i32;
        let cx = x.div_euclid(16) * 16 + 8;
        let cz = z.div_euclid(16) * 16 + 8;
        let x0 = (cx - half).div_euclid(16) * 16;
        let z0 = (cz - half).div_euclid(16) * 16;
        Region {
            x0,
            z0,
            w: side,
            h: side,
        }
    }

    #[cfg(test)]
    pub fn contains(&self, x: i32, z: i32) -> bool {
        x >= self.x0 && z >= self.z0 && x < self.x0 + self.w as i32 && z < self.z0 + self.h as i32
    }

    pub fn len(&self) -> usize {
        (self.w * self.h) as usize
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub region: Region,
    pub cells: Vec<Cell>,
    /// Each column's ground (`palette::is_cover`): its top block again where nothing covers it.
    pub ground: Vec<Cell>,
    /// How many columns have been read from a loaded chunk (the rest are unknown).
    pub known: usize,
}

impl Grid {
    pub fn new(region: Region) -> Self {
        Grid {
            region,
            cells: vec![Cell::default(); region.len()],
            ground: vec![Cell::default(); region.len()],
            known: 0,
        }
    }

    /// The grid as the `map` message carries it: material then height for each column, row by
    /// row (z), base64.
    #[cfg(test)]
    pub fn encode(&self) -> String {
        encode(&self.cells)
    }

    /// The `map` message: the top blocks and, in the same layout, the ground.
    pub fn map_message(&self, seq: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "map", "seq": seq, "region": self.region,
            "cells": encode(&self.cells), "ground": encode(&self.ground),
        })
    }

    /// The columns of `next` that differ from `self`: `[index, material, height]`.
    pub fn delta(&self, next: &Grid) -> Vec<[u32; 3]> {
        if self.region != next.region {
            return Vec::new();
        }
        changed(&self.cells, &next.cells)
    }

    /// The columns whose ground differs in `next`, as `delta` gives the top blocks.
    pub fn ground_delta(&self, next: &Grid) -> Vec<[u32; 3]> {
        if self.region != next.region {
            return Vec::new();
        }
        changed(&self.ground, &next.ground)
    }

    /// The `delta` message from `self` to `next`: `None` when neither layer changed, and
    /// `ground` only when the ground did.
    pub fn delta_message(&self, next: &Grid, seq: u64) -> Option<serde_json::Value> {
        let (cells, ground) = (self.delta(next), self.ground_delta(next));
        if cells.is_empty() && ground.is_empty() {
            return None;
        }
        let mut m = serde_json::json!({"type": "delta", "seq": seq, "cells": cells});
        if !ground.is_empty() {
            m["ground"] = serde_json::json!(ground);
        }
        Some(m)
    }
}

fn encode(cells: &[Cell]) -> String {
    let mut b = Vec::with_capacity(cells.len() * 2);
    for c in cells {
        b.push(c.material);
        b.push(c.height);
    }
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn changed(a: &[Cell], b: &[Cell]) -> Vec<[u32; 3]> {
    a.iter()
        .zip(b)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, (_, b))| [i as u32, b.material as u32, b.height as u32])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_is_chunk_aligned_around_the_spawn_chunk() {
        let r = Region::around(3, -5, 128);
        // The spawn chunk (0, -1) is one of the four in the middle.
        assert_eq!(
            r,
            Region {
                x0: -64,
                z0: -80,
                w: 128,
                h: 128
            }
        );
        assert!(r.contains(3, -5));
        assert!(r.contains(-64, -80) && !r.contains(64, 0) && r.contains(63, 47));
        assert_eq!(r.x0 % 16, 0);
    }

    #[test]
    fn heights_are_held_to_a_byte() {
        assert_eq!(Cell::new(2, 63).height, 127);
        assert_eq!(Cell::new(2, -64).height, 0);
        assert_eq!(Cell::new(2, -100).height, 0);
        assert_eq!(Cell::new(2, 300).height, 255);
    }

    #[test]
    fn the_delta_names_only_changed_columns_and_the_encoding_is_two_bytes_each() {
        let r = Region {
            x0: 0,
            z0: 0,
            w: 4,
            h: 2,
        };
        let a = Grid::new(r);
        let mut b = a.clone();
        b.cells[5] = Cell::new(46, 70);
        assert_eq!(a.delta(&b), vec![[5, 46, 134]]);
        assert!(a.delta(&a).is_empty());
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b.encode())
            .unwrap();
        assert_eq!(raw.len(), 16);
        assert_eq!(&raw[10..12], &[46, 134]);
    }

    #[test]
    fn the_ground_travels_beside_the_top_blocks_and_only_when_it_changes() {
        let r = Region {
            x0: 0,
            z0: 0,
            w: 4,
            h: 2,
        };
        let mut a = Grid::new(r);
        // column 5: a lake, its bed three blocks down
        a.cells[5] = Cell::new(1, 62);
        a.ground[5] = Cell::new(4, 59);
        let m = a.map_message(7);
        let raw = |k: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(m[k].as_str().unwrap())
                .unwrap()
        };
        assert_eq!(
            (m["type"].as_str(), m["seq"].as_u64()),
            (Some("map"), Some(7))
        );
        // the same layout and length as the top blocks
        assert_eq!((raw("cells").len(), raw("ground").len()), (16, 16));
        assert_eq!(
            (&raw("cells")[10..12], &raw("ground")[10..12]),
            (&[1u8, 126][..], &[4u8, 123][..])
        );
        // nothing changed: no message
        assert!(a.delta_message(&a.clone(), 8).is_none());
        // a block placed on dry ground changes the top block and the ground with it
        let mut b = a.clone();
        b.cells[2] = Cell::new(46, 70);
        b.ground[2] = Cell::new(46, 70);
        let d = a.delta_message(&b, 8).unwrap();
        assert_eq!(d["cells"], serde_json::json!([[2, 46, 134]]));
        assert_eq!(d["ground"], serde_json::json!([[2, 46, 134]]));
        // the lake's surface changes (ice) and its bed does not: no `ground` in the message
        let mut c = a.clone();
        c.cells[5] = Cell::new(8, 62);
        let d = a.delta_message(&c, 9).unwrap();
        assert_eq!(d["cells"], serde_json::json!([[5, 8, 126]]));
        assert!(d.get("ground").is_none());
        // another region: no delta at all
        let other = Grid::new(Region { x0: 16, ..r });
        assert!(a.delta_message(&other, 10).is_none());
    }
}
