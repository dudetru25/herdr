use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::schema::{
    WorkerRunArtifact, WorkerRunArtifactKind, WorkerRunCancellationDisposition, WorkerRunChange,
    WorkerRunChangeKind, WorkerRunExecution, WorkerRunMetadata, WorkerRunRecord, WorkerRunRequest,
    WorkerRunResultManifest, WorkerRunResultStatus, WorkerRunResultTemplate, WorkerRunState,
    WorkerRunSubmissionDisposition, WorkerRunSubmitParams,
};
use crate::config::WorkerPlacementConfig;
use crate::worker_adapters::{canonical_json_bytes, prepare_worker_launch, WorkerLaunchSpec};
use crate::worker_placements::resolve_worker_placement;

const STORE_SCHEMA: &str = "herdr-worker-run-store/v1";
const RECORD_SCHEMA: &str = "herdr-worker-run/v1";
const REQUEST_SCHEMA: &str = "skills-herdr-worker-request/v1";
const CONTEXT_SCHEMA: &str = "skills-herdr-worker-context/v1";
const RESULT_SCHEMA: &str = "skills-herdr-worker-result/v1";
/// Directory that holds one artifact tree per worker attempt. Patch and diff
/// artifacts are resolved and hash-verified against real bytes below this root,
/// and a harness worker publishes its result manifest here.
const ARTIFACT_DIRECTORY: &str = "worker-run-artifacts";
use crate::worker_adapters::WORKER_RESULT_MANIFEST_FILE as HARNESS_RESULT_FILE;

/// The observed termination of a supervised harness process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHarnessTermination {
    pub success: bool,
    pub status: String,
}

#[derive(Debug)]
pub struct WorkerRunError {
    pub code: &'static str,
    pub message: String,
}

impl WorkerRunError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn io(action: &str, error: std::io::Error) -> Self {
        Self::new("worker_run_storage", format!("{action}: {error}"))
    }
}

#[derive(Debug, Clone)]
pub struct WorkerRunStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredWorkerRun {
    record: WorkerRunRecord,
    execution: WorkerRunExecution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<WorkerRunResultManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkerRunStoreState {
    schema: String,
    attempts: BTreeMap<String, String>,
    runs: BTreeMap<String, StoredWorkerRun>,
}

impl Default for WorkerRunStoreState {
    fn default() -> Self {
        Self {
            schema: STORE_SCHEMA.into(),
            attempts: BTreeMap::new(),
            runs: BTreeMap::new(),
        }
    }
}

impl WorkerRunStore {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn persistent() -> Self {
        Self::open(crate::session::data_dir().join("worker-runs.json"))
    }

    #[cfg(test)]
    pub fn submit(
        &self,
        params: WorkerRunSubmitParams,
    ) -> Result<(WorkerRunRecord, WorkerRunSubmissionDisposition), WorkerRunError> {
        self.submit_with_initializer(params, |_launch| {
            Err(WorkerRunError::new(
                "worker_placement_blocked",
                "harness execution requires an initialized local pane",
            ))
        })
    }

    #[allow(dead_code)]
    pub fn submit_with_initializer(
        &self,
        params: WorkerRunSubmitParams,
        initialize: impl FnOnce(&WorkerLaunchSpec) -> Result<String, WorkerRunError>,
    ) -> Result<(WorkerRunRecord, WorkerRunSubmissionDisposition), WorkerRunError> {
        #[cfg(test)]
        let approvals = test_approved_worker_placements();
        #[cfg(not(test))]
        let approvals = Vec::new();
        self.submit_with_initializer_and_placements(params, &approvals, initialize)
    }

    pub fn submit_with_initializer_and_placements(
        &self,
        params: WorkerRunSubmitParams,
        approved_placements: &[WorkerPlacementConfig],
        initialize: impl FnOnce(&WorkerLaunchSpec) -> Result<String, WorkerRunError>,
    ) -> Result<(WorkerRunRecord, WorkerRunSubmissionDisposition), WorkerRunError> {
        validate_submit(&params)?;
        let content_hash = hash_json(&(
            &params.request_hash,
            &params.context_hash,
            &params.execution,
        ))?;
        let reserved_run_id = run_id(&params.attempt_id, &content_hash);
        let artifact_root = self.artifact_root(&params.attempt_id);
        let prepared_launch = prepare_harness_launch(
            &params,
            &reserved_run_id,
            &artifact_root,
            approved_placements,
        )?;
        if prepared_launch.is_some() {
            std::fs::create_dir_all(&artifact_root)
                .map_err(|error| WorkerRunError::io("create worker artifact directory", error))?;
        }
        let (record, disposition, launch) = self.update(|state| {
            if let Some(run_id) = state.attempts.get(&params.attempt_id) {
                let existing = state.runs.get(run_id).ok_or_else(|| {
                    WorkerRunError::new(
                        "worker_run_store_corrupt",
                        format!(
                            "attempt {} references missing run {run_id}",
                            params.attempt_id
                        ),
                    )
                })?;
                if existing.record.content_hash != content_hash {
                    return Err(WorkerRunError::new(
                        "worker_run_attempt_conflict",
                        format!(
                            "attempt {} already belongs to immutable content {}",
                            params.attempt_id, existing.record.content_hash
                        ),
                    ));
                }
                return Ok((
                    existing.record.clone(),
                    WorkerRunSubmissionDisposition::DuplicateEquivalent,
                    None,
                ));
            }

            let now = unix_ms();
            let run_id = reserved_run_id.clone();
            let is_harness = prepared_launch.is_some();
            let mut stored = StoredWorkerRun {
                record: WorkerRunRecord {
                    schema: RECORD_SCHEMA.into(),
                    attempt_id: params.attempt_id.clone(),
                    run_id: run_id.clone(),
                    request_hash: params.request_hash,
                    context_hash: params.context_hash,
                    content_hash,
                    metadata: None,
                    state: if is_harness {
                        WorkerRunState::Submitted
                    } else {
                        WorkerRunState::Running
                    },
                    created_unix_ms: now,
                    updated_unix_ms: now,
                    deadline_unix_ms: params.request.lifecycle.deadline_unix_ms,
                    cancellation_requested: false,
                    terminal_reason: None,
                    result_ref: None,
                },
                execution: params.execution,
                result: None,
            };
            apply_deadline_or_deterministic_result(&mut stored, now, &artifact_root)?;
            state.attempts.insert(params.attempt_id, run_id.clone());
            let record = stored.record.clone();
            state.runs.insert(run_id, stored);
            Ok((
                record,
                WorkerRunSubmissionDisposition::Created,
                prepared_launch,
            ))
        })?;

        let Some(mut launch) = launch else {
            return Ok((record, disposition));
        };
        let placement = match initialize(&launch) {
            Ok(placement) => placement,
            Err(error) => {
                self.fail_reserved(&record.run_id, &error.message)?;
                return Err(error);
            }
        };
        validate_bounded_text("worker placement ref", &placement, 1_024)?;
        launch.metadata.placement = placement;
        let activated = self.activate_reserved(&record.run_id, launch.metadata)?;
        Ok((activated, disposition))
    }

    /// Artifact tree owned by one worker attempt. Patch and diff bytes and a
    /// harness result manifest are read from here and never from caller input.
    pub fn artifact_root(&self, attempt_id: &str) -> PathBuf {
        self.path
            .with_file_name(ARTIFACT_DIRECTORY)
            .join(attempt_id)
    }

