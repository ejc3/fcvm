//! Cutting the image into one rectangle per live GPU.
//!
//! The image is the unit square in its own coordinates: `x` and `w` are fractions of the
//! image width, `y` and `h` of its height, origin top left. The fcvm side is on the left
//! and the EC2 side on the right, each as wide as its share of the live GPUs: 100 fcvm GPUs
//! and 12 EC2 tiles give the fcvm side 100/112 of the width. A side with no GPU has no
//! width; with none on either side there is no rectangle at all.
//!
//! A grid of N rectangles has `r` rows of equal height. Row `i` holds `N / r` or `N / r + 1`
//! rectangles of equal width (the longer rows last), so every N from 1 up is an exact
//! partition with no holes. `r` is the row count whose average tile aspect is closest to
//! the target aspect (renderers draw 16:9 frames).

use serde::{Deserialize, Serialize};

/// A rectangle of the whole image, in fractions of its width and height.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Which half of the image a tile belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Half {
    Fcvm,
    Ec2,
}

impl Half {
    pub fn as_str(self) -> &'static str {
        match self {
            Half::Fcvm => "fcvm",
            Half::Ec2 => "ec2",
        }
    }
}

/// Number of rows for `n` tiles in a half of aspect `half_aspect` (width / height), aiming
/// for tiles of aspect `tile_aspect`.
pub fn rows_for(n: usize, half_aspect: f64, tile_aspect: f64) -> usize {
    if n == 0 {
        return 0;
    }
    let mut best = 1;
    let mut best_err = f64::INFINITY;
    for r in 1..=n {
        // Average tile: width half_aspect / (n / r), height 1 / r, in half-height units.
        let aspect = half_aspect * (r * r) as f64 / n as f64;
        let err = (aspect / tile_aspect).ln().abs();
        if err < best_err - 1e-12 {
            best = r;
            best_err = err;
        }
    }
    best
}

/// Tiles per row for `n` tiles in `rows` rows: the short rows first.
pub fn row_counts(n: usize, rows: usize) -> Vec<usize> {
    if rows == 0 {
        return Vec::new();
    }
    let base = n / rows;
    let extra = n % rows;
    (0..rows)
        .map(|i| if i < rows - extra { base } else { base + 1 })
        .collect()
}

/// The two sides for `fcvm` and `ec2` live GPUs, each as wide as its share of them.
pub fn sides(fcvm: usize, ec2: usize) -> (Rect, Rect) {
    let total = fcvm + ec2;
    let w = if total == 0 {
        0.0
    } else {
        fcvm as f64 / total as f64
    };
    (
        Rect {
            x: 0.0,
            y: 0.0,
            w,
            h: 1.0,
        },
        Rect {
            x: w,
            y: 0.0,
            w: if total == 0 { 0.0 } else { 1.0 - w },
            h: 1.0,
        },
    )
}

/// `n` rectangles exactly covering `area` (whole-image coordinates), row-major from its
/// top left. `image_aspect` is the whole image's width / height.
pub fn grid_in(area: Rect, n: usize, image_aspect: f64, tile_aspect: f64) -> Vec<Rect> {
    let rows = rows_for(n, area.w * image_aspect / area.h, tile_aspect);
    let counts = row_counts(n, rows);
    let mut rects = Vec::with_capacity(n);
    for (i, &cols) in counts.iter().enumerate() {
        let y0 = i as f64 / rows as f64;
        let y1 = (i + 1) as f64 / rows as f64;
        for j in 0..cols {
            let x0 = j as f64 / cols as f64;
            let x1 = (j + 1) as f64 / cols as f64;
            rects.push(Rect {
                x: area.x + area.w * x0,
                y: area.y + area.h * y0,
                w: area.w * (x1 - x0),
                h: area.h * (y1 - y0),
            });
        }
    }
    rects
}

