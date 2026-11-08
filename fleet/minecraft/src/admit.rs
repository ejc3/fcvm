//! The coordinator's direct tokens (fleet/PROTOCOL.md, "Access" under "Renderer"):
//! `v1.<expiry unix s>.<viewer>.<mac>`, the mac an HMAC-SHA256 of the first three fields with
//! one of the admission keys, base64url without padding. The same check as
//! `src/fleet/coord/admission.rs::direct_token_valid`; the view socket's first message carries
//! one, and its viewer is who the socket acts as (the take-over checks it).

/// The admission keys a scene brings, newest first; at most this many are kept.
pub const MAX_KEYS: usize = 4;

/// The viewer of `token` when it is a direct token made with one of `keys` that has not expired
/// at `now` (unix seconds).
pub fn viewer_of(keys: &[[u8; 32]], token: &str, now: u64) -> Option<String> {
    let (signed, mac) = token.rsplit_once('.')?;
    let mut parts = signed.splitn(3, '.');
    let (Some("v1"), Some(exp), Some(viewer)) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let exp: u64 = exp.parse().ok()?;
    if exp < now || viewer.is_empty() {
        return None;
    }
    let mac = b64url_decode(mac)?;
    keys.iter()
        .any(|k| {
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, k);
            ring::hmac::verify(&key, signed.as_bytes(), &mac).is_ok()
        })
        .then(|| viewer.to_string())
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Parse a scene's `admit_keys` (hex, newest first). `None` names the first key that is not 32
/// bytes of hex.
pub fn parse_keys(hex_keys: &[String]) -> Result<Vec<[u8; 32]>, String> {
    if hex_keys.len() > MAX_KEYS {
        return Err(format!(
            "admit_keys has {} keys; at most {MAX_KEYS}",
            hex_keys.len()
        ));
    }
    hex_keys
        .iter()
        .map(|h| {
            hex::decode(h)
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
                .ok_or_else(|| "admit_keys holds a key that is not 32 bytes of hex".to_string())
        })
        .collect()
}

#[cfg(test)]
pub fn make_token(key: &[u8; 32], viewer: &str, exp: u64) -> String {
    use base64::Engine;
    let signed = format!("v1.{exp}.{viewer}");
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mac = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(ring::hmac::sign(&k, signed.as_bytes()));
    format!("{signed}.{mac}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_names_its_viewer_until_it_expires_and_only_under_its_key() {
        let k = [7u8; 32];
        let other = [8u8; 32];
        let t = make_token(&k, "v-17", 1000);
        assert_eq!(viewer_of(&[k], &t, 999).as_deref(), Some("v-17"));
        assert_eq!(viewer_of(&[other, k], &t, 1000).as_deref(), Some("v-17"));
        assert_eq!(viewer_of(&[k], &t, 1001), None);
        assert_eq!(viewer_of(&[other], &t, 10), None);
        assert_eq!(viewer_of(&[], &t, 10), None);
        // A changed viewer keeps the old mac, which no longer matches.
        let forged = t.replace("v-17", "v-18");
        assert_eq!(viewer_of(&[k], &forged, 10), None);
        for bad in [
            "",
            "v1",
            "v1.1000.v-17",
            "v2.1000.v-17.AAAA",
            "v1.x.v-17.AAAA",
            "v1.1000..AAAA",
        ] {
            assert_eq!(viewer_of(&[k], bad, 10), None, "{bad:?}");
        }
    }

    #[test]
    fn keys_must_be_32_bytes_of_hex_and_at_most_four() {
        assert_eq!(parse_keys(&["07".repeat(32)]).unwrap(), vec![[7u8; 32]]);
        assert!(parse_keys(&["07".repeat(31)]).is_err());
        assert!(parse_keys(&["zz".repeat(32)]).is_err());
        assert!(parse_keys(&vec!["07".repeat(32); 5]).is_err());
    }
}
