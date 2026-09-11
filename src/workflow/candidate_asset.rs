use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use crate::{
    content::{AssetDependencyGraph, DependencyProblem},
    domain::{ContentPath, SnapshotId},
};

use super::PublicPolicyRunResult;

/// One local asset eligible for the later asset-review stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateAsset {
    path: ContentPath,
    dependents: Vec<ContentPath>,
}

impl CandidateAsset {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    /// Approved Markdown documents which require this asset, in stable order.
    pub fn dependents(&self) -> &[ContentPath] {
        &self.dependents
    }
}

/// Deterministic asset closure of the approved Markdown set.
///
/// Membership means only that an asset is eligible for subsequent review; it
/// does not mean that the asset is approved for publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateAssetSet {
    snapshot_id: SnapshotId,
    assets: Vec<CandidateAsset>,
}

impl CandidateAssetSet {
    #[cfg(test)]
    pub(crate) fn from_entries_for_test(
        snapshot_id: SnapshotId,
        entries: impl IntoIterator<Item = (ContentPath, Vec<ContentPath>)>,
    ) -> Self {
        let mut assets = entries
            .into_iter()
            .map(|(path, mut dependents)| {
                dependents.sort();
                dependents.dedup();
                CandidateAsset { path, dependents }
            })
            .collect::<Vec<_>>();
        assets.sort_by(|left, right| left.path.cmp(&right.path));
        Self {
            snapshot_id,
            assets,
        }
    }

    /// Selects only resolved asset edges originating from approved Markdown.
    ///
    /// This consumes existing policy and dependency results and performs no IO,
    /// parsing, resolution, policy evaluation, or review.
    pub fn select(
        policy_result: &PublicPolicyRunResult,
        graph: &AssetDependencyGraph,
    ) -> Result<Self, CandidateAssetSelectionError> {
        if policy_result.snapshot_id() != graph.snapshot_id() {
            return Err(CandidateAssetSelectionError::SnapshotMismatch {
                policy_snapshot_id: policy_result.snapshot_id(),
                graph_snapshot_id: graph.snapshot_id(),
            });
        }

        let approved = policy_result
            .approved_markdown_paths()
            .into_iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let unresolved = graph
            .problems()
            .iter()
            .filter(|problem| approved.contains(problem.document_path()))
            .cloned()
            .collect::<Vec<_>>();
        if !unresolved.is_empty() {
            return Err(
                CandidateAssetSelectionError::ApprovedDocumentsHaveDependencyProblems(unresolved),
            );
        }

        let mut closure: BTreeMap<ContentPath, BTreeSet<ContentPath>> = BTreeMap::new();
        for dependency in graph.dependencies() {
            if approved.contains(dependency.document_path()) {
                closure
                    .entry(dependency.asset_path().clone())
                    .or_default()
                    .insert(dependency.document_path().clone());
            }
        }

        let assets = closure
            .into_iter()
            .map(|(path, dependents)| CandidateAsset {
                path,
                dependents: dependents.into_iter().collect(),
            })
            .collect();

        Ok(Self {
            snapshot_id: policy_result.snapshot_id(),
            assets,
        })
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn assets(&self) -> impl ExactSizeIterator<Item = &ContentPath> {
        self.assets.iter().map(CandidateAsset::path)
    }

    pub fn entries(&self) -> &[CandidateAsset] {
        &self.assets
    }

    pub fn contains(&self, asset: &ContentPath) -> bool {
        self.assets
            .binary_search_by(|candidate| candidate.path().cmp(asset))
            .is_ok()
    }

    pub fn dependents(&self, asset: &ContentPath) -> Option<&[ContentPath]> {
        self.assets
            .binary_search_by(|candidate| candidate.path().cmp(asset))
            .ok()
            .map(|index| self.assets[index].dependents())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateAssetSelectionError {
    SnapshotMismatch {
        policy_snapshot_id: SnapshotId,
        graph_snapshot_id: SnapshotId,
    },
    ApprovedDocumentsHaveDependencyProblems(Vec<DependencyProblem>),
}

impl fmt::Display for CandidateAssetSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotMismatch {
                policy_snapshot_id,
                graph_snapshot_id,
            } => write!(
                formatter,
                "public policy snapshot {policy_snapshot_id:?} does not match asset dependency graph snapshot {graph_snapshot_id:?}"
            ),
            Self::ApprovedDocumentsHaveDependencyProblems(problems) => write!(
                formatter,
                "{} unresolved dependency problem(s) belong to approved Markdown",
                problems.len()
            ),
        }
    }
}

impl Error for CandidateAssetSelectionError {}

