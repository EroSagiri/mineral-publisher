use std::{error::Error, fmt};

use crate::domain::Sha256;

/// The fixed object-key prefix every delivered asset lives under.
///
/// Publication identity is content-addressed, so the logical vault path never
/// becomes object identity: renaming `notes/photo.jpg` cannot orphan or
/// overwrite the object, and two different logical assets with identical
/// published bytes share exactly one physical object.
pub const ASSET_OBJECT_KEY_PREFIX: &str = "assets/sha256";

/// The media type of the exact bytes that will be served.
///
/// This is a sanitizer/inspection fact, never a file-name guess: an image that
/// sanitization re-encoded to JPEG is `image/jpeg` even when the vault path still
/// ends in `.png`, and an asset published unchanged carries the media type the
/// program check actually detected in its bytes.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AssetContentType(String);

impl AssetContentType {
    pub fn new(value: impl Into<String>) -> Result<Self, AssetContentTypeError> {
        let value = value.into();
        let mut parts = value.split('/');
        let (Some(kind), Some(subtype), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(AssetContentTypeError::NotAMediaType);
        };
        if !is_media_type_token(kind) || !is_media_type_token(subtype) {
            return Err(AssetContentTypeError::NotAMediaType);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetContentType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A single `type/subtype` token: no parameters, no whitespace, no wildcards.
fn is_media_type_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'+'))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetContentTypeError {
    NotAMediaType,
}

impl fmt::Display for AssetContentTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset content type must be a plain `type/subtype` media type")
    }
}

impl Error for AssetContentTypeError {}

/// The pure, deterministic inputs that turn published bytes into a delivery
/// location.
///
/// This is a plain domain input, not runtime configuration: core never reads an
/// environment variable, a config file, or a network client. The composition
/// root decides where assets live and hands the decision down.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetDeliveryConfig {
    public_base_url: AssetPublicBaseUrl,
}

impl AssetDeliveryConfig {
    pub fn new(public_base_url: impl Into<String>) -> Result<Self, AssetDeliveryConfigError> {
        Ok(Self {
            public_base_url: AssetPublicBaseUrl::new(public_base_url)?,
        })
    }

    pub fn public_base_url(&self) -> &AssetPublicBaseUrl {
        &self.public_base_url
    }

    /// The deterministic public location of one object key.
    ///
    /// The base URL is normalized (no trailing slash) and the object key has no
    /// leading slash, so concatenation is exact and never yields `//`.
    pub fn public_url(&self, object_key: &AssetObjectKey) -> AssetPublicUrl {
        AssetPublicUrl(format!(
            "{}/{}",
            self.public_base_url.as_str(),
            object_key.as_str()
        ))
    }
}

/// A validated, canonical HTTPS base URL for published assets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetPublicBaseUrl(String);

impl AssetPublicBaseUrl {
    pub fn new(value: impl Into<String>) -> Result<Self, AssetDeliveryConfigError> {
        let value = value.into();
        if value.is_empty() {
            return Err(AssetDeliveryConfigError::Empty);
        }
        let Some(remainder) = value.strip_prefix("https://") else {
            return Err(AssetDeliveryConfigError::UnsupportedScheme);
        };
        if !value.is_ascii() {
            return Err(AssetDeliveryConfigError::NotAscii);
        }
        if value.contains(['?', '#']) {
            return Err(AssetDeliveryConfigError::ContainsQueryOrFragment);
        }
        if value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(AssetDeliveryConfigError::NotCanonical);
        }

        // Trailing slashes are normalized away so `base + "/" + key` is exact.
        let remainder = remainder.trim_end_matches('/');
        let (authority, path) = match remainder.split_once('/') {
            Some((authority, path)) => (authority, Some(path)),
            None => (remainder, None),
        };
        if authority.is_empty() {
            return Err(AssetDeliveryConfigError::MissingAuthority);
        }
        if authority.contains('@') {
            return Err(AssetDeliveryConfigError::ContainsCredentials);
        }
        if let Some(path) = path
            && path
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        {
            return Err(AssetDeliveryConfigError::NotCanonical);
        }

