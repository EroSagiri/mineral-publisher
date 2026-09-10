use std::collections::BTreeMap;

use super::{ContentPath, Snapshot, SnapshotFile, SnapshotId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Change {
    Added(SnapshotFile),
    Modified {
        before: SnapshotFile,
        after: SnapshotFile,
    },
    Deleted(SnapshotFile),
}

impl Change {
    pub fn path(&self) -> &ContentPath {
        match self {
            Self::Added(file) | Self::Deleted(file) => file.path(),
            Self::Modified { after, .. } => after.path(),
        }
    }
}

/// The changes observed between two complete snapshots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSet {
    from_snapshot_id: SnapshotId,
    to_snapshot_id: SnapshotId,
    changes: Vec<Change>,
}

impl ChangeSet {
    pub fn between(before: &Snapshot, after: &Snapshot) -> Self {
        let before_files: BTreeMap<_, _> = before
            .files()
            .iter()
            .map(|file| (file.path(), file))
            .collect();
        let after_files: BTreeMap<_, _> = after
            .files()
            .iter()
            .map(|file| (file.path(), file))
            .collect();

        let mut changes = Vec::new();
        for (path, before_file) in &before_files {
            match after_files.get(path) {
                None => changes.push(Change::Deleted((*before_file).clone())),
                Some(after_file) if before_file != after_file => changes.push(Change::Modified {
                    before: (*before_file).clone(),
                    after: (*after_file).clone(),
                }),
                Some(_) => {}
            }
        }
        for (path, after_file) in &after_files {
            if !before_files.contains_key(path) {
                changes.push(Change::Added((*after_file).clone()));
            }
        }
        changes.sort_by(|left, right| left.path().cmp(right.path()));

        Self {
            from_snapshot_id: before.id(),
            to_snapshot_id: after.id(),
            changes,
        }
    }

    pub fn from_snapshot_id(&self) -> SnapshotId {
        self.from_snapshot_id
    }

    pub fn to_snapshot_id(&self) -> SnapshotId {
        self.to_snapshot_id
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::domain::{Sha256, SourceId};

    fn file(path: &str, marker: u8) -> SnapshotFile {
        SnapshotFile::new(
            ContentPath::new(path).unwrap(),
            marker as u64,
            Sha256::new([marker; 32]),
            Some("text/markdown".into()),
        )
    }

    fn snapshot(id: u64, files: Vec<SnapshotFile>) -> Snapshot {
        Snapshot::new(
            SnapshotId::new(id).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-source").unwrap(),
            files,
        )
        .unwrap()
    }

    #[test]
    fn computes_added_modified_and_deleted_changes_in_path_order() {
        let before = snapshot(1, vec![file("deleted.md", 1), file("modified.md", 1)]);
        let after = snapshot(2, vec![file("added.md", 2), file("modified.md", 2)]);

        let changes = ChangeSet::between(&before, &after);

        assert_eq!(changes.from_snapshot_id(), SnapshotId::new(1).unwrap());
        assert_eq!(changes.to_snapshot_id(), SnapshotId::new(2).unwrap());
        assert!(matches!(changes.changes()[0], Change::Added(_)));
        assert!(matches!(changes.changes()[1], Change::Deleted(_)));
        assert!(matches!(changes.changes()[2], Change::Modified { .. }));
    }

    #[test]
    fn identical_snapshots_produce_no_changes() {
        let before = snapshot(1, vec![file("note.md", 1)]);
        let after = snapshot(2, vec![file("note.md", 1)]);

        assert!(ChangeSet::between(&before, &after).is_empty());
    }
}
