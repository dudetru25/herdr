use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::schema::{WorkerHarness, WorkerRunMetadata, WorkerRunRequest};
use crate::worker_placements::{
    ResolvedWorkerPlacement, WorkerPlacementFailure, WorkerPlacementFailureKind,
    WorkerPlacementKind,
};

#[derive(Debug)]
pub struct WorkerAdapterError {
    pub code: &'static str,
    pub message: String,
}

impl WorkerAdapterError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Directory the approved worker publishes its result manifest and patch bytes
/// into. It is set by Herdr, never inherited, and never part of the immutable
/// provider-neutral request.
pub const WORKER_ARTIFACT_DIR_ENV_VAR: &str = "HERDR_WORKER_ARTIFACT_DIR";

/// Manifest file every approved worker publishes into its artifact directory.
pub const WORKER_RESULT_MANIFEST_FILE: &str = "result.json";

const ASSIGNMENT_SCHEMA: &str = "herdr-worker-assignment/v1";
const PUBLICATION_SCHEMA: &str = "herdr-worker-result-publication/v1";

/// Where and how the worker publishes its result manifest. Herdr owns every
/// value here; none of it is read from the ambient environment or from the
/// immutable provider-neutral request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkerResultPublication {
    schema: &'static str,
    directory: String,
    manifest_path: String,
    manifest_schema: String,
    instruction: String,
}

/// The exact payload one approved worker receives: the immutable Skills request
/// plus the Herdr-owned location it must publish its result manifest into.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkerAssignment<'a> {
    schema: &'static str,
    request: &'a WorkerRunRequest,
    result_publication: WorkerResultPublication,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLaunchSpec {
    pub run_id: String,
    pub argv: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub request_bytes: Vec<u8>,
    pub cwd: PathBuf,
    pub artifact_dir: PathBuf,
    pub metadata: WorkerRunMetadata,
    pub resolved_placement: ResolvedWorkerPlacement,
}

pub(crate) const REMOTE_WORKER_ARTIFACT_MARKER: &str = "__HERDR_REMOTE_ARTIFACT_DIR__";

pub fn prepare_worker_launch(
    request: &WorkerRunRequest,
    metadata: &WorkerRunMetadata,
    placement: &ResolvedWorkerPlacement,
    run_id: &str,
    artifact_dir: &Path,
) -> Result<WorkerLaunchSpec, WorkerAdapterError> {
    for (field, value) in [
        ("profile", metadata.profile.as_str()),
        ("model", metadata.model.as_str()),
        ("target", metadata.target.as_str()),
        ("placement", metadata.placement.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > 1_024 || value.chars().any(char::is_control) {
            return Err(WorkerAdapterError::new(
                "worker_adapter_invalid",
                format!("{field} must be non-empty, bounded, and contain no control characters"),
            ));
        }
    }
    if metadata.target != placement.target_tag {
        return Err(WorkerAdapterError::new(
            "worker_adapter_invalid",
            "resolved metadata target does not match placement target",
        ));
    }

    let request_bytes = canonical_json_bytes(request)?;
    let artifact_dir_value = artifact_dir.to_str().ok_or_else(|| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            "worker artifact directory is not valid utf-8",
        )
    })?;
    let cwd_value = placement.cwd.to_str().ok_or_else(|| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            "worker checkout directory is not valid utf-8",
        )
    })?;
    let codex_writable_roots = [
        toml_literal_string(cwd_value)?,
        toml_literal_string(artifact_dir_value)?,
    ]
    .join(",");
    let publication = result_publication(request, artifact_dir_value);
    let assignment_bytes = canonical_json_bytes(&WorkerAssignment {
        schema: ASSIGNMENT_SCHEMA,
        request,
        result_publication: publication,
    })?;

    // Each harness receives exactly the write authority its worker needs to
    // publish into the Herdr-owned artifact directory, and nothing wider.
    let mut argv = match metadata.harness {
        WorkerHarness::Codex => vec![
            "codex".into(),
            "exec".into(),
            "--ephemeral".into(),
            "--ignore-user-config".into(),
            "--ignore-rules".into(),
            "--json".into(),
            "--model".into(),
            metadata.model.clone(),
            "--profile".into(),
            metadata.profile.clone(),
            "--config".into(),
            "approval_policy='never'".into(),
            "--config".into(),
            "windows.sandbox='elevated'".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--config".into(),
            format!("sandbox_workspace_write.writable_roots=[{codex_writable_roots}]"),
        ],
        WorkerHarness::Claude => vec![
            "claude".into(),
            "--print".into(),
            "--safe-mode".into(),
            "--no-session-persistence".into(),
            "--output-format".into(),
            "json".into(),
            "--model".into(),
            metadata.model.clone(),
            "--agent".into(),
            metadata.profile.clone(),
            // `--add-dir` is variadic, so it must never be the last flag before
            // the payload or the payload is swallowed as another directory.
            "--add-dir".into(),
            artifact_dir_value.into(),
            "--permission-mode".into(),
            "auto".into(),
        ],
    };
    argv.push(String::from_utf8(assignment_bytes).map_err(|error| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            format!("worker assignment payload is not utf-8: {error}"),
        )
    })?);

    let mut environment = isolated_worker_environment()?;
    environment.push((
        WORKER_ARTIFACT_DIR_ENV_VAR.to_string(),
        artifact_dir_value.to_string(),
    ));

    Ok(WorkerLaunchSpec {
        run_id: run_id.to_string(),
        argv,
        environment,
        request_bytes,
        cwd: placement.cwd.clone(),
        artifact_dir: artifact_dir.to_path_buf(),
        metadata: metadata.clone(),
        resolved_placement: placement.clone(),
    })
}

