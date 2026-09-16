//! One function per command: build the request, call a use case, render.
//!
//! A command here is deliberately dull. It resolves the workspace, hands the
//! operation API a request, streams the progress the use case reports, and
//! passes the result to [`super::output`]. Anything that looks like a decision
//! belongs in the layer below.
//!
//! Long work goes through [`OperationSupervisor`] rather than calling a use case
//! and waiting: the terminal is then one adapter of the operation API, exactly
//! like the Web adapter planned for S8.2, and both see the same progress and the
//! same typed outcome.

use std::{error::Error, fs, path::Path, sync::Arc};

use mineral_publisher::{
    application::{
        backup::{self, BackupRequest},
        publish::PublishRequest,
        review::{self, ReviewRequest},
        status,
    },
    config::{ConfigFormat, SourceType},
    operations::{
        ApplicationExecutor, OperationEvent, OperationRequest, OperationResult,
        OperationSupervisor, ReviewOperation,
    },
    runtime::WorkspaceRuntime,
};

use super::output;

/// Runs one operation to completion, streaming its progress and rendering its
/// result, and hands the typed result back for a command that needs it.
fn operate(
    workspace: WorkspaceRuntime,
    request: OperationRequest,
) -> Result<Arc<OperationResult>, Box<dyn Error>> {
    workspace.prepare()?;
    let _lease = if request.kind().is_mutating() {
        Some(mineral_publisher::service::process_lease(&workspace.config.state_file("daemon-lock.sqlite3"))
            .map_err(|_| "workspace is owned by a Web/daemon process; submit this operation through its authenticated API")?)
    } else {
        None
    };
    let supervisor =
        OperationSupervisor::new(Arc::new(ApplicationExecutor::new(Arc::new(workspace))));
    let id = supervisor.start(request)?;
    let subscription = supervisor
        .subscribe(id)
        .ok_or("the operation was accepted but cannot be observed")?;
    for event in subscription {
        match event {
            OperationEvent::Progress(progress) => output::progress(&progress),
            OperationEvent::Finished { .. } => break,
        }
    }
    let snapshot = supervisor
        .snapshot(id)
        .ok_or("the operation finished but its record is gone")?;
    match (snapshot.result(), snapshot.failure()) {
        (Some(result), _) => {
            output::operated(result);
            Ok(Arc::clone(result))
        }
        (None, Some(failure)) => {
            // The one failure whose report names what the attempt froze.
            if let Some((target, snapshot_id, files)) = failure.no_base_commit() {
                output::backup_missing_ref(target, snapshot_id, files);
            }
            Err(Box::new(failure.to_error()))
        }
        (None, None) => Err(format!("{id} finished without an outcome").into()),
    }
}

/// Creates a fresh workspace in the language its name promises.
pub fn init(path: &Path, format: ConfigFormat) -> Result<(), Box<dyn Error>> {
    if format != ConfigFormat::Toml {
        return Err("only TOML configuration is supported".into());
    }
    if path.exists() {
        return Err(format!("configuration already exists: {}", path.display()).into());
    }
    // The extension is what chooses the syntax when the file is read back, so a
    // file whose name promises a language it is not written in is refused here
    // rather than discovered by a parser later.
    if ConfigFormat::of(path) != Some(format) {
        return Err(format!(
            "a {} configuration must be named with a .{} extension, not {}",
            format.extension(),
            format.extension(),
            path.display()
        )
        .into());
    }
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format.template())?;
    let workspace = WorkspaceRuntime::load(path.to_path_buf())?;
    if workspace.source_kind() == SourceType::Local {
        fs::create_dir_all(workspace.local_source_path()?)?;
    }
    workspace.prepare()?;
    workspace.open_stores()?;
    output::initialized(&workspace);
    Ok(())
}

/// Publishes the current source state.
pub fn publish(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    operate(workspace, OperationRequest::Publish(PublishRequest::now()))?;
    Ok(())
}

/// Reports the workspace's state.
///
/// A read is not an operation: it answers immediately and needs no identity.
pub fn status(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    output::status(&status::status(&workspace)?);
    Ok(())
}

/// Scans the workspace for health, failing when a check failed.
pub fn doctor(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    let result = operate(workspace, OperationRequest::Doctor)?;
    // The scan itself succeeded either way; whether it found a problem is what
    // this command's exit status is about.
    if let OperationResult::Diagnosed(scanned) = result.as_ref()
        && scanned.failed()
    {
        return Err("one or more doctor checks failed".into());
    }
    Ok(())
}

