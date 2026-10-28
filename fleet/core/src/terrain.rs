//! The renderer's terrain height, so the director can keep its paths above the ground.
//!
//! A port of `surface_h` and the noise table it reads from fleet/renderer/src/world.h and
//! scene.cpp (`fill_noise_table`), in the same f32 arithmetic. The renderer keeps the eye
//! 60 m above this surface; the director keeps its paths well above that, so a few metres of
//! disagreement (fused multiply-adds on one side, not the other) does not matter.
//! `matches_the_renderer` pins it to heights printed by the renderer's own code.

use std::sync::OnceLock;

const NOISE_N: usize = 256;

fn hash_u32(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

fn u01(h: u32) -> f32 {
    (h >> 8) as f32 * (1.0 / 16_777_216.0)
}

/// Texel (i, j) holds the lattice values at (i, j), (i+1, j), (i, j+1), (i+1, j+1).
fn table() -> &'static [[f32; 4]] {
    static TAB: OnceLock<Vec<[f32; 4]>> = OnceLock::new();
    TAB.get_or_init(|| {
        let val = |i: usize, j: usize| {
            let h = hash_u32(
                ((i & (NOISE_N - 1)) as u32).wrapping_mul(0x8DA6_B343)
                    ^ hash_u32(((j & (NOISE_N - 1)) as u32).wrapping_add(0x0123_4567)),
            );
            u01(h) * 2.0 - 1.0
        };
        let mut t = Vec::with_capacity(NOISE_N * NOISE_N);
        for j in 0..NOISE_N {
            for i in 0..NOISE_N {
                t.push([val(i, j), val(i + 1, j), val(i, j + 1), val(i + 1, j + 1)]);
            }
        }
        t
    })
}

/// Value noise and its derivatives: (value, d/dx, d/dy).
fn noised(x: f32, y: f32) -> [f32; 3] {
    let (fx, fy) = (x.floor(), y.floor());
    let ix = (fx as i32 & (NOISE_N as i32 - 1)) as usize;
    let iy = (fy as i32 & (NOISE_N as i32 - 1)) as usize;
    let c = table()[iy * NOISE_N + ix];
    let (wx, wy) = (x - fx, y - fy);
    let ux = wx * wx * wx * (wx * (wx * 6.0 - 15.0) + 10.0);
    let uy = wy * wy * wy * (wy * (wy * 6.0 - 15.0) + 10.0);
    let dux = 30.0 * wx * wx * (wx * (wx - 2.0) + 1.0);
    let duy = 30.0 * wy * wy * (wy * (wy - 2.0) + 1.0);
    let (k0, k1, k2) = (c[0], c[1] - c[0], c[2] - c[0]);
    let k4 = c[0] - c[1] - c[2] + c[3];
    [
        k0 + k1 * ux + k2 * uy + k4 * ux * uy,
        dux * (k1 + k4 * uy),
        duy * (k2 + k4 * ux),
    ]
}

fn noise(x: f32, y: f32) -> f32 {
    noised(x, y)[0]
}

fn oct_step(p: (f32, f32)) -> (f32, f32) {
    (1.6 * p.0 - 1.2 * p.1 + 0.37, 1.2 * p.0 + 1.6 * p.1 - 1.71)
}

fn fbm(mut p: (f32, f32), oct: usize) -> f32 {
    let (mut a, mut amp) = (0.0f32, 0.5f32);
    for _ in 0..oct {
        a += amp * noise(p.0, p.1);
        amp *= 0.5;
        p = oct_step(p);
    }
    a
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Height of the surface the camera meets at world (x, z): terrain, or the water above it
/// (km above sea level; negative over the sea floor, where the renderer uses the sea at 0).
pub fn surface_h(x: f32, z: f32) -> f32 {
    let oct = 9;
    // macro_direct
    let cont = fbm((x * (1.0 / 260.0) + 13.1, z * (1.0 / 260.0) + 7.7), 4);
    let range = fbm((x * (1.0 / 90.0) - 5.3, z * (1.0 / 90.0) + 21.4), 4);
    let q = (x * (1.0 / 4.5), z * (1.0 / 4.5));
    let w = (
        fbm((q.0 * 0.21 + 1.7, q.1 * 0.21 + 9.2), 3),
        fbm((q.0 * 0.21 + 8.3, q.1 * 0.21 + 2.8), 3),
    );
    let land = smoothstep(-0.12, 0.16, cont + 0.08);
    let massif = smoothstep(-0.12, 0.32, range) * land;
    // terrain_from
    let q = (q.0 + w.0 * 1.2, q.1 + w.1 * 1.2);
    let (mut a, mut amp, mut a2) = (0.0f32, 1.0f32, 0.0f32);
    let mut d = (0.0f32, 0.0f32);
    let mut qe = q;
    for i in 0..oct {
        let n = noised(qe.0, qe.1);
        d = (d.0 + n[1], d.1 + n[2]);
        a += amp * n[0] / (1.0 + d.0 * d.0 + d.1 * d.1);
        amp *= 0.48;
        qe = oct_step(qe);
        if i == 1 {
            a2 = a;
        }
    }
    let mut r = (q.0 * 0.62 + 4.4, q.1 * 0.62 - 2.2);
    let (mut rid, mut ra) = (0.0f32, 0.55f32);
    for _ in 0..3 {
        let nv = noise(r.0, r.1);
        let n = 1.0 - (nv * nv + 0.02).sqrt();
        rid += ra * n * n;
        ra *= 0.5;
        r = oct_step(r);
    }
    let lowland = 0.13 + 0.08 * a;
    let mount = 0.2 + 1.5 * rid + 1.1 * a;
    let basin = massif * (-mount * 3.0).clamp(0.0, 1.0);
    let water = if basin > 0.0 {
        (mix(-0.55 + 0.25 * a2, 0.13 + 0.08 * a2, land) - 0.006).max(0.0)
    } else {
        0.0
    };
    let ground =
        mix(-0.55 + 0.25 * a, lowland, land) + massif * mount.max(0.0) * 1.15 - 0.045 * basin;
    ground.max(water)
}

/// The ground or water the camera must stay above at (x, z): `surface_h`, and the sea.
pub fn ground_h(x: f64, z: f64) -> f64 {
    (surface_h(x as f32, z as f32) as f64).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_renderer() {
        // Printed by fleet/renderer/src/world.h's surface_h (oct 9) over the table from
        // scene.cpp's fill_noise_table, compiled with g++ -O2.
        let want = [
            (-56.0, -114.75, 3.012317),
            (-25.0, -140.0, 0.684099),
            (-100.0, -90.0, 0.0),
            (-18.0, -92.0, 1.375424),
            (-47.0, -101.0, 1.291909),
            (-62.5, -130.25, 0.481591),
            (-30.0, -60.0, 0.259759),
        ];
        for (x, z, h) in want {
            let got = surface_h(x, z);
            assert!((got - h).abs() < 0.002, "({x}, {z}): {got} against {h}");
        }
    }
}