    /// Drive a supervised harness run to exactly one explicit terminal state
    /// from its observed process termination. A successful exit must publish a
    /// validated result manifest; anything else fails loud.
    pub fn complete_harness(
        &self,
        run_id: &str,
        termination: &WorkerHarnessTermination,
    ) -> Result<WorkerRunRecord, WorkerRunError> {
        validate_run_id(run_id)?;
        validate_bounded_text("harness termination status", &termination.status, 1_024)?;
        self.update(|state| {
            let stored = state.runs.get_mut(run_id).ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_not_found",
                    format!("worker run {run_id} not found"),
                )
            })?;
            if stored.record.state.is_terminal() {
                return Ok(stored.record.clone());
            }
            if !matches!(stored.execution, WorkerRunExecution::Harness { .. }) {
                return Err(WorkerRunError::new(
                    "worker_run_invalid",
                    format!("worker run {run_id} is not a supervised harness run"),
                ));
            }
            let artifact_root = self.artifact_root(&stored.record.attempt_id);
            match harness_result_template(termination, &artifact_root) {
                Ok(template) => {
                    let now = unix_ms();
                    stored.result = Some(WorkerRunResultManifest {
                        schema: RESULT_SCHEMA.into(),
                        attempt_id: stored.record.attempt_id.clone(),
                        run_ref: stored.record.run_id.clone(),
                        status: WorkerRunResultStatus::Succeeded,
                        summary: template.summary,
                        change: template.change,
                        artifacts: template.artifacts,
                        provenance: template.provenance,
                        completed_at: rfc3339_from_unix_ms(now),
                    });
                    stored.record.state = WorkerRunState::Succeeded;
                    stored.record.updated_unix_ms = now;
                    stored.record.result_ref = Some(result_ref(&stored.record.run_id));
                }
                Err(reason) => finish_terminal(
                    stored,
                    WorkerRunState::Failed,
                    WorkerRunResultStatus::Failed,
                    &reason,
                )?,
            }
            Ok(stored.record.clone())
        })
    }

    fn activate_reserved(
        &self,
        run_id: &str,
        metadata: WorkerRunMetadata,
    ) -> Result<WorkerRunRecord, WorkerRunError> {
        self.update(|state| {
            let stored = state.runs.get_mut(run_id).ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_store_corrupt",
                    format!("reserved worker run {run_id} disappeared before activation"),
                )
            })?;
            if stored.record.state != WorkerRunState::Submitted
                || !matches!(stored.execution, WorkerRunExecution::Harness { .. })
                || stored.record.metadata.is_some()
            {
                return Err(WorkerRunError::new(
                    "worker_run_store_corrupt",
                    format!("worker run {run_id} is not an inactive harness reservation"),
                ));
            }
            stored.record.metadata = Some(metadata);
            stored.record.state = WorkerRunState::Running;
            stored.record.updated_unix_ms = unix_ms();
            Ok(stored.record.clone())
        })
    }

    fn fail_reserved(&self, run_id: &str, reason: &str) -> Result<(), WorkerRunError> {
        self.update(|state| {
            let stored = state.runs.get_mut(run_id).ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_store_corrupt",
                    format!("reserved worker run {run_id} disappeared before failure recording"),
                )
            })?;
            if stored.record.state != WorkerRunState::Submitted {
                return Err(WorkerRunError::new(
                    "worker_run_store_corrupt",
                    format!("worker run {run_id} is not an inactive harness reservation"),
                ));
            }
            finish_terminal(
                stored,
                WorkerRunState::Failed,
                WorkerRunResultStatus::Failed,
                reason,
            )
        })
    }

    pub fn get(&self, run_id: &str) -> Result<WorkerRunRecord, WorkerRunError> {
        validate_run_id(run_id)?;
        self.update(|state| {
            let stored = state.runs.get_mut(run_id).ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_not_found",
                    format!("worker run {run_id} not found"),
                )
            })?;
            apply_deadline(stored, unix_ms())?;
            Ok(stored.record.clone())
        })
    }

    pub fn cancel(
        &self,
        run_id: &str,
        reason: Option<&str>,
    ) -> Result<(WorkerRunRecord, WorkerRunCancellationDisposition), WorkerRunError> {
        validate_run_id(run_id)?;
        self.update(|state| {
            let stored = state.runs.get_mut(run_id).ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_not_found",
                    format!("worker run {run_id} not found"),
                )
            })?;
            apply_deadline(stored, unix_ms())?;
            if stored.record.state.is_terminal() {
                let disposition = if stored.record.cancellation_requested {
                    WorkerRunCancellationDisposition::AlreadyRequested
                } else {
                    WorkerRunCancellationDisposition::AlreadyTerminal
                };
                return Ok((stored.record.clone(), disposition));
            }
            stored.record.cancellation_requested = true;
            finish_terminal(
                stored,
                WorkerRunState::Cancelled,
                WorkerRunResultStatus::Cancelled,
                reason.unwrap_or("explicit cancellation requested"),
            )?;
            Ok((
                stored.record.clone(),
                WorkerRunCancellationDisposition::Requested,
            ))
        })
    }

    pub fn timeout(&self, run_id: &str, reason: &str) -> Result<WorkerRunRecord, WorkerRunError> {
        validate_run_id(run_id)?;
        if reason.trim().is_empty() {
            return Err(WorkerRunError::new(
                "worker_run_invalid",
                "timeout reason must not be empty",
            ));
        }
        self.update(|state| {
            let stored = state.runs.get_mut(run_id).ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_not_found",
                    format!("worker run {run_id} not found"),
                )
            })?;
            if stored.record.state == WorkerRunState::TimedOut {
                return Ok(stored.record.clone());
            }
            if stored.record.state.is_terminal() {
                return Err(WorkerRunError::new(
                    "worker_run_already_terminal",
                    format!("worker run {run_id} is already terminal"),
                ));
            }
            finish_terminal(
                stored,
                WorkerRunState::TimedOut,
                WorkerRunResultStatus::TimedOut,
                reason,
            )?;
            Ok(stored.record.clone())
        })
    }

    pub fn result(&self, result_ref: &str) -> Result<WorkerRunResultManifest, WorkerRunError> {
        let run_id = run_id_from_result_ref(result_ref)?;
        self.update(|state| {
            let stored = state.runs.get_mut(run_id).ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_not_found",
                    format!("worker run {run_id} not found"),
                )
            })?;
            apply_deadline(stored, unix_ms())?;
            if stored.record.result_ref.as_deref() != Some(result_ref) {
                return Err(WorkerRunError::new(
                    "worker_run_store_corrupt",
                    format!("worker run {run_id} does not own result reference {result_ref}"),
                ));
            }
            stored.result.clone().ok_or_else(|| {
                WorkerRunError::new(
                    "worker_run_result_unavailable",
                    format!("worker run {run_id} has no terminal result"),
                )
            })
        })
    }

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut WorkerRunStoreState) -> Result<T, WorkerRunError>,
    ) -> Result<T, WorkerRunError> {
        let lock_path = self.path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| WorkerRunError::io("create worker-run directory", error))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| WorkerRunError::io("open worker-run lock", error))?;
        lock.lock()
            .map_err(|error| WorkerRunError::io("lock worker-run store", error))?;
        let mut state = self.load()?;
        let before = state.clone();
        let result = operation(&mut state)?;
        validate_state(&state)?;
        if state != before {
            self.save(&state)?;
        }
        Ok(result)
    }

    fn load(&self) -> Result<WorkerRunStoreState, WorkerRunError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(WorkerRunStoreState::default());
            }
            Err(error) => return Err(WorkerRunError::io("read worker-run store", error)),
        };
        let state = serde_json::from_slice::<WorkerRunStoreState>(&bytes).map_err(|error| {
            WorkerRunError::new(
                "worker_run_store_corrupt",
                format!("parse {}: {error}", self.path.display()),
            )
        })?;
        validate_state(&state)?;
        Ok(state)
    }

    fn save(&self, state: &WorkerRunStoreState) -> Result<(), WorkerRunError> {
        self.save_with(state, crate::platform::replace_file)
    }

    fn save_with(
        &self,
        state: &WorkerRunStoreState,
        replace: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<(), WorkerRunError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| WorkerRunError::io("create worker-run directory", error))?;
        }
        let bytes = serde_json::to_vec_pretty(state).map_err(|error| {
            WorkerRunError::new(
                "worker_run_storage",
                format!("serialize worker-run store: {error}"),
            )
        })?;
        let temporary = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| WorkerRunError::io("open worker-run temporary file", error))?;
        file.write_all(&bytes)
            .map_err(|error| WorkerRunError::io("write worker-run temporary file", error))?;
        file.sync_all()
            .map_err(|error| WorkerRunError::io("sync worker-run temporary file", error))?;
        drop(file);
        if let Err(error) = replace(&temporary, &self.path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(WorkerRunError::io("commit worker-run store", error));
        }
        #[cfg(unix)]
        if let Some(parent) = self.path.parent() {
            OpenOptions::new()
                .read(true)
                .open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| WorkerRunError::io("sync worker-run directory", error))?;
        }
        Ok(())
    }
}

