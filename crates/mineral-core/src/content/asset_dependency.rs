use std::{collections::BTreeMap, ops::Range};

use serde::{Deserialize, Serialize};

use crate::domain::{ContentPath, SnapshotId};

use super::{
    AnalyzedMarkdown, InvalidResolutionReason, ReferenceKind, Resolution, ResolutionCandidate,
};

/// Where a resolved dependency or unresolved dependency problem appeared.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ReferenceOrigin {
    span_start: usize,
    span_end: usize,
    kind: ReferenceKind,
    target: String,
}

impl ReferenceOrigin {
    pub fn kind(&self) -> ReferenceKind {
        self.kind
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn span(&self) -> Range<usize> {
        self.span_start..self.span_end
    }
}

/// One logical local asset required by a Markdown document.
///
/// Repeated references to the same asset are represented by multiple origins,
/// not duplicate dependency edges.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AssetDependency {
    document_path: ContentPath,
    asset_path: ContentPath,
    origins: Vec<ReferenceOrigin>,
}

impl AssetDependency {
    pub fn document_path(&self) -> &ContentPath {
        &self.document_path
    }

    pub fn asset_path(&self) -> &ContentPath {
        &self.asset_path
    }

    pub fn origins(&self) -> &[ReferenceOrigin] {
        &self.origins
    }
}

/// Why a local reference could not be safely classified as an asset dependency.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyProblemKind {
    Missing {
        target: String,
    },
    Ambiguous {
        target: String,
        candidates: Vec<ResolutionCandidate>,
    },
    Invalid {
        target: String,
        reason: InvalidResolutionReason,
    },
}

/// An unresolved local reference retained for later policy or reporting layers.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DependencyProblem {
    document_path: ContentPath,
    origin: ReferenceOrigin,
    kind: DependencyProblemKind,
}

impl DependencyProblem {
    pub fn document_path(&self) -> &ContentPath {
        &self.document_path
    }

    pub fn origin(&self) -> &ReferenceOrigin {
        &self.origin
    }

    pub fn kind(&self) -> &DependencyProblemKind {
        &self.kind
    }
}

/// Deterministic asset dependency edges and unresolved local-reference problems.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetDependencyGraph {
    snapshot_id: SnapshotId,
    dependencies: Vec<AssetDependency>,
    problems: Vec<DependencyProblem>,
}

impl AssetDependencyGraph {
    /// Analyzes the asset dependencies of one already-analyzed Markdown file.
    pub fn from_document(snapshot_id: SnapshotId, document: &AnalyzedMarkdown) -> Self {
        Self::build(snapshot_id, std::slice::from_ref(document))
    }

