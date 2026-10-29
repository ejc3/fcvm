//! Who receives direct streams and who receives the broadcast (docs/fleet/ARCHITECTURE.md,
//! "Broadcast mode", "Who gets the broadcast").
//!
//! - [`Seats`]: at most `--direct-capacity` viewers hold a seat (direct and aggregated
//!   streams). A seat is kept while its connection is open and for [`SEAT_GRACE`] after it
//!   closes, for a page that reconnects with its ticket. A viewer that arrives with every
//!   seat held is late: broadcast only, for the whole connection.
//! - Direct tokens: `v1.<expiry unix s>.<viewer>.<mac>`, the mac an HMAC-SHA256 of the first
//!   three fields with the admission key, base64url without padding. Renderers and
//!   aggregators that hold the key refuse `GET /frames` without a valid one.
//! - Broadcast tokens: IVS playback authorization, an ES384 JWT signed with the channel's
//!   playback key pair.

use anyhow::{bail, Context, Result};
use ring::signature::{EcdsaKeyPair, ECDSA_P384_SHA384_FIXED_SIGNING};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a closed connection's seat waits for the page to come back with its ticket.
pub const SEAT_GRACE: Duration = Duration::from_secs(60);
/// How long a direct token lasts, and how often a seated connection gets a new one.
pub const DIRECT_TOKEN_SECS: u64 = 600;
pub const DIRECT_REFRESH: Duration = Duration::from_secs(300);
/// How long a playback token lasts (renewed with the direct token).
pub const PLAYBACK_TOKEN_SECS: u64 = 3600;
/// The direct token's query parameter on a frame URL.
pub const ADMIT_PARAM: &str = "admit";