fn toml_literal_string(value: &str) -> Result<String, WorkerAdapterError> {
    if value.chars().any(char::is_control) || value.contains("'''") {
        return Err(WorkerAdapterError::new(
            "worker_adapter_invalid",
            "worker artifact directory cannot be represented as a TOML literal string",
        ));
    }
    Ok(format!("'''{value}'''"))
}

/// Replace only Herdr-owned artifact paths in a prepared launch. The marker is
/// expanded by the remote PowerShell bootstrap after it resolves the remote
/// temp directory; request bytes and client environment values are unchanged.
pub(crate) fn rewrite_worker_argv_for_remote(
    launch: &WorkerLaunchSpec,
) -> Result<Vec<String>, WorkerAdapterError> {
    let local_artifact_dir = launch.artifact_dir.to_str().ok_or_else(|| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            "worker artifact directory is not valid utf-8",
        )
    })?;
    let mut argv = launch.argv.clone();
    let local_artifact_literal = toml_literal_string(local_artifact_dir)?;
    let remote_artifact_literal = toml_literal_string(REMOTE_WORKER_ARTIFACT_MARKER)?;
    let mut replaced = false;
    for index in 0..argv.len().saturating_sub(1) {
        if argv[index] == "--add-dir" && argv[index + 1] == local_artifact_dir {
            argv[index + 1] = REMOTE_WORKER_ARTIFACT_MARKER.to_string();
            replaced = true;
        } else if argv[index].starts_with("sandbox_workspace_write.writable_roots=") {
            let updated = argv[index].replace(&local_artifact_literal, &remote_artifact_literal);
            replaced |= updated != argv[index];
            argv[index] = updated;
        }
    }
    let payload = argv.last_mut().ok_or_else(|| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            "remote worker launch did not contain an assignment payload",
        )
    })?;
    let mut assignment: serde_json::Value = serde_json::from_str(payload).map_err(|error| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            format!("remote worker assignment payload is invalid JSON: {error}"),
        )
    })?;
    let publication = assignment
        .get_mut("resultPublication")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| {
            WorkerAdapterError::new(
                "worker_adapter_invalid",
                "remote worker assignment payload is missing resultPublication",
            )
        })?;
    for field in ["directory", "manifestPath", "instruction"] {
        let value = publication
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let Some(value) = value else {
            return Err(WorkerAdapterError::new(
                "worker_adapter_invalid",
                format!("remote worker resultPublication is missing {field}"),
            ));
        };
        let updated = value.replace(local_artifact_dir, REMOTE_WORKER_ARTIFACT_MARKER);
        replaced |= updated != value;
        publication.insert(field.to_string(), serde_json::Value::String(updated));
    }
    if !replaced {
        return Err(WorkerAdapterError::new(
            "worker_adapter_invalid",
            "remote worker launch did not contain the Herdr artifact directory",
        ));
    }
    *payload = String::from_utf8(canonical_json_bytes(&assignment)?).map_err(|error| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            format!("remote worker assignment payload is not utf-8: {error}"),
        )
    })?;
    Ok(argv)
}

