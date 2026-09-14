use std::{error::Error, fmt};

use crate::{domain::Sha256, publication::asset::ImmutableBlobSource};

/// The pointer version string Git LFS v1 defines.
pub const LFS_POINTER_VERSION: &str = "https://git-lfs.github.com/spec/v1";

/// One LFS object a backup requires, identified by content alone.
///
/// There is deliberately no filename here: an LFS object's identity is exactly the
/// SHA-256 of its bytes plus its size, and the same bytes referenced from two paths
/// are one object. The path lives in the Git tree, not in LFS.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequiredLfsObject {
    oid: Sha256,
    size: u64,
}

impl Ord for RequiredLfsObject {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.oid
            .as_bytes()
            .cmp(other.oid.as_bytes())
            .then(self.size.cmp(&other.size))
    }
}

impl PartialOrd for RequiredLfsObject {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl RequiredLfsObject {
    /// Invariant 1: `oid` is the SHA-256 of the original bytes, which is also the
    /// Mineral CAS identity of the Snapshot file — never a second binary identity.
    /// Invariant 2: `size` is the Snapshot file size, frozen with the object.
    pub fn new(oid: Sha256, size: u64) -> Self {
        Self { oid, size }
    }

    pub fn oid(&self) -> Sha256 {
        self.oid
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

/// One required object together with the upload instruction a runtime handed back.
#[derive(Debug)]
pub struct LfsUpload<UploadAction, VerifyAction> {
    object: RequiredLfsObject,
    action: UploadAction,
    verify: Option<VerifyAction>,
}

impl<U, V> LfsUpload<U, V> {
    pub fn new(object: RequiredLfsObject, action: U, verify: Option<V>) -> Self {
        Self {
            object,
            action,
            verify,
        }
    }

    pub fn object(&self) -> RequiredLfsObject {
        self.object
    }

    pub fn action(&self) -> &U {
        &self.action
    }

    pub fn verify(&self) -> Option<&V> {
        self.verify.as_ref()
    }
}

/// What one batch request answered: what the remote already holds, and what it wants.
#[derive(Debug)]
pub struct LfsUploadPlan<U, V> {
    present: Vec<RequiredLfsObject>,
    uploads: Vec<LfsUpload<U, V>>,
}

impl<U, V> LfsUploadPlan<U, V> {
    pub fn new(present: Vec<RequiredLfsObject>, uploads: Vec<LfsUpload<U, V>>) -> Self {
        Self { present, uploads }
    }

    pub fn present(&self) -> &[RequiredLfsObject] {
        &self.present
    }

    pub fn uploads(&self) -> &[LfsUpload<U, V>] {
        &self.uploads
    }

    pub fn is_complete(&self) -> bool {
        self.uploads.is_empty()
    }

    /// The objects the remote does not have yet.
    pub fn missing(&self) -> Vec<RequiredLfsObject> {
        self.uploads.iter().map(|upload| upload.object).collect()
    }
}

/// The runtime side of a Git LFS endpoint.
///
/// Core owns the facts and the order: which objects a commit requires, that upload
/// happens before the ref moves, and that a corrupted or short stream is a failure.
/// The runtime owns the protocol: discovery, HTTP, signed URLs, credentials. None of
/// that — no URL, header, token or SDK type — may appear in the engine.
pub trait LfsRemote {
    type Error: Error + 'static;

    /// The opaque upload instruction one runtime understands (an href plus headers).
    type UploadAction;

    /// The opaque verify instruction one runtime understands.
    type VerifyAction;

    /// Asks the endpoint which of these objects it already holds.
    ///
    /// Reporting an object as present is a claim the endpoint makes; the engine
    /// still refuses to move a ref while any required object is missing from this
    /// answer.
    fn prepare_upload(
        &self,
        objects: &[RequiredLfsObject],
    ) -> Result<LfsUploadPlan<Self::UploadAction, Self::VerifyAction>, Self::Error>;

    /// Streams one object to the endpoint from an immutable blob source.
    ///
    /// The runtime must hash and count the exact bytes it sends and fail the upload
    /// when they do not equal `object`, so a damaged CAS blob can never become a
    /// remote object that claims to be it.
    fn upload(
        &self,
        object: &RequiredLfsObject,
        source: &mut dyn ImmutableBlobSource,
        action: &Self::UploadAction,
    ) -> Result<(), Self::Error>;

    /// Performs the endpoint's own verify action, when the batch answer asked for one.
    fn verify(
        &self,
        object: &RequiredLfsObject,
        action: &Self::VerifyAction,
    ) -> Result<(), Self::Error>;
}

/// Renders the exact bytes of an LFS v1 pointer.
///
/// The format is fixed: one version line, one `oid sha256:<64 lowercase hex>` line,
/// one `size <decimal>` line, and exactly one trailing newline. The pointer is a
/// pure function of the object it describes, so the same object always produces the
/// same pointer bytes and therefore the same CAS identity.
pub fn build_lfs_pointer(oid: Sha256, size: u64) -> Vec<u8> {
    format!("version {LFS_POINTER_VERSION}\noid sha256:{oid}\nsize {size}\n").into_bytes()
}

/// One parsed LFS pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LfsPointer {
    oid: Sha256,
    size: u64,
}

impl LfsPointer {
    pub fn oid(&self) -> Sha256 {
        self.oid
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

/// Parses the pointer bytes a backup tree holds for one binary path.
///
/// Strict on purpose: an extra line, a different version, a digest that is not 64
/// lowercase hex, or a size that does not parse is a damaged backup, not a pointer
/// to be interpreted generously.
pub fn parse_lfs_pointer(bytes: &[u8]) -> Result<LfsPointer, LfsPointerError> {
    let text = std::str::from_utf8(bytes).map_err(|_| LfsPointerError::NotText)?;
    let mut lines = text.split('\n');
    let version = lines.next().ok_or(LfsPointerError::Damaged)?;
    match version.strip_prefix("version ") {
        Some(version) if version == LFS_POINTER_VERSION => {}
        Some(_) => return Err(LfsPointerError::UnknownVersion),
        None => return Err(LfsPointerError::Damaged),
    }
    let oid_line = lines.next().ok_or(LfsPointerError::Damaged)?;
    let oid = oid_line
        .strip_prefix("oid sha256:")
        .ok_or(LfsPointerError::Damaged)?;
    if oid.len() != 64
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(LfsPointerError::Damaged);
    }
    let mut digest = [0_u8; 32];
    for (index, chunk) in oid.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(chunk).map_err(|_| LfsPointerError::Damaged)?;
        digest[index] = u8::from_str_radix(text, 16).map_err(|_| LfsPointerError::Damaged)?;
    }
    let size_line = lines.next().ok_or(LfsPointerError::Damaged)?;
    let size = size_line
        .strip_prefix("size ")
        .ok_or(LfsPointerError::Damaged)?
        .parse::<u64>()
        .map_err(|_| LfsPointerError::Damaged)?;
    if lines.next() != Some("") || lines.next().is_some() {
        return Err(LfsPointerError::Damaged);
    }
    Ok(LfsPointer {
        oid: Sha256::new(digest),
        size,
    })
}

/// Why pointer bytes are not a usable LFS pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LfsPointerError {
    NotText,
    Damaged,
    UnknownVersion,
}

impl fmt::Display for LfsPointerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotText => formatter.write_str("LFS pointer is not valid UTF-8"),
            Self::Damaged => formatter.write_str("LFS pointer is damaged"),
            Self::UnknownVersion => formatter.write_str("LFS pointer uses an unknown version"),
        }
    }
}

