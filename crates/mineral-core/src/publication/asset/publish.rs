use std::{error::Error, fmt};

use crate::{
    domain::{ContentPath, TimestampMillis},
    ports::{BlobStore, Clock, ContentStoreError},
    publish::PublishRun,
    workflow::{DeliveryProjection, PublishedAsset},
};

use super::{
    AssetContentError, AssetObservationIdGenerator, AssetObservationStore, AssetTarget,
    AssetTargetConflict, AssetTargetObservation, AssetTargetState, AssetVerification,
};

/// What the asset side of one publication attempt did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetPublicationOutcome {
    /// The intent binds no durable delivery projection, so there is no asset set
    /// to satisfy. The Git-side result is the whole result.
    NoDurableProjection,
    /// The Git side was already unreachable, so nothing was inspected: placing
    /// objects for an attempt that cannot succeed would only leave pointless
    /// staged data behind.
    NotAttempted,
    /// Every required asset is verified on the target. `published` counts the ones
    /// this attempt had to place; `verified` counts the whole required set.
    Satisfied { verified: usize, published: usize },
}

impl AssetPublicationOutcome {
    /// Whether the asset side of the delivery holds nothing outstanding.
    pub fn is_satisfied(&self) -> bool {
        matches!(self, Self::Satisfied { .. } | Self::NoDurableProjection)
    }

    pub fn verified(&self) -> usize {
        match self {
            Self::Satisfied { verified, .. } => *verified,
            Self::NoDurableProjection | Self::NotAttempted => 0,
        }
    }

    pub fn published(&self) -> usize {
        match self {
            Self::Satisfied { published, .. } => *published,
            Self::NoDurableProjection | Self::NotAttempted => 0,
        }
    }
}

/// Places and verifies every asset one delivery projection requires.
///
/// This is the portable asset-publication sequence. [`AssetTarget`] is the port;
/// this type is the engine that decides when to inspect, when to publish, when a
/// target fact is enough, and when it authorizes the next Git side effect.
///
/// The sequence for one required asset is always: inspect, persist the observed
/// fact, judge it, and — only when the object is missing — read the frozen
/// representation from the immutable content store, prove its identity, place it,
/// inspect again, persist that fact, and judge it again. A target that reports
/// success is never trusted on its word.
#[derive(Clone, Copy, Debug, Default)]
pub struct AssetPublication;

impl AssetPublication {
    /// Ensures the required assets of one delivery projection.
    ///
    /// Assets are handled in the projection's canonical order, so a partial
    /// failure leaves a deterministic prefix placed and every later asset
    /// untouched.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn ensure_required<T, S, G, B, C>(
        run: &PublishRun,
        delivery: Option<&DeliveryProjection>,
        target: &T,
        observations: &S,
        observation_ids: &mut G,
        blobs: &B,
        clock: &C,
    ) -> Result<AssetPublicationOutcome, AssetPublicationError<T::Error, S::Error, G::Error>>
    where
        T: AssetTarget,
        S: AssetObservationStore,
        G: AssetObservationIdGenerator,
        B: BlobStore,
        C: Clock,
    {
        let Some(delivery) = delivery else {
            return Ok(AssetPublicationOutcome::NoDurableProjection);
        };

        let mut verified = 0;
        let mut published = 0;
        for asset in delivery.assets().assets() {
            let observed_at = clock.now().ok_or(AssetPublicationError::ClockUnavailable)?;
            let state = target
                .inspect(asset.object_key())
                .map_err(AssetPublicationError::TargetInspect)?;
            Self::record::<T, S, G>(
                run,
                delivery,
                asset,
                state.clone(),
                observed_at,
                observations,
                observation_ids,
            )?;

            match asset.judge(&state) {
                AssetVerification::Ready => {
                    verified += 1;
                    continue;
                }
                AssetVerification::Conflict(conflict) => {
                    return Err(AssetPublicationError::Conflict {
                        logical_path: asset.logical_path().clone(),
                        conflict,
                    });
                }
                AssetVerification::Unverifiable => {
                    return Err(AssetPublicationError::Unverifiable {
                        logical_path: asset.logical_path().clone(),
                    });
                }
                AssetVerification::Missing => {}
            }

            // The object is missing, so the frozen representation must be placed.
            // Only bytes this engine verified itself are ever published, and they
            // are the same bytes the target receives.
            let bytes = blobs.read(asset.published_sha256()).map_err(|source| {
                AssetPublicationError::BlobRead {
                    logical_path: asset.logical_path().clone(),
                    source,
                }
            })?;
            let content =
                asset
                    .verify_bytes(&bytes)
                    .map_err(|source| AssetPublicationError::Content {
                        logical_path: asset.logical_path().clone(),
                        source,
                    })?;
            target
                .publish(content)
                .map_err(AssetPublicationError::TargetPublish)?;
            published += 1;

            // A target reporting success is not evidence, so the object is
            // inspected and judged again before it can authorize anything.
            let observed_at = clock.now().ok_or(AssetPublicationError::ClockUnavailable)?;
            let state = target
                .inspect(asset.object_key())
                .map_err(AssetPublicationError::TargetInspect)?;
            Self::record::<T, S, G>(
                run,
                delivery,
                asset,
                state.clone(),
                observed_at,
                observations,
                observation_ids,
            )?;
            match asset.judge(&state) {
                AssetVerification::Ready => verified += 1,
                AssetVerification::Missing => {
                    return Err(AssetPublicationError::PublishedObjectMissing {
                        logical_path: asset.logical_path().clone(),
                    });
                }
                AssetVerification::Conflict(conflict) => {
                    return Err(AssetPublicationError::Conflict {
                        logical_path: asset.logical_path().clone(),
                        conflict,
                    });
                }
                AssetVerification::Unverifiable => {
                    return Err(AssetPublicationError::Unverifiable {
                        logical_path: asset.logical_path().clone(),
                    });
                }
            }
        }

        Ok(AssetPublicationOutcome::Satisfied {
            verified,
            published,
        })
    }

    /// Persists one asset-target fact before anything is allowed to depend on it.
    #[allow(clippy::type_complexity)]
    fn record<T, S, G>(
        run: &PublishRun,
        delivery: &DeliveryProjection,
        asset: &PublishedAsset,
        observed: AssetTargetState,
        observed_at: TimestampMillis,
        observations: &S,
        observation_ids: &mut G,
    ) -> Result<(), AssetPublicationError<T::Error, S::Error, G::Error>>
    where
        T: AssetTarget,
        S: AssetObservationStore,
        G: AssetObservationIdGenerator,
    {
        let id = observation_ids
            .next_id()
            .map_err(AssetPublicationError::ObservationId)?;
        let observation = AssetTargetObservation::new(
            id,
            run,
            delivery.delivery_sha256(),
            asset,
            observed,
            observed_at,
        )
        .map_err(AssetPublicationError::Observation)?;
        observations
            .save(&observation)
            .map_err(AssetPublicationError::ObservationPersistence)
    }
}

