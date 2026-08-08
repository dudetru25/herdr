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
    released_indices: Vec<usize>,
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
        let entry = entry.map_err(|err| {
            ParentSpaceActionError::new(
                "parent_space_scan_failed",
                format!(
                    "failed to read an entry in parent-space root {}: {err}",
                    root.display()
                ),
            )
        })?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let entry_path = entry.path();
        let file_type = entry.file_type().map_err(|err| {
            ParentSpaceActionError::new(
                "parent_space_scan_failed",
                format!(
                    "failed to inspect parent-space entry {}: {err}",
                    entry_path.display()
                ),
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let canonical_path = entry_path.canonicalize().map_err(|err| {
            ParentSpaceActionError::new(
                "parent_space_scan_failed",
                format!(
                    "failed to canonicalize sub-space {}: {err}",
                    entry_path.display()
                ),
            )
        })?;
        directories.push((entry.file_name(), canonical_path));
    }
    directories.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut seen = HashSet::new();
    Ok(directories
        .into_iter()
        .filter_map(|(_, path)| seen.insert(path.clone()).then_some(path))
        .collect())
}

impl AppState {
    fn plan_parent_space_scan(
        &self,
        parent_idx: usize,
        membership: &ParentSpaceMembership,
        directories: &[PathBuf],
    ) -> Result<ParentSpaceScanPlan, ParentSpaceActionError> {
        let Some(parent) = self.workspaces.get(parent_idx) else {
            return Err(ParentSpaceActionError::new(
                "workspace_not_found",
                "workspace not found",
            ));
        };
        let previous_parent_key = parent
            .parent_space()
            .filter(|membership| membership.is_parent)
            .map(|membership| membership.key.clone());

        let existing_paths = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| !workspace.is_machine())
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
            adopted_indices.push(existing_idx);
        }
        let directory_paths = directories.iter().collect::<HashSet<_>>();
        let adopted = adopted_indices.iter().copied().collect::<HashSet<_>>();
        let mut released_indices = Vec::new();
        if let Some(previous_parent_key) = previous_parent_key.as_ref() {
            for (ws_idx, workspace) in self.workspaces.iter().enumerate() {
                if ws_idx == parent_idx
                    || adopted.contains(&ws_idx)
                    || !workspace.parent_space().is_some_and(|previous| {
                        !previous.is_parent && previous.key == *previous_parent_key
                    })
                {
                    continue;
                }
                let resolved_path = existing_paths
                    .iter()
                    .find_map(|(existing_idx, path)| (*existing_idx == ws_idx).then_some(path));
                let parent_moved = previous_parent_key != &membership.key;
                if parent_moved || resolved_path.is_some_and(|path| !directory_paths.contains(path))
                {
                    released_indices.push(ws_idx);
                }
            }
        }
        Ok(ParentSpaceScanPlan {
            adopted_indices,
            released_indices,
            missing_directories,
        })
    }

    fn apply_parent_space_scan(
        &mut self,
        parent_idx: usize,
        membership: &ParentSpaceMembership,
        plan: &ParentSpaceScanPlan,
    ) -> Result<(), ParentSpaceActionError> {
        if self.workspaces.get(parent_idx).is_none()
            || plan
                .adopted_indices
                .iter()
                .chain(&plan.released_indices)
                .any(|index| self.workspaces.get(*index).is_none())
        {
            return Err(ParentSpaceActionError::new(
                "workspace_not_found",
                "workspace disappeared while applying parent-space scan",
            ));
        }
        let previous_parent_key = self.workspaces[parent_idx]
            .parent_space()
            .filter(|previous| previous.is_parent)
            .map(|previous| previous.key.clone());
        self.workspaces[parent_idx].parent_space = Some(membership.clone());

        for &released_idx in &plan.released_indices {
            self.workspaces[released_idx].parent_space = None;
        }

        for &existing_idx in &plan.adopted_indices {
            let previous_parent_key = self.workspaces[existing_idx]
                .parent_space()
                .filter(|previous| previous.is_parent && previous.key != membership.key)
                .map(|previous| previous.key.clone());
            if let Some(previous_parent_key) = previous_parent_key {
                warn!(
                    workspace_id = %self.workspaces[existing_idx].id,
                    path = %self.workspaces[existing_idx].identity_cwd.display(),
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
            let workspace = &mut self.workspaces[existing_idx];
            if workspace.parent_space.as_ref() != Some(&child_membership) {
                workspace.parent_space = Some(child_membership);
            }
        }
        if let Some(previous_parent_key) = previous_parent_key {
            if previous_parent_key != membership.key
                && self.collapsed_space_keys.remove(&previous_parent_key)
            {
                self.collapsed_space_keys.insert(membership.key.clone());
            }
        }
        self.mark_session_dirty();
        Ok(())
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
    pub(crate) fn show_parent_space_error(&mut self, err: &ParentSpaceActionError) {
        let previous_toast = self.state.toast.clone();
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::NeedsAttention,
            title: "parent-space action failed".to_string(),
            context: format!("{}: {}", err.code, err.message),
            position: None,
            target: None,
        });
        self.sync_toast_deadline(previous_toast);
    }

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
        if workspace.is_machine() {
            return Err(ParentSpaceActionError::new(
                "machine_workspace_parent_space",
                "machine workspaces cannot participate in parent spaces",
            ));
        }
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
        if self.state.workspaces.get(ws_idx).is_none() {
            return Err(ParentSpaceActionError::new(
                "workspace_not_found",
                "workspace not found",
            ));
        }
        self.state.refresh_workspace_staleness();
        let workspace = &self.state.workspaces[ws_idx];
        let parent_workspace_id = workspace.id.clone();
        if !create_parent
            && !workspace
                .parent_space()
                .is_some_and(|membership| membership.is_parent)
        {
            return Err(ParentSpaceActionError::new(
                "not_parent_space",
                "workspace is not a parent space",
            ));
        }
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
        let membership = ParentSpaceMembership {
            key: parent_space_key(&root),
            root,
            is_parent: true,
        };

        let directories = immediate_subdirectories(&membership.root)?;
        let plan = self
            .state
            .plan_parent_space_scan(ws_idx, &membership, &directories)?;
        let mut prepared_children = Vec::with_capacity(plan.missing_directories.len());
        for path in &plan.missing_directories {
            let prepared = self
                .prepare_workspace_with_launch_env(path.clone(), Vec::new())
                .map_err(|err| {
                    ParentSpaceActionError::new(
                        "parent_space_child_create_failed",
                        format!(
                            "failed to create child workspace for {}: {err}",
                            path.display()
                        ),
                    )
                })?;
            prepared_children.push(prepared);
        }

        self.state
            .apply_parent_space_scan(ws_idx, &membership, &plan)?;
        let mut child_workspace_ids = plan
            .adopted_indices
            .iter()
            .map(|&child_idx| self.state.workspaces[child_idx].id.clone())
            .collect::<Vec<_>>();
        let mut created_indices = Vec::with_capacity(prepared_children.len());
        for prepared in prepared_children {
            let child_idx = self.finish_created_workspace(prepared, false);
            let child = &mut self.state.workspaces[child_idx];
            child.parent_space = Some(ParentSpaceMembership {
                key: membership.key.clone(),
                root: membership.root.clone(),
                is_parent: false,
            });
            child_workspace_ids.push(child.id.clone());
            created_indices.push(child_idx);
        }
        for child_idx in created_indices {
            self.emit_workspace_open_events(child_idx);
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

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn test_git_checkout(path: &Path) -> PathBuf {
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        path.canonicalize().unwrap()
    }

    fn app_with_workspaces(workspaces: Vec<Workspace>) -> App {
        let mut app = test_app();
        app.state.workspaces = workspaces;
        app.state.selected = 0;
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app
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
        let plan = state
            .plan_parent_space_scan(0, &membership, &directories)
            .unwrap();
        state
            .apply_parent_space_scan(0, &membership, &plan)
            .unwrap();
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
        let plan = state
            .plan_parent_space_scan(
                0,
                &membership,
                &immediate_subdirectories(&fixture.root).unwrap(),
            )
            .unwrap();
        state
            .apply_parent_space_scan(0, &membership, &plan)
            .unwrap();
        assert_eq!(plan.missing_directories, vec![beta.canonicalize().unwrap()]);
        assert_eq!(state.workspaces.len(), 3);
    }

    #[test]
    fn scan_ignores_workspace_with_missing_identity_path() {
        let fixture = TempFixture::new();
        let child_path = fixture.root.join("child");
        std::fs::create_dir(&child_path).unwrap();
        let membership = ParentSpaceMembership {
            key: parent_space_key(&fixture.root),
            root: fixture.root.clone(),
            is_parent: true,
        };

        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = fixture.root.clone();
        parent.parent_space = Some(membership.clone());
        let mut stale = Workspace::test_new("stale");
        stale.identity_cwd = fixture.root.join("removed-workspace");
        stale.parent_space = Some(child_membership(&membership));
        let mut state = AppState::test_new();
        state.workspaces = vec![parent, stale];
        let workspace_ids = state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        state.refresh_workspace_staleness();
        assert!(state.workspaces[1].is_stale());

        let directories = immediate_subdirectories(&fixture.root).unwrap();
        let plan = state
            .plan_parent_space_scan(0, &membership, &directories)
            .unwrap();
        state
            .apply_parent_space_scan(0, &membership, &plan)
            .unwrap();

        assert!(plan.adopted_indices.is_empty());
        assert!(plan.released_indices.is_empty());
        assert_eq!(
            plan.missing_directories,
            vec![child_path.canonicalize().unwrap()]
        );
        assert_eq!(
            state
                .workspaces
                .iter()
                .map(|workspace| workspace.id.clone())
                .collect::<Vec<_>>(),
            workspace_ids
        );
        assert_eq!(
            state.workspaces[1].identity_cwd,
            fixture.root.join("removed-workspace")
        );
        assert_eq!(
            state.workspaces[1].parent_space(),
            Some(&child_membership(&membership))
        );
    }

    #[test]
    fn rescan_adopts_a_retargeted_standalone_workspace_once() {
        let fixture = TempFixture::new();
        let parent_path = test_git_checkout(&fixture.root.join("parent"));
        let standalone_path = test_git_checkout(&fixture.root.join("standalone"));
        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = parent_path.clone();
        let mut standalone = Workspace::test_new("standalone");
        standalone.identity_cwd = standalone_path.clone();
        let standalone_id = standalone.id.clone();
        let mut app = app_with_workspaces(vec![parent, standalone]);

        let initial = app
            .apply_parent_space_action(0, ParentSpaceAction::Become)
            .unwrap();
        assert!(initial.child_workspace_ids.is_empty());

        let nested_path = parent_path.join("nested");
        std::fs::rename(&standalone_path, &nested_path).unwrap();
        app.state
            .retarget_workspace(1, nested_path.canonicalize().unwrap())
            .unwrap();

        let outcome = app
            .apply_parent_space_action(0, ParentSpaceAction::Rescan)
            .unwrap();

        assert_eq!(outcome.child_workspace_ids, vec![standalone_id]);
        assert_eq!(app.state.workspaces.len(), 2);
        let parent_membership = app.state.workspaces[0].parent_space().unwrap().clone();
        assert_eq!(
            app.state.workspaces[1].parent_space(),
            Some(&child_membership(&parent_membership))
        );
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn rescan_releases_a_child_retargeted_outside_the_parent() {
        let fixture = TempFixture::new();
        let parent_path = test_git_checkout(&fixture.root.join("parent"));
        let child_path = test_git_checkout(&parent_path.join("child"));
        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = parent_path.clone();
        let mut child = Workspace::test_new("child");
        child.identity_cwd = child_path.clone();
        let mut app = app_with_workspaces(vec![parent, child]);
        app.apply_parent_space_action(0, ParentSpaceAction::Become)
            .unwrap();
        assert!(app.state.workspaces[1].parent_space().is_some());

        let detached_path = fixture.root.join("detached");
        std::fs::rename(&child_path, &detached_path).unwrap();
        app.state
            .retarget_workspace(1, detached_path.canonicalize().unwrap())
            .unwrap();

        let outcome = app
            .apply_parent_space_action(0, ParentSpaceAction::Rescan)
            .unwrap();

        assert!(outcome.child_workspace_ids.is_empty());
        assert!(app.state.workspaces[1].parent_space().is_none());
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn rescan_rekeys_a_retargeted_parent_and_its_child() {
        let fixture = TempFixture::new();
        let old_parent_path = test_git_checkout(&fixture.root.join("old-parent"));
        let old_child_path = test_git_checkout(&old_parent_path.join("child"));
        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = old_parent_path.clone();
        let mut child = Workspace::test_new("child");
        child.identity_cwd = old_child_path;
        let child_id = child.id.clone();
        let mut app = app_with_workspaces(vec![parent, child]);
        app.apply_parent_space_action(0, ParentSpaceAction::Become)
            .unwrap();
        let old_membership = app.state.workspaces[0].parent_space().unwrap().clone();
        app.state
            .collapsed_space_keys
            .insert(old_membership.key.clone());

        let new_parent_path = fixture.root.join("new-parent");
        std::fs::rename(&old_parent_path, &new_parent_path).unwrap();
        let new_parent_path = new_parent_path.canonicalize().unwrap();
        let new_child_path = new_parent_path.join("child").canonicalize().unwrap();
        app.state
            .retarget_workspace(0, new_parent_path.clone())
            .unwrap();
        app.state.retarget_workspace(1, new_child_path).unwrap();

        let outcome = app
            .apply_parent_space_action(0, ParentSpaceAction::Rescan)
            .unwrap();

        let new_membership = ParentSpaceMembership {
            key: parent_space_key(&new_parent_path),
            root: new_parent_path,
            is_parent: true,
        };
        assert_eq!(outcome.child_workspace_ids, vec![child_id]);
        assert_eq!(
            app.state.workspaces[0].parent_space(),
            Some(&new_membership)
        );
        assert_eq!(
            app.state.workspaces[1].parent_space(),
            Some(&child_membership(&new_membership))
        );
        assert!(!app.state.collapsed_space_keys.contains(&old_membership.key));
        assert!(app.state.collapsed_space_keys.contains(&new_membership.key));
        app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn child_creation_failure_leaves_parent_space_state_unchanged() {
        let fixture = TempFixture::new();
        let adopted_path = fixture.root.join("adopted");
        let missing_path = fixture.root.join("missing");
        std::fs::create_dir(&adopted_path).unwrap();
        std::fs::create_dir(&missing_path).unwrap();

        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = fixture.root.clone();
        let mut adopted = Workspace::test_new("adopted");
        adopted.identity_cwd = adopted_path.canonicalize().unwrap();
        let mut app = test_app();
        app.state.workspaces = vec![parent, adopted];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.default_shell = fixture.root.join("missing-shell").display().to_string();

        let err = app
            .apply_parent_space_action(0, ParentSpaceAction::Become)
            .unwrap_err();

        assert_eq!(err.code, "parent_space_child_create_failed");
        assert_eq!(app.state.workspaces.len(), 2);
        assert!(app
            .state
            .workspaces
            .iter()
            .all(|workspace| workspace.parent_space().is_none()));
        assert_eq!(app.terminal_runtimes.len(), 0);
    }

    #[tokio::test]
    async fn moved_parent_child_creation_failure_preserves_the_old_group() {
        let fixture = TempFixture::new();
        let old_parent_path = test_git_checkout(&fixture.root.join("old-parent"));
        let old_child_path = test_git_checkout(&old_parent_path.join("child"));
        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = old_parent_path.clone();
        let mut child = Workspace::test_new("child");
        child.identity_cwd = old_child_path;
        let mut app = app_with_workspaces(vec![parent, child]);
        app.apply_parent_space_action(0, ParentSpaceAction::Become)
            .unwrap();
        let old_membership = app.state.workspaces[0].parent_space().unwrap().clone();
        app.state
            .collapsed_space_keys
            .insert(old_membership.key.clone());

        let new_parent_path = fixture.root.join("new-parent");
        std::fs::rename(&old_parent_path, &new_parent_path).unwrap();
        let new_parent_path = new_parent_path.canonicalize().unwrap();
        let new_child_path = new_parent_path.join("child").canonicalize().unwrap();
        app.state
            .retarget_workspace(0, new_parent_path.clone())
            .unwrap();
        app.state.retarget_workspace(1, new_child_path).unwrap();
        std::fs::create_dir(new_parent_path.join("missing-child")).unwrap();
        app.state.default_shell = fixture.root.join("missing-shell").display().to_string();

        let err = app
            .apply_parent_space_action(0, ParentSpaceAction::Rescan)
            .unwrap_err();

        assert_eq!(err.code, "parent_space_child_create_failed");
        assert_eq!(
            app.state.workspaces[0].parent_space(),
            Some(&old_membership)
        );
        assert_eq!(
            app.state.workspaces[1].parent_space(),
            Some(&child_membership(&old_membership))
        );
        assert!(app.state.collapsed_space_keys.contains(&old_membership.key));
        assert!(!app
            .state
            .collapsed_space_keys
            .contains(&parent_space_key(&new_parent_path)));
        assert_eq!(app.state.workspaces.len(), 2);
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[test]
    fn entry_canonicalization_failure_does_not_commit_parent_membership() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = TempFixture::new();
        std::fs::create_dir(fixture.root.join("child")).unwrap();
        let original_permissions = std::fs::metadata(&fixture.root).unwrap().permissions();
        std::fs::set_permissions(&fixture.root, std::fs::Permissions::from_mode(0o400)).unwrap();

        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = fixture.root.clone();
        let mut app = test_app();
        app.state.workspaces = vec![parent];
        app.state.active = Some(0);
        app.state.selected = 0;
        let result = app.apply_parent_space_action(0, ParentSpaceAction::Become);

        std::fs::set_permissions(&fixture.root, original_permissions).unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.code, "parent_space_scan_failed");
        assert!(app.state.workspaces[0].parent_space().is_none());
        assert_eq!(app.state.workspaces.len(), 1);
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
