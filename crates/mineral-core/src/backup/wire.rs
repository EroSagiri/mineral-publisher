use std::{error::Error, fmt};

use crate::domain::{ContentPath, Sha256, SnapshotId, SourceId};

use super::{
    delivery::{BackupDeliveryFile, BackupDeliveryProjection, BackupRepresentation},
    lfs::RequiredLfsObject,
};

/// Version of the durable backup-delivery encoding.
pub const BACKUP_DELIVERY_WIRE_VERSION: u8 = 1;

/// Deterministic text encoding of one backup delivery, for the durable intent.
///
/// A resume must rebuild exactly the tree and commit the original run froze, so the
/// delivery is stored in full rather than re-derived from configuration. The
/// encoding carries no credential and no endpoint: it is a description of bytes, and
/// it never leaves the local state store.
pub struct BackupDeliveryWire;

impl BackupDeliveryWire {
    pub fn encode(delivery: &BackupDeliveryProjection) -> String {
        let mut text = String::new();
        text.push_str(&format!(
            "mineral-backup-delivery v{BACKUP_DELIVERY_WIRE_VERSION}\n"
        ));
        text.push_str(&format!("snapshot {}\n", delivery.snapshot_id().get()));
        text.push_str(&format!("source {}\n", delivery.source_id().as_str()));
        text.push_str(&format!(
            "projection {}\n",
            delivery.source_projection_sha256()
        ));
        text.push_str(&format!(
            "attributes {}\n",
            delivery.git_attributes_blob_sha256()
        ));
        text.push_str(&format!("manifest {}\n", delivery.manifest_blob_sha256()));
        for file in delivery.files() {
            match file.representation() {
                BackupRepresentation::GitBlob { blob_sha256, size } => {
                    text.push_str(&format!(
                        "git {blob_sha256} {size} {}\n",
                        file.path().as_str()
                    ));
                }
                BackupRepresentation::GitLfs {
                    source_sha256,
                    size,
                    pointer_blob_sha256,
                } => {
                    text.push_str(&format!(
                        "lfs {source_sha256} {size} {pointer_blob_sha256} {}\n",
                        file.path().as_str()
                    ));
                }
            }
        }
        for object in delivery.required_lfs_objects() {
            text.push_str(&format!("object {} {}\n", object.oid(), object.size()));
        }
        text
    }

    pub fn decode(encoded: &str) -> Result<BackupDeliveryProjection, BackupDeliveryWireError> {
        let mut lines = encoded.split('\n');
        if lines.next()
            != Some(&format!(
                "mineral-backup-delivery v{BACKUP_DELIVERY_WIRE_VERSION}"
            ))
        {
            return Err(BackupDeliveryWireError::UnknownVersion);
        }
        let snapshot_id = parse_field(lines.next(), "snapshot")?
            .parse::<u64>()
            .ok()
            .and_then(|value| SnapshotId::new(value).ok())
            .ok_or(BackupDeliveryWireError::Damaged)?;
        let source_id = SourceId::new(parse_field(lines.next(), "source")?)
            .map_err(|_| BackupDeliveryWireError::Damaged)?;
        let source_projection_sha256 = parse_sha256(parse_field(lines.next(), "projection")?)?;
        let git_attributes_blob_sha256 = parse_sha256(parse_field(lines.next(), "attributes")?)?;
        let manifest_blob_sha256 = parse_sha256(parse_field(lines.next(), "manifest")?)?;

        let mut files = Vec::new();
        let mut objects = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.splitn(5, ' ');
            match fields.next() {
                Some("git") => {
                    let blob_sha256 = parse_sha256(next(&mut fields)?)?;
                    let size = parse_size(next(&mut fields)?)?;
                    let path = parse_path(next(&mut fields)?)?;
                    files.push(BackupDeliveryFile::from_parts(
                        path,
                        BackupRepresentation::GitBlob { blob_sha256, size },
                    ));
                }
                Some("lfs") => {
                    let source_sha256 = parse_sha256(next(&mut fields)?)?;
                    let size = parse_size(next(&mut fields)?)?;
                    let pointer_blob_sha256 = parse_sha256(next(&mut fields)?)?;
                    let path = parse_path(next(&mut fields)?)?;
                    files.push(BackupDeliveryFile::from_parts(
                        path,
                        BackupRepresentation::GitLfs {
                            source_sha256,
                            size,
                            pointer_blob_sha256,
                        },
                    ));
                }
                Some("object") => {
                    let oid = parse_sha256(next(&mut fields)?)?;
                    let size = parse_size(next(&mut fields)?)?;
                    objects.push(RequiredLfsObject::new(oid, size));
                }
                _ => return Err(BackupDeliveryWireError::Damaged),
            }
        }

        BackupDeliveryProjection::from_parts(
            snapshot_id,
            source_id,
            source_projection_sha256,
            files,
            objects,
            manifest_blob_sha256,
            git_attributes_blob_sha256,
        )
        .map_err(BackupDeliveryWireError::Delivery)
    }
}