/// A side cut into full-width bands stacked top to bottom, band `i` holding `counts[i]`
/// tiles and as tall as its share of all of them, each band cut into its own grid.
/// Returns each band's rectangle and its tiles. A GPU host's tiles are one band, so they
/// cover one rectangle its aggregator can encode whole.
pub fn bands(
    side: Rect,
    counts: &[usize],
    image_aspect: f64,
    tile_aspect: f64,
) -> Vec<(Rect, Vec<Rect>)> {
    let total: usize = counts.iter().sum();
    let mut out = Vec::with_capacity(counts.len());
    let mut before = 0;
    for &c in counts {
        let y0 = before as f64 / total as f64;
        let y1 = (before + c) as f64 / total as f64;
        let band = Rect {
            x: side.x,
            y: side.y + side.h * y0,
            w: side.w,
            h: side.h * (y1 - y0),
        };
        out.push((band, grid_in(band, c, image_aspect, tile_aspect)));
        before += c;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn overlap(a: &Rect, b: &Rect) -> f64 {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        if w <= EPS || h <= EPS {
            0.0
        } else {
            w * h
        }
    }

    #[test]
    fn every_split_of_live_gpus_partitions_the_image() {
        for f in 0..=24 {
            for e in 0..=24 {
                let (l, r) = sides(f, e);
                let mut all = grid_in(l, f, 16.0 / 9.0, 16.0 / 9.0);
                all.extend(grid_in(r, e, 16.0 / 9.0, 16.0 / 9.0));
                assert_eq!(all.len(), f + e);
                if f + e > 0 {
                    let whole = Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 1.0,
                        h: 1.0,
                    };
                    assert_partition(whole, &all, &format!("{f} fcvm, {e} EC2"));
                    assert!((l.w * (f + e) as f64 - f as f64).abs() < EPS, "{f} {e}");
                }
            }
        }
        assert_eq!(sides(0, 0).0.w, 0.0);
        assert_eq!(sides(0, 0).1.w, 0.0);
        assert_eq!(sides(5, 0).0.w, 1.0);
        assert_eq!(
            sides(0, 5).1,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0
            }
        );
        assert_eq!(sides(100, 12).0.w, 100.0 / 112.0);
    }

    fn assert_partition(area: Rect, rects: &[Rect], what: &str) {
        let mut sum = 0.0;
        for r in rects {
            assert!(r.w > 0.0 && r.h > 0.0, "{what}: empty {r:?}");
            assert!(
                r.x >= area.x - EPS && r.x + r.w <= area.x + area.w + EPS,
                "{what}: {r:?} outside {area:?}"
            );
            assert!(
                r.y >= area.y - EPS && r.y + r.h <= area.y + area.h + EPS,
                "{what}: {r:?} outside {area:?}"
            );
            sum += r.w * r.h;
        }
        assert!((sum - area.w * area.h).abs() < 1e-9, "{what}: area {sum}");
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert_eq!(overlap(a, b), 0.0, "{what}: {a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn host_bands_partition_the_side_and_each_band_is_one_rectangle() {
        let side = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.75,
            h: 1.0,
        };
        for a in 1..=12 {
            for b in 0..=12 {
                for c in 0..=12 {
                    let counts: Vec<usize> = [a, b, c].into_iter().filter(|n| *n > 0).collect();
                    let bs = bands(side, &counts, 16.0 / 9.0, 16.0 / 9.0);
                    let mut all = Vec::new();
                    for (band, tiles) in &bs {
                        assert_partition(*band, tiles, &format!("band of {counts:?}"));
                        all.extend(tiles.iter().copied());
                    }
                    assert_partition(side, &all, &format!("tiles {counts:?}"));
                }
            }
        }
    }

    #[test]
    fn one_band_is_the_plain_grid() {
        let side = Rect {
            x: 0.25,
            y: 0.0,
            w: 0.75,
            h: 1.0,
        };
        for n in 1..=40 {
            let bs = bands(side, &[n], 16.0 / 9.0, 16.0 / 9.0);
            assert_eq!(bs[0].0, side);
            assert_eq!(bs[0].1, grid_in(side, n, 16.0 / 9.0, 16.0 / 9.0));
        }
    }

    #[test]
    fn square_counts_give_square_grids_of_16_by_9_tiles() {
        // A 16:9 half split into k*k tiles keeps 16:9 tiles.
        for k in 1..=10 {
            assert_eq!(rows_for(k * k, 16.0 / 9.0, 16.0 / 9.0), k);
            assert_eq!(row_counts(k * k, k), vec![k; k]);
        }
    }

    #[test]
    fn uneven_counts_put_the_longer_rows_last() {
        assert_eq!(row_counts(7, 3), vec![2, 2, 3]);
        assert_eq!(row_counts(8, 3), vec![2, 3, 3]);
        assert_eq!(row_counts(0, 0), Vec::<usize>::new());
    }
}