#[cfg(test)]
mod tests {
    use crate::{
        content::{AnalyzedMarkdown, MarkdownReferenceParser, Resolution, ResolvedReference},
        domain::{Sha256, SnapshotFile},
        policy::{
            HumanReviewReason, PolicyIdentity, ProgramCheckIssue, PublicPolicyDecision, ReviewRun,
            ReviewRunId, ReviewerError,
        },
    };

    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn snapshot_id(value: u64) -> SnapshotId {
        SnapshotId::new(value).unwrap()
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
        AnalyzedMarkdown::new(
            SnapshotFile::new(
                path(document_path),
                markdown.len() as u64,
                Sha256::digest(markdown.as_bytes()),
                None,
            ),
            resolved,
        )
    }

    fn asset(value: &str) -> Resolution {
        Resolution::ResolvedAsset { path: path(value) }
    }

    fn run(
        id: u64,
        snapshot_id: SnapshotId,
        document_path: &str,
        decision: PublicPolicyDecision,
    ) -> ReviewRun {
        ReviewRun::rehydrate(
            ReviewRunId::new(id).unwrap(),
            snapshot_id,
            path(document_path),
            Sha256::digest(document_path.as_bytes()),
            PolicyIdentity::new("public", "1", Sha256::digest(b"policy")).unwrap(),
            (decision, None),
            0,
        )
    }

    fn policy(snapshot_id: SnapshotId, outcomes: Vec<ReviewRun>) -> PublicPolicyRunResult {
        PublicPolicyRunResult::from_document_outcomes_for_test(snapshot_id, outcomes)
    }

    fn approved(id: u64, snapshot_id: SnapshotId, document_path: &str) -> ReviewRun {
        run(
            id,
            snapshot_id,
            document_path,
            PublicPolicyDecision::ReviewApproved,
        )
    }

    fn select(
        snapshot_id: SnapshotId,
        documents: &[AnalyzedMarkdown],
        outcomes: Vec<ReviewRun>,
    ) -> CandidateAssetSet {
        CandidateAssetSet::select(
            &policy(snapshot_id, outcomes),
            &AssetDependencyGraph::build(snapshot_id, documents),
        )
        .unwrap()
    }

    #[test]
    fn one_approved_document_selects_one_asset() {
        let id = snapshot_id(1);
        let document = analyzed("a.md", "![[image.png]]", vec![asset("image.png")]);

        let candidates = select(id, &[document], vec![approved(1, id, "a.md")]);

        assert_eq!(
            candidates.assets().collect::<Vec<_>>(),
            [&path("image.png")]
        );
        assert!(candidates.contains(&path("image.png")));
        assert_eq!(
            candidates.dependents(&path("image.png")).unwrap(),
            [path("a.md")]
        );
    }