fn harness_result_template(
    termination: &WorkerHarnessTermination,
    artifact_root: &Path,
) -> Result<WorkerRunResultTemplate, String> {
    if !termination.success {
        return Err(format!(
            "approved local worker pane terminated without success: {}",
            termination.status
        ));
    }
    let manifest_path = artifact_root.join(HARNESS_RESULT_FILE);
    let bytes = std::fs::read(&manifest_path).map_err(|error| {
        format!(
            "approved local worker exited successfully without a readable result manifest at {}: {error}",
            manifest_path.display()
        )
    })?;
    let template = serde_json::from_slice::<WorkerRunResultTemplate>(&bytes).map_err(|error| {
        format!(
            "approved local worker result manifest at {} is not a valid result template: {error}",
            manifest_path.display()
        )
    })?;
    validate_result_template(&template, artifact_root).map_err(|error| {
        format!(
            "approved local worker result manifest is invalid: {}",
            error.message
        )
    })?;
    Ok(template)
}

fn apply_deadline_or_deterministic_result(
    stored: &mut StoredWorkerRun,
    now: u64,
    artifact_root: &Path,
) -> Result<(), WorkerRunError> {
    if stored
        .record
        .deadline_unix_ms
        .is_some_and(|deadline| now >= deadline)
    {
        return finish_terminal(
            stored,
            WorkerRunState::TimedOut,
            WorkerRunResultStatus::TimedOut,
            "approved worker deadline elapsed before execution",
        );
    }
    let result = match &stored.execution {
        WorkerRunExecution::Deterministic { result } => result,
        WorkerRunExecution::Harness { .. } => return Ok(()),
    };
    if let Some(template) = result.clone() {
        validate_result_template(&template, artifact_root)?;
        let completed_at = rfc3339_from_unix_ms(now);
        stored.result = Some(WorkerRunResultManifest {
            schema: RESULT_SCHEMA.into(),
            attempt_id: stored.record.attempt_id.clone(),
            run_ref: stored.record.run_id.clone(),
            status: WorkerRunResultStatus::Succeeded,
            summary: template.summary,
            change: template.change,
            artifacts: template.artifacts,
            provenance: template.provenance,
            completed_at,
        });
        stored.record.state = WorkerRunState::Succeeded;
        stored.record.updated_unix_ms = now;
        stored.record.result_ref = Some(result_ref(&stored.record.run_id));
    }
    Ok(())
}

fn apply_deadline(stored: &mut StoredWorkerRun, now: u64) -> Result<(), WorkerRunError> {
    if !stored.record.state.is_terminal()
        && stored
            .record
            .deadline_unix_ms
            .is_some_and(|deadline| now >= deadline)
    {
        finish_terminal(
            stored,
            WorkerRunState::TimedOut,
            WorkerRunResultStatus::TimedOut,
            "approved worker deadline elapsed",
        )?;
    }
    Ok(())
}

fn finish_terminal(
    stored: &mut StoredWorkerRun,
    state: WorkerRunState,
    status: WorkerRunResultStatus,
    reason: &str,
) -> Result<(), WorkerRunError> {
    let reason = reason.trim();
    validate_bounded_text("terminal reason", reason, 65_536)?;
    let now = unix_ms();
    let run_id = stored.record.run_id.clone();
    stored.record.state = state;
    stored.record.updated_unix_ms = now;
    stored.record.terminal_reason = Some(reason.into());
    stored.record.result_ref = Some(result_ref(&run_id));
    stored.result = Some(WorkerRunResultManifest {
        schema: RESULT_SCHEMA.into(),
        attempt_id: stored.record.attempt_id.clone(),
        run_ref: run_id.clone(),
        status,
        summary: reason.into(),
        change: WorkerRunChange {
            kind: WorkerRunChangeKind::None,
            changed_files: Vec::new(),
        },
        artifacts: vec![WorkerRunArtifact {
            kind: WorkerRunArtifactKind::Log,
            reference: format!("worker-run://{run_id}/terminal"),
            hash: hash_bytes(reason.as_bytes()),
            media_type: "text/plain".into(),
        }],
        provenance: None,
        completed_at: rfc3339_from_unix_ms(now),
    });
    Ok(())
}

fn validate_submit(params: &WorkerRunSubmitParams) -> Result<(), WorkerRunError> {
    validate_identifier("attempt_id", &params.attempt_id)?;
    validate_hash("request_hash", &params.request_hash)?;
    validate_hash("context_hash", &params.context_hash)?;
    validate_request(&params.request)?;
    let request_hash = hash_json(&params.request)?;
    if request_hash != params.request_hash {
        return Err(WorkerRunError::new(
            "worker_run_request_hash_mismatch",
            format!(
                "request_hash {} does not match immutable request {request_hash}",
                params.request_hash
            ),
        ));
    }
    let context_hash = hash_json(&params.request.context)?;
    if context_hash != params.context_hash {
        return Err(WorkerRunError::new(
            "worker_run_context_hash_mismatch",
            format!(
                "context_hash {} does not match immutable context {context_hash}",
                params.context_hash
            ),
        ));
    }
    Ok(())
}

fn prepare_harness_launch(
    params: &WorkerRunSubmitParams,
    run_id: &str,
    artifact_dir: &Path,
    approved_placements: &[WorkerPlacementConfig],
) -> Result<Option<WorkerLaunchSpec>, WorkerRunError> {
    let WorkerRunExecution::Harness {
        harness,
        profile,
        model,
        target_tag,
        placement,
        candidates,
    } = &params.execution
    else {
        return Ok(None);
    };
    if candidates.len() > 64 {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "worker placement candidates must contain at most 64 entries",
        ));
    }
    let resolved = resolve_worker_placement(
        target_tag,
        *harness,
        *placement,
        candidates,
        approved_placements,
    )
    .map_err(crate::worker_adapters::WorkerAdapterError::from)
    .map_err(|error| WorkerRunError::new(error.code, error.message))?;
    let metadata = WorkerRunMetadata {
        harness: *harness,
        profile: profile.clone(),
        model: model.clone(),
        target: resolved.target_tag.clone(),
        placement: if resolved.kind.is_remote() {
            format!("remote-pane:{}", resolved.workspace_id)
        } else {
            format!("local-pane:{}", resolved.workspace_id)
        },
    };
    prepare_worker_launch(&params.request, &metadata, &resolved, run_id, artifact_dir)
        .map(Some)
        .map_err(|error| WorkerRunError::new(error.code, error.message))
}

