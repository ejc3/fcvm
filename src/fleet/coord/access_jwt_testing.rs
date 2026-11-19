//! Test-only Access tokens: a signer with fixed RSA keys, and a server for its JWKS.
//! Self-contained (no `crate::` paths), so `tests/test_fleet_coord.rs` includes it by path as
//! well as the unit tests of `access_jwt`.

/// Two RSA-2048 keys made for these tests alone (PKCS#8 DER, standard base64). They sign
/// nothing outside the tests.
const TEST_KEY: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCoUKuOBeyjB+HUr4y1aHvzkvNWm8j4TPhBbrZ53st/6MaF6ov9Myo3ToAnLKvdGRJogLS3ZbMSmtJMCG8YkZBdEGba9fTT1NDSNsVPa2v32zQBNPFQTGAEQBpTA1BXLoZT7dkboUZzgDzRdvNUpxSoIkL8f11VUKLCj99GdhGG88rBtiGnSmQ1O/ci/MkEX1tjRQjvBnyT3mK6/vSGrC0EyqfR5brFJO14QMCNSBkJ0n/+IQRC1b3GV3qND1mhnq28TeVtp2Cz66B7ER+seiZtcCkPDfMLYR2YgZXVm/txSvJv7NbwrZUsPUV+K+vUrOh+B7Lxn6RDpTkmxYoujuv1AgMBAAECggEAGFLMGp1Fj2ErZAqDKZDHX4bsI9HQhFLPNNl/L551nG7zCCrsfwiUAGPCRlGre9X3NeM/pRnie9JO3g5WoQohftQeIQ9r3umWzIIowmuSMe0wwenjjkid6Y9VSTYl4pYbyGjgt/Whp5MFIaTF0QUc3u1QmTT+CRRlk6X/TKtG/iWPWiXcGSfvjMiKnfUTWWi2z6KEJlZdez/fVVtItOBs1SD23Z2a2eVAZJccjp6eA1mG13FY1qaNfq50PgEzVWdhowXFq+Jiuhz5hj6mwywFyRMGGsgGtreCJnhQn1BF9ZvBZBGtQDfUWzPshIXVFpXXIoGiaHu3DD2MlbyuP32MkwKBgQDWhSY+WFN3UKprmFew2vsAXRTt/fTwUsv5QmNhnBWsVj68leU/JedY44bTx1YsNdZd4NSMEKwkIx8F4kLfMHf58UXcH9nLD9513EW2llgP1vLdLcPu4yGwTKm2rmFgq+vWMmrQtnoqG82Z9WVpcC9QQY+4LUXhMucXF/G3Nt6cxwKBgQDI3FnNcxEyo4Wi6xoeV9HHCqOaTuP4d5UkXu9JAtJR110Gn3vAUK7cf99KVklzSE8X4BHh88iso1pBN0yxMGz5w4YfionqYGOVVIht2BTvQrR6ghbUc3nrcpCvTAm/s8A9SiSMmRx8RvQQM4aWrAq5JTKAFieY6IOwaiD5QFxdYwKBgQCB6NBoIQUq1GbgEbnJGFO4vKXYGlsizoFHybR1gBh/7RrBCeVygHnBksgrqCO/UjsnTuhoihA1Mi+vWH42uxts+Xfc/kzedBW8OlDdad6aeQlKeFSXbmUQZtzADIpcCMEhThbxnhANQBkai0mGuLFuYMWwO3p+Dl1z1sRCt6/IMQKBgGlUW+dH4wnaBFcl0GF/DevBoX9mxVdjZhtXiUWRLDdsp0S011FHPhs1FyPFUekZTxinENmbXG1jkA3I8Pet/EeRh8D8hkwY1b/WP2DSwk+hfEEN/OjV0WZtgn/acPZNTY8WVUpBdFWbY5snWfmjKFBkuDHA7Gzb6UrWrInofUcfAoGBAK3FLd6wzcnEC7lRWIcK4PGiwMtZ/udqJD9+oFOQmWj5zCjXBvMQsG4QvDsRX7T+bbIOTtJ+XMDjVXb3wt02BsUWsWZ7UecOl8aoxPMcBbAtiO9MqLkjcv/mrK7o3Z5+72xhABSq7caI5m1lzbQE/tyoIHJhRDdhh/uQnpqs1Jqj";
const OTHER_KEY: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCOCDtU79uacZ4WOd+5rs3JDNY9sbDwKtTKN70MpZteksDF/+vz2XioBuYaYhMEzvjoXgeGk62yFAqGbFW4SX98d6hou/WAjl0tagapCEl14flCKY8EJhnudf0STRbc6NNfXTln7n6tz+come3zrVNE5Zqrr3DdjyWpdD2lW7fycxoZQtagTt/FUMv9pXV+bYJPDQEMOuprG3Jk1T9hRrqCbhc9gDY3JoEm3IZs2eZyl0yMeNAZtqayGlcFWisR+ee180cUAMaOSMiJIGWprF6imwAnRdJSerKscdDgkmUl1+agSMGNeMUHZqvy8SbrqQCU/mpX0enfc6frqbL8k0bFAgMBAAECggEAChHOqwyU+X55TRgYossgLyh9fpg8yXov5eT3vpAgfh78vczmxz4GzjAsW3h9OAe4do5GuFAmA2G6W/di8xLQOrHSZcKAUEyc5zMEz90U4DT/fU+sTQiqfqaCJQGuz5tOHmJh8dq4bng2lOv828bRCCnDF82eEdyBOLdTW4gveBJ9Sj2yRViKlG8QuWmDBEQDfGQaHYg7nL8rSJ8vm4j0+axgSKVgKitEhWuspWqIQb7L0AkeG7jS1qu1X5jYXWVv+/1wurlfZQHw+/XzXP/Z0i0zzo2EJjbkwQedcGpm1uEwwL4Yxwxh044LkZLi1rgPcQwSGkJGRZji/w3kWydg4QKBgQDByvaOtWD3igxvU7N9+f2EVxtlVMei+LwKK7tT3n6mR9nWNomYlvawaF3DkRJ5jCnSZ/C14xypEfKSYfRpmOh+1vi3NVNb8s4VE1KigGXYCElNg1szF2QJN7nPHcWb1gdjzCnSESeyhzTZdjW8EHZYXdDsYeQv4JHUFwutnjq83QKBgQC7n85M7eDRBMI7ZUM/lBCmnf4kfO+uPazRKHRSZInwH7DQqhaVIQ2+5DUlyiy/siflNLKE8gcSGDNMo1lE5zfdai+eIYXnuwNcZMOR44K4X1hwXzFF5XcQv8JbkVTYFKglIVtnUGU59C5q8Z+SEgskv2el6AX6D/jOmXxaMoV/CQKBgGFVI0jB6JXrjBtOGtSm/qG9blESp8I+05L4K/oPweSaKdrhQXKWQAiSIqXa80CBGXBILCnui7KPBRpogG2ZEwzBw+GgsuF3rfH/5n0fqAc9AjQgSWn70Y6fon9YIKwIb2W8YTTU0O9l4TFnoc13JzlyxUA0WnuBezze5kqpMi8xAoGAabNA7PQCeJ6+sel9dj77FnLhrzq3aKkUSA0LYzOCxkWEPjnkWQVzEfzMuEpq5V//u8/xVpZnR1lc5SsNapT8Uv+0bdii8IbaAN4/k1vh0+7bCwuF3cPFTSlK3H1GmCkjBTACp3SSYpDe6Z7P7+G8Z2pxgPTzEQqYeAPeG45kyMkCgYEArrVM9Bp9ayh5IX1ze5uIG8wNTktvZfQaW3PnAMtjIjS7p1sSXxeeWGKcgn88YzOBPdAqc+w1QXFvIQvnlxEESt8PDxUhUJDFC3HEjDKgsv2M0vYbOn6DItul2LeltG/piZXsEBejN02v64sk3m0q9ll7DtvvF+aPZsnrpslJizM=";

