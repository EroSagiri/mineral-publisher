//! Which source paths the public publication is allowed to consider at all.
//!
//! A public exclusion is a statement about *publication scope*, and it is not any
//! of the things it is easy to confuse it with:
//!
//! * it is not source ignoring: an excluded path is still captured by the Snapshot
//!   and still stored in the immutable content store;
//! * it is not a privacy rejection: excluded content is never evaluated, so it
//!   cannot carry a privacy reason code or a reviewer decision;
//! * it is not deletion: the source file stays exactly where it is.
//!
//! It sits between the Snapshot and the deterministic privacy policy, because it is
//! the *scope* of the question those later stages answer. Everything downstream sees
//! only the included candidate set, and the decision is taken from the canonical
//! `ContentPath` alone: an excluded path's bytes are never read to decide that it is
//! out of scope.

use std::{error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::domain::{ContentPath, Sha256};

/// The pattern grammar version frozen into every [`PublicScopeIdentity`].
///
/// It changes only if the meaning of a pattern changes, because an old identity
/// must keep describing the rules that produced it.
pub const PUBLIC_SCOPE_RULES_VERSION: u32 = 1;

/// One validated public-exclusion pattern.
///
/// The grammar is deliberately small, and it is not `.gitignore`:
///
/// * a pattern is relative to the vault root and anchored there;
/// * `*` matches any run of characters inside one path segment;
/// * `?` matches exactly one character inside one path segment;
/// * `**` as a whole segment crosses segments: leading `**/` matches zero or more
///   leading segments, a trailing `/**` matches one or more trailing segments, and
///   a middle `/**/` matches zero or more segments in between.
///
/// A rule is matched against a canonical `ContentPath`, so the same rules mean the
/// same thing on every host: there is no filesystem, no directory entry and no
/// operating-system path syntax involved.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PublicExclusionRule {
    pattern: String,
    segments: Vec<PatternSegment>,
}

impl PublicExclusionRule {
    pub fn new(pattern: impl Into<String>) -> Result<Self, PublicExclusionRuleError> {
        let pattern = pattern.into();
        validate(&pattern)?;
        Ok(Self {
            segments: compile(&pattern),
            pattern,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// Whether this rule puts one canonical path out of public scope.
    pub fn matches(&self, path: &ContentPath) -> bool {
        let segments = path.as_str().split('/').collect::<Vec<_>>();
        match self.segments.split_last() {
            // A trailing `**` means "everything inside", so at least one segment has
            // to remain after the prefix: `private/**` never matches a file that is
            // itself called `private`.
            Some((PatternSegment::DoubleStar, prefix)) => match_positions(prefix, &segments)
                .iter()
                .any(|end| *end < segments.len()),
            _ => match_positions(&self.segments, &segments).contains(&segments.len()),
        }
    }
}

impl fmt::Display for PublicExclusionRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.pattern)
    }
}

/// The validated, canonical set of public-exclusion rules.
///
/// The set is an unordered union: rule order carries no meaning, duplicates are
/// dropped, and the canonical order is part of the frozen audit record. Two
/// configurations that differ only in the order of their rules are the same scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicExclusionRules {
    rules: Vec<PublicExclusionRule>,
    identity: PublicScopeIdentity,
}

impl Default for PublicExclusionRules {
    fn default() -> Self {
        Self::empty()
    }
}

impl PublicExclusionRules {
    /// The scope that excludes nothing, which is what an unconfigured workspace has.
    pub fn empty() -> Self {
        Self::from_validated(Vec::new())
    }