fn validate_request(request: &WorkerRunRequest) -> Result<(), WorkerRunError> {
    if request.schema != REQUEST_SCHEMA {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            format!("unsupported worker request schema {}", request.schema),
        ));
    }
    validate_bounded_text("worker role", &request.role, 200)?;
    if request.capabilities.is_empty() || request.capabilities.len() > 32 {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "worker capabilities must contain between 1 and 32 entries",
        ));
    }
    let mut capabilities = std::collections::BTreeSet::new();
    for capability in &request.capabilities {
        validate_identifier("worker capability", capability)?;
        if !capabilities.insert(capability) {
            return Err(WorkerRunError::new(
                "worker_run_invalid",
                "worker capabilities must be unique",
            ));
        }
    }
    if request.context.schema != CONTEXT_SCHEMA {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            format!(
                "unsupported worker context schema {}",
                request.context.schema
            ),
        ));
    }
    validate_bounded_text("worker instruction", &request.context.instruction, 65_536)?;
    validate_bounded_text(
        "worker repository ref",
        &request.context.repository_ref,
        1_024,
    )?;
    validate_hash("worker revision", &request.context.revision)?;
    if request.context.inputs.len() > 128 {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "worker context inputs must contain at most 128 entries",
        ));
    }
    let mut input_refs = std::collections::BTreeSet::new();
    for input in &request.context.inputs {
        validate_bounded_text("worker context input ref", &input.reference, 1_024)?;
        validate_hash("worker context input hash", &input.hash)?;
        if !input_refs.insert(&input.reference) {
            return Err(WorkerRunError::new(
                "worker_run_invalid",
                "worker context input refs must be unique",
            ));
        }
    }
    if request.result_contract.schema != RESULT_SCHEMA
        || !request.result_contract.require_patch_for_code_changes
    {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "worker result contract must require hash-bound patch or diff artifacts for code changes",
        ));
    }
    Ok(())
}

fn validate_bounded_text(field: &str, value: &str, max_bytes: usize) -> Result<(), WorkerRunError> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            format!("{field} must be non-empty and at most {max_bytes} bytes"),
        ));
    }
    Ok(())
}

fn validate_run_id(run_id: &str) -> Result<(), WorkerRunError> {
    validate_identifier("run_id", run_id)
}

fn run_id_from_result_ref(result_ref: &str) -> Result<&str, WorkerRunError> {
    let run_id = result_ref
        .strip_prefix("worker-run://")
        .and_then(|value| value.strip_suffix("/result"))
        .filter(|value| !value.contains('/'))
        .ok_or_else(|| {
            WorkerRunError::new(
                "worker_run_invalid",
                "result_ref must be an opaque worker-run result reference",
            )
        })?;
    validate_run_id(run_id)?;
    Ok(run_id)
}

fn validate_identifier(field: &str, value: &str) -> Result<(), WorkerRunError> {
    let valid = value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(WorkerRunError::new(
            "worker_run_invalid",
            format!("{field} must be a bounded portable identifier"),
        ))
    }
}

fn validate_hash(field: &str, value: &str) -> Result<(), WorkerRunError> {
    let digest = value.strip_prefix("sha256:").unwrap_or_default();
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(WorkerRunError::new(
            "worker_run_invalid",
            format!("{field} must be a sha256 hash"),
        ))
    }
}

/// Bind a recorded patch or diff hash to the real artifact bytes published
/// under the attempt's artifact root. A missing artifact or a hash that does
/// not cover the produced bytes is a typed fail-closed error.
fn verify_patch_artifact_bytes(
    artifact: &WorkerRunArtifact,
    artifact_root: &Path,
) -> Result<(), WorkerRunError> {
    let relative = Path::new(&artifact.reference);
    let contained = relative.components().all(|component| {
        matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    });
    if !contained {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            format!(
                "patch artifact reference {:?} must be a path inside the worker artifact root",
                artifact.reference
            ),
        ));
    }
    let path = artifact_root.join(relative);
    let bytes = std::fs::read(&path).map_err(|error| {
        WorkerRunError::new(
            "worker_run_artifact_unavailable",
            format!(
                "patch artifact {:?} has no readable bytes at {}: {error}",
                artifact.reference,
                path.display()
            ),
        )
    })?;
    let observed = hash_bytes(&bytes);
    if observed != artifact.hash {
        return Err(WorkerRunError::new(
            "worker_run_artifact_hash_mismatch",
            format!(
                "patch artifact {:?} records {} but its bytes hash to {observed}",
                artifact.reference, artifact.hash
            ),
        ));
    }
    Ok(())
}

fn validate_result_template(
    template: &WorkerRunResultTemplate,
    artifact_root: &Path,
) -> Result<(), WorkerRunError> {
    validate_bounded_text("result summary", &template.summary, 65_536)?;
    if template.artifacts.is_empty() || template.artifacts.len() > 128 {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "result artifacts must contain between 1 and 128 entries",
        ));
    }
    if template.change.changed_files.len() > 128 {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "changed files must contain at most 128 entries",
        ));
    }
    for artifact in &template.artifacts {
        validate_bounded_text("result artifact reference", &artifact.reference, 4_096)?;
        validate_bounded_text("result artifact media type", &artifact.media_type, 200)?;
        validate_hash("artifact hash", &artifact.hash)?;
        if matches!(
            artifact.kind,
            WorkerRunArtifactKind::Patch | WorkerRunArtifactKind::Diff
        ) {
            verify_patch_artifact_bytes(artifact, artifact_root)?;
        }
    }
    for path in &template.change.changed_files {
        if path.is_empty()
            || path.len() > 4_096
            || Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return Err(WorkerRunError::new(
                "worker_run_invalid",
                format!("changed file {path:?} is not a repository-relative path"),
            ));
        }
    }
    if template
        .change
        .changed_files
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != template.change.changed_files.len()
    {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "changed files must be unique",
        ));
    }
    if template
        .artifacts
        .iter()
        .enumerate()
        .any(|(index, artifact)| template.artifacts[..index].contains(artifact))
    {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "result artifacts must be unique",
        ));
    }
    if let Some(provenance) = &template.provenance {
        validate_bounded_text("provenance harness", &provenance.harness, 200)?;
        validate_bounded_text("provenance model", &provenance.model, 200)?;
        validate_bounded_text("provenance placement ref", &provenance.placement_ref, 1_024)?;
        if let Some(branch) = provenance.branch.as_deref() {
            validate_bounded_text("provenance branch", branch, 1_024)?;
        }
        if let Some(commit) = provenance.commit.as_deref() {
            let valid = matches!(commit.len(), 40 | 64)
                && commit
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            if !valid {
                return Err(WorkerRunError::new(
                    "worker_run_invalid",
                    "provenance commit must be a lowercase 40- or 64-character Git object id",
                ));
            }
        }
    }
    match template.change.kind {
        WorkerRunChangeKind::None if !template.change.changed_files.is_empty() => {
            return Err(WorkerRunError::new(
                "worker_run_invalid",
                "no-change result cannot list changed files",
            ));
        }
        WorkerRunChangeKind::Code => {
            if template.change.changed_files.is_empty() {
                return Err(WorkerRunError::new(
                    "worker_run_invalid",
                    "code-change result must list changed files",
                ));
            }
            if !template.artifacts.iter().any(|artifact| {
                matches!(
                    artifact.kind,
                    WorkerRunArtifactKind::Patch | WorkerRunArtifactKind::Diff
                )
            }) {
                return Err(WorkerRunError::new(
                    "worker_run_invalid",
                    "code-change result must include a patch or diff artifact",
                ));
            }
        }
        WorkerRunChangeKind::None => {}
    }
    Ok(())
}