/// Errors that stopped the asset side of one publication attempt.
#[derive(Debug)]
pub enum AssetPublicationError<T: Error, S: Error, G: Error> {
    /// The runtime could not inspect the asset target.
    TargetInspect(T),
    /// The runtime could not place the verified representation.
    TargetPublish(T),
    /// The immutable content store could not produce the published bytes.
    BlobRead {
        logical_path: ContentPath,
        source: ContentStoreError,
    },
    /// The bytes the content store returned are not the frozen representation.
    Content {
        logical_path: ContentPath,
        source: AssetContentError,
    },
    /// The frozen object key already holds different content.
    Conflict {
        logical_path: ContentPath,
        conflict: AssetTargetConflict,
    },
    /// The target cannot prove which bytes it serves.
    Unverifiable {
        logical_path: ContentPath,
    },
    /// The object was published, and the target does not show it afterwards.
    PublishedObjectMissing {
        logical_path: ContentPath,
    },
    /// The observed facts cannot be recorded against the frozen asset.
    Observation(super::AssetTargetObservationError),
    ObservationId(G),
    ObservationPersistence(S),
    ClockUnavailable,
}

impl<T: Error, S: Error, G: Error> fmt::Display for AssetPublicationError<T, S, G> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetInspect(error) => {
                write!(formatter, "could not inspect asset target: {error}")
            }
            Self::TargetPublish(error) => write!(formatter, "could not publish asset: {error}"),
            Self::BlobRead { logical_path, .. } => {
                write!(
                    formatter,
                    "could not read published bytes for {logical_path}"
                )
            }
            Self::Content { logical_path, .. } => {
                write!(
                    formatter,
                    "published bytes for {logical_path} are not the frozen representation"
                )
            }
            Self::Conflict {
                logical_path,
                conflict,
            } => write!(
                formatter,
                "asset target holds conflicting content under the frozen key for {logical_path}: {conflict:?}"
            ),
            Self::Unverifiable { logical_path } => write!(
                formatter,
                "asset target cannot verify the bytes it serves for {logical_path}"
            ),
            Self::PublishedObjectMissing { logical_path } => write!(
                formatter,
                "asset {logical_path} was published but is not present afterwards"
            ),
            Self::Observation(error) => {
                write!(
                    formatter,
                    "could not record the observed asset fact: {error}"
                )
            }
            Self::ObservationId(_) => {
                formatter.write_str("could not allocate asset observation ID")
            }
            Self::ObservationPersistence(_) => {
                formatter.write_str("asset target observation could not be persisted")
            }
            Self::ClockUnavailable => {
                formatter.write_str("clock reading cannot be recorded as an observation timestamp")
            }
        }
    }
}

impl<T: Error + 'static, S: Error + 'static, G: Error + 'static> Error
    for AssetPublicationError<T, S, G>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TargetInspect(error) | Self::TargetPublish(error) => Some(error),
            Self::BlobRead { source, .. } => Some(source),
            Self::Content { source, .. } => Some(source),
            Self::ObservationId(error) => Some(error),
            Self::ObservationPersistence(error) => Some(error),
            Self::Observation(error) => Some(error),
            Self::Conflict { .. }
            | Self::Unverifiable { .. }
            | Self::PublishedObjectMissing { .. }
            | Self::ClockUnavailable => None,
        }
    }
}
