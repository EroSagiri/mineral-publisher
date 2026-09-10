use crate::{
    asset_dependency::{
        AssetDependencyGraph, DependencyProblem, DependencyProblemKind, ReferenceOrigin,
    },
    domain::ContentPath,
    privacy_filter::PublicCandidateMarkdown,
};
use serde::{Deserialize, Serialize};

/// One deterministic structural issue found before external AI review.
///
/// The underlying dependency problem is retained intact so callers can inspect
/// the original reference, its source span, all ambiguous candidates, and a
/// typed invalid-resolution reason.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProgramCheckIssue(DependencyProblem);

impl ProgramCheckIssue {
    pub fn document_path(&self) -> &ContentPath {
        self.0.document_path()
    }

    pub fn origin(&self) -> &ReferenceOrigin {
        self.0.origin()
    }

    pub fn kind(&self) -> &DependencyProblemKind {
        self.0.kind()
    }
}

/// Result of deterministic checks that run before external AI review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramCheckResult {
    Pass,
    Issues(Vec<ProgramCheckIssue>),
}

impl ProgramCheckResult {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    pub fn has_issues(&self) -> bool {
        !self.is_pass()
    }

    pub fn issues(&self) -> &[ProgramCheckIssue] {
        match self {
            Self::Pass => &[],
            Self::Issues(issues) => issues,
        }
    }
}

/// Pure, deterministic structural checks over public Markdown candidates.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProgramCheck;

impl ProgramCheck {
    /// Checks only analyses that have crossed the deterministic privacy filter.
    ///
    /// This performs no parsing, resolution, source access, or network IO. The
    /// existing asset dependency model remains the single interpreter of
    /// unresolved references.
    pub fn check(documents: &[PublicCandidateMarkdown]) -> ProgramCheckResult {
        let analyses = documents
            .iter()
            .map(|document| document.analysis().clone())
            .collect::<Vec<_>>();
        let graph = AssetDependencyGraph::build(&analyses);
        let issues = graph
            .problems()
            .iter()
            .cloned()
            .map(ProgramCheckIssue)
            .collect::<Vec<_>>();

        if issues.is_empty() {
            ProgramCheckResult::Pass
        } else {
            ProgramCheckResult::Issues(issues)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        asset_dependency::{AssetDependencyGraph, DependencyProblemKind},
        domain::{
            ContentPath, InvalidResolutionReason, MarkdownFrontmatterParser,
            MarkdownReferenceParser, ReferenceKind, Resolution, ResolutionCandidate,
            ResolvedTargetKind, Sha256, SnapshotFile,
        },
        privacy_filter::PrivacyFilter,
        snapshot_markdown_analysis::{AnalyzedMarkdown, ResolvedReference},
    };

    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn candidate(kind: ResolvedTargetKind, value: &str) -> ResolutionCandidate {
        ResolutionCandidate::new(kind, path(value))
    }

    fn analyzed(
        document_path: &str,
        markdown: &str,
        resolutions: Vec<Resolution>,
    ) -> AnalyzedMarkdown {
        let references = MarkdownReferenceParser::parse(markdown);
        assert_eq!(references.len(), resolutions.len());
        let resolved = references
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
            resolved,
        )
    }

    fn public_candidates(documents: Vec<AnalyzedMarkdown>) -> Vec<PublicCandidateMarkdown> {
        let result = PrivacyFilter::filter(documents);
        assert!(result.private_documents().is_empty());
        assert!(result.invalid_documents().is_empty());
        result.into_public_candidates()
    }

    #[test]
    fn fully_resolved_document_passes() {
        let documents = public_candidates(vec![analyzed(
            "article.md",
            "[[existing-note]] ![[existing.png]] [report](report.pdf) https://example.com",
            vec![
                Resolution::ResolvedNote {
                    path: path("existing-note.md"),
                },
                Resolution::ResolvedAsset {
                    path: path("existing.png"),
                },
                Resolution::ResolvedAsset {
                    path: path("report.pdf"),
                },
                Resolution::External {
                    target: "https://example.com".to_owned(),
                },
            ],
        )]);

        assert_eq!(ProgramCheck::check(&documents), ProgramCheckResult::Pass);
    }

    #[test]
    fn missing_local_reference_is_an_issue() {
        let documents = public_candidates(vec![analyzed(
            "article.md",
            "![[missing.png]]",
            vec![Resolution::Missing {
                target: "missing.png".to_owned(),
            }],
        )]);

        let result = ProgramCheck::check(&documents);

        assert!(result.has_issues());
        assert!(matches!(
            result.issues()[0].kind(),
            DependencyProblemKind::Missing { target } if target == "missing.png"
        ));
        assert_eq!(result.issues()[0].document_path(), &path("article.md"));
        assert_eq!(result.issues()[0].origin().kind(), ReferenceKind::WikiEmbed);
        assert_eq!(result.issues()[0].origin().target(), "missing.png");
        assert_eq!(result.issues()[0].origin().span(), 0..16);
    }

