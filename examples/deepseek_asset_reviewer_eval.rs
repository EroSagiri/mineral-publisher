//! Explicitly opt-in, real-provider calibration runner for human-labeled image fixtures.
//!
//! `cargo run --example deepseek_asset_reviewer_eval`
//! `cargo run --example deepseek_asset_reviewer_eval -- <fixture-directory>`

use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, SystemTime},
};

use mineral_publisher::{
    content::{AssetDependencyGraph, SnapshotMarkdownAnalyzer},
    domain::{ContentPath, Sha256, SnapshotId, SourceId},
    policy::{
        PolicyIdentity, ReviewCandidate, ReviewDecision, ReviewReasonCode, ReviewRunId, Reviewer,
        ReviewerError, ReviewerReport,
    },
    reviewer::{
        AssetCalibrationCaseResult, AssetCalibrationObservation, CalibrationExitStatus,
        CalibrationSummary, DEFAULT_DEEPSEEK_ASSET_MODEL, DeepSeekApiKey, DeepSeekAssetReviewer,
        DeepSeekAssetReviewerConfig, DeepSeekImageDetail, DeepSeekReasoningEffort,
        DeepSeekThinking, load_asset_calibration_cases,
    },
    source::LocalSource,
    storage::{LocalContentStore, SqliteAssetReviewRunStore, SqliteReviewRunStore},
    workflow::{
        AssetHumanReviewReason, AssetReviewDisposition, AssetReviewRunId, AssetReviewWorkflow,
        AssetReviewWorkflowInput, CandidateAssetSet, PublicPolicyRun,
        SequentialAssetReviewRunIdGenerator, SequentialReviewRunIdGenerator,
    },
};

const DEFAULT_API_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_TIMEOUT_SECONDS: u64 = 45;
const DEFAULT_MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(status) => ExitCode::from(status.code() as u8),
        Err(error) => {
            eprintln!("calibration runner error: {error}");
            ExitCode::from(CalibrationExitStatus::ExecutionError.code() as u8)
        }
    }
}

