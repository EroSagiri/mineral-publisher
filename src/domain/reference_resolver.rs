use super::{ContentPath, Reference, ReferenceKind, Snapshot};

/// The concrete type of a local Snapshot target.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResolvedTargetKind {
    Note,
    Asset,
}

/// A deterministically ordered candidate retained for an ambiguous reference.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ResolutionCandidate {
    kind: ResolvedTargetKind,
    path: ContentPath,
}

impl ResolutionCandidate {
    pub fn kind(&self) -> ResolvedTargetKind {
        self.kind
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidResolutionReason {
    InvalidLocalPath,
    EscapesContentRoot,
    MarkdownImageTargetsNote,
}

/// The complete outcome of resolving one parsed reference against a Snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolution {
    ResolvedNote {
        path: ContentPath,
    },
    ResolvedAsset {
        path: ContentPath,
    },
    External {
        target: String,
    },
    Missing {
        target: String,
    },
    Ambiguous {
        target: String,
        candidates: Vec<ResolutionCandidate>,
    },
    Invalid {
        target: String,
        reason: InvalidResolutionReason,
    },
}

/// Pure resolver whose only view of local content is the supplied Snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReferenceResolver;

impl ReferenceResolver {
    pub fn resolve(
        reference: &Reference,
        source_path: &ContentPath,
        snapshot: &Snapshot,
    ) -> Resolution {
        let target = reference.target();
        if reference.kind() == ReferenceKind::ExternalUrl || is_external(target) {
            return Resolution::External {
                target: target.to_owned(),
            };
        }

        match reference.kind() {
            ReferenceKind::WikiLink => resolve_wiki_link(target, source_path, snapshot),
            ReferenceKind::WikiEmbed => resolve_wiki_embed(target, snapshot),
            ReferenceKind::MarkdownLink => {
                resolve_markdown_path(target, source_path, snapshot, false)
            }
            ReferenceKind::MarkdownImage => {
                resolve_markdown_path(target, source_path, snapshot, true)
            }
            ReferenceKind::ExternalUrl => unreachable!("handled above"),
        }
    }
}

fn resolve_wiki_link(target: &str, source_path: &ContentPath, snapshot: &Snapshot) -> Resolution {
    let file_target = strip_fragment(target);
    if file_target.is_empty() {
        return resolve_exact(target, source_path.clone(), snapshot, false);
    }

    let candidate_name = if file_target.ends_with(".md") {
        file_target.to_owned()
    } else {
        format!("{file_target}.md")
    };

    if file_target.contains('/') {
        let Ok(path) = ContentPath::new(candidate_name) else {
            return invalid_path(target);
        };
        return resolve_exact(target, path, snapshot, false);
    }

    resolve_candidates(
        target,
        snapshot
            .files()
            .iter()
            .filter(|file| file_name(file.path()) == candidate_name)
            .map(|file| candidate(file.path().clone())),
    )
}

fn resolve_wiki_embed(target: &str, snapshot: &Snapshot) -> Resolution {
    let file_target = strip_fragment(target);
    if file_target.is_empty() {
        return invalid_path(target);
    }

    if file_target.contains('/') {
        let mut paths = Vec::with_capacity(2);
        let Ok(exact) = ContentPath::new(file_target) else {
            return invalid_path(target);
        };
        paths.push(exact);
        if !has_extension(file_target) {
            let Ok(note) = ContentPath::new(format!("{file_target}.md")) else {
                return invalid_path(target);
            };
            paths.push(note);
        }
        return resolve_candidates(
            target,
            snapshot
                .files()
                .iter()
                .filter(|file| paths.contains(file.path()))
                .map(|file| candidate(file.path().clone())),
        );
    }

    resolve_candidates(
        target,
        snapshot
            .files()
            .iter()
            .filter(|file| {
                let name = file_name(file.path());
                name == file_target
                    || (!has_extension(file_target) && name == format!("{file_target}.md"))
            })
            .map(|file| candidate(file.path().clone())),
    )
}

fn resolve_markdown_path(
    target: &str,
    source_path: &ContentPath,
    snapshot: &Snapshot,
    image: bool,
) -> Resolution {
    let file_target = strip_fragment(target);
    let path = if file_target.is_empty() {
        source_path.clone()
    } else {
        match normalize_relative(source_path, file_target) {
            Ok(path) => path,
            Err(reason) => {
                return Resolution::Invalid {
                    target: target.to_owned(),
                    reason,
                };
            }
        }
    };

    resolve_exact(target, path, snapshot, image)
}

fn resolve_exact(target: &str, path: ContentPath, snapshot: &Snapshot, image: bool) -> Resolution {
    let Some(file) = snapshot.files().iter().find(|file| file.path() == &path) else {
        return Resolution::Missing {
            target: target.to_owned(),
        };
    };
    if image && is_note(file.path()) {
        return Resolution::Invalid {
            target: target.to_owned(),
            reason: InvalidResolutionReason::MarkdownImageTargetsNote,
        };
    }
    resolved(candidate(file.path().clone()))
}

fn resolve_candidates(
    target: &str,
    candidates: impl IntoIterator<Item = ResolutionCandidate>,
) -> Resolution {
    let mut candidates: Vec<_> = candidates.into_iter().collect();
    candidates.sort();
    candidates.dedup();
    match candidates.len() {
        0 => Resolution::Missing {
            target: target.to_owned(),
        },
        1 => resolved(candidates.pop().expect("length checked")),
        _ => Resolution::Ambiguous {
            target: target.to_owned(),
            candidates,
        },
    }
}

fn resolved(candidate: ResolutionCandidate) -> Resolution {
    match candidate.kind {
        ResolvedTargetKind::Note => Resolution::ResolvedNote {
            path: candidate.path,
        },
        ResolvedTargetKind::Asset => Resolution::ResolvedAsset {
            path: candidate.path,
        },
    }
}

fn candidate(path: ContentPath) -> ResolutionCandidate {
    ResolutionCandidate {
        kind: if is_note(&path) {
            ResolvedTargetKind::Note
        } else {
            ResolvedTargetKind::Asset
        },
        path,
    }
}

fn normalize_relative(
    source_path: &ContentPath,
    target: &str,
) -> Result<ContentPath, InvalidResolutionReason> {
    if target.is_empty()
        || target.starts_with('/')
        || target.contains(['\\', '\0'])
        || has_windows_drive_prefix(target)
    {
        return Err(InvalidResolutionReason::InvalidLocalPath);
    }

    let mut parts: Vec<&str> = source_path.as_str().split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" => return Err(InvalidResolutionReason::InvalidLocalPath),
            "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(InvalidResolutionReason::EscapesContentRoot);
                }
            }
            part => parts.push(part),
        }
    }
    ContentPath::new(parts.join("/")).map_err(|_| InvalidResolutionReason::InvalidLocalPath)
}

