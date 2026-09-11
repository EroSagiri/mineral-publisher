use std::{collections::BTreeMap, error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::domain::{Sha256, SnapshotId};

use super::{ManagedRoot, ProjectionTargetPath, PublicProjection, PublicationFileMode};

/// One observed file in the complete managed target subtree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentTargetEntry {
    target_path: ProjectionTargetPath,
    blob_sha256: Sha256,
    file_mode: PublicationFileMode,
}

impl CurrentTargetEntry {
    pub fn new(target_path: ProjectionTargetPath, blob_sha256: Sha256) -> Self {
        Self::with_mode(target_path, blob_sha256, PublicationFileMode::Regular)
    }

    pub fn with_mode(
        target_path: ProjectionTargetPath,
        blob_sha256: Sha256,
        file_mode: PublicationFileMode,
    ) -> Self {
        Self {
            target_path,
            blob_sha256,
            file_mode,
        }
    }

    pub fn target_path(&self) -> &ProjectionTargetPath {
        &self.target_path
    }

    /// SHA-256 of the exact bytes currently present at this target path.
    pub fn blob_sha256(&self) -> Sha256 {
        self.blob_sha256
    }

    pub fn file_mode(&self) -> PublicationFileMode {
        self.file_mode
    }
}

/// A publisher-neutral, complete observed state for one managed target subtree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentTargetState {
    managed_root: ManagedRoot,
    entries: Vec<CurrentTargetEntry>,
}

impl CurrentTargetState {
    pub fn new(
        managed_root: ManagedRoot,
        entries: Vec<CurrentTargetEntry>,
    ) -> Result<Self, CurrentTargetStateError> {
        let mut ordered_entries = BTreeMap::new();
        for entry in entries {
            if !is_within_managed_root(&managed_root, entry.target_path()) {
                return Err(CurrentTargetStateError::PathOutsideManagedRoot {
                    managed_root,
                    target_path: entry.target_path,
                });
            }
            if let Some(existing) = ordered_entries.insert(entry.target_path.clone(), entry) {
                return Err(CurrentTargetStateError::DuplicateTargetPath {
                    target_path: existing.target_path,
                });
            }
        }

        Ok(Self {
            managed_root,
            entries: ordered_entries.into_values().collect(),
        })
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }

    pub fn entries(&self) -> &[CurrentTargetEntry] {
        &self.entries
    }
}

/// One logical target-state change. Execution ordering is deliberately left to a Publisher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishOperation {
    Added {
        target_path: ProjectionTargetPath,
        desired_sha256: Sha256,
        desired_mode: PublicationFileMode,
    },
    Modified {
        target_path: ProjectionTargetPath,
        previous_sha256: Sha256,
        desired_sha256: Sha256,
        previous_mode: PublicationFileMode,
        desired_mode: PublicationFileMode,
    },
    Deleted {
        target_path: ProjectionTargetPath,
        previous_sha256: Sha256,
        previous_mode: PublicationFileMode,
    },
}

impl PublishOperation {
    pub fn target_path(&self) -> &ProjectionTargetPath {
        match self {
            Self::Added { target_path, .. }
            | Self::Modified { target_path, .. }
            | Self::Deleted { target_path, .. } => target_path,
        }
    }
}

/// The deterministic logical diff from an observed target state to one complete projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishPlan {
    snapshot_id: SnapshotId,
    projection_sha256: Sha256,
    managed_root: ManagedRoot,
    operations: Vec<PublishOperation>,
    plan_sha256: Sha256,
}

