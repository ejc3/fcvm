//! System words: lines of GPU history the wall writes by itself when no viewer has put a
//! word in the sky for a while. The schedule lives here, in the core, so a replay of the
//! same events gives the same words at the same times: the line is a function of the seed
//! and of how many have been placed, and the time of each is the `Tick` that found the
//! idle gap over.

use crate::scene::TextStyle;
use std::sync::OnceLock;

/// The lines, one per line of the file; `#` starts a comment.
const SOURCE: &str = include_str!("../data/gpu_history.txt");

/// The styles system words rotate through, one per word.
const STYLES: [TextStyle; 3] = [
    TextStyle::Lockup,
    TextStyle::Skywriting,
    TextStyle::Landmark,
];

/// The lines of GPU history, in file order.
pub fn lines() -> &'static [&'static str] {
    static LINES: OnceLock<Vec<&'static str>> = OnceLock::new();
    LINES.get_or_init(|| {
        SOURCE
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    })
}

/// How long the renderer keeps a word in the sky after it appears (fleet/PROTOCOL.md,
/// "Words, the clock and lights"): 90 s for lockup and landmark; skywriting is written
/// at 2.2 em/s, holds 25 s after its last letter and fades over 8 s, which for a line of
/// 28 characters is under 45 s.
pub fn hold_ms(style: TextStyle) -> u64 {
    match style {
        TextStyle::Lockup | TextStyle::Landmark => 90_000,
        TextStyle::Skywriting => 45_000,
    }
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The order of the `n` lines in one pass: a Fisher-Yates shuffle seeded by `seed` and
/// the pass number, so every pass is a permutation (no line twice before all have been
/// shown) and the same on every replay.
fn raw_order(seed: u64, pass: u64, n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    let mut rng = seed ^ pass.wrapping_mul(0xD1B5_4A32_D192_ED03);
    for i in (1..n).rev() {
        let j = (splitmix(&mut rng) % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    order
}

/// The index into [`lines`] of the `count`th system word (counting from 0). The first
/// line of a pass is never the last line of the pass before it.
pub fn index(seed: u64, count: u64, n: usize) -> usize {
    assert!(n > 0, "no history lines");
    let pass = count / n as u64;
    let at = (count % n as u64) as usize;
    let mut order = raw_order(seed, pass, n);
    if pass > 0 && n > 2 && order[0] == raw_order(seed, pass - 1, n)[n - 1] {
        order.swap(0, 1);
    }
    order[at]
}

/// The `count`th system word and the style it is drawn in.
pub fn pick(seed: u64, count: u64) -> (&'static str, TextStyle) {
    let l = lines();
    (
        l[index(seed, count, l.len())],
        STYLES[(count % STYLES.len() as u64) as usize],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::normalise_word;
    use crate::scene::MAX_WORD_CHARS;
    use std::collections::BTreeSet;

    #[test]
    fn the_list_is_about_forty_lines() {
        assert!(lines().len() >= 40, "{} lines", lines().len());
    }

    #[test]
    fn every_line_fits_the_limit_the_core_and_the_renderer_enforce() {
        for l in lines() {
            // The core's viewer-word rule: no control characters, 1 to 28 characters,
            // and nothing for it to collapse.
            assert_eq!(normalise_word(l).as_deref(), Ok(*l), "{l:?}");
            assert!(l.chars().count() <= MAX_WORD_CHARS, "{l:?}");
            // The renderer keeps printable ASCII (its font is ASCII) and trims spaces.
            assert!(l.bytes().all(|b| (0x20..=0x7E).contains(&b)), "{l:?}");
        }
    }

    #[test]
    fn no_line_is_listed_twice() {
        let set: BTreeSet<_> = lines().iter().collect();
        assert_eq!(set.len(), lines().len());
    }

    #[test]
    fn the_rotation_shows_every_line_before_any_repeats() {
        let n = lines().len();
        for seed in [0, 1, 7, u64::MAX] {
            for pass in 0..4u64 {
                let seen: BTreeSet<usize> = (0..n as u64)
                    .map(|i| index(seed, pass * n as u64 + i, n))
                    .collect();
                assert_eq!(seen.len(), n, "seed {seed} pass {pass} repeats a line");
            }
            // Across the end of a pass too.
            for pass in 1..50u64 {
                assert_ne!(
                    index(seed, pass * n as u64 - 1, n),
                    index(seed, pass * n as u64, n),
                    "seed {seed}: the same line twice running at pass {pass}"
                );
            }
        }
    }

    #[test]
    fn the_order_is_the_same_every_time_and_depends_on_the_seed() {
        let n = lines().len();
        let a: Vec<_> = (0..n as u64).map(|i| index(1, i, n)).collect();
        let b: Vec<_> = (0..n as u64).map(|i| index(1, i, n)).collect();
        let c: Vec<_> = (0..n as u64).map(|i| index(2, i, n)).collect();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn styles_rotate() {
        let s: Vec<_> = (0..6).map(|i| pick(1, i).1).collect();
        assert_eq!(&s[..3], &STYLES);
        assert_eq!(&s[3..], &STYLES);
    }
}
