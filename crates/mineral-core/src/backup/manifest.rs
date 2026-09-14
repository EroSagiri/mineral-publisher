use std::{collections::BTreeSet, error::Error, fmt};

use crate::domain::ContentPath;

use super::delivery::{BackupManifest, BackupManifestEntry};

/// The manifest format version this engine writes and reads.
pub const BACKUP_MANIFEST_FORMAT_VERSION: &str = "v1";

/// Checks that a manifest describes exactly the paths a Snapshot froze.
///
/// The manifest is generated, so a mismatch means the tree and the manifest do not
/// describe one backup: a missing path is an incomplete restore and an unexpected
/// path is content nobody can account for. Both fail closed.
pub fn check_manifest_paths(
    manifest: &BackupManifest,
    snapshot_paths: &[ContentPath],
) -> Result<(), ManifestPathMismatch> {
    let documented = manifest
        .entries()
        .iter()
        .map(|entry| entry.path().clone())
        .collect::<BTreeSet<_>>();
    let expected = snapshot_paths.iter().cloned().collect::<BTreeSet<_>>();

    let missing = expected
        .difference(&documented)
        .cloned()
        .collect::<Vec<_>>();
    let unexpected = documented
        .difference(&expected)
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() && unexpected.is_empty() {
        return Ok(());
    }
    Err(ManifestPathMismatch {
        missing,
        unexpected,
    })
}

/// The manifest and the tree disagree about which paths exist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestPathMismatch {
    missing: Vec<ContentPath>,
    unexpected: Vec<ContentPath>,
}

impl ManifestPathMismatch {
    pub fn missing(&self) -> &[ContentPath] {
        &self.missing
    }

    pub fn unexpected(&self) -> &[ContentPath] {
        &self.unexpected
    }
}

impl fmt::Display for ManifestPathMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "backup manifest describes a different path set ({} missing, {} unexpected)",
            self.missing.len(),
            self.unexpected.len()
        )
    }
}

impl Error for ManifestPathMismatch {}

/// Looks one path up in a parsed manifest.
pub fn manifest_entry<'a>(
    manifest: &'a BackupManifest,
    path: &ContentPath,
) -> Option<&'a BackupManifestEntry> {
    manifest.entries().iter().find(|entry| entry.path() == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_documented_path_set_matches_only_itself() {
        // The check is a set comparison; the parse side is covered by the delivery
        // tests, so this only pins the comparison's two failure directions.
        let manifest = BackupManifest::parse(
            b"mineral-backup-manifest v1\nsnapshot 1\nsource vault\nprojection 0000000000000000000000000000000000000000000000000000000000000000\ngit 1 0000000000000000000000000000000000000000000000000000000000000000 a.md\n",
        )
        .unwrap();

        check_manifest_paths(&manifest, &[ContentPath::new("a.md").unwrap()]).unwrap();

        let missing = check_manifest_paths(
            &manifest,
            &[
                ContentPath::new("a.md").unwrap(),
                ContentPath::new("b.md").unwrap(),
            ],
        )
        .unwrap_err();
        assert_eq!(missing.missing().len(), 1);
        assert!(missing.unexpected().is_empty());

        let replaced =
            check_manifest_paths(&manifest, &[ContentPath::new("b.md").unwrap()]).unwrap_err();
        assert_eq!(replaced.missing().len(), 1);
        assert_eq!(replaced.unexpected().len(), 1);
    }
}
