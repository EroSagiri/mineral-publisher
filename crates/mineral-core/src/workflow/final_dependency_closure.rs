use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use crate::{
    content::{AssetDependencyGraph, DependencyProblem, is_navigation_warning},
    domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId},
};

use super::{
    AssetReviewRunId, EffectiveReviewDecision, EffectiveReviewSet, SanitizedAsset,
    SanitizedAssetSet,
};

/// Why an effectively approved Markdown document cannot enter the final set.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MarkdownBlockingReason {
    AssetReviewMissing { asset_path: ContentPath },
    AssetRejected { asset_path: ContentPath },
    AssetPendingHumanReview { asset_path: ContentPath },
    AssetNotSanitized { asset_path: ContentPath },
}

impl MarkdownBlockingReason {
    pub fn asset_path(&self) -> &ContentPath {
        match self {
            Self::AssetReviewMissing { asset_path }
            | Self::AssetRejected { asset_path }
            | Self::AssetPendingHumanReview { asset_path }
            | Self::AssetNotSanitized { asset_path } => asset_path,
        }
    }
}

/// An effectively approved Markdown document removed only by final dependency closure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockedMarkdown {
    path: ContentPath,
    reasons: Vec<MarkdownBlockingReason>,
}

impl BlockedMarkdown {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn reasons(&self) -> &[MarkdownBlockingReason] {
        &self.reasons
    }
}

/// The deterministic, Snapshot-bound logical publication set.
///
/// This type does not choose projection paths or perform publication IO. Asset
/// entries retain the immutable source identity and the sanitized publication
/// blob identity selected by the preceding sanitization stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalPublicationSet {
    snapshot_id: SnapshotId,
    markdown: Vec<ContentPath>,
    assets: Vec<SanitizedAsset>,
    blocked_markdown: Vec<BlockedMarkdown>,
}

impl FinalPublicationSet {
    #[doc(hidden)]
    pub fn from_parts_for_test(
        snapshot_id: SnapshotId,
        markdown: Vec<ContentPath>,
        assets: Vec<SanitizedAsset>,
    ) -> Self {
        Self {
            snapshot_id,
            markdown,
            assets,
            blocked_markdown: Vec::new(),
        }
    }

