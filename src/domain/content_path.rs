use std::{error::Error, fmt};

/// A canonical, vault-relative path.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentPath(String);

impl ContentPath {
    pub fn new(path: impl Into<String>) -> Result<Self, ContentPathError> {
        let path = path.into();

        if path.is_empty() {
            return Err(ContentPathError::Empty);
        }
        let bytes = path.as_bytes();
        let has_windows_drive_prefix =
            bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if path.starts_with('/') || path.contains(['\\', '\0']) || has_windows_drive_prefix {
            return Err(ContentPathError::NotCanonical);
        }
        if path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(ContentPathError::NotCanonical);
        }

        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentPathError {
    Empty,
    NotCanonical,
}

impl fmt::Display for ContentPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("content path cannot be empty"),
            Self::NotCanonical => formatter
                .write_str("content path must be a canonical relative path using forward slashes"),
        }
    }
}

impl Error for ContentPathError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_relative_path() {
        let path = ContentPath::new("notes/第一天.md").unwrap();

        assert_eq!(path.as_str(), "notes/第一天.md");
    }

    #[test]
    fn rejects_paths_that_can_escape_or_have_multiple_representations() {
        for path in [
            "",
            "/note.md",
            "C:/note.md",
            "notes\\note.md",
            "a/../note.md",
            "a//note.md",
        ] {
            assert!(ContentPath::new(path).is_err(), "accepted {path:?}");
        }
    }
}