fn next<'a>(fields: &mut std::str::SplitN<'a, char>) -> Result<&'a str, BackupDeliveryWireError> {
    fields.next().ok_or(BackupDeliveryWireError::Damaged)
}

fn parse_field<'a>(line: Option<&'a str>, name: &str) -> Result<&'a str, BackupDeliveryWireError> {
    line.and_then(|line| line.strip_prefix(&format!("{name} ")))
        .filter(|value| !value.is_empty())
        .ok_or(BackupDeliveryWireError::Damaged)
}

fn parse_sha256(value: &str) -> Result<Sha256, BackupDeliveryWireError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BackupDeliveryWireError::Damaged);
    }
    let mut digest = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(chunk).map_err(|_| BackupDeliveryWireError::Damaged)?;
        digest[index] =
            u8::from_str_radix(text, 16).map_err(|_| BackupDeliveryWireError::Damaged)?;
    }
    Ok(Sha256::new(digest))
}

fn parse_size(value: &str) -> Result<u64, BackupDeliveryWireError> {
    value
        .parse::<u64>()
        .map_err(|_| BackupDeliveryWireError::Damaged)
}

fn parse_path(value: &str) -> Result<ContentPath, BackupDeliveryWireError> {
    ContentPath::new(value).map_err(|_| BackupDeliveryWireError::Damaged)
}

/// Why a stored delivery cannot be read back.
#[derive(Debug)]
pub enum BackupDeliveryWireError {
    Damaged,
    UnknownVersion,
    Delivery(super::delivery::BackupDeliveryError),
}

impl fmt::Display for BackupDeliveryWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Damaged => formatter.write_str("stored backup delivery is damaged"),
            Self::UnknownVersion => {
                formatter.write_str("stored backup delivery uses an unknown version")
            }
            Self::Delivery(error) => {
                write!(formatter, "stored backup delivery is unusable: {error}")
            }
        }
    }
}

impl Error for BackupDeliveryWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Delivery(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, time::SystemTime};

    use super::*;
    use crate::{
        backup::{
            delivery::build_backup_delivery,
            projection::{BackupProjection, TypeFirstBackupRepresentationPolicy},
        },
        domain::{Snapshot, SnapshotFile},
        ports::{BlobStore, ContentStoreError},
    };

    #[derive(Default)]
    struct MemoryStore(RefCell<HashMap<Sha256, Vec<u8>>>);

    impl BlobStore for MemoryStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.0
                .borrow()
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            let identity = Sha256::digest(content);
            self.0.borrow_mut().insert(identity, content.to_vec());
            Ok(identity)
        }
    }

    fn delivery() -> BackupDeliveryProjection {
        let store = MemoryStore::default();
        let files = vec![
            SnapshotFile::new(
                ContentPath::new("notes/a.md").unwrap(),
                5,
                Sha256::digest(b"note\n"),
                None,
            ),
            SnapshotFile::new(
                ContentPath::new("img/photo.jpg").unwrap(),
                8,
                Sha256::digest(b"original"),
                None,
            ),
            SnapshotFile::new(
                ContentPath::new("img/copy.jpg").unwrap(),
                8,
                Sha256::digest(b"original"),
                None,
            ),
        ];
        let snapshot = Snapshot::new(
            SnapshotId::new(9).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("bedrock").unwrap(),
            files,
        )
        .unwrap();
        let projection = BackupProjection::build(&snapshot).unwrap();
        build_backup_delivery(&projection, &TypeFirstBackupRepresentationPolicy, &store).unwrap()
    }

    #[test]
    fn a_delivery_round_trips_through_its_durable_form() {
        let delivery = delivery();

        let encoded = BackupDeliveryWire::encode(&delivery);
        let decoded = BackupDeliveryWire::decode(&encoded).unwrap();

        assert_eq!(decoded, delivery);
        assert_eq!(decoded.delivery_sha256(), delivery.delivery_sha256());
        assert_eq!(
            decoded.required_lfs_objects(),
            delivery.required_lfs_objects()
        );
        assert_eq!(
            decoded.required_lfs_objects().len(),
            1,
            "two paths with the same bytes require one object"
        );
    }

    #[test]
    fn a_stored_delivery_from_another_version_or_a_damaged_one_fails_closed() {
        let encoded = BackupDeliveryWire::encode(&delivery());

        assert!(matches!(
            BackupDeliveryWire::decode(&encoded.replace("v1", "v2")),
            Err(BackupDeliveryWireError::UnknownVersion)
        ));
        for damaged in [
            encoded.replace("snapshot 9", "snapshot 0"),
            encoded.replace("lfs", "ternary"),
            encoded.replace("object ", "objects "),
            encoded.split("\n").take(3).collect::<Vec<_>>().join("\n"),
        ] {
            assert!(
                BackupDeliveryWire::decode(&damaged).is_err(),
                "{damaged} was accepted"
            );
        }
    }
}
