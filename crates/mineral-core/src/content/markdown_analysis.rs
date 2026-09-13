use std::{error::Error, fmt, string::FromUtf8Error};

use crate::{
    domain::{ContentPath, Snapshot, SnapshotFile},
    policy::{
        FrontmatterParseResult, MarkdownFrontmatterParser, PrivacyClassification, PrivacyClassifier,
    },
    ports::{BlobStore, ContentStoreError},
};

use super::{MarkdownReferenceParser, Reference, ReferenceResolver, Resolution};

/// Parsed and resolved references from one Markdown file in an immutable Snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalyzedMarkdown {
    file: SnapshotFile,
    frontmatter: FrontmatterParseResult,
    privacy: PrivacyClassification,
    references: Vec<ResolvedReference>,
}

impl AnalyzedMarkdown {
    /// Parses and resolves one Markdown document against the Snapshot it belongs to.
    ///
    /// This is the single definition of reference analysis. Every consumer that
    /// needs to know what a reference points at — dependency scanning, delivery
    /// rewriting, policy — goes through it, so two stages can never disagree
    /// about the target of the same syntax.
    pub fn analyze(file: SnapshotFile, markdown: &str, snapshot: &Snapshot) -> Self {
        let frontmatter = MarkdownFrontmatterParser::parse(markdown);
        let references = MarkdownReferenceParser::parse(markdown)
            .into_iter()
            .map(|reference| {
                let resolution = ReferenceResolver::resolve(&reference, file.path(), snapshot);
                ResolvedReference::new(reference, resolution)
            })
            .collect();
        Self::with_frontmatter(file, frontmatter, references)
    }

    pub fn with_frontmatter(
        file: SnapshotFile,
        frontmatter: FrontmatterParseResult,
        references: Vec<ResolvedReference>,
    ) -> Self {
        let privacy = PrivacyClassifier::classify(file.path(), &frontmatter);
        Self {
            file,
            frontmatter,
            privacy,
            references,
        }
    }

    #[doc(hidden)]
    pub fn new(file: SnapshotFile, references: Vec<ResolvedReference>) -> Self {
        Self::with_frontmatter(file, FrontmatterParseResult::Absent, references)
    }

    pub fn file(&self) -> &SnapshotFile {
        &self.file
    }

    pub fn path(&self) -> &ContentPath {
        self.file.path()
    }

    pub fn references(&self) -> &[ResolvedReference] {
        &self.references
    }

    pub fn frontmatter(&self) -> &FrontmatterParseResult {
        &self.frontmatter
    }

    pub fn privacy(&self) -> &PrivacyClassification {
        &self.privacy
    }
}

/// One parsed reference together with its resolution in the same Snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedReference {
    reference: Reference,
    resolution: Resolution,
}

impl ResolvedReference {
    #[doc(hidden)]
    pub fn new(reference: Reference, resolution: Resolution) -> Self {
        Self {
            reference,
            resolution,
        }
    }

    pub fn reference(&self) -> &Reference {
        &self.reference
    }

    pub fn resolution(&self) -> &Resolution {
        &self.resolution
    }
}

/// Reads Markdown only through the immutable content identity recorded by a Snapshot.
#[derive(Clone, Debug)]
pub struct SnapshotMarkdownAnalyzer<B: BlobStore> {
    content_store: B,
}

impl<B: BlobStore> SnapshotMarkdownAnalyzer<B> {
    pub fn new(content_store: B) -> Self {
        Self { content_store }
    }

    pub fn analyze(
        &self,
        snapshot: &Snapshot,
        path: &ContentPath,
    ) -> Result<AnalyzedMarkdown, SnapshotMarkdownAnalysisError> {
        let file = snapshot
            .files()
            .iter()
            .find(|file| file.path() == path)
            .ok_or_else(|| SnapshotMarkdownAnalysisError::FileNotInSnapshot {
                path: path.clone(),
            })?;
        if !is_markdown(file.path()) {
            return Err(SnapshotMarkdownAnalysisError::NotMarkdown {
                path: file.path().clone(),
            });
        }

        let bytes = self.content_store.read(file.sha256()).map_err(|source| {
            SnapshotMarkdownAnalysisError::ContentStore {
                path: file.path().clone(),
                source,
            }
        })?;
        let markdown = String::from_utf8(bytes).map_err(|source| {
            SnapshotMarkdownAnalysisError::InvalidUtf8 {
                path: file.path().clone(),
                source,
            }
        })?;

        Ok(AnalyzedMarkdown::analyze(file.clone(), &markdown, snapshot))
    }
}

fn is_markdown(path: &ContentPath) -> bool {
    path.as_str().ends_with(".md")
}

#[derive(Debug)]
pub enum SnapshotMarkdownAnalysisError {
    FileNotInSnapshot {
        path: ContentPath,
    },
    NotMarkdown {
        path: ContentPath,
    },
    ContentStore {
        path: ContentPath,
        source: ContentStoreError,
    },
    InvalidUtf8 {
        path: ContentPath,
        source: FromUtf8Error,
    },
}

impl fmt::Display for SnapshotMarkdownAnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileNotInSnapshot { path } => {
                write!(formatter, "snapshot does not contain Markdown file: {path}")
            }
            Self::NotMarkdown { path } => {
                write!(formatter, "snapshot file is not Markdown: {path}")
            }
            Self::ContentStore { path, .. } => write!(
                formatter,
                "could not read immutable snapshot content for Markdown file: {path}"
            ),
            Self::InvalidUtf8 { path, .. } => {
                write!(formatter, "snapshot Markdown is not valid UTF-8: {path}")
            }
        }
    }
}

impl Error for SnapshotMarkdownAnalysisError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ContentStore { source, .. } => Some(source),
            Self::InvalidUtf8 { source, .. } => Some(source),
            _ => None,
        }
    }
}
