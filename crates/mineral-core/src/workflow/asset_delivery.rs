use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::domain::Sha256;

/// The fixed object-key prefix every delivered asset lives under.
///
/// Publication identity is content-addressed and then named: every key is
/// `assets/sha256/<2 hex>/<64 hex>/<filename>`. The digest names the exact bytes,
/// and the trailing segment is the presentation filename a browser or client sees
/// when it reads the URL. Bytes identity and presentation identity are different
/// facts, so two logical assets whose published bytes are identical still get
/// their own key when their filenames differ — a URL path is the object key, with
/// no router and no alias in between.
pub const ASSET_OBJECT_KEY_PREFIX: &str = "assets/sha256";

/// The media type of the exact bytes that will be served.
///
/// This is a sanitizer/inspection fact, never a file-name guess: an image that
/// sanitization re-encoded to JPEG is `image/jpeg` even when the vault path still
/// ends in `.png`, and an asset published unchanged carries the media type the
/// program check actually detected in its bytes.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AssetContentType(String);

impl<'de> Deserialize<'de> for AssetContentType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

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

/// The deterministic presentation filename of one published asset.
///
/// It is exactly one path segment of the object key, and the last segment of the
/// public URL. It is deliberately **not** a path: the logical vault location never
/// becomes part of object identity, so a nested source such as
/// `attachments/images/photo.png` presents as `photo.png` and can never add a
/// segment, escape the key, or introduce `..` traversal.
///
/// The value is a canonical UTF-8 segment: it is stored in the object key exactly
/// as written, and percent-encoded only when a public URL is serialized from it.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AssetPublicFilename(String);

impl<'de> Deserialize<'de> for AssetPublicFilename {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl AssetPublicFilename {
    /// The longest presentation filename a delivery may freeze.
    ///
    /// This is a domain invariant, not a runtime limit. A public filename is one
    /// path segment by definition, and a delivery fact is only usable if every
    /// runtime can name it: the bound is the largest single segment universally
    /// accepted as one name, so a name no runtime could place is refused while the
    /// delivery is being built instead of surfacing halfway through a publication,
    /// in whichever runtime happened to run first.
    pub const MAX_BYTES: usize = 255;

    pub fn new(value: impl Into<String>) -> Result<Self, AssetPublicFilenameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(AssetPublicFilenameError::Empty);
        }
        if value.contains(['/', '\\']) {
            return Err(AssetPublicFilenameError::NotASingleSegment);
        }
        if matches!(value.as_str(), "." | "..") {
            return Err(AssetPublicFilenameError::RelativeSegment);
        }
        if value.chars().any(|character| character.is_control()) {
            return Err(AssetPublicFilenameError::ControlCharacter);
        }
        if value.len() > Self::MAX_BYTES {
            return Err(AssetPublicFilenameError::TooLong);
        }
        Ok(Self(value))
    }

    /// Derives the presentation filename of one logical asset.
    ///
    /// The filename is the logical path's **last** segment, and its extension is
    /// reconciled with the frozen published media type: when sanitization changed
    /// the format, the source extension would describe a representation that is
    /// no longer being served, so the stem is kept and the canonical extension of
    /// the published format replaces it. The extension is never guessed from the
    /// source name or a source MIME type.
    pub fn from_logical_path(
        logical_path: &crate::domain::ContentPath,
        published_content_type: &AssetContentType,
    ) -> Result<Self, AssetPublicFilenameError> {
        let basename = logical_path
            .as_str()
            .rsplit('/')
            .next()
            .expect("a canonical content path always has a last segment");
        let rule = extension_rule(published_content_type)?;
        let (stem, extension) = split_extension(basename);
        let name = match rule {
            // The media type claims nothing about the format, so the authored
            // extension cannot contradict it and is kept as it is.
            ExtensionRule::Unclaimed => basename.to_owned(),
            ExtensionRule::Exact(canonical) => match extension {
                Some(extension)
                    if canonical
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(extension)) =>
                {
                    basename.to_owned()
                }
                _ => {
                    let stem = stem.trim_end_matches('.');
                    if stem.is_empty() {
                        return Err(AssetPublicFilenameError::UnusableBasename);
                    }
                    format!("{stem}.{}", canonical[0])
                }
            },
        };
        Self::new(name)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetPublicFilename {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How one published media type constrains the filename extension.
enum ExtensionRule {
    /// The media type names an exact format, so the filename must carry one of
    /// these extensions (`canonical[0]` is the one written when it must change).
    Exact(&'static [&'static str]),
    /// The media type explicitly claims nothing about the format, so any
    /// extension — or none — is consistent with it.
    Unclaimed,
}

