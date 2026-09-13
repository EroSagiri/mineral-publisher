use std::{error::Error, fmt};

/// One object one listing page reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedObject {
    pub key: String,
    pub last_modified: String,
    pub etag: String,
    pub size: u64,
}

/// One parsed `ListObjectsV2` response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListPage {
    pub objects: Vec<ListedObject>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// Parses one `ListObjectsV2` response body.
///
/// The parser is deliberately narrow: it reads exactly the elements this reader
/// depends on, cross-checks the reported key count against the objects it actually
/// saw, and fails closed on a body that is malformed, incomplete or contradictory.
/// A listing is the only thing that decides which objects exist, so a body this
/// parser is unsure about must stop the scan rather than produce a smaller
/// inventory.
pub fn parse_list_page(xml: &str) -> Result<ListPage, ListPageError> {
    let is_truncated = match element(xml, "IsTruncated")? {
        Some(value) => match unescape(value)?.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err(ListPageError::Malformed("IsTruncated")),
        },
        None => return Err(ListPageError::Malformed("IsTruncated")),
    };

    let mut objects = Vec::new();
    let mut cursor = 0;
    while let Some(start) = xml[cursor..].find("<Contents>") {
        let start = cursor + start;
        let body_start = start + "<Contents>".len();
        let end = xml[body_start..]
            .find("</Contents>")
            .map(|offset| body_start + offset)
            .ok_or(ListPageError::Malformed("Contents"))?;
        objects.push(parse_contents(&xml[body_start..end])?);
        cursor = end + "</Contents>".len();
    }

    if objects.is_empty() && !xml.contains("<Contents>") && !xml.contains("</Contents>") {
        // An empty page is legitimate: the namespace is empty, or a page boundary
        // fell between two objects. Nothing to check beyond the count below.
    }

    if let Some(count) = element(xml, "KeyCount")? {
        let count = unescape(count)?
            .parse::<usize>()
            .map_err(|_| ListPageError::Malformed("KeyCount"))?;
        if count != objects.len() {
            return Err(ListPageError::KeyCountMismatch {
                reported: count,
                observed: objects.len(),
            });
        }
    }

    let next_continuation_token = match element(xml, "NextContinuationToken")? {
        Some(value) => {
            let value = unescape(value)?;
            if value.is_empty() {
                return Err(ListPageError::Malformed("NextContinuationToken"));
            }
            Some(value)
        }
        None => None,
    };

    if is_truncated && next_continuation_token.is_none() {
        return Err(ListPageError::MissingContinuationToken);
    }

    Ok(ListPage {
        objects,
        is_truncated,
        next_continuation_token,
    })
}

fn parse_contents(block: &str) -> Result<ListedObject, ListPageError> {
    let key = required(block, "Key")?;
    if key.is_empty() {
        return Err(ListPageError::Malformed("Key"));
    }
    let last_modified = required(block, "LastModified")?;
    let etag = element(block, "ETag")?
        .ok_or(ListPageError::Malformed("ETag"))
        .and_then(unescape)?;
    let etag = etag
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(&etag)
        .to_owned();
    if etag.is_empty() {
        return Err(ListPageError::Malformed("ETag"));
    }
    let size = required(block, "Size")?
        .parse::<u64>()
        .map_err(|_| ListPageError::Malformed("Size"))?;

    Ok(ListedObject {
        key,
        last_modified,
        etag,
        size,
    })
}

fn required(block: &str, tag: &'static str) -> Result<String, ListPageError> {
    match element(block, tag)? {
        Some(value) => unescape(value),
        None => Err(ListPageError::Malformed(tag)),
    }
}

