//! Cutting each half of the image into N rectangles.
//!
//! The image is the unit square in its own coordinates: `x` and `w` are fractions of the
//! image width, `y` and `h` of its height, origin top left. The fcvm half is
//! `x in [0, 0.5)`, the EC2 half `x in [0.5, 1)`. Both halves get the same grid, so tile `k`
//! of one half has the same shape as tile `k` of the other.
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

    fn x_offset(self) -> f64 {
        match self {
            Half::Fcvm => 0.0,
            Half::Ec2 => 0.5,
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

/// The `n` rectangles of one half, row-major from the top left, in whole-image
/// coordinates. `image_aspect` is the whole image's width / height.
pub fn half_rects(half: Half, n: usize, image_aspect: f64, tile_aspect: f64) -> Vec<Rect> {
    let rows = rows_for(n, image_aspect / 2.0, tile_aspect);
    let counts = row_counts(n, rows);
    let mut rects = Vec::with_capacity(n);
    for (i, &cols) in counts.iter().enumerate() {
        let y0 = i as f64 / rows as f64;
        let y1 = (i + 1) as f64 / rows as f64;
        for j in 0..cols {
            let x0 = j as f64 / cols as f64;
            let x1 = (j + 1) as f64 / cols as f64;
            rects.push(Rect {
                x: half.x_offset() + x0 * 0.5,
                y: y0,
                w: (x1 - x0) * 0.5,
                h: y1 - y0,
            });
        }
    }
    rects
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
    fn every_n_from_1_to_100_is_an_exact_partition_of_both_halves() {
        for n in 1..=100 {
            for half in [Half::Fcvm, Half::Ec2] {
                let rects = half_rects(half, n, 32.0 / 9.0, 16.0 / 9.0);
                assert_eq!(rects.len(), n, "n={n}");
                let (lo, hi) = match half {
                    Half::Fcvm => (0.0, 0.5),
                    Half::Ec2 => (0.5, 1.0),
                };
                let mut area = 0.0;
                for r in &rects {
                    assert!(r.w > 0.0 && r.h > 0.0, "n={n} empty {r:?}");
                    assert!(r.x >= lo - EPS && r.x + r.w <= hi + EPS, "n={n} {r:?}");
                    assert!(r.y >= -EPS && r.y + r.h <= 1.0 + EPS, "n={n} {r:?}");
                    area += r.w * r.h;
                }
                assert!((area - 0.5).abs() < 1e-9, "n={n} area {area}");
                for (i, a) in rects.iter().enumerate() {
                    for b in &rects[i + 1..] {
                        assert_eq!(overlap(a, b), 0.0, "n={n} {a:?} overlaps {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn both_halves_have_the_same_grid() {
        for n in 1..=100 {
            let l = half_rects(Half::Fcvm, n, 32.0 / 9.0, 16.0 / 9.0);
            let r = half_rects(Half::Ec2, n, 32.0 / 9.0, 16.0 / 9.0);
            for (a, b) in l.iter().zip(&r) {
                assert!((a.x + 0.5 - b.x).abs() < EPS);
                assert_eq!((a.y, a.w, a.h), (b.y, b.w, b.h));
            }
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
