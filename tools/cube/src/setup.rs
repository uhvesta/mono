use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::CubeError;
use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::metadata::WorkspaceRecord;
use crate::store::{Store, WorkspaceSetupState};

pub const SETUP_FILE_RELATIVE: &str = ".cube/setup.yaml";
const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SetupConfig {
    pub version: u32,
    #[serde(default)]
    pub steps: Vec<SetupStep>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SetupStep {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub run_when: RunPolicy,
    #[serde(default)]
    pub fingerprint: Vec<FingerprintInput>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunPolicy {
    /// Run only when no successful run has been recorded for this step.
    OnCreate,
    /// Run when the fingerprint of tracked inputs differs from the
    /// last recorded fingerprint, or on first run. This is the default.
    #[default]
    OnFingerprintChange,
    /// Always run, regardless of recorded state.
    Always,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum FingerprintInput {
    File { file: String },
    Value { value: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupReport {
    pub steps: Vec<StepOutcome>,
}

impl SetupReport {
    pub fn empty() -> Self {
        Self { steps: Vec::new() }
    }

    pub fn ran_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Ran))
            .count()
    }

    pub fn skipped_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Skipped { .. }))
            .count()
    }

    pub fn failed_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Failed { .. }))
            .count()
    }

    pub fn first_failure(&self) -> Option<&StepOutcome> {
        self.steps
            .iter()
            .find(|step| matches!(step.status, StepStatus::Failed { .. }))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub id: String,
    #[serde(flatten)]
    pub status: StepStatus,
    pub fingerprint: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StepStatus {
    Ran,
    Skipped { reason: SkipReason },
    Failed { error: String },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    AlreadyRan,
    FingerprintUnchanged,
}

pub fn setup_config_path(workspace_path: &Path) -> PathBuf {
    workspace_path.join(SETUP_FILE_RELATIVE)
}

pub fn read_setup_config(workspace_path: &Path) -> Result<Option<SetupConfig>, CubeError> {
    let path = setup_config_path(workspace_path);
    let raw = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CubeError::Io(source)),
    };
    let config: SetupConfig = serde_yaml::from_str(&raw)
        .map_err(|source| CubeError::InvalidArgument(format!("failed to parse `{}`: {source}", path.display())))?;
    if config.version != SUPPORTED_VERSION {
        return Err(CubeError::InvalidArgument(format!(
            "unsupported setup config version `{}` in `{}`",
            config.version,
            path.display()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for step in &config.steps {
        if step.id.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "setup step in `{}` is missing an id",
                path.display()
            )));
        }
        if !seen.insert(step.id.clone()) {
            return Err(CubeError::InvalidArgument(format!(
                "duplicate setup step id `{}` in `{}`",
                step.id,
                path.display()
            )));
        }
        if step.command.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "setup step `{}` has an empty command in `{}`",
                step.id,
                path.display()
            )));
        }
    }
    Ok(Some(config))
}

