use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerRunSubmitParams {
    pub attempt_id: String,
    pub request_hash: String,
    pub context_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    pub execution: WorkerRunExecution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerRunExecution {
    Deterministic {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<WorkerRunResultTemplate>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerRunTarget {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerRunResultTarget {
    pub result_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerRunCancelParams {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerRunTimeoutParams {
    pub run_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerRunState {
    Submitted,
    Running,
    Cancelled,
    TimedOut,
    Failed,
    Lost,
    Succeeded,
}

impl WorkerRunState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::TimedOut | Self::Failed | Self::Lost | Self::Succeeded
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerRunSubmissionDisposition {
    Created,
    DuplicateEquivalent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerRunCancellationDisposition {
    Requested,
    AlreadyRequested,
    AlreadyTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerRunRecord {
    pub schema: String,
    pub attempt_id: String,
    pub run_id: String,
    pub request_hash: String,
    pub context_hash: String,
    pub content_hash: String,
    pub state: WorkerRunState,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    #[serde(default)]
    pub cancellation_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerRunResultTemplate {
    pub summary: String,
    pub change: WorkerRunChange,
    pub artifacts: Vec<WorkerRunArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunResultManifest {
    pub schema: String,
    pub attempt_id: String,
    pub run_ref: String,
    pub status: WorkerRunResultStatus,
    pub summary: String,
    pub change: WorkerRunChange,
    pub artifacts: Vec<WorkerRunArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<WorkerRunProvenance>,
    pub completed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerRunResultStatus {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunChange {
    pub kind: WorkerRunChangeKind,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRunChangeKind {
    None,
    Code,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunArtifact {
    pub kind: WorkerRunArtifactKind,
    #[serde(rename = "ref")]
    pub reference: String,
    pub hash: String,
    pub media_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerRunArtifactKind {
    Patch,
    Diff,
    TestOutput,
    ValidationReport,
    Log,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunProvenance {
    pub harness: String,
    pub model: String,
    pub placement_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}
