use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, WorkspaceCreateParams,
    WorkspaceMoveBlockParams, WorkspaceMoveParams, WorkspaceParentSpaceParams,
    WorkspaceRenameParams, WorkspaceReportMetadataParams, WorkspaceRetargetParams, WorkspaceTarget,
};
use crate::app::{App, IntentionalPaneRestart};

use super::super::api_helpers::{normalize_metadata_source, normalize_metadata_ttl};
use super::super::state::ParentSpaceAction;
use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_workspace_list(&mut self, id: String) -> String {
        self.state.refresh_workspace_staleness();
        encode_success(
            id,
            ResponseResult::WorkspaceList {
                workspaces: self.workspace_list_info(),
            },
        )
    }

    pub(super) fn handle_workspace_get(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        let Some(_) = self.state.workspaces.get(index) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        self.state.refresh_workspace_staleness();

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_create(
        &mut self,
        id: String,
        params: WorkspaceCreateParams,
    ) -> String {
        if params.machine.is_some() && params.cwd.is_some() {
            return encode_error(
                id,
                "workspace_create_invalid",
                "machine and cwd are mutually exclusive",
            );
        }
        let extra_env = match super::env::normalize_launch_env(params.env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let created = if let Some(machine) = params.machine {
            self.create_machine_workspace_with_launch_env(machine, params.focus, extra_env)
        } else {
            let cwd = match params.cwd.map(PathBuf::from) {
                Some(cwd) => cwd,
                None => {
                    let follow_cwd = self.workspace_creation_source().and_then(|ws_idx| {
                        self.focused_pane_cwd_in_workspace(ws_idx)
                            .or_else(|| self.seed_cwd_from_workspace(ws_idx))
                    });
                    match self.resolve_new_terminal_cwd(follow_cwd) {
                        Ok(cwd) => cwd,
                        Err(err) => {
                            return encode_error(id, "workspace_create_failed", err.to_string())
                        }
                    }
                }
            };
            self.create_workspace_with_launch_env(cwd, params.focus, extra_env)
        };
        match created {
            Ok(index) => {
                if let Some(label) = params.label {
                    if let Some(workspace) = self.state.workspaces.get_mut(index) {
                        workspace.set_custom_name(label);
                        crate::logging::workspace_renamed(&workspace.id);
                    }
                }
                self.emit_workspace_open_events(index);
                encode_success(
                    id,
                    self.workspace_created_result(index)
                        .expect("new workspace should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "workspace_create_failed", err.to_string()),
        }
    }

    pub(super) fn handle_workspace_focus(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &target.workspace_id);
        }
        self.state.switch_workspace(index);

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_rename(
        &mut self,
        id: String,
        params: WorkspaceRenameParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let Some(ws) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        ws.set_custom_name(params.label.clone());
        crate::logging::workspace_renamed(&ws.id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceRenamed,
            data: EventData::WorkspaceRenamed {
                workspace_id: self.public_workspace_id(index),
                label: params.label,
            },
        });

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_retarget(
        &mut self,
        id: String,
        params: WorkspaceRetargetParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let Some(workspace) = self.state.workspaces.get(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if workspace.is_machine() {
            return encode_error(
                id,
                "workspace_retarget_machine_unsupported",
                "machine workspaces cannot be retargeted",
            );
        }

        let worktree_membership = workspace.worktree_space().cloned();
        let needs_worktree_repair = worktree_membership
            .as_ref()
            .is_some_and(|membership| membership.is_linked_worktree);
        let path = if needs_worktree_repair {
            match resolve_workspace_retarget_directory(&params.path) {
                Ok(path) => path,
                Err((code, message)) => return encode_error(id, code, message),
            }
        } else {
            match resolve_workspace_retarget_path(&params.path) {
                Ok(path) => path,
                Err((code, message)) => return encode_error(id, code, message),
            }
        };
        let pane_ids = self.state.pane_ids_for_workspace(index);
        let mut restarts = Vec::new();
        for pane_id in &pane_ids {
            if self.intentional_pane_restarts.contains_key(pane_id) {
                return encode_error(
                    id,
                    "workspace_retarget_restart_pending",
                    "workspace has a pane restart already in progress",
                );
            }
            let Some((pane_ws_idx, pane)) = self.find_pane(*pane_id) else {
                return encode_error(
                    id,
                    "workspace_retarget_state_invalid",
                    "workspace pane is missing",
                );
            };
            if pane_ws_idx != index {
                return encode_error(
                    id,
                    "workspace_retarget_state_invalid",
                    "workspace pane identity changed",
                );
            }
            let terminal_id = pane.attached_terminal_id.clone();
            if !self.state.terminals.contains_key(&terminal_id) {
                return encode_error(
                    id,
                    "workspace_retarget_state_invalid",
                    "workspace pane terminal is missing",
                );
            }
            if let Some(runtime) = self.terminal_runtimes.get(&terminal_id) {
                if self.pane_launch_env(index, *pane_id, Vec::new()).is_none() {
                    return encode_error(
                        id,
                        "workspace_retarget_state_invalid",
                        "workspace pane launch identity is invalid",
                    );
                }
                let (rows, cols) = runtime.current_size();
                restarts.push((
                    *pane_id,
                    IntentionalPaneRestart {
                        terminal_id,
                        rows,
                        cols,
                    },
                ));
            }
        }
        let worktree_repair = if let Some(membership) = worktree_membership
            .as_ref()
            .filter(|membership| membership.is_linked_worktree)
        {
            let parents = self
                .state
                .workspaces
                .iter()
                .enumerate()
                .filter_map(|(candidate_index, candidate)| {
                    let candidate_membership = candidate.worktree_space()?;
                    (candidate_index != index
                        && candidate_membership.key == membership.key
                        && !candidate_membership.is_linked_worktree
                        && crate::worktree::canonical_or_original(&candidate.identity_cwd)
                            == crate::worktree::canonical_or_original(
                                &candidate_membership.repo_root,
                            ))
                    .then_some(candidate_membership.repo_root.clone())
                })
                .collect::<Vec<_>>();
            if parents.len() != 1 {
                let code = if parents.is_empty() {
                    "workspace_retarget_worktree_parent_missing"
                } else {
                    "workspace_retarget_worktree_parent_ambiguous"
                };
                return encode_error(
                    id,
                    code,
                    format!(
                        "linked worktree {} requires exactly one retargeted parent checkout; found {}",
                        membership.checkout_path.display(),
                        parents.len()
                    ),
                );
            }
            let repair = match crate::worktree::repair_linked_worktree_after_parent_move(
                &path,
                &membership.key,
                &membership.repo_root,
                &membership.checkout_path,
                &parents[0],
            ) {
                Ok(repair) => repair,
                Err(err) => {
                    let code = match err.kind {
                        crate::worktree::LinkedWorktreeRepairErrorKind::PointerBroken => {
                            "workspace_retarget_worktree_pointer_broken"
                        }
                        crate::worktree::LinkedWorktreeRepairErrorKind::RepairFailed => {
                            "workspace_retarget_worktree_repair_failed"
                        }
                    };
                    return encode_error(id, code, err.message);
                }
            };
            if let Err((code, message)) = resolve_workspace_retarget_path(&params.path) {
                return encode_error(id, code, message);
            }
            Some(repair)
        } else {
            None
        };
        if let Err(message) = self.state.retarget_workspace(index, path.clone()) {
            return encode_error(id, "workspace_retarget_state_invalid", message);
        }
        self.state
            .apply_retargeted_worktree_membership(index, &path, worktree_repair.as_ref());
        for (pane_id, restart) in restarts {
            let terminal_id = restart.terminal_id.clone();
            self.intentional_pane_restarts.insert(pane_id, restart);
            self.shutdown_terminal_runtime(terminal_id);
        }
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceUpdated,
            data: EventData::WorkspaceUpdated {
                workspace: self.workspace_info(index),
            },
        });
        for pane_id in pane_ids {
            self.emit_pane_updated(index, pane_id);
        }

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_move(
        &mut self,
        id: String,
        params: WorkspaceMoveParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &params.workspace_id);
        }
        if params.insert_index > self.state.workspaces.len() {
            return encode_error(
                id,
                "workspace_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let workspace_id = self.public_workspace_id(index);
        let insert_index = params.insert_index;
        let moved = self.state.move_workspace(index, insert_index);
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceMoved,
                data: EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index,
                    workspaces: workspaces.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::WorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_become_parent(
        &mut self,
        id: String,
        params: WorkspaceParentSpaceParams,
    ) -> String {
        self.handle_workspace_parent_space_action(id, params, ParentSpaceAction::Become)
    }

    pub(super) fn handle_workspace_rescan_children(
        &mut self,
        id: String,
        params: WorkspaceParentSpaceParams,
    ) -> String {
        self.handle_workspace_parent_space_action(id, params, ParentSpaceAction::Rescan)
    }

    pub(super) fn handle_workspace_stop_parent(
        &mut self,
        id: String,
        params: WorkspaceParentSpaceParams,
    ) -> String {
        self.handle_workspace_parent_space_action(id, params, ParentSpaceAction::Stop)
    }

    fn handle_workspace_parent_space_action(
        &mut self,
        id: String,
        params: WorkspaceParentSpaceParams,
        action: ParentSpaceAction,
    ) -> String {
        let index = if let Some(workspace_id) = params.workspace_id {
            let Some(index) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            if self.state.workspaces.get(index).is_none() {
                return workspace_not_found(id, &workspace_id);
            }
            index
        } else {
            let Some(index) = self.workspace_creation_source() else {
                return encode_error(id, "workspace_not_found", "no workspace is focused");
            };
            index
        };

        let outcome = match self.apply_parent_space_action(index, action) {
            Ok(outcome) => outcome,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        encode_success(
            id,
            ResponseResult::WorkspaceParentSpace {
                parent_workspace_id: outcome.parent_workspace_id,
                child_workspace_ids: outcome.child_workspace_ids,
                cleared_count: outcome.cleared_count,
            },
        )
    }

    pub(super) fn handle_workspace_move_block(
        &mut self,
        id: String,
        params: WorkspaceMoveBlockParams,
    ) -> String {
        if params.workspace_ids.is_empty() {
            return encode_error(
                id,
                "workspace_move_block_failed",
                "workspace_ids must not be empty",
            );
        }

        let mut workspace_ids = Vec::with_capacity(params.workspace_ids.len());
        let mut seen_ids = std::collections::HashSet::new();
        for requested_id in &params.workspace_ids {
            let Some(index) = self.parse_workspace_id(requested_id) else {
                return workspace_not_found(id, requested_id);
            };
            let Some(workspace) = self.state.workspaces.get(index) else {
                return workspace_not_found(id, requested_id);
            };
            if !seen_ids.insert(workspace.id.clone()) {
                return encode_error(
                    id,
                    "workspace_move_block_failed",
                    format!("workspace {requested_id} appears more than once"),
                );
            }
            workspace_ids.push(workspace.id.clone());
        }

        let before_workspace_id = match params.before_workspace_id {
            Some(requested_id) => {
                let Some(index) = self.parse_workspace_id(&requested_id) else {
                    return workspace_not_found(id, &requested_id);
                };
                let Some(workspace) = self.state.workspaces.get(index) else {
                    return workspace_not_found(id, &requested_id);
                };
                if seen_ids.contains(&workspace.id) {
                    return encode_error(
                        id,
                        "workspace_move_block_failed",
                        "before_workspace_id must not be part of workspace_ids",
                    );
                }
                Some(workspace.id.clone())
            }
            None => None,
        };

        let moved = self
            .state
            .move_workspace_block(&workspace_ids, before_workspace_id.as_deref());
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceReordered,
                data: EventData::WorkspaceReordered {
                    workspace_ids,
                    before_workspace_id,
                    workspaces: workspaces.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::WorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_report_metadata(
        &mut self,
        id: String,
        params: WorkspaceReportMetadataParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let source = match normalize_metadata_source(params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_metadata_source", message),
        };
        let ttl = match normalize_metadata_ttl(params.ttl_ms) {
            Ok(ttl) => ttl,
            Err(message) => return encode_error(id, "invalid_metadata_ttl", message),
        };
        let tokens = match super::super::api_helpers::normalize_metadata_tokens(params.tokens) {
            Ok(tokens) => tokens,
            Err(message) => return encode_error(id, "invalid_metadata_token", message),
        };
        let Some(workspace) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if !crate::metadata_tokens::sequence_is_fresh(
            &workspace.metadata_token_sequences,
            &source,
            params.seq,
        ) {
            return encode_success(id, ResponseResult::Ok {});
        }
        if workspace.metadata_tokens.key_count_after_patch(&tokens)
            > super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
        {
            return encode_error(
                id,
                "metadata_token_limit",
                format!(
                    "workspace metadata may contain at most {} tokens",
                    super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
                ),
            );
        }
        match crate::metadata_tokens::accept_sequence(
            &mut workspace.metadata_token_sequences,
            &source,
            params.seq,
        ) {
            Ok(true) => {}
            Ok(false) => return encode_success(id, ResponseResult::Ok {}),
            Err(()) => {
                return encode_error(
                    id,
                    "metadata_sequence_source_limit",
                    format!(
                        "workspace metadata may track at most {} sequenced sources",
                        crate::metadata_tokens::MAX_SEQUENCE_SOURCES
                    ),
                );
            }
        }
        let changed = workspace
            .metadata_tokens
            .patch(tokens, ttl, std::time::Instant::now());
        if changed {
            self.sync_agent_metadata_deadline();
            self.emit_workspace_token_updated(index);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_workspace_close(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &target.workspace_id);
        }
        let workspace_id = self.public_workspace_id(index);
        let workspace = self.workspace_info(index);
        let pane_ids = self
            .state
            .workspaces
            .get(index)
            .map(|ws| {
                ws.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.state.selected = index;
        self.state.close_selected_workspace();
        self.state.remove_plugin_pane_records(pane_ids);
        self.shutdown_detached_terminal_runtimes();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id,
                workspace: Some(workspace),
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }

    fn workspace_list_info(&self) -> Vec<crate::api::schema::WorkspaceInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .map(|(idx, _)| self.workspace_info(idx))
            .collect()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

fn resolve_workspace_retarget_path(raw_path: &str) -> Result<PathBuf, (&'static str, String)> {
    let path = resolve_workspace_retarget_directory(raw_path)?;
    let is_non_bare_checkout =
        crate::workspace::git_worktree_is_bare(&path).is_some_and(|is_bare| !is_bare);
    if !is_non_bare_checkout {
        return Err((
            "workspace_retarget_path_not_checkout",
            format!(
                "workspace retarget path is not a usable Git checkout: {}",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn resolve_workspace_retarget_directory(raw_path: &str) -> Result<PathBuf, (&'static str, String)> {
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return Err((
            "workspace_retarget_path_not_absolute",
            "workspace retarget path must be absolute".into(),
        ));
    }
    if !path.exists() {
        return Err((
            "workspace_retarget_path_not_found",
            format!("workspace retarget path does not exist: {}", path.display()),
        ));
    }
    if !path.is_dir() {
        return Err((
            "workspace_retarget_path_not_checkout",
            format!(
                "workspace retarget path is not a directory: {}",
                path.display()
            ),
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::{
        api::{
            schema::{ErrorResponse, Method, Request, SuccessResponse},
            EventHub,
        },
        config::{Config, MachineConfig},
        workspace::{ParentSpaceMembership, Workspace},
    };
    use ratatui::layout::Direction;

    static NEXT_PARENT_SPACE_FIXTURE: AtomicU64 = AtomicU64::new(1);

    struct ParentSpaceApiFixture {
        root: PathBuf,
    }

    impl ParentSpaceApiFixture {
        fn new() -> Self {
            let suffix = NEXT_PARENT_SPACE_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "herdr-parent-space-api-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for ParentSpaceApiFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn parent_space_api_app(workspaces: Vec<Workspace>) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = super::super::test_support::exiting_test_command().into();
        app.state.workspaces = workspaces;
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        app
    }

    #[cfg(unix)]
    fn restart_parent_space_api_app(app: &App) -> App {
        let snapshot = crate::persist::capture(
            &app.state.workspaces,
            &app.state.terminals,
            &app.terminal_runtimes,
            app.state.active,
            app.state.selected,
            app.state.sidebar_width,
            app.state.sidebar_section_split,
            app.state.collapsed_space_keys.clone(),
        );
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut restarted = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let (workspaces, terminals, terminal_runtimes) = crate::persist::restore(
            &snapshot,
            None,
            24,
            80,
            0,
            "/bin/sh",
            crate::config::ShellModeConfig::NonLogin,
            crate::persist::RestorePolicy::new(false, &[]),
            restarted.event_tx.clone(),
            restarted.render_notify.clone(),
            restarted.render_dirty.clone(),
        );
        restarted.state.workspaces = workspaces;
        restarted.state.terminals = terminals;
        restarted.terminal_runtimes = terminal_runtimes.into();
        restarted.state.active = snapshot.active;
        restarted.state.selected = snapshot.selected;
        restarted.state.sidebar_width = snapshot.sidebar_width.unwrap_or(32);
        restarted.state.sidebar_section_split = snapshot.sidebar_section_split.unwrap_or(0.5);
        restarted.state.collapsed_space_keys = snapshot.collapsed_space_keys;
        restarted.state.default_shell = "/bin/sh".into();
        restarted
    }

    #[test]
    fn workspace_reads_refresh_staleness_after_checkout_removal_and_retarget() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "old");
        let new_path = test_git_checkout(&fixture.root, "new");
        let (mut app, _event_hub, workspace_id) = retarget_test_app(&old_path);

        std::fs::remove_dir_all(&old_path).unwrap();

        let get_response = app.handle_api_request(Request {
            id: "get-stale".into(),
            method: Method::WorkspaceGet(WorkspaceTarget {
                workspace_id: workspace_id.clone(),
            }),
        });
        let get_success: SuccessResponse = serde_json::from_str(&get_response).unwrap();
        let ResponseResult::WorkspaceInfo {
            workspace: get_workspace,
        } = get_success.result
        else {
            panic!("expected workspace.get response");
        };
        assert!(get_workspace.stale);
        assert_eq!(
            get_workspace.stale_path.as_deref(),
            Some(old_path.to_str().unwrap())
        );

        let list_response = app.handle_api_request(Request {
            id: "list-stale".into(),
            method: Method::WorkspaceList(crate::api::schema::EmptyParams::default()),
        });
        let list_success: SuccessResponse = serde_json::from_str(&list_response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = list_success.result else {
            panic!("expected workspace.list response");
        };
        assert_eq!(workspaces.len(), 1);
        assert!(workspaces[0].stale);
        assert_eq!(
            workspaces[0].stale_path.as_deref(),
            Some(old_path.to_str().unwrap())
        );

        let retarget_response = app.handle_api_request(Request {
            id: "retarget-valid".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id: workspace_id.clone(),
                path: new_path.display().to_string(),
            }),
        });
        let retarget_success: SuccessResponse = serde_json::from_str(&retarget_response).unwrap();
        let ResponseResult::WorkspaceInfo {
            workspace: retarget_workspace,
        } = retarget_success.result
        else {
            panic!("expected workspace.retarget response");
        };
        assert!(!retarget_workspace.stale);
        assert!(retarget_workspace.stale_path.is_none());

        let get_response = app.handle_api_request(Request {
            id: "get-current".into(),
            method: Method::WorkspaceGet(WorkspaceTarget { workspace_id }),
        });
        let get_success: SuccessResponse = serde_json::from_str(&get_response).unwrap();
        let ResponseResult::WorkspaceInfo { workspace } = get_success.result else {
            panic!("expected workspace.get response");
        };
        assert!(!workspace.stale);
        assert!(workspace.stale_path.is_none());

        let list_response = app.handle_api_request(Request {
            id: "list-current".into(),
            method: Method::WorkspaceList(crate::api::schema::EmptyParams::default()),
        });
        let list_success: SuccessResponse = serde_json::from_str(&list_response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = list_success.result else {
            panic!("expected workspace.list response");
        };
        assert_eq!(workspaces.len(), 1);
        assert!(!workspaces[0].stale);
        assert!(workspaces[0].stale_path.is_none());
        assert!(app.state.session_dirty);
    }

    fn test_git_checkout(root: &Path, name: &str) -> PathBuf {
        let checkout = root.join(name);
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&checkout)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        checkout
    }

    fn test_bare_git_repository(root: &Path, name: &str) -> PathBuf {
        let repository = root.join(name);
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&repository)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        repository
    }

    fn test_linked_git_worktree(root: &Path, name: &str) -> PathBuf {
        let source = test_git_checkout(root, &format!("{name}-source"));
        commit_test_git_checkout(&source);

        let checkout = root.join(name);
        add_test_linked_git_worktree(&source, &checkout, &format!("{name}-branch"));
        checkout
    }

    fn commit_test_git_checkout(source: &Path) {
        for args in [
            ["config", "user.email", "herdr@example.invalid"],
            ["config", "user.name", "Herdr Test"],
        ] {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(source)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git config failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["commit", "--quiet", "--allow-empty", "-m", "initial"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn add_test_linked_git_worktree(source: &Path, checkout: &Path, branch: &str) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["worktree", "add", "--quiet", "-b"])
            .arg(branch)
            .arg(checkout)
            .arg("HEAD")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(checkout.join(".git").is_file());
    }

    fn grouped_worktree_workspace(
        name: &str,
        checkout: &Path,
        key: &str,
        repo_root: &Path,
        is_linked_worktree: bool,
    ) -> Workspace {
        let mut workspace = Workspace::test_new(name);
        workspace.identity_cwd = checkout.to_path_buf();
        workspace.cached_identity_cwd = checkout.to_path_buf();
        workspace.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: key.to_string(),
            label: "linked-source".into(),
            repo_root: repo_root.to_path_buf(),
            checkout_path: checkout.to_path_buf(),
            is_linked_worktree,
        });
        workspace
    }

    fn retarget_workspace_response(app: &mut App, workspace_id: &str, path: &Path) -> String {
        app.handle_api_request(Request {
            id: format!("retarget-{workspace_id}"),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id: workspace_id.to_string(),
                path: path.display().to_string(),
            }),
        })
    }

    fn git_worktree_registry(repo_root: &Path) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn retarget_test_app(old_path: &Path) -> (App, EventHub, String) {
        let mut workspace = Workspace::test_new("one");
        workspace.identity_cwd = old_path.to_path_buf();
        workspace.cached_identity_cwd = old_path.to_path_buf();
        let workspace_id = workspace.id.clone();
        let event_hub = EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.default_shell = super::super::test_support::exiting_test_command().into();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        (app, event_hub, workspace_id)
    }

    #[cfg(unix)]
    fn install_live_shell_runtime(
        app: &mut App,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        cwd: &Path,
    ) -> (crate::terminal::TerminalId, u32) {
        let terminal_id = app.state.workspaces[ws_idx]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        let launch_env = app
            .pane_launch_env(ws_idx, pane_id, Vec::new())
            .expect("test pane should have launch identity");
        let runtime = crate::terminal::TerminalRuntime::spawn(
            pane_id,
            24,
            80,
            cwd.to_path_buf(),
            app.state.pane_scrollback_limit_bytes,
            app.state.host_terminal_theme,
            app.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&app.state.default_shell, app.state.shell_mode),
            &launch_env,
            app.event_tx.clone(),
            app.render_notify.clone(),
            app.render_dirty.clone(),
        )
        .expect("test shell should spawn");
        let child_pid = runtime.child_pid().expect("test shell should have a pid");
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        (terminal_id, child_pid)
    }

    fn assert_retarget_rejection(app: &App, old_path: &Path, response: &str, expected_code: &str) {
        let error: ErrorResponse = serde_json::from_str(response).unwrap();
        assert_eq!(error.error.code, expected_code);
        assert_eq!(app.state.workspaces[0].identity_cwd, old_path);
        assert_eq!(app.state.workspaces[0].cached_identity_cwd, old_path);
        assert!(app
            .state
            .terminal_ids_for_workspace(0)
            .iter()
            .all(|terminal_id| app.state.terminals[terminal_id].cwd == old_path));
        assert!(!app.state.session_dirty);
    }

    fn machine_config(target: &str) -> Config {
        Config {
            machines: vec![MachineConfig {
                name: "build".into(),
                target: target.into(),
                cwd: None,
            }],
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn workspace_create_machine_spawns_ssh_root_and_uses_local_identity() {
        let config = machine_config("-V");
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let response = app.handle_workspace_create(
            "machine".into(),
            WorkspaceCreateParams {
                cwd: None,
                machine: Some("build".into()),
                focus: true,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceCreated { workspace, .. } = success.result else {
            panic!("expected workspace create response");
        };
        assert_eq!(workspace.machine.as_deref(), Some("build"));
        let created = &app.state.workspaces[0];
        assert_eq!(created.machine_name(), Some("build"));
        assert_eq!(created.custom_name.as_deref(), Some("build"));
        assert!(created.identity_cwd.is_dir());
        let terminal_id = created.terminal_id(created.tabs[0].root_pane).unwrap();
        assert_eq!(
            app.state.terminals[terminal_id]
                .launch_argv
                .as_ref()
                .unwrap(),
            &["ssh".to_string(), "-t".to_string(), "-V".to_string()]
        );
        app.state.assert_invariants_for_test();

        super::super::test_support::shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn workspace_create_rejects_machine_with_cwd() {
        let mut app = parent_space_api_app(Vec::new());
        let response = app.handle_workspace_create(
            "machine".into(),
            WorkspaceCreateParams {
                cwd: Some("/tmp".into()),
                machine: Some("build".into()),
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "workspace_create_invalid");
    }

    #[test]
    fn machine_workspace_parent_space_action_is_rejected() {
        let mut workspace = Workspace::test_new("build");
        workspace.machine = Some("build".into());
        let workspace_id = workspace.id.clone();
        let mut app = parent_space_api_app(vec![workspace]);

        let response = app.handle_api_request(Request {
            id: "machine-parent".into(),
            method: Method::WorkspaceBecomeParent(WorkspaceParentSpaceParams {
                workspace_id: Some(workspace_id.clone()),
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "machine_workspace_parent_space");
        assert_eq!(
            error.error.message,
            "machine workspaces cannot participate in parent spaces"
        );
        assert!(app.state.workspaces[0].parent_space().is_none());
    }

    #[test]
    fn machine_workspace_close_removes_its_terminal_state() {
        let mut workspace = Workspace::test_new("build");
        workspace.machine = Some("build".into());
        let workspace_id = workspace.id.clone();
        let terminal_id = workspace
            .terminal_id(workspace.tabs[0].root_pane)
            .cloned()
            .expect("machine root terminal");
        let mut app = parent_space_api_app(vec![workspace]);
        assert!(app.state.terminals.contains_key(&terminal_id));

        let response =
            app.handle_workspace_close("machine-close".into(), WorkspaceTarget { workspace_id });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.result, ResponseResult::Ok {});
        assert!(app.state.workspaces.is_empty());
        assert!(!app.state.terminals.contains_key(&terminal_id));
        assert!(app.state.session_dirty);
    }

    #[test]
    fn workspace_retarget_updates_workspace_and_all_panes() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "old");
        let new_path = test_git_checkout(&fixture.root, "new");
        let (mut app, event_hub, workspace_id) = retarget_test_app(&old_path);
        app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();
        let new_path_display = new_path.display().to_string();

        let response = app.handle_api_request(Request {
            id: "retarget".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: new_path_display.clone(),
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        assert_eq!(app.state.workspaces[0].identity_cwd, new_path);
        assert_eq!(app.state.workspaces[0].cached_identity_cwd, new_path);
        assert!(!app.state.workspaces[0].is_stale());
        assert!(app
            .state
            .terminal_ids_for_workspace(0)
            .iter()
            .all(|terminal_id| app.state.terminals[terminal_id].cwd == new_path));
        for pane_id in app.state.pane_ids_for_workspace(0) {
            let pane = app.pane_info(0, pane_id).expect("retargeted pane");
            assert_eq!(pane.cwd.as_deref(), Some(new_path_display.as_str()));
        }
        assert!(app.state.session_dirty);
        assert!(event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| matches!(&event.data, EventData::WorkspaceUpdated { .. })));
        assert_eq!(
            event_hub
                .events_after(0)
                .iter()
                .filter(|(_, event)| matches!(&event.data, EventData::PaneUpdated { .. }))
                .count(),
            2
        );
        assert!(app.intentional_pane_restarts.is_empty());
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_preserves_adversarial_identity_without_live_runtimes() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "identity-old");
        let new_path = test_git_checkout(&fixture.root, "identity-new");
        let mut state = crate::app::AppState::test_with_adversarial_identity_state();
        state.workspaces[0].identity_cwd = old_path.clone();
        state.workspaces[0].cached_identity_cwd = old_path.clone();
        for terminal_id in state.terminal_ids_for_workspace(0) {
            state.terminals.get_mut(&terminal_id).unwrap().cwd = old_path.clone();
        }
        let workspace_id = state.workspaces[0].id.clone();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = state;
        let identity_before: Vec<_> = app
            .state
            .pane_ids_for_workspace(0)
            .into_iter()
            .map(|pane_id| {
                (
                    pane_id,
                    app.public_pane_id(0, pane_id).unwrap(),
                    app.state.workspaces[0]
                        .terminal_id(pane_id)
                        .unwrap()
                        .clone(),
                )
            })
            .collect();
        let layout_before: Vec<_> = app.state.workspaces[0]
            .tabs
            .iter()
            .map(|tab| (tab.root_pane, tab.layout.focused()))
            .collect();

        let response = app.handle_api_request(Request {
            id: "identity-retarget".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: new_path.display().to_string(),
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        let identity_after: Vec<_> = app
            .state
            .pane_ids_for_workspace(0)
            .into_iter()
            .map(|pane_id| {
                (
                    pane_id,
                    app.public_pane_id(0, pane_id).unwrap(),
                    app.state.workspaces[0]
                        .terminal_id(pane_id)
                        .unwrap()
                        .clone(),
                )
            })
            .collect();
        let layout_after: Vec<_> = app.state.workspaces[0]
            .tabs
            .iter()
            .map(|tab| (tab.root_pane, tab.layout.focused()))
            .collect();
        assert_eq!(identity_after, identity_before);
        assert_eq!(layout_after, layout_before);
        assert!(app.intentional_pane_restarts.is_empty());
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_retarget_restarts_live_child_in_new_checkout() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "live-old");
        let new_path = test_git_checkout(&fixture.root, "live-new");
        let other_path = test_git_checkout(&fixture.root, "live-other");
        let (mut app, event_hub, workspace_id) = retarget_test_app(&old_path);
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let split_pane_id = app.state.workspaces[0].test_split(Direction::Horizontal);
        let mut other_workspace = Workspace::test_new("other");
        other_workspace.identity_cwd = other_path.clone();
        other_workspace.cached_identity_cwd = other_path.clone();
        let other_pane_id = other_workspace.tabs[0].root_pane;
        app.state.workspaces.push(other_workspace);
        app.state.ensure_test_terminals();
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let public_split_pane_id = app.public_pane_id(0, split_pane_id).unwrap();
        let focused_pane_id = app.state.workspaces[0].tabs[0].layout.focused();
        let (terminal_id, old_pid) = install_live_shell_runtime(&mut app, 0, pane_id, &old_path);
        let (split_terminal_id, old_split_pid) =
            install_live_shell_runtime(&mut app, 0, split_pane_id, &old_path);
        let (other_terminal_id, other_pid) =
            install_live_shell_runtime(&mut app, 1, other_pane_id, &other_path);
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.manual_label = Some("manual label".into());
        terminal.launch_argv = Some(vec!["agent".into(), "--resume".into()]);
        terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Working,
        );
        let session_ref = crate::agent_resume::AgentSessionRef::path(
            fixture.root.join("pi-session.jsonl").display().to_string(),
        )
        .unwrap();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: session_ref.clone(),
        });
        terminal
            .set_hook_authority_with_session_ref(
                "herdr:pi".into(),
                "pi".into(),
                crate::detect::AgentState::Working,
                None,
                Some(session_ref.clone()),
                Some(1),
            )
            .expect("full-lifecycle hook authority should be accepted");
        assert!(terminal.full_lifecycle_hook_authority_active());
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref,
        });

        let response = app.handle_api_request(Request {
            id: "live-retarget".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: new_path.display().to_string(),
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        let restart_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !app.intentional_pane_restarts.is_empty()
            && std::time::Instant::now() < restart_deadline
        {
            app.drain_internal_events();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("retargeted runtime should stay attached");
        let new_pid = runtime
            .child_pid()
            .expect("retargeted shell should have a pid");
        assert_ne!(new_pid, old_pid, "retarget should replace the live child");
        let new_split_pid = app
            .terminal_runtimes
            .get(&split_terminal_id)
            .expect("retargeted split runtime")
            .child_pid()
            .expect("retargeted split should have a pid");
        assert_ne!(new_split_pid, old_split_pid);
        assert_eq!(
            app.terminal_runtimes
                .get(&other_terminal_id)
                .and_then(|runtime| runtime.child_pid()),
            Some(other_pid)
        );

        let expected_cwd = std::fs::canonicalize(&new_path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while crate::platform::process_cwd(new_pid).and_then(|cwd| std::fs::canonicalize(cwd).ok())
            != Some(expected_cwd.clone())
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            crate::platform::process_cwd(new_pid).and_then(|cwd| std::fs::canonicalize(cwd).ok()),
            Some(expected_cwd)
        );
        assert_eq!(
            crate::platform::process_cwd(new_split_pid)
                .and_then(|cwd| std::fs::canonicalize(cwd).ok()),
            Some(std::fs::canonicalize(&new_path).unwrap())
        );
        assert_eq!(
            crate::platform::process_cwd(other_pid).and_then(|cwd| std::fs::canonicalize(cwd).ok()),
            Some(std::fs::canonicalize(&other_path).unwrap())
        );
        assert_eq!(
            app.public_pane_id(0, pane_id).as_deref(),
            Some(public_pane_id.as_str())
        );
        assert_eq!(
            app.public_pane_id(0, split_pane_id).as_deref(),
            Some(public_split_pane_id.as_str())
        );
        assert_eq!(app.state.workspaces[0].tabs[0].root_pane, pane_id);
        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.focused(),
            focused_pane_id
        );
        assert_eq!(
            app.state.terminals[&terminal_id].manual_label.as_deref(),
            Some("manual label")
        );
        assert!(app.state.terminals[&terminal_id].launch_argv.is_none());
        assert!(app.state.terminals[&terminal_id]
            .persisted_agent_session
            .is_none());
        assert!(!app.state.terminals[&terminal_id].full_lifecycle_hook_authority_active());
        app.state.assert_invariants_for_test();
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::PaneAgentDetected {
                pane_id: released_pane_id,
                released: true,
                ..
            } if released_pane_id == &public_pane_id
        )));
        assert!(!event_hub.events_after(0).iter().any(|(_, event)| matches!(
            event.event,
            EventKind::PaneCreated | EventKind::PaneClosed | EventKind::PaneExited
        )));

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        app.drain_all_internal_events();
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .and_then(|runtime| runtime.child_pid()),
            Some(new_pid),
            "a delayed event from the killed child must not close the replacement"
        );
        assert!(app.find_pane(pane_id).is_some());

        super::super::test_support::shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn workspace_retarget_restart_preparation_failure_is_atomic() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "pending-old");
        let new_path = test_git_checkout(&fixture.root, "pending-new");
        let (mut app, _event_hub, workspace_id) = retarget_test_app(&old_path);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .unwrap()
            .clone();
        app.intentional_pane_restarts.insert(
            pane_id,
            IntentionalPaneRestart {
                terminal_id,
                rows: 24,
                cols: 80,
            },
        );

        let response = app.handle_api_request(Request {
            id: "pending-retarget".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: new_path.display().to_string(),
            }),
        });

        assert_retarget_rejection(
            &app,
            &old_path,
            &response,
            "workspace_retarget_restart_pending",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_retarget_spawn_failure_keeps_inert_pane() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "failure-old");
        let new_path = test_git_checkout(&fixture.root, "failure-new");
        let (mut app, event_hub, workspace_id) = retarget_test_app(&old_path);
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let (terminal_id, _pid) = install_live_shell_runtime(&mut app, 0, pane_id, &old_path);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .manual_label = Some("keep me".into());
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .launch_argv = Some(vec!["agent".into()]);
        app.state.default_shell = fixture.root.join("missing-shell").display().to_string();

        let response = app.handle_api_request(Request {
            id: "failed-spawn-retarget".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: new_path.display().to_string(),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !app.intentional_pane_restarts.is_empty() && std::time::Instant::now() < deadline {
            app.drain_internal_events();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(app.intentional_pane_restarts.is_empty());
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        assert_eq!(app.state.workspaces[0].tabs[0].root_pane, pane_id);
        assert_eq!(app.state.terminals[&terminal_id].cwd, new_path);
        assert_eq!(
            app.state.terminals[&terminal_id].manual_label.as_deref(),
            Some("keep me")
        );
        assert!(app.state.terminals[&terminal_id].launch_argv.is_none());
        app.state.assert_invariants_for_test();
        assert!(!event_hub.events_after(0).iter().any(|(_, event)| matches!(
            event.event,
            EventKind::PaneCreated | EventKind::PaneClosed | EventKind::PaneExited
        )));
    }

    #[test]
    fn workspace_retarget_rejects_nonexistent_path() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "old");
        let (mut app, _event_hub, workspace_id) = retarget_test_app(&old_path);
        let missing_path = fixture.root.join("missing");

        let response = app.handle_api_request(Request {
            id: "missing".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: missing_path.display().to_string(),
            }),
        });

        assert_retarget_rejection(
            &app,
            &old_path,
            &response,
            "workspace_retarget_path_not_found",
        );
    }

    #[test]
    fn workspace_retarget_rejects_non_checkout_path() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "old");
        let non_checkout = fixture.root.join("not-checkout");
        std::fs::create_dir_all(&non_checkout).unwrap();
        let (mut app, _event_hub, workspace_id) = retarget_test_app(&old_path);

        let response = app.handle_api_request(Request {
            id: "non-checkout".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: non_checkout.display().to_string(),
            }),
        });

        assert_retarget_rejection(
            &app,
            &old_path,
            &response,
            "workspace_retarget_path_not_checkout",
        );
    }

    #[test]
    fn workspace_retarget_rejects_bare_repository() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "old");
        let bare_repository = test_bare_git_repository(&fixture.root, "bare.git");
        let (mut app, _event_hub, workspace_id) = retarget_test_app(&old_path);

        let response = app.handle_api_request(Request {
            id: "bare-repository".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: bare_repository.display().to_string(),
            }),
        });

        assert_retarget_rejection(
            &app,
            &old_path,
            &response,
            "workspace_retarget_path_not_checkout",
        );
    }

    #[test]
    fn workspace_retarget_accepts_linked_worktree() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "old");
        let linked_worktree = test_linked_git_worktree(&fixture.root, "linked");
        let (mut app, _event_hub, workspace_id) = retarget_test_app(&old_path);

        let response = app.handle_api_request(Request {
            id: "linked-worktree".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: linked_worktree.display().to_string(),
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        assert_eq!(app.state.workspaces[0].identity_cwd, linked_worktree);
        assert!(app
            .state
            .terminal_ids_for_workspace(0)
            .iter()
            .all(|terminal_id| app.state.terminals[terminal_id].cwd == linked_worktree));
    }

    #[test]
    fn workspace_retarget_repairs_linked_worktree_after_parent_move() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_checkout = test_linked_git_worktree(&old_container, "linked");
        let old_repo_root = old_container.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();

        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = old_repo_root.clone();
        parent.cached_identity_cwd = old_repo_root.clone();
        let parent_id = parent.id.clone();
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: old_key.clone(),
            label: "linked-source".into(),
            repo_root: old_repo_root.clone(),
            checkout_path: old_repo_root.clone(),
            is_linked_worktree: false,
        });

        let mut child = Workspace::test_new("child");
        child.identity_cwd = old_checkout.clone();
        child.cached_identity_cwd = old_checkout.clone();
        let child_id = child.id.clone();
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: old_key.clone(),
            label: "linked-source".into(),
            repo_root: old_repo_root.clone(),
            checkout_path: old_checkout.clone(),
            is_linked_worktree: true,
        });

        let stale_sibling_path = old_container.join("sibling");
        add_test_linked_git_worktree(&old_repo_root, &stale_sibling_path, "sibling-branch");
        let registry_before_move = std::process::Command::new("git")
            .arg("-C")
            .arg(&old_repo_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(registry_before_move.status.success());
        let registry_before_move = String::from_utf8(registry_before_move.stdout).unwrap();
        let sibling_record_before = registry_before_move
            .split("\n\n")
            .find(|record| record.contains("branch refs/heads/sibling-branch"))
            .unwrap();
        let sibling_registry_path = sibling_record_before
            .lines()
            .find(|line| line.starts_with("worktree "))
            .unwrap()
            .to_owned();
        let mut sibling = Workspace::test_new("sibling");
        sibling.identity_cwd = stale_sibling_path.clone();
        sibling.cached_identity_cwd = stale_sibling_path.clone();
        sibling.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: old_key.clone(),
            label: "linked-source".into(),
            repo_root: old_repo_root.clone(),
            checkout_path: stale_sibling_path.clone(),
            is_linked_worktree: true,
        });

        let mut app = parent_space_api_app(vec![parent, child, sibling]);
        app.state.collapsed_space_keys.insert(old_key.clone());

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("linked-source");
        let new_checkout = new_container.join("linked");

        let parent_response = app.handle_api_request(Request {
            id: "moved-parent".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id: parent_id,
                path: new_repo_root.display().to_string(),
            }),
        });
        let parent_success: SuccessResponse = serde_json::from_str(&parent_response).unwrap();
        assert!(matches!(
            parent_success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        let parent_membership = app.state.workspaces[0].worktree_space().unwrap();
        assert_eq!(parent_membership.key, old_key);
        assert_eq!(parent_membership.repo_root, new_repo_root);
        assert_eq!(parent_membership.checkout_path, new_repo_root);

        let child_response = app.handle_api_request(Request {
            id: "moved-child".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id: child_id,
                path: new_checkout.display().to_string(),
            }),
        });
        let child_success: SuccessResponse = serde_json::from_str(&child_response).unwrap();
        assert!(matches!(
            child_success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&new_checkout)
            .args(["status", "--porcelain=v1"])
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git status failed after repair: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        let child_git_file = new_checkout.join(".git");
        let child_pointer = std::fs::read_to_string(&child_git_file).unwrap();
        let child_admin = child_pointer
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .map(PathBuf::from)
            .unwrap();
        let child_admin = std::fs::canonicalize(child_admin).unwrap();
        let expected_admin =
            std::fs::canonicalize(new_repo_root.join(".git").join("worktrees").join("linked"))
                .unwrap();
        assert_eq!(child_admin, expected_admin);
        let admin_pointer = std::fs::read_to_string(expected_admin.join("gitdir")).unwrap();
        assert_eq!(
            std::fs::canonicalize(admin_pointer.trim()).unwrap(),
            std::fs::canonicalize(&child_git_file).unwrap()
        );

        let registry = std::process::Command::new("git")
            .arg("-C")
            .arg(&new_repo_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(registry.status.success());
        let registry = String::from_utf8(registry.stdout).unwrap();
        assert!(registry.contains(&format!(
            "worktree {}",
            std::fs::canonicalize(&new_repo_root).unwrap().display()
        )));
        assert!(registry.contains(&format!(
            "worktree {}",
            std::fs::canonicalize(&new_checkout).unwrap().display()
        )));
        let sibling_record_after = registry
            .split("\n\n")
            .find(|record| record.contains("branch refs/heads/sibling-branch"))
            .unwrap_or_else(|| panic!("unrelated sibling missing from registry:\n{registry}"));
        assert!(sibling_record_after
            .lines()
            .any(|line| line == sibling_registry_path));
        assert_eq!(
            registry
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            3
        );

        let new_key = std::fs::canonicalize(new_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let canonical_new_repo_root = std::fs::canonicalize(&new_repo_root).unwrap();
        for workspace in &app.state.workspaces {
            let membership = workspace.worktree_space().unwrap();
            assert_eq!(membership.key, new_key);
            assert_eq!(membership.repo_root, canonical_new_repo_root);
        }
        assert_eq!(
            app.state.workspaces[1]
                .worktree_space()
                .unwrap()
                .checkout_path,
            new_checkout
        );
        assert_eq!(
            app.state.workspaces[2]
                .worktree_space()
                .unwrap()
                .checkout_path,
            stale_sibling_path
        );
        assert!(!app.state.collapsed_space_keys.contains(&old_key));
        assert!(app.state.collapsed_space_keys.contains(&new_key));
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_repairs_second_sibling_after_group_rekey() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_first = test_linked_git_worktree(&old_container, "first");
        let old_repo_root = old_container.join("first-source");
        let old_second = old_container.join("second");
        add_test_linked_git_worktree(&old_repo_root, &old_second, "second-branch");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();

        let parent =
            grouped_worktree_workspace("parent", &old_repo_root, &old_key, &old_repo_root, false);
        let parent_id = parent.id.clone();
        let first = grouped_worktree_workspace("first", &old_first, &old_key, &old_repo_root, true);
        let first_id = first.id.clone();
        let second =
            grouped_worktree_workspace("second", &old_second, &old_key, &old_repo_root, true);
        let second_id = second.id.clone();
        let mut app = parent_space_api_app(vec![parent, first, second]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("first-source");
        let new_first = new_container.join("first");
        let new_second = new_container.join("second");

        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_repo_root,
        ))
        .unwrap();
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app, &first_id, &new_first,
        ))
        .unwrap();

        let second_before = app.state.workspaces[2].worktree_space().unwrap();
        assert_eq!(
            second_before.repo_root,
            std::fs::canonicalize(&new_repo_root).unwrap()
        );
        assert_eq!(second_before.checkout_path, old_second);

        let response = retarget_workspace_response(&mut app, &second_id, &new_second);
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        assert_eq!(
            app.state.workspaces[2]
                .worktree_space()
                .unwrap()
                .checkout_path,
            new_second
        );
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&new_second)
            .args(["status", "--porcelain=v1"])
            .output()
            .unwrap();
        assert!(status.status.success());
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_retarget_repairs_linked_worktree_after_parent_retarget_and_restart() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_child =
            std::fs::canonicalize(test_linked_git_worktree(&old_container, "child")).unwrap();
        let old_parent = std::fs::canonicalize(old_container.join("child-source")).unwrap();
        let old_key = std::fs::canonicalize(old_parent.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_parent, &old_key, &old_parent, false);
        let parent_id = parent.id.clone();
        let child = grouped_worktree_workspace("child", &old_child, &old_key, &old_parent, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_parent = std::fs::canonicalize(new_container.join("child-source")).unwrap();
        let new_child = std::fs::canonicalize(new_container.join("child")).unwrap();
        let parent_response = retarget_workspace_response(&mut app, &parent_id, &new_parent);
        let _: SuccessResponse = serde_json::from_str(&parent_response).unwrap();

        let mut app = restart_parent_space_api_app(&app);
        assert_eq!(app.state.workspaces.len(), 2);
        let restored_parent = app
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == parent_id)
            .unwrap();
        let restored_child = app
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == child_id)
            .unwrap();
        assert_eq!(restored_parent.worktree_space().unwrap().key, old_key);
        assert_eq!(
            restored_parent.worktree_space().unwrap().repo_root,
            new_parent
        );
        assert_eq!(restored_parent.identity_cwd, new_parent);
        assert_eq!(restored_child.worktree_space().unwrap().key, old_key);
        assert_eq!(
            restored_child.worktree_space().unwrap().checkout_path,
            old_child
        );
        let response = retarget_workspace_response(&mut app, &child_id, &new_child);
        let success: SuccessResponse = serde_json::from_str(&response)
            .unwrap_or_else(|error| panic!("child repair failed: {error}: {response}"));
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        let new_key = std::fs::canonicalize(new_parent.join(".git"))
            .unwrap()
            .display()
            .to_string();
        assert!(app.state.workspaces.iter().all(|workspace| {
            workspace
                .worktree_space()
                .is_some_and(|membership| membership.key == new_key)
        }));
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&new_child)
            .args(["status", "--porcelain=v1"])
            .status()
            .unwrap()
            .success());
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_restore_preserves_cold_moved_group_without_repairing_git() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_child =
            std::fs::canonicalize(test_linked_git_worktree(&old_container, "child")).unwrap();
        let old_parent = std::fs::canonicalize(old_container.join("child-source")).unwrap();
        let old_key = std::fs::canonicalize(old_parent.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_parent, &old_key, &old_parent, false);
        let child = grouped_worktree_workspace("child", &old_child, &old_key, &old_parent, true);
        let app = parent_space_api_app(vec![parent, child]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_parent = std::fs::canonicalize(new_container.join("child-source")).unwrap();
        let new_child = std::fs::canonicalize(new_container.join("child")).unwrap();
        let child_pointer = new_child.join(".git");
        let admin_pointer = new_parent
            .join(".git")
            .join("worktrees")
            .join("child")
            .join("gitdir");
        let child_pointer_before = std::fs::read(&child_pointer).unwrap();
        let admin_pointer_before = std::fs::read(&admin_pointer).unwrap();
        let registry_before = git_worktree_registry(&new_parent);

        let restarted = restart_parent_space_api_app(&app);

        assert_eq!(restarted.state.workspaces.len(), 2);
        assert!(restarted.state.workspaces.iter().all(|workspace| {
            workspace
                .worktree_space()
                .is_some_and(|membership| membership.key == old_key)
        }));
        assert_eq!(std::fs::read(child_pointer).unwrap(), child_pointer_before);
        assert_eq!(std::fs::read(admin_pointer).unwrap(), admin_pointer_before);
        assert_eq!(git_worktree_registry(&new_parent), registry_before);
        restarted.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_retarget_repairs_moved_worktree_and_restarts_live_pane() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_child =
            std::fs::canonicalize(test_linked_git_worktree(&old_container, "child")).unwrap();
        let old_parent = std::fs::canonicalize(old_container.join("child-source")).unwrap();
        let old_key = std::fs::canonicalize(old_parent.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_parent, &old_key, &old_parent, false);
        let parent_id = parent.id.clone();
        let child = grouped_worktree_workspace("child", &old_child, &old_key, &old_parent, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        let event_hub = app.event_hub.clone();
        let pane_id = app.state.workspaces[1].tabs[0].root_pane;
        let public_pane_id = app.public_pane_id(1, pane_id).unwrap();
        let (terminal_id, old_pid) = install_live_shell_runtime(&mut app, 1, pane_id, &old_child);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_parent = std::fs::canonicalize(new_container.join("child-source")).unwrap();
        let new_child = std::fs::canonicalize(new_container.join("child")).unwrap();
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_parent,
        ))
        .unwrap();
        let event_cursor = event_hub.current_sequence();

        let response = retarget_workspace_response(&mut app, &child_id, &new_child);
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !app.intentional_pane_restarts.is_empty() && std::time::Instant::now() < deadline {
            app.drain_internal_events();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let new_pid = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(|runtime| runtime.child_pid())
            .expect("repaired pane should have a replacement shell");
        assert_ne!(new_pid, old_pid);
        let expected_cwd = std::fs::canonicalize(&new_child).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while crate::platform::process_cwd(new_pid).and_then(|cwd| std::fs::canonicalize(cwd).ok())
            != Some(expected_cwd.clone())
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            crate::platform::process_cwd(new_pid).and_then(|cwd| std::fs::canonicalize(cwd).ok()),
            Some(expected_cwd)
        );
        assert_eq!(app.public_pane_id(1, pane_id), Some(public_pane_id));
        assert_eq!(
            app.state.workspaces[1].terminal_id(pane_id),
            Some(&terminal_id)
        );
        let events = event_hub.events_after(event_cursor);
        assert!(events
            .iter()
            .any(|(_, event)| event.event == crate::api::schema::EventKind::WorkspaceUpdated));
        assert!(events
            .iter()
            .any(|(_, event)| event.event == crate::api::schema::EventKind::PaneUpdated));
        assert!(!events.iter().any(|(_, event)| matches!(
            event.event,
            crate::api::schema::EventKind::PaneCreated
                | crate::api::schema::EventKind::PaneClosed
                | crate::api::schema::EventKind::PaneExited
        )));
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_retarget_repairs_second_sibling_after_first_repair_and_restart() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_first =
            std::fs::canonicalize(test_linked_git_worktree(&old_container, "first")).unwrap();
        let old_parent = std::fs::canonicalize(old_container.join("first-source")).unwrap();
        let old_second = old_container.join("second");
        add_test_linked_git_worktree(&old_parent, &old_second, "second-branch");
        let old_second = std::fs::canonicalize(old_second).unwrap();
        let old_key = std::fs::canonicalize(old_parent.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_parent, &old_key, &old_parent, false);
        let parent_id = parent.id.clone();
        let first = grouped_worktree_workspace("first", &old_first, &old_key, &old_parent, true);
        let first_id = first.id.clone();
        let second = grouped_worktree_workspace("second", &old_second, &old_key, &old_parent, true);
        let second_id = second.id.clone();
        let mut app = parent_space_api_app(vec![parent, first, second]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_parent = std::fs::canonicalize(new_container.join("first-source")).unwrap();
        let new_first = std::fs::canonicalize(new_container.join("first")).unwrap();
        let new_second = std::fs::canonicalize(new_container.join("second")).unwrap();
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_parent,
        ))
        .unwrap();
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app, &first_id, &new_first,
        ))
        .unwrap();

        let mut app = restart_parent_space_api_app(&app);
        assert_eq!(app.state.workspaces.len(), 3);
        let response = retarget_workspace_response(&mut app, &second_id, &new_second);
        let success: SuccessResponse = serde_json::from_str(&response)
            .unwrap_or_else(|error| panic!("second repair failed: {error}: {response}"));
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .find(|workspace| workspace.id == second_id)
                .and_then(|workspace| workspace.worktree_space())
                .map(|membership| membership.checkout_path.clone()),
            Some(new_second.clone())
        );
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&new_second)
            .args(["status", "--porcelain=v1"])
            .status()
            .unwrap()
            .success());
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_repairs_real_herdr_worktrees_sibling_layout() {
        let fixture = ParentSpaceApiFixture::new();
        let old_projects = fixture.root.join("old-home").join("GithubProjects");
        std::fs::create_dir_all(old_projects.join("herdr-worktrees")).unwrap();
        let old_repo_root = test_git_checkout(&old_projects, "herdr");
        commit_test_git_checkout(&old_repo_root);
        let old_checkout = old_projects
            .join("herdr-worktrees")
            .join("task-11-4-repair-moved-worktrees");
        add_test_linked_git_worktree(
            &old_repo_root,
            &old_checkout,
            "w/TASK-11.4-repair-moved-worktrees",
        );
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("herdr", &old_repo_root, &old_key, &old_repo_root, false);
        let parent_id = parent.id.clone();
        let child =
            grouped_worktree_workspace("task-11-4", &old_checkout, &old_key, &old_repo_root, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_home = fixture.root.join("new-home");
        std::fs::create_dir_all(&new_home).unwrap();
        let new_projects = new_home.join("GithubProjects");
        std::fs::rename(&old_projects, &new_projects).unwrap();
        let new_repo_root = new_projects.join("herdr");
        let new_checkout = new_projects
            .join("herdr-worktrees")
            .join("task-11-4-repair-moved-worktrees");

        eprintln!("layout=GithubProjects/herdr + GithubProjects/herdr-worktrees/<task>");
        eprintln!(
            "before.child_git={}",
            std::fs::read_to_string(new_checkout.join(".git"))
                .unwrap()
                .trim()
        );
        eprintln!(
            "before.registry=\n{}",
            git_worktree_registry(&new_repo_root)
        );

        let parent_response = retarget_workspace_response(&mut app, &parent_id, &new_repo_root);
        let _: SuccessResponse = serde_json::from_str(&parent_response).unwrap();
        let child_response = retarget_workspace_response(&mut app, &child_id, &new_checkout);
        let _: SuccessResponse = serde_json::from_str(&child_response).unwrap();

        let child_git = std::fs::read_to_string(new_checkout.join(".git")).unwrap();
        let registry = git_worktree_registry(&new_repo_root);
        eprintln!("after.parent_response={parent_response}");
        eprintln!("after.child_response={child_response}");
        eprintln!("after.child_git={}", child_git.trim());
        eprintln!("after.registry=\n{registry}");

        assert!(child_git.contains(&new_repo_root.join(".git").display().to_string()));
        assert!(registry.contains(&new_checkout.display().to_string()));
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&new_checkout)
            .args(["status", "--porcelain=v1"])
            .output()
            .unwrap();
        assert!(status.status.success());
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_repairs_child_that_did_not_move_with_parent() {
        let fixture = ParentSpaceApiFixture::new();
        let checkout = test_linked_git_worktree(&fixture.root, "linked");
        let old_repo_root = fixture.root.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_repo_root, &old_key, &old_repo_root, false);
        let parent_id = parent.id.clone();
        let child = grouped_worktree_workspace("child", &checkout, &old_key, &old_repo_root, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_repo_root = fixture.root.join("moved-source");
        std::fs::rename(&old_repo_root, &new_repo_root).unwrap();
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_repo_root,
        ))
        .unwrap();
        let response = retarget_workspace_response(&mut app, &child_id, &checkout);
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceInfo { .. }
        ));

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["status", "--porcelain=v1"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert_eq!(
            app.state.workspaces[1]
                .worktree_space()
                .unwrap()
                .checkout_path,
            checkout
        );
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_rejects_missing_or_ambiguous_recovery_parent() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_checkout = test_linked_git_worktree(&old_container, "linked");
        let old_repo_root = old_container.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let missing_child =
            grouped_worktree_workspace("child", &old_checkout, &old_key, &old_repo_root, true);
        let missing_child_id = missing_child.id.clone();
        let ambiguous_child =
            grouped_worktree_workspace("child", &old_checkout, &old_key, &old_repo_root, true);
        let ambiguous_child_id = ambiguous_child.id.clone();

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("linked-source");
        let new_checkout = new_container.join("linked");

        let mut missing_app = parent_space_api_app(vec![missing_child]);
        missing_app.state.session_dirty = false;
        let response =
            retarget_workspace_response(&mut missing_app, &missing_child_id, &new_checkout);
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_parent_missing"
        );
        assert!(error.error.message.ends_with("found 0"));
        assert_eq!(missing_app.state.workspaces[0].identity_cwd, old_checkout);
        assert!(!missing_app.state.session_dirty);
        missing_app.state.assert_invariants_for_test();

        let first_parent = grouped_worktree_workspace(
            "first-parent",
            &new_repo_root,
            &old_key,
            &new_repo_root,
            false,
        );
        let second_parent = grouped_worktree_workspace(
            "second-parent",
            &new_repo_root,
            &old_key,
            &new_repo_root,
            false,
        );
        let mut ambiguous_app =
            parent_space_api_app(vec![ambiguous_child, first_parent, second_parent]);
        ambiguous_app.state.session_dirty = false;
        let response =
            retarget_workspace_response(&mut ambiguous_app, &ambiguous_child_id, &new_checkout);
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_parent_ambiguous"
        );
        assert!(error.error.message.ends_with("found 2"));
        assert_eq!(ambiguous_app.state.workspaces[0].identity_cwd, old_checkout);
        assert!(!ambiguous_app.state.session_dirty);
        ambiguous_app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_names_malformed_admin_pointer_and_keeps_state() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_checkout = test_linked_git_worktree(&old_container, "linked");
        let old_repo_root = old_container.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_repo_root, &old_key, &old_repo_root, false);
        let parent_id = parent.id.clone();
        let child =
            grouped_worktree_workspace("child", &old_checkout, &old_key, &old_repo_root, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("linked-source");
        let new_checkout = new_container.join("linked");
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_repo_root,
        ))
        .unwrap();
        app.state.session_dirty = false;

        let admin_pointer = new_repo_root
            .join(".git")
            .join("worktrees")
            .join("linked")
            .join("gitdir");
        std::fs::write(&admin_pointer, "one\ntwo\n").unwrap();
        let old_identity = app.state.workspaces[1].identity_cwd.clone();
        let old_membership = app.state.workspaces[1].worktree_space().cloned();
        let response = retarget_workspace_response(&mut app, &child_id, &new_checkout);
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_pointer_broken"
        );
        assert!(error
            .error
            .message
            .contains(&admin_pointer.display().to_string()));
        assert_eq!(app.state.workspaces[1].identity_cwd, old_identity);
        assert_eq!(
            app.state.workspaces[1].worktree_space().cloned(),
            old_membership
        );
        assert!(!app.state.session_dirty);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_rejects_well_formed_admin_pointer_to_unrelated_checkout() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_checkout = test_linked_git_worktree(&old_container, "linked");
        let old_repo_root = old_container.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_repo_root, &old_key, &old_repo_root, false);
        let parent_id = parent.id.clone();
        let child =
            grouped_worktree_workspace("child", &old_checkout, &old_key, &old_repo_root, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("linked-source");
        let new_checkout = new_container.join("linked");
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_repo_root,
        ))
        .unwrap();
        app.state.session_dirty = false;

        let unrelated = test_git_checkout(&fixture.root, "unrelated");
        let admin_pointer = new_repo_root
            .join(".git")
            .join("worktrees")
            .join("linked")
            .join("gitdir");
        std::fs::write(
            &admin_pointer,
            format!("{}\n", unrelated.join(".git").display()),
        )
        .unwrap();
        let registry_before = git_worktree_registry(&new_repo_root);
        let old_identity = app.state.workspaces[1].identity_cwd.clone();
        let old_membership = app.state.workspaces[1].worktree_space().cloned();

        let response = retarget_workspace_response(&mut app, &child_id, &new_checkout);

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_pointer_broken"
        );
        assert!(error
            .error
            .message
            .contains(&admin_pointer.display().to_string()));
        assert_eq!(app.state.workspaces[1].identity_cwd, old_identity);
        assert_eq!(
            app.state.workspaces[1].worktree_space().cloned(),
            old_membership
        );
        assert_eq!(git_worktree_registry(&new_repo_root), registry_before);
        assert!(!app.state.session_dirty);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_reports_native_repair_failure_without_changing_state_or_registry() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_checkout = test_linked_git_worktree(&old_container, "linked");
        let old_repo_root = old_container.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_repo_root, &old_key, &old_repo_root, false);
        let parent_id = parent.id.clone();
        let child =
            grouped_worktree_workspace("child", &old_checkout, &old_key, &old_repo_root, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("linked-source");
        let new_checkout = new_container.join("linked");
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_repo_root,
        ))
        .unwrap();
        app.state.session_dirty = false;

        let registry_before = git_worktree_registry(&new_repo_root);
        let child_pointer = new_checkout.join(".git");
        let child_pointer_before = std::fs::read_to_string(&child_pointer).unwrap();
        let admin_pointer = new_repo_root
            .join(".git")
            .join("worktrees")
            .join("linked")
            .join("gitdir");
        let admin_pointer_before = std::fs::read_to_string(&admin_pointer).unwrap();
        let old_identity = app.state.workspaces[1].identity_cwd.clone();
        let old_membership = app.state.workspaces[1].worktree_space().cloned();

        let response = crate::worktree::with_forced_linked_worktree_repair_failure(|| {
            retarget_workspace_response(&mut app, &child_id, &new_checkout)
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_repair_failed"
        );
        assert!(error
            .error
            .message
            .contains(&child_pointer.display().to_string()));
        assert!(error
            .error
            .message
            .contains("forced native git worktree repair failure"));
        assert_eq!(app.state.workspaces[1].identity_cwd, old_identity);
        assert_eq!(
            app.state.workspaces[1].worktree_space().cloned(),
            old_membership
        );
        assert_eq!(
            std::fs::read_to_string(child_pointer).unwrap(),
            child_pointer_before
        );
        assert_eq!(
            std::fs::read_to_string(admin_pointer).unwrap(),
            admin_pointer_before
        );
        assert_eq!(git_worktree_registry(&new_repo_root), registry_before);
        assert!(!app.state.session_dirty);
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_retarget_repair_failure_keeps_live_runtime_and_snapshot_unchanged() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_child =
            std::fs::canonicalize(test_linked_git_worktree(&old_container, "child")).unwrap();
        let old_parent = std::fs::canonicalize(old_container.join("child-source")).unwrap();
        let old_key = std::fs::canonicalize(old_parent.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_parent, &old_key, &old_parent, false);
        let parent_id = parent.id.clone();
        let child = grouped_worktree_workspace("child", &old_child, &old_key, &old_parent, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        let pane_id = app.state.workspaces[1].tabs[0].root_pane;
        let (terminal_id, old_pid) = install_live_shell_runtime(&mut app, 1, pane_id, &old_child);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_parent = std::fs::canonicalize(new_container.join("child-source")).unwrap();
        let new_child = std::fs::canonicalize(new_container.join("child")).unwrap();
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_parent,
        ))
        .unwrap();
        app.state.session_dirty = false;
        app.session_save_deadline = None;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let snapshot_before = serde_json::to_vec(&crate::persist::capture(
            &app.state.workspaces,
            &app.state.terminals,
            &app.terminal_runtimes,
            app.state.active,
            app.state.selected,
            app.state.sidebar_width,
            app.state.sidebar_section_split,
            app.state.collapsed_space_keys.clone(),
        ))
        .unwrap();
        let child_pointer = new_child.join(".git");
        let admin_pointer = new_parent
            .join(".git")
            .join("worktrees")
            .join("child")
            .join("gitdir");
        let child_pointer_before = std::fs::read(&child_pointer).unwrap();
        let admin_pointer_before = std::fs::read(&admin_pointer).unwrap();
        let registry_before = git_worktree_registry(&new_parent);
        let event_cursor = app.event_hub.current_sequence();

        let response = crate::worktree::with_forced_linked_worktree_repair_failure(|| {
            retarget_workspace_response(&mut app, &child_id, &new_child)
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_repair_failed"
        );
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .and_then(|runtime| runtime.child_pid()),
            Some(old_pid)
        );
        let snapshot_after = serde_json::to_vec(&crate::persist::capture(
            &app.state.workspaces,
            &app.state.terminals,
            &app.terminal_runtimes,
            app.state.active,
            app.state.selected,
            app.state.sidebar_width,
            app.state.sidebar_section_split,
            app.state.collapsed_space_keys.clone(),
        ))
        .unwrap();
        assert_eq!(snapshot_after, snapshot_before);
        assert!(!app.state.session_dirty);
        assert!(app.session_save_deadline.is_none());
        assert!(app.intentional_pane_restarts.is_empty());
        assert!(app.event_hub.events_after(event_cursor).is_empty());
        assert_eq!(std::fs::read(child_pointer).unwrap(), child_pointer_before);
        assert_eq!(std::fs::read(admin_pointer).unwrap(), admin_pointer_before);
        assert_eq!(git_worktree_registry(&new_parent), registry_before);
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_retarget_rejects_symlinked_admin_pointer() {
        use std::os::unix::fs::symlink;

        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_checkout = test_linked_git_worktree(&old_container, "linked");
        let old_repo_root = old_container.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let parent =
            grouped_worktree_workspace("parent", &old_repo_root, &old_key, &old_repo_root, false);
        let parent_id = parent.id.clone();
        let child =
            grouped_worktree_workspace("child", &old_checkout, &old_key, &old_repo_root, true);
        let child_id = child.id.clone();
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("linked-source");
        let new_checkout = new_container.join("linked");
        let _: SuccessResponse = serde_json::from_str(&retarget_workspace_response(
            &mut app,
            &parent_id,
            &new_repo_root,
        ))
        .unwrap();
        app.state.session_dirty = false;

        let admin_pointer = new_repo_root
            .join(".git")
            .join("worktrees")
            .join("linked")
            .join("gitdir");
        let saved_pointer = admin_pointer.with_extension("saved");
        std::fs::rename(&admin_pointer, &saved_pointer).unwrap();
        symlink(&saved_pointer, &admin_pointer).unwrap();

        let response = retarget_workspace_response(&mut app, &child_id, &new_checkout);
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_pointer_broken"
        );
        assert!(error
            .error
            .message
            .contains(&admin_pointer.display().to_string()));
        assert!(!app.state.session_dirty);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_names_broken_linked_worktree_pointer_and_keeps_state() {
        let fixture = ParentSpaceApiFixture::new();
        let old_container = fixture.root.join("old-location");
        std::fs::create_dir_all(&old_container).unwrap();
        let old_checkout = test_linked_git_worktree(&old_container, "linked");
        let old_repo_root = old_container.join("linked-source");
        let old_key = std::fs::canonicalize(old_repo_root.join(".git"))
            .unwrap()
            .display()
            .to_string();

        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = old_repo_root.clone();
        parent.cached_identity_cwd = old_repo_root.clone();
        let parent_id = parent.id.clone();
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: old_key.clone(),
            label: "linked-source".into(),
            repo_root: old_repo_root.clone(),
            checkout_path: old_repo_root.clone(),
            is_linked_worktree: false,
        });

        let mut child = Workspace::test_new("child");
        child.identity_cwd = old_checkout.clone();
        child.cached_identity_cwd = old_checkout.clone();
        let child_id = child.id.clone();
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: old_key,
            label: "linked-source".into(),
            repo_root: old_repo_root,
            checkout_path: old_checkout,
            is_linked_worktree: true,
        });
        let mut app = parent_space_api_app(vec![parent, child]);

        let new_container = fixture.root.join("new-location");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_repo_root = new_container.join("linked-source");
        let new_checkout = new_container.join("linked");

        let parent_response = app.handle_api_request(Request {
            id: "moved-parent".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id: parent_id,
                path: new_repo_root.display().to_string(),
            }),
        });
        let _: SuccessResponse = serde_json::from_str(&parent_response).unwrap();
        app.state.session_dirty = false;

        let broken_pointer = new_checkout.join(".git");
        std::fs::write(&broken_pointer, "gitdir: /unrelated/.git/worktrees/wrong\n").unwrap();
        let old_identity = app.state.workspaces[1].identity_cwd.clone();
        let old_membership = app.state.workspaces[1].worktree_space().cloned();

        let response = app.handle_api_request(Request {
            id: "broken-child".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id: child_id,
                path: new_checkout.display().to_string(),
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code,
            "workspace_retarget_worktree_pointer_broken"
        );
        assert!(error
            .error
            .message
            .contains(&broken_pointer.display().to_string()));
        assert_eq!(app.state.workspaces[1].identity_cwd, old_identity);
        assert_eq!(
            app.state.workspaces[1].worktree_space().cloned(),
            old_membership
        );
        assert!(!app.state.session_dirty);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn workspace_retarget_rejection_leaves_state_unchanged() {
        let fixture = ParentSpaceApiFixture::new();
        let old_path = test_git_checkout(&fixture.root, "old");
        let (mut app, _event_hub, workspace_id) = retarget_test_app(&old_path);
        let before_auto_label = app.state.workspaces[0].cached_auto_label.clone();
        let before_status_key = app.state.workspaces[0].cached_git_status_key.clone();
        let before_branch = app.state.workspaces[0].cached_git_branch.clone();
        let before_ahead_behind = app.state.workspaces[0].cached_git_ahead_behind;
        let before_git_space = app.state.workspaces[0].cached_git_space.clone();
        let before_terminal_cwds: Vec<_> = app
            .state
            .terminal_ids_for_workspace(0)
            .iter()
            .map(|terminal_id| app.state.terminals[terminal_id].cwd.clone())
            .collect();

        let response = app.handle_api_request(Request {
            id: "relative".into(),
            method: Method::WorkspaceRetarget(WorkspaceRetargetParams {
                workspace_id,
                path: "relative/path".into(),
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "workspace_retarget_path_not_absolute");
        assert_eq!(app.state.workspaces[0].cached_auto_label, before_auto_label);
        assert_eq!(
            app.state.workspaces[0].cached_git_status_key,
            before_status_key
        );
        assert_eq!(app.state.workspaces[0].cached_git_branch, before_branch);
        assert_eq!(
            app.state.workspaces[0].cached_git_ahead_behind,
            before_ahead_behind
        );
        assert_eq!(app.state.workspaces[0].cached_git_space, before_git_space);
        let after_terminal_cwds: Vec<_> = app
            .state
            .terminal_ids_for_workspace(0)
            .iter()
            .map(|terminal_id| app.state.terminals[terminal_id].cwd.clone())
            .collect();
        assert_eq!(after_terminal_cwds, before_terminal_cwds);
        assert!(!app.state.session_dirty);
    }

    #[tokio::test]
    async fn api_parent_space_become_creates_and_adopts_children() {
        let fixture = ParentSpaceApiFixture::new();
        let alpha = fixture.root.join("alpha");
        let beta = fixture.root.join("beta");
        std::fs::create_dir(&alpha).unwrap();
        std::fs::create_dir(&beta).unwrap();

        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = fixture.root.clone();
        let parent_id = parent.id.clone();
        let mut existing_child = Workspace::test_new("alpha");
        existing_child.identity_cwd = alpha.canonicalize().unwrap();
        let existing_child_id = existing_child.id.clone();
        let mut app = parent_space_api_app(vec![parent, existing_child]);

        let response = app.handle_api_request(Request {
            id: "become".into(),
            method: Method::WorkspaceBecomeParent(WorkspaceParentSpaceParams {
                workspace_id: None,
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceParentSpace {
            parent_workspace_id,
            child_workspace_ids,
            cleared_count,
        } = success.result
        else {
            panic!("expected parent-space result");
        };
        assert_eq!(parent_workspace_id, parent_id);
        assert_eq!(cleared_count, 0);
        assert_eq!(app.state.workspaces.len(), 3);
        let created_child = app
            .state
            .workspaces
            .iter()
            .find(|workspace| {
                crate::worktree::canonical_or_original(&workspace.identity_cwd)
                    == beta.canonicalize().unwrap()
            })
            .expect("beta child workspace");
        assert!(child_workspace_ids.contains(&existing_child_id));
        assert!(child_workspace_ids.contains(&created_child.id));
        assert!(app.state.workspaces[0].is_parent_space());
        assert!(app.state.workspaces[1]
            .parent_space()
            .is_some_and(|membership| !membership.is_parent));
        assert!(created_child
            .parent_space()
            .is_some_and(|membership| !membership.is_parent));

        super::super::test_support::shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn api_parent_space_become_rejects_linked_worktree_child() {
        let mut app = app_with_linked_worktree();
        let workspace_id = app.state.workspaces[0].id.clone();

        let response = app.handle_api_request(Request {
            id: "linked".into(),
            method: Method::WorkspaceBecomeParent(WorkspaceParentSpaceParams {
                workspace_id: Some(workspace_id),
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "linked_worktree_parent_space");
        assert!(app.state.workspaces[0].parent_space().is_none());
    }

    #[test]
    fn api_parent_space_rescan_rejects_non_parent() {
        let mut app = parent_space_api_app(vec![Workspace::test_new("plain")]);

        let response = app.handle_api_request(Request {
            id: "rescan".into(),
            method: Method::WorkspaceRescanChildren(WorkspaceParentSpaceParams {
                workspace_id: None,
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "not_parent_space");
    }

    #[test]
    fn api_parent_space_stop_clears_membership() {
        let fixture = ParentSpaceApiFixture::new();
        let root = fixture.root.canonicalize().unwrap();
        let parent_membership = ParentSpaceMembership {
            key: format!("folder:{}", root.display()),
            root: root.clone(),
            is_parent: true,
        };
        let mut parent = Workspace::test_new("parent");
        parent.identity_cwd = root.clone();
        parent.parent_space = Some(parent_membership.clone());
        let parent_id = parent.id.clone();
        let mut child = Workspace::test_new("child");
        child.parent_space = Some(ParentSpaceMembership {
            key: parent_membership.key.clone(),
            root,
            is_parent: false,
        });
        let mut app = parent_space_api_app(vec![parent, child]);

        let response = app.handle_api_request(Request {
            id: "stop".into(),
            method: Method::WorkspaceStopParent(WorkspaceParentSpaceParams {
                workspace_id: Some(parent_id.clone()),
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            success.result,
            ResponseResult::WorkspaceParentSpace {
                parent_workspace_id: parent_id,
                child_workspace_ids: Vec::new(),
                cleared_count: 2,
            }
        );
        assert!(app
            .state
            .workspaces
            .iter()
            .all(|workspace| workspace.parent_space().is_none()));
    }

    // `new_cwd = follow` must anchor on the focused pane for every creation
    // surface. Splits and tabs already do; a new workspace must follow the
    // focused pane too, not the source workspace's first-tab root pane.
    #[tokio::test]
    async fn workspace_create_follows_focused_pane_cwd_not_first_tab_root() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
        use crate::config::ShellModeConfig;

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        app.state.workspaces = vec![Workspace::test_new("spaces")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();

        // Second tab becomes the focused pane, away from tab 1's root pane.
        let response = app.handle_tab_create(
            "tab".into(),
            crate::api::schema::TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: true,
                label: None,
                env: Default::default(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        // Drop runtimes so cwd resolution deterministically uses cached state.
        shutdown_test_runtimes(&mut app);

        let focused_cwd = std::env::temp_dir().join(format!(
            "herdr-ws-follow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&focused_cwd).unwrap();
        let ws = &app.state.workspaces[0];
        let root_cwd = ws.identity_cwd.clone();
        let focused_pane = ws.focused_pane_id().unwrap();
        assert_ne!(focused_pane, ws.tabs[0].root_pane);
        let terminal_id = ws.terminal_id(focused_pane).cloned().unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = focused_cwd.clone();

        let response = app.handle_workspace_create(
            "req".into(),
            WorkspaceCreateParams {
                cwd: None,
                machine: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceCreated { .. }
        ));
        let created_cwd = &app.state.workspaces[1].identity_cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&focused_cwd)
        );
        assert_ne!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&root_cwd)
        );
        shutdown_test_runtimes(&mut app);
        let _ = std::fs::remove_dir_all(&focused_cwd);
    }

    fn app_with_linked_worktree() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("issue")];
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        app
    }

    #[test]
    fn api_workspace_close_closes_linked_worktree_workspace_only() {
        let mut app = app_with_linked_worktree();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceTarget {
                workspace_id: app.state.workspaces[0].id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        assert_eq!(app.state.request_remove_linked_worktree, None);
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn api_workspace_close_event_includes_final_worktree_snapshot() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = app_with_linked_worktree().state.workspaces;
        let workspace_id = app.state.workspaces[0].id.clone();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceTarget {
                workspace_id: workspace_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceClosed {
                    workspace_id: closed_id,
                    workspace: Some(workspace),
                } if closed_id == &workspace_id
                    && workspace
                        .worktree
                        .as_ref()
                        .is_some_and(|worktree| worktree.is_linked_worktree)
            )
        }));
    }

    #[test]
    fn workspace_metadata_tokens_patch_clear_and_emit_snapshot() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0);

        for (tokens, expected) in [
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("reviewing auth".into())),
                    ("jj_status".into(), Some("2 changes".into())),
                ]),
                std::collections::HashMap::from([
                    ("summary".into(), "reviewing auth".into()),
                    ("jj_status".into(), "2 changes".into()),
                ]),
            ),
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("done".into())),
                    ("jj_status".into(), None),
                ]),
                std::collections::HashMap::from([("summary".into(), "done".into())]),
            ),
        ] {
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "req".into(),
                method: crate::api::schema::Method::WorkspaceReportMetadata(
                    WorkspaceReportMetadataParams {
                        workspace_id: workspace_id.clone(),
                        source: "user:test".into(),
                        tokens,
                        seq: None,
                        ttl_ms: None,
                    },
                ),
            });
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(success.result, ResponseResult::Ok {});
            assert_eq!(app.workspace_info(0).tokens, expected);
        }

        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace }
                if workspace.tokens.get("summary").map(String::as_str) == Some("done")
                    && !workspace.tokens.contains_key("jj_status")
        )));
    }

    #[test]
    fn workspace_token_ttl_expires_through_runtime_and_emits_update() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0);
        let response = app.handle_workspace_report_metadata(
            "req".into(),
            WorkspaceReportMetadataParams {
                workspace_id,
                source: "user:test".into(),
                tokens: std::collections::HashMap::from([(
                    "summary".into(),
                    Some("temporary".into()),
                )]),
                seq: None,
                ttl_ms: Some(1),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let deadline = app.agent_metadata_deadline.expect("token deadline");

        app.expire_metadata_at(deadline, deadline);

        assert!(app.workspace_info(0).tokens.is_empty());
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace } if workspace.tokens.is_empty()
        )));
    }

    #[test]
    fn api_workspace_move_reorders_workspaces() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_id = app.public_workspace_id(0);

        let response = app.handle_workspace_move(
            "req".into(),
            WorkspaceMoveParams {
                workspace_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[2].workspace_id, moved_id);
        assert_eq!(app.state.workspaces[2].display_name(), "one");
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index: 3,
                    workspaces,
                } if workspace_id == &moved_id
                    && workspaces[2].workspace_id == moved_id
            )
        }));
    }

    #[test]
    fn api_workspace_move_block_reorders_atomically() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![
            Workspace::test_new("child"),
            Workspace::test_new("normal"),
            Workspace::test_new("parent"),
            Workspace::test_new("tail"),
        ];
        let parent_id = app.public_workspace_id(2);
        let child_id = app.public_workspace_id(0);
        let tail_id = app.public_workspace_id(3);

        let response = app.handle_workspace_move_block(
            "req".into(),
            WorkspaceMoveBlockParams {
                workspace_ids: vec![parent_id.clone(), child_id.clone()],
                before_workspace_id: Some(tail_id.clone()),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(|workspace| workspace.display_name())
                .collect::<Vec<_>>(),
            ["normal", "parent", "child", "tail"]
        );
        assert_eq!(workspaces[1].workspace_id, parent_id);
        assert_eq!(workspaces[2].workspace_id, child_id);
        let events = event_hub.events_after(0);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1.data,
            EventData::WorkspaceReordered {
                workspace_ids,
                before_workspace_id,
                workspaces,
            } if workspace_ids.first() == Some(&parent_id)
                && workspace_ids.get(1) == Some(&child_id)
                && workspace_ids.len() == 2
                && before_workspace_id.as_deref() == Some(tail_id.as_str())
                && workspaces[1].workspace_id == parent_id
        ));
    }

    #[test]
    fn api_workspace_move_noop_does_not_emit_event() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        let moved_id = app.public_workspace_id(0);

        let response = app.handle_workspace_move(
            "req".into(),
            WorkspaceMoveParams {
                workspace_id: moved_id.clone(),
                insert_index: 1,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[0].workspace_id, moved_id);
        assert!(event_hub.events_after(0).is_empty());
    }
}