fn result_publication(request: &WorkerRunRequest, artifact_dir: &str) -> WorkerResultPublication {
    let manifest_path = format!(
        "{}/{WORKER_RESULT_MANIFEST_FILE}",
        artifact_dir.trim_end_matches('/')
    );
    let patch_rule = if request.result_contract.require_patch_for_code_changes {
        " A run that changes code must include one patch or diff artifact."
    } else {
        ""
    };
    WorkerResultPublication {
        schema: PUBLICATION_SCHEMA,
        directory: artifact_dir.to_string(),
        manifest_path: manifest_path.clone(),
        manifest_schema: request.result_contract.schema.clone(),
        instruction: format!(
            "Before exiting, write your result manifest as JSON to {manifest_path}. \
             It must contain summary, change {{kind: none|code, changedFiles: [repository-relative paths]}}, \
             and a non-empty artifacts list of {{kind, ref, hash, mediaType}} entries, where every patch or \
             diff ref is a path relative to {artifact_dir} whose bytes you also write there and hash is \
             sha256:<hex> of exactly those bytes.{patch_rule} \
             Exiting successfully without a valid manifest at that path is recorded as a failed run."
        ),
    }
}

const WORKER_ENVIRONMENT_ALLOWLIST: [&str; 6] = [
    "HOME",
    "PATH",
    "TMPDIR",
    // The approved harnesses resolve their stored local credentials by account
    // identity; without it a real worker exits unauthenticated.
    "USER",
    crate::integration::CODEX_HOME_ENV_VAR,
    crate::integration::CLAUDE_CONFIG_DIR_ENV_VAR,
];

fn isolated_worker_environment() -> Result<Vec<(String, String)>, WorkerAdapterError> {
    let environment = WORKER_ENVIRONMENT_ALLOWLIST
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .and_then(|value| value.into_string().ok())
                .map(|value| (key.to_string(), value))
        })
        .collect::<Vec<_>>();
    for required in ["HOME", "PATH"] {
        if !environment.iter().any(|(key, _)| key == required) {
            return Err(WorkerAdapterError::new(
                "worker_harness_unavailable",
                format!("{required} is unavailable for the isolated worker environment"),
            ));
        }
    }
    Ok(environment)
}

pub(crate) fn canonical_json_bytes(value: &impl Serialize) -> Result<Vec<u8>, WorkerAdapterError> {
    let value = serde_json::to_value(value).map_err(|error| {
        WorkerAdapterError::new(
            "worker_adapter_invalid",
            format!("serialize provider-neutral worker request: {error}"),
        )
    })?;
    let mut bytes = Vec::new();
    write_canonical_json(&value, &mut bytes)?;
    Ok(bytes)
}

