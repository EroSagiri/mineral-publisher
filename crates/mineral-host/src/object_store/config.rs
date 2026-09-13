use std::{
    error::Error,
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A secret access key.
///
/// The inner value is never printed: a credential must not reach a log, a panic
/// message or an audit record.
#[derive(Clone)]
pub struct R2SecretKey(String);

impl R2SecretKey {
    pub fn new(value: impl Into<String>) -> Result<Self, R2ObjectStoreConfigError> {
        let value = value.into();
        if value.is_empty() || value.contains(['\0', '\n', '\r']) {
            return Err(R2ObjectStoreConfigError::InvalidSecret);
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for R2SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("R2SecretKey(<redacted>)")
    }
}

/// Everything a runtime needs to reach one bucket.
///
/// This is runtime configuration: it lives in the host, is never handed to the
/// engine, and the engine's durable records never contain any of it. It describes a
/// *connection*, which is why the asset target and the source reader share it — what
/// they do with the connection is entirely separate.
#[derive(Clone)]
pub struct R2ObjectStoreConfig {
    endpoint: String,
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: R2SecretKey,
    /// How long one HTTP request may take, including the streaming body.
    timeout: Duration,
}

impl R2ObjectStoreConfig {
    pub fn new(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        access_key_id: impl Into<String>,
        secret_access_key: R2SecretKey,
    ) -> Result<Self, R2ObjectStoreConfigError> {
        let endpoint = endpoint.into();
        let bucket = bucket.into();
        let access_key_id = access_key_id.into();
        if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
            return Err(R2ObjectStoreConfigError::InvalidEndpoint);
        }
        if endpoint.ends_with('/') {
            return Err(R2ObjectStoreConfigError::InvalidEndpoint);
        }
        if bucket.is_empty()
            || bucket.contains(['/', '\\', '\0'])
            || access_key_id.is_empty()
            || access_key_id.contains(['\0', '\n', '\r'])
        {
            return Err(R2ObjectStoreConfigError::InvalidIdentity);
        }
        Ok(Self {
            endpoint,
            bucket,
            // R2 accepts `auto`; a generic S3 endpoint may need its own region.
            region: "auto".to_owned(),
            access_key_id,
            secret_access_key,
            timeout: Duration::from_secs(300),
        })
    }

    pub fn with_region(
        mut self,
        region: impl Into<String>,
    ) -> Result<Self, R2ObjectStoreConfigError> {
        let region = region.into();
        if region.is_empty() || region.contains(['\0', '\n', '\r']) {
            return Err(R2ObjectStoreConfigError::InvalidIdentity);
        }
        self.region = region;
        Ok(self)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    pub(crate) fn secret_access_key(&self) -> &R2SecretKey {
        &self.secret_access_key
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The endpoint host and bucket, for a report. It never contains a credential.
    pub fn describe(&self) -> String {
        format!("{}/{}", self.endpoint, self.bucket)
    }
}

impl fmt::Debug for R2ObjectStoreConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("R2ObjectStoreConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &self.secret_access_key)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum R2ObjectStoreConfigError {
    InvalidEndpoint,
    InvalidIdentity,
    InvalidSecret,
}

impl fmt::Display for R2ObjectStoreConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint => formatter.write_str(
                "object store endpoint must be an absolute http(s) URL without a trailing slash",
            ),
            Self::InvalidIdentity => {
                formatter.write_str("object store bucket or access key id is invalid")
            }
            Self::InvalidSecret => formatter.write_str("object store secret access key is invalid"),
        }
    }
}

impl Error for R2ObjectStoreConfigError {}

/// The two timestamps one Signature Version 4 request needs.
pub(crate) fn timestamps() -> Result<(String, String), R2TransportError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| R2TransportError::Clock)?
        .as_secs();
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    Ok((
        format!("{year:04}{month:02}{day:02}"),
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
    ))
}