fn validate_state(state: &WorkerRunStoreState) -> Result<(), WorkerRunError> {
    if state.schema != STORE_SCHEMA {
        return Err(WorkerRunError::new(
            "worker_run_store_corrupt",
            format!("unsupported worker-run store schema {}", state.schema),
        ));
    }
    for (attempt_id, run_id) in &state.attempts {
        let stored = state.runs.get(run_id).ok_or_else(|| {
            WorkerRunError::new(
                "worker_run_store_corrupt",
                format!("attempt {attempt_id} references missing run {run_id}"),
            )
        })?;
        if stored.record.attempt_id != *attempt_id || stored.record.run_id != *run_id {
            return Err(WorkerRunError::new(
                "worker_run_store_corrupt",
                format!("worker run {run_id} identity does not match its index"),
            ));
        }
        if stored.record.state.is_terminal() != stored.result.is_some() {
            return Err(WorkerRunError::new(
                "worker_run_store_corrupt",
                format!("worker run {run_id} terminal result binding is inconsistent"),
            ));
        }
        if stored.record.state.is_terminal() {
            let expected_ref = result_ref(run_id);
            let result = stored.result.as_ref().expect("terminal result checked");
            if stored.record.result_ref.as_deref() != Some(expected_ref.as_str())
                || result.run_ref != *run_id
                || result.attempt_id != *attempt_id
            {
                return Err(WorkerRunError::new(
                    "worker_run_store_corrupt",
                    format!("worker run {run_id} terminal result identity is inconsistent"),
                ));
            }
        } else if stored.record.result_ref.is_some() {
            return Err(WorkerRunError::new(
                "worker_run_store_corrupt",
                format!("worker run {run_id} exposes a result before reaching terminal state"),
            ));
        }
    }
    if state.runs.len() != state.attempts.len() {
        return Err(WorkerRunError::new(
            "worker_run_store_corrupt",
            "worker-run store contains an unindexed run",
        ));
    }
    Ok(())
}

fn run_id(attempt_id: &str, content_hash: &str) -> String {
    let digest = hash_bytes(format!("{attempt_id}\0{content_hash}").as_bytes());
    let hex = digest.strip_prefix("sha256:").unwrap_or(&digest);
    format!("worker-run:{}", &hex[..32])
}

fn result_ref(run_id: &str) -> String {
    format!("worker-run://{run_id}/result")
}