    /// Builds a graph only from existing Markdown analysis results.
    ///
    /// This function performs no parsing, resolution, storage, or filesystem IO.
    pub fn build(snapshot_id: SnapshotId, documents: &[AnalyzedMarkdown]) -> Self {
        let (dependencies, problems) = analyze_documents(documents);

        Self {
            snapshot_id,
            dependencies,
            problems,
        }
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn dependencies(&self) -> &[AssetDependency] {
        &self.dependencies
    }

    pub fn problems(&self) -> &[DependencyProblem] {
        &self.problems
    }
}

pub(crate) fn dependency_problems(documents: &[AnalyzedMarkdown]) -> Vec<DependencyProblem> {
    analyze_documents(documents).1
}

/// Missing or ambiguous extensionless WikiLinks are navigation diagnostics,
/// not publication dependencies. Obsidian commonly uses links to notes that
/// may not exist yet (including daily-note paths and heading links). Explicit
/// file extensions, embeds, Markdown links/images, and invalid paths remain
/// blocking because they can denote required publication files.
pub fn is_navigation_warning(problem: &DependencyProblem) -> bool {
    if problem.origin().kind() != ReferenceKind::WikiLink {
        return false;
    }
    if !matches!(
        problem.kind(),
        DependencyProblemKind::Missing { .. } | DependencyProblemKind::Ambiguous { .. }
    ) {
        return false;
    }
    let target = problem
        .origin()
        .target()
        .split('#')
        .next()
        .unwrap_or_default();
    let final_segment = target.rsplit('/').next().unwrap_or(target);
    !final_segment.contains('.')
}

fn analyze_documents(
    documents: &[AnalyzedMarkdown],
) -> (Vec<AssetDependency>, Vec<DependencyProblem>) {
    let mut dependencies = Vec::new();
    let mut problems = Vec::new();

    for document in documents {
        let document_path = document.path().clone();
        let mut document_assets: BTreeMap<ContentPath, Vec<ReferenceOrigin>> = BTreeMap::new();

        for resolved_reference in document.references() {
            let reference = resolved_reference.reference();
            let span = reference.span();
            let origin = ReferenceOrigin {
                kind: reference.kind(),
                target: reference.target().to_owned(),
                span_start: span.start,
                span_end: span.end,
            };

            match resolved_reference.resolution() {
                Resolution::ResolvedAsset { path }
                    if matches!(
                        reference.kind(),
                        ReferenceKind::WikiEmbed
                            | ReferenceKind::MarkdownImage
                            | ReferenceKind::MarkdownLink
                    ) =>
                {
                    document_assets
                        .entry(path.clone())
                        .or_default()
                        .push(origin);
                }
                // A note is never an asset dependency, including when embedded.
                Resolution::ResolvedNote { .. } | Resolution::External { .. } => {}
                Resolution::Missing { target } => problems.push(DependencyProblem {
                    document_path: document_path.clone(),
                    origin,
                    kind: DependencyProblemKind::Missing {
                        target: target.clone(),
                    },
                }),
                Resolution::Ambiguous { target, candidates } => {
                    let mut candidates = candidates.clone();
                    candidates.sort();
                    candidates.dedup();
                    problems.push(DependencyProblem {
                        document_path: document_path.clone(),
                        origin,
                        kind: DependencyProblemKind::Ambiguous {
                            target: target.clone(),
                            candidates,
                        },
                    });
                }
                Resolution::Invalid { target, reason } => {
                    problems.push(DependencyProblem {
                        document_path: document_path.clone(),
                        origin,
                        kind: DependencyProblemKind::Invalid {
                            target: target.clone(),
                            reason: *reason,
                        },
                    });
                }
                // These combinations cannot be produced by the current resolver.
                Resolution::ResolvedAsset { .. } => {}
            }
        }

        dependencies.extend(
            document_assets
                .into_iter()
                .map(|(asset_path, mut origins)| {
                    origins.sort();
                    origins.dedup();
                    AssetDependency {
                        document_path: document_path.clone(),
                        asset_path,
                        origins,
                    }
                }),
        );
    }

    dependencies.sort();
    dependencies.dedup();
    problems.sort();
    problems.dedup();

    (dependencies, problems)
}

#[cfg(test)]
mod tests {
    use crate::{
        content::{MarkdownReferenceParser, Resolution, ResolvedReference, ResolvedTargetKind},
        domain::{Sha256, SnapshotFile},
    };

    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn snapshot_id() -> SnapshotId {
        SnapshotId::new(1).unwrap()
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
        AnalyzedMarkdown::new(file, resolved)
    }

    fn asset(value: &str) -> Resolution {
        Resolution::ResolvedAsset { path: path(value) }
    }

    fn note(value: &str) -> Resolution {
        Resolution::ResolvedNote { path: path(value) }
    }

    #[test]
    fn supported_local_asset_references_form_dependencies() {
        let document = analyzed(
            "article.md",
            "![[wiki.png]] ![image](image.png) [file](report.pdf)",
            vec![
                asset("assets/wiki.png"),
                asset("assets/image.png"),
                asset("assets/report.pdf"),
            ],
        );

        let graph = AssetDependencyGraph::from_document(snapshot_id(), &document);

        assert_eq!(
            graph
                .dependencies()
                .iter()
                .map(|dependency| dependency.asset_path().as_str())
                .collect::<Vec<_>>(),
            ["assets/image.png", "assets/report.pdf", "assets/wiki.png"]
        );
        assert!(graph.problems().is_empty());
    }

    #[test]
    fn resolved_notes_never_form_asset_dependencies() {
        let document = analyzed(
            "public.md",
            "[[private]] ![[embedded-note]] [note](other.md)",
            vec![
                note("private.md"),
                note("embedded-note.md"),
                note("other.md"),
            ],
        );

        let graph = AssetDependencyGraph::build(snapshot_id(), &[document]);

        assert!(graph.dependencies().is_empty());
        assert!(graph.problems().is_empty());
    }

    #[test]
    fn external_references_form_neither_dependencies_nor_problems() {
        let document = analyzed(
            "article.md",
            "https://example.com [site](https://example.com/a)",
            vec![
                Resolution::External {
                    target: "https://example.com".to_owned(),
                },
                Resolution::External {
                    target: "https://example.com/a".to_owned(),
                },
            ],
        );

        let graph = AssetDependencyGraph::build(snapshot_id(), &[document]);

        assert!(graph.dependencies().is_empty());
        assert!(graph.problems().is_empty());
    }