    /// Computes the final Markdown set and then derives its exact asset closure.
    ///
    /// This consumes existing results only. It performs no parsing, policy,
    /// review, sanitization, content-store access, or filesystem IO.
    pub fn close(
        effective_reviews: &EffectiveReviewSet,
        graph: &AssetDependencyGraph,
        sanitized_assets: &SanitizedAssetSet,
        snapshot: &Snapshot,
    ) -> Result<Self, FinalDependencyClosureError> {
        validate_snapshot_ids(effective_reviews, graph, sanitized_assets, snapshot)?;

        let approved_markdown = effective_reviews
            .approved_markdown_paths()
            .into_iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let problems = graph
            .problems()
            .iter()
            .filter(|problem| {
                approved_markdown.contains(problem.document_path())
                    && !is_navigation_warning(problem)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !problems.is_empty() {
            return Err(
                FinalDependencyClosureError::ApprovedDocumentsHaveDependencyProblems(problems),
            );
        }

        let effective_assets = unique_effective_assets(effective_reviews)?;
        let sanitized_by_path = unique_sanitized_assets(sanitized_assets)?;
        let dependencies_by_document = dependencies_by_document(graph, &approved_markdown);

        let mut markdown = Vec::new();
        let mut blocked_markdown = Vec::new();
        let mut final_asset_paths = BTreeSet::new();

        for document_path in approved_markdown {
            let dependencies = dependencies_by_document
                .get(&document_path)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut reasons = Vec::new();

            for asset_path in dependencies {
                match effective_assets.get(asset_path) {
                    None => reasons.push(MarkdownBlockingReason::AssetReviewMissing {
                        asset_path: (*asset_path).clone(),
                    }),
                    Some(review) => match review.decision() {
                        EffectiveReviewDecision::Rejected => {
                            reasons.push(MarkdownBlockingReason::AssetRejected {
                                asset_path: (*asset_path).clone(),
                            });
                        }
                        EffectiveReviewDecision::PendingHumanReview => {
                            reasons.push(MarkdownBlockingReason::AssetPendingHumanReview {
                                asset_path: (*asset_path).clone(),
                            });
                        }
                        EffectiveReviewDecision::Approved => {
                            let snapshot_file = snapshot_file(snapshot, asset_path)?;
                            let Some(sanitized) = sanitized_by_path.get(asset_path) else {
                                reasons.push(MarkdownBlockingReason::AssetNotSanitized {
                                    asset_path: (*asset_path).clone(),
                                });
                                continue;
                            };
                            validate_sanitized_binding(
                                asset_path,
                                review.review_run_id(),
                                snapshot_file,
                                sanitized,
                            )?;
                        }
                    },
                }
            }

            reasons.sort();
            reasons.dedup();
            if reasons.is_empty() {
                markdown.push(document_path);
                final_asset_paths.extend(dependencies.iter().map(|path| (*path).clone()));
            } else {
                blocked_markdown.push(BlockedMarkdown {
                    path: document_path,
                    reasons,
                });
            }
        }

        let assets = final_asset_paths
            .into_iter()
            .map(|path| {
                (*sanitized_by_path
                    .get(&path)
                    .expect("publishable dependencies were validated above"))
                .clone()
            })
            .collect();

        Ok(Self {
            snapshot_id: snapshot.id(),
            markdown,
            assets,
            blocked_markdown,
        })
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn markdown_paths(&self) -> impl ExactSizeIterator<Item = &ContentPath> {
        self.markdown.iter()
    }

    pub fn asset_paths(&self) -> impl ExactSizeIterator<Item = &ContentPath> {
        self.assets.iter().map(SanitizedAsset::path)
    }

    pub fn assets(&self) -> &[SanitizedAsset] {
        &self.assets
    }

    pub fn blocked_markdown(&self) -> &[BlockedMarkdown] {
        &self.blocked_markdown
    }

    pub fn contains_markdown(&self, path: &ContentPath) -> bool {
        self.markdown.binary_search(path).is_ok()
    }

    pub fn contains_asset(&self, path: &ContentPath) -> bool {
        self.assets
            .binary_search_by(|asset| asset.path().cmp(path))
            .is_ok()
    }
}

fn validate_snapshot_ids(
    effective_reviews: &EffectiveReviewSet,
    graph: &AssetDependencyGraph,
    sanitized_assets: &SanitizedAssetSet,
    snapshot: &Snapshot,
) -> Result<(), FinalDependencyClosureError> {
    let snapshot_id = snapshot.id();
    if effective_reviews.snapshot_id() != snapshot_id
        || graph.snapshot_id() != snapshot_id
        || sanitized_assets.snapshot_id() != snapshot_id
    {
        return Err(FinalDependencyClosureError::SnapshotMismatch {
            effective_review_snapshot_id: effective_reviews.snapshot_id(),
            graph_snapshot_id: graph.snapshot_id(),
            sanitized_snapshot_id: sanitized_assets.snapshot_id(),
            snapshot_id,
        });
    }
    Ok(())
}

fn unique_effective_assets(
    reviews: &EffectiveReviewSet,
) -> Result<BTreeMap<ContentPath, &super::EffectiveAssetReview>, FinalDependencyClosureError> {
    let mut by_path = BTreeMap::new();
    for review in reviews.assets() {
        if by_path
            .insert(review.content_path().clone(), review)
            .is_some()
        {
            return Err(FinalDependencyClosureError::DuplicateEffectiveAssetReview(
                review.content_path().clone(),
            ));
        }
    }
    Ok(by_path)
}

fn unique_sanitized_assets(
    assets: &SanitizedAssetSet,
) -> Result<BTreeMap<ContentPath, &SanitizedAsset>, FinalDependencyClosureError> {
    let mut by_path = BTreeMap::new();
    for asset in assets.assets() {
        if by_path.insert(asset.path().clone(), asset).is_some() {
            return Err(FinalDependencyClosureError::DuplicateSanitizedAsset(
                asset.path().clone(),
            ));
        }
    }
    Ok(by_path)
}

fn dependencies_by_document<'a>(
    graph: &'a AssetDependencyGraph,
    approved_markdown: &BTreeSet<ContentPath>,
) -> BTreeMap<ContentPath, Vec<&'a ContentPath>> {
    let mut by_document: BTreeMap<ContentPath, Vec<&ContentPath>> = BTreeMap::new();
    for dependency in graph.dependencies() {
        if approved_markdown.contains(dependency.document_path()) {
            by_document
                .entry(dependency.document_path().clone())
                .or_default()
                .push(dependency.asset_path());
        }
    }
    for dependencies in by_document.values_mut() {
        dependencies.sort();
        dependencies.dedup();
    }
    by_document
}

fn snapshot_file<'a>(
    snapshot: &'a Snapshot,
    path: &ContentPath,
) -> Result<&'a SnapshotFile, FinalDependencyClosureError> {
    snapshot
        .files()
        .binary_search_by(|file| file.path().cmp(path))
        .ok()
        .map(|index| &snapshot.files()[index])
        .ok_or_else(|| FinalDependencyClosureError::ApprovedAssetMissingFromSnapshot(path.clone()))
}