    /// Parses, validates, sorts and deduplicates one configured rule list.
    pub fn new(
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, PublicExclusionRuleError> {
        let mut rules = patterns
            .into_iter()
            .map(PublicExclusionRule::new)
            .collect::<Result<Vec<_>, _>>()?;
        rules.sort();
        rules.dedup();
        Ok(Self::from_validated(rules))
    }

    /// Rebuilds a scope from rules that were already validated and canonicalized,
    /// as a durable record stores them.
    ///
    /// The rules are validated again: a stored row is exactly the input that can be
    /// damaged, and a scope that cannot be trusted must not silently become a scope
    /// that excludes nothing.
    pub fn from_canonical(
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, PublicExclusionRuleError> {
        let rules = Self::new(patterns)?;
        Ok(rules)
    }

    fn from_validated(rules: Vec<PublicExclusionRule>) -> Self {
        let identity = PublicScopeIdentity::of(&rules);
        Self { rules, identity }
    }

    /// The frozen identity of exactly these rules.
    pub fn identity(&self) -> PublicScopeIdentity {
        self.identity
    }

    /// The canonical rules, in canonical order, as a durable record stores them.
    pub fn canonical(&self) -> Vec<&str> {
        self.rules.iter().map(PublicExclusionRule::as_str).collect()
    }

    pub fn rules(&self) -> &[PublicExclusionRule] {
        &self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// The scope decision for one canonical path.
    ///
    /// Any matching rule excludes; a path no rule names is inside public scope.
    pub fn decide(&self, path: &ContentPath) -> PublicScopeDecision {
        if self.excludes(path) {
            PublicScopeDecision::Excluded
        } else {
            PublicScopeDecision::Included
        }
    }

    pub fn excludes(&self, path: &ContentPath) -> bool {
        self.rules.iter().any(|rule| rule.matches(path))
    }
}

impl fmt::Display for PublicExclusionRules {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} rule(s) [{}]",
            self.rules.len(),
            self.canonical().join(", ")
        )
    }
}

/// What public scope one path is in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicScopeDecision {
    /// The path may enter the public candidate set.
    Included,
    /// The path is out of public scope: it is never a candidate, never evaluated,
    /// and never published.
    Excluded,
}

/// The deterministic identity of one configured public scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicScopeIdentity(Sha256);

impl Ord for PublicScopeIdentity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_bytes().cmp(other.0.as_bytes())
    }
}

impl PartialOrd for PublicScopeIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PublicScopeIdentity {
    fn of(rules: &[PublicExclusionRule]) -> Self {
        let mut hasher = Sha256Hasher::new();
        hasher.update(b"mineral-publisher-public-scope-v1\0");
        hasher.update(PUBLIC_SCOPE_RULES_VERSION.to_be_bytes());
        for rule in rules {
            let pattern = rule.as_str().as_bytes();
            hasher.update((pattern.len() as u64).to_be_bytes());
            hasher.update(pattern);
        }
        Self(Sha256::new(hasher.finalize().into()))
    }

    pub fn as_sha256(&self) -> Sha256 {
        self.0
    }
}

impl fmt::Display for PublicScopeIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One compiled pattern segment.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PatternSegment {
    /// A whole `**` segment.
    DoubleStar,
    /// A segment that may contain `*` and `?`.
    Glob(String),
}

fn validate(pattern: &str) -> Result<(), PublicExclusionRuleError> {
    if pattern.trim().is_empty() {
        return Err(PublicExclusionRuleError::Empty);
    }
    if pattern.starts_with('/') {
        return Err(PublicExclusionRuleError::Absolute);
    }
    if pattern.starts_with('!') {
        return Err(PublicExclusionRuleError::NegationUnsupported);
    }
    let bytes = pattern.as_bytes();
    let has_drive_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if has_drive_prefix {
        return Err(PublicExclusionRuleError::Absolute);
    }
    if pattern.contains('\\') {
        return Err(PublicExclusionRuleError::Backslash);
    }
    if pattern
        .chars()
        .any(|character| character.is_control() || character == '\0')
    {
        return Err(PublicExclusionRuleError::ControlCharacter);
    }
    if pattern.ends_with('/') {
        return Err(PublicExclusionRuleError::NotCanonical);
    }
    if pattern.split('/').any(|segment| {
        segment.is_empty() || matches!(segment, "." | "..") || segment.contains("***")
    }) {
        return Err(PublicExclusionRuleError::NotCanonical);
    }
    Ok(())
}

fn compile(pattern: &str) -> Vec<PatternSegment> {
    pattern
        .split('/')
        .map(|segment| {
            if segment == "**" {
                PatternSegment::DoubleStar
            } else {
                PatternSegment::Glob(segment.to_owned())
            }
        })
        .collect()
}

