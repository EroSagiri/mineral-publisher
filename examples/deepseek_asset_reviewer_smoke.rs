//! Opt-in real DeepSeek visual review through Asset Program Check and Asset Policy.
//! `cargo run --example deepseek_asset_reviewer_smoke -- <image.jpg|image.png>`

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use mineral_publisher::{
    content::{AssetDependencyGraph, SnapshotMarkdownAnalyzer},
    domain::{ContentPath, SnapshotId, SourceId},
    policy::{
        PolicyIdentity, ReviewCandidate, ReviewDecision, ReviewReasonCode, ReviewRunId, Reviewer,
        ReviewerError, ReviewerReport,
    },
    reviewer::{DeepSeekApiKey, DeepSeekAssetReviewer, DeepSeekAssetReviewerConfig},
    source::LocalSource,
    storage::{LocalContentStore, SqliteAssetReviewRunStore, SqliteReviewRunStore},
    workflow::{
        AssetHumanReviewReason, AssetReviewDisposition, AssetReviewRunId, AssetReviewWorkflow,
        AssetReviewWorkflowInput, CandidateAssetSet, PublicPolicyRun,
        SequentialAssetReviewRunIdGenerator, SequentialReviewRunIdGenerator,
    },
};

const API_BASE_URL: &str = "https://api.deepseek.com";
const TIMEOUT: Duration = Duration::from_secs(45);
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

fn main() -> Result<(), Box<dyn Error>> {
    let input = parse_input()?;
    let api_key = DeepSeekApiKey::from_env("MINERAL_DEEPSEEK_API_KEY")?;
    let workspace = TemporaryWorkspace::new()?;
    let filename = input.file_name().ok_or("input path has no filename")?;
    let staged = workspace.source().join(filename);
    fs::copy(&input, &staged)?;
    fs::write(
        workspace.source().join("smoke.md"),
        format!("![]({})\n", filename.to_string_lossy()),
    )?;

    let content_store = LocalContentStore::new(workspace.path.join("cas"));
    let source = LocalSource::new(
        workspace.source(),
        SourceId::new("deepseek-asset-reviewer-smoke")?,
        content_store.clone(),
    );
    let snapshot = source.snapshot(SnapshotId::new(1)?, SystemTime::now())?;
    let content_path = ContentPath::new(filename.to_string_lossy().replace('\\', "/"))?;
    let markdown_path = ContentPath::new("smoke.md")?;
    let markdown_reviewer = FixedReviewer(ReviewerReport::new(
        ReviewDecision::Approve,
        vec![ReviewReasonCode::OrdinaryPersonalContent],
        "Smoke document contains only the image dependency.",
    )?);
    let markdown_store =
        SqliteReviewRunStore::open(workspace.path.join("markdown-reviews.sqlite3"))?;
    let markdown_policy = PolicyIdentity::new(
        "smoke-public-policy",
        "smoke-v1",
        mineral_publisher::domain::Sha256::digest(b"smoke-v1"),
    )?;
    let mut markdown_ids = SequentialReviewRunIdGenerator::new(ReviewRunId::new(1)?);
    let public_result = PublicPolicyRun::execute(
        &snapshot,
        &content_store,
        &markdown_reviewer,
        &markdown_store,
        &markdown_policy,
        &mut markdown_ids,
    )?;
    let analysis =
        SnapshotMarkdownAnalyzer::new(content_store.clone()).analyze(&snapshot, &markdown_path)?;
    let graph = AssetDependencyGraph::build(snapshot.id(), &[analysis]);
    let candidates = CandidateAssetSet::select(&public_result, &graph)?;
    if !candidates.contains(&content_path) {
        return Err("image was not selected as an approved Markdown dependency".into());
    }
    let config = DeepSeekAssetReviewerConfig::new(
        API_BASE_URL,
        api_key,
        TIMEOUT,
        MAX_IMAGE_BYTES,
        MAX_RESPONSE_BYTES,
    )?
    .with_policy_instruction_from_env("MINERAL_DEEPSEEK_ASSET_POLICY_PROMPT")?;
    let reviewer = DeepSeekAssetReviewer::new(config, content_store.clone())?;
    let policy = PolicyIdentity::new(
        "asset-visual-policy",
        reviewer.prompt_version(),
        reviewer.prompt_sha256(),
    )?;
    let store = SqliteAssetReviewRunStore::open(workspace.path.join("asset-reviews.sqlite3"))?;
    let mut ids = SequentialAssetReviewRunIdGenerator::new(AssetReviewRunId::new(1)?);
    let result = AssetReviewWorkflow::execute(
        AssetReviewWorkflowInput::new(&candidates, &snapshot, &content_store, &policy),
        &reviewer,
        &store,
        &mut ids,
    )?;
    let entry = result.entries().first().ok_or("no asset review outcome")?;

    println!("path: {}", input.display());
    println!(
        "source_sha256: {}",
        entry
            .outcome()
            .sha256()
            .ok_or("asset has no source identity")?
    );
    println!("provider: DeepSeek");
    println!("model: {}", reviewer.config().model());
    println!("endpoint: {}", reviewer.config().endpoint());
    println!("thinking: {:?}", reviewer.config().thinking());
    println!("detail: {:?}", reviewer.config().detail());
    println!("prompt_version: {}", reviewer.prompt_version());
    println!("prompt_sha256: {}", reviewer.prompt_sha256());
    match entry.outcome().disposition() {
        AssetReviewDisposition::Blocked => {
            println!("policy_decision: Blocked");
            println!("reviewer_result: NotCalled");
        }
        AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings) => {
            println!("policy_decision: NeedsHumanReview");
            println!("reviewer_result: NotCalled");
        }
        AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::ReviewerFailed(error)) => {
            println!("policy_decision: NeedsHumanReview");
            println!("reviewer_result: Error");
            println!("reviewer_error_category: {:?}", error.kind());
            println!("reviewer_error_message: {}", error.message());
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
        AssetReviewDisposition::Reviewed(decision) => {
            println!("policy_decision: {:?}", decision);
            println!("reviewer_result: {:?}", decision);
            if let Some(report) = entry.outcome().reviewer_report() {
                println!("reason_codes:");
                for reason in report.reason_codes() {
                    println!(
                        "  - {}",
                        serde_json::to_value(reason)?.as_str().unwrap_or("invalid")
                    );
                }
                println!("summary: {}", report.summary());
            }
        }
    }
    Ok(())
}

fn parse_input() -> Result<PathBuf, Box<dyn Error>> {
    let mut args = env::args_os();
    let program = args.next().unwrap_or_default();
    let Some(path) = args.next() else {
        return Err(format!(
            "usage: {} <image.jpg|image.png>",
            Path::new(&program).display()
        )
        .into());
    };
    if args.next().is_some() {
        return Err("expected exactly one image path".into());
    }
    let path = PathBuf::from(path);
    let supported = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png"));
    if !supported {
        return Err("the input must be a JPEG or PNG image".into());
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
                "mineral-deepseek-asset-smoke-{}-{attempt}",
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
        Err("could not allocate temporary smoke workspace".into())
    }
    fn source(&self) -> PathBuf {
        self.path.join("source")
    }
}
impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct FixedReviewer(ReviewerReport);
impl Reviewer for FixedReviewer {
    fn review(&self, _candidate: &ReviewCandidate) -> Result<ReviewerReport, ReviewerError> {
        Ok(self.0.clone())
    }
}
