//! A health scan of one workspace.
//!
//! `doctor` answers "can this workspace do its job right now?" — configuration
//! loads, the source exists or its credential is present, state and stores are
//! there, the publication ref is reachable, and the review credential is
//! available. It reports a check per question and never repairs anything.
//!
//! The scan is deliberately shallow: presence, never content. It stays usable
//! while a remote is slow or a provider is down.

use crate::runtime::WorkspaceRuntime;

use super::ApplicationError;

/// One named question and its answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorCheck {
    /// What was checked, in the operator's words.
    pub name: String,
    /// Whether the check passed.
    pub ok: bool,
    /// What was found, whether or not it passed.
    pub detail: String,
}

impl DoctorCheck {
    fn new(name: &str, ok: bool, detail: String) -> Self {
        Self {
            name: name.to_owned(),
            ok,
            detail,
        }
    }
}

/// Every check, in the order an operator reads them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorOutcome {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorOutcome {
    /// Whether any check failed.
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|check| !check.ok)
    }

    /// How many checks passed.
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|check| check.ok).count()
    }
}

/// Runs every health check against one workspace.
pub fn doctor(runtime: &WorkspaceRuntime) -> Result<DoctorOutcome, ApplicationError> {
    let mut checks = Vec::new();
    checks.push(DoctorCheck::new(
        "configuration",
        true,
        runtime.config_path.display().to_string(),
    ));

    // A local source is checked by presence of its directory; an R2 source by
    // presence of the credential it named. Neither touches the network.
    match runtime.source_kind() {
        crate::config::SourceType::Local => checks.push(DoctorCheck::new(
            "source",
            runtime
                .config
                .source
                .path
                .as_ref()
                .is_some_and(|path| path.is_dir()),
            runtime.source_description(),
        )),
        crate::config::SourceType::R2 => {
            let present = match runtime.source_r2_secret_name() {
                Ok(name) => runtime.secrets().is_present(&name),
                Err(_) => false,
            };
            checks.push(DoctorCheck::new(
                "source",
                present,
                format!("{} (credential)", runtime.source_description()),
            ));
        }
    }

    checks.push(DoctorCheck::new(
        "state",
        runtime.config.state.path.is_dir(),
        runtime.config.state.path.display().to_string(),
    ));
    checks.push(DoctorCheck::new(
        "CAS",
        runtime.cas().is_dir(),
        runtime.cas().display().to_string(),
    ));
    checks.push(DoctorCheck::new(
        "database",
        [
            runtime.document_db(),
            runtime.asset_db(),
            runtime.human_db(),
            runtime.publish_db(),
            runtime.observation_db(),
            runtime.delivery_db(),
            runtime.asset_observations_db(),
            runtime.source_materializations_db(),
        ]
        .iter()
        .all(|path| path.is_file()),
        runtime.config.state.path.display().to_string(),
    ));
    checks.push(DoctorCheck::new(
        "Git target",
        runtime.config.git.repository.is_dir(),
        runtime.config.git.repository.display().to_string(),
    ));

    // Presence only: the backup check never touches the network, so `doctor`
    // stays usable while the backup endpoint is unreachable.
    checks.push(DoctorCheck::new(
        "backup",
        true,
        match runtime.config.backup.as_ref() {
            Some(backup) if backup.enabled => {
                let repository = backup
                    .git
                    .as_ref()
                    .and_then(|git| git.repository.as_ref())
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unconfigured>".to_owned());
                format!(
                    "{repository} (lfs {})",
                    if backup.lfs.as_ref().is_some_and(|lfs| lfs.enabled) {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )
            }
            _ => "not configured".to_owned(),
        },
    ));

    checks.push(DoctorCheck::new(
        "remote/ref",
        runtime.publication_ref_present(),
        format!(
            "{} {}",
            runtime.config.git.remote, runtime.config.git.reference
        ),
    ));

    // The credential name may itself be unusable; that is a failed check, not a
    // failure of the scan.
    let (credential_ok, credential_detail) = match runtime.review_api_key_name() {
        Ok(name) => {
            let present = runtime.secrets().is_present(&name);
            (
                present,
                format!("{name}: {}", if present { "present" } else { "missing" }),
            )
        }
        Err(error) => (false, error.to_string()),
    };
    checks.push(DoctorCheck::new(
        "provider credential",
        credential_ok,
        credential_detail,
    ));

    Ok(DoctorOutcome { checks })
}
