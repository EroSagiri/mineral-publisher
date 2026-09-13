//! Platform ports: the capabilities the portable engine needs from a runtime.
//!
//! Ports are defined around domain needs (blobs by content identity, review
//! attempt persistence, publication) rather than around any specific SDK.

mod blob;
mod clock;

pub use blob::{BlobStore, BlobWriter, ContentStoreError, StoredBlob};
pub use clock::Clock;