impl PublishPlan {
    /// Builds a pure identity-space diff without reading target files or projection blobs.
    pub fn build(
        current: &CurrentTargetState,
        projection: &PublicProjection,
    ) -> Result<Self, PublishPlanError> {
        if current.managed_root() != projection.managed_root() {
            return Err(PublishPlanError::ManagedRootMismatch {
                current_managed_root: current.managed_root().clone(),
                projection_managed_root: projection.managed_root().clone(),
            });
        }

        let current_entries = current
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.target_path(),
                    (entry.blob_sha256(), entry.file_mode()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let desired_entries = projection
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.target_path(),
                    (entry.blob_sha256(), entry.file_mode()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut operations = Vec::new();

        for (path, (desired_sha256, desired_mode)) in &desired_entries {
            match current_entries.get(path) {
                None => operations.push(PublishOperation::Added {
                    target_path: (*path).clone(),
                    desired_sha256: *desired_sha256,
                    desired_mode: *desired_mode,
                }),
                Some((previous_sha256, previous_mode))
                    if previous_sha256 != desired_sha256 || previous_mode != desired_mode =>
                {
                    operations.push(PublishOperation::Modified {
                        target_path: (*path).clone(),
                        previous_sha256: *previous_sha256,
                        desired_sha256: *desired_sha256,
                        previous_mode: *previous_mode,
                        desired_mode: *desired_mode,
                    });
                }
                Some(_) => {}
            }
        }
        for (path, (previous_sha256, previous_mode)) in &current_entries {
            if !desired_entries.contains_key(path) {
                operations.push(PublishOperation::Deleted {
                    target_path: (*path).clone(),
                    previous_sha256: *previous_sha256,
                    previous_mode: *previous_mode,
                });
            }
        }
        operations.sort_by(|left, right| left.target_path().cmp(right.target_path()));

        let plan_sha256 = plan_identity(projection.projection_sha256(), &operations);
        Ok(Self {
            snapshot_id: projection.snapshot_id(),
            projection_sha256: projection.projection_sha256(),
            managed_root: projection.managed_root().clone(),
            operations,
            plan_sha256,
        })
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }

    pub fn operations(&self) -> &[PublishOperation] {
        &self.operations
    }

    pub fn plan_sha256(&self) -> Sha256 {
        self.plan_sha256
    }
}

fn is_within_managed_root(root: &ManagedRoot, path: &ProjectionTargetPath) -> bool {
    path.as_str()
        .strip_prefix(root.as_str())
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn plan_identity(projection_sha256: Sha256, operations: &[PublishOperation]) -> Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(b"mineral-publisher-publish-plan-v2\0");
    hasher.update(projection_sha256.as_bytes());
    for operation in operations {
        match operation {
            PublishOperation::Added {
                target_path,
                desired_sha256,
                desired_mode,
            } => {
                hasher.update(b"A");
                hash_path(&mut hasher, target_path);
                hasher.update(desired_sha256.as_bytes());
                hash_mode(&mut hasher, *desired_mode);
            }
            PublishOperation::Modified {
                target_path,
                previous_sha256,
                desired_sha256,
                previous_mode,
                desired_mode,
            } => {
                hasher.update(b"M");
                hash_path(&mut hasher, target_path);
                hasher.update(previous_sha256.as_bytes());
                hasher.update(desired_sha256.as_bytes());
                hash_mode(&mut hasher, *previous_mode);
                hash_mode(&mut hasher, *desired_mode);
            }
            PublishOperation::Deleted {
                target_path,
                previous_sha256,
                previous_mode,
            } => {
                hasher.update(b"D");
                hash_path(&mut hasher, target_path);
                hasher.update(previous_sha256.as_bytes());
                hash_mode(&mut hasher, *previous_mode);
            }
        }
    }
    Sha256::new(hasher.finalize().into())
}

fn hash_mode(hasher: &mut Sha256Hasher, mode: PublicationFileMode) {
    hasher.update(match mode {
        PublicationFileMode::Regular => b"regular".as_slice(),
        PublicationFileMode::Executable => b"executable".as_slice(),
    });
}

fn hash_path(hasher: &mut Sha256Hasher, path: &ProjectionTargetPath) {
    let bytes = path.as_str().as_bytes();
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CurrentTargetStateError {
    PathOutsideManagedRoot {
        managed_root: ManagedRoot,
        target_path: ProjectionTargetPath,
    },
    DuplicateTargetPath {
        target_path: ProjectionTargetPath,
    },
}

impl fmt::Display for CurrentTargetStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathOutsideManagedRoot { target_path, .. } => {
                write!(
                    formatter,
                    "target path is outside the managed root: {target_path}"
                )
            }
            Self::DuplicateTargetPath { target_path } => {
                write!(formatter, "duplicate current target path: {target_path}")
            }
        }
    }
}

