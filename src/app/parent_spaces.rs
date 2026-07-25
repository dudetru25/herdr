use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::{state::ParentSpaceAction, App, AppState};
use crate::workspace::ParentSpaceMembership;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParentSpaceActionOutcome {
    pub parent_workspace_id: String,
    pub child_workspace_ids: Vec<String>,
    pub cleared_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParentSpaceActionError {
    pub code: &'static str,
    pub message: String,
}

impl ParentSpaceActionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct ParentSpaceScanPlan {
    adopted_indices: Vec<usize>,
    missing_directories: Vec<PathBuf>,
}

fn parent_space_key(root: &Path) -> String {
    format!("folder:{}", root.display())
}

fn immediate_subdirectories(root: &Path) -> Result<Vec<PathBuf>, ParentSpaceActionError> {
    let entries = std::fs::read_dir(root).map_err(|err| {
        ParentSpaceActionError::new(
            "parent_space_scan_failed",
            format!("failed to scan parent-space root {}: {err}", root.display()),
        )
    })?;
    let mut directories = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                warn!(path = %root.display(), %err, "failed to read parent-space entry");
                continue;
            }
        };
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                warn!(path = %entry.path().display(), %err, "failed to inspect parent-space entry");
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        match entry.path().canonicalize() {
            Ok(path) => directories.push((entry.file_name(), path)),
            Err(err) => {
                warn!(path = %entry.path().display(), %err, "failed to canonicalize sub-space");
            }
        }
    }
    directories.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut seen = HashSet::new();
    Ok(directories
        .into_iter()
        .filter_map(|(_, path)| seen.insert(path.clone()).then_some(path))
        .collect())
}

impl AppState {
    fn apply_parent_space_scan(
        &mut self,
        parent_idx: usize,
        membership: &ParentSpaceMembership,
        directories: &[PathBuf],
    ) -> ParentSpaceScanPlan {
        let Some(parent) = self.workspaces.get_mut(parent_idx) else {
            return ParentSpaceScanPlan {
                adopted_indices: Vec::new(),
                missing_directories: Vec::new(),
            };
        };
        parent.parent_space = Some(membership.clone());

        let existing_paths = self
            .workspaces
            .iter()
            .enumerate()
            .filter_map(
                |(ws_idx, workspace)| match workspace.identity_cwd.canonicalize() {
                    Ok(path) => Some((ws_idx, path)),
                    Err(err) => {
                        warn!(
                            workspace_id = %workspace.id,
                            path = %workspace.identity_cwd.display(),
                            %err,
                            "failed to canonicalize workspace identity for parent-space adoption"
                        );
                        None
                    }
                },
            )
            .collect::<Vec<_>>();

        let mut adopted_indices = Vec::new();
        let mut missing_directories = Vec::new();
        for directory in directories {
            let existing_idx = existing_paths
                .iter()
                .find_map(|(ws_idx, path)| (path == directory).then_some(*ws_idx));
            let Some(existing_idx) = existing_idx else {
                missing_directories.push(directory.clone());
                continue;
            };
            let previous_parent_key = self
                .workspaces
                .get(existing_idx)
                .and_then(|workspace| workspace.parent_space())
                .filter(|previous| previous.is_parent && previous.key != membership.key)
                .map(|previous| previous.key.clone());
            if let Some(previous_parent_key) = previous_parent_key {
                warn!(
                    workspace_id = %self.workspaces[existing_idx].id,
                    path = %directory.display(),
                    "demoting an existing parent space during adoption"
                );
                for workspace in &mut self.workspaces {
                    if workspace
                        .parent_space()
                        .is_some_and(|membership| membership.key == previous_parent_key)
                    {
                        workspace.parent_space = None;
                    }
                }
                self.collapsed_space_keys.remove(&previous_parent_key);
            }
            let child_membership = ParentSpaceMembership {
                key: membership.key.clone(),
                root: membership.root.clone(),
                is_parent: false,
            };
            if let Some(workspace) = self.workspaces.get_mut(existing_idx) {
                if workspace.parent_space.as_ref() != Some(&child_membership) {
                    workspace.parent_space = Some(child_membership);
                    adopted_indices.push(existing_idx);
                }
            }
        }
        self.mark_session_dirty();
        ParentSpaceScanPlan {
            adopted_indices,
            missing_directories,
        }
    }