    #[test]
    fn one_document_selects_multiple_assets_in_stable_order() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "[report](report.pdf) ![[image.png]]",
            vec![asset("report.pdf"), asset("image.png")],
        );

        let candidates = select(id, &[document], vec![approved(1, id, "a.md")]);

        assert_eq!(
            candidates
                .assets()
                .map(ContentPath::as_str)
                .collect::<Vec<_>>(),
            ["image.png", "report.pdf"]
        );
    }

    #[test]
    fn duplicate_references_and_shared_assets_are_deduplicated_with_all_approved_dependents() {
        let id = snapshot_id(1);
        let a = analyzed(
            "a.md",
            "![[shared.png]] ![again](shared.png)",
            vec![asset("shared.png"), asset("shared.png")],
        );
        let b = analyzed("b.md", "![[shared.png]]", vec![asset("shared.png")]);

        let candidates = select(
            id,
            &[b, a],
            vec![approved(2, id, "b.md"), approved(1, id, "a.md")],
        );

        assert_eq!(candidates.entries().len(), 1);
        assert_eq!(
            candidates.dependents(&path("shared.png")).unwrap(),
            [path("a.md"), path("b.md")]
        );
    }

    #[test]
    fn only_approved_documents_contribute_assets() {
        let id = snapshot_id(1);
        let documents = [
            analyzed("approved.md", "![[public.png]]", vec![asset("public.png")]),
            analyzed("private.md", "![[private.png]]", vec![asset("private.png")]),
            analyzed("invalid.md", "![[invalid.png]]", vec![asset("invalid.png")]),
            analyzed("issues.md", "![[issues.png]]", vec![asset("issues.png")]),
            analyzed(
                "rejected.md",
                "![[rejected.png]]",
                vec![asset("rejected.png")],
            ),
            analyzed("pending.md", "![[pending.png]]", vec![asset("pending.png")]),
            analyzed("failed.md", "![[failed.png]]", vec![asset("failed.png")]),
        ];
        let outcomes = vec![
            approved(1, id, "approved.md"),
            // Private and invalid-privacy documents are excluded before ReviewRun creation.
            run(
                2,
                id,
                "issues.md",
                PublicPolicyDecision::ProgramIssues(Vec::<ProgramCheckIssue>::new()),
            ),
            run(3, id, "rejected.md", PublicPolicyDecision::ReviewRejected),
            run(
                4,
                id,
                "pending.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            run(
                5,
                id,
                "failed.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(
                    ReviewerError::new("offline"),
                )),
            ),
        ];

        let candidates = select(id, &documents, outcomes);

        assert_eq!(
            candidates
                .assets()
                .map(ContentPath::as_str)
                .collect::<Vec<_>>(),
            ["public.png"]
        );
    }

    #[test]
    fn shared_asset_remains_when_at_least_one_dependent_is_approved() {
        let id = snapshot_id(1);
        let documents = [
            analyzed("a.md", "![[shared.png]]", vec![asset("shared.png")]),
            analyzed("b.md", "![[shared.png]]", vec![asset("shared.png")]),
        ];
        let outcomes = vec![
            approved(1, id, "a.md"),
            run(2, id, "b.md", PublicPolicyDecision::ReviewRejected),
        ];

        let candidates = select(id, &documents, outcomes);

        assert_eq!(
            candidates.assets().collect::<Vec<_>>(),
            [&path("shared.png")]
        );
        assert_eq!(
            candidates.dependents(&path("shared.png")).unwrap(),
            [path("a.md")]
        );
    }

    #[test]
    fn shared_asset_is_excluded_when_all_dependents_are_non_approved() {
        let id = snapshot_id(1);
        let documents = [
            analyzed("a.md", "![[shared.png]]", vec![asset("shared.png")]),
            analyzed("b.md", "![[shared.png]]", vec![asset("shared.png")]),
        ];
        let outcomes = vec![
            run(1, id, "a.md", PublicPolicyDecision::ReviewRejected),
            run(
                2,
                id,
                "b.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
        ];

        let candidates = select(id, &documents, outcomes);

        assert!(candidates.assets().next().is_none());
    }

    #[test]
    fn no_approved_markdown_produces_no_candidates_or_orphans() {
        let id = snapshot_id(1);
        let referenced = analyzed("a.md", "![[used.png]]", vec![asset("used.png")]);
        let candidates = select(
            id,
            &[referenced],
            vec![run(1, id, "a.md", PublicPolicyDecision::ReviewRejected)],
        );

        assert!(candidates.assets().next().is_none());
        assert!(!candidates.contains(&path("unused.png")));
    }

    #[test]
    fn approved_document_with_dependency_problem_fails_closed() {
        let id = snapshot_id(1);
        let document = analyzed(
            "a.md",
            "![[missing.png]]",
            vec![Resolution::Missing {
                target: "missing.png".to_owned(),
            }],
        );
        let graph = AssetDependencyGraph::build(id, &[document]);

        let error = CandidateAssetSet::select(&policy(id, vec![approved(1, id, "a.md")]), &graph)
            .unwrap_err();

        assert!(matches!(
            error,
            CandidateAssetSelectionError::ApprovedDocumentsHaveDependencyProblems(problems)
                if problems.len() == 1 && problems[0].document_path() == &path("a.md")
        ));
    }

    #[test]
    fn dependency_problem_on_non_approved_document_does_not_block_selection() {
        let id = snapshot_id(1);
        let approved_document = analyzed("a.md", "![[image.png]]", vec![asset("image.png")]);
        let rejected_document = analyzed(
            "b.md",
            "![[missing.png]]",
            vec![Resolution::Missing {
                target: "missing.png".to_owned(),
            }],
        );
        let outcomes = vec![
            approved(1, id, "a.md"),
            run(2, id, "b.md", PublicPolicyDecision::ReviewRejected),
        ];

        let candidates = select(id, &[rejected_document, approved_document], outcomes);

        assert_eq!(
            candidates.assets().collect::<Vec<_>>(),
            [&path("image.png")]
        );
    }

    #[test]
    fn snapshot_mismatch_is_rejected() {
        let policy_id = snapshot_id(1);
        let graph_id = snapshot_id(2);
        let graph = AssetDependencyGraph::build(graph_id, &[]);

        let error = CandidateAssetSet::select(&policy(policy_id, vec![]), &graph).unwrap_err();

        assert_eq!(
            error,
            CandidateAssetSelectionError::SnapshotMismatch {
                policy_snapshot_id: policy_id,
                graph_snapshot_id: graph_id,
            }
        );
    }

    #[test]
    fn output_is_independent_of_document_and_policy_input_order() {
        let id = snapshot_id(1);
        let a = analyzed(
            "a.md",
            "![[z.png]] ![[shared.png]]",
            vec![asset("z.png"), asset("shared.png")],
        );
        let b = analyzed(
            "b.md",
            "![[shared.png]] ![[a.png]]",
            vec![asset("shared.png"), asset("a.png")],
        );
        let first = select(
            id,
            &[a.clone(), b.clone()],
            vec![approved(1, id, "a.md"), approved(2, id, "b.md")],
        );
        let second = select(
            id,
            &[b, a],
            vec![approved(2, id, "b.md"), approved(1, id, "a.md")],
        );

        assert_eq!(first, second);
        assert_eq!(
            first.assets().map(ContentPath::as_str).collect::<Vec<_>>(),
            ["a.png", "shared.png", "z.png"]
        );
    }
}