fn civil_from_unix(seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let remainder = seconds % 86_400;
    let (hour, minute, second) = (
        u32::try_from(remainder / 3_600).unwrap_or(0),
        u32::try_from((remainder % 3_600) / 60).unwrap_or(0),
        u32::try_from(remainder % 60).unwrap_or(0),
    );

    // Days since 1970-01-01 to a civil date, by the standard era-based algorithm.
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_position + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    })
    .unwrap_or(1);
    year += i64::from(month <= 2);
    (year, month, day, hour, minute, second)
}

/// Why a request could not be described to the endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum R2TransportError {
    /// The endpoint is not a usable URL.
    InvalidEndpoint,
    /// The system clock cannot produce the signing timestamps.
    Clock,
}

impl fmt::Display for R2TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint => formatter.write_str("object store endpoint is invalid"),
            Self::Clock => formatter.write_str("system clock cannot be used to sign a request"),
        }
    }
}

impl Error for R2TransportError {}

/// The host part of one endpoint URL, as the signature must name it.
pub(crate) fn host_of(endpoint: &str) -> Result<String, R2TransportError> {
    let without_scheme = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .ok_or(R2TransportError::InvalidEndpoint)?;
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .filter(|host| !host.is_empty())
        .ok_or(R2TransportError::InvalidEndpoint)?;
    Ok(host.to_owned())
}

/// Percent-encodes one object key for the request path.
///
/// `/` is the only byte that keeps its meaning, because it separates the segments
/// of a key; every other byte outside the unreserved set is encoded from its UTF-8
/// bytes, uppercased, so two runtimes cannot disagree about the same key.
pub(crate) fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(char::from(*byte));
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Percent-encodes one query value.
pub(crate) fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(*byte));
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// The canonical query string for one request: sorted by name, then URL-encoded.
pub(crate) fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut pairs = pairs
        .iter()
        .map(|(name, value)| (encode_query_value(name), encode_query_value(value)))
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_never_printed() {
        let secret = R2SecretKey::new("super-secret").unwrap();

        assert!(!format!("{secret:?}").contains("super-secret"));
        let config =
            R2ObjectStoreConfig::new("https://example.invalid", "bucket", "key", secret).unwrap();
        assert!(!format!("{config:?}").contains("super-secret"));
    }

    #[test]
    fn an_endpoint_without_a_scheme_or_with_a_trailing_slash_is_refused() {
        let secret = || R2SecretKey::new("secret").unwrap();

        assert_eq!(
            R2ObjectStoreConfig::new("example.invalid", "b", "k", secret()).unwrap_err(),
            R2ObjectStoreConfigError::InvalidEndpoint
        );
        assert_eq!(
            R2ObjectStoreConfig::new("https://example.invalid/", "b", "k", secret()).unwrap_err(),
            R2ObjectStoreConfigError::InvalidEndpoint
        );
        assert_eq!(
            R2ObjectStoreConfig::new("https://example.invalid", "b/c", "k", secret()).unwrap_err(),
            R2ObjectStoreConfigError::InvalidIdentity
        );
    }

    #[test]
    fn keys_and_query_values_are_encoded_the_same_way_by_both_readers_and_writers() {
        assert_eq!(
            encode_path("vault/旅行 照片.png"),
            "vault/%E6%97%85%E8%A1%8C%20%E7%85%A7%E7%89%87.png"
        );
        assert_eq!(encode_query_value("a/b c"), "a%2Fb%20c");
        assert_eq!(
            canonical_query(&[
                ("prefix".to_owned(), "vault/".to_owned()),
                ("list-type".to_owned(), "2".to_owned()),
                ("continuation-token".to_owned(), "a b".to_owned()),
            ]),
            "continuation-token=a%20b&list-type=2&prefix=vault%2F"
        );
    }

    #[test]
    fn the_endpoint_host_is_extracted_without_a_port_loss() {
        assert_eq!(
            host_of("https://example.invalid").unwrap(),
            "example.invalid"
        );
        assert_eq!(host_of("http://localhost:9000").unwrap(), "localhost:9000");
        assert!(host_of("ftp://example.invalid").is_err());
    }
}
