//! AWS Signature Version 4 request signing (header-based), for the Amazon
//! Bedrock provider.
//!
//! Only what Bedrock needs: a request with no query string, signed in the
//! `Authorization` header, with the payload hash computed over the exact body
//! bytes that are sent. The path handed to [`sign`] is the path *as sent on
//! the wire* (already percent-encoded where the request needs it); the
//! canonical URI encodes each segment of it once more, which is SigV4's rule
//! for every service except S3 (so a `%3A` on the wire is `%253A` in the
//! canonical request). Dot segments are not normalized: callers never produce
//! them.
//!
//! Checked against the published `aws-sig-v4-test-suite` vectors (see the
//! tests below).

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// IAM credentials. `Debug` prints the access key id only.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Everything besides the request that goes into a signature.
pub(crate) struct SigningParams<'a> {
    pub credentials: &'a Credentials,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDD'T'HHMMSS'Z'` (UTC), sent as `x-amz-date`.
    pub amz_date: &'a str,
    /// Also send and sign `x-amz-content-sha256` (the payload hash).
    pub sign_content_sha256: bool,
}

/// The result of signing, with the intermediate strings (read by the test
/// vectors; the provider only needs `headers`).
pub(crate) struct Signed {
    #[cfg_attr(not(test), allow(dead_code))]
    pub canonical_request: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub string_to_sign: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub signature: String,
    /// Headers the caller must add to the request, `authorization` last.
    pub headers: Vec<(String, String)>,
}

/// Sign a request. `headers` are the caller's headers to sign (must include
/// `host`); `x-amz-date`, `x-amz-security-token` (with a session token) and
/// optionally `x-amz-content-sha256` are added here. `payload` must be the
/// exact bytes sent as the body.
pub(crate) fn sign(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: &[u8],
    params: &SigningParams<'_>,
) -> Signed {
    let payload_hash = hex(&Sha256::digest(payload));

    let mut added: Vec<(String, String)> = vec![("x-amz-date".into(), params.amz_date.into())];
    if let Some(token) = &params.credentials.session_token {
        added.push(("x-amz-security-token".into(), token.clone()));
    }
    if params.sign_content_sha256 {
        added.push(("x-amz-content-sha256".into(), payload_hash.clone()));
    }

    let mut canonical: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), canonical_header_value(v)))
        .chain(
            added
                .iter()
                .map(|(k, v)| (k.clone(), canonical_header_value(v))),
        )
        .collect();
    canonical.sort_by(|a, b| a.0.cmp(&b.0));
    // Repeated names are joined with commas, in the order given.
    let mut merged: Vec<(String, String)> = Vec::with_capacity(canonical.len());
    for (k, v) in canonical {
        match merged.last_mut() {
            Some((last, value)) if *last == k => {
                value.push(',');
                value.push_str(&v);
            }
            _ => merged.push((k, v)),
        }
    }
    let signed_headers = merged
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = merged.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

    let canonical_request = format!(
        "{method}\n{uri}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        uri = canonical_uri(path),
        query = "",
    );

    let date = &params.amz_date[..8.min(params.amz_date.len())];
    let scope = format!(
        "{date}/{region}/{service}/aws4_request",
        region = params.region,
        service = params.service
    );
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{hash}",
        amz_date = params.amz_date,
        hash = hex(&Sha256::digest(canonical_request.as_bytes())),
    );

    let key = signing_key(
        &params.credentials.secret_access_key,
        date,
        params.region,
        params.service,
    );
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));

    let authorization = format!(
        "{ALGORITHM} Credential={access}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        access = params.credentials.access_key_id,
    );
    added.push(("authorization".into(), authorization));

    Signed {
        canonical_request,
        string_to_sign,
        signature,
        headers: added,
    }
}