/// Extracts the text of the first `<tag>...</tag>` in `xml`.
fn element<'a>(xml: &'a str, tag: &'static str) -> Result<Option<&'a str>, ListPageError> {
    let opening = format!("<{tag}>");
    let closing = format!("</{tag}>");
    let Some(start) = xml.find(&opening) else {
        if xml.contains(&closing) {
            return Err(ListPageError::Malformed(tag));
        }
        return Ok(None);
    };
    let body_start = start + opening.len();
    let end = xml[body_start..]
        .find(&closing)
        .map(|offset| body_start + offset)
        .ok_or(ListPageError::Malformed(tag))?;
    if xml[body_start..end].contains(&opening) {
        return Err(ListPageError::Malformed(tag));
    }
    Ok(Some(&xml[body_start..end]))
}

/// Expands the XML entity references one listing may contain.
fn unescape(value: &str) -> Result<String, ListPageError> {
    if !value.contains('&') {
        return Ok(value.to_owned());
    }
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(position) = rest.find('&') {
        decoded.push_str(&rest[..position]);
        let after = &rest[position + 1..];
        let end = after.find(';').ok_or(ListPageError::Malformed("entity"))?;
        let entity = &after[..end];
        match entity {
            "amp" => decoded.push('&'),
            "lt" => decoded.push('<'),
            "gt" => decoded.push('>'),
            "quot" => decoded.push('"'),
            "apos" => decoded.push('\''),
            numeric if numeric.starts_with("#x") || numeric.starts_with("#X") => {
                let code = u32::from_str_radix(&numeric[2..], 16)
                    .map_err(|_| ListPageError::Malformed("entity"))?;
                decoded.push(char::from_u32(code).ok_or(ListPageError::Malformed("entity"))?);
            }
            numeric if numeric.starts_with('#') => {
                let code = numeric[1..]
                    .parse::<u32>()
                    .map_err(|_| ListPageError::Malformed("entity"))?;
                decoded.push(char::from_u32(code).ok_or(ListPageError::Malformed("entity"))?);
            }
            _ => return Err(ListPageError::Malformed("entity")),
        }
        rest = &after[end + 1..];
    }
    decoded.push_str(rest);
    Ok(decoded)
}

/// Why a listing page cannot be used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListPageError {
    /// An element is missing, repeated or malformed.
    Malformed(&'static str),
    /// The page claims a number of keys it did not contain.
    KeyCountMismatch { reported: usize, observed: usize },
    /// The page is truncated but names no cursor to continue from.
    MissingContinuationToken,
}

impl fmt::Display for ListPageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(element) => {
                write!(formatter, "object listing is malformed at {element}")
            }
            Self::KeyCountMismatch { reported, observed } => write!(
                formatter,
                "object listing claims {reported} keys but described {observed}"
            ),
            Self::MissingContinuationToken => {
                formatter.write_str("object listing is truncated without a continuation token")
            }
        }
    }
}

impl Error for ListPageError {}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>mineral-vault</Name>
  <Prefix>vault/</Prefix>
  <KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>vault/index.md</Key>
    <LastModified>2024-01-01T00:00:00.000Z</LastModified>
    <ETag>&quot;d41d8cd98f00b204e9800998ecf8427e&quot;</ETag>
    <Size>3</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>vault/notes/a&amp;b.md</Key>
    <LastModified>2024-01-02T00:00:00.000Z</LastModified>
    <ETag>&quot;5d41402abc4b2a76b9719d911017c592-2&quot;</ETag>
    <Size>0</Size>
  </Contents>
