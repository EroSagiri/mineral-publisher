use crate::{content::AnalyzedMarkdown, domain::ContentPath};

use super::{PrivacyClassification, PrivateReason};

/// A Markdown analysis that passed the deterministic privacy boundary.
///
/// This is only an input candidate for later checks and review. It is not an
/// approval to publish the document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicCandidateMarkdown(AnalyzedMarkdown);

impl PublicCandidateMarkdown {
    pub fn path(&self) -> &ContentPath {
        self.0.path()
    }

    /// Exposes the already-computed analysis without reading or parsing content again.
    pub fn analysis(&self) -> &AnalyzedMarkdown {
        &self.0
    }

    pub fn into_analysis(self) -> AnalyzedMarkdown {
        self.0
    }
}

/// An explicitly private Markdown document rejected before external review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateDocument {
    path: ContentPath,
    reasons: Vec<PrivateReason>,
}

impl PrivateDocument {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn reasons(&self) -> &[PrivateReason] {
        &self.reasons
    }
}

/// A Markdown document whose privacy metadata could not be interpreted safely.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidPrivacyDocument {
    path: ContentPath,
    reason: String,
}

impl InvalidPrivacyDocument {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Auditable output of the deterministic pre-review privacy boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrivacyFilterResult {
    public_candidates: Vec<PublicCandidateMarkdown>,
    private_documents: Vec<PrivateDocument>,
    invalid_documents: Vec<InvalidPrivacyDocument>,
}

impl PrivacyFilterResult {
    /// The only documents eligible to enter later programmatic and AI review.
    pub fn public_candidates(&self) -> &[PublicCandidateMarkdown] {
        &self.public_candidates
    }

    pub fn private_documents(&self) -> &[PrivateDocument] {
        &self.private_documents
    }

    pub fn invalid_documents(&self) -> &[InvalidPrivacyDocument] {
        &self.invalid_documents
    }

    pub fn into_public_candidates(self) -> Vec<PublicCandidateMarkdown> {
        self.public_candidates
    }

    pub fn into_parts(
        self,
    ) -> (
        Vec<PublicCandidateMarkdown>,
        Vec<PrivateDocument>,
        Vec<InvalidPrivacyDocument>,
    ) {
        (
            self.public_candidates,
            self.private_documents,
            self.invalid_documents,
        )
    }
}

/// Applies existing privacy classifications without content or source access.
pub struct PrivacyFilter;

impl PrivacyFilter {
    /// Filters the complete analyzed Markdown set for a Snapshot.
    ///
    /// The input is consumed so public candidates retain the existing analysis
    /// without copying Markdown content. Private and invalid analyses cross no
    /// later-review boundary; only their audit path and reasons are retained.
    pub fn filter(documents: Vec<AnalyzedMarkdown>) -> PrivacyFilterResult {
        let mut result = PrivacyFilterResult::default();

        for document in documents {
            match document.privacy() {
                PrivacyClassification::PublicCandidate => result
                    .public_candidates
                    .push(PublicCandidateMarkdown(document)),
                PrivacyClassification::Private { reasons } => {
                    result.private_documents.push(PrivateDocument {
                        path: document.path().clone(),
                        reasons: reasons.clone(),
                    });
                }
                PrivacyClassification::Invalid { reason } => {
                    result.invalid_documents.push(InvalidPrivacyDocument {
                        path: document.path().clone(),
                        reason: reason.clone(),
                    });
                }
            }
        }

        result
            .public_candidates
            .sort_by(|left, right| left.path().cmp(right.path()));
        result
            .private_documents
            .sort_by(|left, right| left.path.cmp(&right.path));
        result
            .invalid_documents
            .sort_by(|left, right| left.path.cmp(&right.path));

        result
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        content::{AssetDependencyGraph, MarkdownReferenceParser, Resolution, ResolvedReference},
        domain::{Sha256, SnapshotFile},
        policy::{FrontmatterParseResult, MarkdownFrontmatterParser},
    };

    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn analyzed(document_path: &str, markdown: &str) -> AnalyzedMarkdown {
        analyzed_with_resolutions(document_path, markdown, Vec::new())
    }

    fn analyzed_with_resolutions(
        document_path: &str,
        markdown: &str,
        resolutions: Vec<Resolution>,
    ) -> AnalyzedMarkdown {
        let references = MarkdownReferenceParser::parse(markdown);
        assert_eq!(references.len(), resolutions.len());
        let references = references
            .into_iter()
            .zip(resolutions)
            .map(|(reference, resolution)| ResolvedReference::new(reference, resolution))
            .collect();
        let file = SnapshotFile::new(
            path(document_path),
            markdown.len() as u64,
            Sha256::digest(markdown.as_bytes()),
            None,
        );

        AnalyzedMarkdown::with_frontmatter(
            file,
            MarkdownFrontmatterParser::parse(markdown),
            references,
        )
    }

    fn candidate_paths(result: &PrivacyFilterResult) -> Vec<&str> {
        result
            .public_candidates()
            .iter()
            .map(|document| document.path().as_str())
            .collect()
    }