/// Inspects or decides the human review queue.
pub fn review(workspace: WorkspaceRuntime, args: &[String]) -> Result<(), Box<dyn Error>> {
    match args.first().map(String::as_str) {
        Some("list") => {
            output::review(&review::review(&workspace, &ReviewRequest::List)?);
            Ok(())
        }
        Some("show") => {
            let subject = required(args, 1, "review show requires an ID")?;
            output::review(&review::review(&workspace, &ReviewRequest::Show(subject))?);
            Ok(())
        }
        Some("approve") => {
            let subject = required(args, 1, "review approve requires an ID")?;
            operate(
                workspace,
                OperationRequest::ReviewDecision(ReviewOperation::Approve(subject)),
            )?;
            Ok(())
        }
        Some("reject") => {
            let subject = required(args, 1, "review reject requires an ID")?;
            operate(
                workspace,
                OperationRequest::ReviewDecision(ReviewOperation::Reject(subject)),
            )?;
            Ok(())
        }
        _ => Err("review requires list, show, approve, or reject".into()),
    }
}

/// Runs or inspects the private backup.
pub fn backup(workspace: WorkspaceRuntime, args: &[String]) -> Result<(), Box<dyn Error>> {
    match args.first().map(String::as_str) {
        None => {
            operate(workspace, OperationRequest::Backup(BackupRequest::now()?))?;
            Ok(())
        }
        Some("status") => {
            output::backup_status(&backup::backup_status(&workspace)?);
            Ok(())
        }
        Some("verify") => {
            operate(workspace, OperationRequest::VerifyBackup)?;
            Ok(())
        }
        Some("init") => {
            operate(workspace, OperationRequest::BackupInit)?;
            Ok(())
        }
        Some(other) => Err(format!("unknown backup command: {other}").into()),
    }
}

/// Serves the local HTTP API until the process is stopped.
///
/// The default address is loopback; authentication is required on every bind.
pub fn web(
    workspace: WorkspaceRuntime,
    bind: Option<&str>,
    assets: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    serve(workspace, bind, assets, false)
}

pub fn serve(
    workspace: WorkspaceRuntime,
    bind: Option<&str>,
    assets: Option<&Path>,
    daemon: bool,
) -> Result<(), Box<dyn Error>> {
    let address: std::net::SocketAddr = bind
        .unwrap_or(mineral_publisher::web::DEFAULT_BIND)
        .parse()?;
    workspace.prepare()?;
    let _lease = mineral_publisher::service::process_lease(
        &workspace.config.state_file("daemon-lock.sqlite3"),
    )
    .map_err(
        |_| "another Web/daemon process already owns this workspace, or the lease cannot be opened",
    )?;
    let token = std::env::var(&workspace.config.daemon.token_env).map_err(|_| {
        format!(
            "set {} to a random token of at least 32 bytes before starting Web/daemon",
            workspace.config.daemon.token_env
        )
    })?;
    let assets = assets
        .map(Path::to_path_buf)
        .unwrap_or_else(|| mineral_publisher::web::DEFAULT_ASSETS.into());
    let state = Arc::new(
        mineral_publisher::web::WebState::persistent(
            Arc::new(workspace),
            Some(assets),
            &token,
            daemon,
        )
        .map_err(|e| e.to_string())?,
    );
    output::serving(address, state.assets());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let shutdown_tx = stop_tx.clone();
        let scheduler_state = state.clone();
        let scheduler = tokio::spawn(async move {
            if !daemon { return; }
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tokio::select! {
                    biased;
                    _ = stop_rx.changed() => break,
                    _ = interval.tick() => {
                        let s = scheduler_state.clone();
                        if !matches!(tokio::task::spawn_blocking(move || s.scheduler_tick()).await, Ok(Ok(()))) {
                            eprintln!("scheduler storage error; next tick will recheck durable claims");
                        }
                    }
                }
            }
        });
        let result = mineral_publisher::web::serve_listener_with_shutdown(state.clone(), listener, async move {
            #[cfg(unix)] {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))] { let _ = tokio::signal::ctrl_c().await; }
            let _ = shutdown_tx.send(true);
        }).await;
        let _ = stop_tx.send(true);
        let _ = scheduler.await;
        // Wait for accepted work, including verification, before releasing the lease.
        let supervisor = state.supervisor().clone();
        let _ = tokio::task::spawn_blocking(move || {
            for operation in supervisor.snapshots() { supervisor.wait(operation.id); }
        }).await;
        result
    })?;
    Ok(())
}

/// The positional argument a subcommand requires.
fn required(args: &[String], index: usize, message: &str) -> Result<String, Box<dyn Error>> {
    Ok(args.get(index).ok_or(message)?.to_owned())
}
