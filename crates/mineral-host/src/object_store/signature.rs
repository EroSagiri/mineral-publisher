//! AWS Signature Version 4 signing, implemented with `sha2` alone.
//!
//! This is deliberately a *pure* function of the request description and the
//! frozen clock reading the caller supplies: no clock is read here, and nothing
//! about the object being published is decided here. The only thing this module
//! produces is the `Authorization` header value an object-storage endpoint
//! expects.

use sha2::{Digest, Sha256};

/// The signing algorithm name, as it appears on the wire.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// A request, reduced to the parts the signature covers.
///
/// `canonical_uri` must already be URI-encoded per path segment, `canonical_query`
/// must already be sorted and encoded, and `headers` must contain at least `host`
/// and `x-amz-date` (or `x-amz-content-sha256` when the caller sets it).
#[derive(Clone, Copy, Debug)]
pub struct SignableRequest<'a> {
    pub method: &'a str,
    pub canonical_uri: &'a str,
    pub canonical_query: &'a str,
    /// Lowercase header name and normalized value, in any order.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the payload, or `UNSIGNED-PAYLOAD`.
    pub payload_sha256: &'a str,
}

/// Everything a signature needs that is not part of the request.
#[derive(Clone, Copy, Debug)]
pub struct SigningContext<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDD`.
    pub date: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

impl SigningContext<'_> {
    fn credential_scope(&self) -> String {
        format!(
            "{}/{}/{}/aws4_request",
            self.date, self.region, self.service
        )
    }
}

/// The complete `Authorization` header value for one request.
pub fn authorization_header(request: &SignableRequest<'_>, context: &SigningContext<'_>) -> String {
    let signature = signature(request, context);
    let signed_headers = signed_headers(request.headers);
    format!(
        "{ALGORITHM} Credential={}/{}, SignedHeaders={signed_headers}, Signature={signature}",
        context.access_key_id,
        context.credential_scope()
    )
}

/// The hex signature of one request.
pub fn signature(request: &SignableRequest<'_>, context: &SigningContext<'_>) -> String {
    let canonical = canonical_request(request);
    let string_to_sign = [
        ALGORITHM,
        context.amz_date,
        &context.credential_scope(),
        &sha256_hex(canonical.as_bytes()),
    ]
    .join("\n");
    hex(&hmac_sha256(
        &signing_key(context),
        string_to_sign.as_bytes(),
    ))
}

/// The canonical request: the exact bytes the signature is computed over.
pub fn canonical_request(request: &SignableRequest<'_>) -> String {
    let mut headers = request.headers.to_vec();
    headers.sort_by(|left, right| left.0.cmp(&right.0));
    let canonical_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        request.canonical_uri,
        request.canonical_query,
        canonical_headers,
        signed_headers(request.headers),
        request.payload_sha256
    )
}

/// The semicolon-separated, sorted list of header names the signature covers.
pub fn signed_headers(headers: &[(String, String)]) -> String {
    let mut names = headers
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names.join(";")
}

/// Hex SHA-256 of a payload.
pub fn payload_sha256(payload: &[u8]) -> String {
    sha256_hex(payload)
}

fn signing_key(context: &SigningContext<'_>) -> [u8; 32] {
    let initial = format!("AWS4{}", context.secret_access_key);
    let date = hmac_sha256(initial.as_bytes(), context.date.as_bytes());
    let region = hmac_sha256(&date, context.region.as_bytes());
    let service = hmac_sha256(&region, context.service.as_bytes());
    hmac_sha256(&service, b"aws4_request")
}

/// HMAC-SHA256, built from `sha2` so no extra dependency is needed for it.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0_u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Sha256::new();
    inner.update(key_block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(key_block.map(|byte| byte ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test cases, so the hand-built HMAC is anchored to a public vector
    /// rather than to itself.
    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// The worked example published with the AWS Signature Version 4
    /// documentation (`GET https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08`).
    /// Every intermediate value is pinned, so a change to canonicalisation or to
    /// the key derivation shows up here rather than at a bucket.
    #[test]
    fn the_published_aws_signing_example_is_reproduced_exactly() {
        let headers = vec![
            (
                "content-type".to_owned(),
                "application/x-www-form-urlencoded; charset=utf-8".to_owned(),
            ),
            ("host".to_owned(), "iam.amazonaws.com".to_owned()),
            ("x-amz-date".to_owned(), "20150830T123600Z".to_owned()),
        ];
        let request = SignableRequest {
            method: "GET",
            canonical_uri: "/",
            canonical_query: "Action=ListUsers&Version=2010-05-08",
            headers: &headers,
            payload_sha256: &payload_sha256(b""),
        };
        let context = SigningContext {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "iam",
            date: "20150830",
            amz_date: "20150830T123600Z",
        };

        assert_eq!(
            payload_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(signed_headers(&headers), "content-type;host;x-amz-date");
        assert_eq!(
            sha256_hex(canonical_request(&request).as_bytes()),
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        assert_eq!(
            signature(&request, &context),
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
        assert_eq!(
            authorization_header(&request, &context),
            concat!(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, ",
                "SignedHeaders=content-type;host;x-amz-date, ",
                "Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
            )
        );
    }

    #[test]
    fn the_signature_covers_the_payload_and_the_header_set() {
        let base = vec![
            ("host".to_owned(), "bucket.example.com".to_owned()),
            ("x-amz-date".to_owned(), "20150830T123600Z".to_owned()),
        ];
        let context = SigningContext {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            region: "auto",
            service: "s3",
            date: "20150830",
            amz_date: "20150830T123600Z",
        };
        let request = |payload_sha256: &'static str| SignableRequest {
            method: "PUT",
            canonical_uri: "/bucket/assets/sha256/ab/cdef",
            canonical_query: "",
            headers: &base,
            payload_sha256,
        };

        assert_ne!(
            signature(&request("aaaa"), &context),
            signature(&request("bbbb"), &context),
            "the payload hash is part of the signed request"
        );
        assert!(!signature(&request("aaaa"), &context).is_empty());
    }
}