impl Error for LfsPointerError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: u8) -> Sha256 {
        Sha256::new([seed; 32])
    }

    #[test]
    fn a_pointer_has_the_exact_v1_shape_and_one_trailing_newline() {
        let oid = Sha256::digest(b"original bytes");

        let pointer = build_lfs_pointer(oid, 20_971_520);

        assert_eq!(
            String::from_utf8(pointer.clone()).unwrap(),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 20971520\n"
            )
        );
        assert!(pointer.ends_with(b"\n"));
        assert!(!pointer.ends_with(b"\n\n"));
    }

    #[test]
    fn pointer_generation_is_deterministic_and_self_consistent() {
        let object = RequiredLfsObject::new(digest(7), 12_345);

        let first = build_lfs_pointer(object.oid(), object.size());
        let second = build_lfs_pointer(object.oid(), object.size());

        assert_eq!(first, second);
        let parsed = parse_lfs_pointer(&first).unwrap();
        // Invariants 1 and 2: the pointer names the Snapshot bytes and their size.
        assert_eq!(parsed.oid(), object.oid());
        assert_eq!(parsed.size(), object.size());
    }

    #[test]
    fn a_pointer_for_a_zero_byte_object_is_still_a_pointer() {
        let pointer = build_lfs_pointer(Sha256::digest(b""), 0);

        assert!(
            String::from_utf8(pointer.clone())
                .unwrap()
                .contains("size 0\n")
        );
        assert_eq!(parse_lfs_pointer(&pointer).unwrap().size(), 0);
    }

    #[test]
    fn a_damaged_pointer_fails_closed() {
        for damaged in [
            &b"version https://git-lfs.github.com/spec/v1\noid sha256:abcd\nsize 1\n"[..],
            b"version https://example.invalid/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nsize 1\n",
            b"version https://git-lfs.github.com/spec/v1\noid sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\nsize 1\n",
            b"version https://git-lfs.github.com/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nsize one\n",
            b"version https://git-lfs.github.com/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            b"version https://git-lfs.github.com/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nsize 1\nextra\n",
            b"not a pointer at all",
            &[0xff, 0xfe, 0x00],
        ] {
            assert!(
                parse_lfs_pointer(damaged).is_err(),
                "{damaged:?} was accepted"
            );
        }
    }
}
