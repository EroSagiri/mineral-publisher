use serde_yaml_ng::Value;

use super::ContentPath;

/// The minimal supported metadata extracted from a Markdown YAML frontmatter block.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MarkdownFrontmatter {
    private: Option<Value>,
    visibility: Option<Value>,
    public: Option<Value>,
    publish: Option<Value>,
    tags: Option<Value>,
}

/// The result of parsing an optional frontmatter block at the start of a document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontmatterParseResult {
    Absent,
    Parsed(Box<MarkdownFrontmatter>),
    Invalid { reason: String },
}

/// A deterministic reason why the complete Markdown document is private.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PrivateReason {
    PathContainsPrivateMarker,
    FrontmatterPrivate,
    VisibilityPrivate,
    PublicFalse,
    PublishFalse,
    PrivateTag,
}

/// The deterministic privacy result consumed by a later private filter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrivacyClassification {
    PublicCandidate,
    Private { reasons: Vec<PrivateReason> },
    Invalid { reason: String },
}

/// Parses only a YAML frontmatter block beginning on the first line of a document.
pub struct MarkdownFrontmatterParser;

impl MarkdownFrontmatterParser {
    pub fn parse(markdown: &str) -> FrontmatterParseResult {
        let Some(after_opening) = markdown
            .strip_prefix("---\n")
            .or_else(|| markdown.strip_prefix("---\r\n"))
        else {
            return FrontmatterParseResult::Absent;
        };

        let Some(frontmatter) = frontmatter_contents(after_opening) else {
            return FrontmatterParseResult::Invalid {
                reason: "frontmatter opening delimiter has no closing delimiter".to_owned(),
            };
        };

        if frontmatter.trim().is_empty() {
            return FrontmatterParseResult::Parsed(Box::default());
        }

        match parse_yaml_frontmatter(frontmatter) {
            Ok(metadata) => FrontmatterParseResult::Parsed(Box::new(metadata)),
            Err(reason) => FrontmatterParseResult::Invalid { reason },
        }
    }
}

fn parse_yaml_frontmatter(frontmatter: &str) -> Result<MarkdownFrontmatter, String> {
    let value: Value = serde_yaml_ng::from_str(frontmatter).map_err(|error| error.to_string())?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| "frontmatter must be a YAML mapping".to_owned())?;

    Ok(MarkdownFrontmatter {
        private: mapping_value(mapping, "private"),
        visibility: mapping_value(mapping, "visibility"),
        public: mapping_value(mapping, "public"),
        publish: mapping_value(mapping, "publish"),
        tags: mapping_value(mapping, "tags"),
    })
}

fn mapping_value(mapping: &serde_yaml_ng::Mapping, key: &str) -> Option<Value> {
    mapping.get(Value::String(key.to_owned())).cloned()
}

fn frontmatter_contents(markdown_after_opening: &str) -> Option<&str> {
    let mut offset = 0;
    for line_with_ending in markdown_after_opening.split_inclusive('\n') {
        let line = line_with_ending
            .strip_suffix('\n')
            .unwrap_or(line_with_ending);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "---" {
            return Some(&markdown_after_opening[..offset]);
        }
        offset += line_with_ending.len();
    }

    None
}

/// Combines normalized ContentPath rules and parsed metadata into a deny-only result.
pub struct PrivacyClassifier;

impl PrivacyClassifier {
    pub fn classify(
        path: &ContentPath,
        frontmatter: &FrontmatterParseResult,
    ) -> PrivacyClassification {
        let mut reasons = Vec::new();
        let mut invalid_reasons = Vec::new();

        if path_contains_private_marker(path) {
            reasons.push(PrivateReason::PathContainsPrivateMarker);
        }

        match frontmatter {
            FrontmatterParseResult::Absent => {}
            FrontmatterParseResult::Invalid { reason } => invalid_reasons.push(reason.clone()),
            FrontmatterParseResult::Parsed(metadata) => {
                collect_boolean_signal(
                    metadata.private.as_ref(),
                    "private",
                    true,
                    PrivateReason::FrontmatterPrivate,
                    &mut reasons,
                    &mut invalid_reasons,
                );
                collect_visibility_signal(
                    metadata.visibility.as_ref(),
                    &mut reasons,
                    &mut invalid_reasons,
                );
                collect_boolean_signal(
                    metadata.public.as_ref(),
                    "public",
                    false,
                    PrivateReason::PublicFalse,
                    &mut reasons,
                    &mut invalid_reasons,
                );
                collect_boolean_signal(
                    metadata.publish.as_ref(),
                    "publish",
                    false,
                    PrivateReason::PublishFalse,
                    &mut reasons,
                    &mut invalid_reasons,
                );
                collect_tag_signal(metadata.tags.as_ref(), &mut reasons, &mut invalid_reasons);
            }
        }

        if !reasons.is_empty() {
            reasons.sort_unstable();
            reasons.dedup();
            PrivacyClassification::Private { reasons }
        } else if invalid_reasons.is_empty() {
            PrivacyClassification::PublicCandidate
        } else {
            PrivacyClassification::Invalid {
                reason: invalid_reasons.join("; "),
            }
        }
    }
}

