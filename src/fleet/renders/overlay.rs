//! What the replays put into every page for an overlay order: a band with the visitor's
//! text and a style block for the transform. The markup is the one both renderers were
//! screenshotted with (the band drawn alike on all 14 pages; the transforms in the two
//! groups of `fleet_core::renders`).

use fleet_core::renders::OrderOverlay;

const BAND_STYLE: &str = "<style>#fcvm-overlay{position:fixed;left:0;right:0;top:38%;\
z-index:2147483647;background:rgba(17,17,17,.88);color:#fff;font:700 44px/1.3 sans-serif;\
padding:18px 24px;text-align:center;letter-spacing:.01em;text-shadow:0 2px 6px #000}</style>";

/// The style of a transform, by its name in the protocol. A name this build does not know
/// styles nothing.
fn transform_css(name: &str) -> Option<&'static str> {
    Some(match name {
        "mirror" => "html{transform:scaleX(-1)}",
        "flip" => "html{transform:rotate(180deg)}",
        "rotate" => "html{transform:rotate(4deg);transform-origin:50% 0}",
        "scale" => "html{transform:scale(.7);transform-origin:50% 0}",
        "zoom" => "html{zoom:1.6}",
        "skew" => "html{transform:skewX(-10deg)}",
        "invert" => "html{filter:invert(1)}",
        "tilt" => "html{transform:perspective(900px) rotateY(20deg);transform-origin:50% 0}",
        "blur" => "html{filter:blur(3px)}",
        _ => return None,
    })
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The bytes a side's replays inject into each page's main document; empty for no overlay.
/// `side` is 0 for `fcvm` and 1 for `kitesurf`, which gets no transform marked
/// `chromium_only`.
pub fn html(overlay: Option<&OrderOverlay>, side: usize) -> String {
    let Some(o) = overlay else {
        return String::new();
    };
    let mut out = String::new();
    if let Some(css) = o
        .transform
        .as_deref()
        .filter(|_| side == 0 || !o.chromium_only)
        .and_then(transform_css)
    {
        out.push_str("<style>");
        out.push_str(css);
        out.push_str("</style>");
    }
    if let Some(text) = o.text.as_deref().filter(|t| !t.is_empty()) {
        out.push_str(BAND_STYLE);
        out.push_str("<div id=\"fcvm-overlay\">");
        out.push_str(&escape(text));
        out.push_str("</div>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::renders::{TRANSFORMS_BOTH, TRANSFORMS_CHROMIUM_ONLY};

    fn order(text: Option<&str>, transform: Option<&str>, chromium_only: bool) -> OrderOverlay {
        OrderOverlay {
            text: text.map(str::to_string),
            transform: transform.map(str::to_string),
            chromium_only,
            seq: 1,
        }
    }

    #[test]
    fn no_overlay_injects_nothing() {
        assert_eq!(html(None, 0), "");
        assert_eq!(html(Some(&order(None, None, false)), 1), "");
    }

    #[test]
    fn the_text_is_escaped_into_the_band() {
        let o = order(Some("<b>a&b</b> \"x\" 'y'"), None, false);
        for side in 0..2 {
            let out = html(Some(&o), side);
            assert!(out.ends_with(
                "<div id=\"fcvm-overlay\">&lt;b&gt;a&amp;b&lt;/b&gt; &quot;x&quot; &#39;y&#39;</div>"
            ));
            assert_eq!(
                out.matches('<').count(),
                4,
                "style, /style, div, /div: {out}"
            );
        }
    }

    /// Every transform the protocol names has a style, and a transform only Chromium draws
    /// reaches the `fcvm` side's pages alone.
    #[test]
    fn every_named_transform_has_a_style_on_the_sides_that_draw_it() {
        for name in TRANSFORMS_BOTH {
            let o = order(None, Some(name), false);
            assert!(html(Some(&o), 0).starts_with("<style>html{"), "{name}");
            assert_eq!(html(Some(&o), 0), html(Some(&o), 1), "{name}");
        }
        for name in TRANSFORMS_CHROMIUM_ONLY {
            let o = order(Some("hi"), Some(name), true);
            assert!(html(Some(&o), 0).starts_with("<style>html{"), "{name}");
            let other = html(Some(&o), 1);
            assert!(
                other.starts_with(BAND_STYLE),
                "{name}: the text still goes to both"
            );
            assert!(!other.contains("html{"), "{name}");
        }
        let o = order(None, Some("no-such-transform"), false);
        assert_eq!(html(Some(&o), 0), "");
    }
}