/// The canonical URI: each `/`-separated segment of the path as sent,
/// URI-encoded once more (everything but the unreserved characters
/// `A-Z a-z 0-9 - _ . ~` becomes `%XY`, uppercase hex).
pub(crate) fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encode every byte outside SigV4's unreserved set (RFC 3986
/// unreserved characters), uppercase hex.
pub(crate) fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Trim the value and collapse runs of spaces to one.
fn canonical_header_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for word in v.split(' ').filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.trim().to_string()
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The current time as `YYYYMMDD'T'HHMMSS'Z'`.
pub(crate) fn amz_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    amz_date(secs)
}

/// Format seconds since the Unix epoch as `YYYYMMDD'T'HHMMSS'Z'` (UTC).
pub(crate) fn amz_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Official SigV4 test vectors.
///
/// Source: the `aws-sig-v4-test-suite` as maintained in awslabs/aws-c-auth,
/// `tests/aws-signing-test-suite/v4/<name>/` (`request.txt`, `context.json`,
/// `header-canonical-request.txt`, `header-string-to-sign.txt`,
/// `header-signature.txt`, `header-signed-request.txt`):
/// <https://github.com/awslabs/aws-c-auth/tree/main/tests/aws-signing-test-suite/v4>
///
/// All use the example credentials `AKIDEXAMPLE` /
/// `wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY`, region `us-east-1`, service
/// `service` and time `20150830T123600Z`. Expected values are copied from
/// those files verbatim.
#[cfg(test)]
mod tests {
    use super::*;

    const DATE: &str = "20150830T123600Z";