        Ok(Self(format!("https://{remainder}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetPublicBaseUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The deterministic object identity of one published asset blob.
///
/// The key is derived from the *published* bytes only. It is not derived from the
/// logical path, the source bytes, a clock, or a random value, so the same
/// published blob under the same scheme always produces the same key.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AssetObjectKey(String);

impl AssetObjectKey {
    pub fn for_published_sha256(published_sha256: &Sha256) -> Self {
        let hex = published_sha256.to_string();
        // `Sha256` renders as exactly 64 lowercase hexadecimal characters, so the
        // two-character fan-out prefix is always available.
        Self(format!("{ASSET_OBJECT_KEY_PREFIX}/{}/{}", &hex[..2], hex))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetObjectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The exact public HTTPS URL one published asset will be served from.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AssetPublicUrl(String);

impl AssetPublicUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetPublicUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetDeliveryConfigError {
    Empty,
    UnsupportedScheme,
    MissingAuthority,
    ContainsCredentials,
    ContainsQueryOrFragment,
    NotAscii,
    NotCanonical,
}

impl fmt::Display for AssetDeliveryConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("asset public base URL cannot be empty"),
            Self::UnsupportedScheme => {
                formatter.write_str("asset public base URL must be an absolute https URL")
            }
            Self::MissingAuthority => {
                formatter.write_str("asset public base URL must name a host")
            }
            Self::ContainsCredentials => {
                formatter.write_str("asset public base URL must not contain credentials")
            }
            Self::ContainsQueryOrFragment => formatter.write_str(
                "asset public base URL must not contain a query string or fragment",
            ),
            Self::NotAscii => {
                formatter.write_str("asset public base URL must be ASCII to stay URL-safe")
            }
            Self::NotCanonical => formatter.write_str(
                "asset public base URL must be canonical: no whitespace and no empty, `.`, or `..` path segment",
            ),
        }
    }
}

impl Error for AssetDeliveryConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(value: &str) -> AssetPublicBaseUrl {
        AssetPublicBaseUrl::new(value).unwrap()
    }

    fn url(value: &str) -> String {
        let config = AssetDeliveryConfig::new(value).unwrap();
        let key = AssetObjectKey::for_published_sha256(&Sha256::new([0xab; 32]));
        config.public_url(&key).as_str().to_owned()
    }

    #[test]
    fn object_key_is_content_addressed_and_deterministic() {
        let identity = Sha256::new([0xab; 32]);
        let first = AssetObjectKey::for_published_sha256(&identity);
        let second = AssetObjectKey::for_published_sha256(&identity);

        assert_eq!(first, second);
        assert_eq!(first.as_str(), format!("assets/sha256/ab/{}", identity));
    }

    #[test]
    fn different_published_bytes_produce_different_keys() {
        assert_ne!(
            AssetObjectKey::for_published_sha256(&Sha256::new([1; 32])),
            AssetObjectKey::for_published_sha256(&Sha256::new([2; 32]))
        );
    }

    #[test]
    fn trailing_slash_is_normalized_without_doubling_the_separator() {
        let key = AssetObjectKey::for_published_sha256(&Sha256::new([0xab; 32]));

        assert_eq!(
            base("https://assets.example.com/").as_str(),
            "https://assets.example.com"
        );
        assert_eq!(
            base("https://assets.example.com///").as_str(),
            "https://assets.example.com"
        );
        for value in [
            "https://assets.example.com",
            "https://assets.example.com/",
            "https://assets.example.com///",
        ] {
            assert_eq!(
                url(value),
                format!("https://assets.example.com/{key}"),
                "failed for {value:?}"
            );
        }
    }

    #[test]
    fn a_base_url_path_prefix_is_preserved() {
        assert_eq!(
            base("https://cdn.example.com/mineral").as_str(),
            "https://cdn.example.com/mineral"
        );
        assert_eq!(
            url("https://cdn.example.com/mineral/"),
            format!(
                "https://cdn.example.com/mineral/{ASSET_OBJECT_KEY_PREFIX}/ab/{}",
                Sha256::new([0xab; 32])
            )
        );
    }

    #[test]
    fn a_content_type_is_a_single_plain_media_type() {
        for value in [
            "image/jpeg",
            "image/png",
            "application/pdf",
            "image/svg+xml",
            "application/vnd.ms-excel",
        ] {
            assert_eq!(
                AssetContentType::new(value).unwrap().as_str(),
                value,
                "rejected {value:?}"
            );
        }
        for value in [
            "",
            "image",
            "image/",
            "/jpeg",
            "image/jpeg; charset=binary",
            "image/jpeg png",
            "image/*",
            "text/plain\nimage/png",
            "图片/png",
        ] {
            assert!(AssetContentType::new(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn unusable_base_urls_fail_closed() {
        for value in [
            "",
            "http://assets.example.com",
            "assets.example.com",
            "https://",
            "https:///path",
            "https://user:secret@assets.example.com",
            "https://assets.example.com?token=1",
            "https://assets.example.com#fragment",
            "https://assets.example.com/a//b",
            "https://assets.example.com/a/../b",
            "https://assets.example.com/a/./b",
            "https://assets.example.com/a b",
            "https://assets.example.com/资产",
        ] {
            assert!(
                AssetPublicBaseUrl::new(value).is_err(),
                "accepted {value:?}"
            );
        }
    }
}