fn validate_sanitized_binding(
    path: &ContentPath,
    effective_review_run_id: AssetReviewRunId,
    snapshot_file: &SnapshotFile,
    sanitized: &SanitizedAsset,
) -> Result<(), FinalDependencyClosureError> {
    if sanitized.review_run_id() != effective_review_run_id {
        return Err(FinalDependencyClosureError::SanitizedReviewRunMismatch {
            path: path.clone(),
            effective_review_run_id,
            sanitized_review_run_id: sanitized.review_run_id(),
        });
    }
    if sanitized.source_sha256() != snapshot_file.sha256() {
        return Err(
            FinalDependencyClosureError::SanitizedSourceIdentityMismatch {
                path: path.clone(),
                snapshot_sha256: snapshot_file.sha256(),
                sanitized_source_sha256: sanitized.source_sha256(),
            },
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinalDependencyClosureError {
    SnapshotMismatch {
        effective_review_snapshot_id: SnapshotId,
        graph_snapshot_id: SnapshotId,
        sanitized_snapshot_id: SnapshotId,
        snapshot_id: SnapshotId,
    },
    ApprovedDocumentsHaveDependencyProblems(Vec<DependencyProblem>),
    DuplicateEffectiveAssetReview(ContentPath),
    DuplicateSanitizedAsset(ContentPath),
    ApprovedAssetMissingFromSnapshot(ContentPath),
    SanitizedReviewRunMismatch {
        path: ContentPath,
        effective_review_run_id: AssetReviewRunId,
        sanitized_review_run_id: AssetReviewRunId,
    },
    SanitizedSourceIdentityMismatch {
        path: ContentPath,
        snapshot_sha256: Sha256,
        sanitized_source_sha256: Sha256,
    },
}

impl fmt::Display for FinalDependencyClosureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotMismatch { .. } => formatter.write_str(
                "effective reviews, dependency graph, sanitized assets, and snapshot must share one snapshot identity",
            ),
            Self::ApprovedDocumentsHaveDependencyProblems(problems) => write!(
                formatter,
                "{} unresolved dependency problem(s) belong to effectively approved Markdown",
                problems.len()
            ),
            Self::DuplicateEffectiveAssetReview(path) => {
                write!(formatter, "duplicate effective asset review: {path}")
            }
            Self::DuplicateSanitizedAsset(path) => {
                write!(formatter, "duplicate sanitized asset: {path}")
            }
            Self::ApprovedAssetMissingFromSnapshot(path) => {
                write!(formatter, "approved asset is missing from snapshot: {path}")
            }
            Self::SanitizedReviewRunMismatch { path, .. } => write!(
                formatter,
                "sanitized asset was produced for a different effective review run: {path}"
            ),
            Self::SanitizedSourceIdentityMismatch { path, .. } => write!(
                formatter,
                "sanitized asset source identity does not match snapshot: {path}"
            ),
        }
    }
}

