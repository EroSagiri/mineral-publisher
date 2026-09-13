use std::{
    error::Error,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use crate::domain::{
    ContentPath, ContentPathError, Snapshot, SnapshotError, SnapshotFile, SnapshotId, SourceId,
};
use crate::storage::{ContentStoreError, LocalContentStore};

/// A source adapter that reads regular files from one local directory tree.
#[derive(Clone, Debug)]
pub struct LocalSource {
    root: PathBuf,
    source_id: SourceId,
    content_store: LocalContentStore,
}

impl LocalSource {
    pub fn new(
        root: impl Into<PathBuf>,
        source_id: SourceId,
        content_store: LocalContentStore,
    ) -> Self {
        Self {
            root: root.into(),
            source_id,
            content_store,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Reads the current directory tree into a new immutable snapshot.
    pub fn snapshot(
        &self,
        id: SnapshotId,
        created_at: SystemTime,
    ) -> Result<Snapshot, LocalSourceError> {
        self.validate_root()?;

        let mut paths = Vec::new();
        Self::collect_regular_files(&self.root, &mut paths)?;

        let mut files = paths
            .into_iter()
            .map(|path| self.read_snapshot_file(path))
            .collect::<Result<Vec<_>, _>>()?;
        files.sort_by(|left, right| left.path().cmp(right.path()));

        Snapshot::new(id, created_at, self.source_id.clone(), files)
            .map_err(LocalSourceError::Snapshot)
    }

    fn validate_root(&self) -> Result<(), LocalSourceError> {
        let metadata = match fs::metadata(&self.root) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(LocalSourceError::RootNotFound(self.root.clone()));
            }
            Err(source) => {
                return Err(LocalSourceError::RootMetadata {
                    path: self.root.clone(),
                    source,
                });
            }
        };

        if !metadata.is_dir() {
            return Err(LocalSourceError::RootNotDirectory(self.root.clone()));
        }

        Ok(())
    }

    fn collect_regular_files(
        directory: &Path,
        paths: &mut Vec<PathBuf>,
    ) -> Result<(), LocalSourceError> {
        let entries =
            fs::read_dir(directory).map_err(|source| LocalSourceError::DirectoryRead {
                path: directory.to_path_buf(),
                source,
            })?;
        let mut entries = entries
            .map(|entry| {
                entry.map_err(|source| LocalSourceError::DirectoryRead {
                    path: directory.to_path_buf(),
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| LocalSourceError::DirectoryRead {
                        path: path.clone(),
                        source,
                    })?;

            if file_type.is_dir() {
                Self::collect_regular_files(&path, paths)?;
            } else if file_type.is_file() {
                paths.push(path);
            }
        }

        Ok(())
    }

    fn read_snapshot_file(&self, path: PathBuf) -> Result<SnapshotFile, LocalSourceError> {
        let content_path = self.content_path(&path)?;
        let bytes = fs::read(&path).map_err(|source| LocalSourceError::FileRead {
            path: path.clone(),
            source,
        })?;
        let size = u64::try_from(bytes.len())
            .map_err(|_| LocalSourceError::FileTooLarge { path: path.clone() })?;
        let sha256 =
            self.content_store
                .store(&bytes)
                .map_err(|source| LocalSourceError::ContentStore {
                    path: path.clone(),
                    source,
                })?;

        Ok(SnapshotFile::new(content_path, size, sha256, None))
    }

    fn content_path(&self, path: &Path) -> Result<ContentPath, LocalSourceError> {
        let relative =
            path.strip_prefix(&self.root)
                .map_err(|_| LocalSourceError::PathOutsideRoot {
                    path: path.to_path_buf(),
                })?;
        let canonical = relative
            .components()
            .map(|component| match component {
                Component::Normal(component) => {
                    component.to_str().map(str::to_owned).ok_or_else(|| {
                        LocalSourceError::NonUnicodePath {
                            path: path.to_path_buf(),
                        }
                    })
                }
                _ => Err(LocalSourceError::InvalidContentPath {
                    path: path.to_path_buf(),
                    source: ContentPathError::NotCanonical,
                }),
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");

        ContentPath::new(canonical).map_err(|source| LocalSourceError::InvalidContentPath {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug)]
pub enum LocalSourceError {
    RootNotFound(PathBuf),
    RootNotDirectory(PathBuf),
    RootMetadata {
        path: PathBuf,
        source: io::Error,
    },
    DirectoryRead {
        path: PathBuf,
        source: io::Error,
    },
    FileRead {
        path: PathBuf,
        source: io::Error,
    },
    FileTooLarge {
        path: PathBuf,
    },
    PathOutsideRoot {
        path: PathBuf,
    },
    NonUnicodePath {
        path: PathBuf,
    },
    InvalidContentPath {
        path: PathBuf,
        source: ContentPathError,
    },
    ContentStore {
        path: PathBuf,
        source: ContentStoreError,
    },
    Snapshot(SnapshotError),
}

impl fmt::Display for LocalSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootNotFound(path) => write!(
                formatter,
                "local source root does not exist: {}",
                path.display()
            ),
            Self::RootNotDirectory(path) => write!(
                formatter,
                "local source root is not a directory: {}",
                path.display()
            ),
            Self::RootMetadata { path, .. } => write!(
                formatter,
                "could not inspect local source root: {}",
                path.display()
            ),
            Self::DirectoryRead { path, .. } => write!(
                formatter,
                "could not enumerate local source directory: {}",
                path.display()
            ),
            Self::FileRead { path, .. } => write!(
                formatter,
                "could not read local source file: {}",
                path.display()
            ),
            Self::FileTooLarge { path } => write!(
                formatter,
                "local source file size cannot be represented: {}",
                path.display()
            ),
            Self::PathOutsideRoot { path } => write!(
                formatter,
                "local source path is outside its root: {}",
                path.display()
            ),
            Self::NonUnicodePath { path } => write!(
                formatter,
                "local source path is not valid Unicode: {}",
                path.display()
            ),
            Self::InvalidContentPath { path, .. } => write!(
                formatter,
                "local source path cannot be represented as a content path: {}",
                path.display()
            ),
            Self::ContentStore { path, .. } => write!(
                formatter,
                "could not preserve local source file content: {}",
                path.display()
            ),
            Self::Snapshot(_) => {
                formatter.write_str("could not create snapshot from local source files")
            }
        }
    }
}

impl Error for LocalSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RootMetadata { source, .. }
            | Self::DirectoryRead { source, .. }
            | Self::FileRead { source, .. } => Some(source),
            Self::InvalidContentPath { source, .. } => Some(source),
            Self::ContentStore { source, .. } => Some(source),
            Self::Snapshot(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::SystemTime,
    };

    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-local-source-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("source")).unwrap();
            Self(path)
        }

        fn source_path(&self) -> PathBuf {
            self.0.join("source")
        }

        fn store_path(&self) -> PathBuf {
            self.0.join("content-store")
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

    fn source(directory: &TestDirectory) -> LocalSource {
        LocalSource::new(
            directory.source_path(),
            SourceId::new("test-local-source").unwrap(),
            LocalContentStore::new(directory.store_path()),
        )
    }

    fn source_at(root: impl Into<PathBuf>, store: impl Into<PathBuf>) -> LocalSource {
        LocalSource::new(
            root,
            SourceId::new("test-local-source").unwrap(),
            LocalContentStore::new(store),
        )
    }

    fn snapshot(source: &LocalSource) -> Snapshot {
        source
            .snapshot(SnapshotId::new(1).unwrap(), SystemTime::UNIX_EPOCH)
            .unwrap()
    }

    #[test]
    fn snapshots_an_empty_directory() {
        let directory = TestDirectory::new();

        let snapshot = snapshot(&source(&directory));

        assert!(snapshot.files().is_empty());
    }

    #[test]
    fn snapshots_a_single_file_with_relative_path_size_and_sha256() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"hello");

        let content_store = LocalContentStore::new(directory.store_path());
        let local_source = source(&directory);
        let snapshot = snapshot(&local_source);

        assert_eq!(snapshot.files().len(), 1);
        let file = &snapshot.files()[0];
        assert_eq!(file.path().as_str(), "note.md");
        assert_eq!(file.size(), 5);
        assert_eq!(
            file.sha256().to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            file.size(),
            u64::try_from(content_store.read(file.sha256()).unwrap().len()).unwrap()
        );
    }

    #[test]
    fn snapshots_multiple_directory_levels_using_content_paths() {
        let directory = TestDirectory::new();
        directory.write("notes/2026/first.md", b"first");

        let snapshot = snapshot(&source(&directory));

        assert_eq!(snapshot.files()[0].path().as_str(), "notes/2026/first.md");
    }

    #[test]
    fn sorts_multiple_files_deterministically() {
        let directory = TestDirectory::new();
        directory.write("z.md", b"z");
        directory.write("a/note.md", b"a");
        directory.write("a.md", b"a");

        let local_source = source(&directory);
        let first = snapshot(&local_source);
        let second = snapshot(&local_source);
        let paths = first
            .files()
            .iter()
            .map(|file| file.path().as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, ["a.md", "a/note.md", "z.md"]);
        assert_eq!(first.files(), second.files());
    }

    #[test]
    fn changes_sha256_when_file_content_changes() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"before");
        let content_store = LocalContentStore::new(directory.store_path());
        let local_source = source(&directory);
        let first = snapshot(&local_source);
        let first_identity = first.files()[0].sha256();

        directory.write("note.md", b"after");
        let second = snapshot(&local_source);
        let second_identity = second.files()[0].sha256();

        assert_ne!(first_identity, second_identity);
        assert_eq!(content_store.read(first_identity).unwrap(), b"before");
        assert_eq!(content_store.read(second_identity).unwrap(), b"after");
    }

    #[test]
    fn old_snapshot_content_survives_source_modification() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"version A");
        let content_store = LocalContentStore::new(directory.store_path());
        let local_source = source(&directory);
        let old_snapshot = snapshot(&local_source);

        directory.write("note.md", b"version B");

        let old_identity = old_snapshot.files()[0].sha256();
        assert_eq!(content_store.read(old_identity).unwrap(), b"version A");
    }

    #[test]
    fn snapshot_content_survives_source_deletion() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"preserved");
        let content_store = LocalContentStore::new(directory.store_path());
        let local_source = source(&directory);
        let old_snapshot = snapshot(&local_source);

        directory.remove("note.md");

        let identity = old_snapshot.files()[0].sha256();
        assert_eq!(content_store.read(identity).unwrap(), b"preserved");
    }

    #[test]
    fn identical_files_share_one_content_identity_and_blob() {
        let directory = TestDirectory::new();
        directory.write("a.md", b"hello");
        directory.write("b.md", b"hello");
        let local_source = source(&directory);

        let snapshot = snapshot(&local_source);

        assert_eq!(snapshot.files()[0].sha256(), snapshot.files()[1].sha256());
        assert_eq!(fs::read_dir(directory.store_path()).unwrap().count(), 1);
    }

    #[test]
    fn snapshot_fails_when_content_cannot_be_stored() {
        let directory = TestDirectory::new();
        directory.write("note.md", b"content");
        let store_file = directory.store_path();
        fs::write(&store_file, b"not a directory").unwrap();
        let local_source = source_at(directory.source_path(), store_file);

        let result = local_source.snapshot(SnapshotId::new(1).unwrap(), SystemTime::UNIX_EPOCH);

        assert!(matches!(result, Err(LocalSourceError::ContentStore { .. })));
    }

    #[test]
    fn rejects_a_missing_root_directory() {
        let missing = std::env::temp_dir().join(format!(
            "mineral-publisher-missing-root-{}",
            std::process::id()
        ));
        let result = source_at(&missing, missing.with_extension("content-store"))
            .snapshot(SnapshotId::new(1).unwrap(), SystemTime::UNIX_EPOCH);

        assert!(matches!(result, Err(LocalSourceError::RootNotFound(path)) if path == missing));
    }

    #[test]
    fn rejects_a_file_as_the_root() {
        let directory = TestDirectory::new();
        directory.write("not-a-directory.md", b"content");
        let root = directory.source_path().join("not-a-directory.md");

        let result = source_at(&root, directory.store_path())
            .snapshot(SnapshotId::new(1).unwrap(), SystemTime::UNIX_EPOCH);

        assert!(matches!(result, Err(LocalSourceError::RootNotDirectory(path)) if path == root));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_path_that_cannot_be_represented_as_content_path() {
        let directory = TestDirectory::new();
        directory.write("note\\with-backslash.md", b"content");

        let result =
            source(&directory).snapshot(SnapshotId::new(1).unwrap(), SystemTime::UNIX_EPOCH);

        assert!(matches!(
            result,
            Err(LocalSourceError::InvalidContentPath {
                source: ContentPathError::NotCanonical,
                ..
            })
        ));
    }
}