/// The media types this publisher can name truthfully.
///
/// A type that is absent here is refused rather than guessed at: writing a
/// filename whose extension the frozen media type contradicts would serve a
/// self-describing lie. Adding an entry is a deliberate statement that the
/// mapping is known.
fn extension_rule(
    content_type: &AssetContentType,
) -> Result<ExtensionRule, AssetPublicFilenameError> {
    let rule = match content_type.as_str() {
        "image/png" => ExtensionRule::Exact(&["png"]),
        "image/jpeg" => ExtensionRule::Exact(&["jpg", "jpeg"]),
        "image/gif" => ExtensionRule::Exact(&["gif"]),
        "image/webp" => ExtensionRule::Exact(&["webp"]),
        "image/avif" => ExtensionRule::Exact(&["avif"]),
        "image/tiff" => ExtensionRule::Exact(&["tiff", "tif"]),
        "image/bmp" => ExtensionRule::Exact(&["bmp"]),
        "image/svg+xml" => ExtensionRule::Exact(&["svg"]),
        "application/pdf" => ExtensionRule::Exact(&["pdf"]),
        "application/json" => ExtensionRule::Exact(&["json"]),
        "application/zip" => ExtensionRule::Exact(&["zip"]),
        "text/plain" => ExtensionRule::Exact(&["txt"]),
        "text/markdown" => ExtensionRule::Exact(&["md"]),
        "text/csv" => ExtensionRule::Exact(&["csv"]),
        "audio/mpeg" => ExtensionRule::Exact(&["mp3"]),
        "video/mp4" => ExtensionRule::Exact(&["mp4"]),
        "application/octet-stream" => ExtensionRule::Unclaimed,
        _ => return Err(AssetPublicFilenameError::UnsupportedContentType),
    };
    Ok(rule)
}

/// Splits a basename into its stem and its extension.
///
/// A leading dot is part of the name (`.gitignore` has no extension), and a
/// trailing dot is not an extension separator either.
fn split_extension(basename: &str) -> (&str, Option<&str>) {
    match basename.rfind('.') {
        Some(index) if index > 0 && index + 1 < basename.len() => {
            (&basename[..index], Some(&basename[index + 1..]))
        }
        _ => (basename, None),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetPublicFilenameError {
    Empty,
    NotASingleSegment,
    RelativeSegment,
    ControlCharacter,
    TooLong,
    UnusableBasename,
    UnsupportedContentType,
}

impl fmt::Display for AssetPublicFilenameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("asset public filename cannot be empty"),
            Self::NotASingleSegment => formatter.write_str(
                "asset public filename must be one path segment: no `/` and no `\\`",
            ),
            Self::RelativeSegment => {
                formatter.write_str("asset public filename must not be `.` or `..`")
            }
            Self::ControlCharacter => {
                formatter.write_str("asset public filename must not contain control characters")
            }
            Self::TooLong => write!(
                formatter,
                "a public filename is one path segment and must be at most {} bytes",
                AssetPublicFilename::MAX_BYTES
            ),
            Self::UnusableBasename => formatter
                .write_str("asset public filename has no stem left once its extension is replaced"),
            Self::UnsupportedContentType => formatter.write_str(
                "published media type has no known canonical filename extension, so no truthful filename can be derived",
            ),
        }
    }
}