impl Error for FinalDependencyClosureError {}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{
        content::{AnalyzedMarkdown, MarkdownReferenceParser, Resolution, ResolvedReference},
        domain::{SnapshotFile, SourceId},
        workflow::{
            EffectiveAssetReview, EffectiveDocumentDecision, ImageSanitizationFormat,
            SanitizationTransformation,
        },
    };

    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn snapshot_id(value: u64) -> SnapshotId {
        SnapshotId::new(value).unwrap()
    }

    fn review_id(value: u64) -> AssetReviewRunId {
        AssetReviewRunId::new(value).unwrap()
    }

    fn hash(value: u8) -> Sha256 {
        Sha256::new([value; 32])
    }

    fn analyzed(
        document_path: &str,
        markdown: &str,
        resolutions: Vec<Resolution>,
    ) -> AnalyzedMarkdown {
        let references = MarkdownReferenceParser::parse(markdown);
        assert_eq!(references.len(), resolutions.len());
        AnalyzedMarkdown::new(
            SnapshotFile::new(
                path(document_path),
                markdown.len() as u64,
                Sha256::digest(markdown.as_bytes()),
                None,
            ),
            references
                .into_iter()
                .zip(resolutions)
                .map(|(reference, resolution)| ResolvedReference::new(reference, resolution))
                .collect(),
        )
    }

    fn asset_resolution(value: &str) -> Resolution {
        Resolution::ResolvedAsset { path: path(value) }
    }

    fn snapshot(id: SnapshotId, files: &[(&str, Sha256)]) -> Snapshot {
        Snapshot::new(
            id,
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files
                .iter()
                .map(|(name, sha256)| SnapshotFile::new(path(name), 1, *sha256, None))
                .collect(),
        )
        .unwrap()
    }

    fn reviews(
        id: SnapshotId,
        documents: &[(&str, EffectiveDocumentDecision)],
        assets: &[(&str, u64, EffectiveReviewDecision)],
    ) -> EffectiveReviewSet {
        EffectiveReviewSet::from_parts_for_test(
            id,
            documents
                .iter()
                .map(|(name, decision)| (path(name), *decision))
                .collect(),
            assets
                .iter()
                .map(|(name, run, decision)| {
                    EffectiveAssetReview::from_parts_for_test(
                        path(name),
                        review_id(*run),
                        *decision,
                        Vec::new(),
                    )
                })
                .collect(),
        )
    }

    fn sanitized_asset(
        name: &str,
        run: u64,
        source_sha256: Sha256,
        published_sha256: Sha256,
    ) -> SanitizedAsset {
        SanitizedAsset::from_parts(
            path(name),
            review_id(run),
            source_sha256,
            published_sha256,
            42,
            vec![
                SanitizationTransformation::StripMetadata,
                SanitizationTransformation::ReencodeImage {
                    format: ImageSanitizationFormat::Png,
                },
            ],
        )
    }

    fn sanitized(id: SnapshotId, assets: Vec<SanitizedAsset>) -> SanitizedAssetSet {
        SanitizedAssetSet::from_assets(id, assets)
    }

    fn graph(id: SnapshotId, documents: &[AnalyzedMarkdown]) -> AssetDependencyGraph {
        AssetDependencyGraph::build(id, documents)
    }

    #[test]
    fn clean_closure_retains_published_blob_identity() {
        let id = snapshot_id(1);
        let source = hash(1);
        let published = hash(2);
        let document = analyzed(
            "a.md",
            "![[image.png]]",
            vec![asset_resolution("image.png")],
        );
        let sanitized_asset = sanitized_asset("image.png", 1, source, published);

        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[("a.md", EffectiveDocumentDecision::Approved)],
                &[("image.png", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![sanitized_asset]),
            &snapshot(id, &[("a.md", hash(9)), ("image.png", source)]),
        )
        .unwrap();

        assert_eq!(result.snapshot_id(), id);
        assert_eq!(result.markdown_paths().collect::<Vec<_>>(), [&path("a.md")]);
        assert_eq!(
            result.asset_paths().collect::<Vec<_>>(),
            [&path("image.png")]
        );
        assert!(result.contains_markdown(&path("a.md")));
        assert!(result.contains_asset(&path("image.png")));
        assert!(result.blocked_markdown().is_empty());
        assert_eq!(result.assets()[0].source_sha256(), source);
        assert_eq!(result.assets()[0].published_sha256(), published);
        assert_eq!(result.assets()[0].published_size(), 42);
        assert_eq!(
            result.assets()[0].transformations(),
            [
                SanitizationTransformation::StripMetadata,
                SanitizationTransformation::ReencodeImage {
                    format: ImageSanitizationFormat::Png,
                },
            ]
        );
    }

    #[test]
    fn rejected_asset_blocks_only_the_effectively_approved_document() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[image.png]]",
            vec![asset_resolution("image.png")],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[
                    ("a.md", EffectiveDocumentDecision::Approved),
                    ("already-rejected.md", EffectiveDocumentDecision::Rejected),
                ],
                &[("image.png", 1, EffectiveReviewDecision::Rejected)],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![]),
            &snapshot(id, &[("image.png", hash(1))]),
        )
        .unwrap();

        assert!(result.markdown_paths().next().is_none());
        assert!(result.asset_paths().next().is_none());
        assert_eq!(result.blocked_markdown().len(), 1);
        assert_eq!(result.blocked_markdown()[0].path(), &path("a.md"));
        assert_eq!(
            result.blocked_markdown()[0].reasons(),
            [MarkdownBlockingReason::AssetRejected {
                asset_path: path("image.png")
            }]
        );
    }

    #[test]
    fn pending_asset_is_not_treated_as_approved() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[image.png]]",
            vec![asset_resolution("image.png")],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[("a.md", EffectiveDocumentDecision::Approved)],
                &[("image.png", 1, EffectiveReviewDecision::PendingHumanReview)],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![]),
            &snapshot(id, &[("image.png", hash(1))]),
        )
        .unwrap();

        assert_eq!(
            result.blocked_markdown()[0].reasons(),
            [MarkdownBlockingReason::AssetPendingHumanReview {
                asset_path: path("image.png")
            }]
        );
    }

    #[test]
    fn missing_effective_asset_review_blocks_document() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[image.png]]",
            vec![asset_resolution("image.png")],
        );
        let result = FinalPublicationSet::close(
            &reviews(id, &[("a.md", EffectiveDocumentDecision::Approved)], &[]),
            &graph(id, &[document]),
            &sanitized(id, vec![]),
            &snapshot(id, &[("image.png", hash(1))]),
        )
        .unwrap();

        assert_eq!(
            result.blocked_markdown()[0].reasons(),
            [MarkdownBlockingReason::AssetReviewMissing {
                asset_path: path("image.png")
            }]
        );
    }

    #[test]
    fn approved_asset_without_sanitized_result_blocks_document_including_pdf_case() {
        let id = snapshot_id(1);
        let document = analyzed(
            "article.md",
            "[pdf](report.pdf)",
            vec![asset_resolution("report.pdf")],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[("article.md", EffectiveDocumentDecision::Approved)],
                &[("report.pdf", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![]),
            &snapshot(id, &[("report.pdf", hash(1))]),
        )
        .unwrap();

        assert_eq!(
            result.blocked_markdown()[0].reasons(),
            [MarkdownBlockingReason::AssetNotSanitized {
                asset_path: path("report.pdf")
            }]
        );
    }

    #[test]
    fn one_bad_mandatory_asset_blocks_document_and_removes_good_orphan() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[good.png]] ![[bad.png]]",
            vec![asset_resolution("good.png"), asset_resolution("bad.png")],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[("a.md", EffectiveDocumentDecision::Approved)],
                &[
                    ("good.png", 1, EffectiveReviewDecision::Approved),
                    ("bad.png", 2, EffectiveReviewDecision::Rejected),
                ],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![sanitized_asset("good.png", 1, hash(1), hash(2))]),
            &snapshot(id, &[("bad.png", hash(3)), ("good.png", hash(1))]),
        )
        .unwrap();

        assert!(result.markdown_paths().next().is_none());
        assert!(result.asset_paths().next().is_none());
        assert!(!result.contains_asset(&path("good.png")));
    }

    #[test]
    fn shared_asset_is_deduplicated_for_two_final_documents() {
        let id = snapshot_id(1);
        let a = analyzed(
            "a.md",
            "![[shared.png]]",
            vec![asset_resolution("shared.png")],
        );
        let b = analyzed(
            "b.md",
            "![[shared.png]]",
            vec![asset_resolution("shared.png")],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[
                    ("b.md", EffectiveDocumentDecision::Approved),
                    ("a.md", EffectiveDocumentDecision::Approved),
                ],
                &[("shared.png", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[b, a]),
            &sanitized(id, vec![sanitized_asset("shared.png", 1, hash(1), hash(2))]),
            &snapshot(id, &[("shared.png", hash(1))]),
        )
        .unwrap();

        assert_eq!(
            result
                .markdown_paths()
                .map(ContentPath::as_str)
                .collect::<Vec<_>>(),
            ["a.md", "b.md"]
        );
        assert_eq!(
            result.asset_paths().collect::<Vec<_>>(),
            [&path("shared.png")]
        );
    }

    #[test]
    fn shared_asset_remains_when_one_document_is_blocked_by_another_asset() {
        let id = snapshot_id(1);
        let a = analyzed(
            "a.md",
            "![[shared.png]] ![[bad.png]]",
            vec![asset_resolution("shared.png"), asset_resolution("bad.png")],
        );
        let b = analyzed(
            "b.md",
            "![[shared.png]]",
            vec![asset_resolution("shared.png")],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[
                    ("a.md", EffectiveDocumentDecision::Approved),
                    ("b.md", EffectiveDocumentDecision::Approved),
                ],
                &[
                    ("bad.png", 2, EffectiveReviewDecision::Rejected),
                    ("shared.png", 1, EffectiveReviewDecision::Approved),
                ],
            ),
            &graph(id, &[a, b]),
            &sanitized(id, vec![sanitized_asset("shared.png", 1, hash(1), hash(2))]),
            &snapshot(id, &[("bad.png", hash(3)), ("shared.png", hash(1))]),
        )
        .unwrap();

        assert_eq!(result.markdown_paths().collect::<Vec<_>>(), [&path("b.md")]);
        assert_eq!(
            result.asset_paths().collect::<Vec<_>>(),
            [&path("shared.png")]
        );
        assert_eq!(result.blocked_markdown()[0].path(), &path("a.md"));
    }

    #[test]
    fn shared_asset_disappears_when_all_dependents_are_blocked() {
        let id = snapshot_id(1);
        let a = analyzed(
            "a.md",
            "![[shared.png]] ![[bad-a.png]]",
            vec![
                asset_resolution("shared.png"),
                asset_resolution("bad-a.png"),
            ],
        );
        let b = analyzed(
            "b.md",
            "![[shared.png]] ![[bad-b.png]]",
            vec![
                asset_resolution("shared.png"),
                asset_resolution("bad-b.png"),
            ],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[
                    ("a.md", EffectiveDocumentDecision::Approved),
                    ("b.md", EffectiveDocumentDecision::Approved),
                ],
                &[
                    ("bad-a.png", 2, EffectiveReviewDecision::Rejected),
                    ("bad-b.png", 3, EffectiveReviewDecision::Rejected),
                    ("shared.png", 1, EffectiveReviewDecision::Approved),
                ],
            ),
            &graph(id, &[a, b]),
            &sanitized(id, vec![sanitized_asset("shared.png", 1, hash(1), hash(2))]),
            &snapshot(
                id,
                &[
                    ("bad-a.png", hash(3)),
                    ("bad-b.png", hash(4)),
                    ("shared.png", hash(1)),
                ],
            ),
        )
        .unwrap();

        assert_eq!(result.blocked_markdown().len(), 2);
        assert!(result.markdown_paths().next().is_none());
        assert!(result.asset_paths().next().is_none());
    }

    #[test]
    fn orphan_sanitized_asset_is_excluded() {
        let id = snapshot_id(1);
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[("a.md", EffectiveDocumentDecision::Approved)],
                &[("unused.png", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[analyzed("a.md", "no assets", vec![])]),
            &sanitized(id, vec![sanitized_asset("unused.png", 1, hash(1), hash(2))]),
            &snapshot(id, &[("unused.png", hash(1))]),
        )
        .unwrap();

        assert_eq!(result.markdown_paths().collect::<Vec<_>>(), [&path("a.md")]);
        assert!(result.asset_paths().next().is_none());
    }

    #[test]
    fn private_only_asset_never_enters_final_set() {
        let id = snapshot_id(1);
        let private = analyzed(
            "private.md",
            "![[secret.png]]",
            vec![asset_resolution("secret.png")],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[("private.md", EffectiveDocumentDecision::Private)],
                &[("secret.png", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[private]),
            &sanitized(id, vec![sanitized_asset("secret.png", 1, hash(1), hash(2))]),
            &snapshot(id, &[("secret.png", hash(1))]),
        )
        .unwrap();

        assert!(result.markdown_paths().next().is_none());
        assert!(result.asset_paths().next().is_none());
        assert!(result.blocked_markdown().is_empty());
    }

    #[test]
    fn note_links_neither_expand_nor_block_final_markdown() {
        let id = snapshot_id(1);
        let a = analyzed(
            "a.md",
            "[[b]]",
            vec![Resolution::ResolvedNote { path: path("b.md") }],
        );
        let result = FinalPublicationSet::close(
            &reviews(
                id,
                &[
                    ("a.md", EffectiveDocumentDecision::Approved),
                    ("b.md", EffectiveDocumentDecision::Private),
                ],
                &[],
            ),
            &graph(id, &[a]),
            &sanitized(id, vec![]),
            &snapshot(id, &[]),
        )
        .unwrap();

        assert_eq!(result.markdown_paths().collect::<Vec<_>>(), [&path("a.md")]);
        assert!(!result.contains_markdown(&path("b.md")));
    }

    #[test]
    fn dependency_problem_on_approved_markdown_fails_closed() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[missing.png]]",
            vec![Resolution::Missing {
                target: "missing.png".to_owned(),
            }],
        );

        let error = FinalPublicationSet::close(
            &reviews(id, &[("a.md", EffectiveDocumentDecision::Approved)], &[]),
            &graph(id, &[document]),
            &sanitized(id, vec![]),
            &snapshot(id, &[]),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            FinalDependencyClosureError::ApprovedDocumentsHaveDependencyProblems(problems)
                if problems.len() == 1 && problems[0].document_path() == &path("a.md")
        ));
    }

    #[test]
    fn snapshot_mismatch_from_any_input_is_rejected_before_closure() {
        let one = snapshot_id(1);
        let two = snapshot_id(2);
        let effective = reviews(one, &[], &[]);
        let graph_one = graph(one, &[]);
        let graph_two = graph(two, &[]);
        let sanitized_one = sanitized(one, vec![]);
        let sanitized_two = sanitized(two, vec![]);
        let snapshot_one = snapshot(one, &[]);

        for result in [
            FinalPublicationSet::close(&effective, &graph_two, &sanitized_one, &snapshot_one),
            FinalPublicationSet::close(&effective, &graph_one, &sanitized_two, &snapshot_one),
            FinalPublicationSet::close(
                &reviews(two, &[], &[]),
                &graph_one,
                &sanitized_one,
                &snapshot_one,
            ),
        ] {
            assert!(matches!(
                result,
                Err(FinalDependencyClosureError::SnapshotMismatch { .. })
            ));
        }
    }

    #[test]
    fn sanitized_source_identity_mismatch_is_an_invariant_error() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[image.png]]",
            vec![asset_resolution("image.png")],
        );
        let error = FinalPublicationSet::close(
            &reviews(
                id,
                &[("a.md", EffectiveDocumentDecision::Approved)],
                &[("image.png", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![sanitized_asset("image.png", 1, hash(2), hash(3))]),
            &snapshot(id, &[("image.png", hash(1))]),
        )
        .unwrap_err();

        assert_eq!(
            error,
            FinalDependencyClosureError::SanitizedSourceIdentityMismatch {
                path: path("image.png"),
                snapshot_sha256: hash(1),
                sanitized_source_sha256: hash(2),
            }
        );
    }

    #[test]
    fn approved_asset_missing_from_snapshot_is_an_invariant_error() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[image.png]]",
            vec![asset_resolution("image.png")],
        );
        let error = FinalPublicationSet::close(
            &reviews(
                id,
                &[("a.md", EffectiveDocumentDecision::Approved)],
                &[("image.png", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![]),
            &snapshot(id, &[]),
        )
        .unwrap_err();

        assert_eq!(
            error,
            FinalDependencyClosureError::ApprovedAssetMissingFromSnapshot(path("image.png"))
        );
    }

    #[test]
    fn sanitized_result_must_belong_to_selected_effective_review_run() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[image.png]]",
            vec![asset_resolution("image.png")],
        );
        let error = FinalPublicationSet::close(
            &reviews(
                id,
                &[("a.md", EffectiveDocumentDecision::Approved)],
                &[("image.png", 1, EffectiveReviewDecision::Approved)],
            ),
            &graph(id, &[document]),
            &sanitized(id, vec![sanitized_asset("image.png", 2, hash(1), hash(2))]),
            &snapshot(id, &[("image.png", hash(1))]),
        )
        .unwrap_err();

        assert_eq!(
            error,
            FinalDependencyClosureError::SanitizedReviewRunMismatch {
                path: path("image.png"),
                effective_review_run_id: review_id(1),
                sanitized_review_run_id: review_id(2),
            }
        );
    }

    #[test]
    fn empty_approved_markdown_set_is_a_valid_empty_result() {
        let id = snapshot_id(1);
        let result = FinalPublicationSet::close(
            &reviews(id, &[], &[]),
            &graph(id, &[]),
            &sanitized(id, vec![]),
            &snapshot(id, &[]),
        )
        .unwrap();

        assert!(result.markdown_paths().next().is_none());
        assert!(result.asset_paths().next().is_none());
        assert!(result.blocked_markdown().is_empty());
    }

    #[test]
    fn output_is_independent_of_document_dependency_and_sanitized_input_order() {
        let id = snapshot_id(1);
        let a = analyzed(
            "a.md",
            "![[z.png]] ![[shared.png]]",
            vec![asset_resolution("z.png"), asset_resolution("shared.png")],
        );
        let b = analyzed(
            "b.md",
            "![[bad.png]] ![[shared.png]]",
            vec![asset_resolution("bad.png"), asset_resolution("shared.png")],
        );
        let first_reviews = reviews(
            id,
            &[
                ("b.md", EffectiveDocumentDecision::Approved),
                ("a.md", EffectiveDocumentDecision::Approved),
            ],
            &[
                ("z.png", 3, EffectiveReviewDecision::Approved),
                ("bad.png", 2, EffectiveReviewDecision::Rejected),
                ("shared.png", 1, EffectiveReviewDecision::Approved),
            ],
        );
        let second_reviews = reviews(
            id,
            &[
                ("a.md", EffectiveDocumentDecision::Approved),
                ("b.md", EffectiveDocumentDecision::Approved),
            ],
            &[
                ("shared.png", 1, EffectiveReviewDecision::Approved),
                ("bad.png", 2, EffectiveReviewDecision::Rejected),
                ("z.png", 3, EffectiveReviewDecision::Approved),
            ],
        );
        let first_sanitized = sanitized(
            id,
            vec![
                sanitized_asset("z.png", 3, hash(3), hash(4)),
                sanitized_asset("shared.png", 1, hash(1), hash(2)),
            ],
        );
        let second_sanitized = sanitized(
            id,
            vec![
                sanitized_asset("shared.png", 1, hash(1), hash(2)),
                sanitized_asset("z.png", 3, hash(3), hash(4)),
            ],
        );
        let source_snapshot = snapshot(
            id,
            &[
                ("bad.png", hash(5)),
                ("shared.png", hash(1)),
                ("z.png", hash(3)),
            ],
        );

        let first = FinalPublicationSet::close(
            &first_reviews,
            &graph(id, &[b.clone(), a.clone()]),
            &first_sanitized,
            &source_snapshot,
        )
        .unwrap();
        let second = FinalPublicationSet::close(
            &second_reviews,
            &graph(id, &[a, b]),
            &second_sanitized,
            &source_snapshot,
        )
        .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first
                .markdown_paths()
                .map(ContentPath::as_str)
                .collect::<Vec<_>>(),
            ["a.md"]
        );
        assert_eq!(
            first
                .asset_paths()
                .map(ContentPath::as_str)
                .collect::<Vec<_>>(),
            ["shared.png", "z.png"]
        );
        assert_eq!(first.blocked_markdown()[0].path(), &path("b.md"));
    }
}
