use reqwest::Method;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::{AUTHORIZATION, HOST};

use super::{
    config::{R2ObjectStoreConfig, R2TransportError, canonical_query, host_of, timestamps},
    signature::{SignableRequest, SigningContext, authorization_header, payload_sha256},
};

/// One request, described in exactly the parts Signature Version 4 covers.
///
/// `path` is absolute and already percent-encoded (it starts with the bucket
/// segment), `query` is an unordered set of raw name/value pairs, and `headers`
/// holds whatever else must be signed. Nothing here knows what the request means.
pub struct SignedRequestSpec<'a> {
    pub method: &'static str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub headers: Vec<(String, String)>,
    pub payload_sha256: String,
}

impl<'a> SignedRequestSpec<'a> {
    pub fn new(method: &'static str, path: &'a str, payload_sha256: String) -> Self {
        Self {
            method,
            path,
            query: &[],
            headers: Vec::new(),
            payload_sha256,
        }
    }

    pub fn with_query(mut self, query: &'a [(String, String)]) -> Self {
        self.query = query;
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Builds and signs one request to an S3-compatible endpoint.
///
/// This is the only place a request is signed, for both the asset target and the
/// source reader: one signature implementation, one canonical-query encoding, one
/// URL assembly.
pub fn signed_request_builder(
    client: &Client,
    config: &R2ObjectStoreConfig,
    spec: &SignedRequestSpec<'_>,
) -> Result<RequestBuilder, R2TransportError> {
    let host = host_of(config.endpoint())?;
    let (date, amz_date) = timestamps()?;

    let mut headers = vec![
        ("host".to_owned(), host.clone()),
        (
            "x-amz-content-sha256".to_owned(),
            spec.payload_sha256.clone(),
        ),
        ("x-amz-date".to_owned(), amz_date.clone()),
    ];
    headers.extend(spec.headers.iter().cloned());
    headers.sort_by(|left, right| left.0.cmp(&right.0));

    let query = canonical_query(spec.query);
    let request = SignableRequest {
        method: spec.method,
        canonical_uri: spec.path,
        canonical_query: &query,
        headers: &headers,
        payload_sha256: &spec.payload_sha256,
    };
    let context = SigningContext {
        access_key_id: config.access_key_id(),
        secret_access_key: config.secret_access_key().expose(),
        region: config.region(),
        service: "s3",
        date: &date,
        amz_date: &amz_date,
    };
    let authorization = authorization_header(&request, &context);

    let url = request_url(config.endpoint(), spec.path, spec.query);
    let method =
        Method::from_bytes(spec.method.as_bytes()).expect("the method is a static HTTP verb");
    let mut builder = client
        .request(method, &url)
        .header(HOST, host)
        .header("x-amz-content-sha256", spec.payload_sha256.clone())
        .header("x-amz-date", amz_date)
        .header(AUTHORIZATION, authorization);
    for (name, value) in &spec.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    Ok(builder)
}

/// The absolute URL one request goes to, with its canonical query.
pub fn request_url(endpoint: &str, path: &str, query: &[(String, String)]) -> String {
    let query = canonical_query(query);
    if query.is_empty() {
        format!("{endpoint}{path}")
    } else {
        format!("{endpoint}{path}?{query}")
    }
}

/// The payload hash to sign for a request that carries no body.
pub fn empty_payload_sha256() -> String {
    payload_sha256(b"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_request_carries_its_query_in_canonical_order() {
        let query = vec![
            ("prefix".to_owned(), "vault/".to_owned()),
            ("list-type".to_owned(), "2".to_owned()),
            ("continuation-token".to_owned(), "1/abc+def==".to_owned()),
            ("max-keys".to_owned(), "1000".to_owned()),
        ];

        assert_eq!(
            request_url("https://example.invalid", "/bucket", &query),
            "https://example.invalid/bucket?continuation-token=1%2Fabc%2Bdef%3D%3D&list-type=2&max-keys=1000&prefix=vault%2F"
        );
    }

    #[test]
    fn a_request_without_a_query_has_no_question_mark() {
        assert_eq!(
            request_url("https://example.invalid", "/bucket/vault/index.md", &[]),
            "https://example.invalid/bucket/vault/index.md"
        );
    }
}