    fn clear_parent_space_group(&mut self, parent_idx: usize) -> usize {
        let Some(key) = self
            .workspaces
            .get(parent_idx)
            .and_then(|workspace| workspace.parent_space())
            .filter(|membership| membership.is_parent)
            .map(|membership| membership.key.clone())
        else {
            return 0;
        };
        let mut cleared_count = 0;
        for workspace in &mut self.workspaces {
            if workspace
                .parent_space()
                .is_some_and(|membership| membership.key == key)
            {
                workspace.parent_space = None;
                cleared_count += 1;
            }
        }
        self.collapsed_space_keys.remove(&key);
        self.mark_session_dirty();
        cleared_count
    }
}

impl App {
    pub(crate) fn apply_parent_space_action(
        &mut self,
        ws_idx: usize,
        action: ParentSpaceAction,
    ) -> Result<ParentSpaceActionOutcome, ParentSpaceActionError> {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return Err(ParentSpaceActionError::new(
                "workspace_not_found",
                "workspace not found",
            ));
        };
        let parent_workspace_id = workspace.id.clone();
        match action {
            ParentSpaceAction::Become => {
                if self.workspace_is_linked_worktree(ws_idx) {
                    return Err(ParentSpaceActionError::new(
                        "linked_worktree_parent_space",
                        "linked-worktree workspaces cannot become parent spaces",
                    ));
                }
                if workspace.is_parent_space() {
                    return self.scan_parent_space(ws_idx, false);
                }
                if workspace.parent_space().is_some() {
                    return Err(ParentSpaceActionError::new(
                        "parent_space_child",
                        "a child workspace cannot become a parent space",
                    ));
                }
                self.scan_parent_space(ws_idx, true)
            }
            ParentSpaceAction::Rescan => {
                if !workspace.is_parent_space() {
                    return Err(ParentSpaceActionError::new(
                        "not_parent_space",
                        "workspace is not a parent space",
                    ));
                }
                self.scan_parent_space(ws_idx, false)
            }
            ParentSpaceAction::Stop => {
                if !workspace.is_parent_space() {
                    return Err(ParentSpaceActionError::new(
                        "not_parent_space",
                        "workspace is not a parent space",
                    ));
                }
                let cleared_count = self.state.clear_parent_space_group(ws_idx);
                Ok(ParentSpaceActionOutcome {
                    parent_workspace_id,
                    child_workspace_ids: Vec::new(),
                    cleared_count,
                })
            }
        }
    }

    fn workspace_is_linked_worktree(&self, ws_idx: usize) -> bool {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return false;
        };
        workspace
            .worktree_space()
            .is_some_and(|space| space.is_linked_worktree)
            || workspace
                .git_space()
                .cloned()
                .or_else(|| crate::workspace::git_space_metadata(&workspace.identity_cwd))
                .is_some_and(|space| space.is_linked_worktree)
    }

    fn scan_parent_space(
        &mut self,
        ws_idx: usize,
        create_parent: bool,
    ) -> Result<ParentSpaceActionOutcome, ParentSpaceActionError> {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return Err(ParentSpaceActionError::new(
                "workspace_not_found",
                "workspace not found",
            ));
        };
        let parent_workspace_id = workspace.id.clone();
        let membership = if create_parent {
            let root = workspace.identity_cwd.canonicalize().map_err(|err| {
                ParentSpaceActionError::new(
                    "parent_space_invalid_root",
                    format!(
                        "failed to resolve parent-space root {}: {err}",
                        workspace.identity_cwd.display()
                    ),
                )
            })?;
            if !root.is_dir() {
                return Err(ParentSpaceActionError::new(
                    "parent_space_invalid_root",
                    format!("parent-space root {} is not a directory", root.display()),
                ));
            }
            ParentSpaceMembership {
                key: parent_space_key(&root),
                root,
                is_parent: true,
            }
        } else {
            let Some(membership) = workspace
                .parent_space()
                .filter(|membership| membership.is_parent)
                .cloned()
            else {
                return Err(ParentSpaceActionError::new(
                    "not_parent_space",
                    "workspace is not a parent space",
                ));
            };
            membership
        };

        let directories = immediate_subdirectories(&membership.root)?;
        let plan = self
            .state
            .apply_parent_space_scan(ws_idx, &membership, &directories);
        let mut child_workspace_ids = plan
            .adopted_indices
            .into_iter()
            .filter_map(|child_idx| {
                self.state
                    .workspaces
                    .get(child_idx)
                    .map(|workspace| workspace.id.clone())
            })
            .collect::<Vec<_>>();
        for path in plan.missing_directories {
            match self.create_workspace_with_options(path.clone(), false) {
                Ok(child_idx) => {
                    if let Some(child) = self.state.workspaces.get_mut(child_idx) {
                        child.parent_space = Some(ParentSpaceMembership {
                            key: membership.key.clone(),
                            root: membership.root.clone(),
                            is_parent: false,
                        });
                        child_workspace_ids.push(child.id.clone());
                    }
                    self.emit_workspace_open_events(child_idx);
                }
                Err(err) => {
                    return Err(ParentSpaceActionError::new(
                        "parent_space_child_create_failed",
                        format!(
                            "failed to create child workspace for {}: {err}",
                            path.display()
                        ),
                    ));
                }
            }
        }
        self.state.mark_session_dirty();
        Ok(ParentSpaceActionOutcome {
            parent_workspace_id,
            child_workspace_ids,
            cleared_count: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::workspace::Workspace;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    struct TempFixture {
        root: PathBuf,
    }

    impl TempFixture {
        fn new() -> Self {
            let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "herdr-parent-spaces-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn child_membership(parent: &ParentSpaceMembership) -> ParentSpaceMembership {
        ParentSpaceMembership {
            key: parent.key.clone(),
            root: parent.root.clone(),
            is_parent: false,
        }
    }

    #[test]
    fn scan_adopts_existing_workspace_and_rescan_only_returns_missing_directories() {
        let fixture = TempFixture::new();
        let alpha = fixture.root.join("alpha");
        let archive = fixture.root.join("_archive");
        std::fs::create_dir(&alpha).unwrap();
        std::fs::create_dir(&archive).unwrap();
        std::fs::create_dir(fixture.root.join(".cache")).unwrap();
        std::fs::write(fixture.root.join("notes.txt"), "not a directory").unwrap();

        let root = fixture.root.canonicalize().unwrap();
        let membership = ParentSpaceMembership {
            key: parent_space_key(&root),
            root,
            is_parent: true,
        };
        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = fixture.root.clone();
        let mut adopted = Workspace::test_new("alpha");
        adopted.identity_cwd = alpha.canonicalize().unwrap();
        let mut state = AppState::test_new();
        state.workspaces = vec![parent, adopted];

        let directories = immediate_subdirectories(&fixture.root).unwrap();
        assert_eq!(
            directories,
            vec![
                archive.canonicalize().unwrap(),
                alpha.canonicalize().unwrap()
            ]
        );
        let plan = state.apply_parent_space_scan(0, &membership, &directories);
        assert_eq!(
            plan.missing_directories,
            vec![archive.canonicalize().unwrap()]
        );
        assert_eq!(plan.adopted_indices, vec![1]);
        assert_eq!(
            state.workspaces[1].parent_space,
            Some(child_membership(&membership))
        );

        let mut created = Workspace::test_new("_archive");
        created.identity_cwd = plan.missing_directories[0].clone();
        created.parent_space = Some(child_membership(&membership));
        state.workspaces.push(created);

        let beta = fixture.root.join("beta");
        std::fs::create_dir(&beta).unwrap();
        let plan = state.apply_parent_space_scan(
            0,
            &membership,
            &immediate_subdirectories(&fixture.root).unwrap(),
        );
        assert_eq!(plan.missing_directories, vec![beta.canonicalize().unwrap()]);
        assert_eq!(state.workspaces.len(), 3);
    }

    #[test]
    fn stop_parent_space_clears_parent_and_child_membership() {
        let fixture = TempFixture::new();
        let root = fixture.root.canonicalize().unwrap();
        let membership = ParentSpaceMembership {
            key: parent_space_key(&root),
            root,
            is_parent: true,
        };
        let mut parent = Workspace::test_new("parent");
        parent.parent_space = Some(membership.clone());
        let mut child = Workspace::test_new("child");
        child.parent_space = Some(child_membership(&membership));
        let mut unrelated = Workspace::test_new("unrelated");
        unrelated.parent_space = Some(ParentSpaceMembership {
            key: "folder:other".into(),
            root: "/other".into(),
            is_parent: false,
        });
        let mut state = AppState::test_new();
        state.workspaces = vec![parent, child, unrelated];
        state.collapsed_space_keys.insert(membership.key.clone());

        assert_eq!(state.clear_parent_space_group(0), 2);
        assert!(state.workspaces[0].parent_space.is_none());
        assert!(state.workspaces[1].parent_space.is_none());
        assert!(state.workspaces[2].parent_space.is_some());
        assert!(!state.collapsed_space_keys.contains(&membership.key));
    }
}
