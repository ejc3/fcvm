//! What a bot that nobody drives does next, planned over the map's grid: where it walks (dry
//! land only, so no bot ends standing in a lake) and what it builds there (a wall, the outline
//! of a house, a tower). Pure: the grid and a few numbers in, columns to raise out.

use crate::map::{Cell, Grid};
use crate::palette;

/// What a bot builds once it has walked somewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// One column beside the bot, raised `rise` blocks.
    Tower,
    /// `len` columns in a line, raised to one level.
    Wall,
    /// The outline of a `len` x `len` house, raised to one level, with a gap for a door.
    House,
}

/// One column of a build: the bot stands at `stand` and raises `(x, z)` until its top block is
/// at `top` (a world y).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub x: i32,
    pub z: i32,
    pub stand: (i32, i32),
    pub top: i32,
}

/// A build raises a column this many blocks at most: a bot reaches the top of a column that is
/// three over the ground it stands on, from beside it.
pub const MAX_RISE: i32 = 3;
/// Ground this far above or below the build's first column is not part of it.
const LEVEL: i32 = 1;

fn cell(grid: &Grid, x: i32, z: i32) -> Option<Cell> {
    let r = grid.region;
    if x < r.x0 || z < r.z0 || x >= r.x0 + r.w as i32 || z >= r.z0 + r.h as i32 {
        return None;
    }
    grid.cells
        .get(((z - r.z0) * r.w as i32 + (x - r.x0)) as usize)
        .copied()
}

/// The top block's y of a column a bot can stand on and build on: inside the map, read from a
/// loaded chunk, and not water, lava or a tree's crown. `None` for every other column.
pub fn land(grid: &Grid, x: i32, z: i32) -> Option<i32> {
    let c = cell(grid, x, z)?;
    palette::is_land(c.material).then_some(c.height as i32 - 64)
}

/// Whether a bot standing in column `(x, z)` stands in water (the column's top block is water).
pub fn in_water(grid: &Grid, x: i32, z: i32) -> bool {
    cell(grid, x, z).is_some_and(|c| c.material == palette::index("water"))
}

/// The dry column nearest `(x, z)`, searched in growing squares up to `reach` columns away.
pub fn nearest_land(grid: &Grid, x: i32, z: i32, reach: i32) -> Option<(i32, i32)> {
    for r in 0..=reach {
        let mut best: Option<((i32, i32), i32)> = None;
        for dz in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dz.abs()) != r {
                    continue;
                }
                if land(grid, x + dx, z + dz).is_some() {
                    let d = dx * dx + dz * dz;
                    if best.is_none_or(|(_, b)| d < b) {
                        best = Some(((x + dx, z + dz), d));
                    }
                }
            }
        }
        if let Some((p, _)) = best {
            return Some(p);
        }
    }
    None
}

/// Where to walk next from `(x, z)`: a dry column within `reach` whose eight neighbours are dry
/// too (room to build), from the `picks` given (offsets a caller draws at random, each in
/// `-reach..=reach`). The first pick that fits; else the nearest dry column; else nowhere.
pub fn walk_target(
    grid: &Grid,
    x: i32,
    z: i32,
    reach: i32,
    picks: &[(i32, i32)],
) -> Option<(i32, i32)> {
    let roomy = |px: i32, pz: i32| {
        (-1..=1).all(|dz| (-1..=1).all(|dx| land(grid, px + dx, pz + dz).is_some()))
    };
    picks
        .iter()
        .map(|&(dx, dz)| (x + dx.clamp(-reach, reach), z + dz.clamp(-reach, reach)))
        .find(|(px, pz)| (*px, *pz) != (x, z) && roomy(*px, *pz))
        .or_else(|| nearest_land(grid, x, z, reach.max(48)))
}

/// The four directions a build can run: +x, +z, -x, -z.
pub const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