fn path_contains_private_marker(path: &ContentPath) -> bool {
    let path = path.as_str();
    path.contains("私有") || path.contains("私人") || path.to_ascii_lowercase().contains("private")
}

fn collect_boolean_signal(
    value: Option<&Value>,
    field: &str,
    private_value: bool,
    reason: PrivateReason,
    reasons: &mut Vec<PrivateReason>,
    invalid_reasons: &mut Vec<String>,
) {
    let Some(value) = value else {
        return;
    };

    match parse_boolean(value) {
        Some(value) if value == private_value => reasons.push(reason),
        Some(_) => {}
        None => invalid_reasons.push(format!(
            "frontmatter field `{field}` must be a supported boolean"
        )),
    }
}

fn parse_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::String(value) if value.eq_ignore_ascii_case("true") => Some(true),
        Value::String(value) if value.eq_ignore_ascii_case("yes") => Some(true),
        Value::String(value) if value.eq_ignore_ascii_case("false") => Some(false),
        Value::String(value) if value.eq_ignore_ascii_case("no") => Some(false),
        _ => None,
    }
}

fn collect_visibility_signal(
    value: Option<&Value>,
    reasons: &mut Vec<PrivateReason>,
    invalid_reasons: &mut Vec<String>,
) {
    let Some(value) = value else {
        return;
    };

    match value {
        Value::String(value) if value.eq_ignore_ascii_case("private") => {
            reasons.push(PrivateReason::VisibilityPrivate);
        }
        Value::String(_) => {}
        _ => invalid_reasons.push("frontmatter field `visibility` must be a string".to_owned()),
    }
}

fn collect_tag_signal(
    value: Option<&Value>,
    reasons: &mut Vec<PrivateReason>,
    invalid_reasons: &mut Vec<String>,
) {
    let Some(value) = value else {
        return;
    };

    let tags = match value {
        Value::Sequence(tags) => tags.as_slice(),
        _ => {
            invalid_reasons
                .push("frontmatter field `tags` must be a sequence of strings".to_owned());
            return;
        }
    };

    for tag in tags {
        let Value::String(tag) = tag else {
            invalid_reasons.push("frontmatter field `tags` must contain only strings".to_owned());
            continue;
        };
        if is_private_tag(tag) {
            reasons.push(PrivateReason::PrivateTag);
        }
    }
}

