use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::schema::WorkerHarness;
use crate::config::{WorkerPlacementConfig, WorkerPlacementKindConfig};

#[cfg(test)]
pub struct TestApprovedWorkerPlacement {
    pub target_tag: &'static str,
}

#[cfg(test)]
pub const APPROVED_LOCAL_PLACEMENT: TestApprovedWorkerPlacement = TestApprovedWorkerPlacement {
    target_tag: "herdr-target:local-macos-primary",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPlacementKind {
    LocalPane,
    Remote,
    RemoteTemporary,
    TemporarySubspace,
}

impl WorkerPlacementKind {
    pub fn is_remote(self) -> bool {
        matches!(
            self,
            Self::Remote | Self::RemoteTemporary | Self::TemporarySubspace
        )
    }
}

impl From<WorkerPlacementKindConfig> for WorkerPlacementKind {
    fn from(kind: WorkerPlacementKindConfig) -> Self {
        match kind {
            WorkerPlacementKindConfig::LocalPane => Self::LocalPane,
            WorkerPlacementKindConfig::Remote => Self::Remote,
            WorkerPlacementKindConfig::RemoteTemporary => Self::RemoteTemporary,
            WorkerPlacementKindConfig::TemporarySubspace => Self::TemporarySubspace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkerPlacementAvailability {
    Available,
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementCandidate {
    pub target_tag: String,
    pub kind: WorkerPlacementKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub availability: WorkerPlacementAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkerPlacement {
    pub target_tag: String,
    pub kind: WorkerPlacementKind,
    pub workspace_id: String,
    pub cwd: PathBuf,
    pub machine: Option<String>,
    pub approval_ref: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPlacementFailureKind {
    UnknownTarget,
    AmbiguousTarget,
    Unavailable,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerPlacementFailure {
    pub kind: WorkerPlacementFailureKind,
    pub reason: String,
}

/// Resolves one approved worker placement. Approval comes entirely from the
/// user-granted `[[worker_placements]]` registry; no host, path, or operating
/// system fact compiled into this file may approve or reject a placement.
pub fn resolve_worker_placement(
    target_tag: &str,
    harness: WorkerHarness,
    requested_kind: WorkerPlacementKind,
    candidates: &[WorkerPlacementCandidate],
    approved: &[WorkerPlacementConfig],
) -> Result<ResolvedWorkerPlacement, WorkerPlacementFailure> {
    let matching = candidates
        .iter()
        .filter(|candidate| candidate.target_tag == target_tag)
        .collect::<Vec<_>>();
    let candidate = match matching.as_slice() {
        [] => {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::UnknownTarget,
                reason: format!("worker placement target {target_tag:?} is not configured"),
            });
        }
        [candidate] => *candidate,
        _ => {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::AmbiguousTarget,
                reason: format!("worker placement target {target_tag:?} has multiple bindings"),
            });
        }
    };

    if candidate.kind != requested_kind {
        return Err(WorkerPlacementFailure {
            kind: if requested_kind.is_remote() {
                WorkerPlacementFailureKind::Unavailable
            } else {
                WorkerPlacementFailureKind::Blocked
            },
            reason: if requested_kind.is_remote() {
                "remote execution requires a candidate bound to an approved remote target; no remote target is approved for this binding".into()
            } else {
                "the requested worker placement kind does not match the candidate binding".into()
            },
        });
    }

    let approval_matches = approved
        .iter()
        .filter(|placement| placement.target_tag == target_tag)
        .collect::<Vec<_>>();
    let approval = match approval_matches.as_slice() {
        [] => {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::UnknownTarget,
                reason: format!("worker placement target {target_tag:?} is not approved"),
            });
        }
        [approval] => *approval,
        _ => {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::AmbiguousTarget,
                reason: format!("worker placement target {target_tag:?} has multiple approvals"),
            });
        }
    };

    let approved_kind = WorkerPlacementKind::from(approval.kind);
    if requested_kind != approved_kind || candidate.kind != approved_kind {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: format!(
                "worker placement target {target_tag:?} is approved for {approved_kind:?}, not {requested_kind:?}"
            ),
        });
    }

    let required_harness = match harness {
        WorkerHarness::Codex => "codex",
        WorkerHarness::Claude => "claude",
    };
    if !approval
        .harnesses
        .iter()
        .any(|authorized| authorized == required_harness)
    {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Unavailable,
            reason: format!(
                "worker placement target {target_tag:?} does not authorize the {required_harness} harness"
            ),
        });
    }

    if let WorkerPlacementAvailability::Unavailable { reason } = &candidate.availability {
        if reason.trim().is_empty() {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::Blocked,
                reason: "unavailable worker placement must carry a non-empty reason".into(),
            });
        }
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Unavailable,
            reason: reason.clone(),
        });
    }

    let workspace_id = candidate
        .workspace_id
        .clone()
        .ok_or_else(|| WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: "worker placement requires an explicit workspace id".into(),
        })?;
    let cwd = candidate
        .cwd
        .clone()
        .ok_or_else(|| WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: "worker placement requires an explicit working directory".into(),
        })?;
    let configured_cwd = approval.cwd.as_deref().unwrap_or_default();
    if cwd.to_string_lossy() != configured_cwd {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: format!(
                "worker placement working directory does not match the approved directory {configured_cwd:?}"
            ),
        });
    }

    let machine = approval.machine.clone();
    if approved_kind.is_remote() {
        if machine
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::Blocked,
                reason: "approved remote placement has no machine binding".into(),
            });
        }
    } else {
        if machine.is_some() {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::Blocked,
                reason: "approved local placement must not have a machine binding".into(),
            });
        }
        if !cwd.is_absolute() {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::Blocked,
                reason: "local worker placement working directory must be absolute".into(),
            });
        }
        if !cwd.is_dir() {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::Blocked,
                reason: format!("local worker placement working directory {cwd:?} does not exist"),
            });
        }
        if !cwd
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists())
        {
            return Err(WorkerPlacementFailure {
                kind: WorkerPlacementFailureKind::Blocked,
                reason: format!(
                    "local worker placement working directory {cwd:?} is not inside a repository checkout"
                ),
            });
        }
    }

    Ok(ResolvedWorkerPlacement {
        target_tag: target_tag.into(),
        kind: approved_kind,
        workspace_id,
        cwd,
        machine,
        approval_ref: approval.approval.reference.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{WorkerPlacementApprovalConfig, WorkerPlacementKindConfig};

    const LOCAL_TARGET: &str = "herdr-target:local-macos-primary";
    const REMOTE_TARGET: &str = "herdr-target:remote-windows-primary";

    fn approval(target_tag: &str, kind: WorkerPlacementKindConfig) -> WorkerPlacementConfig {
        WorkerPlacementConfig {
            target_tag: target_tag.into(),
            kind,
            machine: (kind != WorkerPlacementKindConfig::LocalPane).then(|| "RUG-DEV-3-WIN".into()),
            cwd: Some(if kind == WorkerPlacementKindConfig::LocalPane {
                env!("CARGO_MANIFEST_DIR").into()
            } else {
                "G:\\GithubProjects\\agent-ide-lab".into()
            }),
            harnesses: vec!["codex".into(), "claude".into()],
            approval: WorkerPlacementApprovalConfig {
                reference: "user-approved:TASK-10".into(),
                approved_at: "2026-08-01T05:20:00Z".into(),
            },
        }
    }

    fn local_candidate(available: bool) -> WorkerPlacementCandidate {
        WorkerPlacementCandidate {
            target_tag: LOCAL_TARGET.into(),
            kind: WorkerPlacementKind::LocalPane,
            workspace_id: Some("workspace:1".into()),
            cwd: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
            availability: if available {
                WorkerPlacementAvailability::Available
            } else {
                WorkerPlacementAvailability::Unavailable {
                    reason: "local pane runtime is not available".into(),
                }
            },
        }
    }

    fn remote_candidate() -> WorkerPlacementCandidate {
        WorkerPlacementCandidate {
            target_tag: REMOTE_TARGET.into(),
            kind: WorkerPlacementKind::Remote,
            workspace_id: Some("workspace:remote".into()),
            cwd: Some(PathBuf::from("G:\\GithubProjects\\agent-ide-lab")),
            availability: WorkerPlacementAvailability::Available,
        }
    }

    #[test]
    fn only_explicitly_approved_local_pane_resolves() {
        let resolved = resolve_worker_placement(
            LOCAL_TARGET,
            WorkerHarness::Codex,
            WorkerPlacementKind::LocalPane,
            &[local_candidate(true)],
            &[approval(LOCAL_TARGET, WorkerPlacementKindConfig::LocalPane)],
        )
        .unwrap();
        assert_eq!(resolved.target_tag, LOCAL_TARGET);
        assert_eq!(resolved.workspace_id, "workspace:1");
        assert_eq!(resolved.cwd, PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        assert_eq!(resolved.approval_ref, "user-approved:TASK-10");
    }

    #[test]
    fn approved_remote_target_resolves_without_a_local_os_assumption() {
        let resolved = resolve_worker_placement(
            REMOTE_TARGET,
            WorkerHarness::Claude,
            WorkerPlacementKind::Remote,
            &[remote_candidate()],
            &[approval(REMOTE_TARGET, WorkerPlacementKindConfig::Remote)],
        )
        .unwrap();
        assert_eq!(resolved.machine.as_deref(), Some("RUG-DEV-3-WIN"));
        assert_eq!(
            resolved.cwd,
            PathBuf::from("G:\\GithubProjects\\agent-ide-lab")
        );
    }

    #[test]
    fn approved_local_placement_resolves_without_a_host_os_literal() {
        let resolved = resolve_worker_placement(
            LOCAL_TARGET,
            WorkerHarness::Codex,
            WorkerPlacementKind::LocalPane,
            &[local_candidate(true)],
            &[approval(LOCAL_TARGET, WorkerPlacementKindConfig::LocalPane)],
        )
        .unwrap();
        assert_eq!(resolved.target_tag, LOCAL_TARGET);
        assert_eq!(resolved.approval_ref, "user-approved:TASK-10");
    }

    #[test]
    fn unknown_and_ambiguous_targets_fail_closed() {
        let unknown = resolve_worker_placement(
            "herdr-target:unknown",
            WorkerHarness::Codex,
            WorkerPlacementKind::LocalPane,
            &[local_candidate(true)],
            &[approval(LOCAL_TARGET, WorkerPlacementKindConfig::LocalPane)],
        )
        .unwrap_err();
        assert_eq!(unknown.kind, WorkerPlacementFailureKind::UnknownTarget);

        let ambiguous = resolve_worker_placement(
            LOCAL_TARGET,
            WorkerHarness::Codex,
            WorkerPlacementKind::LocalPane,
            &[local_candidate(true), local_candidate(true)],
            &[approval(LOCAL_TARGET, WorkerPlacementKindConfig::LocalPane)],
        )
        .unwrap_err();
        assert_eq!(ambiguous.kind, WorkerPlacementFailureKind::AmbiguousTarget);
    }

    #[test]
    fn unavailable_and_unauthorized_harnesses_are_explicit_results() {
        let local = resolve_worker_placement(
            LOCAL_TARGET,
            WorkerHarness::Codex,
            WorkerPlacementKind::LocalPane,
            &[local_candidate(false)],
            &[approval(LOCAL_TARGET, WorkerPlacementKindConfig::LocalPane)],
        )
        .unwrap_err();
        assert_eq!(local.kind, WorkerPlacementFailureKind::Unavailable);
        assert!(local.reason.contains("local pane runtime"));

        let mut claude_only = approval(REMOTE_TARGET, WorkerPlacementKindConfig::Remote);
        claude_only.harnesses = vec!["claude".into()];
        let unauthorized = resolve_worker_placement(
            REMOTE_TARGET,
            WorkerHarness::Codex,
            WorkerPlacementKind::Remote,
            &[remote_candidate()],
            &[claude_only],
        )
        .unwrap_err();
        assert_eq!(unauthorized.kind, WorkerPlacementFailureKind::Unavailable);
        assert!(unauthorized.reason.contains("codex"));
    }
}
