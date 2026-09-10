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
                // A ChangeSet captures content changes. SnapshotFile metadata such as size or
                // content type is descriptive; the SHA-256 is the content identity.
                Some(after_file) if before_file.sha256() != after_file.sha256() => {
                    changes.push(Change::Modified {
                        before: (*before_file).clone(),
                        after: (*after_file).clone(),
                    })
                }
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

    fn file(path: &str, size: u64, hash_marker: u8) -> SnapshotFile {
        SnapshotFile::new(
            ContentPath::new(path).unwrap(),
            size,
            Sha256::new([hash_marker; 32]),
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
    fn empty_snapshots_produce_no_changes() {
        let before = snapshot(1, vec![]);
        let after = snapshot(2, vec![]);

        let changes = ChangeSet::between(&before, &after);

        assert!(changes.is_empty());
        assert_eq!(changes.from_snapshot_id(), SnapshotId::new(1).unwrap());
        assert_eq!(changes.to_snapshot_id(), SnapshotId::new(2).unwrap());
    }

    #[test]
    fn identical_snapshots_produce_no_changes() {
        let before = snapshot(1, vec![file("note.md", 10, 1)]);
        let after = snapshot(2, vec![file("note.md", 10, 1)]);

        assert!(ChangeSet::between(&before, &after).is_empty());
    }

    #[test]
    fn detects_a_single_added_file() {
        let changes = ChangeSet::between(
            &snapshot(1, vec![]),
            &snapshot(2, vec![file("note.md", 10, 1)]),
        );

        assert_eq!(changes.changes(), &[Change::Added(file("note.md", 10, 1))]);
    }

    #[test]
    fn detects_a_single_deleted_file() {
        let changes = ChangeSet::between(
            &snapshot(1, vec![file("note.md", 10, 1)]),
            &snapshot(2, vec![]),
        );

        assert_eq!(
            changes.changes(),
            &[Change::Deleted(file("note.md", 10, 1))]
        );
    }

    #[test]
    fn detects_a_single_modified_file_when_its_hash_changes() {
        let before_file = file("note.md", 10, 1);
        let after_file = file("note.md", 20, 2);
        let changes = ChangeSet::between(
            &snapshot(1, vec![before_file.clone()]),
            &snapshot(2, vec![after_file.clone()]),
        );

        assert_eq!(
            changes.changes(),
            &[Change::Modified {
                before: before_file,
                after: after_file,
            }]
        );
    }

    #[test]
    fn computes_added_modified_and_deleted_changes_in_path_order() {
        let before = snapshot(1, vec![file("deleted.md", 1, 1), file("modified.md", 1, 1)]);
        let after = snapshot(2, vec![file("added.md", 2, 2), file("modified.md", 2, 2)]);

        let changes = ChangeSet::between(&before, &after);

        assert_eq!(changes.from_snapshot_id(), SnapshotId::new(1).unwrap());
        assert_eq!(changes.to_snapshot_id(), SnapshotId::new(2).unwrap());
        assert!(matches!(changes.changes()[0], Change::Added(_)));
        assert!(matches!(changes.changes()[1], Change::Deleted(_)));
        assert!(matches!(changes.changes()[2], Change::Modified { .. }));
    }

    #[test]
    fn ignores_size_changes_when_the_hash_is_unchanged() {
        let before = snapshot(1, vec![file("note.md", 10, 1)]);
        let after = snapshot(2, vec![file("note.md", 20, 1)]);

        assert!(ChangeSet::between(&before, &after).is_empty());
    }

    #[test]
    fn input_file_order_does_not_change_the_result() {
        let before = snapshot(
            1,
            vec![
                file("z.md", 1, 1),
                file("unchanged.md", 1, 1),
                file("a.md", 1, 1),
            ],
        );
        let after = snapshot(
            2,
            vec![
                file("unchanged.md", 1, 1),
                file("new.md", 1, 2),
                file("z.md", 2, 2),
            ],
        );
        let before_reordered = snapshot(
            1,
            vec![
                file("a.md", 1, 1),
                file("z.md", 1, 1),
                file("unchanged.md", 1, 1),
            ],
        );
        let after_reordered = snapshot(
            2,
            vec![
                file("z.md", 2, 2),
                file("unchanged.md", 1, 1),
                file("new.md", 1, 2),
            ],
        );

        assert_eq!(
            ChangeSet::between(&before, &after),
            ChangeSet::between(&before_reordered, &after_reordered)
        );
    }
}
