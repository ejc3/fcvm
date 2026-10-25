//! AWS Signature Version 4 for one request, enough for Bedrock's InvokeModel.
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv4-create-signed-request.html>

use sha2::{Digest, Sha256};

pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// RFC 3986 percent-encoding of everything but the unreserved characters, as SigV4 wants.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Sign a request. `path` is the URL path exactly as sent (segments already encoded once);
/// for every service but S3 the canonical path encodes each segment again. `headers` must
/// include `host` and `x-amz-date`; names are lowercase. Returns the `Authorization` value.
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    creds: &Credentials,
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload: &[u8],
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    let canonical_path = path
        .split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/");
    let mut hs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    hs.sort();
    let canonical_headers: String = hs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = hs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{method}\n{canonical_path}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
        hex::encode(Sha256::digest(payload))
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        assert_eq!(
            hex::encode(hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// `get-vanilla` from the AWS SigV4 test suite.
    #[test]
    fn signs_the_aws_get_vanilla_vector() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let auth = authorization(
            &creds,
            "GET",
            "/",
            "",
            &[
                ("host".into(), "example.amazonaws.com".into()),
                ("x-amz-date".into(), "20150830T123600Z".into()),
            ],
            b"",
            "us-east-1",
            "service",
            "20150830T123600Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn canonical_path_encodes_an_encoded_colon_again() {
        // Bedrock model ids carry ':'; the URL holds %3A and the canonical path %253A.
        let p = "/model/example.model-v1%3A0/invoke";
        let canonical: Vec<String> = p.split('/').map(uri_encode).collect();
        assert_eq!(canonical.join("/"), "/model/example.model-v1%253A0/invoke");
    }
}