/// Base64url without padding.
pub fn b64url(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..c.len() + 1 {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn std_b64_decode(s: &str) -> Vec<u8> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = A.iter().position(|a| *a == c).expect("base64") as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// A signer for test tokens under key id `kid`.
pub struct TestSigner {
    key: ring::signature::RsaKeyPair,
    pub kid: String,
}

impl TestSigner {
    /// The test key, or with `other` the second one (a forger's).
    pub fn new(other: bool, kid: &str) -> Self {
        let der = std_b64_decode(if other { OTHER_KEY } else { TEST_KEY });
        TestSigner {
            key: ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap(),
            kid: kid.into(),
        }
    }

    /// The JWKS entry Cloudflare would publish for this key.
    pub fn jwk(&self) -> serde_json::Value {
        let c = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(self.key.public());
        serde_json::json!({"kid": self.kid, "kty": "RSA", "alg": "RS256", "use": "sig",
            "n": b64url(&c.n), "e": b64url(&c.e)})
    }

    /// An RS256 token over `claims`.
    pub fn token(&self, claims: &serde_json::Value) -> String {
        let header = b64url(
            serde_json::json!({"alg": "RS256", "kid": self.kid, "typ": "JWT"})
                .to_string()
                .as_bytes(),
        );
        let payload = b64url(claims.to_string().as_bytes());
        let input = format!("{header}.{payload}");
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                input.as_bytes(),
                &mut sig,
            )
            .unwrap();
        format!("{input}.{}", b64url(&sig))
    }
}

/// Serves `keys` as a team's JWKS; returns the certs URL and how many times it was read.
pub async fn serve_keys(
    keys: Vec<serde_json::Value>,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let r = reads.clone();
    let app = axum::Router::new().route(
        "/cdn-cgi/access/certs",
        axum::routing::get(move || {
            let r = r.clone();
            let keys = keys.clone();
            async move {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                axum::Json(serde_json::json!({"keys": keys}))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/cdn-cgi/access/certs", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, reads)
}
