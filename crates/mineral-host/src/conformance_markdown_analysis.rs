//! Adapter conformance tests for the portable Snapshot Markdown analyzer.
//!
//! These tests exercise engine behaviour against the real native adapters
//! (filesystem CAS and local source), so they live in the host crate.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::SystemTime,
};

use crate::{source::LocalSource, storage::LocalContentStore};
use mineral_core::content::{
    ReferenceKind, Resolution, SnapshotMarkdownAnalysisError, SnapshotMarkdownAnalyzer,
};
use mineral_core::domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId, SourceId};
use mineral_core::policy::{PrivacyClassification, PrivateReason};
use mineral_core::ports::ContentStoreError;

mod tests {
    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-snapshot-markdown-analysis-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("source")).unwrap();
            Self(path)
        }

        fn source_path(&self) -> PathBuf {
            self.0.join("source")
        }

        fn store(&self) -> LocalContentStore {
            LocalContentStore::new(self.0.join("content-store"))
        }

        fn write(&self, relative_path: &str, content: &[u8]) {
            let path = self.source_path().join(relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn remove(&self, relative_path: &str) {
            fs::remove_file(self.source_path().join(relative_path)).unwrap();
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot(directory: &TestDirectory, store: LocalContentStore) -> Snapshot {
        LocalSource::new(
            directory.source_path(),
            SourceId::new("test-source").unwrap(),
            store,
        )
        .snapshot(SnapshotId::new(1).unwrap(), SystemTime::UNIX_EPOCH)
        .unwrap()
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    #[test]
    fn parses_and_resolves_references_from_snapshot_markdown() {
        let directory = TestDirectory::new();
        directory.write(
            "article.md",
            b"[[note]]\n![[image.png]]\n[report](attachments/report.pdf)\nhttps://example.com",
        );
        directory.write("note.md", b"note");
        directory.write("image.png", b"image");
        directory.write("attachments/report.pdf", b"pdf");
        let store = directory.store();
        let snapshot = snapshot(&directory, store.clone());

        let analyzed = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("article.md"))
            .unwrap();

        assert_eq!(analyzed.path().as_str(), "article.md");
        assert_eq!(analyzed.references().len(), 4);
        assert_eq!(
            analyzed.references()[0].reference().kind(),
            ReferenceKind::WikiLink
        );
        assert_eq!(analyzed.references()[0].reference().target(), "note");
        assert_eq!(analyzed.references()[0].reference().span(), 0..8);
        assert_eq!(
            analyzed.references()[0].resolution(),
            &Resolution::ResolvedNote {
                path: path("note.md")
            }
        );
        assert_eq!(
            analyzed.references()[1].resolution(),
            &Resolution::ResolvedAsset {
                path: path("image.png")
            }
        );
        assert_eq!(
            analyzed.references()[2].resolution(),
            &Resolution::ResolvedAsset {
                path: path("attachments/report.pdf")
            }
        );
        assert_eq!(
            analyzed.references()[3].resolution(),
            &Resolution::External {
                target: "https://example.com".to_owned()
            }
        );
    }

    #[test]
    fn analysis_uses_snapshot_content_after_source_changes() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"![[a.png]]");
        directory.write("a.png", b"a");
        let store = directory.store();
        let snapshot = snapshot(&directory, store.clone());

        directory.write("note.md", b"![[b.png]]");
        directory.write("b.png", b"b");

        let analyzed = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("note.md"))
            .unwrap();

        assert_eq!(analyzed.references()[0].reference().target(), "a.png");
        assert_eq!(
            analyzed.references()[0].resolution(),
            &Resolution::ResolvedAsset {
                path: path("a.png")
            }
        );
    }

    #[test]
    fn privacy_uses_private_snapshot_content_after_source_becomes_public() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"---\nprivate: true\n---\nold body");
        let store = directory.store();
        let snapshot = snapshot(&directory, store.clone());

        directory.write("note.md", b"---\nprivate: false\n---\nnew body");

        let analyzed = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("note.md"))
            .unwrap();

        assert!(matches!(
            analyzed.privacy(),
            PrivacyClassification::Private { reasons }
                if reasons == &[PrivateReason::FrontmatterPrivate]
        ));
    }

    #[test]
    fn privacy_uses_public_snapshot_content_after_source_becomes_private() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"---\nprivate: false\n---\nsnapshot body");
        let store = directory.store();
        let snapshot = snapshot(&directory, store.clone());

        directory.write("note.md", b"---\nprivate: true\n---\nnew body");

        let analyzed = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("note.md"))
            .unwrap();

        assert_eq!(analyzed.privacy(), &PrivacyClassification::PublicCandidate);
    }

    #[test]
    fn privacy_classification_uses_the_complete_snapshot_content_path() {
        let directory = TestDirectory::new();
        directory.write("notes/PRIVATE/note.md", b"ordinary body");
        let store = directory.store();
        let snapshot = snapshot(&directory, store.clone());

        let analyzed = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("notes/PRIVATE/note.md"))
            .unwrap();

        assert!(matches!(
            analyzed.privacy(),
            PrivacyClassification::Private { reasons }
                if reasons == &[PrivateReason::PathContainsPrivateMarker]
        ));
    }

    #[test]
    fn analysis_survives_source_deletion() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"![[image.png]]");
        directory.write("image.png", b"image");
        let store = directory.store();
        let snapshot = snapshot(&directory, store.clone());
        directory.remove("note.md");

        let analyzed = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("note.md"))
            .unwrap();

        assert_eq!(analyzed.references()[0].reference().target(), "image.png");
    }

    #[test]
    fn rejects_invalid_utf8_from_a_valid_content_blob() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let sha256 = store.store(&[0xff, 0xfe]).unwrap();
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-source").unwrap(),
            vec![SnapshotFile::new(path("note.md"), 2, sha256, None)],
        )
        .unwrap();

        let result = SnapshotMarkdownAnalyzer::new(store).analyze(&snapshot, &path("note.md"));

        assert!(matches!(
            result,
            Err(SnapshotMarkdownAnalysisError::InvalidUtf8 { path: actual, .. }) if actual == path("note.md")
        ));
    }

    #[test]
    fn rejects_non_markdown_snapshot_files() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let sha256 = store.store(b"image").unwrap();
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-source").unwrap(),
            vec![SnapshotFile::new(path("image.png"), 5, sha256, None)],
        )
        .unwrap();

        let result = SnapshotMarkdownAnalyzer::new(store).analyze(&snapshot, &path("image.png"));

        assert!(matches!(
            result,
            Err(SnapshotMarkdownAnalysisError::NotMarkdown { path: actual }) if actual == path("image.png")
        ));
    }

    #[test]
    fn preserves_missing_ambiguous_and_external_resolutions() {
        let directory = TestDirectory::new();
        directory.write("article.md", b"[[missing]] [[note]] https://example.com");
        directory.write("a/note.md", b"a");
        directory.write("b/note.md", b"b");
        let store = directory.store();
        let snapshot = snapshot(&directory, store.clone());

        let analyzed = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("article.md"))
            .unwrap();

        assert!(matches!(
            analyzed.references()[0].resolution(),
            Resolution::Missing { target } if target == "missing"
        ));
        assert!(matches!(
            analyzed.references()[1].resolution(),
            Resolution::Ambiguous { target, candidates } if target == "note" && candidates.len() == 2
        ));
        assert!(matches!(
            analyzed.references()[2].resolution(),
            Resolution::External { target } if target == "https://example.com"
        ));
    }

    #[test]
    fn preserves_content_store_missing_and_corrupt_errors() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let missing = Sha256::digest(b"missing");
        let missing_snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-source").unwrap(),
            vec![SnapshotFile::new(path("missing.md"), 7, missing, None)],
        )
        .unwrap();
        assert!(matches!(
            SnapshotMarkdownAnalyzer::new(store.clone()).analyze(&missing_snapshot, &path("missing.md")),
            Err(SnapshotMarkdownAnalysisError::ContentStore {
                source: ContentStoreError::Missing(identity),
                ..
            }) if identity == missing
        ));

        let identity = store.store(b"original").unwrap();
        fs::write(store.root().join(identity.to_string()), b"corrupt").unwrap();
        let corrupt_snapshot = Snapshot::new(
            SnapshotId::new(2).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-source").unwrap(),
            vec![SnapshotFile::new(path("corrupt.md"), 8, identity, None)],
        )
        .unwrap();
        assert!(matches!(
            SnapshotMarkdownAnalyzer::new(store).analyze(&corrupt_snapshot, &path("corrupt.md")),
            Err(SnapshotMarkdownAnalysisError::ContentStore {
                source: ContentStoreError::Corrupt { expected, .. },
                ..
            }) if expected == identity
        ));
    }
}