fn strip_fragment(target: &str) -> &str {
    target.split_once('#').map_or(target, |(path, _)| path)
}

fn is_external(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn file_name(path: &ContentPath) -> &str {
    path.as_str()
        .rsplit('/')
        .next()
        .expect("ContentPath is non-empty")
}

fn has_extension(path: &str) -> bool {
    file_name_str(path).rsplit_once('.').is_some()
}

fn file_name_str(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_note(path: &ContentPath) -> bool {
    path.as_str().ends_with(".md")
}

fn invalid_path(target: &str) -> Resolution {
    Resolution::Invalid {
        target: target.to_owned(),
        reason: InvalidResolutionReason::InvalidLocalPath,
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::domain::{MarkdownReferenceParser, Sha256, SnapshotFile, SnapshotId, SourceId};

    fn file(path: &str) -> SnapshotFile {
        SnapshotFile::new(
            ContentPath::new(path).unwrap(),
            1,
            Sha256::new([1; 32]),
            None,
        )
    }

    fn snapshot(paths: &[&str]) -> Snapshot {
        Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("vault").unwrap(),
            paths.iter().map(|path| file(path)).collect(),
        )
        .unwrap()
    }

    fn resolve(markdown: &str, source: &str, paths: &[&str]) -> Resolution {
        let reference = MarkdownReferenceParser::parse(markdown).pop().unwrap();
        ReferenceResolver::resolve(
            &reference,
            &ContentPath::new(source).unwrap(),
            &snapshot(paths),
        )
    }

    fn note(path: &str) -> Resolution {
        Resolution::ResolvedNote {
            path: ContentPath::new(path).unwrap(),
        }
    }

    fn asset(path: &str) -> Resolution {
        Resolution::ResolvedAsset {
            path: ContentPath::new(path).unwrap(),
        }
    }

    #[test]
    fn resolves_wiki_links_by_unique_name_explicit_path_and_fragment() {
        let paths = ["notes/day.md", "other/note.md", "folder/note.md"];
        assert_eq!(
            resolve("[[day]]", "notes/day.md", &paths),
            note("notes/day.md")
        );
        assert_eq!(
            resolve("[[folder/note]]", "notes/day.md", &paths),
            note("folder/note.md")
        );
        assert_eq!(
            resolve("[[folder/note.md]]", "notes/day.md", &paths),
            note("folder/note.md")
        );
        assert_eq!(
            resolve("[[day#heading]]", "notes/day.md", &paths),
            note("notes/day.md")
        );
        assert_eq!(
            resolve("[[#heading]]", "notes/day.md", &paths),
            note("notes/day.md")
        );
    }

    #[test]
    fn reports_missing_and_sorted_ambiguous_wiki_links() {
        assert_eq!(
            resolve("[[missing]]", "source.md", &["source.md"]),
            Resolution::Missing {
                target: "missing".to_owned()
            }
        );
        assert_eq!(
            resolve(
                "[[note]]",
                "source.md",
                &["source.md", "z/note.md", "a/note.md"]
            ),
            Resolution::Ambiguous {
                target: "note".to_owned(),
                candidates: vec![
                    candidate(ContentPath::new("a/note.md").unwrap()),
                    candidate(ContentPath::new("z/note.md").unwrap())
                ]
            }
        );
    }

    #[test]
    fn wiki_embed_uses_actual_snapshot_target_type() {
        let paths = ["source.md", "assets/image.png", "notes/embedded-note.md"];
        assert_eq!(
            resolve("![[image.png]]", "source.md", &paths),
            asset("assets/image.png")
        );
        assert_eq!(
            resolve("![[embedded-note]]", "source.md", &paths),
            note("notes/embedded-note.md")
        );
        assert!(matches!(
            resolve("![[missing]]", "source.md", &paths),
            Resolution::Missing { .. }
        ));
    }

    #[test]
    fn wiki_embed_preserves_multiple_actual_candidates() {
        let resolution = resolve(
            "![[item]]",
            "source.md",
            &["source.md", "z/item", "a/item.md"],
        );
        assert_eq!(
            resolution,
            Resolution::Ambiguous {
                target: "item".to_owned(),
                candidates: vec![
                    candidate(ContentPath::new("a/item.md").unwrap()),
                    candidate(ContentPath::new("z/item").unwrap())
                ]
            }
        );
    }

    #[test]
    fn markdown_links_resolve_relative_dot_and_parent_segments() {
        let paths = [
            "article.md",
            "notes/day.md",
            "notes/sibling.md",
            "attachments/report.pdf",
        ];
        assert_eq!(
            resolve("[article](../article.md)", "notes/day.md", &paths),
            note("article.md")
        );
        assert_eq!(
            resolve("[file](../attachments/report.pdf)", "notes/day.md", &paths),
            asset("attachments/report.pdf")
        );
        assert_eq!(
            resolve("[sibling](./sibling.md#part)", "notes/day.md", &paths),
            note("notes/sibling.md")
        );
    }

    #[test]
    fn markdown_paths_cannot_escape_the_content_root() {
        assert_eq!(
            resolve(
                "[outside](../../outside.md)",
                "notes/day.md",
                &["notes/day.md"]
            ),
            Resolution::Invalid {
                target: "../../outside.md".to_owned(),
                reason: InvalidResolutionReason::EscapesContentRoot,
            }
        );
    }

    #[test]
    fn markdown_images_require_an_existing_asset() {
        let paths = ["notes/day.md", "attachments/image.png", "article.md"];
        assert_eq!(
            resolve("![image](../attachments/image.png)", "notes/day.md", &paths),
            asset("attachments/image.png")
        );
        assert!(matches!(
            resolve("![missing](../missing.png)", "notes/day.md", &paths),
            Resolution::Missing { .. }
        ));
        assert_eq!(
            resolve("![not image](../article.md)", "notes/day.md", &paths),
            Resolution::Invalid {
                target: "../article.md".to_owned(),
                reason: InvalidResolutionReason::MarkdownImageTargetsNote,
            }
        );
    }

    #[test]
    fn external_references_never_depend_on_snapshot_contents() {
        for markdown in [
            "https://example.com",
            "[site](https://example.com)",
            "![img](https://example.com/a.png)",
        ] {
            let empty = resolve(markdown, "source.md", &[]);
            let populated = resolve(markdown, "source.md", &["https:/example.com", "source.md"]);
            assert_eq!(empty, populated);
            assert!(matches!(empty, Resolution::External { .. }));
        }
    }

    #[test]
    fn resolution_is_independent_of_snapshot_input_order() {
        let first = resolve(
            "![[item]]",
            "source.md",
            &["z/item", "source.md", "a/item.md"],
        );
        let second = resolve(
            "![[item]]",
            "source.md",
            &["a/item.md", "z/item", "source.md"],
        );
        assert_eq!(first, second);
    }
}
