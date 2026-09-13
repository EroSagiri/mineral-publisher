use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha256Hasher};

use super::ContentPath;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotId(u64);

impl SnapshotId {
    pub fn new(value: u64) -> Result<Self, SnapshotError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(SnapshotError::InvalidSnapshotId)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(String);

impl SourceId {
    pub fn new(value: impl Into<String>) -> Result<Self, SnapshotError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SnapshotError::InvalidSourceId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn digest(content: &[u8]) -> Self {
        Self(Sha256Hasher::digest(content).into())
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotFile {
    path: ContentPath,
    size: u64,
    sha256: Sha256,
    content_type: Option<String>,
    source_metadata: BTreeMap<String, String>,
}

impl SnapshotFile {
    pub fn new(path: ContentPath, size: u64, sha256: Sha256, content_type: Option<String>) -> Self {
        Self {
            path,
            size,
            sha256,
            content_type,
            source_metadata: BTreeMap::new(),
        }
    }

    pub fn with_source_metadata(mut self, source_metadata: BTreeMap<String, String>) -> Self {
        self.source_metadata = source_metadata;
        self
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn sha256(&self) -> Sha256 {
        self.sha256
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub fn source_metadata(&self) -> &BTreeMap<String, String> {
        &self.source_metadata
    }
}

/// An immutable view of the complete source state at one point in time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    id: SnapshotId,
    created_at: SystemTime,
    source_id: SourceId,
    files: Vec<SnapshotFile>,
}

impl Snapshot {
    pub fn new(
        id: SnapshotId,
        created_at: SystemTime,
        source_id: SourceId,
        mut files: Vec<SnapshotFile>,
    ) -> Result<Self, SnapshotError> {
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let mut paths = BTreeSet::new();
        if let Some(duplicate) = files.iter().find(|file| !paths.insert(file.path.clone())) {
            return Err(SnapshotError::DuplicatePath(duplicate.path.clone()));
        }

        Ok(Self {
            id,
            created_at,
            source_id,
            files,
        })
    }

    pub fn id(&self) -> SnapshotId {
        self.id
    }

    pub fn created_at(&self) -> SystemTime {
        self.created_at
    }

    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    pub fn files(&self) -> &[SnapshotFile] {
        &self.files
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotError {
    InvalidSnapshotId,
    InvalidSourceId,
    DuplicatePath(ContentPath),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSnapshotId => formatter.write_str("snapshot id must be non-zero"),
            Self::InvalidSourceId => formatter.write_str("source id cannot be empty"),
            Self::DuplicatePath(path) => write!(formatter, "duplicate snapshot path: {path}"),
        }
    }
}

impl Error for SnapshotError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> SnapshotFile {
        SnapshotFile::new(
            ContentPath::new(path).unwrap(),
            1,
            Sha256::new([1; 32]),
            None,
        )
    }

    #[test]
    fn snapshot_has_stable_file_order_and_read_only_access() {
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("local-vault").unwrap(),
            vec![file("z.md"), file("a.md")],
        )
        .unwrap();

        let paths: Vec<_> = snapshot
            .files()
            .iter()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(paths, ["a.md", "z.md"]);
    }

    #[test]
    fn snapshot_rejects_duplicate_paths() {
        let result = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("local-vault").unwrap(),
            vec![file("note.md"), file("note.md")],
        );

        assert_eq!(
            result.unwrap_err(),
            SnapshotError::DuplicatePath(ContentPath::new("note.md").unwrap())
        );
    }
}