    #[test]
    fn all_public_documents_become_review_candidates() {
        let result = PrivacyFilter::filter(vec![
            analyzed("c.md", "c"),
            analyzed("a.md", "a"),
            analyzed("b.md", "b"),
        ]);

        assert_eq!(candidate_paths(&result), ["a.md", "b.md", "c.md"]);
        assert!(result.private_documents().is_empty());
        assert!(result.invalid_documents().is_empty());
    }

    #[test]
    fn private_document_is_rejected_with_its_reasons() {
        let result = PrivacyFilter::filter(vec![
            analyzed("普通.md", "ordinary"),
            analyzed("私人/日记.md", "private"),
        ]);

        assert_eq!(candidate_paths(&result), ["普通.md"]);
        assert_eq!(result.private_documents().len(), 1);
        assert_eq!(
            result.private_documents()[0].path().as_str(),
            "私人/日记.md"
        );
        assert_eq!(
            result.private_documents()[0].reasons(),
            [PrivateReason::PathContainsPrivateMarker]
        );
        assert!(result.invalid_documents().is_empty());
    }

    #[test]
    fn invalid_privacy_metadata_fails_closed_and_remains_distinct() {
        let result = PrivacyFilter::filter(vec![analyzed(
            "invalid.md",
            "---\nprivate: maybe\n---\nbody",
        )]);

        assert!(result.public_candidates().is_empty());
        assert!(result.private_documents().is_empty());
        assert_eq!(result.invalid_documents().len(), 1);
        assert_eq!(result.invalid_documents()[0].path().as_str(), "invalid.md");
        assert!(result.invalid_documents()[0].reason().contains("private"));
    }

    #[test]
    fn mixed_classifications_are_grouped_separately() {
        let result = PrivacyFilter::filter(vec![
            analyzed("public.md", "body"),
            analyzed("private.md", "body"),
            analyzed("invalid.md", "---\npublic: perhaps\n---"),
        ]);

        assert_eq!(candidate_paths(&result), ["public.md"]);
        assert_eq!(result.private_documents()[0].path().as_str(), "private.md");
        assert_eq!(result.invalid_documents()[0].path().as_str(), "invalid.md");
    }

    #[test]
    fn all_private_reasons_are_preserved() {
        let result = PrivacyFilter::filter(vec![analyzed(
            "私人/private-note.md",
            "---\nprivate: true\nvisibility: private\npublic: false\npublish: false\ntags: [private]\n---",
        )]);

        assert_eq!(
            result.private_documents()[0].reasons(),
            [
                PrivateReason::PathContainsPrivateMarker,
                PrivateReason::FrontmatterPrivate,
                PrivateReason::VisibilityPrivate,
                PrivateReason::PublicFalse,
                PrivateReason::PublishFalse,
                PrivateReason::PrivateTag,
            ]
        );
    }

    #[test]
    fn output_is_independent_of_input_order() {
        fn documents() -> Vec<AnalyzedMarkdown> {
            vec![
                analyzed("z-public.md", "body"),
                analyzed("z-private.md", "body"),
                analyzed("z-invalid.md", "---\nprivate: maybe\n---"),
                analyzed("a-public.md", "body"),
                analyzed("a-private.md", "body"),
                analyzed("a-invalid.md", "---\nprivate: maybe\n---"),
            ]
        }

        let first = PrivacyFilter::filter(documents());
        let mut reversed = documents();
        reversed.reverse();
        let second = PrivacyFilter::filter(reversed);

        assert_eq!(first, second);
    }

    #[test]
    fn markdown_links_do_not_propagate_candidate_status() {
        let public = analyzed_with_resolutions(
            "public.md",
            "[[private-note]]",
            vec![Resolution::ResolvedNote {
                path: path("private-note.md"),
            }],
        );
        let private = analyzed("private-note.md", "private body");

        let result = PrivacyFilter::filter(vec![private, public]);

        assert_eq!(candidate_paths(&result), ["public.md"]);
        assert_eq!(
            result.private_documents()[0].path().as_str(),
            "private-note.md"
        );
    }

    #[test]
    fn private_document_assets_do_not_enter_a_public_asset_set() {
        let public = analyzed("public.md", "body");
        let private = analyzed_with_resolutions(
            "private.md",
            "![[secret.png]]",
            vec![Resolution::ResolvedAsset {
                path: path("secret.png"),
            }],
        );

        let candidates = PrivacyFilter::filter(vec![private, public]).into_public_candidates();
        let public_graph = AssetDependencyGraph::build(
            &candidates
                .into_iter()
                .map(PublicCandidateMarkdown::into_analysis)
                .collect::<Vec<_>>(),
        );

        assert!(public_graph.dependencies().is_empty());
    }

    #[test]
    fn filtering_consumes_existing_analysis_without_reclassifying() {
        let document = analyzed("public.md", "body");
        let expected_file = document.file().clone();
        let expected_frontmatter = FrontmatterParseResult::Absent;

        let candidate = PrivacyFilter::filter(vec![document])
            .into_public_candidates()
            .pop()
            .unwrap();

        assert_eq!(candidate.analysis().file(), &expected_file);
        assert_eq!(candidate.analysis().frontmatter(), &expected_frontmatter);
    }
}
