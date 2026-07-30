use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::schema::{
    WorkerRunArtifact, WorkerRunArtifactKind, WorkerRunCancellationDisposition, WorkerRunChange,
    WorkerRunChangeKind, WorkerRunExecution, WorkerRunRecord, WorkerRunResultManifest,
    WorkerRunResultStatus, WorkerRunResultTemplate, WorkerRunState, WorkerRunSubmissionDisposition,
    WorkerRunSubmitParams,
};

const STORE_SCHEMA: &str = "herdr-worker-run-store/v1";
const RECORD_SCHEMA: &str = "herdr-worker-run/v1";
const RESULT_SCHEMA: &str = "skills-herdr-worker-result/v1";

#[derive(Debug)]
pub struct WorkerRunError {
    pub code: &'static str,
    pub message: String,
}

impl WorkerRunError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
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

    pub fn submit(
        &self,
        params: WorkerRunSubmitParams,
    ) -> Result<(WorkerRunRecord, WorkerRunSubmissionDisposition), WorkerRunError> {
        validate_submit(&params)?;
        let content_hash = hash_json(&(
            &params.request_hash,
            &params.context_hash,
            params.deadline_unix_ms,
            &params.execution,
        ))?;
        self.update(|state| {
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
                ));
            }

            let now = unix_ms();
            let run_id = run_id(&params.attempt_id, &content_hash);
            let mut stored = StoredWorkerRun {
                record: WorkerRunRecord {
                    schema: RECORD_SCHEMA.into(),
                    attempt_id: params.attempt_id.clone(),
                    run_id: run_id.clone(),
                    request_hash: params.request_hash,
                    context_hash: params.context_hash,
                    content_hash,
                    state: WorkerRunState::Running,
                    created_unix_ms: now,
                    updated_unix_ms: now,
                    deadline_unix_ms: params.deadline_unix_ms,
                    cancellation_requested: false,
                    terminal_reason: None,
                    result_ref: None,
                },
                execution: params.execution,
                result: None,
            };
            apply_deadline_or_deterministic_result(&mut stored, now)?;
            state.attempts.insert(params.attempt_id, run_id.clone());
            let record = stored.record.clone();
            state.runs.insert(run_id, stored);
            Ok((record, WorkerRunSubmissionDisposition::Created))
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

fn apply_deadline_or_deterministic_result(
    stored: &mut StoredWorkerRun,
    now: u64,
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
    let WorkerRunExecution::Deterministic { result } = &stored.execution;
    if let Some(template) = result.clone() {
        validate_result_template(&template)?;
        let completed_at = rfc3339_from_unix_ms(now);
        stored.result = Some(WorkerRunResultManifest {
            schema: RESULT_SCHEMA.into(),
            attempt_id: stored.record.attempt_id.clone(),
            run_ref: stored.record.run_id.clone(),
            status: WorkerRunResultStatus::Succeeded,
            summary: template.summary,
            change: template.change,
            artifacts: template.artifacts,
            provenance: None,
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
    if reason.is_empty() {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "terminal reason must not be empty",
        ));
    }
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

fn validate_result_template(template: &WorkerRunResultTemplate) -> Result<(), WorkerRunError> {
    if template.summary.trim().is_empty() {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "result summary must not be empty",
        ));
    }
    if template.artifacts.is_empty() {
        return Err(WorkerRunError::new(
            "worker_run_invalid",
            "result artifacts must not be empty",
        ));
    }
    for artifact in &template.artifacts {
        if artifact.reference.trim().is_empty() || artifact.media_type.trim().is_empty() {
            return Err(WorkerRunError::new(
                "worker_run_invalid",
                "result artifact reference and media type must not be empty",
            ));
        }
        validate_hash("artifact hash", &artifact.hash)?;
    }
    for path in &template.change.changed_files {
        if path.is_empty()
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
    let bytes = serde_json::to_vec(value).map_err(|error| {
        WorkerRunError::new(
            "worker_run_invalid",
            format!("serialize immutable worker request: {error}"),
        )
    })?;
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
mod tests {
    use super::*;

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

    fn deferred(attempt_id: &str, request_hash: &str) -> WorkerRunSubmitParams {
        WorkerRunSubmitParams {
            attempt_id: attempt_id.into(),
            request_hash: request_hash.into(),
            context_hash: hash('b'),
            deadline_unix_ms: None,
            execution: WorkerRunExecution::Deterministic { result: None },
        }
    }

    fn successful(attempt_id: &str) -> WorkerRunSubmitParams {
        WorkerRunSubmitParams {
            attempt_id: attempt_id.into(),
            request_hash: hash('a'),
            context_hash: hash('b'),
            deadline_unix_ms: None,
            execution: WorkerRunExecution::Deterministic {
                result: Some(WorkerRunResultTemplate {
                    summary: "deterministic worker completed".into(),
                    change: WorkerRunChange {
                        kind: WorkerRunChangeKind::Code,
                        changed_files: vec!["src/lib.rs".into()],
                    },
                    artifacts: vec![WorkerRunArtifact {
                        kind: WorkerRunArtifactKind::Patch,
                        reference: "artifact://worker/change.patch".into(),
                        hash: hash('c'),
                        media_type: "text/x-diff".into(),
                    }],
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
        request.deadline_unix_ms = Some(1);
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
        let (run, _) = store.submit(successful("attempt-result-ref")).unwrap();
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
        let (run, _) = store.submit(successful("attempt-success")).unwrap();
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
    fn rfc3339_formatter_matches_unix_epoch() {
        assert_eq!(rfc3339_from_unix_ms(0), "1970-01-01T00:00:00Z");
    }
}