/// The columns of `shape`, for a bot standing at `(x, z)` facing `dir` (an index into
/// [`DIRS`]): each with where the bot stands to raise it and the height it is raised to. Only
/// columns on dry ground near the level of the first, with a dry place to stand, are part of
/// it; a shape with nothing left to raise is empty.
pub fn plan(
    shape: Shape,
    grid: &Grid,
    x: i32,
    z: i32,
    dir: usize,
    len: i32,
    rise: i32,
) -> Vec<Step> {
    let (fx, fz) = DIRS[dir % 4];
    // To the bot's right when it faces `dir`.
    let (rx, rz) = (-fz, fx);
    let rise = rise.clamp(1, MAX_RISE);
    let Some(here) = land(grid, x, z) else {
        return Vec::new();
    };
    // (column, where to stand for it)
    let cells: Vec<((i32, i32), (i32, i32))> = match shape {
        Shape::Tower => vec![((x + fx, z + fz), (x, z))],
        // A line one column ahead of the bot, running to its right; the bot walks along it on
        // its own side.
        Shape::Wall => (0..len.max(2))
            .map(|i| ((x + fx + rx * i, z + fz + rz * i), (x + rx * i, z + rz * i)))
            .collect(),
        // A square whose near side is two columns ahead; the bot walks round it on the
        // outside. The middle of the near side is left open: the door.
        Shape::House => {
            let n = len.max(3);
            let (ox, oz) = (x + fx * 2 - rx * (n / 2), z + fz * 2 - rz * (n / 2));
            let at = |a: i32, b: i32| (ox + rx * a + fx * b, oz + rz * a + fz * b);
            let mut v = Vec::new();
            // near side, left to right (b = 0), standing one nearer the bot
            for a in 0..n {
                if a != n / 2 {
                    v.push((at(a, 0), at(a, -1)));
                }
            }
            // right side, front to back (a = n - 1), standing outside to the right
            for b in 1..n {
                v.push((at(n - 1, b), at(n, b)));
            }
            // far side, right to left (b = n - 1), standing beyond it
            for a in (0..n - 1).rev() {
                v.push((at(a, n - 1), at(a, n)));
            }
            // left side, back to front (a = 0), standing outside to the left
            for b in (1..n - 1).rev() {
                v.push((at(0, b), at(-1, b)));
            }
            v
        }
    };
    let level = match shape {
        Shape::Tower => land(grid, x + fx, z + fz).unwrap_or(here),
        _ => here,
    };
    let top = level + rise;
    cells
        .into_iter()
        .filter_map(|((cx, cz), stand)| {
            let y = land(grid, cx, cz)?;
            land(grid, stand.0, stand.1)?;
            ((y - level).abs() <= LEVEL && y < top).then_some(Step {
                x: cx,
                z: cz,
                stand,
                top,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Region;

    /// A 32 x 32 map of grass at y = 70 with a lake (water at y = 62) over x 20..28, z 4..12 and
    /// a tree's crown (leaves at y = 76) at (10, 10).
    fn grid() -> Grid {
        let mut g = Grid::new(Region {
            x0: 0,
            z0: 0,
            w: 32,
            h: 32,
        });
        for z in 0..32 {
            for x in 0..32 {
                let c = if (20..28).contains(&x) && (4..12).contains(&z) {
                    Cell::new(palette::index("water"), 62)
                } else if (x, z) == (10, 10) {
                    Cell::new(palette::index("leaves"), 76)
                } else {
                    Cell::new(palette::index("grass"), 70)
                };
                g.cells[(z * 32 + x) as usize] = c;
            }
        }
        g.known = g.cells.len();
        g
    }

    #[test]
    fn land_is_dry_ground_inside_the_map() {
        let g = grid();
        assert_eq!(land(&g, 3, 3), Some(70));
        assert_eq!(land(&g, 22, 6), None, "water");
        assert_eq!(land(&g, 10, 10), None, "a tree's crown");
        assert_eq!(land(&g, -1, 3), None);
        assert_eq!(land(&g, 3, 32), None);
        // a column no chunk has given yet is unknown, and nobody is sent there
        let mut blank = g.clone();
        blank.cells[5] = Cell::default();
        assert_eq!(land(&blank, 5, 0), None);
        assert!(in_water(&g, 22, 6) && !in_water(&g, 3, 3) && !in_water(&g, -5, 0));
    }

    #[test]
    fn a_bot_in_the_lake_is_sent_to_the_nearest_shore() {
        let g = grid();
        // from the lake's middle the nearest dry column is four away, on the x = 19 or 28 shore
        let (x, z) = nearest_land(&g, 23, 8, 48).unwrap();
        assert!(land(&g, x, z).is_some());
        assert_eq!((x - 23).abs().max((z - 8).abs()), 4);
        // on land it is where the bot already stands
        assert_eq!(nearest_land(&g, 3, 3, 48), Some((3, 3)));
        // a map of nothing but water has no shore
        let mut sea = g.clone();
        sea.cells
            .iter_mut()
            .for_each(|c| *c = Cell::new(palette::index("water"), 62));
        assert_eq!(nearest_land(&sea, 3, 3, 48), None);
    }

    #[test]
    fn walks_end_on_dry_ground_with_room_to_build() {
        let g = grid();
        // picks in the lake, on its shore and beside the tree are passed over
        let t = walk_target(&g, 16, 8, 12, &[(6, 0), (3, 0), (-5, 1), (-9, -3)]).unwrap();
        assert_eq!(t, (7, 5));
        // with every pick in the water, the nearest dry column
        let t = walk_target(&g, 23, 8, 12, &[(0, 0), (1, 1), (2, -1)]).unwrap();
        assert!(land(&g, t.0, t.1).is_some());
        // a pick outside the reach is held to it
        assert_eq!(walk_target(&g, 5, 20, 4, &[(-100, 0)]), Some((1, 20)));
    }

    #[test]
    fn a_wall_is_a_level_line_and_the_bot_walks_along_its_own_side() {
        let g = grid();
        // facing +z from (5, 20): the wall is at z = 21, running toward -x (the bot's right)
        let w = plan(Shape::Wall, &g, 5, 20, 1, 4, 2);
        assert_eq!(
            w,
            (0..4)
                .map(|i| Step {
                    x: 5 - i,
                    z: 21,
                    stand: (5 - i, 20),
                    top: 72
                })
                .collect::<Vec<_>>()
        );
        // a wall that would run into the lake stops at the shore
        let w = plan(Shape::Wall, &g, 17, 6, 3, 6, 2);
        assert!(
            w.iter()
                .all(|s| land(&g, s.x, s.z).is_some() && land(&g, s.stand.0, s.stand.1).is_some())
        );
        assert_eq!(w.len(), 3);
        // no rise is over the cap
        assert_eq!(plan(Shape::Wall, &g, 5, 20, 1, 4, 9)[0].top, 70 + MAX_RISE);
    }

    #[test]
    fn a_house_is_a_square_outline_with_a_door_built_from_outside() {
        let g = grid();
        let h = plan(Shape::House, &g, 12, 20, 0, 5, 2);
        // 5 x 5 has 16 outline columns; one is the door
        assert_eq!(h.len(), 15);
        let cols: std::collections::HashSet<(i32, i32)> = h.iter().map(|s| (s.x, s.z)).collect();
        assert_eq!(cols.len(), 15);
        // facing +x from (12, 20): x 14..=18, z 18..=22, the door in the middle of the near side
        assert!(
            cols.iter()
                .all(|(x, z)| (14..=18).contains(x) && (18..=22).contains(z))
        );
        assert!(
            cols.iter()
                .all(|(x, z)| *x == 14 || *x == 18 || *z == 18 || *z == 22)
        );
        assert!(!cols.contains(&(14, 20)), "the door");
        // the bot never stands on a column of the house or inside it
        assert!(h.iter().all(|s| {
            !cols.contains(&s.stand)
                && !((15..=17).contains(&s.stand.0) && (19..=21).contains(&s.stand.1))
        }));
        // each column is raised from where the bot stands beside it
        assert!(
            h.iter()
                .all(|s| (s.x - s.stand.0).abs() + (s.z - s.stand.1).abs() == 1)
        );
        assert!(h.iter().all(|s| s.top == 72));
    }

    #[test]
    fn nothing_is_planned_from_water_or_onto_it() {
        let g = grid();
        assert!(plan(Shape::Wall, &g, 23, 8, 0, 5, 2).is_empty());
        assert!(
            plan(Shape::Tower, &g, 19, 8, 0, 1, 3).is_empty(),
            "the column ahead is the lake"
        );
        assert_eq!(
            plan(Shape::Tower, &g, 5, 5, 2, 1, 3),
            vec![Step {
                x: 4,
                z: 5,
                stand: (5, 5),
                top: 73
            }]
        );
    }
}