</ListBucketResult>"#;

    #[test]
    fn a_page_is_parsed_with_its_objects_and_entities() {
        let page = parse_list_page(PAGE).unwrap();

        assert!(!page.is_truncated);
        assert_eq!(page.next_continuation_token, None);
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "vault/index.md");
        assert_eq!(
            page.objects[0].etag, "d41d8cd98f00b204e9800998ecf8427e",
            "the ETag keeps its bytes but loses the XML quoting"
        );
        assert_eq!(page.objects[0].size, 3);
        assert_eq!(page.objects[0].last_modified, "2024-01-01T00:00:00.000Z");
        assert_eq!(page.objects[1].key, "vault/notes/a&b.md");
        assert_eq!(page.objects[1].etag, "5d41402abc4b2a76b9719d911017c592-2");
        assert_eq!(page.objects[1].size, 0);
    }

    #[test]
    fn a_truncated_page_carries_its_cursor() {
        let page = parse_list_page(
            r#"<ListBucketResult><KeyCount>0</KeyCount><IsTruncated>true</IsTruncated>
               <NextContinuationToken>1/abc+def==</NextContinuationToken></ListBucketResult>"#,
        )
        .unwrap();

        assert!(page.is_truncated);
        assert_eq!(page.next_continuation_token.as_deref(), Some("1/abc+def=="));
        assert!(page.objects.is_empty());
    }

    #[test]
    fn an_empty_namespace_is_an_empty_page() {
        let page =
            parse_list_page(r#"<ListBucketResult><KeyCount>0</KeyCount><IsTruncated>false</IsTruncated></ListBucketResult>"#)
                .unwrap();

        assert!(page.objects.is_empty());
        assert!(!page.is_truncated);
    }

    #[test]
    fn a_damaged_or_contradictory_page_fails_closed() {
        for body in [
            // No truncation flag at all.
            "<ListBucketResult><KeyCount>0</KeyCount></ListBucketResult>",
            // Claims two keys, describes one.
            r#"<ListBucketResult><KeyCount>2</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key>vault/a.md</Key><LastModified>t</LastModified><ETag>&quot;e&quot;</ETag><Size>1</Size></Contents></ListBucketResult>"#,
            // Truncated without a cursor.
            "<ListBucketResult><KeyCount>0</KeyCount><IsTruncated>true</IsTruncated></ListBucketResult>",
            // An object without a size.
            r#"<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key>vault/a.md</Key><LastModified>t</LastModified><ETag>&quot;e&quot;</ETag></Contents></ListBucketResult>"#,
            // An empty key.
            r#"<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key></Key><LastModified>t</LastModified><ETag>&quot;e&quot;</ETag><Size>1</Size></Contents></ListBucketResult>"#,
            // A size that is not a number.
            r#"<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key>vault/a.md</Key><LastModified>t</LastModified><ETag>&quot;e&quot;</ETag><Size>many</Size></Contents></ListBucketResult>"#,
            // An unclosed object block.
            r#"<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key>vault/a.md</Key></ListBucketResult>"#,
            // An entity that is not XML.
            r#"<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key>vault/a&nbsp;b.md</Key><LastModified>t</LastModified><ETag>&quot;e&quot;</ETag><Size>1</Size></Contents></ListBucketResult>"#,
            // A closing tag with no opening tag.
            "<ListBucketResult><KeyCount>0</KeyCount><IsTruncated>false</IsTruncated><NextContinuationToken></NextContinuationToken></ListBucketResult>",
        ] {
            assert!(parse_list_page(body).is_err(), "{body} was accepted");
        }
    }

    #[test]
    fn a_key_that_contains_a_tag_like_sequence_is_still_one_key() {
        let page = parse_list_page(
            r#"<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key>vault/&lt;Size&gt;.md</Key><LastModified>t</LastModified><ETag>&quot;e&quot;</ETag><Size>1</Size></Contents></ListBucketResult>"#,
        )
        .unwrap();

        assert_eq!(page.objects[0].key, "vault/<Size>.md");
        assert_eq!(page.objects[0].size, 1);
    }

    #[test]
    fn a_directory_marker_is_reported_like_any_other_object() {
        // The marker rule belongs to the inventory, not to the parser: the parser
        // reports exactly what the endpoint said.
        let page = parse_list_page(
            r#"<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated>
               <Contents><Key>vault/private/</Key><LastModified>t</LastModified><ETag>&quot;e&quot;</ETag><Size>0</Size></Contents></ListBucketResult>"#,
        )
        .unwrap();

        assert_eq!(page.objects[0].key, "vault/private/");
        assert_eq!(page.objects[0].size, 0);
    }
}