fn write_canonical_json(
    value: &serde_json::Value,
    output: &mut Vec<u8>,
) -> Result<(), WorkerAdapterError> {
    match value {
        serde_json::Value::Object(object) => {
            output.push(b'{');
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key).map_err(|error| {
                    WorkerAdapterError::new(
                        "worker_adapter_invalid",
                        format!("serialize provider-neutral worker request key: {error}"),
                    )
                })?;
                output.push(b':');
                write_canonical_json(&object[key], output)?;
            }
            output.push(b'}');
        }
        serde_json::Value::Array(items) => {
            output.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(item, output)?;
            }
            output.push(b']');
        }
        _ => serde_json::to_writer(output, value).map_err(|error| {
            WorkerAdapterError::new(
                "worker_adapter_invalid",
                format!("serialize provider-neutral worker request value: {error}"),
            )
        })?,
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSmokeStatus {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerSmokePrerequisite {
    pub harness: WorkerHarness,
    pub placement: WorkerPlacementKind,
    pub status: WorkerSmokeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl WorkerSmokePrerequisite {
    pub fn is_available(&self) -> bool {
        self.status == WorkerSmokeStatus::Available
    }
}

pub fn smoke_prerequisite(
    harness: WorkerHarness,
    placement: WorkerPlacementKind,
) -> WorkerSmokePrerequisite {
    if placement.is_remote() {
        return WorkerSmokePrerequisite {
            harness,
            placement,
            status: WorkerSmokeStatus::Available,
            reason: None,
        };
    }
    if placement != WorkerPlacementKind::LocalPane {
        return WorkerSmokePrerequisite {
            harness,
            placement,
            status: WorkerSmokeStatus::Unavailable,
            reason: Some(
                "remote execution and temporary sub-spaces are unavailable because no remote target is approved"
                    .into(),
            ),
        };
    }
    smoke_prerequisite_with(harness, crate::integration::command_available)
}

fn smoke_prerequisite_with(
    harness: WorkerHarness,
    available: impl FnOnce(&str) -> bool,
) -> WorkerSmokePrerequisite {
    let command = match harness {
        WorkerHarness::Codex => "codex",
        WorkerHarness::Claude => "claude",
    };
    if available(command) {
        WorkerSmokePrerequisite {
            harness,
            placement: WorkerPlacementKind::LocalPane,
            status: WorkerSmokeStatus::Available,
            reason: None,
        }
    } else {
        WorkerSmokePrerequisite {
            harness,
            placement: WorkerPlacementKind::LocalPane,
            status: WorkerSmokeStatus::Unavailable,
            reason: Some(format!("{command} executable is unavailable on PATH")),
        }
    }
}

impl From<WorkerPlacementFailure> for WorkerAdapterError {
    fn from(failure: WorkerPlacementFailure) -> Self {
        let code = match failure.kind {
            WorkerPlacementFailureKind::UnknownTarget => "worker_placement_unknown",
            WorkerPlacementFailureKind::AmbiguousTarget => "worker_placement_ambiguous",
            WorkerPlacementFailureKind::Unavailable => "worker_placement_unavailable",
            WorkerPlacementFailureKind::Blocked => "worker_placement_blocked",
        };
        Self::new(code, failure.reason)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::api::schema::{
        WorkerHarness, WorkerRunContext, WorkerRunContextInput, WorkerRunLifecycle,
        WorkerRunMetadata, WorkerRunRequest, WorkerRunResultContract,
    };
    use crate::worker_placements::ResolvedWorkerPlacement;

    use super::*;

    fn hash(character: char) -> String {
        format!("sha256:{}", character.to_string().repeat(64))
    }

    fn request() -> WorkerRunRequest {
        WorkerRunRequest {
            schema: "skills-herdr-worker-request/v1".into(),
            role: "implementation-worker".into(),
            capabilities: vec!["read-repository".into(), "edit-repository".into()],
            context: WorkerRunContext {
                schema: "skills-herdr-worker-context/v1".into(),
                instruction: "Change only fixture.txt and return a patch manifest.".into(),
                repository_ref: "github.com/example/project".into(),
                revision: hash('a'),
                inputs: vec![WorkerRunContextInput {
                    reference: "skills-attempt://TASK-10.2/attempt-1".into(),
                    hash: hash('b'),
                }],
            },
            lifecycle: WorkerRunLifecycle {
                deadline_unix_ms: Some(1_900_000_000_000),
            },
            result_contract: WorkerRunResultContract {
                schema: "skills-herdr-worker-result/v1".into(),
                require_patch_for_code_changes: true,
            },
        }
    }

    fn placement() -> ResolvedWorkerPlacement {
        ResolvedWorkerPlacement {
            target_tag: "herdr-target:local-macos-primary".into(),
            kind: WorkerPlacementKind::LocalPane,
            workspace_id: "workspace:1".into(),
            cwd: PathBuf::from("/explicit/request/worktree"),
            machine: None,
            approval_ref: "test:placement".into(),
        }
    }

    #[test]
    fn codex_and_claude_share_one_provider_neutral_contract() {
        let request = request();
        let request_before = canonical_json_bytes(&request).unwrap();
        for harness in [WorkerHarness::Codex, WorkerHarness::Claude] {
            let metadata = WorkerRunMetadata {
                harness,
                profile: "profile-under-test".into(),
                model: "opaque-model-under-test".into(),
                target: "herdr-target:local-macos-primary".into(),
                placement: "local-pane:workspace:1".into(),
            };
            let launch = prepare_worker_launch(
                &request,
                &metadata,
                &placement(),
                "worker-run:0123456789abcdef0123456789abcdef",
                Path::new("/explicit/request/artifacts"),
            )
            .unwrap();
            assert_eq!(launch.request_bytes, request_before);
            let payload: serde_json::Value =
                serde_json::from_slice(launch.argv.last().unwrap().as_bytes()).unwrap();
            assert_eq!(
                canonical_json_bytes(&payload["request"]).unwrap(),
                request_before
            );
            assert_eq!(launch.metadata, metadata);
            assert_eq!(launch.cwd, placement().cwd);
            assert!(launch.environment.iter().all(|(key, _)| {
                (WORKER_ENVIRONMENT_ALLOWLIST.contains(&key.as_str())
                    || key == WORKER_ARTIFACT_DIR_ENV_VAR)
                    && !key.contains("TOKEN")
                    && !key.contains("SECRET")
                    && !key.contains("API_KEY")
            }));
            assert_eq!(
                launch
                    .environment
                    .iter()
                    .find(|(key, _)| key == WORKER_ARTIFACT_DIR_ENV_VAR)
                    .map(|(_, value)| value.as_str()),
                Some("/explicit/request/artifacts")
            );
            assert!(launch
                .argv
                .iter()
                .any(|arg| arg == "opaque-model-under-test"));
            assert_eq!(canonical_json_bytes(&request).unwrap(), request_before);
        }
    }

    #[test]
    fn remote_rewrite_changes_only_herdr_owned_artifact_fields() {
        let mut request = request();
        request.context.instruction = format!(
            "Keep the literal path /explicit/request/artifacts and marker {REMOTE_WORKER_ARTIFACT_MARKER} unchanged."
        );
        let metadata = WorkerRunMetadata {
            harness: WorkerHarness::Claude,
            profile: "profile-under-test".into(),
            model: "opaque-model-under-test".into(),
            target: "herdr-target:local-macos-primary".into(),
            placement: "local-pane:workspace:1".into(),
        };
        let launch = prepare_worker_launch(
            &request,
            &metadata,
            &placement(),
            "worker-run:0123456789abcdef0123456789abcdef",
            Path::new("/explicit/request/artifacts"),
        )
        .unwrap();
        let rewritten = rewrite_worker_argv_for_remote(&launch).unwrap();
        let add_dir = rewritten
            .iter()
            .position(|argument| argument == "--add-dir")
            .unwrap();
        assert_eq!(rewritten[add_dir + 1], REMOTE_WORKER_ARTIFACT_MARKER);
        let payload: serde_json::Value = serde_json::from_str(rewritten.last().unwrap()).unwrap();
        assert_eq!(
            payload["request"]["context"]["instruction"],
            request.context.instruction
        );
        assert_eq!(
            canonical_json_bytes(&payload["request"]).unwrap(),
            canonical_json_bytes(&request).unwrap()
        );
        assert_eq!(
            payload["resultPublication"]["directory"],
            REMOTE_WORKER_ARTIFACT_MARKER
        );
        assert!(payload["resultPublication"]["instruction"]
            .as_str()
            .unwrap()
            .contains(REMOTE_WORKER_ARTIFACT_MARKER));
    }

    #[test]
    fn worker_payload_states_the_herdr_owned_result_publication_location() {
        let request = request();
        for harness in [WorkerHarness::Codex, WorkerHarness::Claude] {
            let metadata = WorkerRunMetadata {
                harness,
                profile: "profile-under-test".into(),
                model: "opaque-model-under-test".into(),
                target: "herdr-target:local-macos-primary".into(),
                placement: "local-pane:workspace:1".into(),
            };
            let launch = prepare_worker_launch(
                &request,
                &metadata,
                &placement(),
                "worker-run:0123456789abcdef0123456789abcdef",
                Path::new("/explicit/request/artifacts"),
            )
            .unwrap();
            let payload: serde_json::Value =
                serde_json::from_slice(launch.argv.last().unwrap().as_bytes()).unwrap();

            // The worker learns the publication location from the payload itself.
            let publication = &payload["resultPublication"];
            assert_eq!(publication["directory"], "/explicit/request/artifacts");
            assert_eq!(
                publication["manifestPath"],
                "/explicit/request/artifacts/result.json"
            );
            assert_eq!(
                publication["manifestSchema"],
                request.result_contract.schema.as_str()
            );
            assert!(publication["instruction"]
                .as_str()
                .unwrap()
                .contains("result.json"));

            // The immutable hash-bound Skills request is carried unchanged.
            assert_eq!(
                launch.request_bytes,
                canonical_json_bytes(&request).unwrap()
            );
            assert_eq!(
                canonical_json_bytes(&payload["request"]).unwrap(),
                launch.request_bytes
            );

            // The approved harness may write into exactly that directory.
            assert!(launch
                .argv
                .iter()
                .any(|argument| argument.contains("/explicit/request/artifacts")));

            // A variadic flag directly before the payload would swallow it.
            assert_ne!(launch.argv[launch.argv.len() - 2], "--add-dir");
        }
    }

    #[test]
    fn codex_writable_roots_include_checkout_and_artifacts() {
        let metadata = WorkerRunMetadata {
            harness: WorkerHarness::Codex,
            profile: "profile-under-test".into(),
            model: "opaque-model-under-test".into(),
            target: "herdr-target:local-macos-primary".into(),
            placement: "local-pane:workspace:1".into(),
        };
        let launch = prepare_worker_launch(
            &request(),
            &metadata,
            &placement(),
            "worker-run:0123456789abcdef0123456789abcdef",
            Path::new("/explicit/o'malley/artifacts"),
        )
        .unwrap();
        let config = launch
            .argv
            .windows(2)
            .find(|window| {
                window[0] == "--config"
                    && window[1].starts_with("sandbox_workspace_write.writable_roots=")
            })
            .map(|window| window[1].as_str())
            .expect("Codex launch should include its writable-root config");

        assert!(launch
            .argv
            .windows(2)
            .any(|window| { window[0] == "--config" && window[1] == "approval_policy='never'" }));
        assert!(launch.argv.windows(2).any(|window| {
            window[0] == "--config" && window[1] == "windows.sandbox='elevated'"
        }));
        assert_eq!(
            config,
            "sandbox_workspace_write.writable_roots=['''/explicit/request/worktree''','''/explicit/o'malley/artifacts''']"
        );
    }

    #[test]
    fn result_publication_location_is_never_inherited_from_the_ambient_environment() {
        assert!(!WORKER_ENVIRONMENT_ALLOWLIST.contains(&WORKER_ARTIFACT_DIR_ENV_VAR));
        let metadata = WorkerRunMetadata {
            harness: WorkerHarness::Claude,
            profile: "profile-under-test".into(),
            model: "opaque-model-under-test".into(),
            target: "herdr-target:local-macos-primary".into(),
            placement: "local-pane:workspace:1".into(),
        };
        let launch = prepare_worker_launch(
            &request(),
            &metadata,
            &placement(),
            "worker-run:0123456789abcdef0123456789abcdef",
            Path::new("/explicit/request/artifacts"),
        )
        .unwrap();
        let published = launch
            .environment
            .iter()
            .filter(|(key, _)| key == WORKER_ARTIFACT_DIR_ENV_VAR)
            .collect::<Vec<_>>();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].1, "/explicit/request/artifacts");
    }

    #[test]
    fn adapter_payload_has_no_implicit_history_context_or_secrets() {
        let request = request();
        let metadata = WorkerRunMetadata {
            harness: WorkerHarness::Codex,
            profile: "profile-under-test".into(),
            model: "opaque-model-under-test".into(),
            target: "herdr-target:local-macos-primary".into(),
            placement: "local-pane:workspace:1".into(),
        };
        let launch = prepare_worker_launch(
            &request,
            &metadata,
            &placement(),
            "worker-run:0123456789abcdef0123456789abcdef",
            Path::new("/explicit/request/artifacts"),
        )
        .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&launch.request_bytes).unwrap();
        assert!(payload.get("chat_history").is_none());
        assert!(payload.get("messages").is_none());
        assert!(payload.get("environment").is_none());
        assert!(payload.get("credentials").is_none());
        assert!(payload.get("secrets").is_none());
    }

    #[test]
    fn smoke_prerequisites_report_each_exact_harness_without_substitution() {
        let codex = smoke_prerequisite_with(WorkerHarness::Codex, |command| command == "codex");
        assert!(codex.is_available());

        let claude = smoke_prerequisite_with(WorkerHarness::Claude, |command| command == "codex");
        assert!(!claude.is_available());
        assert_eq!(claude.harness, WorkerHarness::Claude);
        assert!(claude.reason.unwrap().contains("claude"));
    }

    #[test]
    fn local_smoke_reports_real_prerequisite_for_both_harnesses() {
        for harness in [WorkerHarness::Codex, WorkerHarness::Claude] {
            let result = smoke_prerequisite(harness, WorkerPlacementKind::LocalPane);
            assert_eq!(result.harness, harness);
            assert_eq!(result.placement, WorkerPlacementKind::LocalPane);
            assert_eq!(
                result.status == WorkerSmokeStatus::Available,
                result.reason.is_none()
            );
            println!("{}", serde_json::to_value(result).unwrap());
        }
    }

    #[test]
    fn remote_smoke_defers_harness_resolution_to_the_approved_remote_device() {
        for harness in [WorkerHarness::Codex, WorkerHarness::Claude] {
            let result = smoke_prerequisite(harness, WorkerPlacementKind::RemoteTemporary);
            assert_eq!(result.harness, harness);
            assert_eq!(result.status, WorkerSmokeStatus::Available);
            assert!(result.is_available());
            assert!(result.reason.is_none());
            let json = serde_json::to_value(result).unwrap();
            assert_eq!(json["status"], "available");
            println!("{json}");
        }
    }
}
