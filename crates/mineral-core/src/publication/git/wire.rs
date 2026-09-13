use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::domain::TimestampMillis;

use super::model::{GitCommitOid, GitCommitSpec, GitCommitSpecError, GitTreeOid};

/// Versioned durable encoding of a frozen [`GitCommitSpec`].
///
/// The column that stores a specification is a single nullable `TEXT`, so the
/// content has to carry its own version: when the specification shape changes, an
/// old durable row must fail loudly instead of being silently bound to today's
/// field set. Version 1 is:
///
/// ```json
/// {
///   "version": 1,
///   "parent": "…",
///   "tree": "…",
///   "author":    { "name": "…", "email": "…", "time_unix_ms": 0 },
///   "committer": { "name": "…", "email": "…", "time_unix_ms": 0 },
///   "message": "…"
/// }
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct CommitSpecWire;

impl CommitSpecWire {
    /// The only durable version this engine writes and reads.
    pub const VERSION: u32 = 1;

    pub fn encode(spec: &GitCommitSpec) -> String {
        let wire = WireCommitSpec {
            version: Self::VERSION,
            parent: spec.parent().as_str().to_owned(),
            tree: spec.tree().as_str().to_owned(),
            author: WireIdentity {
                name: spec.author_name().to_owned(),
                email: spec.author_email().to_owned(),
                time_unix_ms: spec.author_time().as_unix_millis(),
            },
            committer: WireIdentity {
                name: spec.committer_name().to_owned(),
                email: spec.committer_email().to_owned(),
                time_unix_ms: spec.committer_time().as_unix_millis(),
            },
            message: spec.message().to_owned(),
        };
        serde_json::to_string(&wire)
            .expect("a commit specification is always representable as a JSON object")
    }

    /// Decodes a durable specification, rejecting anything this engine cannot
    /// interpret as exactly the frozen commit identity it describes.
    pub fn decode(value: &str) -> Result<GitCommitSpec, CommitSpecWireError> {
        // The version is read first so an unknown durable format is reported as
        // such instead of as the shape error its different fields would cause.
        let probe: WireVersion =
            serde_json::from_str(value).map_err(|_| CommitSpecWireError::Malformed)?;
        if probe.version != Self::VERSION {
            return Err(CommitSpecWireError::UnsupportedVersion(probe.version));
        }
        let wire: WireCommitSpec =
            serde_json::from_str(value).map_err(|_| CommitSpecWireError::Malformed)?;
        GitCommitSpec::new(
            GitCommitOid::new(wire.parent).map_err(|_| CommitSpecWireError::Malformed)?,
            GitTreeOid::new(wire.tree).map_err(|_| CommitSpecWireError::Malformed)?,
            wire.author.name,
            wire.author.email,
            TimestampMillis::from_unix_millis(wire.author.time_unix_ms),
            wire.committer.name,
            wire.committer.email,
            TimestampMillis::from_unix_millis(wire.committer.time_unix_ms),
            wire.message,
        )
        .map_err(CommitSpecWireError::Invalid)
    }
}

#[derive(Deserialize)]
struct WireVersion {
    version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCommitSpec {
    version: u32,
    parent: String,
    tree: String,
    author: WireIdentity,
    committer: WireIdentity,
    message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIdentity {
    name: String,
    email: String,
    time_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitSpecWireError {
    /// The payload is not a JSON object with the versioned shape.
    Malformed,
    /// The payload declares a durable version this engine does not understand.
    UnsupportedVersion(u32),
    /// The payload is structurally sound but is not a valid commit specification.
    Invalid(GitCommitSpecError),
}

impl fmt::Display for CommitSpecWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("encoded commit specification is malformed"),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "unsupported commit specification version: {version}"
            ),
            Self::Invalid(error) => {
                write!(
                    formatter,
                    "encoded commit specification is invalid: {error}"
                )
            }
        }
    }
}

impl Error for CommitSpecWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Malformed | Self::UnsupportedVersion(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(value: char) -> GitCommitOid {
        GitCommitOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn tree(value: char) -> GitTreeOid {
        GitTreeOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn spec() -> GitCommitSpec {
        GitCommitSpec::new(
            commit('a'),
            tree('b'),
            "Mineral Publisher",
            "publisher@example.invalid",
            TimestampMillis::from_unix_millis(1_000),
            "Mineral Publisher",
            "publisher@example.invalid",
            TimestampMillis::from_unix_millis(1_000),
            "Publish Mineral content",
        )
        .unwrap()
    }

    #[test]
    fn version_one_pins_the_field_names_and_the_identity_inputs() {
        let encoded = CommitSpecWire::encode(&spec());

        assert_eq!(
            encoded,
            format!(
                concat!(
                    "{{\"version\":1,",
                    "\"parent\":\"{}\",",
                    "\"tree\":\"{}\",",
                    "\"author\":{{\"name\":\"Mineral Publisher\",",
                    "\"email\":\"publisher@example.invalid\",\"time_unix_ms\":1000}},",
                    "\"committer\":{{\"name\":\"Mineral Publisher\",",
                    "\"email\":\"publisher@example.invalid\",\"time_unix_ms\":1000}},",
                    "\"message\":\"Publish Mineral content\"}}"
                ),
                "a".repeat(40),
                "b".repeat(40)
            )
        );
        assert_eq!(CommitSpecWire::decode(&encoded).unwrap(), spec());
    }

    #[test]
    fn an_unknown_durable_version_is_rejected() {
        let encoded = CommitSpecWire::encode(&spec());
        assert_eq!(
            CommitSpecWire::decode(&encoded.replace("\"version\":1", "\"version\":2")),
            Err(CommitSpecWireError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn a_payload_that_is_not_the_versioned_shape_is_rejected() {
        let encoded = CommitSpecWire::encode(&spec());
        // A structurally valid version 1 payload that carries an extra field.
        let unknown_field = encoded.replace("\"message\"", "\"unexpected\":1,\"message\"");
        for value in [
            "",
            "null",
            "[]",
            "not json",
            "{}",
            "{\"version\":1}",
            "{\"version\":\"1\"}",
            unknown_field.as_str(),
        ] {
            assert_eq!(
                CommitSpecWire::decode(value),
                Err(CommitSpecWireError::Malformed),
                "accepted {value:?}"
            );
        }
    }

    #[test]
    fn a_valid_shape_with_an_unusable_identity_is_rejected() {
        let usable = CommitSpecWire::encode(&spec());
        for value in [
            usable.replace(&"a".repeat(40), "not-a-commit"),
            usable.replace(&"b".repeat(40), ""),
            usable.replace("Mineral Publisher", ""),
            usable.replace("Publish Mineral content", ""),
        ] {
            let error = CommitSpecWire::decode(&value).unwrap_err();
            assert!(
                matches!(
                    error,
                    CommitSpecWireError::Malformed | CommitSpecWireError::Invalid(_)
                ),
                "accepted {value:?}"
            );
        }
    }
}