fn hash_json(value: &impl Serialize) -> Result<String, WorkerRunError> {
    let bytes = canonical_json_bytes(value)
        .map_err(|error| WorkerRunError::new(error.code, error.message))?;
    Ok(hash_bytes(&bytes))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn rfc3339_from_unix_ms(unix_ms: u64) -> String {
    let seconds: i64 = (unix_ms / 1000).try_into().unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
fn test_approved_worker_placements() -> Vec<WorkerPlacementConfig> {
    vec![WorkerPlacementConfig {
        target_tag: "herdr-target:local-macos-primary".into(),
        kind: crate::config::WorkerPlacementKindConfig::LocalPane,
        machine: None,
        cwd: Some(env!("CARGO_MANIFEST_DIR").into()),
        harnesses: vec!["codex".into(), "claude".into()],
        approval: crate::config::WorkerPlacementApprovalConfig {
            reference: "test:local-placement".into(),
            approved_at: "2026-08-01T00:00:00Z".into(),
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_PATCH: &[u8] =
        b"diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";

    fn hash(character: char) -> String {
        format!("sha256:{}", character.to_string().repeat(64))
    }

    fn temp_store(name: &str) -> WorkerRunStore {
        let unique = format!(
            "herdr-worker-runs-{name}-{}-{}",
            std::process::id(),
            unix_ms()
        );
        WorkerRunStore::open(std::env::temp_dir().join(unique).join("runs.json"))
    }

    fn provider_neutral_request(instruction_marker: &str) -> WorkerRunRequest {
        WorkerRunRequest {
            schema: REQUEST_SCHEMA.into(),
            role: "implementation-worker".into(),
            capabilities: vec!["read-repository".into(), "edit-repository".into()],
            context: crate::api::schema::WorkerRunContext {
                schema: CONTEXT_SCHEMA.into(),
                instruction: format!("apply bounded fixture change {instruction_marker}"),
                repository_ref: "github.com/example/project".into(),
                revision: hash('e'),
                inputs: vec![crate::api::schema::WorkerRunContextInput {
                    reference: "skills-attempt://TASK-10.2/input".into(),
                    hash: hash('f'),
                }],
            },
            lifecycle: crate::api::schema::WorkerRunLifecycle {
                deadline_unix_ms: None,
            },
            result_contract: crate::api::schema::WorkerRunResultContract {
                schema: RESULT_SCHEMA.into(),
                require_patch_for_code_changes: true,
            },
        }
    }

    fn deferred(attempt_id: &str, instruction_marker: &str) -> WorkerRunSubmitParams {
        let request = provider_neutral_request(instruction_marker);
        WorkerRunSubmitParams {
            attempt_id: attempt_id.into(),
            request_hash: hash_json(&request).unwrap(),
            context_hash: hash_json(&request.context).unwrap(),
            request,
            execution: WorkerRunExecution::Deterministic { result: None },
        }
    }

    /// Publish the real patch bytes the recorded artifact hash must cover.
    fn seed_patch(store: &WorkerRunStore, attempt_id: &str, bytes: &[u8]) {
        let root = store.artifact_root(attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("change.patch"), bytes).unwrap();
    }

    fn successful(store: &WorkerRunStore, attempt_id: &str) -> WorkerRunSubmitParams {
        seed_patch(store, attempt_id, FIXTURE_PATCH);
        let request = provider_neutral_request("success");
        WorkerRunSubmitParams {
            attempt_id: attempt_id.into(),
            request_hash: hash_json(&request).unwrap(),
            context_hash: hash_json(&request.context).unwrap(),
            request,
            execution: WorkerRunExecution::Deterministic {
                result: Some(WorkerRunResultTemplate {
                    summary: "deterministic worker completed".into(),
                    change: WorkerRunChange {
                        kind: WorkerRunChangeKind::Code,
                        changed_files: vec!["src/lib.rs".into()],
                    },
                    artifacts: vec![WorkerRunArtifact {
                        kind: WorkerRunArtifactKind::Patch,
                        reference: "change.patch".into(),
                        hash: hash_bytes(FIXTURE_PATCH),
                        media_type: "text/x-diff".into(),
                    }],
                    provenance: None,
                }),
            },
        }
    }

    #[test]
    fn duplicate_submission_returns_original_run_and_conflict_fails_closed() {
        let store = temp_store("idempotency");
        let request = deferred("attempt-1", &hash('a'));
        let (created, disposition) = store.submit(request.clone()).unwrap();
        assert_eq!(disposition, WorkerRunSubmissionDisposition::Created);

        let (duplicate, disposition) = store.submit(request).unwrap();
        assert_eq!(
            disposition,
            WorkerRunSubmissionDisposition::DuplicateEquivalent
        );
        assert_eq!(duplicate.run_id, created.run_id);

        let error = store.submit(deferred("attempt-1", &hash('d'))).unwrap_err();
        assert_eq!(error.code, "worker_run_attempt_conflict");
        assert_eq!(store.get(&created.run_id).unwrap(), created);
    }

    #[test]
    fn persistent_reload_reconciles_same_run_and_retry_needs_new_attempt() {
        let store = temp_store("reload");
        let path = store.path.clone();
        let (first, _) = store.submit(deferred("attempt-1", &hash('a'))).unwrap();
        let reopened = WorkerRunStore::open(path);
        assert_eq!(reopened.get(&first.run_id).unwrap(), first);

        let (retry, _) = reopened.submit(deferred("attempt-2", &hash('a'))).unwrap();
        assert_ne!(retry.run_id, first.run_id);
        assert_eq!(retry.attempt_id, "attempt-2");
    }

    #[test]
    fn failed_atomic_replace_preserves_previous_ledger_for_reopen() {
        let store = temp_store("replace-failure");
        let path = store.path.clone();
        let (original, _) = store
            .submit(deferred("attempt-replace", &hash('a')))
            .unwrap();
        let mut updated = store.load().unwrap();
        let stored = updated.runs.get_mut(&original.run_id).unwrap();
        finish_terminal(
            stored,
            WorkerRunState::Failed,
            WorkerRunResultStatus::Failed,
            "simulated worker failure",
        )
        .unwrap();
        validate_state(&updated).unwrap();

        let error = store
            .save_with(&updated, |_source, _target| {
                Err(std::io::Error::other("simulated atomic replace failure"))
            })
            .unwrap_err();
        assert_eq!(error.code, "worker_run_storage");

        let reopened = WorkerRunStore::open(path);
        assert_eq!(reopened.get(&original.run_id).unwrap(), original);
    }

    #[test]
    fn cancellation_is_explicit_idempotent_and_persists_terminal_result() {
        let store = temp_store("cancel");
        let (run, _) = store
            .submit(deferred("attempt-cancel", &hash('a')))
            .unwrap();
        let (cancelled, disposition) = store
            .cancel(&run.run_id, Some("operator cancelled"))
            .unwrap();
        assert_eq!(disposition, WorkerRunCancellationDisposition::Requested);
        assert_eq!(cancelled.state, WorkerRunState::Cancelled);
        assert!(cancelled.cancellation_requested);

        let (again, disposition) = store.cancel(&run.run_id, None).unwrap();
        assert_eq!(
            disposition,
            WorkerRunCancellationDisposition::AlreadyRequested
        );
        assert_eq!(again, cancelled);
        let result = store
            .result(cancelled.result_ref.as_deref().unwrap())
            .unwrap();
        assert_eq!(result.status, WorkerRunResultStatus::Cancelled);
        assert_eq!(result.run_ref, run.run_id);
    }

    #[test]
    fn elapsed_deadline_becomes_one_durable_timeout_outcome() {
        let store = temp_store("timeout");
        let mut request = deferred("attempt-timeout", &hash('a'));
        request.request.lifecycle.deadline_unix_ms = Some(1);
        request.request_hash = hash_json(&request.request).unwrap();
        let (run, _) = store.submit(request).unwrap();
        assert_eq!(run.state, WorkerRunState::TimedOut);
        assert_eq!(store.get(&run.run_id).unwrap(), run);
        assert_eq!(
            store
                .result(run.result_ref.as_deref().unwrap())
                .unwrap()
                .status,
            WorkerRunResultStatus::TimedOut
        );
    }

    #[test]
    fn result_reference_rejects_unknown_and_mismatched_bindings() {
        let store = temp_store("result-reference");
        let (run, _) = store
            .submit(successful(&store, "attempt-result-ref"))
            .unwrap();
        let published_ref = run.result_ref.clone().unwrap();
        assert_eq!(store.result(&published_ref).unwrap().run_ref, run.run_id);

        let unknown = store
            .result("worker-run://worker-run:00000000000000000000000000000000/result")
            .unwrap_err();
        assert_eq!(unknown.code, "worker_run_not_found");

        let mut corrupted = store.load().unwrap();
        corrupted
            .runs
            .get_mut(&run.run_id)
            .unwrap()
            .record
            .result_ref =
            Some("worker-run://worker-run:ffffffffffffffffffffffffffffffff/result".into());
        std::fs::write(&store.path, serde_json::to_vec_pretty(&corrupted).unwrap()).unwrap();
        let mismatch = store.result(&published_ref).unwrap_err();
        assert_eq!(mismatch.code, "worker_run_store_corrupt");
    }

    #[test]
    fn explicit_timeout_is_one_durable_terminal_outcome() {
        let store = temp_store("explicit-timeout");
        let (run, _) = store
            .submit(deferred("attempt-explicit-timeout", &hash('a')))
            .unwrap();
        let timed_out = store
            .timeout(&run.run_id, "approved deadline elapsed")
            .unwrap();
        assert_eq!(timed_out.state, WorkerRunState::TimedOut);
        assert_eq!(store.get(&run.run_id).unwrap(), timed_out);
        assert_eq!(
            store
                .timeout(&run.run_id, "approved deadline elapsed")
                .unwrap(),
            timed_out
        );
        assert_eq!(
            store
                .result(timed_out.result_ref.as_deref().unwrap())
                .unwrap()
                .status,
            WorkerRunResultStatus::TimedOut
        );
    }

    #[test]
    fn deterministic_worker_returns_versioned_hash_bound_patch_manifest() {
        let store = temp_store("result");
        let (run, _) = store.submit(successful(&store, "attempt-success")).unwrap();
        assert_eq!(run.state, WorkerRunState::Succeeded);
        let result = store.result(run.result_ref.as_deref().unwrap()).unwrap();
        assert_eq!(result.schema, RESULT_SCHEMA);
        assert_eq!(result.attempt_id, "attempt-success");
        assert_eq!(result.run_ref, run.run_id);
        assert_eq!(result.status, WorkerRunResultStatus::Succeeded);
        assert_eq!(result.change.kind, WorkerRunChangeKind::Code);
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.kind == WorkerRunArtifactKind::Patch));
        assert_eq!(result.artifacts[0].hash, hash_bytes(FIXTURE_PATCH));
        assert!(result.completed_at.ends_with('Z'));
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["schema"], RESULT_SCHEMA);
        assert_eq!(json["attemptId"], "attempt-success");
        assert_eq!(json["runRef"], run.run_id);
        assert_eq!(json["status"], "succeeded");
        assert_eq!(json["change"]["kind"], "code");
        assert_eq!(json["change"]["changedFiles"][0], "src/lib.rs");
        assert_eq!(json["artifacts"][0]["kind"], "patch");
        assert_eq!(json["artifacts"][0]["mediaType"], "text/x-diff");
    }

    #[test]
    fn code_change_result_binds_the_recorded_hash_to_real_patch_bytes() {
        // The recorded hash must cover the produced bytes, not merely round-trip.
        let bound = temp_store("patch-bound");
        let (run, _) = bound.submit(successful(&bound, "attempt-bound")).unwrap();
        let result = bound.result(run.result_ref.as_deref().unwrap()).unwrap();
        assert_eq!(result.artifacts[0].hash, hash_bytes(FIXTURE_PATCH));
        assert_eq!(
            std::fs::read(bound.artifact_root("attempt-bound").join("change.patch")).unwrap(),
            FIXTURE_PATCH
        );

        // Bytes that changed after the hash was recorded fail closed.
        let tampered = temp_store("patch-tampered");
        let params = successful(&tampered, "attempt-tampered");
        seed_patch(
            &tampered,
            "attempt-tampered",
            b"bytes a worker never produced",
        );
        let error = tampered.submit(params).unwrap_err();
        assert_eq!(error.code, "worker_run_artifact_hash_mismatch");

        // A recorded patch with no bytes at all fails closed.
        let absent = temp_store("patch-absent");
        let params = successful(&absent, "attempt-absent");
        std::fs::remove_file(absent.artifact_root("attempt-absent").join("change.patch")).unwrap();
        let error = absent.submit(params).unwrap_err();
        assert_eq!(error.code, "worker_run_artifact_unavailable");
    }

    #[test]
    fn immutable_request_and_context_hash_mismatches_fail_closed() {
        let store = temp_store("hash-mismatch");
        let mut request = deferred("attempt-request-mismatch", "request");
        request.request_hash = hash('1');
        let error = store.submit(request).unwrap_err();
        assert_eq!(error.code, "worker_run_request_hash_mismatch");

        let mut context = deferred("attempt-context-mismatch", "context");
        context.context_hash = hash('2');
        let error = store.submit(context).unwrap_err();
        assert_eq!(error.code, "worker_run_context_hash_mismatch");
    }

    #[cfg(target_os = "macos")]
    fn harness_params(attempt_id: &str) -> WorkerRunSubmitParams {
        let request = provider_neutral_request("supervision");
        WorkerRunSubmitParams {
            attempt_id: attempt_id.into(),
            request_hash: hash_json(&request).unwrap(),
            context_hash: hash_json(&request.context).unwrap(),
            request,
            execution: WorkerRunExecution::Harness {
                harness: crate::api::schema::WorkerHarness::Codex,
                profile: "profile-under-test".into(),
                model: "opaque-model-under-test".into(),
                target_tag: crate::worker_placements::APPROVED_LOCAL_PLACEMENT
                    .target_tag
                    .into(),
                placement: crate::worker_placements::WorkerPlacementKind::LocalPane,
                candidates: vec![crate::worker_placements::WorkerPlacementCandidate {
                    target_tag: crate::worker_placements::APPROVED_LOCAL_PLACEMENT
                        .target_tag
                        .into(),
                    kind: crate::worker_placements::WorkerPlacementKind::LocalPane,
                    workspace_id: Some("workspace:1".into()),
                    cwd: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
                    availability: crate::worker_placements::WorkerPlacementAvailability::Available,
                }],
            },
        }
    }

    #[cfg(target_os = "macos")]
    fn running_harness_run(store: &WorkerRunStore, attempt_id: &str) -> WorkerRunRecord {
        let (run, _) = store
            .submit_with_initializer(harness_params(attempt_id), |_launch| {
                Ok("local-pane:pane:1".into())
            })
            .unwrap();
        assert_eq!(run.state, WorkerRunState::Running);
        run
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn supervised_harness_exit_drives_one_explicit_terminal_state() {
        let store = temp_store("harness-supervision-exit");
        let run = running_harness_run(&store, "attempt-nonzero-exit");
        let terminal = store
            .complete_harness(
                &run.run_id,
                &WorkerHarnessTermination {
                    success: false,
                    status: "ExitStatus { code: 1, signal: None }".into(),
                },
            )
            .unwrap();
        assert_eq!(terminal.state, WorkerRunState::Failed);
        assert!(terminal
            .terminal_reason
            .as_deref()
            .unwrap()
            .contains("code: 1"));
        assert_eq!(
            store
                .result(terminal.result_ref.as_deref().unwrap())
                .unwrap()
                .status,
            WorkerRunResultStatus::Failed
        );
        // Supervision is idempotent and never reopens a terminal run.
        assert_eq!(
            store
                .complete_harness(
                    &run.run_id,
                    &WorkerHarnessTermination {
                        success: true,
                        status: "ExitStatus { code: 0, signal: None }".into(),
                    },
                )
                .unwrap(),
            terminal
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn supervised_harness_success_publishes_a_validated_result_manifest() {
        let store = temp_store("harness-supervision-manifest");
        let run = running_harness_run(&store, "attempt-harness-result");
        let root = store.artifact_root("attempt-harness-result");
        std::fs::write(root.join("change.patch"), FIXTURE_PATCH).unwrap();
        let template = WorkerRunResultTemplate {
            summary: "approved local worker completed".into(),
            change: WorkerRunChange {
                kind: WorkerRunChangeKind::Code,
                changed_files: vec!["src/lib.rs".into()],
            },
            artifacts: vec![WorkerRunArtifact {
                kind: WorkerRunArtifactKind::Patch,
                reference: "change.patch".into(),
                hash: hash_bytes(FIXTURE_PATCH),
                media_type: "text/x-diff".into(),
            }],
            provenance: None,
        };
        std::fs::write(
            root.join(HARNESS_RESULT_FILE),
            serde_json::to_vec(&template).unwrap(),
        )
        .unwrap();

        let terminal = store
            .complete_harness(
                &run.run_id,
                &WorkerHarnessTermination {
                    success: true,
                    status: "ExitStatus { code: 0, signal: None }".into(),
                },
            )
            .unwrap();
        assert_eq!(terminal.state, WorkerRunState::Succeeded);
        let result = store
            .result(terminal.result_ref.as_deref().unwrap())
            .unwrap();
        assert_eq!(result.schema, RESULT_SCHEMA);
        assert_eq!(result.status, WorkerRunResultStatus::Succeeded);
        assert_eq!(result.change.kind, WorkerRunChangeKind::Code);
        assert_eq!(result.artifacts[0].hash, hash_bytes(FIXTURE_PATCH));
    }

    /// A worker that ignores the publication location stated in its assignment
    /// is a failed run: Herdr reads the manifest only from the artifact root it
    /// owns, never from the worker's own choice of path.
    #[cfg(target_os = "macos")]
    #[test]
    fn manifest_published_outside_the_stated_location_fails_loud() {
        let store = temp_store("harness-supervision-elsewhere");
        let run = running_harness_run(&store, "attempt-harness-elsewhere");
        let root = store.artifact_root("attempt-harness-elsewhere");
        let elsewhere = root.parent().unwrap().join("worker-chosen-location");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("change.patch"), FIXTURE_PATCH).unwrap();
        let template = WorkerRunResultTemplate {
            summary: "approved local worker completed".into(),
            change: WorkerRunChange {
                kind: WorkerRunChangeKind::Code,
                changed_files: vec!["src/lib.rs".into()],
            },
            artifacts: vec![WorkerRunArtifact {
                kind: WorkerRunArtifactKind::Patch,
                reference: "change.patch".into(),
                hash: hash_bytes(FIXTURE_PATCH),
                media_type: "text/x-diff".into(),
            }],
            provenance: None,
        };
        std::fs::write(
            elsewhere.join(HARNESS_RESULT_FILE),
            serde_json::to_vec(&template).unwrap(),
        )
        .unwrap();

        let terminal = store
            .complete_harness(
                &run.run_id,
                &WorkerHarnessTermination {
                    success: true,
                    status: "ExitStatus { code: 0, signal: None }".into(),
                },
            )
            .unwrap();
        assert_eq!(terminal.state, WorkerRunState::Failed);
        assert!(terminal
            .terminal_reason
            .as_deref()
            .unwrap()
            .contains("without a readable result manifest"));
        assert!(
            store
                .result(terminal.result_ref.as_deref().unwrap())
                .unwrap()
                .status
                == WorkerRunResultStatus::Failed
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn supervised_harness_without_a_valid_manifest_fails_loud() {
        let missing = temp_store("harness-supervision-missing");
        let run = running_harness_run(&missing, "attempt-harness-missing");
        let terminal = missing
            .complete_harness(
                &run.run_id,
                &WorkerHarnessTermination {
                    success: true,
                    status: "ExitStatus { code: 0, signal: None }".into(),
                },
            )
            .unwrap();
        assert_eq!(terminal.state, WorkerRunState::Failed);
        assert!(terminal
            .terminal_reason
            .as_deref()
            .unwrap()
            .contains("without a readable result manifest"));

        let tampered = temp_store("harness-supervision-tampered");
        let run = running_harness_run(&tampered, "attempt-harness-tampered");
        let root = tampered.artifact_root("attempt-harness-tampered");
        std::fs::write(root.join("change.patch"), b"bytes a worker never produced").unwrap();
        let template = WorkerRunResultTemplate {
            summary: "approved local worker completed".into(),
            change: WorkerRunChange {
                kind: WorkerRunChangeKind::Code,
                changed_files: vec!["src/lib.rs".into()],
            },
            artifacts: vec![WorkerRunArtifact {
                kind: WorkerRunArtifactKind::Patch,
                reference: "change.patch".into(),
                hash: hash_bytes(FIXTURE_PATCH),
                media_type: "text/x-diff".into(),
            }],
            provenance: None,
        };
        std::fs::write(
            root.join(HARNESS_RESULT_FILE),
            serde_json::to_vec(&template).unwrap(),
        )
        .unwrap();
        let terminal = tampered
            .complete_harness(
                &run.run_id,
                &WorkerHarnessTermination {
                    success: true,
                    status: "ExitStatus { code: 0, signal: None }".into(),
                },
            )
            .unwrap();
        assert_eq!(terminal.state, WorkerRunState::Failed);
        assert!(terminal
            .terminal_reason
            .as_deref()
            .unwrap()
            .contains("bytes hash to"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn harness_reserves_durably_before_one_idempotent_initialization() {
        use std::cell::Cell;

        let store = temp_store("harness-metadata");
        let store_path = store.path.clone();
        let request = provider_neutral_request("adapter");
        let request_hash = hash_json(&request).unwrap();
        let context_hash = hash_json(&request.context).unwrap();
        let params = WorkerRunSubmitParams {
            attempt_id: "attempt-harness".into(),
            request_hash: request_hash.clone(),
            context_hash,
            request,
            execution: WorkerRunExecution::Harness {
                harness: crate::api::schema::WorkerHarness::Codex,
                profile: "profile-under-test".into(),
                model: "opaque-model-under-test".into(),
                target_tag: crate::worker_placements::APPROVED_LOCAL_PLACEMENT
                    .target_tag
                    .into(),
                placement: crate::worker_placements::WorkerPlacementKind::LocalPane,
                candidates: vec![crate::worker_placements::WorkerPlacementCandidate {
                    target_tag: crate::worker_placements::APPROVED_LOCAL_PLACEMENT
                        .target_tag
                        .into(),
                    kind: crate::worker_placements::WorkerPlacementKind::LocalPane,
                    workspace_id: Some("workspace:1".into()),
                    cwd: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
                    availability: crate::worker_placements::WorkerPlacementAvailability::Available,
                }],
            },
        };
        let duplicate_params = params.clone();
        let initialize_count = Cell::new(0);
        let (run, disposition) = store
            .submit_with_initializer(params, |_launch| {
                initialize_count.set(initialize_count.get() + 1);
                let reserved = WorkerRunStore::open(store_path).load().unwrap();
                let reserved_run = reserved.runs.values().next().unwrap();
                assert_eq!(reserved_run.record.state, WorkerRunState::Submitted);
                assert!(reserved_run.record.metadata.is_none());
                Ok("local-pane:pane:9".into())
            })
            .unwrap();
        assert_eq!(disposition, WorkerRunSubmissionDisposition::Created);
        assert_eq!(initialize_count.get(), 1);
        assert_eq!(run.request_hash, request_hash);
        assert_eq!(
            run.metadata,
            Some(WorkerRunMetadata {
                harness: crate::api::schema::WorkerHarness::Codex,
                profile: "profile-under-test".into(),
                model: "opaque-model-under-test".into(),
                target: crate::worker_placements::APPROVED_LOCAL_PLACEMENT
                    .target_tag
                    .into(),
                placement: "local-pane:pane:9".into(),
            })
        );
        let (duplicate, disposition) = store
            .submit_with_initializer(duplicate_params, |_launch| {
                initialize_count.set(initialize_count.get() + 1);
                Ok("local-pane:pane:10".into())
            })
            .unwrap();
        assert_eq!(
            disposition,
            WorkerRunSubmissionDisposition::DuplicateEquivalent
        );
        assert_eq!(duplicate, run);
        assert_eq!(initialize_count.get(), 1);
    }

    #[test]
    fn patch_manifest_accepts_optional_branch_and_commit_provenance() {
        let store = temp_store("patch-provenance");
        let mut params = successful(&store, "attempt-provenance");
        let WorkerRunExecution::Deterministic {
            result: Some(result),
        } = &mut params.execution
        else {
            panic!("successful fixture must be deterministic");
        };
        result.provenance = Some(crate::api::schema::WorkerRunProvenance {
            harness: "fixture-harness".into(),
            model: "opaque-model-under-test".into(),
            placement_ref: "local-pane:pane:fixture".into(),
            branch: Some("w/fixture".into()),
            commit: Some("a".repeat(40)),
        });
        let (run, _) = store.submit(params).unwrap();
        let result = store.result(run.result_ref.as_deref().unwrap()).unwrap();
        let provenance = result.provenance.unwrap();
        assert_eq!(provenance.branch.as_deref(), Some("w/fixture"));
        assert_eq!(
            provenance.commit.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn result_manifest_rejects_unbounded_fixture_output() {
        let store = temp_store("result-bounds");
        let mut params = successful(&store, "attempt-unbounded");
        let WorkerRunExecution::Deterministic {
            result: Some(result),
        } = &mut params.execution
        else {
            panic!("successful fixture must be deterministic");
        };
        result.artifacts = (0..129)
            .map(|index| WorkerRunArtifact {
                kind: WorkerRunArtifactKind::Log,
                reference: format!("artifact://worker/log-{index}"),
                hash: hash('d'),
                media_type: "text/plain".into(),
            })
            .collect();
        let error = store.submit(params).unwrap_err();
        assert_eq!(error.code, "worker_run_invalid");
        assert!(error.message.contains("between 1 and 128"));
    }

    #[test]
    fn rfc3339_formatter_matches_unix_epoch() {
        assert_eq!(rfc3339_from_unix_ms(0), "1970-01-01T00:00:00Z");
    }
}