impl Error for CurrentTargetStateError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishPlanError {
    ManagedRootMismatch {
        current_managed_root: ManagedRoot,
        projection_managed_root: ManagedRoot,
    },
}

impl fmt::Display for PublishPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManagedRootMismatch { .. } => formatter
                .write_str("current target state and projection must have the same managed root"),
        }
    }
}

impl Error for PublishPlanError {}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{
        domain::{ContentPath, Snapshot, SnapshotFile, SourceId},
        workflow::FinalPublicationSet,
    };

    use super::*;

    fn sha(value: u8) -> Sha256 {
        Sha256::new([value; 32])
    }

    fn root() -> ManagedRoot {
        ManagedRoot::new("content").unwrap()
    }

    fn target(path: &str) -> ProjectionTargetPath {
        ProjectionTargetPath::new(path).unwrap()
    }

    fn current(entries: &[(&str, Sha256)]) -> CurrentTargetState {
        CurrentTargetState::new(
            root(),
            entries
                .iter()
                .map(|(path, identity)| CurrentTargetEntry::new(target(path), *identity))
                .collect(),
        )
        .unwrap()
    }

    fn projection(entries: &[(&str, Sha256)]) -> PublicProjection {
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            entries
                .iter()
                .map(|(path, identity)| {
                    SnapshotFile::new(ContentPath::new(*path).unwrap(), 1, *identity, None)
                })
                .collect(),
        )
        .unwrap();
        let publication_set = FinalPublicationSet::from_parts_for_test(
            SnapshotId::new(1).unwrap(),
            entries
                .iter()
                .map(|(path, _)| ContentPath::new(*path).unwrap())
                .collect(),
            vec![],
        );
        PublicProjection::build(&publication_set, &snapshot, root()).unwrap()
    }

    #[test]
    fn adds_every_desired_entry_missing_from_current_state() {
        let plan = PublishPlan::build(&current(&[]), &projection(&[("a.md", sha(1))])).unwrap();

        assert_eq!(
            plan.operations(),
            &[PublishOperation::Added {
                target_path: target("content/a.md"),
                desired_sha256: sha(1),
                desired_mode: PublicationFileMode::Regular,
            }]
        );
    }

    #[test]
    fn preserves_a_modification_as_one_operation() {
        let plan = PublishPlan::build(
            &current(&[("content/a.md", sha(1))]),
            &projection(&[("a.md", sha(2))]),
        )
        .unwrap();

        assert_eq!(
            plan.operations(),
            &[PublishOperation::Modified {
                target_path: target("content/a.md"),
                previous_sha256: sha(1),
                desired_sha256: sha(2),
                previous_mode: PublicationFileMode::Regular,
                desired_mode: PublicationFileMode::Regular,
            }]
        );
    }

    #[test]
    fn deletes_entries_absent_from_the_complete_desired_state() {
        let plan =
            PublishPlan::build(&current(&[("content/a.md", sha(1))]), &projection(&[])).unwrap();

        assert_eq!(
            plan.operations(),
            &[PublishOperation::Deleted {
                target_path: target("content/a.md"),
                previous_sha256: sha(1),
                previous_mode: PublicationFileMode::Regular,
            }]
        );
    }

    #[test]
    fn unchanged_entries_do_not_produce_operations() {
        let plan = PublishPlan::build(
            &current(&[("content/a.md", sha(1))]),
            &projection(&[("a.md", sha(1))]),
        )
        .unwrap();

        assert!(plan.operations().is_empty());
    }

    #[test]
    fn executable_current_file_is_modified_to_the_v1_regular_mode() {
        let current = CurrentTargetState::new(
            root(),
            vec![CurrentTargetEntry::with_mode(
                target("content/a.md"),
                sha(1),
                PublicationFileMode::Executable,
            )],
        )
        .unwrap();

        let plan = PublishPlan::build(&current, &projection(&[("a.md", sha(1))])).unwrap();

        assert!(matches!(
            plan.operations(),
            [PublishOperation::Modified {
                previous_mode: PublicationFileMode::Executable,
                desired_mode: PublicationFileMode::Regular,
                ..
            }]
        ));
    }

    #[test]
    fn mixed_diff_is_path_ordered_and_excludes_unchanged_entries() {
        let plan = PublishPlan::build(
            &current(&[
                ("content/a.md", sha(1)),
                ("content/b.md", sha(2)),
                ("content/old.md", sha(3)),
                ("content/same.md", sha(4)),
            ]),
            &projection(&[
                ("a.md", sha(5)),
                ("b.md", sha(2)),
                ("new.md", sha(6)),
                ("same.md", sha(4)),
            ]),
        )
        .unwrap();

        assert_eq!(
            plan.operations(),
            &[
                PublishOperation::Modified {
                    target_path: target("content/a.md"),
                    previous_sha256: sha(1),
                    desired_sha256: sha(5),
                    previous_mode: PublicationFileMode::Regular,
                    desired_mode: PublicationFileMode::Regular,
                },
                PublishOperation::Added {
                    target_path: target("content/new.md"),
                    desired_sha256: sha(6),
                    desired_mode: PublicationFileMode::Regular,
                },
                PublishOperation::Deleted {
                    target_path: target("content/old.md"),
                    previous_sha256: sha(3),
                    previous_mode: PublicationFileMode::Regular,
                },
            ]
        );
    }

    #[test]
    fn rename_like_changes_remain_a_delete_and_an_add() {
        let plan = PublishPlan::build(
            &current(&[("content/old.md", sha(1))]),
            &projection(&[("new.md", sha(1))]),
        )
        .unwrap();

        assert!(matches!(
            plan.operations()[0],
            PublishOperation::Added { .. }
        ));
        assert!(matches!(
            plan.operations()[1],
            PublishOperation::Deleted { .. }
        ));
    }

    #[test]
    fn both_empty_states_produce_a_valid_noop_plan() {
        let plan = PublishPlan::build(&current(&[]), &projection(&[])).unwrap();

        assert!(plan.operations().is_empty());
    }

    #[test]
    fn current_state_rejects_paths_outside_its_managed_root() {
        let error = CurrentTargetState::new(
            root(),
            vec![CurrentTargetEntry::new(target("Cargo.toml"), sha(1))],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CurrentTargetStateError::PathOutsideManagedRoot { .. }
        ));
    }

    #[test]
    fn current_state_rejects_duplicate_target_paths() {
        let error = CurrentTargetState::new(
            root(),
            vec![
                CurrentTargetEntry::new(target("content/a.md"), sha(1)),
                CurrentTargetEntry::new(target("content/a.md"), sha(2)),
            ],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CurrentTargetStateError::DuplicateTargetPath { .. }
        ));
    }

    #[test]
    fn managed_root_mismatch_is_rejected() {
        let state = CurrentTargetState::new(ManagedRoot::new("public").unwrap(), vec![]).unwrap();

        assert!(matches!(
            PublishPlan::build(&state, &projection(&[])),
            Err(PublishPlanError::ManagedRootMismatch { .. })
        ));
    }

    #[test]
    fn input_order_does_not_change_operations_or_plan_identity() {
        let projection = projection(&[("a.md", sha(1)), ("b.md", sha(2))]);
        let first = CurrentTargetState::new(
            root(),
            vec![
                CurrentTargetEntry::new(target("content/z.md"), sha(3)),
                CurrentTargetEntry::new(target("content/a.md"), sha(4)),
            ],
        )
        .unwrap();
        let second = CurrentTargetState::new(
            root(),
            vec![
                CurrentTargetEntry::new(target("content/a.md"), sha(4)),
                CurrentTargetEntry::new(target("content/z.md"), sha(3)),
            ],
        )
        .unwrap();

        let first = PublishPlan::build(&first, &projection).unwrap();
        let second = PublishPlan::build(&second, &projection).unwrap();

        assert_eq!(first.operations(), second.operations());
        assert_eq!(first.plan_sha256(), second.plan_sha256());
    }

    #[test]
    fn plan_binds_the_projection_and_snapshot_identity() {
        let projection = projection(&[("a.md", sha(1))]);
        let plan = PublishPlan::build(&current(&[]), &projection).unwrap();

        assert_eq!(plan.projection_sha256(), projection.projection_sha256());
        assert_eq!(plan.snapshot_id(), projection.snapshot_id());
    }
}