pub fn compute_fingerprint(workspace_path: &Path, step: &SetupStep) -> Result<String, CubeError> {
    let mut hasher = Sha256::new();
    hasher.update(step.command.as_bytes());
    hasher.update([0u8]);
    for input in &step.fingerprint {
        match input {
            FingerprintInput::File { file } => {
                hasher.update(b"file:");
                hasher.update(file.as_bytes());
                hasher.update([0u8]);
                let candidate = workspace_path.join(file);
                match fs::read(&candidate) {
                    Ok(bytes) => hasher.update(&bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        hasher.update(b"<missing>");
                    }
                    Err(source) => return Err(CubeError::Io(source)),
                }
                hasher.update([0u8]);
            }
            FingerprintInput::Value { value } => {
                hasher.update(b"value:");
                hasher.update(value.as_bytes());
                hasher.update([0u8]);
            }
        }
    }
    let digest = hasher.finalize();
    Ok(hex_digest(&digest))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn run_setup_engine(
    store: &Store,
    runner: &dyn CommandRunner,
    workspace: &WorkspaceRecord,
    config: &SetupConfig,
    now_epoch_s: i64,
) -> Result<SetupReport, CubeError> {
    let mut report = SetupReport::empty();
    for step in &config.steps {
        let started = Instant::now();
        let fingerprint = compute_fingerprint(&workspace.workspace_path, step)?;
        let stored = store.get_workspace_setup_state(&workspace.repo, &workspace.workspace_id, &step.id)?;
        let action = decide_action(step.run_when, stored.as_ref(), &fingerprint);
        match action {
            StepAction::Skip(reason) => {
                report.steps.push(StepOutcome {
                    id: step.id.clone(),
                    status: StepStatus::Skipped { reason },
                    fingerprint,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
            StepAction::Run => match invoke_step(runner, &workspace.workspace_path, step) {
                Ok(()) => {
                    store.upsert_workspace_setup_state(&WorkspaceSetupState {
                        repo: workspace.repo.clone(),
                        workspace_id: workspace.workspace_id.clone(),
                        step_id: step.id.clone(),
                        fingerprint: fingerprint.clone(),
                        last_run_epoch_s: now_epoch_s,
                    })?;
                    report.steps.push(StepOutcome {
                        id: step.id.clone(),
                        status: StepStatus::Ran,
                        fingerprint,
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                }
                Err(error) => {
                    let duration_ms = started.elapsed().as_millis() as u64;
                    report.steps.push(StepOutcome {
                        id: step.id.clone(),
                        status: StepStatus::Failed {
                            error: error.to_string(),
                        },
                        fingerprint,
                        duration_ms,
                    });
                    // Stop on first failure: subsequent steps may depend on
                    // this one having run.
                    return Ok(report);
                }
            },
        }
    }
    Ok(report)
}

#[derive(Debug)]
enum StepAction {
    Run,
    Skip(SkipReason),
}

fn decide_action(policy: RunPolicy, stored: Option<&WorkspaceSetupState>, fingerprint: &str) -> StepAction {
    match policy {
        RunPolicy::Always => StepAction::Run,
        RunPolicy::OnCreate => match stored {
            Some(_) => StepAction::Skip(SkipReason::AlreadyRan),
            None => StepAction::Run,
        },
        RunPolicy::OnFingerprintChange => match stored {
            Some(state) if state.fingerprint == fingerprint => StepAction::Skip(SkipReason::FingerprintUnchanged),
            _ => StepAction::Run,
        },
    }
}

fn invoke_step(runner: &dyn CommandRunner, workspace_path: &Path, step: &SetupStep) -> Result<(), CubeError> {
    let parts = shlex::split(&step.command).ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "setup step `{}` has an unparseable command: {}",
            step.id, step.command
        ))
    })?;
    let mut iter = parts.into_iter();
    let program = iter
        .next()
        .ok_or_else(|| CubeError::InvalidArgument(format!("setup step `{}` resolved to an empty command", step.id)))?;
    let args: Vec<String> = iter.collect();
    runner.run(&CommandInvocation {
        cwd: workspace_path.to_path_buf(),
        program,
        args,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{
        FingerprintInput, RunPolicy, SetupConfig, SetupStep, compute_fingerprint, decide_action, read_setup_config,
        setup_config_path,
    };
    use crate::store::WorkspaceSetupState;

    use super::{SkipReason, StepAction};

    #[test]
    fn read_setup_config_returns_none_when_file_missing() {
        let temp = TempDir::new().unwrap();
        assert!(read_setup_config(temp.path()).unwrap().is_none());
    }

    #[test]
    fn read_setup_config_parses_full_example() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(setup_config_path(temp.path()).parent().unwrap()).unwrap();
        fs::write(
            setup_config_path(temp.path()),
            r#"version: 1
steps:
  - id: secrets
    command: ./tools/dev/decode-secrets.sh
    run_when: on-create
  - id: deps
    command: pnpm install --frozen-lockfile
    fingerprint:
      - file: pnpm-lock.yaml
      - value: v3
"#,
        )
        .unwrap();

        let config = read_setup_config(temp.path()).unwrap().unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].id, "secrets");
        assert_eq!(config.steps[0].run_when, RunPolicy::OnCreate);
        assert_eq!(config.steps[1].id, "deps");
        assert_eq!(config.steps[1].run_when, RunPolicy::OnFingerprintChange);
        assert_eq!(
            config.steps[1].fingerprint,
            vec![
                FingerprintInput::File {
                    file: "pnpm-lock.yaml".to_string(),
                },
                FingerprintInput::Value {
                    value: "v3".to_string(),
                },
            ]
        );
    }

    #[test]
    fn read_setup_config_rejects_duplicate_ids() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(setup_config_path(temp.path()).parent().unwrap()).unwrap();
        fs::write(
            setup_config_path(temp.path()),
            r#"version: 1
steps:
  - id: deps
    command: a
  - id: deps
    command: b
"#,
        )
        .unwrap();
        let err = read_setup_config(temp.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate setup step id"));
    }

    #[test]
    fn fingerprint_changes_when_file_contents_change() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("lock"), b"a").unwrap();
        let step = SetupStep {
            id: "deps".to_string(),
            command: "echo".to_string(),
            run_when: RunPolicy::OnFingerprintChange,
            fingerprint: vec![FingerprintInput::File {
                file: "lock".to_string(),
            }],
        };
        let first = compute_fingerprint(temp.path(), &step).unwrap();

        fs::write(temp.path().join("lock"), b"b").unwrap();
        let second = compute_fingerprint(temp.path(), &step).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn fingerprint_changes_when_command_changes() {
        let temp = TempDir::new().unwrap();
        let mut step = SetupStep {
            id: "deps".to_string(),
            command: "a".to_string(),
            run_when: RunPolicy::OnFingerprintChange,
            fingerprint: vec![],
        };
        let first = compute_fingerprint(temp.path(), &step).unwrap();
        step.command = "b".to_string();
        let second = compute_fingerprint(temp.path(), &step).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn missing_fingerprint_file_is_distinct_from_empty_file() {
        let temp = TempDir::new().unwrap();
        let step = SetupStep {
            id: "deps".to_string(),
            command: "echo".to_string(),
            run_when: RunPolicy::OnFingerprintChange,
            fingerprint: vec![FingerprintInput::File {
                file: "lock".to_string(),
            }],
        };
        let missing = compute_fingerprint(temp.path(), &step).unwrap();

        fs::write(temp.path().join("lock"), b"").unwrap();
        let empty = compute_fingerprint(temp.path(), &step).unwrap();
        assert_ne!(missing, empty);
    }

    fn stored(fingerprint: &str) -> WorkspaceSetupState {
        WorkspaceSetupState {
            repo: "mono".to_string(),
            workspace_id: "mono-agent-001".to_string(),
            step_id: "deps".to_string(),
            fingerprint: fingerprint.to_string(),
            last_run_epoch_s: 0,
        }
    }

    #[test]
    fn decide_action_on_create_runs_only_on_first_run() {
        assert!(matches!(
            decide_action(RunPolicy::OnCreate, None, "abc"),
            StepAction::Run
        ));
        let ran = stored("abc");
        assert!(matches!(
            decide_action(RunPolicy::OnCreate, Some(&ran), "abc"),
            StepAction::Skip(SkipReason::AlreadyRan)
        ));
        // Even with a different fingerprint, on-create stays skipped after first run.
        assert!(matches!(
            decide_action(RunPolicy::OnCreate, Some(&ran), "different"),
            StepAction::Skip(SkipReason::AlreadyRan)
        ));
    }

    #[test]
    fn decide_action_on_fingerprint_change_runs_when_changed_or_unset() {
        assert!(matches!(
            decide_action(RunPolicy::OnFingerprintChange, None, "abc"),
            StepAction::Run
        ));
        let ran = stored("abc");
        assert!(matches!(
            decide_action(RunPolicy::OnFingerprintChange, Some(&ran), "abc"),
            StepAction::Skip(SkipReason::FingerprintUnchanged)
        ));
        assert!(matches!(
            decide_action(RunPolicy::OnFingerprintChange, Some(&ran), "different"),
            StepAction::Run
        ));
    }

    #[test]
    fn decide_action_always_runs() {
        let ran = stored("abc");
        assert!(matches!(decide_action(RunPolicy::Always, None, "abc"), StepAction::Run));
        assert!(matches!(
            decide_action(RunPolicy::Always, Some(&ran), "abc"),
            StepAction::Run
        ));
    }

    #[test]
    fn config_steps_default_to_on_fingerprint_change() {
        let raw = r#"version: 1
steps:
  - id: deps
    command: pnpm install
"#;
        let parsed: SetupConfig = serde_yaml::from_str(raw).unwrap();
        assert_eq!(parsed.steps[0].run_when, RunPolicy::OnFingerprintChange);
    }
}