impl Error for AssetPublicFilenameError {}

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
    /// leading slash, so concatenation is exact and never yields `//`. The key is
    /// serialized in its URL form, which percent-encodes the presentation filename
    /// segment — a URL is not a key, and a filename that is not URL-safe must not
    /// be able to add a segment or truncate the path.
    pub fn public_url(&self, object_key: &AssetObjectKey) -> AssetPublicUrl {
        AssetPublicUrl(format!(
            "{}/{}",
            self.public_base_url.as_str(),
            object_key.as_url_path()
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

/// The deterministic delivery identity of one published asset blob.
///
/// It names the published bytes, and — since S6.4.1 — the presentation filename
/// they are served under:
///
/// ```text
/// assets/sha256/<2 hex>/<64 hex>/<filename>
/// ```
///
/// The two halves are different facts. The digest is bytes identity: it comes
/// from the published representation alone, never from the logical path, a clock
/// or a random value. The filename is presentation identity: it comes from the
/// logical asset's basename, reconciled with the frozen published media type.
/// Two logical assets whose published bytes are identical therefore share a
/// digest and still get one key each when their filenames differ.
///
/// The legacy shape `assets/sha256/<2 hex>/<64 hex>` is still understood, because
/// delivery projections published before S6.4.1 froze it durably. It is only ever
/// *rebuilt* by the V1 wire decoder — no current publication path can produce it.
#[derive(Clone, Debug)]
pub struct AssetObjectKey {
    /// The key exactly as an object store names it.
    ///
    /// Every other fact is derived from this one string, which keeps the type
    /// small enough to travel inside errors and observations without a box: the
    /// digest is the 64 hexadecimal characters the canonical shape pins, and the
    /// filename is the segment after it.
    text: String,
    /// The presentation filename, or `None` for the legacy filename-less shape.
    public_filename: Option<AssetPublicFilename>,
}

impl AssetObjectKey {
    /// The current scheme: content-addressed, with the presentation filename.
    pub fn for_published_asset(
        published_sha256: &Sha256,
        public_filename: &AssetPublicFilename,
    ) -> Self {
        let hex = published_sha256.to_string();
        // `Sha256` renders as exactly 64 lowercase hexadecimal characters, so the
        // two-character fan-out prefix is always derivable.
        Self {
            text: format!(
                "{ASSET_OBJECT_KEY_PREFIX}/{}/{hex}/{}",
                &hex[..2],
                public_filename.as_str()
            ),
            public_filename: Some(public_filename.clone()),
        }
    }

    /// The legacy scheme, for the durable V1 shape only.
    ///
    /// A key built here is a filename-less key, so it can never be produced by a
    /// current publication without the intent being obvious at the call site.
    pub fn legacy_for_published_sha256(published_sha256: &Sha256) -> Self {
        let hex = published_sha256.to_string();
        Self {
            text: format!("{ASSET_OBJECT_KEY_PREFIX}/{}/{hex}", &hex[..2]),
            public_filename: None,
        }
    }

    /// Rebuilds a key from its durable textual form, in either shape.
    ///
    /// Both canonical shapes are pinned segment by segment, so a stored key can
    /// never smuggle in an absolute path, a `..` segment, an extra segment, or a
    /// key that does not match the bytes it names. Whether the filename segment is
    /// *required* is a question for the wire version that stored it, not for this
    /// parser: V1 rows have none, V2 rows must have one.
    pub fn rehydrate(value: impl Into<String>) -> Result<Self, AssetObjectKeyError> {
        let value = value.into();
        let Some(rest) = value
            .strip_prefix(ASSET_OBJECT_KEY_PREFIX)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return Err(AssetObjectKeyError::NotCanonical);
        };
        let mut segments = rest.split('/');
        let (Some(prefix), Some(digest)) = (segments.next(), segments.next()) else {
            return Err(AssetObjectKeyError::NotCanonical);
        };
        let public_filename = match segments.next() {
            None => None,
            Some(filename) => Some(
                AssetPublicFilename::new(filename)
                    .map_err(|_| AssetObjectKeyError::NotCanonical)?,
            ),
        };
        if segments.next().is_some() {
            return Err(AssetObjectKeyError::NotCanonical);
        }
        if !is_lower_hex(prefix, 2) || !is_lower_hex(digest, 64) || !digest.starts_with(prefix) {
            return Err(AssetObjectKeyError::NotCanonical);
        }
        Ok(Self {
            text: value,
            public_filename,
        })
    }

    /// The key exactly as an object store names it.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The key as it appears in a public URL path.
    ///
    /// The store's key and the URL are different serializations of the same
    /// identity: the filename segment is percent-encoded here and nowhere else.
    pub fn as_url_path(&self) -> String {
        let Some(filename) = &self.public_filename else {
            // The legacy key has no segment that needs encoding.
            return self.text.clone();
        };
        let prefix = self
            .text
            .strip_suffix(filename.as_str())
            .expect("a key always ends with the filename it carries");
        format!("{prefix}{}", percent_encode_segment(filename.as_str()))
    }

    /// The published-bytes identity this key names.
    pub fn published_sha256(&self) -> Sha256 {
        let hex = self.digest_hex();
        let mut bytes = [0_u8; 32];
        for (index, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let text = std::str::from_utf8(chunk).expect("hexadecimal digits are ASCII");
            bytes[index] = u8::from_str_radix(text, 16).expect("validated hexadecimal");
        }
        Sha256::new(bytes)
    }

    /// The 64 hexadecimal characters every canonical shape carries.
    fn digest_hex(&self) -> &str {
        // `assets/sha256/` is 14 bytes; the fan-out segment and its separator are
        // three more, and the digest is exactly 64 characters. Only the three
        // constructors of this type produce that shape.
        let start = ASSET_OBJECT_KEY_PREFIX.len() + 4;
        self.text
            .get(start..start + 64)
            .expect("a canonical asset object key embeds a 64-character digest")
    }

    /// The presentation filename, absent only for the legacy durable shape.
    pub fn public_filename(&self) -> Option<&AssetPublicFilename> {
        self.public_filename.as_ref()
    }

    /// Whether this key is the filename-less shape frozen by wire V1.
    pub fn is_legacy(&self) -> bool {
        self.public_filename.is_none()
    }
}

impl PartialEq for AssetObjectKey {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for AssetObjectKey {}

impl Ord for AssetObjectKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.text.cmp(&other.text)
    }
}

impl PartialOrd for AssetObjectKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Percent-encodes one path segment, keeping only the unreserved characters.
///
/// Everything else — including `%` itself, so an encoded name can never be
/// decoded twice — becomes uppercase `%XX` over its UTF-8 bytes.
fn percent_encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetObjectKeyError {
    NotCanonical,
}

impl fmt::Display for AssetObjectKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset object key is not the canonical content-addressed key")
    }
}