    #[test]
    fn ambiguous_local_reference_retains_every_sorted_candidate() {
        let documents = public_candidates(vec![analyzed(
            "article.md",
            "![[image.png]]",
            vec![Resolution::Ambiguous {
                target: "image.png".to_owned(),
                candidates: vec![
                    candidate(ResolvedTargetKind::Asset, "b/image.png"),
                    candidate(ResolvedTargetKind::Asset, "a/image.png"),
                ],
            }],
        )]);

        assert!(matches!(
            ProgramCheck::check(&documents).issues()[0].kind(),
            DependencyProblemKind::Ambiguous { target, candidates }
                if target == "image.png"
                    && candidates == &[
                        candidate(ResolvedTargetKind::Asset, "a/image.png"),
                        candidate(ResolvedTargetKind::Asset, "b/image.png"),
                    ]
        ));
    }

    #[test]
    fn invalid_local_reference_retains_typed_reason() {
        let documents = public_candidates(vec![analyzed(
            "notes/article.md",
            "[file](../../secret.pdf)",
            vec![Resolution::Invalid {
                target: "../../secret.pdf".to_owned(),
                reason: InvalidResolutionReason::EscapesContentRoot,
            }],
        )]);

        assert!(matches!(
            ProgramCheck::check(&documents).issues()[0].kind(),
            DependencyProblemKind::Invalid { target, reason }
                if target == "../../secret.pdf"
                    && *reason == InvalidResolutionReason::EscapesContentRoot
        ));
    }

    #[test]
    fn resolved_note_and_embedded_note_are_not_issues_or_asset_dependencies() {
        let document = analyzed(
            "article.md",
            "[[another-note]] ![[embedded-note]]",
            vec![
                Resolution::ResolvedNote {
                    path: path("another-note.md"),
                },
                Resolution::ResolvedNote {
                    path: path("embedded-note.md"),
                },
            ],
        );
        let graph = AssetDependencyGraph::from_document(&document);
        let documents = public_candidates(vec![document]);

        assert!(graph.dependencies().is_empty());
        assert!(graph.problems().is_empty());
        assert!(ProgramCheck::check(&documents).is_pass());
    }

    #[test]
    fn external_reference_is_not_an_issue() {
        let documents = public_candidates(vec![analyzed(
            "article.md",
            "[site](https://example.com)",
            vec![Resolution::External {
                target: "https://example.com".to_owned(),
            }],
        )]);

        assert!(ProgramCheck::check(&documents).is_pass());
    }

    #[test]
    fn all_structural_issues_are_retained() {
        let markdown = "![[missing.png]] ![[image.png]] [file](../../secret.pdf)";
        let documents = public_candidates(vec![analyzed(
            "notes/article.md",
            markdown,
            vec![
                Resolution::Missing {
                    target: "missing.png".to_owned(),
                },
                Resolution::Ambiguous {
                    target: "image.png".to_owned(),
                    candidates: vec![
                        candidate(ResolvedTargetKind::Asset, "b/image.png"),
                        candidate(ResolvedTargetKind::Asset, "a/image.png"),
                    ],
                },
                Resolution::Invalid {
                    target: "../../secret.pdf".to_owned(),
                    reason: InvalidResolutionReason::EscapesContentRoot,
                },
            ],
        )]);

        let result = ProgramCheck::check(&documents);

        assert_eq!(result.issues().len(), 3);
        assert!(matches!(
            result.issues()[0].kind(),
            DependencyProblemKind::Missing { .. }
        ));
        assert!(matches!(
            result.issues()[1].kind(),
            DependencyProblemKind::Ambiguous { .. }
        ));
        assert!(matches!(
            result.issues()[2].kind(),
            DependencyProblemKind::Invalid { .. }
        ));
    }

    #[test]
    fn output_is_independent_of_document_and_candidate_order() {
        fn documents(reverse_candidates: bool) -> Vec<AnalyzedMarkdown> {
            let mut candidates = vec![
                candidate(ResolvedTargetKind::Asset, "a/image.png"),
                candidate(ResolvedTargetKind::Asset, "z/image.png"),
            ];
            if reverse_candidates {
                candidates.reverse();
            }
            vec![
                analyzed(
                    "z.md",
                    "![[missing.png]]",
                    vec![Resolution::Missing {
                        target: "missing.png".to_owned(),
                    }],
                ),
                analyzed(
                    "a.md",
                    "![[image.png]]",
                    vec![Resolution::Ambiguous {
                        target: "image.png".to_owned(),
                        candidates,
                    }],
                ),
            ]
        }

        let first = public_candidates(documents(false));
        let mut second = public_candidates(documents(true));
        second.reverse();

        assert_eq!(ProgramCheck::check(&first), ProgramCheck::check(&second));
    }

    #[test]
    fn private_and_invalid_documents_cannot_reach_the_check_entrypoint() {
        let filtered = PrivacyFilter::filter(vec![
            analyzed("private.md", "body", vec![]),
            analyzed("invalid.md", "---\nprivate: maybe\n---", vec![]),
            analyzed("public.md", "body", vec![]),
        ]);

        assert_eq!(filtered.public_candidates().len(), 1);
        assert_eq!(filtered.private_documents().len(), 1);
        assert_eq!(filtered.invalid_documents().len(), 1);
        assert!(ProgramCheck::check(filtered.public_candidates()).is_pass());
    }
}
