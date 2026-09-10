use std::ops::Range;

/// The source syntax used for a reference found in Markdown.
///
/// This intentionally does not describe the target's eventual type. Resolving
/// a target to a note, asset, missing file, or ambiguous file needs a Snapshot
/// and belongs to a later resolver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceKind {
    WikiLink,
    WikiEmbed,
    MarkdownLink,
    MarkdownImage,
    ExternalUrl,
}

/// A reference found directly in a Markdown document.
///
/// `target` is deliberately unresolved: it is the exact target text used by
/// the author (apart from the Obsidian display/size suffix after `|`). A later
/// resolver can interpret it against a Snapshot without this parser needing
/// any filesystem or Snapshot access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    kind: ReferenceKind,
    target: String,
    span: Range<usize>,
}

impl Reference {
    pub fn kind(&self) -> ReferenceKind {
        self.kind
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    /// The byte range of the complete reference syntax in the source text.
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }
}

/// Pure Markdown / Obsidian reference parser.
#[derive(Clone, Copy, Debug, Default)]
pub struct MarkdownReferenceParser;

impl MarkdownReferenceParser {
    pub fn parse(markdown: &str) -> Vec<Reference> {
        let mut references = Vec::new();
        let mut offset = 0;
        let mut fence: Option<(u8, usize)> = None;

        for line in markdown.split_inclusive('\n') {
            let without_lf = line.strip_suffix('\n').unwrap_or(line);
            let content = without_lf.strip_suffix('\r').unwrap_or(without_lf);
            if let Some((marker, length)) = fence {
                if is_fence_close(content, marker, length) {
                    fence = None;
                }
            } else if let Some((marker, length)) = fence_open(content) {
                fence = Some((marker, length));
            } else if is_indented_code(content) {
                // This minimal parser treats every line with a Markdown code
                // indentation as code. It intentionally does not attempt the
                // full CommonMark continuation rules.
            } else {
                Self::parse_line(content, offset, &mut references);
            }
            offset += line.len();
        }

        references
    }

    fn parse_line(line: &str, line_offset: usize, references: &mut Vec<Reference>) {
        let bytes = line.as_bytes();
        let mut index = 0;
        let mut inline_code: Option<usize> = None;

        while index < bytes.len() {
            if bytes[index] == b'\\' {
                index += 1;
                if index < bytes.len() {
                    index += line[index..]
                        .chars()
                        .next()
                        .expect("index is inside the line")
                        .len_utf8();
                }
                continue;
            }
            if bytes[index] == b'`' {
                let length = run_length(bytes, index, b'`');
                if inline_code == Some(length) {
                    inline_code = None;
                } else if inline_code.is_none() {
                    inline_code = Some(length);
                }
                index += length;
                continue;
            }
            if inline_code.is_some() {
                index += 1;
                continue;
            }

            if let Some((kind, target, end)) = obsidian_reference(line, index) {
                references.push(Reference {
                    kind,
                    target: target.to_owned(),
                    span: line_offset + index..line_offset + end,
                });
                index = end;
                continue;
            }
            if let Some((kind, target, end)) = markdown_link(line, index) {
                references.push(Reference {
                    kind,
                    target: target.to_owned(),
                    span: line_offset + index..line_offset + end,
                });
                index = end;
                continue;
            }
            if let Some((target, end)) = external_url(line, index) {
                references.push(Reference {
                    kind: ReferenceKind::ExternalUrl,
                    target: target.to_owned(),
                    span: line_offset + index..line_offset + end,
                });
                index = end;
                continue;
            }
            index += line[index..]
                .chars()
                .next()
                .expect("index is inside the line")
                .len_utf8();
        }
    }
}

fn fence_open(line: &str) -> Option<(u8, usize)> {
    let trimmed = strip_fence_indent(line)?;
    let marker = *trimmed.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = run_length(trimmed.as_bytes(), 0, marker);
    (length >= 3).then_some((marker, length))
}

fn is_fence_close(line: &str, marker: u8, opening_length: usize) -> bool {
    let Some(trimmed) = strip_fence_indent(line) else {
        return false;
    };
    run_length(trimmed.as_bytes(), 0, marker) >= opening_length
}

/// Fenced blocks may start after zero through three ASCII spaces.
fn strip_fence_indent(line: &str) -> Option<&str> {
    let indent = line
        .as_bytes()
        .iter()
        .take_while(|&&byte| byte == b' ')
        .count();
    (indent <= 3).then_some(&line[indent..])
}

fn is_indented_code(line: &str) -> bool {
    line.starts_with('\t') || line.starts_with("    ")
}

fn run_length(bytes: &[u8], start: usize, byte: u8) -> usize {
    bytes[start..]
        .iter()
        .take_while(|&&current| current == byte)
        .count()
}

fn obsidian_reference(line: &str, start: usize) -> Option<(ReferenceKind, &str, usize)> {
    let (kind, opening) = if line[start..].starts_with("![[") {
        (ReferenceKind::WikiEmbed, 3)
    } else if line[start..].starts_with("[[") && (start == 0 || line.as_bytes()[start - 1] != b'!')
    {
        (ReferenceKind::WikiLink, 2)
    } else {
        return None;
    };
    let remainder = &line[start + opening..];
    let close = remainder.find("]]")?;
    let inner = &remainder[..close];
    let target = inner.split_once('|').map_or(inner, |(target, _)| target);
    if target.is_empty() {
        return None;
    }
    Some((kind, target, start + opening + close + 2))
}

