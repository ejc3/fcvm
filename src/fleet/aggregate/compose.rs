//! Pictures and placing tiles into a region. Pictures are planar YUV 4:4:4 (Y, then U, then
//! V, one byte per sample, BT.709 limited range), so a tile can land on any pixel without
//! chroma siting trouble; the encoder subsamples the composed region once.

use super::frame::Header;
use fleet_core::tiling::Rect;

#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Limited-range black.
pub const BLACK: [u8; 3] = [16, 128, 128];

impl Picture {
    pub fn size(width: u32, height: u32) -> usize {
        width as usize * height as usize * 3
    }

    pub fn filled(width: u32, height: u32, yuv: [u8; 3]) -> Picture {
        let plane = width as usize * height as usize;
        let mut data = Vec::with_capacity(plane * 3);
        for v in yuv {
            data.extend(std::iter::repeat_n(v, plane));
        }
        Picture {
            width,
            height,
            data,
        }
    }

    pub fn get(&self, x: u32, y: u32) -> [u8; 3] {
        let plane = self.width as usize * self.height as usize;
        let i = y as usize * self.width as usize + x as usize;
        [self.data[i], self.data[plane + i], self.data[2 * plane + i]]
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, yuv: [u8; 3]) {
        let plane = self.width as usize * self.height as usize;
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        for (p, v) in yuv.iter().enumerate() {
            for row in y.min(self.height)..y1 {
                let s = p * plane + row as usize * self.width as usize;
                self.data[s + x as usize..s + x1 as usize].fill(*v);
            }
        }
    }
}

/// Where a region sits in the whole image's pixel grid, and its encoded size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub full_width: u32,
    pub full_height: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

fn even_up(v: u32, limit: u32) -> u32 {
    if v.is_multiple_of(2) {
        v
    } else if v < limit {
        v + 1
    } else {
        v - 1
    }
}

/// The region's pixels in an image `image_height` tall, with even sizes (the encoder's
/// 4:2:0 output needs them).
pub fn geometry(region: Rect, image_aspect: f64, image_height: u32) -> Geometry {
    let full_height = image_height;
    let full_width = (image_height as f64 * image_aspect).round() as u32;
    let x = (region.x * full_width as f64).round() as u32;
    let y = (region.y * full_height as f64).round() as u32;
    let x1 = ((region.x + region.w) * full_width as f64).round() as u32;
    let y1 = ((region.y + region.h) * full_height as f64).round() as u32;
    Geometry {
        full_width,
        full_height,
        x,
        y,
        width: even_up(x1.saturating_sub(x), full_width - x),
        height: even_up(y1.saturating_sub(y), full_height - y),
    }
}

/// The height to ask a renderer for, so its whole-image grid is (to within rounding) the
/// region's: the renderer computes `full_h = round(h / rect.h)`.
pub fn tile_height(tile: Rect, full_height: u32) -> u32 {
    let h = (full_height as f64 * tile.h).round() as u32;
    (h & !1).max(16)
}

/// Copy a decoded tile's inner rectangle (the picture without its crop margins) to its
/// place in the region. A tile whose whole-image grid differs from the region's (a
/// rounding difference) has its position scaled to the region's grid.
pub fn blit(canvas: &mut Picture, g: &Geometry, h: &Header, pic: &Picture) {
    if pic.width != h.width || pic.height != h.height {
        return;
    }
    let sx = h.crop.left;
    let sy = h.crop.top;
    let sw = h.width.saturating_sub(h.crop.left + h.crop.right);
    let sh = h.height.saturating_sub(h.crop.top + h.crop.bottom);
    let scale = |v: u32, from: u32, to: u32| -> u32 {
        if from == 0 || from == to {
            v
        } else {
            (v as f64 * to as f64 / from as f64).round() as u32
        }
    };
    let dx = scale(h.x0, h.full_width, g.full_width) as i64 - g.x as i64;
    let dy = scale(h.y0, h.full_height, g.full_height) as i64 - g.y as i64;
    // Clip the destination to the canvas.
    let x0 = dx.max(0);
    let y0 = dy.max(0);
    let x1 = (dx + sw as i64).min(canvas.width as i64);
    let y1 = (dy + sh as i64).min(canvas.height as i64);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let cplane = canvas.width as usize * canvas.height as usize;
    let pplane = pic.width as usize * pic.height as usize;
    let n = (x1 - x0) as usize;
    for p in 0..3 {
        for row in y0..y1 {
            let src_row = (sy as i64 + row - dy) as usize;
            let src_col = (sx as i64 + x0 - dx) as usize;
            let s = p * pplane + src_row * pic.width as usize + src_col;
            let d = p * cplane + row as usize * canvas.width as usize + x0 as usize;
            canvas.data[d..d + n].copy_from_slice(&pic.data[s..s + n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::Crop;
    use super::*;

    fn header(x0: u32, y0: u32, w: u32, h: u32, crop: Crop) -> Header {
        Header {
            codec: "raw".into(),
            width: w,
            height: h,
            full_width: 64,
            full_height: 36,
            x0,
            y0,
            fps: 30,
            crop,
        }
    }

    #[test]
    fn geometry_is_even_and_inside_the_image() {
        let g = geometry(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            },
            16.0 / 9.0,
            1080,
        );
        assert_eq!(
            (g.full_width, g.full_height, g.x, g.y, g.width, g.height),
            (1920, 1080, 0, 0, 960, 1080)
        );
        for k in 0..7 {
            let r = Rect {
                x: 0.0,
                y: k as f64 / 7.0,
                w: 0.5,
                h: 1.0 / 7.0 - 1e-12,
            };
            let g = geometry(r, 16.0 / 9.0, 1080);
            assert_eq!(g.width % 2, 0);
            assert_eq!(g.height % 2, 0);
            assert!(g.y + g.height <= 1080);
        }
        assert_eq!(
            tile_height(
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.25,
                    h: 1.0 / 3.0
                },
                1080
            ),
            360
        );
    }

    #[test]
    fn blit_drops_the_crop_margins_and_clips_to_the_region() {
        // A tile whose inner rect is 8x6 at (8, 6) with a margin of 2 left and 1 below.
        let mut pic = Picture::filled(10, 7, [200, 100, 50]);
        pic.fill_rect(0, 0, 2, 7, [1, 1, 1]); // left margin
        pic.fill_rect(0, 6, 10, 1, [2, 2, 2]); // bottom margin
        let h = header(
            8,
            6,
            10,
            7,
            Crop {
                left: 2,
                top: 0,
                right: 0,
                bottom: 1,
            },
        );
        // Region covers whole-image (4..14, 4..10): the tile's inner rect lands at (4, 2)
        // and its right part is clipped.
        let g = Geometry {
            full_width: 64,
            full_height: 36,
            x: 4,
            y: 4,
            width: 10,
            height: 6,
        };
        let mut canvas = Picture::filled(10, 6, BLACK);
        blit(&mut canvas, &g, &h, &pic);
        assert_eq!(canvas.get(3, 2), BLACK);
        assert_eq!(canvas.get(4, 2), [200, 100, 50]);
        assert_eq!(canvas.get(9, 5), [200, 100, 50]);
        assert_eq!(canvas.get(9, 1), BLACK);
        for y in 0..6 {
            for x in 0..10 {
                let p = canvas.get(x, y);
                assert!(
                    p != [1, 1, 1] && p != [2, 2, 2],
                    "a margin pixel reached ({x},{y})"
                );
            }
        }
    }
}
