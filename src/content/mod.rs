mod asset_dependency;
mod markdown_analysis;
mod reference;
mod reference_resolver;

pub(crate) use asset_dependency::dependency_problems;
pub use asset_dependency::{
    AssetDependency, AssetDependencyGraph, DependencyProblem, DependencyProblemKind,
    ReferenceOrigin,
};
pub use markdown_analysis::{
    AnalyzedMarkdown, ResolvedReference, SnapshotMarkdownAnalysisError, SnapshotMarkdownAnalyzer,
};
pub use reference::{MarkdownReferenceParser, Reference, ReferenceKind};
pub use reference_resolver::{
    InvalidResolutionReason, ReferenceResolver, Resolution, ResolutionCandidate, ResolvedTargetKind,
};