fn markdown_link(line: &str, start: usize) -> Option<(ReferenceKind, &str, usize)> {
    let (image, opening) = if line[start..].starts_with("![") {
        (true, 2)
    } else if line[start..].starts_with('[') {
        (false, 1)
    } else {
        return None;
    };
    let label_end = line[start + opening..].find(']')? + start + opening;
    if !line[label_end..].starts_with("](") {
        return None;
    }
    let target_start = label_end + 2;
    let target_end = line[target_start..].find(')')? + target_start;
    let target = &line[target_start..target_end];
    if target.is_empty() {
        return None;
    }
    let kind = if image {
        ReferenceKind::MarkdownImage
    } else {
        ReferenceKind::MarkdownLink
    };
    Some((kind, target, target_end + 1))
}

fn external_url(line: &str, start: usize) -> Option<(&str, usize)> {
    if start > 0 && !is_url_boundary(line.as_bytes()[start - 1]) {
        return None;
    }
    let remainder = &line[start..];
    if !(remainder.starts_with("https://") || remainder.starts_with("http://")) {
        return None;
    }
    let end = remainder
        .find(|character: char| {
            character.is_whitespace() || matches!(character, '<' | '>' | '"' | '\'')
        })
        .unwrap_or(remainder.len());
    let target = remainder[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']);
    (!target.is_empty()).then_some((target, start + target.len()))
}

fn is_url_boundary(byte: u8) -> bool {
    !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-' | b'.' | b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(markdown: &str) -> Vec<(ReferenceKind, String)> {
        MarkdownReferenceParser::parse(markdown)
            .iter()
            .map(|reference| (reference.kind(), reference.target().to_owned()))
            .collect()
    }

    fn reference(kind: ReferenceKind, target: &str) -> (ReferenceKind, String) {
        (kind, target.to_owned())
    }

    #[test]
    fn parses_obsidian_note_links_with_aliases_and_headings() {
        assert_eq!(
            parsed("[[note]] [[note|alias]] [[note#heading]]"),
            [
                reference(ReferenceKind::WikiLink, "note"),
                reference(ReferenceKind::WikiLink, "note"),
                reference(ReferenceKind::WikiLink, "note#heading"),
            ]
        );
    }

    #[test]
    fn parses_wiki_embeds_without_guessing_the_target_type() {
        assert_eq!(
            parsed("![[image.png]] ![[image.png|600]] ![[embedded-note]]"),
            [
                reference(ReferenceKind::WikiEmbed, "image.png"),
                reference(ReferenceKind::WikiEmbed, "image.png"),
                reference(ReferenceKind::WikiEmbed, "embedded-note"),
            ]
        );
    }

    #[test]
    fn parses_standard_markdown_syntax_without_classifying_targets() {
        assert_eq!(
            parsed(
                "![image](attachments/image.png) [note](private.md) [file](attachments/file.pdf) [site](https://example.com)"
            ),
            [
                reference(ReferenceKind::MarkdownImage, "attachments/image.png"),
                reference(ReferenceKind::MarkdownLink, "private.md"),
                reference(ReferenceKind::MarkdownLink, "attachments/file.pdf"),
                reference(ReferenceKind::MarkdownLink, "https://example.com"),
            ]
        );
    }

    #[test]
    fn parses_bare_external_urls() {
        assert_eq!(
            parsed("https://example.com"),
            [reference(ReferenceKind::ExternalUrl, "https://example.com")]
        );
    }

    #[test]
    fn returns_no_references_for_plain_text() {
        assert!(MarkdownReferenceParser::parse("just ordinary prose").is_empty());
    }

    #[test]
    fn ignores_references_inside_inline_code_and_escaped_syntax() {
        let markdown = "`![[not-inline.png]]` \\![[not-escaped.png]] \\[[not-escaped]] \\[file](not-escaped.pdf) ![[real.png]]";

        assert_eq!(
            parsed(markdown),
            [reference(ReferenceKind::WikiEmbed, "real.png")]
        );
    }

    #[test]
    fn ignores_fenced_code_with_zero_through_three_spaces_of_indent() {
        for indent in ["", " ", "  ", "   "] {
            let markdown = format!(
                "{indent}```text\n{indent}![[not-a-real-reference.png]]\n{indent}```\n![[real.png]]"
            );

            assert_eq!(
                parsed(&markdown),
                [reference(ReferenceKind::WikiEmbed, "real.png")],
                "failed for indent {indent:?}"
            );
        }
    }

    #[test]
    fn ignores_indented_code() {
        assert_eq!(
            parsed("    ![[not-a-real-reference.png]]\n![[real.png]]"),
            [reference(ReferenceKind::WikiEmbed, "real.png")]
        );
    }

    #[test]
    fn parses_lf_and_crlf_with_the_same_results_and_correct_spans() {
        for markdown in ["[[first]]\n![[second.png]]", "[[first]]\r\n![[second.png]]"] {
            let references = MarkdownReferenceParser::parse(markdown);

            assert_eq!(
                references
                    .iter()
                    .map(|reference| (reference.kind(), reference.target()))
                    .collect::<Vec<_>>(),
                [
                    (ReferenceKind::WikiLink, "first"),
                    (ReferenceKind::WikiEmbed, "second.png"),
                ]
            );
            assert_eq!(&markdown[references[0].span()], "[[first]]");
            assert_eq!(&markdown[references[1].span()], "![[second.png]]");
        }
    }

    #[test]
    fn retains_the_complete_source_span() {
        let markdown = "before ![[image.png|600]] after";
        let reference = MarkdownReferenceParser::parse(markdown).pop().unwrap();

        assert_eq!(&markdown[reference.span()], "![[image.png|600]]");
    }

    #[test]
    fn preserves_wiki_embed_syntax_for_embedded_notes() {
        assert_eq!(
            parsed("![[embedded-note]]"),
            [reference(ReferenceKind::WikiEmbed, "embedded-note")]
        );
    }
}
