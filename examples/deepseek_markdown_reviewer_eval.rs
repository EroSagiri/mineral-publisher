//! Explicitly opt-in, real-provider calibration runner for human-labeled Markdown fixtures.
//!
//! `cargo run --example deepseek_markdown_reviewer_eval`
//! `cargo run --example deepseek_markdown_reviewer_eval -- <fixture-directory>`

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
    domain::{SnapshotId, SourceId},
    policy::{HumanReviewReason, PolicyIdentity, PublicPolicyDecision, ReviewRunId},
    reviewer::{
        CalibrationCaseResult, CalibrationExitStatus, CalibrationExpectation,
        CalibrationObservation, CalibrationSummary, DeepSeekApiKey, DeepSeekMarkdownReviewer,
        DeepSeekMarkdownReviewerConfig,
    },
    source::LocalSource,
    storage::{LocalContentStore, SqliteReviewRunStore},
    workflow::{PublicPolicyRun, SequentialReviewRunIdGenerator},
};

const DEFAULT_API_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-flash";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

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
    let cases = load_cases(&fixture_directory)?;
    if cases.is_empty() {
        return Err("the calibration corpus contains no paired Markdown fixtures".into());
    }

    let api_base_url = env::var("MINERAL_DEEPSEEK_API_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_API_BASE_URL.to_owned());
    let model = env::var("MINERAL_DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
    let api_key = DeepSeekApiKey::from_env("MINERAL_DEEPSEEK_API_KEY")?;
    let config = DeepSeekMarkdownReviewerConfig::new(
        &api_base_url,
        model,
        api_key,
        TIMEOUT,
        MAX_INPUT_BYTES,
        MAX_RESPONSE_BYTES,
    )?
    .with_policy_instruction_from_env("MINERAL_DEEPSEEK_POLICY_PROMPT")?;

    let workspace = TemporaryWorkspace::new()?;
    let content_store = LocalContentStore::new(workspace.path().join("content-store"));
    let source = LocalSource::new(
        &fixture_directory,
        SourceId::new("deepseek-markdown-reviewer-calibration")?,
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
    let mut review_ids = SequentialReviewRunIdGenerator::new(ReviewRunId::new(1)?);

    println!("provider: DeepSeek");
    println!("model: {}", reviewer.config().model());
    println!("endpoint: {}", reviewer.config().endpoint());
    println!("thinking: enabled (high)");
    println!("prompt_version: {}", reviewer.prompt_version());
    println!("prompt_sha256: {}", reviewer.prompt_sha256());
    println!("corpus: {}", fixture_directory.display());
    println!();

    let policy_result = PublicPolicyRun::execute(
        &snapshot,
        &content_store,
        &reviewer,
        &review_store,
        &policy,
        &mut review_ids,
    )?;
    let mut observations = observations_by_path(&policy_result);
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let fixture_name = case
            .markdown_path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or("fixture filename is not valid Unicode")?
            .to_owned();
        let observation = observations
            .remove(&fixture_name)
            .ok_or_else(|| format!("no policy result was produced for {fixture_name}"))?;
        let result = CalibrationCaseResult::evaluate(fixture_name, case.expectation, observation);
        println!("{}\n", result.render());
        results.push(result);
    }

    let summary = CalibrationSummary::from_results(&results);
    println!("{}", summary.render());
    Ok(summary.exit_status())
}

fn observations_by_path(
    result: &mineral_publisher::workflow::PublicPolicyRunResult,
) -> BTreeMap<String, CalibrationObservation> {
    let mut observations = BTreeMap::new();
    for document in result.private_documents() {
        observations.insert(
            document.path().as_str().to_owned(),
            CalibrationObservation::BlockedBeforeReview(format!(
                "deterministic privacy filter: {:?}",
                document.reasons()
            )),
        );
    }
    for document in result.invalid_privacy_documents() {
        observations.insert(
            document.path().as_str().to_owned(),
            CalibrationObservation::BlockedBeforeReview(format!(
                "invalid privacy metadata: {}",
                document.reason()
            )),
        );
    }
    for run in result.document_outcomes() {
        let observation = match run.decision() {
            PublicPolicyDecision::ProgramIssues(issues) => {
                CalibrationObservation::BlockedBeforeReview(format!(
                    "deterministic program check: {issues:?}"
                ))
            }
            PublicPolicyDecision::ReviewApproved | PublicPolicyDecision::ReviewRejected => {
                CalibrationObservation::ReviewerReport(
                    run.reviewer_report()
                        .expect("review decisions retain their report")
                        .clone(),
                )
            }
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested) => {
                CalibrationObservation::ReviewerReport(
                    run.reviewer_report()
                        .expect("reviewer-requested human review retains its report")
                        .clone(),
                )
            }
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(error)) => {
                CalibrationObservation::ProviderError(error.clone())
            }
        };
        observations.insert(run.content_path().as_str().to_owned(), observation);
    }
    observations
}

struct CalibrationCase {
    markdown_path: PathBuf,
    expectation: CalibrationExpectation,
}

fn load_cases(directory: &Path) -> Result<Vec<CalibrationCase>, Box<dyn Error>> {
    if !directory.is_dir() {
        return Err(format!("fixture directory does not exist: {}", directory.display()).into());
    }
    let mut markdown_paths = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    markdown_paths.retain(|path| path.extension() == Some(OsStr::new("md")));
    markdown_paths.sort();

    markdown_paths
        .into_iter()
        .map(|markdown_path| {
            let expectation_path = markdown_path.with_extension("yaml");
            let yaml = fs::read_to_string(&expectation_path).map_err(|error| {
                format!(
                    "could not read expectation {}: {error}",
                    expectation_path.display()
                )
            })?;
            let expectation = CalibrationExpectation::from_yaml(&yaml).map_err(|error| {
                format!(
                    "invalid expectation {}: {error}",
                    expectation_path.display()
                )
            })?;
            Ok(CalibrationCase {
                markdown_path,
                expectation,
            })
        })
        .collect()
}

fn fixture_directory() -> Result<PathBuf, Box<dyn Error>> {
    let mut arguments = env::args_os();
    let program = arguments.next().unwrap_or_default();
    let path = arguments.next().map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("fixtures")
            .join("markdown_reviewer_eval")
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

struct TemporaryWorkspace {
    path: PathBuf,
}

impl TemporaryWorkspace {
    fn new() -> Result<Self, Box<dyn Error>> {
        for attempt in 0..100_u32 {
            let path = env::temp_dir().join(format!(
                "mineral-publisher-deepseek-eval-{}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not allocate a temporary calibration workspace".into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
