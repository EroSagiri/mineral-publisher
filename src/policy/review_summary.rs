use std::{error::Error, fmt};

pub const MAX_REVIEW_SUMMARY_CHARS: usize = 200;

pub fn validate_review_summary(value: &str) -> Result<(), ReviewSummaryError> {
    let summary = value.trim();
    if summary.is_empty() {
        return Err(ReviewSummaryError::Empty);
    }
    if summary.chars().count() > MAX_REVIEW_SUMMARY_CHARS {
        return Err(ReviewSummaryError::TooLong);
    }
    if summary.contains(['\r', '\n']) {
        return Err(ReviewSummaryError::NotBrief);
    }
    let endings = summary
        .chars()
        .filter(|character| matches!(character, '.' | '!' | '?' | '。' | '！' | '？'))
        .count();
    if endings > 2 {
        return Err(ReviewSummaryError::NotBrief);
    }
    if contains_sensitive_value(summary) {
        return Err(ReviewSummaryError::NotSafe);
    }
    Ok(())
}

fn contains_sensitive_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if [
        "sk-",
        "ghp_",
        "akia",
        "bearer ",
        "-----begin openssh private key-----",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return true;
    }
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|run| {
            let digits = run.bytes().filter(u8::is_ascii_digit).count();
            (run.len() >= 20 && digits > 0)
                || (run.len() == 11 && digits == 11)
                || (run.len() == 18 && digits >= 17)
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewSummaryError {
    Empty,
    TooLong,
    NotBrief,
    NotSafe,
}

impl fmt::Display for ReviewSummaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "review summary cannot be empty",
            Self::TooLong => "review summary is too long",
            Self::NotBrief => "review summary must be one or two short sentences",
            Self::NotSafe => "review summary may contain sensitive raw values",
        })
    }
}

impl Error for ReviewSummaryError {}
