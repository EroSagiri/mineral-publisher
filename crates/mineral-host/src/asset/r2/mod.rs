//! An object-storage asset target over the S3-compatible HTTP API.
//!
//! This is the runtime adapter for a remote bucket: it reports facts about the
//! objects it holds and places frozen representations there. It decides nothing:
//! the object key, the media type, the size and the byte identity all come from
//! the frozen [`PublishedAsset`] it is handed, and whether those facts satisfy the
//! asset is decided by the engine.
//!
//! Two properties are worth stating explicitly because they are easy to get wrong:
//!
//! * A store's ETag is **not** a content hash, so it is never reported as a byte
//!   identity. An inspection reads the object's bytes and hashes them.
//! * Nothing is uploaded before the driver has verified the bytes it streamed, and
//!   the request itself is only sent from `finish`, so a stream that does not match
//!   the frozen representation never reaches the bucket.
//! * An upload is a create (`if-none-match: *`), not a replace: if anything
//!   appeared under the frozen key after the inspection that found it absent, the
//!   bucket refuses the write instead of accepting an object under a name that may
//!   no longer describe it.

mod signature;
#[cfg(test)]
mod tests;
mod transport;

pub use transport::{
    R2ObjectStore, R2ObjectStoreConfig, R2ObjectStoreConfigError, R2ObjectStoreError, R2SecretKey,
};