fn run() -> Result<CalibrationExitStatus, Box<dyn Error>> {
    let fixture_directory = fixture_directory()?;
    let cases = load_asset_calibration_cases(&fixture_directory)?;
    if cases.is_empty() {
        return Err("the calibration corpus contains no paired image fixtures".into());
    }

    let workspace = TemporaryWorkspace::new()?;
    let mut neutral_to_fixture = BTreeMap::new();
    let mut markdown = String::new();
    for (index, case) in cases.iter().enumerate() {
        let extension = case
            .image_path()
            .extension()
            .and_then(OsStr::to_str)
            .ok_or("fixture extension is not valid Unicode")?
            .to_ascii_lowercase();
        let neutral_name = format!("case-{:03}.{extension}", index + 1);
        fs::copy(case.image_path(), workspace.source().join(&neutral_name))?;
        markdown.push_str(&format!("![]({neutral_name})\n"));
        let fixture_name = case
            .image_path()
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or("fixture filename is not valid Unicode")?
            .to_owned();
        neutral_to_fixture.insert(neutral_name, fixture_name);
    }
    fs::write(workspace.source().join("calibration.md"), markdown)?;

    let content_store = LocalContentStore::new(workspace.path().join("content-store"));
    let source = LocalSource::new(
        workspace.source(),
        SourceId::new("deepseek-asset-reviewer-calibration")?,
        content_store.clone(),
    );
    let snapshot = source.snapshot(SnapshotId::new(1)?, SystemTime::now())?;
    let markdown_path = ContentPath::new("calibration.md")?;
    let markdown_reviewer = FixedReviewer(ReviewerReport::new(
        ReviewDecision::Approve,
        vec![ReviewReasonCode::OrdinaryPersonalContent],
        "Calibration document contains only image dependencies.",
    )?);
    let markdown_store =
        SqliteReviewRunStore::open(workspace.path().join("markdown-reviews.sqlite3"))?;
    let markdown_policy = PolicyIdentity::new(
        "asset-calibration-input-policy",
        "asset-calibration-input-v1",
        Sha256::digest(b"asset-calibration-input-v1"),
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

    let reviewer = build_reviewer(content_store.clone())?;
    let asset_policy = PolicyIdentity::new(
        "asset-visual-policy",
        reviewer.prompt_version(),
        reviewer.prompt_sha256(),
    )?;

    println!("provider: DeepSeek");
    println!("model: {}", reviewer.config().model());
    println!("endpoint: {}", reviewer.config().endpoint());
    println!("thinking: {}", thinking_name(reviewer.config().thinking()));
    println!(
        "reasoning_effort: {}",
        reasoning_effort_name(reviewer.config().reasoning_effort())
    );
    println!("detail: {}", detail_name(reviewer.config().detail()));
    println!("prompt_version: {}", reviewer.prompt_version());
    println!("prompt_sha256: {}", reviewer.prompt_sha256());
    println!("corpus: {}", fixture_directory.display());
    println!();

    let review_store =
        SqliteAssetReviewRunStore::open(workspace.path().join("asset-reviews.sqlite3"))?;
    let mut review_ids = SequentialAssetReviewRunIdGenerator::new(AssetReviewRunId::new(1)?);
    let workflow_result = AssetReviewWorkflow::execute(
        AssetReviewWorkflowInput::new(&candidates, &snapshot, &content_store, &asset_policy),
        &reviewer,
        &review_store,
        &mut review_ids,
    )?;
    let mut observations = BTreeMap::new();
    for entry in workflow_result.entries() {
        let observation = match entry.outcome().disposition() {
            AssetReviewDisposition::Blocked => {
                AssetCalibrationObservation::BlockedBeforeReview(format!(
                    "deterministic asset policy: {:?}",
                    entry.outcome().findings()
                ))
            }
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings) => {
                AssetCalibrationObservation::BlockedBeforeReview(format!(
                    "deterministic asset policy: {:?}",
                    entry.outcome().findings()
                ))
            }
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::ReviewerFailed(
                error,
            )) => AssetCalibrationObservation::ProviderError(error.clone()),
            AssetReviewDisposition::Reviewed(_) => AssetCalibrationObservation::ReviewerReport(
                entry
                    .outcome()
                    .reviewer_report()
                    .expect("reviewed asset retains its reviewer report")
                    .clone(),
            ),
        };
        observations.insert(entry.content_path().as_str().to_owned(), observation);
    }

    let mut results = Vec::with_capacity(cases.len());
    for (index, case) in cases.into_iter().enumerate() {
        let neutral_name = format!(
            "case-{:03}.{}",
            index + 1,
            case.image_path()
                .extension()
                .and_then(OsStr::to_str)
                .expect("validated fixture extension")
                .to_ascii_lowercase()
        );
        let fixture_name = neutral_to_fixture
            .get(&neutral_name)
            .expect("neutral fixture mapping is complete")
            .clone();
        let observation = observations
            .remove(&neutral_name)
            .ok_or_else(|| format!("no asset policy result was produced for {fixture_name}"))?;
        let result = AssetCalibrationCaseResult::evaluate(
            fixture_name,
            case.into_expectation(),
            observation,
        );
        println!("{}\n", result.render());
        results.push(result);
    }

    let summary = CalibrationSummary::from_asset_results(&results);
    println!("{}", summary.render());
    Ok(summary.exit_status())
}

fn build_reviewer(
    content_store: LocalContentStore,
) -> Result<DeepSeekAssetReviewer, Box<dyn Error>> {
    let api_base_url = env::var("MINERAL_DEEPSEEK_API_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_API_BASE_URL.to_owned());
    let model = env::var("MINERAL_DEEPSEEK_MODEL")
        .unwrap_or_else(|_| DEFAULT_DEEPSEEK_ASSET_MODEL.to_owned());
    let timeout = Duration::from_secs(env_u64(
        "MINERAL_DEEPSEEK_ASSET_TIMEOUT_SECONDS",
        DEFAULT_TIMEOUT_SECONDS,
    )?);
    let max_image_bytes = env_usize(
        "MINERAL_DEEPSEEK_ASSET_MAX_IMAGE_BYTES",
        DEFAULT_MAX_IMAGE_BYTES,
    )?;
    let max_response_bytes = env_usize(
        "MINERAL_DEEPSEEK_ASSET_MAX_RESPONSE_BYTES",
        DEFAULT_MAX_RESPONSE_BYTES,
    )?;
    let config = DeepSeekAssetReviewerConfig::new(
        &api_base_url,
        DeepSeekApiKey::from_env("MINERAL_DEEPSEEK_API_KEY")?,
        timeout,
        max_image_bytes,
        max_response_bytes,
    )?
    .with_model(model)?
    .with_policy_instruction_from_env("MINERAL_DEEPSEEK_ASSET_POLICY_PROMPT")?
    .with_thinking(parse_thinking()?)
    .with_reasoning_effort(parse_reasoning_effort()?)
    .with_detail(parse_detail()?);
    Ok(DeepSeekAssetReviewer::new(config, content_store)?)
}

fn env_u64(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| format!("{name} must be a positive integer").into()),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode").into()),
    }
}

fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    let value = env_u64(name, default as u64)?;
    usize::try_from(value).map_err(|_| format!("{name} is too large").into())
}

fn env_choice(name: &str, default: &str) -> Result<String, Box<dyn Error>> {
    match env::var(name) {
        Ok(value) => Ok(value.to_ascii_lowercase()),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode").into()),
    }
}

fn parse_thinking() -> Result<DeepSeekThinking, Box<dyn Error>> {
    match env_choice("MINERAL_DEEPSEEK_ASSET_THINKING", "enabled")?.as_str() {
        "enabled" => Ok(DeepSeekThinking::Enabled),
        "disabled" => Ok(DeepSeekThinking::Disabled),
        _ => Err("MINERAL_DEEPSEEK_ASSET_THINKING must be enabled or disabled".into()),
    }
}

fn parse_reasoning_effort() -> Result<DeepSeekReasoningEffort, Box<dyn Error>> {
    match env_choice("MINERAL_DEEPSEEK_ASSET_REASONING_EFFORT", "high")?.as_str() {
        "low" => Ok(DeepSeekReasoningEffort::Low),
        "medium" => Ok(DeepSeekReasoningEffort::Medium),
        "high" => Ok(DeepSeekReasoningEffort::High),
        _ => Err("MINERAL_DEEPSEEK_ASSET_REASONING_EFFORT must be low, medium, or high".into()),
    }
}

fn parse_detail() -> Result<DeepSeekImageDetail, Box<dyn Error>> {
    match env_choice("MINERAL_DEEPSEEK_ASSET_DETAIL", "original")?.as_str() {
        "original" => Ok(DeepSeekImageDetail::Original),
        "high" => Ok(DeepSeekImageDetail::High),
        "low" => Ok(DeepSeekImageDetail::Low),
        "auto" => Ok(DeepSeekImageDetail::Auto),
        _ => Err("MINERAL_DEEPSEEK_ASSET_DETAIL must be original, high, low, or auto".into()),
    }
}

fn detail_name(value: DeepSeekImageDetail) -> &'static str {
    match value {
        DeepSeekImageDetail::Original => "original",
        DeepSeekImageDetail::High => "high",
        DeepSeekImageDetail::Low => "low",
        DeepSeekImageDetail::Auto => "auto",
    }
}

fn thinking_name(value: DeepSeekThinking) -> &'static str {
    match value {
        DeepSeekThinking::Enabled => "enabled",
        DeepSeekThinking::Disabled => "disabled",
    }
}

fn reasoning_effort_name(value: DeepSeekReasoningEffort) -> &'static str {
    match value {
        DeepSeekReasoningEffort::Low => "low",
        DeepSeekReasoningEffort::Medium => "medium",
        DeepSeekReasoningEffort::High => "high",
    }
}

fn fixture_directory() -> Result<PathBuf, Box<dyn Error>> {
    let mut arguments = env::args_os();
    let program = arguments.next().unwrap_or_default();
    let path = arguments.next().map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("fixtures")
            .join("asset_reviewer_eval")
    });
    if arguments.next().is_some() {
        return Err(format!(
            "usage: {} [fixture-directory]",
            Path::new(&program).display()
        )
        .into());
    }
    Ok(path)
}

struct FixedReviewer(ReviewerReport);

impl Reviewer for FixedReviewer {
    fn review(&self, _candidate: &ReviewCandidate) -> Result<ReviewerReport, ReviewerError> {
        Ok(self.0.clone())
    }
}

struct TemporaryWorkspace {
    path: PathBuf,
}

impl TemporaryWorkspace {
    fn new() -> Result<Self, Box<dyn Error>> {
        for attempt in 0..100_u32 {
            let path = env::temp_dir().join(format!(
                "mineral-publisher-deepseek-asset-eval-{}-{attempt}",
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
        Err("could not allocate a temporary calibration workspace".into())
    }

    fn path(&self) -> &Path {
        &self.path
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