/// Every path-segment index the pattern prefix can match, in ascending order.
fn match_positions(pattern: &[PatternSegment], path: &[&str]) -> Vec<usize> {
    let mut reachable = vec![false; path.len() + 1];
    reachable[0] = true;
    for segment in pattern {
        let mut next = vec![false; path.len() + 1];
        match segment {
            PatternSegment::DoubleStar => {
                // `**` consumes any number of segments, including none.
                let mut carried = false;
                for index in 0..=path.len() {
                    carried = carried || reachable[index];
                    next[index] = carried;
                }
            }
            PatternSegment::Glob(glob) => {
                for index in 0..path.len() {
                    if reachable[index] && segment_matches(glob, path[index]) {
                        next[index + 1] = true;
                    }
                }
            }
        }
        reachable = next;
        if !reachable.iter().any(|value| *value) {
            return Vec::new();
        }
    }
    reachable
        .iter()
        .enumerate()
        .filter_map(|(index, reachable)| reachable.then_some(index))
        .collect()
}

/// `*` inside one segment: any run of characters, never a separator.
fn segment_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let mut reachable = vec![false; text.len() + 1];
    reachable[0] = true;
    for character in pattern {
        let mut next = vec![false; text.len() + 1];
        match character {
            '*' => {
                let mut carried = false;
                for index in 0..=text.len() {
                    carried = carried || reachable[index];
                    next[index] = carried;
                }
            }
            '?' => {
                next[1..=text.len()].copy_from_slice(&reachable[..text.len()]);
            }
            literal => {
                for index in 0..text.len() {
                    next[index + 1] = reachable[index] && text[index] == literal;
                }
            }
        }
        reachable = next;
        if !reachable.iter().any(|value| *value) {
            return false;
        }
    }
    reachable[text.len()]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicExclusionRuleError {
    /// The rule is empty or only whitespace.
    Empty,
    /// The rule is an absolute path or names a filesystem volume.
    Absolute,
    /// The rule uses `\` as a separator, which is host-dependent.
    Backslash,
    /// The rule contains a control character or NUL.
    ControlCharacter,
    /// The rule is not a canonical relative pattern.
    NotCanonical,
    /// The rule uses `!`, and this grammar has no negation.
    NegationUnsupported,
}

impl fmt::Display for PublicExclusionRuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "public exclusion rule cannot be empty",
            Self::Absolute => {
                "public exclusion rule must be a path relative to the vault root, with no volume or leading separator"
            }
            Self::Backslash => {
                "public exclusion rule must use `/` as its separator, never `\\`"
            }
            Self::ControlCharacter => {
                "public exclusion rule must not contain control characters"
            }
            Self::NotCanonical => {
                "public exclusion rule must be a canonical relative pattern with no empty, `.` or `..` segment"
            }
            Self::NegationUnsupported => {
                "public exclusion rule must not use `!`: this grammar has no negation"
            }
        })
    }
}

