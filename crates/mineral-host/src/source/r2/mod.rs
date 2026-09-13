//! An R2-backed source reader.
//!
//! The reader owns exactly three things: listing one managed prefix, reading the
//! remote revisions a listing observed, and recording which revision produced which
//! content identity. It does not decide public scope, privacy, review, delivery or
//! publication — those stages run on the Snapshot it helped assemble, exactly as
//! they do for a local source.
//!
//! Two invariants are worth stating where the code is:
//!
//! * A remote validator (ETag, last-modified, size) is a *revision*, never a
//!   content identity. The content identity of every file is computed by this
//!   engine from the bytes it actually read into the content-addressed store.
//! * A listing never becomes a Snapshot on its own. Only a stabilized inventory
//!   whose every entry has a verified materialization can be assembled.

mod list;
mod prefix;
mod reader;
mod revision;

#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod tests;

pub use list::{ListPage, ListPageError, ListedObject, parse_list_page};
pub use prefix::{R2SourceKeyError, R2SourcePrefix, R2SourcePrefixError};
pub use reader::{
    DEFAULT_LIST_PAGE_SIZE, MAX_LIST_BODY_BYTES, MAX_SOURCE_OBJECT_BYTES, R2Source, R2SourceError,
    SOURCE_CHUNK_BYTES,
};
pub use revision::{R2ObjectRevision, R2ObjectRevisionError};
