//! An opt-in smoke example for the DeepSeek Markdown reviewer.
//!
//! Run only when you intend to make a real provider request:
//! `cargo run --example deepseek_markdown_reviewer_smoke -- <markdown-file>`

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use mineral_publisher::{
    domain::{SnapshotId, SourceId},
    policy::{HumanReviewReason, PolicyIdentity, PublicPolicyDecision},
    reviewer::{DeepSeekApiKey, DeepSeekMarkdownReviewer, DeepSeekMarkdownReviewerConfig},
    source::LocalSource,
    storage::{LocalContentStore, SqliteReviewRunStore},
    workflow::{PublicPolicyRun, SequentialReviewRunIdGenerator},
};

const API_BASE_URL: &str = "https://api.deepseek.com";
const MODEL: &str = "deepseek-v4-flash";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

fn main() -> Result<(), Box<dyn Error>> {
    let input_path = parse_input_path()?;

    // Read and validate the credential before preparing any source state. Constructing the
    // reviewer does not send a request; its only network operation is `Reviewer::review`.
    let api_key = DeepSeekApiKey::from_env("MINERAL_DEEPSEEK_API_KEY")?;
    let config = DeepSeekMarkdownReviewerConfig::new(
        API_BASE_URL,
        MODEL,
        api_key,
        TIMEOUT,
        MAX_INPUT_BYTES,
        MAX_RESPONSE_BYTES,
    )?
    .with_policy_instruction_from_env("MINERAL_DEEPSEEK_POLICY_PROMPT")?;

    let workspace = TemporaryWorkspace::new()?;
    let staged_markdown = workspace.source_root().join("document.md");
    fs::copy(&input_path, &staged_markdown)?;

    let content_store = LocalContentStore::new(workspace.path().join("content-store"));
    let source = LocalSource::new(
        workspace.source_root(),
        SourceId::new("deepseek-markdown-reviewer-smoke")?,
        content_store.clone(),
    );
    let snapshot = source.snapshot(SnapshotId::new(1)?, SystemTime::now())?;
    let reviewer = DeepSeekMarkdownReviewer::new(config, content_store.clone())?;
    let review_store = SqliteReviewRunStore::open(workspace.path().join("review-runs.sqlite3"))?;
    let policy = PolicyIdentity::new(
        "public-policy",
        reviewer.prompt_version(),
        reviewer.prompt_sha256(),
    )?;
    let mut review_ids =
        SequentialReviewRunIdGenerator::new(mineral_publisher::policy::ReviewRunId::new(1)?);

    let result = PublicPolicyRun::execute(
        &snapshot,
        &content_store,
        &reviewer,
        &review_store,
        &policy,
        &mut review_ids,
    )?;

    println!("path: {}", input_path.display());
    println!("provider: DeepSeek");
    println!("base_url: {API_BASE_URL}");
    println!("model: {MODEL}");
    println!("timeout: {}s", TIMEOUT.as_secs());
    println!("max_input_bytes: {MAX_INPUT_BYTES}");
    if !result.private_documents().is_empty() {
        // `PublicPolicyRun` calls PrivacyFilter before it can invoke the reviewer, so this
        // branch proves the staged Markdown was rejected without an HTTP request.
        println!("policy_decision: Rejected");
        println!("reviewer_result: NotCalled");
        return Ok(());
    }
    if !result.invalid_privacy_documents().is_empty() {
        println!("policy_decision: NeedsHumanReview");
        println!("reviewer_result: NotCalled");
        return Ok(());
    }

    let review = result
        .document_outcomes()
        .first()
        .ok_or("the snapshot did not produce a Markdown review outcome")?;
    match review.decision() {
        PublicPolicyDecision::ReviewApproved => {
            println!("policy_decision: Approved");
            println!("reviewer_result: Approve");
            print_reviewer_diagnostic(&reviewer);
        }
        PublicPolicyDecision::ReviewRejected => {
            println!("policy_decision: Rejected");
            println!("reviewer_result: Reject");
            print_reviewer_diagnostic(&reviewer);
        }
        PublicPolicyDecision::ProgramIssues(_) => {
            println!("policy_decision: NeedsHumanReview");
            println!("reviewer_result: NotCalled");
        }
        PublicPolicyDecision::NeedsHumanReview(reason) => {
            println!("policy_decision: NeedsHumanReview");
            match reason {
                HumanReviewReason::ReviewerRequested => {
                    println!("reviewer_result: NeedsHumanReview");
                    print_reviewer_diagnostic(&reviewer);
                }
                HumanReviewReason::ReviewerFailed(error) => {
                    println!("reviewer_result: Error");
                    println!("reviewer_error_category: {:?}", error.kind());
                    if let Some(status) = error.http_status() {
                        println!("http_status: {status}");
                    }
                    if let Some(code) = error.provider_error_code() {
                        println!("provider_error_code: {code}");
                    }
                    if let Some(message) = error.provider_error_message() {
                        println!("provider_error_message: {message}");
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_reviewer_diagnostic(reviewer: &DeepSeekMarkdownReviewer) {
    let Some(diagnostic) = reviewer.last_diagnostic() else {
        return;
    };
    if !diagnostic.reason_codes().is_empty() {
        println!(
            "reason_codes: {}",
            diagnostic
                .reason_codes()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(summary) = diagnostic.summary() {
        println!("summary: {summary}");
    }
}

fn parse_input_path() -> Result<PathBuf, Box<dyn Error>> {
    let mut arguments = env::args_os();
    let program = arguments.next().unwrap_or_default();
    let Some(path) = arguments.next() else {
        return Err(format!("usage: {} <markdown-file>", Path::new(&program).display()).into());
    };
    if arguments.next().is_some() {
        return Err(format!("usage: {} <markdown-file>", Path::new(&program).display()).into());
    }
    let path = PathBuf::from(path);
    if path.extension().is_none_or(|extension| extension != "md") {
        return Err("the input must be a .md Markdown file".into());
    }
    Ok(path)
}

struct TemporaryWorkspace {
    path: PathBuf,
}

impl TemporaryWorkspace {
    fn new() -> Result<Self, Box<dyn Error>> {
        for attempt in 0..100_u32 {
            let path = env::temp_dir().join(format!(
                "mineral-publisher-deepseek-smoke-{}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::create_dir(path.join("source"))?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not allocate a temporary smoke workspace".into())
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn source_root(&self) -> PathBuf {
        self.path.join("source")
    }
}

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
