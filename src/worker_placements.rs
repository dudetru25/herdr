use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The single explicitly approved worker placement. Both approval facts — the
/// target tag and the operating system it was approved for — live here so a
/// placement cannot be approved by one fact alone.
pub struct ApprovedWorkerPlacement {
    pub target_tag: &'static str,
    pub os: &'static str,
}

pub const APPROVED_LOCAL_PLACEMENT: ApprovedWorkerPlacement = ApprovedWorkerPlacement {
    target_tag: "herdr-target:local-macos-primary",
    os: "macos",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPlacementKind {
    LocalPane,
    Remote,
    RemoteTemporary,
    TemporarySubspace,
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
    pub workspace_id: String,
    pub cwd: PathBuf,
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

pub fn resolve_worker_placement(
    target_tag: &str,
    requested_kind: WorkerPlacementKind,
    candidates: &[WorkerPlacementCandidate],
) -> Result<ResolvedWorkerPlacement, WorkerPlacementFailure> {
    resolve_worker_placement_for_platform(
        target_tag,
        requested_kind,
        candidates,
        std::env::consts::OS,
    )
}

fn resolve_worker_placement_for_platform(
    target_tag: &str,
    requested_kind: WorkerPlacementKind,
    candidates: &[WorkerPlacementCandidate],
    host_os: &str,
) -> Result<ResolvedWorkerPlacement, WorkerPlacementFailure> {
    if matches!(
        requested_kind,
        WorkerPlacementKind::Remote
            | WorkerPlacementKind::RemoteTemporary
            | WorkerPlacementKind::TemporarySubspace
    ) {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Unavailable,
            reason: "remote execution and temporary sub-spaces are unavailable because no remote target is approved".into(),
        });
    }
    if target_tag != APPROVED_LOCAL_PLACEMENT.target_tag {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::UnknownTarget,
            reason: format!("worker placement target {target_tag:?} is not approved"),
        });
    }

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

    if candidate.kind != WorkerPlacementKind::LocalPane {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Unavailable,
            reason: "the approved worker target is bound to a non-local placement".into(),
        });
    }
    if host_os != APPROVED_LOCAL_PLACEMENT.os {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Unavailable,
            reason: format!(
                "the approved worker target is available only on local {}",
                APPROVED_LOCAL_PLACEMENT.os
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
            reason: "local pane placement requires an explicit workspace id".into(),
        })?;
    let cwd = candidate
        .cwd
        .clone()
        .ok_or_else(|| WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: "local pane placement requires an explicit working directory".into(),
        })?;
    if !cwd.is_absolute() {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: "local pane placement working directory must be absolute".into(),
        });
    }
    if !cwd.is_dir() {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: format!("local pane placement working directory {cwd:?} does not exist"),
        });
    }
    // The approved harnesses refuse to run outside a trusted repository checkout,
    // so the precondition is resolved here instead of failing after a pane exists.
    if !cwd
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
    {
        return Err(WorkerPlacementFailure {
            kind: WorkerPlacementFailureKind::Blocked,
            reason: format!(
                "local pane placement working directory {cwd:?} is not inside a repository checkout"
            ),
        });
    }

    Ok(ResolvedWorkerPlacement {
        target_tag: target_tag.into(),
        workspace_id,
        cwd,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn local_candidate(available: bool) -> WorkerPlacementCandidate {
        WorkerPlacementCandidate {
            target_tag: "herdr-target:local-macos-primary".into(),
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

    #[test]
    fn only_approved_local_pane_target_resolves() {
        let resolved = resolve_worker_placement_for_platform(
            "herdr-target:local-macos-primary",
            WorkerPlacementKind::LocalPane,
            &[local_candidate(true)],
            APPROVED_LOCAL_PLACEMENT.os,
        )
        .unwrap();
        assert_eq!(resolved.target_tag, "herdr-target:local-macos-primary");
        assert_eq!(resolved.workspace_id, "workspace:1");
        assert_eq!(resolved.cwd, PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn unknown_and_ambiguous_tags_fail_closed() {
        let unknown = resolve_worker_placement_for_platform(
            "herdr-target:unknown",
            WorkerPlacementKind::LocalPane,
            &[local_candidate(true)],
            APPROVED_LOCAL_PLACEMENT.os,
        )
        .unwrap_err();
        assert_eq!(unknown.kind, WorkerPlacementFailureKind::UnknownTarget);

        let ambiguous = resolve_worker_placement_for_platform(
            "herdr-target:local-macos-primary",
            WorkerPlacementKind::LocalPane,
            &[local_candidate(true), local_candidate(true)],
            APPROVED_LOCAL_PLACEMENT.os,
        )
        .unwrap_err();
        assert_eq!(ambiguous.kind, WorkerPlacementFailureKind::AmbiguousTarget);
    }

    #[test]
    fn non_repository_working_directory_fails_closed_before_any_launch() {
        let outside_repository = std::env::temp_dir().join(format!(
            "herdr-worker-placement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&outside_repository).unwrap();
        let mut candidate = local_candidate(true);
        candidate.cwd = Some(outside_repository.clone());
        let failure = resolve_worker_placement_for_platform(
            APPROVED_LOCAL_PLACEMENT.target_tag,
            WorkerPlacementKind::LocalPane,
            &[candidate],
            APPROVED_LOCAL_PLACEMENT.os,
        )
        .unwrap_err();
        std::fs::remove_dir_all(&outside_repository).unwrap();
        assert_eq!(failure.kind, WorkerPlacementFailureKind::Blocked);
        assert!(failure.reason.contains("repository"));
    }

    #[test]
    fn unavailable_local_and_all_remote_requests_are_explicit_results() {
        let local = resolve_worker_placement_for_platform(
            "herdr-target:local-macos-primary",
            WorkerPlacementKind::LocalPane,
            &[local_candidate(false)],
            APPROVED_LOCAL_PLACEMENT.os,
        )
        .unwrap_err();
        assert_eq!(local.kind, WorkerPlacementFailureKind::Unavailable);
        assert!(local.reason.contains("local pane runtime"));

        let remote = resolve_worker_placement_for_platform(
            "herdr-target:remote-temporary",
            WorkerPlacementKind::RemoteTemporary,
            &[],
            APPROVED_LOCAL_PLACEMENT.os,
        )
        .unwrap_err();
        assert_eq!(remote.kind, WorkerPlacementFailureKind::Unavailable);
        assert!(remote.reason.contains("no remote target is approved"));
        assert!(!remote.reason.contains("fallback"));
    }
}
