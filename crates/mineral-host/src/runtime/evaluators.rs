//! Host review-execution strategies.
//!
//! The portable engine defines the execution seam (`MarkdownReviewEvaluator`,
//! `AssetReviewEvaluator`) and a sequential default. This module supplies the
//! native strategy: bounded parallel review on worker threads, with input order
//! restored. A Cloudflare runtime would supply its own implementation of the
//! same traits instead.

use crate::{
    policy::{PublicCandidateMarkdown, PublicPolicy, PublicPolicyOutcome, Reviewer},
    workflow::{
        AssetPolicyOutcome, AssetReviewEvaluator, AssetReviewOutcome, AssetReviewer,
        MarkdownReviewEvaluator,
    },
};

use super::bounded_map;

/// Native Markdown review execution strategy.
///
/// `Bounded` requires a `Sync` reviewer; `Sequential` accepts any reviewer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostMarkdownReviews {
    Sequential,
    Bounded(usize),
}

impl HostMarkdownReviews {
    /// Chooses the strategy for a configured concurrency limit.
    pub fn for_concurrency(concurrency: usize) -> Self {
        if concurrency > 1 {
            Self::Bounded(concurrency)
        } else {
            Self::Sequential
        }
    }
}

impl<R: Reviewer + Sync + ?Sized> MarkdownReviewEvaluator<R> for HostMarkdownReviews {
    fn is_bounded(&self) -> bool {
        matches!(self, Self::Bounded(_))
    }

    fn evaluate(
        &self,
        candidates: Vec<PublicCandidateMarkdown>,
        reviewer: &R,
    ) -> Vec<PublicPolicyOutcome> {
        match self {
            Self::Sequential => PublicPolicy::evaluate(candidates, reviewer),
            Self::Bounded(limit) => bounded_map(candidates, *limit, &|candidate| {
                PublicPolicy::evaluate(vec![candidate], reviewer)
                    .pop()
                    .expect("one candidate produces one policy outcome")
            }),
        }
    }
}

/// Native asset review execution strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostAssetReviews {
    Sequential,
    Bounded(usize),
}

impl HostAssetReviews {
    /// Chooses the strategy for a configured concurrency limit.
    pub fn for_concurrency(concurrency: usize) -> Self {
        if concurrency > 1 {
            Self::Bounded(concurrency)
        } else {
            Self::Sequential
        }
    }
}

impl<R: AssetReviewer + Sync + ?Sized> AssetReviewEvaluator<R> for HostAssetReviews {
    fn is_bounded(&self) -> bool {
        matches!(self, Self::Bounded(_))
    }

    fn evaluate(&self, outcomes: Vec<AssetPolicyOutcome>, reviewer: &R) -> Vec<AssetReviewOutcome> {
        match self {
            Self::Sequential => outcomes
                .into_iter()
                .map(|outcome| outcome.review(reviewer))
                .collect(),
            Self::Bounded(limit) => {
                bounded_map(outcomes, *limit, &|outcome| outcome.review(reviewer))
            }
        }
    }
}
