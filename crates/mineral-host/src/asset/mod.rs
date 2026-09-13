//! Native asset-target adapters.
//!
//! The engine's asset boundary is [`mineral_core::publication::asset`]: a frozen
//! object key, frozen publication facts, and a port that reports what a target
//! holds. This module supplies the native runtime's implementation — a local
//! object store — plus the observation allocators the composition root binds.
//!
//! A Cloudflare runtime implements the same port over object storage; nothing in
//! the engine changes when it does.

mod filesystem;

use std::error::Error;

pub use filesystem::{FilesystemAssetTarget, FilesystemAssetTargetError};
pub use mineral_core::publication::asset::{
    AssetByteIdentity, AssetContentError, AssetObservationId, AssetObservationIdError,
    AssetObservationIdGenerator, AssetObservationStore, AssetPublication, AssetPublicationError,
    AssetPublicationOutcome, AssetTarget, AssetTargetConflict, AssetTargetFacts,
    AssetTargetObservation, AssetTargetObservationError, AssetTargetState, AssetVerification,
    VerifiedAssetContent,
};

/// A deterministic caller-owned asset-observation allocator for tests.
#[derive(Clone, Debug)]
pub struct SequentialAssetObservationIdGenerator {
    next: Option<u64>,
}

impl SequentialAssetObservationIdGenerator {
    pub fn new(first: AssetObservationId) -> Self {
        Self {
            next: Some(first.get()),
        }
    }
}

impl AssetObservationIdGenerator for SequentialAssetObservationIdGenerator {
    type Error = SequentialAssetObservationIdGeneratorError;

    fn next_id(&mut self) -> Result<AssetObservationId, Self::Error> {
        let value = self
            .next
            .ok_or(SequentialAssetObservationIdGeneratorError::Exhausted)?;
        self.next = value.checked_add(1);
        AssetObservationId::new(value)
            .map_err(|_| SequentialAssetObservationIdGeneratorError::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequentialAssetObservationIdGeneratorError {
    Exhausted,
}

impl std::fmt::Display for SequentialAssetObservationIdGeneratorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("asset observation ID sequence is exhausted")
    }
}

impl Error for SequentialAssetObservationIdGeneratorError {}

/// Production asset-observation allocator backed by UUID v4 entropy.
///
/// The durable schema uses positive `u64` ids, so a UUID v4 is mapped to its
/// first non-zero 63 bits, exactly like the publish-run and remote-observation
/// allocators.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidAssetObservationIdGenerator;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UuidAssetObservationIdGeneratorError {
    InvalidGeneratedId,
}

impl std::fmt::Display for UuidAssetObservationIdGeneratorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UUID-based asset observation ID was invalid")
    }
}

impl Error for UuidAssetObservationIdGeneratorError {}

impl AssetObservationIdGenerator for UuidAssetObservationIdGenerator {
    type Error = UuidAssetObservationIdGeneratorError;

    fn next_id(&mut self) -> Result<AssetObservationId, Self::Error> {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        let mut prefix = [0_u8; 8];
        prefix.copy_from_slice(&bytes[..8]);
        let value = (u64::from_be_bytes(prefix) & i64::MAX as u64).max(1);
        AssetObservationId::new(value)
            .map_err(|_| UuidAssetObservationIdGeneratorError::InvalidGeneratedId)
    }
}