/// Base64url without padding (RFC 4648 section 5).
pub fn b64url(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        let chars = c.len() + 1;
        for i in 0..chars {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// The admission key: 32 random bytes made at start.
#[derive(Clone)]
pub struct AdmitKey(pub [u8; 32]);

impl AdmitKey {
    pub fn random() -> Self {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        AdmitKey(k)
    }

    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(h: &str) -> Option<Self> {
        let b = hex::decode(h).ok()?;
        Some(AdmitKey(b.try_into().ok()?))
    }

    fn mac(&self, signed: &str) -> String {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        b64url(ring::hmac::sign(&key, signed.as_bytes()).as_ref())
    }

    /// A direct token for `viewer` that lasts until `exp` (unix seconds).
    pub fn direct_token(&self, viewer: &str, exp: u64) -> String {
        let signed = format!("v1.{exp}.{viewer}");
        let mac = self.mac(&signed);
        format!("{signed}.{mac}")
    }
}

/// Whether `token` is a direct token made with one of `keys` that has not expired at `now`
/// (unix seconds). What renderers and aggregators check.
pub fn direct_token_valid(keys: &[AdmitKey], token: &str, now: u64) -> bool {
    let Some((signed, mac)) = token.rsplit_once('.') else {
        return false;
    };
    let mut parts = signed.splitn(3, '.');
    let (Some("v1"), Some(exp), Some(viewer)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let Ok(exp) = exp.parse::<u64>() else {
        return false;
    };
    if exp < now || viewer.is_empty() {
        return false;
    }
    keys.iter().any(|k| {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &k.0);
        let Some(mac) = b64url_decode(mac) else {
            return false;
        };
        ring::hmac::verify(&key, signed.as_bytes(), &mac).is_ok()
    })
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
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

/// What a connection was admitted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admitted {
    /// A seat: direct streams, and the broadcast if the viewer switches to it.
    Seat { ticket: String },
    /// Every seat was held when it arrived: the broadcast only.
    Late,
}

struct Seat {
    /// Open connections holding it (a page has one; two with the same ticket share it).
    open: usize,
    /// When the last connection closed.
    closed_at: Option<Instant>,
}

/// The seats. `capacity` `None` seats everyone.
pub struct Seats {
    capacity: Option<usize>,
    grace: Duration,
    held: Mutex<HashMap<String, Seat>>,
}

impl Seats {
    pub fn new(capacity: Option<usize>) -> Self {
        Seats {
            capacity,
            grace: SEAT_GRACE,
            held: Mutex::default(),
        }
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    fn expire(&self, held: &mut HashMap<String, Seat>, now: Instant) {
        held.retain(|_, s| {
            s.open > 0
                || s.closed_at
                    .is_none_or(|c| now.duration_since(c) < self.grace)
        });
    }

    /// Admit a new connection. A ticket the page kept from a seat it still holds (open, or
    /// closed within the grace) gets that seat back; otherwise a free seat, else late.
    pub fn admit(&self, ticket: Option<&str>, now: Instant) -> Admitted {
        let mut held = self.held.lock().unwrap();
        self.expire(&mut held, now);
        if let Some(t) = ticket {
            if let Some(s) = held.get_mut(t) {
                s.open += 1;
                s.closed_at = None;
                return Admitted::Seat {
                    ticket: t.to_string(),
                };
            }
        }
        if self.capacity.is_some_and(|c| held.len() >= c) {
            return Admitted::Late;
        }
        let ticket = uuid::Uuid::new_v4().simple().to_string();
        held.insert(
            ticket.clone(),
            Seat {
                open: 1,
                closed_at: None,
            },
        );
        Admitted::Seat { ticket }
    }

    /// A seated connection closed.
    pub fn release(&self, ticket: &str, now: Instant) {
        let mut held = self.held.lock().unwrap();
        if let Some(s) = held.get_mut(ticket) {
            s.open = s.open.saturating_sub(1);
            if s.open == 0 {
                s.closed_at = Some(now);
            }
        }
        self.expire(&mut held, now);
    }

    /// Whether a new connection would get a seat now.
    pub fn free(&self, now: Instant) -> bool {
        let mut held = self.held.lock().unwrap();
        self.expire(&mut held, now);
        self.capacity.is_none_or(|c| held.len() < c)
    }

    pub fn capacity(&self) -> Option<usize> {
        self.capacity
    }

    /// Seats held now (open or within their grace).
    pub fn held(&self, now: Instant) -> usize {
        let mut held = self.held.lock().unwrap();
        self.expire(&mut held, now);
        held.len()
    }
}

/// Signs IVS playback tokens for one private channel.
pub struct PlaybackSigner {
    pub channel_arn: String,
    pub playback_url: String,
    key: EcdsaKeyPair,
}

impl PlaybackSigner {
    /// `key_pem`: the playback key pair's private key, PKCS#8 PEM (`BEGIN PRIVATE KEY`), P-384.
    pub fn new(channel_arn: String, playback_url: String, key_pem: &str) -> Result<Self> {
        let der = pem_body(key_pem, "PRIVATE KEY").context("the playback key is not PKCS#8 PEM")?;
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P384_SHA384_FIXED_SIGNING,
            &der,
            &ring::rand::SystemRandom::new(),
        )
        .map_err(|e| anyhow::anyhow!("the playback key is not a P-384 key: {e}"))?;
        Ok(PlaybackSigner {
            channel_arn,
            playback_url,
            key,
        })
    }

    /// A token for pages served from `origin`, valid until `exp` (unix seconds).
    pub fn token(&self, origin: Option<&str>, exp: u64) -> Result<String> {
        let header = b64url(br#"{"alg":"ES384","typ":"JWT"}"#);
        let mut claims = serde_json::json!({"aws:channel-arn": self.channel_arn, "exp": exp});
        if let Some(o) = origin {
            claims["aws:access-control-allow-origin"] = o.into();
        }
        let payload = b64url(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{payload}");
        let sig = self
            .key
            .sign(&ring::rand::SystemRandom::new(), signing_input.as_bytes())
            .map_err(|e| anyhow::anyhow!("signing a playback token: {e}"))?;
        Ok(format!("{signing_input}.{}", b64url(sig.as_ref())))
    }

    /// The playback URL with a token.
    pub fn url(&self, origin: Option<&str>, exp: u64) -> Result<String> {
        let sep = if self.playback_url.contains('?') {
            '&'
        } else {
            '?'
        };
        Ok(format!(
            "{}{sep}token={}",
            self.playback_url,
            self.token(origin, exp)?
        ))
    }
}

/// Where the broadcast plays: a private channel's URL signed per viewer, or an open one.
pub enum Playback {
    Signed(Box<PlaybackSigner>),
    Open(String),
}

/// What a late viewer is told beside its disabled toggle.
pub const LATE_REASON: &str = "Direct streams are full, so you are watching the broadcast.";

/// The admission policy of one coordinator: its seats, the key direct tokens are made with,
/// and the broadcast's playback.
pub struct Gate {
    pub seats: Seats,
    pub key: AdmitKey,
    pub playback: Option<Playback>,
    /// How often a connection's admission is looked at again (a late viewer learns of a
    /// free seat this soon).
    pub tick: Duration,
}

impl Gate {
    /// Everyone seated, no broadcast: the coordinator without `--broadcast-listen`.
    pub fn open() -> Self {
        Gate {
            seats: Seats::new(None),
            key: AdmitKey::random(),
            playback: None,
            tick: Duration::from_secs(10),
        }
    }

    /// The admission message for a connection admitted as `admitted`, acting as `viewer`
    /// from a page at `origin`, at `now_ms` (unix ms).
    pub fn message(
        &self,
        admitted: &Admitted,
        viewer: &str,
        origin: Option<&str>,
        now_ms: u64,
        now: Instant,
    ) -> fleet_core::protocol::Admission {
        use fleet_core::protocol::{Admission, Token};
        let now_s = now_ms / 1000;
        let broadcast = match &self.playback {
            None => None,
            Some(Playback::Open(url)) => Some(Token {
                value: url.clone(),
                expires_at: u64::MAX,
            }),
            // A signed token is bound to the page's origin: a connection that names none (not a
            // browser) gets none, rather than a token any origin could play.
            Some(Playback::Signed(_)) if origin.is_none() => None,
            Some(Playback::Signed(s)) => {
                let exp = now_s + PLAYBACK_TOKEN_SECS;
                match s.url(origin, exp) {
                    Ok(value) => Some(Token {
                        value,
                        expires_at: exp * 1000,
                    }),
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "cannot sign a playback token");
                        None
                    }
                }
            }
        };
        match admitted {
            Admitted::Seat { ticket } => {
                let exp = now_s + DIRECT_TOKEN_SECS;
                Admission {
                    seat: Some(ticket.clone()),
                    locked: false,
                    reason: None,
                    direct: Some(Token {
                        value: self.key.direct_token(viewer, exp),
                        expires_at: exp * 1000,
                    }),
                    broadcast,
                    seat_free: false,
                    capacity: self.seats.capacity(),
                }
            }
            Admitted::Late => Admission {
                seat: None,
                locked: true,
                reason: Some(LATE_REASON.into()),
                direct: None,
                broadcast,
                seat_free: self.seats.free(now),
                capacity: self.seats.capacity(),
            },
        }
    }
}

/// The DER bytes of the first PEM block labelled `label`.
fn pem_body(pem: &str, label: &str) -> Result<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin).context("no PEM begin line")? + begin.len();
    let stop = pem[start..].find(&end).context("no PEM end line")? + start;
    let b64: String = pem[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let std_to_url: String = b64
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    match b64url_decode(&std_to_url) {
        Some(d) if !d.is_empty() => Ok(d),
        _ => bail!("the PEM body is not base64"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc4648() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(b64url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(b64url_decode("Zm9vYg").unwrap(), b"foob");
    }

    #[test]
    fn a_direct_token_is_valid_until_it_expires_and_only_under_its_key() {
        let k = AdmitKey([7; 32]);
        let other = AdmitKey([8; 32]);
        let t = k.direct_token("v-3", 1000);
        assert!(direct_token_valid(std::slice::from_ref(&k), &t, 999));
        assert!(direct_token_valid(std::slice::from_ref(&k), &t, 1000));
        assert!(!direct_token_valid(std::slice::from_ref(&k), &t, 1001));
        assert!(!direct_token_valid(std::slice::from_ref(&other), &t, 999));
        // The previous key still admits a token made with it.
        assert!(direct_token_valid(&[other.clone(), k.clone()], &t, 999));
        // Any change to a signed field breaks the mac.
        let forged = t.replacen("1000", "9999", 1);
        assert!(!direct_token_valid(std::slice::from_ref(&k), &forged, 999));
        for bad in [
            "",
            "v1",
            "v1.1000.v-3",
            "v2.1000.v-3.x",
            "v1.x.v-3.y",
            "v1.1000..x",
        ] {
            assert!(
                !direct_token_valid(std::slice::from_ref(&k), bad, 0),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn seats_admit_up_to_capacity_then_viewers_are_late() {
        let s = Seats::new(Some(2));
        let now = Instant::now();
        let a = s.admit(None, now);
        let b = s.admit(None, now);
        assert!(matches!(a, Admitted::Seat { .. }));
        assert!(matches!(b, Admitted::Seat { .. }));
        assert_eq!(s.admit(None, now), Admitted::Late);
        assert!(!s.free(now));
        // A forged or stale ticket is not a seat.
        assert_eq!(s.admit(Some("not-a-ticket"), now), Admitted::Late);
    }

    #[test]
    fn a_closed_seat_waits_for_its_ticket_then_frees() {
        let s = Seats::new(Some(1)).with_grace(Duration::from_secs(60));
        let t0 = Instant::now();
        let Admitted::Seat { ticket } = s.admit(None, t0) else {
            panic!("no seat")
        };
        s.release(&ticket, t0);
        // Within the grace: held for the ticket, not free for anyone else.
        let t1 = t0 + Duration::from_secs(30);
        assert!(!s.free(t1));
        assert_eq!(s.admit(None, t1), Admitted::Late);
        assert_eq!(
            s.admit(Some(&ticket), t1),
            Admitted::Seat {
                ticket: ticket.clone()
            }
        );
        s.release(&ticket, t1);
        // Past the grace: free, and the old ticket is just another arrival.
        let t2 = t1 + Duration::from_secs(61);
        assert!(s.free(t2));
        assert!(matches!(s.admit(Some(&ticket), t2), Admitted::Seat { ticket: n } if n != ticket));
    }

    #[test]
    fn without_a_capacity_everyone_is_seated() {
        let s = Seats::new(None);
        let now = Instant::now();
        for _ in 0..1000 {
            assert!(matches!(s.admit(None, now), Admitted::Seat { .. }));
        }
        assert!(s.free(now));
    }

    fn test_key_pem() -> String {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P384_SHA384_FIXED_SIGNING, &rng).unwrap();
        let b64 = {
            // Standard base64 with padding, as PEM has it.
            let u = b64url(pkcs8.as_ref());
            let mut s: String = u
                .chars()
                .map(|c| match c {
                    '-' => '+',
                    '_' => '/',
                    c => c,
                })
                .collect();
            while !s.len().is_multiple_of(4) {
                s.push('=');
            }
            s
        };
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            lines.join("\n")
        )
    }

    #[test]
    fn a_playback_token_is_an_es384_jwt_over_the_channel() {
        let pem = test_key_pem();
        let s = PlaybackSigner::new(
            "arn:aws:ivs:us-east-1:000000000000:channel/example".into(),
            "https://example.playback.live-video.net/api/video/v1/x.m3u8".into(),
            &pem,
        )
        .unwrap();
        let url = s.url(Some("https://stage.example"), 2_000_000_000).unwrap();
        let token = url.split_once("?token=").unwrap().1;
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&b64url_decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES384");
        let claims: serde_json::Value =
            serde_json::from_slice(&b64url_decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            claims["aws:channel-arn"],
            "arn:aws:ivs:us-east-1:000000000000:channel/example"
        );
        assert_eq!(
            claims["aws:access-control-allow-origin"],
            "https://stage.example"
        );
        assert_eq!(claims["exp"], 2_000_000_000u64);
        // The signature verifies under the key pair's public key.
        let der = pem_body(&pem, "PRIVATE KEY").unwrap();
        let kp = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P384_SHA384_FIXED_SIGNING,
            &der,
            &ring::rand::SystemRandom::new(),
        )
        .unwrap();
        use ring::signature::KeyPair;
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P384_SHA384_FIXED,
            kp.public_key().as_ref().to_vec(),
        );
        let sig = b64url_decode(parts[2]).unwrap();
        assert_eq!(sig.len(), 96);
        public
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .expect("the signature verifies");
    }

    /// A private channel's token is bound to the page's origin; a connection that names no
    /// origin (not a browser) gets no playback URL rather than one any origin could use.
    #[test]
    fn a_connection_without_an_origin_gets_no_signed_playback() {
        let gate = Gate {
            seats: Seats::new(Some(1)),
            key: AdmitKey([1; 32]),
            playback: Some(Playback::Signed(Box::new(
                PlaybackSigner::new(
                    "arn:aws:ivs:us-east-1:000000000000:channel/example".into(),
                    "https://playback.example/x.m3u8".into(),
                    &test_key_pem(),
                )
                .unwrap(),
            ))),
            tick: Duration::from_secs(10),
        };
        let seat = Admitted::Seat { ticket: "t".into() };
        let now = Instant::now();
        let with = gate.message(&seat, "v-1", Some("https://stage.example"), 1_000_000, now);
        assert!(with.broadcast.is_some());
        let without = gate.message(&seat, "v-1", None, 1_000_000, now);
        assert!(without.broadcast.is_none(), "an unbound playback token");
    }

    #[test]
    fn a_key_that_is_not_p384_pkcs8_is_refused() {
        assert!(PlaybackSigner::new("a".into(), "b".into(), "not pem").is_err());
        let wrong = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        assert!(PlaybackSigner::new("a".into(), "b".into(), wrong).is_err());
    }
}