    #[test]
    fn duplicate_asset_references_create_one_edge_with_both_origins() {
        let document = analyzed(
            "article.md",
            "![[image.png]] ![again](image.png)",
            vec![asset("image.png"), asset("image.png")],
        );

        let graph = AssetDependencyGraph::build(snapshot_id(), &[document]);

        assert_eq!(graph.dependencies().len(), 1);
        assert_eq!(graph.dependencies()[0].origins().len(), 2);
    }

    #[test]
    fn shared_asset_is_represented_by_one_edge_per_document() {
        let a = analyzed("a.md", "![[image.png]]", vec![asset("image.png")]);
        let b = analyzed("b.md", "![image](image.png)", vec![asset("image.png")]);

        let graph = AssetDependencyGraph::build(snapshot_id(), &[a, b]);

        assert_eq!(graph.dependencies().len(), 2);
        assert_eq!(graph.dependencies()[0].document_path().as_str(), "a.md");
        assert_eq!(graph.dependencies()[1].document_path().as_str(), "b.md");
        assert!(
            graph
                .dependencies()
                .iter()
                .all(|dependency| dependency.asset_path().as_str() == "image.png")
        );
    }

    #[test]
    fn missing_ambiguous_and_invalid_are_retained_as_problems() {
        let document = analyzed(
            "article.md",
            "![[missing.png]] ![[item]] ![bad](../outside.png)",
            vec![
                Resolution::Missing {
                    target: "missing.png".to_owned(),
                },
                Resolution::Ambiguous {
                    target: "item".to_owned(),
                    candidates: vec![
                        candidate(ResolvedTargetKind::Asset, "z/item"),
                        candidate(ResolvedTargetKind::Note, "a/item.md"),
                    ],
                },
                Resolution::Invalid {
                    target: "../outside.png".to_owned(),
                    reason: InvalidResolutionReason::EscapesContentRoot,
                },
            ],
        );

        let graph = AssetDependencyGraph::build(snapshot_id(), &[document]);

        assert!(graph.dependencies().is_empty());
        assert_eq!(graph.problems().len(), 3);
        assert!(matches!(
            graph.problems()[0].kind(),
            DependencyProblemKind::Missing { target } if target == "missing.png"
        ));
        assert!(matches!(
            graph.problems()[1].kind(),
            DependencyProblemKind::Ambiguous { target, candidates }
                if target == "item"
                    && candidates
                        == &[
                            candidate(ResolvedTargetKind::Note, "a/item.md"),
                            candidate(ResolvedTargetKind::Asset, "z/item"),
                        ]
        ));
        assert!(matches!(
            graph.problems()[2].kind(),
            DependencyProblemKind::Invalid { target, reason }
                if target == "../outside.png"
                    && *reason == InvalidResolutionReason::EscapesContentRoot
        ));
    }

    #[test]
    fn output_is_independent_of_document_and_candidate_input_order() {
        let first_a = analyzed(
            "a.md",
            "![[shared.png]] ![[item]]",
            vec![
                asset("shared.png"),
                Resolution::Ambiguous {
                    target: "item".to_owned(),
                    candidates: vec![
                        candidate(ResolvedTargetKind::Asset, "z/item"),
                        candidate(ResolvedTargetKind::Note, "a/item.md"),
                    ],
                },
            ],
        );
        let first_b = analyzed("b.md", "[file](b.pdf)", vec![asset("b.pdf")]);
        let second_a = analyzed(
            "a.md",
            "![[shared.png]] ![[item]]",
            vec![
                asset("shared.png"),
                Resolution::Ambiguous {
                    target: "item".to_owned(),
                    candidates: vec![
                        candidate(ResolvedTargetKind::Note, "a/item.md"),
                        candidate(ResolvedTargetKind::Asset, "z/item"),
                    ],
                },
            ],
        );
        let second_b = analyzed("b.md", "[file](b.pdf)", vec![asset("b.pdf")]);

        assert_eq!(
            AssetDependencyGraph::build(snapshot_id(), &[first_a, first_b]),
            AssetDependencyGraph::build(snapshot_id(), &[second_b, second_a])
        );
    }

    #[test]
    fn graph_retains_snapshot_identity() {
        let id = SnapshotId::new(42).unwrap();
        let graph = AssetDependencyGraph::build(id, &[]);

        assert_eq!(graph.snapshot_id(), id);
    }
}