fn is_private_tag(tag: &str) -> bool {
    tag == "私有" || tag == "私人" || tag.eq_ignore_ascii_case("private")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn classify(path_value: &str, markdown: &str) -> PrivacyClassification {
        PrivacyClassifier::classify(
            &path(path_value),
            &MarkdownFrontmatterParser::parse(markdown),
        )
    }

    fn reasons(classification: PrivacyClassification) -> Vec<PrivateReason> {
        match classification {
            PrivacyClassification::Private { reasons } => reasons,
            other => panic!("expected private classification, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_paths_are_public_candidates() {
        assert_eq!(
            classify("普通/笔记.md", "hello"),
            PrivacyClassification::PublicCandidate
        );
        assert_eq!(
            classify("notes/today.md", "我的身份证号码……"),
            PrivacyClassification::PublicCandidate
        );
    }

    #[test]
    fn complete_content_path_uses_conservative_substring_matching() {
        for private_path in [
            "私人/笔记.md",
            "notes/私人/笔记.md",
            "notes/私有/笔记.md",
            "notes/private/note.md",
            "notes/PRIVATE/note.md",
            "notes/my_private_notes/note.md",
            "notes/PrivateStuff/note.md",
            "notes/not-private-anymore/note.md",
        ] {
            assert_eq!(
                reasons(classify(private_path, "hello")),
                [PrivateReason::PathContainsPrivateMarker],
                "path should be private: {private_path}"
            );
        }
    }

    #[test]
    fn private_boolean_supports_yaml_and_bounded_string_values() {
        for value in ["true", "TRUE", "yes", "\"true\"", "\"YES\""] {
            assert_eq!(
                reasons(classify(
                    "notes/note.md",
                    &format!("---\nprivate: {value}\n---")
                )),
                [PrivateReason::FrontmatterPrivate]
            );
        }
        for value in ["false", "FALSE", "no", "\"false\"", "\"NO\""] {
            assert_eq!(
                classify("notes/note.md", &format!("---\nprivate: {value}\n---")),
                PrivacyClassification::PublicCandidate
            );
        }
    }

    #[test]
    fn visibility_private_is_ascii_case_insensitive() {
        for value in ["private", "Private", "PRIVATE"] {
            assert_eq!(
                reasons(classify(
                    "notes/note.md",
                    &format!("---\nvisibility: {value}\n---")
                )),
                [PrivateReason::VisibilityPrivate]
            );
        }
        assert_eq!(
            classify("notes/note.md", "---\nvisibility: public\n---"),
            PrivacyClassification::PublicCandidate
        );
    }

    #[test]
    fn public_and_publish_false_are_private_signals() {
        assert_eq!(
            reasons(classify("notes/note.md", "---\npublic: false\n---")),
            [PrivateReason::PublicFalse]
        );
        assert_eq!(
            reasons(classify("notes/note.md", "---\npublish: false\n---")),
            [PrivateReason::PublishFalse]
        );
        assert_eq!(
            classify("notes/note.md", "---\npublic: true\npublish: true\n---"),
            PrivacyClassification::PublicCandidate
        );
    }

    #[test]
    fn supported_private_tags_use_exact_matching() {
        for frontmatter in [
            "---\ntags:\n  - private\n---",
            "---\ntags: [PRIVATE]\n---",
            "---\ntags: [私有]\n---",
            "---\ntags:\n  - 私人\n---",
        ] {
            assert_eq!(
                reasons(classify("notes/note.md", frontmatter)),
                [PrivateReason::PrivateTag]
            );
        }
        assert_eq!(
            classify("notes/note.md", "---\ntags: [privacy-research]\n---"),
            PrivacyClassification::PublicCandidate
        );
    }

    #[test]
    fn all_private_reasons_are_retained_in_deterministic_order() {
        let classification = classify(
            "私人/private-note.md",
            "---\nprivate: true\nvisibility: PRIVATE\npublic: false\npublish: false\ntags: [私人]\n---",
        );

        assert_eq!(
            reasons(classification),
            [
                PrivateReason::PathContainsPrivateMarker,
                PrivateReason::FrontmatterPrivate,
                PrivateReason::VisibilityPrivate,
                PrivateReason::PublicFalse,
                PrivateReason::PublishFalse,
                PrivateReason::PrivateTag,
            ]
        );
    }

    #[test]
    fn reason_order_does_not_depend_on_frontmatter_field_order() {
        let first = classify(
            "notes/private/note.md",
            "---\nprivate: true\nvisibility: private\npublic: false\npublish: false\ntags: [private]\n---",
        );
        let second = classify(
            "notes/private/note.md",
            "---\ntags: [private]\npublish: false\npublic: false\nvisibility: private\nprivate: true\n---",
        );

        assert_eq!(first, second);
    }

    #[test]
    fn positive_fields_cannot_override_any_private_signal() {
        assert_eq!(
            reasons(classify(
                "私人/note.md",
                "---\nprivate: false\npublic: true\npublish: true\n---"
            )),
            [PrivateReason::PathContainsPrivateMarker]
        );
        assert_eq!(
            reasons(classify(
                "notes/note.md",
                "---\nprivate: true\npublic: true\npublish: true\n---"
            )),
            [PrivateReason::FrontmatterPrivate]
        );
    }

    #[test]
    fn ordinary_and_empty_frontmatter_are_public_candidates() {
        for markdown in [
            "---\n---\nbody",
            "---\ntitle: hello\nauthor: june\ntags: [cycling]\n---\nbody",
        ] {
            assert_eq!(
                classify("notes/note.md", markdown),
                PrivacyClassification::PublicCandidate
            );
        }
    }

    #[test]
    fn malformed_or_ambiguous_privacy_metadata_fails_closed() {
        for markdown in [
            "---\nprivate: [broken\n---\nbody",
            "---\nprivate: maybe\n---\nbody",
            "---\nprivate: null\n---\nbody",
            "---\nvisibility: [private]\n---\nbody",
            "---\npublic: sometimes\n---\nbody",
            "---\npublish:\n  value: false\n---\nbody",
            "---\ntags: private\n---\nbody",
            "---\ntags: [cycling, 1]\n---\nbody",
            "---\n- private\n---\nbody",
            "---\nprivate: true\nbody",
        ] {
            assert!(
                matches!(
                    classify("notes/note.md", markdown),
                    PrivacyClassification::Invalid { .. }
                ),
                "metadata should be invalid: {markdown}"
            );
        }
    }

    #[test]
    fn known_private_signal_wins_even_when_another_field_is_invalid() {
        assert_eq!(
            reasons(classify(
                "notes/note.md",
                "---\nprivate: true\npublic: maybe\n---"
            )),
            [PrivateReason::FrontmatterPrivate]
        );
        assert_eq!(
            reasons(classify("私人/note.md", "---\nprivate: [broken\n---")),
            [PrivateReason::PathContainsPrivateMarker]
        );
        assert_eq!(
            reasons(classify("notes/note.md", "---\ntags: [private, 1]\n---")),
            [PrivateReason::PrivateTag]
        );
    }

    #[test]
    fn duplicate_known_fields_are_invalid_without_another_private_signal() {
        assert!(matches!(
            classify("notes/note.md", "---\nprivate: false\nprivate: true\n---"),
            PrivacyClassification::Invalid { .. }
        ));
    }

    #[test]
    fn delimiter_in_the_body_is_not_frontmatter() {
        assert_eq!(
            classify("notes/note.md", "body\n---\nprivate: true\n---"),
            PrivacyClassification::PublicCandidate
        );
    }

    #[test]
    fn windows_line_endings_are_supported() {
        assert_eq!(
            reasons(classify(
                "notes/note.md",
                "---\r\nprivate: true\r\n---\r\nbody"
            )),
            [PrivateReason::FrontmatterPrivate]
        );
    }
}