    fn creds(token: Option<&str>) -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: token.map(str::to_string),
        }
    }

    struct Vector {
        method: &'static str,
        path: &'static str,
        headers: &'static [(&'static str, &'static str)],
        body: &'static [u8],
        token: Option<&'static str>,
        sign_body: bool,
        canonical_request: &'static str,
        string_to_sign: &'static str,
        authorization: &'static str,
    }

    fn check(v: &Vector) {
        let c = creds(v.token);
        let signed = sign(
            v.method,
            v.path,
            v.headers,
            v.body,
            &SigningParams {
                credentials: &c,
                region: "us-east-1",
                service: "service",
                amz_date: DATE,
                sign_content_sha256: v.sign_body,
            },
        );
        assert_eq!(signed.canonical_request, v.canonical_request);
        assert_eq!(signed.string_to_sign, v.string_to_sign);
        let auth = &signed.headers.last().unwrap();
        assert_eq!(auth.0, "authorization");
        assert_eq!(auth.1, v.authorization);
        assert!(v.authorization.ends_with(&signed.signature));
    }

    const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn get_vanilla() {
        check(&Vector {
            method: "GET",
            path: "/",
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            token: None,
            sign_body: false,
            canonical_request: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
        });
    }

    #[test]
    fn post_vanilla() {
        check(&Vector {
            method: "POST",
            path: "/",
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            token: None,
            sign_body: false,
            canonical_request: "POST\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n553f88c9e4d10fc9e109e2aeb65f030801b70c2f6468faca261d401ae622fc87",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
        });
    }

    /// `post-header-key-case`: the request's header name is `Host`; the
    /// canonical form is lowercase. Same expected output as `post-vanilla`.
    #[test]
    fn post_header_key_case() {
        check(&Vector {
            method: "POST",
            path: "/",
            headers: &[("HOST", "example.amazonaws.com")],
            body: b"",
            token: None,
            sign_body: false,
            canonical_request: "POST\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n553f88c9e4d10fc9e109e2aeb65f030801b70c2f6468faca261d401ae622fc87",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
        });
    }

    /// A body, signed with `x-amz-content-sha256` (`sign_body: true`) — the
    /// shape of a Bedrock request.
    #[test]
    fn post_x_www_form_urlencoded() {
        check(&Vector {
            method: "POST",
            path: "/",
            headers: &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Host", "example.amazonaws.com"),
                ("Content-Length", "13"),
            ],
            body: b"Param1=value1",
            token: None,
            sign_body: true,
            canonical_request: "POST\n/\n\ncontent-length:13\ncontent-type:application/x-www-form-urlencoded\nhost:example.amazonaws.com\nx-amz-content-sha256:9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e\nx-amz-date:20150830T123600Z\n\ncontent-length;content-type;host;x-amz-content-sha256;x-amz-date\n9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nb1edd1d03544c25390e32085d55b57acc9a3961bb59415ff86c45c3d89d16cfb",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-length;content-type;host;x-amz-content-sha256;x-amz-date, Signature=d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b",
        });
    }

    #[test]
    fn get_vanilla_with_session_token() {
        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        check(&Vector {
            method: "GET",
            path: "/",
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            token: Some(token),
            sign_body: false,
            canonical_request: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\nx-amz-security-token:6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267\n\nhost;x-amz-date;x-amz-security-token\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n067b36aa60031588cea4a4cde1f21215227a047690c72247f1d70b32fbbfad2b",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8",
        });
    }

    /// `get-utf8`: the request line carries the raw UTF-8 path `/ሴ`, encoded
    /// once in the canonical URI.
    #[test]
    fn get_utf8() {
        check(&Vector {
            method: "GET",
            path: "/\u{1234}",
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            token: None,
            sign_body: false,
            canonical_request: "GET\n/%E1%88%B4\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n2a0a97d02205e45ce2e994789806b19270cfbbb0921b278ccf58f5249ac42102",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85",
        });
    }

    /// `get-space-unnormalized`: a space in a segment, and a trailing slash
    /// that must survive.
    #[test]
    fn get_space_unnormalized() {
        check(&Vector {
            method: "GET",
            path: "/example space/",
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            token: None,
            sign_body: false,
            canonical_request: "GET\n/example%20space/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n63ee75631ed7234ae61b5f736dfc7754cdccfedbff4b5128a915706ee9390d86",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741",
        });
    }

    /// `get-unreserved`: the unreserved characters pass through unencoded.
    #[test]
    fn get_unreserved() {
        check(&Vector {
            method: "GET",
            path: "/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            token: None,
            sign_body: false,
            canonical_request: "GET\n/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n6a968768eefaa713e2a6b16b589a8ea192661f098f37349f4e2c0082757446f9",
            authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=07ef7494c76fa4850883e2b006601f940f8a34d404d0cfa977f52a65bbf5f24f",
        });
    }

    #[test]
    fn empty_payload_hash_is_the_sha256_of_nothing() {
        assert_eq!(hex(&Sha256::digest(b"")), EMPTY_HASH);
    }

    /// A path that is already percent-encoded on the wire is encoded again:
    /// SigV4's double encoding for non-S3 services.
    #[test]
    fn wire_encoded_segments_are_encoded_again() {
        assert_eq!(
            canonical_uri("/model/anthropic.claude-sonnet-5-v1%3A0/converse-stream"),
            "/model/anthropic.claude-sonnet-5-v1%253A0/converse-stream"
        );
        assert_eq!(canonical_uri(""), "/");
        assert_eq!(uri_encode("a/b:c d%"), "a%2Fb%3Ac%20d%25");
    }

    #[test]
    fn header_values_are_trimmed_and_space_runs_collapsed() {
        assert_eq!(canonical_header_value("  a   b  c "), "a b c");
        assert_eq!(canonical_header_value(""), "");
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = creds(Some("TOKENVALUE"));
        let s = format!("{c:?}");
        assert!(s.contains("AKIDEXAMPLE"));
        assert!(!s.contains("wJalrXUtnFEMI"));
        assert!(!s.contains("TOKENVALUE"));
    }

    #[test]
    fn amz_date_formats_utc() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        // Leap days, century rules and a year end.
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
        assert_eq!(amz_date(4_107_542_399), "21000228T235959Z");
        assert_eq!(amz_date(4_107_542_400), "21000301T000000Z");
        assert_eq!(amz_date(1_704_067_199), "20231231T235959Z");
        assert_eq!(amz_date(1_709_164_800), "20240229T000000Z");
        assert_eq!(amz_date_now().len(), 16);
    }
}
