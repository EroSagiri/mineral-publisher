//! The low-level, domain-free half of the S3-compatible object-storage runtime.
//!
//! Two adapters talk to the same kind of endpoint for opposite reasons: the asset
//! target writes published objects, and the source reader reads remote source
//! objects. They share credentials, endpoint configuration, URL encoding and
//! Signature Version 4 — exactly the parts that describe *the connection* — and
//! nothing else. Neither adapter's domain types appear here, and this module knows
//! nothing about assets, manifests, sources, snapshots or publications.

mod config;
mod request;
mod signature;

pub use config::{R2ObjectStoreConfig, R2ObjectStoreConfigError, R2SecretKey, R2TransportError};
pub use request::{SignedRequestSpec, empty_payload_sha256, request_url, signed_request_builder};
pub use signature::payload_sha256;

pub(crate) use config::{encode_path, encode_query_value};
