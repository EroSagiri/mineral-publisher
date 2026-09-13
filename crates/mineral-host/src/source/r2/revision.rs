use std::{error::Error, fmt};

use mineral_core::source::{SOURCE_REVISION_VERSION, SourceRevision};

use crate::object_store::encode_query_value;

/// The remote facts one listed object was observed with.
///
/// These are the strongest, actually observable facts a listing offers: the object's
/// ETag, its last-modified timestamp and its reported size. They describe a
/// *generation* of a remote object. None of them is a content hash, and this type
/// deliberately offers no conversion to one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct R2ObjectRevision {
    etag: String,
    last_modified: String,
    size: u64,
}

impl R2ObjectRevision {
    pub fn new(etag: impl Into<String>, last_modified: impl Into<String>, size: u64) -> Self {
        Self {
            etag: etag.into(),
            last_modified: last_modified.into(),
            size,
        }
    }

    pub fn etag(&self) -> &str {
        &self.etag
    }

    pub fn last_modified(&self) -> &str {
        &self.last_modified
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The `If-Match` value that asks the endpoint for exactly this generation.
    pub fn if_match(&self) -> String {
        if self.etag.starts_with('"') {
            self.etag.clone()
        } else {
            format!("\"{}\"", self.etag)
        }
    }

    /// Encodes these facts as the opaque revision the engine stores.
    ///
    /// Every component is percent-encoded, so the encoding is unambiguous even for
    /// an ETag that contains the separators, and the result never carries a control
    /// character.
    pub fn encode(&self) -> Result<SourceRevision, R2ObjectRevisionError> {
        let payload = format!(
            "etag={};last-modified={};size={}",
            encode_query_value(&self.etag),
            encode_query_value(&self.last_modified),
            self.size
        );
        SourceRevision::versioned(payload).map_err(R2ObjectRevisionError::Revision)
    }

    /// Decodes the revision one inventory observed.
    ///
    /// An unknown encoding version, a missing field or a damaged component fails
    /// closed: a revision this engine cannot read must never be treated as "no
    /// change", and it must never be silently reinterpreted as a newer encoding.
    pub fn decode(revision: &SourceRevision) -> Result<Self, R2ObjectRevisionError> {
        if revision.version() != SOURCE_REVISION_VERSION {
            return Err(R2ObjectRevisionError::UnknownVersion);
        }
        let mut etag = None;
        let mut last_modified = None;
        let mut size = None;
        for field in revision.payload().split(';') {
            let (name, value) = field
                .split_once('=')
                .ok_or(R2ObjectRevisionError::Damaged)?;
            match name {
                "etag" => {
                    if etag.replace(decode_query_value(value)?).is_some() {
                        return Err(R2ObjectRevisionError::Damaged);
                    }
                }
                "last-modified" => {
                    if last_modified.replace(decode_query_value(value)?).is_some() {
                        return Err(R2ObjectRevisionError::Damaged);
                    }
                }
                "size" => {
                    let parsed = value
                        .parse::<u64>()
                        .map_err(|_| R2ObjectRevisionError::Damaged)?;
                    if size.replace(parsed).is_some() {
                        return Err(R2ObjectRevisionError::Damaged);
                    }
                }
                _ => return Err(R2ObjectRevisionError::Damaged),
            }
        }
        Ok(Self {
            etag: etag.ok_or(R2ObjectRevisionError::Damaged)?,
            last_modified: last_modified.ok_or(R2ObjectRevisionError::Damaged)?,
            size: size.ok_or(R2ObjectRevisionError::Damaged)?,
        })
    }
}

fn decode_query_value(encoded: &str) -> Result<String, R2ObjectRevisionError> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .ok_or(R2ObjectRevisionError::Damaged)?;
                let hex = std::str::from_utf8(hex).map_err(|_| R2ObjectRevisionError::Damaged)?;
                let byte =
                    u8::from_str_radix(hex, 16).map_err(|_| R2ObjectRevisionError::Damaged)?;
                decoded.push(byte);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| R2ObjectRevisionError::Damaged)
}

/// Why a remote revision cannot be written or read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum R2ObjectRevisionError {
    /// The revision is not an encoding this reader knows.
    UnknownVersion,
    /// A stored revision is missing, duplicated or damaged.
    Damaged,
    /// The engine refused the encoded revision.
    Revision(mineral_core::source::SourceRevisionError),
}

impl fmt::Display for R2ObjectRevisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownVersion => {
                formatter.write_str("remote revision uses an unknown encoding version")
            }
            Self::Damaged => formatter.write_str("remote revision is damaged"),
            Self::Revision(error) => write!(formatter, "remote revision is unusable: {error}"),
        }
    }
}

impl Error for R2ObjectRevisionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_revision_round_trips_with_every_observable_fact() {
        let revision = R2ObjectRevision::new(
            "d41d8cd98f00b204e9800998ecf8427e",
            "2024-01-01T00:00:00.000Z",
            3,
        );

        let encoded = revision.encode().unwrap();

        assert_eq!(R2ObjectRevision::decode(&encoded).unwrap(), revision);
        assert_eq!(revision.if_match(), "\"d41d8cd98f00b204e9800998ecf8427e\"");
    }

    #[test]
    fn any_change_to_an_observed_fact_changes_the_revision() {
        let base = R2ObjectRevision::new("etag-a", "2024-01-01T00:00:00.000Z", 3);
        let other_etag = R2ObjectRevision::new("etag-b", "2024-01-01T00:00:00.000Z", 3);
        let other_time = R2ObjectRevision::new("etag-a", "2024-01-02T00:00:00.000Z", 3);
        let other_size = R2ObjectRevision::new("etag-a", "2024-01-01T00:00:00.000Z", 4);

        assert_ne!(base.encode().unwrap(), other_etag.encode().unwrap());
        assert_ne!(base.encode().unwrap(), other_time.encode().unwrap());
        assert_ne!(base.encode().unwrap(), other_size.encode().unwrap());
    }

    #[test]
    fn an_etag_with_separators_still_encodes_unambiguously() {
        let revision = R2ObjectRevision::new("a;b=c%d", "2024-01-01T00:00:00.000Z", 0);

        let encoded = revision.encode().unwrap();

        assert!(!encoded.payload().contains("a;b"));
        assert_eq!(R2ObjectRevision::decode(&encoded).unwrap(), revision);
    }

    #[test]
    fn a_revision_from_another_version_or_a_damaged_row_fails_closed() {
        assert_eq!(
            R2ObjectRevision::decode(
                &SourceRevision::new("v2:etag=a;last-modified=b;size=1").unwrap()
            )
            .unwrap_err(),
            R2ObjectRevisionError::UnknownVersion
        );
        for damaged in [
            "v1:etag=a;last-modified=b",                // missing size
            "v1:etag=a;last-modified=b;size=one",       // unparsable size
            "v1:etag=a;last-modified=b;size=1;other=2", // unknown field
            "v1:etag=a;last-modified=b;size=1;size=2",  // duplicated field
            "v1:etag=a;last-modified=b;size=1;etag=c",
            "v1:etag=%zz;last-modified=b;size=1", // bad escape
        ] {
            assert!(
                R2ObjectRevision::decode(&SourceRevision::new(damaged).unwrap()).is_err(),
                "{damaged} was accepted"
            );
        }
    }
}