impl Error for AssetObjectKeyError {}

impl fmt::Display for AssetObjectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
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
    use crate::domain::ContentPath;

    use super::*;

    fn base(value: &str) -> AssetPublicBaseUrl {
        AssetPublicBaseUrl::new(value).unwrap()
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn media_type(value: &str) -> AssetContentType {
        AssetContentType::new(value).unwrap()
    }

    fn filename(logical_path: &str, content_type: &str) -> AssetPublicFilename {
        AssetPublicFilename::from_logical_path(&path(logical_path), &media_type(content_type))
            .unwrap()
    }

    fn key(published: &Sha256, logical_path: &str, content_type: &str) -> AssetObjectKey {
        AssetObjectKey::for_published_asset(published, &filename(logical_path, content_type))
    }

    fn config(value: &str) -> AssetDeliveryConfig {
        AssetDeliveryConfig::new(value).unwrap()
    }

    fn url(value: &str, key: &AssetObjectKey) -> String {
        config(value).public_url(key).as_str().to_owned()
    }

    // 1/2/3. key shape, prefix, basename
    #[test]
    fn an_object_key_names_the_bytes_and_the_presentation_filename() {
        let identity = Sha256::new([0xab; 32]);
        let key = key(&identity, "attachments/object.png", "image/png");

        assert_eq!(
            key.as_str(),
            format!("assets/sha256/ab/{identity}/object.png")
        );
        // The fan-out prefix is not an independent fact: it is the digest's first
        // two hexadecimal characters, and the key can be read back to the digest.
        assert_eq!(&key.as_str()[14..16], &identity.to_string()[..2]);
        assert_eq!(key.published_sha256(), identity);
        assert_eq!(key.public_filename().unwrap().as_str(), "object.png");
        assert!(!key.is_legacy());
    }

    #[test]
    fn the_filename_is_the_logical_basename_and_never_a_path() {
        for (logical, expected) in [
            ("photo.png", "photo.png"),
            ("attachments/photo.png", "photo.png"),
            ("attachments/images/2026/trip/photo.png", "photo.png"),
            ("a/b/c/manual.pdf", "manual.pdf"),
        ] {
            assert_eq!(
                filename(logical, media_type_for(expected)).as_str(),
                expected
            );
        }
    }

    fn media_type_for(filename: &str) -> &'static str {
        match filename.rsplit('.').next().unwrap() {
            "png" => "image/png",
            "pdf" => "application/pdf",
            other => panic!("no test media type for .{other}"),
        }
    }

    // 4. traversal
    #[test]
    fn a_filename_that_is_not_one_plain_segment_is_refused() {
        for value in [
            "",
            "..",
            ".",
            "a/b",
            "a\\b",
            "/leading",
            "trailing/",
            "with\nnewline",
            "with\ttab",
            "nul\0byte",
        ] {
            assert!(
                AssetPublicFilename::new(value).is_err(),
                "accepted {value:?}"
            );
        }
        assert!(AssetPublicFilename::new("...zip").is_ok());
        // A leading dot is part of the name (`.gitignore`), never an extension.
        assert!(AssetPublicFilename::new(".zip").is_ok());
        assert!(AssetPublicFilename::new("a b.png").is_ok());
        // The segment bound is a domain rule: exactly the bound is a usable name,
        // one byte more is not a name a delivery may freeze.
        assert!(AssetPublicFilename::new("x".repeat(AssetPublicFilename::MAX_BYTES)).is_ok());
        assert!(AssetPublicFilename::new("x".repeat(AssetPublicFilename::MAX_BYTES + 1)).is_err());
        // It counts bytes, not characters: a multi-byte name is held to the same
        // single-segment invariant.
        assert!(AssetPublicFilename::new("旅".repeat(85)).is_ok());
        assert!(AssetPublicFilename::new("旅".repeat(86)).is_err());
    }

    // 5/6/7. same bytes, same or different filename; different bytes, same filename
    #[test]
    fn the_key_is_the_pair_of_bytes_identity_and_presentation_filename() {
        let identity = Sha256::new([0xab; 32]);
        let other = Sha256::new([0xcd; 32]);

        assert_eq!(
            key(&identity, "a/photo.png", "image/png"),
            key(&identity, "b/photo.png", "image/png"),
            "same bytes under one filename are one object"
        );
        assert_ne!(
            key(&identity, "a/photo.png", "image/png"),
            key(&identity, "a/trip.png", "image/png"),
            "same bytes under different filenames are different objects"
        );
        assert_ne!(
            key(&identity, "a/photo.png", "image/png"),
            key(&other, "a/photo.png", "image/png"),
            "different bytes under one filename are different objects"
        );
        // Same filename, different digest: the fan-out directory separates them.
        assert!(
            key(&other, "a/photo.png", "image/png")
                .as_str()
                .contains("/cd/")
        );
    }

    // 9. the extension describes the published representation
    #[test]
    fn the_extension_follows_the_published_media_type_not_the_source_name() {
        // Sanitization re-encoded a JPEG source to PNG.
        assert_eq!(
            filename("attachments/photo.jpg", "image/png").as_str(),
            "photo.png"
        );
        // And the other way round.
        assert_eq!(
            filename("attachments/photo.png", "image/jpeg").as_str(),
            "photo.jpg"
        );
        // A JPEG keeps a JPEG name, in either spelling, whatever the case.
        assert_eq!(filename("photo.jpeg", "image/jpeg").as_str(), "photo.jpeg");
        assert_eq!(filename("PHOTO.JPG", "image/jpeg").as_str(), "PHOTO.JPG");
        // No extension at all: the canonical one is added.
        assert_eq!(
            filename("attachments/manual", "application/pdf").as_str(),
            "manual.pdf"
        );
        // Dots inside the stem survive; only the last extension is replaced.
        assert_eq!(
            filename("archive.tar.gz", "application/zip").as_str(),
            "archive.tar.zip"
        );
        // A leading dot is part of the name, not an extension.
        assert_eq!(
            filename(".gitignore", "text/plain").as_str(),
            ".gitignore.txt"
        );
        // A media type that names an exact format this publisher cannot name
        // truthfully is refused rather than given a made-up extension.
        assert_eq!(
            AssetPublicFilename::from_logical_path(
                &path("thing.png"),
                &media_type("application/x-unknown")
            ),
            Err(AssetPublicFilenameError::UnsupportedContentType)
        );
        // A format-agnostic type claims nothing, so the authored name stands.
        assert_eq!(
            filename("data.bin", "application/octet-stream").as_str(),
            "data.bin"
        );
        assert_eq!(
            filename("data", "application/octet-stream").as_str(),
            "data"
        );
    }

    // 10. unicode
    #[test]
    fn a_unicode_filename_is_a_canonical_key_segment_and_an_encoded_url_segment() {
        let key = AssetObjectKey::for_published_asset(
            &Sha256::new([0xab; 32]),
            &filename("attachments/旅行照片.png", "image/png"),
        );

        // The key is the canonical UTF-8 segment: exactly what an object store
        // names.
        assert_eq!(
            key.as_str(),
            format!("assets/sha256/ab/{}/旅行照片.png", Sha256::new([0xab; 32]))
        );
        assert!(key.as_str().ends_with("/旅行照片.png"));
        // The URL is a different serialization of the same identity.
        assert_eq!(
            key.as_url_path(),
            format!(
                "assets/sha256/ab/{}/%E6%97%85%E8%A1%8C%E7%85%A7%E7%89%87.png",
                Sha256::new([0xab; 32])
            )
        );
        assert_eq!(
            url("https://assets.example.com", &key),
            format!(
                "https://assets.example.com/assets/sha256/ab/{}/%E6%97%85%E8%A1%8C%E7%85%A7%E7%89%87.png",
                Sha256::new([0xab; 32])
            )
        );
        // The encoded segment never contains a separator or a reserved delimiter,
        // so it cannot add a path segment or end the URL early.
        assert_eq!(
            key.as_url_path().matches('/').count(),
            key.as_str().matches('/').count()
        );
        assert!(!key.as_url_path().contains(['?', '#', ' ']));
    }

    #[test]
    fn url_serialization_encodes_every_reserved_byte_exactly_once() {
        let key = AssetObjectKey::for_published_asset(
            &Sha256::new([0xab; 32]),
            &AssetPublicFilename::new("a b&c%20d?e#f.png").unwrap(),
        );

        assert!(
            key.as_str().ends_with("/a b&c%20d?e#f.png"),
            "the key keeps the bytes"
        );
        assert!(
            key.as_url_path().ends_with("/a%20b%26c%2520d%3Fe%23f.png"),
            "a literal % must not survive as an escape introducer: {}",
            key.as_url_path()
        );
        // Unreserved characters are never escaped, so extensions stay readable.
        assert!(
            key.as_url_path().ends_with(".png"),
            "the extension stays literal"
        );
    }

    #[test]
    fn a_legacy_key_and_a_current_key_are_distinguishable_and_ordered_by_text() {
        let identity = Sha256::new([0xab; 32]);
        let legacy = AssetObjectKey::legacy_for_published_sha256(&identity);
        let current = key(&identity, "photo.png", "image/png");

        assert_eq!(legacy.as_str(), format!("assets/sha256/ab/{identity}"));
        assert!(legacy.is_legacy());
        assert_eq!(legacy.public_filename(), None);
        assert_eq!(legacy.as_url_path(), legacy.as_str());
        assert_eq!(legacy.published_sha256(), identity);
        assert!(legacy < current, "the legacy key is a strict prefix");
    }

    // 8/14. durable shapes
    #[test]
    fn an_object_key_rehydrates_in_both_durable_shapes_and_nothing_else() {
        let identity = Sha256::new([0xab; 32]);
        let legacy = AssetObjectKey::legacy_for_published_sha256(&identity);
        let current = key(&identity, "photo.png", "image/png");

        assert_eq!(
            AssetObjectKey::rehydrate(legacy.as_str()).unwrap().as_str(),
            legacy.as_str()
        );
        assert_eq!(
            AssetObjectKey::rehydrate(current.as_str()).unwrap(),
            current
        );
        for value in [
            "".to_owned(),
            "assets/sha256/ab".to_owned(),
            format!("assets/sha256/{identity}"),
            format!("assets/sha256/ab/{}", "0".repeat(64)),
            format!("assets/sha256/AB/{identity}"),
            format!("assets/sha256/ab/{identity}/extra/segment.png"),
            format!("assets/sha256/ab/{identity}/"),
            format!("assets/sha256/ab/{identity}/.."),
            format!("assets/sha256/ab/{identity}/a/b.png"),
            format!("assets//sha256/ab/{identity}"),
            format!("assets/sha256/../ab/{identity}"),
            format!("/assets/sha256/ab/{identity}"),
            format!("assets\\sha256\\ab\\{identity}"),
        ] {
            assert!(
                AssetObjectKey::rehydrate(value.clone()).is_err(),
                "accepted {value:?}"
            );
        }
    }

    #[test]
    fn different_published_bytes_produce_different_keys() {
        assert_ne!(
            key(&Sha256::new([1; 32]), "a.png", "image/png"),
            key(&Sha256::new([2; 32]), "a.png", "image/png")
        );
    }

    #[test]
    fn trailing_slash_is_normalized_without_doubling_the_separator() {
        let key = key(&Sha256::new([0xab; 32]), "photo.png", "image/png");

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
                url(value, &key),
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
            url(
                "https://cdn.example.com/mineral/",
                &key(&Sha256::new([0xab; 32]), "photo.png", "image/png")
            ),
            format!(
                "https://cdn.example.com/mineral/{ASSET_OBJECT_KEY_PREFIX}/ab/{}/photo.png",
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