impl Error for PublicExclusionRuleError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn scope(patterns: &[&str]) -> PublicExclusionRules {
        PublicExclusionRules::new(patterns.iter().copied()).unwrap()
    }

    fn excluded(patterns: &[&str], path_value: &str) -> bool {
        scope(patterns).excludes(&path(path_value))
    }

    /// 1/2. exact paths, at the root and nested.
    #[test]
    fn an_exact_path_excludes_exactly_that_path() {
        assert!(excluded(&["secret.md"], "secret.md"));
        assert!(!excluded(&["secret.md"], "other.md"));
        assert!(!excluded(&["secret.md"], "notes/secret.md"));
        assert!(excluded(&["notes/internal.md"], "notes/internal.md"));
        assert!(!excluded(&["notes/internal.md"], "notes/other.md"));
        assert!(!excluded(&["notes/internal.md"], "internal.md"));
    }

    /// 3. a subtree, and the paths that only look like one.
    #[test]
    fn a_double_star_subtree_excludes_descendants_only() {
        let patterns = &["private/**"];
        assert!(excluded(patterns, "private/a.md"));
        assert!(excluded(patterns, "private/a/b.png"));
        assert!(!excluded(patterns, "private.md"));
        assert!(!excluded(patterns, "public/private/a.md"));
        // `**` is one segment of the pattern, not a prefix of a segment name.
        assert!(!excluded(patterns, "privateer/a.md"));
    }

    /// A `**` in the middle crosses any number of segments, including none.
    #[test]
    fn a_middle_double_star_crosses_zero_or_more_segments() {
        let patterns = &["notes/**/draft.md"];
        assert!(excluded(patterns, "notes/draft.md"));
        assert!(excluded(patterns, "notes/a/draft.md"));
        assert!(excluded(patterns, "notes/a/b/draft.md"));
        assert!(!excluded(patterns, "notes/a/other.md"));
    }

    /// 4. `*` stays inside one segment.
    #[test]
    fn a_single_star_never_crosses_a_separator() {
        let patterns = &["attachments/*.png"];
        assert!(excluded(patterns, "attachments/a.png"));
        assert!(excluded(patterns, "attachments/.png"));
        assert!(!excluded(patterns, "attachments/x/a.png"));
        assert!(!excluded(patterns, "attachments/a.jpg"));
        assert!(!excluded(patterns, "other/attachments/a.png"));
    }

    /// 5. a leading `**/` matches at any depth, including the root.
    #[test]
    fn a_leading_double_star_matches_at_any_depth() {
        let patterns = &["**/*.tmp"];
        assert!(excluded(patterns, "a.tmp"));
        assert!(excluded(patterns, "notes/a.tmp"));
        assert!(excluded(patterns, "notes/deep/a.tmp"));
        assert!(!excluded(patterns, "notes/a.md"));
    }

    /// 6. `?` is exactly one character inside a segment.
    #[test]
    fn a_question_mark_matches_exactly_one_character() {
        let patterns = &["drafts/?.md"];
        assert!(excluded(patterns, "drafts/a.md"));
        assert!(!excluded(patterns, "drafts/ab.md"));
        assert!(!excluded(patterns, "drafts/.md"));
        assert!(!excluded(patterns, "drafts/a/b.md"));
    }

    /// The whole scope, as the configuration examples describe it.
    #[test]
    fn the_documented_example_scope_decides_every_example() {
        let scope = scope(&[
            "private/**",
            "drafts/**",
            "secret.md",
            "notes/internal.md",
            "attachments/private.png",
            "**/*.tmp",
        ]);

        for path_value in [
            "private/passwords.md",
            "drafts/post.md",
            "secret.md",
            "notes/internal.md",
            "attachments/private.png",
            "notes/a.tmp",
        ] {
            assert_eq!(
                scope.decide(&path(path_value)),
                PublicScopeDecision::Excluded,
                "{path_value} must be out of public scope"
            );
        }
        for path_value in [
            "index.md",
            "notes/public.md",
            "private.md",
            "attachments/public.png",
            "notes/a.md",
        ] {
            assert_eq!(
                scope.decide(&path(path_value)),
                PublicScopeDecision::Included,
                "{path_value} must stay in public scope"
            );
        }
    }

    /// 7. every unusable rule fails closed at construction.
    #[test]
    fn unusable_rules_are_refused() {
        for pattern in [
            "",
            "   ",
            "/absolute.md",
            "C:/windows.md",
            "c:\\windows.md",
            "..",
            "../outside.md",
            "notes/../../outside.md",
            "notes//internal.md",
            "notes/",
            "with\\backslash.md",
            "with\0nul.md",
            "with\nnewline.md",
            "with\ttab.md",
            "!negated.md",
            "***",
        ] {
            assert!(
                PublicExclusionRule::new(pattern).is_err(),
                "accepted {pattern:?}"
            );
        }
        // A rule that matches nothing is perfectly usable.
        assert!(PublicExclusionRule::new("does-not-exist/**").is_ok());
    }

    /// 8. rule order and duplicates are not part of the identity.
    #[test]
    fn the_identity_is_order_and_duplicate_independent() {
        let first = scope(&["secret.md", "private/**"]);
        let second = scope(&["private/**", "secret.md"]);
        let third = scope(&["private/**", "secret.md", "private/**"]);

        assert_eq!(first.identity(), second.identity());
        assert_eq!(first.identity(), third.identity());
        assert_eq!(first.canonical(), vec!["private/**", "secret.md"]);
        assert_ne!(first.identity(), scope(&["secret.md"]).identity());
        assert_eq!(
            PublicExclusionRules::empty().identity(),
            scope(&[]).identity()
        );
        // The identity is reproducible from the canonical rules alone, which is what
        // a durable record stores.
        assert_eq!(
            PublicExclusionRules::from_canonical(first.canonical()).unwrap(),
            first
        );
    }
}
