use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};
use tracing::{error, warn};

use crate::detect::AgentState;
use crate::events::AppEvent;
use crate::layout::{Node, PaneId, TileLayout};
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};
use crate::workspace::Workspace;

use super::snapshot::{
    PaneAgentSessionSnapshot, PaneHistorySnapshot, TabHistorySnapshot, WorkspaceHistorySnapshot,
};
use super::{
    DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot, TabSnapshot,
    WorkspaceSnapshot,
};

struct AgentRestoreState<'a> {
    enabled: bool,
    resumed_sessions: &'a mut HashSet<String>,
}

struct PaneRestoreStartup<'a> {
    restore_plan: Option<crate::agent_resume::AgentResumePlan>,
    initial_history_ansi: Option<&'a str>,
    duplicate_agent_session: bool,
    reserved_agent_session: Option<String>,
}

#[derive(Clone, Copy)]
pub struct RestorePolicy<'a> {
    resume_agents_on_restore: bool,
    machines: &'a [crate::config::MachineConfig],
}

impl<'a> RestorePolicy<'a> {
    pub fn new(
        resume_agents_on_restore: bool,
        machines: &'a [crate::config::MachineConfig],
    ) -> Self {
        Self {
            resume_agents_on_restore,
            machines,
        }
    }
}

struct RestoreRuntimeContext<'a> {
    scrollback_limit_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'a>,
    resume_agents_on_restore: bool,
    machines: &'a [crate::config::MachineConfig],
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
}

#[derive(Clone, Copy)]
struct MachineRestoreContext<'a> {
    argv: Option<&'a [String]>,
    error: Option<&'a str>,
    is_machine: bool,
    identity_cwd: Option<&'a std::path::Path>,
}

type RestoredSession = (
    Vec<Workspace>,
    HashMap<TerminalId, TerminalState>,
    HashMap<TerminalId, TerminalRuntime>,
);
type RestoredWorkspace = (
    Workspace,
    Vec<TerminalState>,
    HashMap<TerminalId, TerminalRuntime>,
);
type RestoredTab = (
    crate::workspace::Tab,
    Vec<TerminalState>,
    HashMap<TerminalId, TerminalRuntime>,
    HashMap<PaneId, u32>,
);
type RestoreFailures<T> = (T, usize);

/// Restore workspaces from a snapshot. Each pane gets a fresh shell in its saved cwd.
pub fn restore(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    default_shell: &str,
    shell_mode: crate::config::ShellModeConfig,
    policy: RestorePolicy<'_>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
) -> RestoredSession {
    let mut imported_panes = HashMap::new();
    let runtime_context = RestoreRuntimeContext {
        scrollback_limit_bytes,
        shell_config: crate::pane::PaneShellConfig::new(default_shell, shell_mode),
        resume_agents_on_restore: policy.resume_agents_on_restore,
        machines: policy.machines,
        events,
        render_notify,
        render_dirty,
    };
    restore_with_imports(
        snapshot,
        history,
        rows,
        cols,
        &runtime_context,
        &mut imported_panes,
    )
}

#[cfg(unix)]
pub fn restore_handoff(
    snapshot: &SessionSnapshot,
    scrollback_limit_bytes: usize,
    default_shell: &str,
    shell_mode: crate::config::ShellModeConfig,
    machines: &[crate::config::MachineConfig],
    imports: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
) -> std::io::Result<RestoredSession> {
    let runtime_context = RestoreRuntimeContext {
        scrollback_limit_bytes,
        shell_config: crate::pane::PaneShellConfig::new(default_shell, shell_mode),
        resume_agents_on_restore: true,
        machines,
        events,
        render_notify,
        render_dirty,
    };
    restore_with_imports_strict(snapshot, None, 24, 80, &runtime_context, imports)
}

#[cfg(unix)]
pub fn handoff_pane_aliases(
    snapshot: &SessionSnapshot,
    workspaces: &[Workspace],
) -> HashMap<u32, PaneId> {
    let mut aliases = HashMap::new();
    for (ws_snap, workspace) in snapshot.workspaces.iter().zip(workspaces) {
        for (tab_snap, tab) in ws_snap.tabs.iter().zip(&workspace.tabs) {
            let old_ids = collect_snapshot_pane_ids(&tab_snap.layout);
            let new_ids = tab.layout.pane_ids();
            for (old_id, new_id) in old_ids.into_iter().zip(new_ids) {
                if old_id != new_id.raw() {
                    aliases.insert(old_id, new_id);
                }
            }
        }
    }
    aliases
}

#[cfg(unix)]
fn collect_snapshot_pane_ids(node: &LayoutSnapshot) -> Vec<u32> {
    let mut ids = Vec::new();
    collect_snapshot_ids_inner(node, &mut ids);
    ids
}

#[cfg(unix)]
fn collect_snapshot_ids_inner(node: &LayoutSnapshot, ids: &mut Vec<u32>) {
    match node {
        LayoutSnapshot::Pane(id) => ids.push(*id),
        LayoutSnapshot::Split { first, second, .. } => {
            collect_snapshot_ids_inner(first, ids);
            collect_snapshot_ids_inner(second, ids);
        }
    }
}

fn migrated_public_pane_numbers_by_old_raw(
    snap: &WorkspaceSnapshot,
    next_public_pane_number: &mut usize,
) -> HashMap<u32, usize> {
    let mut public_numbers = snap.public_pane_numbers.clone();
    for tab in &snap.tabs {
        let mut pane_ids = Vec::new();
        collect_layout_snapshot_pane_ids(&tab.layout, &mut pane_ids);
        for old_raw in pane_ids {
            public_numbers.entry(old_raw).or_insert_with(|| {
                let number = *next_public_pane_number;
                *next_public_pane_number += 1;
                number
            });
        }
    }
    public_numbers
}

fn collect_layout_snapshot_pane_ids(node: &LayoutSnapshot, ids: &mut Vec<u32>) {
    match node {
        LayoutSnapshot::Pane(id) => ids.push(*id),
        LayoutSnapshot::Split { first, second, .. } => {
            collect_layout_snapshot_pane_ids(first, ids);
            collect_layout_snapshot_pane_ids(second, ids);
        }
    }
}

#[cfg(unix)]
fn restore_with_imports_strict(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
) -> std::io::Result<RestoredSession> {
    let (restored, failed_imports) = restore_with_imports_and_failures(
        snapshot,
        history,
        rows,
        cols,
        runtime_context,
        imported_panes,
    );
    if failed_imports > 0 {
        return Err(std::io::Error::other(format!(
            "handoff failed to restore {failed_imports} imported pane runtime(s)"
        )));
    }
    if !imported_panes.is_empty() {
        return Err(std::io::Error::other(format!(
            "handoff import did not consume {} pane runtime(s)",
            imported_panes.len()
        )));
    }
    Ok(restored)
}

fn restore_with_imports(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
) -> RestoredSession {
    restore_with_imports_and_failures(
        snapshot,
        history,
        rows,
        cols,
        runtime_context,
        imported_panes,
    )
    .0
}

fn restore_with_imports_and_failures(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
) -> RestoreFailures<RestoredSession> {
    let mut workspaces = Vec::new();
    let mut terminals = HashMap::new();
    let mut terminal_runtimes = HashMap::new();
    let mut resumed_agent_sessions = HashSet::new();
    let mut failed_imports = 0;
    let saved_worktree_spaces = snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            if workspace.machine.is_some() {
                None
            } else {
                workspace.worktree_space.clone()
            }
        })
        .collect::<Vec<_>>();
    let restored_worktree_spaces = restored_worktree_space_memberships(&saved_worktree_spaces);
    for (idx, ws_snap) in snapshot.workspaces.iter().enumerate() {
        let (restored, workspace_failed_imports) = restore_workspace(
            ws_snap,
            restored_worktree_spaces.get(idx).cloned().flatten(),
            history.and_then(|history| history.workspaces.get(idx)),
            rows,
            cols,
            runtime_context,
            &mut resumed_agent_sessions,
            imported_panes,
        );
        failed_imports += workspace_failed_imports;
        if let Some((workspace, restored_terminals, restored_runtimes)) = restored {
            for terminal in restored_terminals {
                terminals.insert(terminal.id.clone(), terminal);
            }
            terminal_runtimes.extend(restored_runtimes);
            workspaces.push(workspace);
        }
    }
    crate::workspace::reserve_workspace_ids(&workspaces);
    ((workspaces, terminals, terminal_runtimes), failed_imports)
}

fn restore_workspace(
    snap: &WorkspaceSnapshot,
    restored_worktree_space: Option<crate::workspace::WorktreeSpaceMembership>,
    history: Option<&WorkspaceHistorySnapshot>,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    resumed_agent_sessions: &mut HashSet<String>,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
) -> RestoreFailures<Option<RestoredWorkspace>> {
    let mut tabs = Vec::new();
    let mut terminals = Vec::new();
    let mut terminal_runtimes = HashMap::new();
    let workspace_id = snap
        .id
        .clone()
        .unwrap_or_else(crate::workspace::generate_workspace_id);
    let mut next_public_pane_number = snap
        .public_pane_numbers
        .values()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(snap.next_public_pane_number);
    let public_pane_numbers_by_old_raw =
        migrated_public_pane_numbers_by_old_raw(snap, &mut next_public_pane_number);
    let public_pane_ids_by_old_raw: HashMap<u32, String> = public_pane_numbers_by_old_raw
        .iter()
        .map(|(old_raw, public_number)| {
            (
                *old_raw,
                format!(
                    "{}:p{}",
                    workspace_id,
                    crate::workspace::encode_public_number(*public_number)
                ),
            )
        })
        .collect();
    let mut public_pane_numbers = HashMap::new();
    let mut next_public_tab_number = snap
        .public_tab_numbers
        .iter()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(snap.next_public_tab_number);
    let mut failed_imports = 0;
    let (machine_argv, machine_restore_error) = match snap.machine.as_deref() {
        Some(machine) => match crate::config::machine_ssh_argv(runtime_context.machines, machine) {
            Ok(argv) => (Some(argv), None),
            Err(err) => {
                error!(
                    machine,
                    err = %err,
                    "machine workspace restored without running panes"
                );
                (None, Some(err.to_string()))
            }
        },
        None => (None, None),
    };
    let machine_workspace = snap.machine.is_some();
    let machine_identity_cwd = if machine_workspace {
        Some(restored_machine_identity_cwd(
            &snap.identity_cwd,
            crate::app::machine_identity_cwd(),
        ))
    } else {
        None
    };

    for (idx, tab_snap) in snap.tabs.iter().enumerate() {
        let tab_number = snap.public_tab_numbers.get(idx).copied().unwrap_or(idx + 1);
        let (restored_tab, tab_failed_imports) = restore_tab(
            tab_snap,
            history.and_then(|history| history.tabs.get(idx)),
            tab_number,
            &workspace_id,
            rows,
            cols,
            runtime_context,
            MachineRestoreContext {
                argv: machine_argv.as_deref(),
                error: machine_restore_error.as_deref(),
                is_machine: machine_workspace,
                identity_cwd: machine_identity_cwd.as_deref(),
            },
            resumed_agent_sessions,
            imported_panes,
            &public_pane_ids_by_old_raw,
        );
        failed_imports += tab_failed_imports;
        let Some((mut tab, restored_terminals, restored_runtimes, reverse_id_map)) = restored_tab
        else {
            continue;
        };
        if let Some(public_tab_number) = snap.public_tab_numbers.get(idx).copied() {
            tab.number = public_tab_number;
        }
        next_public_tab_number = next_public_tab_number.max(tab.number + 1);
        for pane_id in tab.layout.pane_ids() {
            let public_number = public_pane_numbers_by_old_raw
                .get(
                    &reverse_id_map
                        .get(&pane_id)
                        .copied()
                        .unwrap_or(pane_id.raw()),
                )
                .copied()
                .unwrap_or_else(|| {
                    let number = next_public_pane_number;
                    next_public_pane_number += 1;
                    number
                });
            public_pane_numbers.insert(pane_id, public_number);
            next_public_pane_number = next_public_pane_number.max(public_number + 1);
        }
        terminals.extend(restored_terminals);
        terminal_runtimes.extend(restored_runtimes);
        tabs.push(tab);
    }

    if tabs.is_empty() {
        return (None, failed_imports);
    }

    let identity_cwd = machine_identity_cwd
        .clone()
        .unwrap_or_else(|| snap.identity_cwd.clone());
    let worktree_space = (!machine_workspace)
        .then_some(restored_worktree_space)
        .flatten();
    let (cached_git_space, cached_auto_label, cached_git_status_key) = if machine_workspace {
        (
            None,
            crate::workspace::fallback_label_from_cwd(&identity_cwd),
            identity_cwd.clone(),
        )
    } else {
        crate::workspace::discover_workspace_git_identity(&identity_cwd)
    };
    let cached_git_branch = (!machine_workspace)
        .then(|| crate::workspace::git_branch(&identity_cwd))
        .flatten();

    (
        Some(Workspace {
            id: workspace_id,
            custom_name: snap.custom_name.clone().or_else(|| snap.machine.clone()),
            machine: snap.machine.clone(),
            identity_cwd: identity_cwd.clone(),
            stale: false,
            cached_identity_cwd: identity_cwd,
            cached_auto_label,
            cached_git_status_key,
            cached_git_branch,
            cached_git_ahead_behind: None,
            cached_git_space,
            worktree_space,
            parent_space: (!machine_workspace)
                .then(|| snap.parent_space.clone())
                .flatten(),
            metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            public_pane_numbers,
            next_public_pane_number,
            next_public_tab_number,
            active_tab: snap.active_tab.min(tabs.len().saturating_sub(1)),
            tabs,
            #[cfg(test)]
            test_runtimes: HashMap::new(),
        })
        .map(|workspace| (workspace, terminals, terminal_runtimes)),
        failed_imports,
    )
}

#[cfg(test)]
fn restored_worktree_space_membership(
    space: Option<crate::workspace::WorktreeSpaceMembership>,
) -> Option<crate::workspace::WorktreeSpaceMembership> {
    restored_worktree_space_memberships(&[space])
        .into_iter()
        .next()
        .flatten()
}

fn restored_worktree_space_memberships(
    spaces: &[Option<crate::workspace::WorktreeSpaceMembership>],
) -> Vec<Option<crate::workspace::WorktreeSpaceMembership>> {
    let mut restored = vec![None; spaces.len()];
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();

    for (index, space) in spaces.iter().enumerate() {
        if let Some(space) = space {
            groups.entry(&space.key).or_default().push(index);
        }
    }

    for indices in groups.into_values() {
        if indices.len() == 1 {
            let index = indices[0];
            let Some(space) = spaces[index].as_ref() else {
                continue;
            };
            if live_membership_matches(space, &space.key) {
                restored[index] = Some(space.clone());
            }
            continue;
        }

        let Some(parent_index) = coherent_recovery_group_parent(spaces, &indices) else {
            continue;
        };
        let Some(parent) = spaces[parent_index].as_ref() else {
            continue;
        };

        let live_parent = crate::workspace::git_space_metadata(&parent.checkout_path);
        let expected_key = match live_parent.as_ref() {
            Some(current)
                if !current.is_linked_worktree
                    && same_path(&current.repo_root, &parent.checkout_path) =>
            {
                current.key.as_str()
            }
            Some(_) => continue,
            None => parent.key.as_str(),
        };

        let retained = indices
            .iter()
            .copied()
            .filter(|index| {
                spaces[*index].as_ref().is_some_and(|space| {
                    crate::workspace::git_space_metadata(&space.checkout_path)
                        .is_none_or(|_| live_membership_matches(space, expected_key))
                })
            })
            .collect::<Vec<_>>();

        if !retained.contains(&parent_index)
            || !retained.iter().any(|index| {
                spaces[*index]
                    .as_ref()
                    .is_some_and(|space| space.is_linked_worktree)
            })
        {
            continue;
        }

        if expected_key != parent.key && !recovery_group_registry_matches(parent, spaces, &retained)
        {
            continue;
        }

        for index in retained {
            restored[index] = spaces[index].clone();
        }
    }

    restored
}

fn coherent_recovery_group_parent(
    spaces: &[Option<crate::workspace::WorktreeSpaceMembership>],
    indices: &[usize],
) -> Option<usize> {
    let mut parent_index = None;
    let mut label = None;
    let mut checkout_paths = HashSet::new();
    let mut linked_count = 0;

    for index in indices {
        let space = spaces.get(*index)?.as_ref()?;
        if !space.checkout_path.is_absolute() || !space.repo_root.is_absolute() {
            return None;
        }
        if !checkout_paths.insert(space.checkout_path.clone()) {
            return None;
        }
        if label.is_some_and(|label| label != space.label) {
            return None;
        }
        label = Some(space.label.as_str());

        if space.is_linked_worktree {
            linked_count += 1;
        } else if parent_index.replace(*index).is_some() || space.checkout_path != space.repo_root {
            return None;
        }
    }

    (linked_count > 0).then_some(parent_index).flatten()
}

fn live_membership_matches(
    space: &crate::workspace::WorktreeSpaceMembership,
    expected_key: &str,
) -> bool {
    crate::workspace::git_space_metadata(&space.checkout_path).is_some_and(|current| {
        current.key == expected_key
            && current.is_linked_worktree == space.is_linked_worktree
            && same_path(&current.repo_root, &space.checkout_path)
    })
}

fn recovery_group_registry_matches(
    parent: &crate::workspace::WorktreeSpaceMembership,
    spaces: &[Option<crate::workspace::WorktreeSpaceMembership>],
    retained: &[usize],
) -> bool {
    let Ok(entries) = crate::worktree::list_existing_worktrees(&parent.checkout_path) else {
        return false;
    };

    retained.iter().all(|index| {
        let Some(space) = spaces[*index].as_ref() else {
            return false;
        };
        if !space.is_linked_worktree {
            return true;
        }
        let expected = crate::worktree::canonical_or_original(&space.checkout_path);
        entries
            .iter()
            .filter(|entry| {
                !entry.is_bare && crate::worktree::canonical_or_original(&entry.path) == expected
            })
            .count()
            == 1
    })
}

fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    crate::worktree::canonical_or_original(left) == crate::worktree::canonical_or_original(right)
}

fn restore_tab(
    snap: &TabSnapshot,
    history: Option<&TabHistorySnapshot>,
    number: usize,
    workspace_id: &str,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    machine: MachineRestoreContext<'_>,
    resumed_agent_sessions: &mut HashSet<String>,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
    public_pane_ids_by_old_raw: &HashMap<u32, String>,
) -> RestoreFailures<Option<RestoredTab>> {
    let (node, id_map) = restore_node_remapped(&snap.layout);
    let reverse_id_map: HashMap<PaneId, u32> = id_map
        .iter()
        .map(|(&old_id, &new_id)| (new_id, old_id))
        .collect();
    let pane_ids = collect_pane_ids(&node);

    let mut panes = HashMap::new();
    let mut terminals = Vec::new();
    let mut terminal_runtimes = HashMap::new();
    let mut failed_imports = 0;
    for id in &pane_ids {
        let old_id = reverse_id_map.get(id);
        let saved_pane = old_id.and_then(|old_id| snap.panes.get(old_id));
        let saved_cwd = saved_pane
            .map(|p| p.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));

        let cwd = if let Some(identity_cwd) = machine.identity_cwd {
            identity_cwd.to_path_buf()
        } else if saved_cwd.exists() {
            saved_cwd
        } else {
            warn!(
                cwd = %saved_cwd.display(),
                "saved pane cwd does not exist, falling back to HOME"
            );
            let home = std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/"));
            if home.exists() {
                home
            } else {
                PathBuf::from("/")
            }
        };

        let saved_label = saved_pane.and_then(|p| p.label.clone());
        let saved_agent_name = saved_pane.and_then(|p| p.agent_name.clone());
        let saved_managed_agent = saved_pane
            .and_then(|pane| pane.managed_agent_kind.as_deref())
            .and_then(crate::detect::parse_canonical_agent_label);
        let saved_launch_argv = saved_pane.and_then(|p| p.launch_argv.clone());
        let saved_agent_session = saved_pane.and_then(|p| p.agent_session.as_ref());
        let saved_history =
            old_id.and_then(|old_id| history.and_then(|history| history.panes.get(old_id)));
        let startup = {
            let mut agent_restore = AgentRestoreState {
                enabled: native_agent_restore_enabled(
                    runtime_context.resume_agents_on_restore,
                    machine.is_machine,
                ),
                resumed_sessions: resumed_agent_sessions,
            };
            pane_restore_startup(saved_agent_session, saved_history, &mut agent_restore)
        };
        let restored_agent_session =
            restored_terminal_agent_session(saved_agent_session, startup.duplicate_agent_session);
        let initial_restore_agent = startup
            .restore_plan
            .as_ref()
            .and_then(|plan| crate::detect::parse_agent_label(&plan.agent));

        let old_pane_id = reverse_id_map.get(id).copied();
        let public_pane_id = old_pane_id
            .and_then(|old_id| public_pane_ids_by_old_raw.get(&old_id))
            .map(String::as_str);
        let launch_env = public_pane_id
            .map(|pane_id| {
                PaneLaunchEnv::from_extra(Vec::new()).with_identity(
                    workspace_id.to_string(),
                    crate::workspace::public_tab_id_for_number(workspace_id, number),
                    pane_id.to_string(),
                )
            })
            .unwrap_or_default();
        let imported_runtime = old_pane_id.and_then(|old_id| imported_panes.remove(&old_id));
        let was_imported = imported_runtime.is_some();
        if imported_runtime.is_none() {
            if let Some(error) = machine.error {
                let terminal_id = TerminalId::alloc();
                let mut terminal = TerminalState::new(terminal_id.clone(), cwd.clone());
                terminal.set_manual_label(error.to_string());
                panes.insert(*id, PaneState::new(terminal_id));
                terminals.push(terminal);
                continue;
            }
        }
        let pending_native_agent_restore = if was_imported || machine.is_machine {
            None
        } else {
            startup.restore_plan.clone()
        };
        if let Some(plan) = pending_native_agent_restore {
            let terminal_id = TerminalId::alloc();
            let mut terminal = TerminalState::new(terminal_id.clone(), cwd.clone())
                .with_pending_agent_resume_plan(plan);
            if let Some(label) = saved_label {
                terminal.set_manual_label(label);
            }
            if let Some(session) = restored_agent_session {
                terminal.set_persisted_agent_session(session);
            }
            match (saved_agent_name, saved_managed_agent) {
                (Some(agent_name), Some(agent)) => {
                    terminal.restore_managed_agent(agent_name, agent)
                }
                (Some(_), None) => {}
                (None, _) => {}
            }
            if let Some(agent) = initial_restore_agent {
                let _ = terminal.set_detected_state_with_screen_signals_at(
                    Some(agent),
                    AgentState::Idle,
                    false,
                    false,
                    false,
                    false,
                    std::time::Instant::now(),
                );
            }
            panes.insert(*id, PaneState::new(terminal_id));
            terminals.push(terminal);
            continue;
        }

        #[cfg(not(unix))]
        if imported_runtime.is_some() {
            failed_imports += 1;
            continue;
        }

        let runtime_result = {
            #[cfg(unix)]
            if let Some(imported) = imported_runtime {
                TerminalRuntime::from_handoff_fd(
                    crate::handoff_runtime::ImportedHandoffRuntime {
                        master_fd: imported.master_fd,
                        state: imported.state.with_pane_id(*id),
                    },
                    runtime_context.scrollback_limit_bytes,
                    crate::terminal_theme::TerminalTheme::default(),
                    None,
                    runtime_context.events.clone(),
                    runtime_context.render_notify.clone(),
                    runtime_context.render_dirty.clone(),
                )
            } else if let Some(argv) = machine.argv {
                TerminalRuntime::spawn_argv_command_with_initial_history(
                    *id,
                    rows,
                    cols,
                    cwd.clone(),
                    argv,
                    &launch_env,
                    crate::pane::AgentDetection::Enabled,
                    runtime_context.scrollback_limit_bytes,
                    crate::terminal_theme::TerminalTheme::default(),
                    None,
                    startup.initial_history_ansi,
                    runtime_context.events.clone(),
                    runtime_context.render_notify.clone(),
                    runtime_context.render_dirty.clone(),
                )
            } else {
                TerminalRuntime::spawn_with_initial_history(
                    *id,
                    rows,
                    cols,
                    cwd.clone(),
                    runtime_context.scrollback_limit_bytes,
                    crate::terminal_theme::TerminalTheme::default(),
                    None,
                    runtime_context.shell_config,
                    &launch_env,
                    startup.initial_history_ansi,
                    runtime_context.events.clone(),
                    runtime_context.render_notify.clone(),
                    runtime_context.render_dirty.clone(),
                )
            }

            #[cfg(not(unix))]
            {
                if let Some(argv) = machine.argv {
                    TerminalRuntime::spawn_argv_command_with_initial_history(
                        *id,
                        rows,
                        cols,
                        cwd.clone(),
                        argv,
                        &launch_env,
                        crate::pane::AgentDetection::Enabled,
                        runtime_context.scrollback_limit_bytes,
                        crate::terminal_theme::TerminalTheme::default(),
                        None,
                        startup.initial_history_ansi,
                        runtime_context.events.clone(),
                        runtime_context.render_notify.clone(),
                        runtime_context.render_dirty.clone(),
                    )
                } else {
                    TerminalRuntime::spawn_with_initial_history(
                        *id,
                        rows,
                        cols,
                        cwd.clone(),
                        runtime_context.scrollback_limit_bytes,
                        crate::terminal_theme::TerminalTheme::default(),
                        None,
                        runtime_context.shell_config,
                        &launch_env,
                        startup.initial_history_ansi,
                        runtime_context.events.clone(),
                        runtime_context.render_notify.clone(),
                        runtime_context.render_dirty.clone(),
                    )
                }
            }
        };

        match runtime_result {
            Ok(runtime) => {
                let terminal_id = TerminalId::alloc();
                let mut terminal = TerminalState::new(terminal_id.clone(), cwd.clone());
                if let Some(argv) = machine.argv {
                    terminal = terminal.with_launch_argv(argv.to_vec());
                }
                if was_imported {
                    if let Some(argv) = saved_launch_argv {
                        terminal = terminal.with_launch_argv(argv).with_respawn_shell_on_exit();
                    }
                }
                if let Some(label) = saved_label {
                    terminal.set_manual_label(label);
                }
                if let Some(session) = restored_agent_session {
                    terminal.set_persisted_agent_session(session);
                }
                match (saved_agent_name, saved_managed_agent) {
                    (Some(agent_name), Some(agent)) if was_imported => {
                        terminal.restore_managed_agent(agent_name, agent)
                    }
                    (Some(_), Some(_)) => {}
                    (Some(agent_name), None) if was_imported => terminal.set_agent_name(agent_name),
                    (Some(_), None) => {}
                    (None, _) => {}
                }
                if let Some(agent) = initial_restore_agent {
                    let _ = terminal.set_detected_state_with_screen_signals_at(
                        Some(agent),
                        AgentState::Idle,
                        false,
                        false,
                        false,
                        false,
                        std::time::Instant::now(),
                    );
                }
                panes.insert(*id, PaneState::new(terminal_id.clone()));
                terminal_runtimes.insert(terminal_id, runtime);
                terminals.push(terminal);
            }
            Err(e) => {
                if let Some(key) = startup.reserved_agent_session.as_deref() {
                    resumed_agent_sessions.remove(key);
                }
                if machine.is_machine && machine.argv.is_some() {
                    let terminal_id = TerminalId::alloc();
                    let mut terminal = TerminalState::new(terminal_id.clone(), cwd.clone());
                    terminal.set_manual_label(format!("machine pane unavailable: {e}"));
                    panes.insert(*id, PaneState::new(terminal_id));
                    terminals.push(terminal);
                    error!(
                        tab = ?snap.custom_name,
                        pane_id = id.raw(),
                        err = %e,
                        "failed to spawn machine pane, restoring inert placeholder"
                    );
                    continue;
                }
                if was_imported {
                    failed_imports += 1;
                    error!(
                        tab = ?snap.custom_name,
                        pane_id = id.raw(),
                        err = %e,
                        "failed to restore imported pane"
                    );
                }
                error!(
                    tab = ?snap.custom_name,
                    pane_id = id.raw(),
                    err = %e,
                    "failed to restore pane, skipping"
                );
            }
        }
    }

    if panes.is_empty() {
        warn!(
            tab = ?snap.custom_name,
            "no panes could be restored for tab, dropping it"
        );
        return (None, failed_imports);
    }

    let surviving: HashSet<PaneId> = panes.keys().copied().collect();
    let Some(node) = prune_restored_node(node, &surviving) else {
        warn!(
            tab = ?snap.custom_name,
            "restored tab lost all panes after pruning missing layout nodes"
        );
        return (None, failed_imports);
    };
    let pane_ids = collect_pane_ids(&node);
    let Some(focus) = resolve_restored_pane(snap.focused, &id_map, &surviving, &pane_ids) else {
        return (None, failed_imports);
    };
    let Some(root_pane) = resolve_restored_pane(snap.root_pane, &id_map, &surviving, &pane_ids)
    else {
        return (None, failed_imports);
    };
    let layout = TileLayout::from_saved(node, focus);

    (
        Some((
            crate::workspace::Tab {
                custom_name: snap.custom_name.clone(),
                number,
                root_pane,
                layout,
                panes,
                #[cfg(test)]
                runtimes: HashMap::new(),
                zoomed: snap.zoomed,
                events: runtime_context.events.clone(),
                render_notify: runtime_context.render_notify.clone(),
                render_dirty: runtime_context.render_dirty.clone(),
            },
            terminals,
            terminal_runtimes,
            reverse_id_map,
        )),
        failed_imports,
    )
}

fn restored_machine_identity_cwd(
    persisted_identity_cwd: &std::path::Path,
    resolved_identity_cwd: std::io::Result<PathBuf>,
) -> PathBuf {
    match resolved_identity_cwd {
        Ok(cwd) => cwd,
        Err(err) => {
            let fallback = if persisted_identity_cwd.is_dir() {
                persisted_identity_cwd.to_path_buf()
            } else {
                std::env::current_dir()
                    .ok()
                    .filter(|cwd| cwd.is_dir())
                    .unwrap_or_else(|| PathBuf::from("/"))
            };
            warn!(
                err = %err,
                fallback = %fallback.display(),
                "failed to resolve local identity for machine workspace, using fallback"
            );
            fallback
        }
    }
}

fn native_agent_restore_enabled(resume_agents_on_restore: bool, machine_workspace: bool) -> bool {
    resume_agents_on_restore && !machine_workspace
}

fn pane_restore_startup<'a>(
    session: Option<&PaneAgentSessionSnapshot>,
    history: Option<&'a PaneHistorySnapshot>,
    agent_restore: &mut AgentRestoreState<'_>,
) -> PaneRestoreStartup<'a> {
    // Native agent resume owns the conversation history. If a pane has a
    // resumable agent session and resume is enabled, do not replay saved pane
    // presentation history into that terminal, even when this pane is a
    // duplicate suppressed by session de-duplication.
    let restore_plan =
        session.and_then(|session| restore_plan_for_snapshot(session, agent_restore.enabled));
    let has_native_agent_restore = restore_plan.is_some();
    // Reserve before spawning so later panes in the same restore pass cannot
    // launch the same native agent session. The caller rolls this reservation
    // back if runtime spawn fails before any agent process is started.
    let mut reserved_agent_session = None;
    let duplicate_agent_session = restore_plan.as_ref().is_some_and(|plan| {
        if agent_restore
            .resumed_sessions
            .insert(plan.dedupe_key.clone())
        {
            reserved_agent_session = Some(plan.dedupe_key.clone());
            false
        } else {
            true
        }
    });
    let restore_plan = if duplicate_agent_session {
        None
    } else {
        restore_plan
    };

    PaneRestoreStartup {
        restore_plan,
        initial_history_ansi: if has_native_agent_restore {
            None
        } else {
            history.map(|history| history.ansi.as_str())
        },
        duplicate_agent_session,
        reserved_agent_session,
    }
}

fn restore_plan_for_snapshot(
    session: &PaneAgentSessionSnapshot,
    resume_agents_on_restore: bool,
) -> Option<crate::agent_resume::AgentResumePlan> {
    if !resume_agents_on_restore {
        return None;
    }
    let persisted = persisted_agent_session_from_snapshot(session)?;
    crate::agent_resume::plan(&session.source, &session.agent, &persisted.session_ref)
}

fn persisted_agent_session_from_snapshot(
    session: &PaneAgentSessionSnapshot,
) -> Option<crate::agent_resume::PersistedAgentSession> {
    crate::agent_resume::session_ref_from_snapshot(
        &session.source,
        &session.agent,
        session.kind,
        &session.value,
    )
}

fn restored_terminal_agent_session(
    session: Option<&PaneAgentSessionSnapshot>,
    duplicate_agent_session: bool,
) -> Option<crate::agent_resume::PersistedAgentSession> {
    if duplicate_agent_session {
        return None;
    }
    session.and_then(persisted_agent_session_from_snapshot)
}

#[cfg(test)]
fn take_restore_plan_for_snapshot(
    session: &PaneAgentSessionSnapshot,
    resume_agents_on_restore: bool,
    resumed_agent_sessions: &mut HashSet<String>,
) -> Option<crate::agent_resume::AgentResumePlan> {
    restore_plan_for_snapshot(session, resume_agents_on_restore)
        .filter(|plan| resumed_agent_sessions.insert(plan.dedupe_key.clone()))
}

pub(super) fn prune_restored_node(node: Node, surviving: &HashSet<PaneId>) -> Option<Node> {
    match node {
        Node::Pane(id) => surviving.contains(&id).then_some(Node::Pane(id)),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first = prune_restored_node(*first, surviving);
            let second = prune_restored_node(*second, surviving);
            match (first, second) {
                (Some(first), Some(second)) => Some(Node::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            }
        }
    }
}

pub(super) fn resolve_restored_pane(
    saved_old_id: Option<u32>,
    id_map: &HashMap<u32, PaneId>,
    surviving: &HashSet<PaneId>,
    pane_ids: &[PaneId],
) -> Option<PaneId> {
    saved_old_id
        .and_then(|old_id| id_map.get(&old_id).copied())
        .filter(|pane_id| surviving.contains(pane_id))
        .or_else(|| pane_ids.first().copied())
}

/// Restore a layout tree, remapping every pane ID to a fresh globally unique one.
/// Returns the new tree and a map of old_raw_id → new PaneId.
pub(super) fn restore_node_remapped(snap: &LayoutSnapshot) -> (Node, HashMap<u32, PaneId>) {
    let mut id_map = HashMap::new();
    let node = remap_inner(snap, &mut id_map);
    (node, id_map)
}

fn remap_inner(snap: &LayoutSnapshot, id_map: &mut HashMap<u32, PaneId>) -> Node {
    match snap {
        LayoutSnapshot::Pane(old_id) => {
            let new_id = PaneId::alloc();
            id_map.insert(*old_id, new_id);
            Node::Pane(new_id)
        }
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first_node = remap_inner(first, id_map);
            let second_node = remap_inner(second, id_map);
            let dir = match direction {
                DirectionSnapshot::Horizontal => Direction::Horizontal,
                DirectionSnapshot::Vertical => Direction::Vertical,
            };
            Node::Split {
                direction: dir,
                ratio: *ratio,
                first: Box::new(first_node),
                second: Box::new(second_node),
            }
        }
    }
}

pub(super) fn collect_pane_ids(node: &Node) -> Vec<PaneId> {
    let mut ids = Vec::new();
    collect_ids_inner(node, &mut ids);
    ids
}

fn collect_ids_inner(node: &Node, ids: &mut Vec<PaneId>) {
    match node {
        Node::Pane(id) => ids.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids_inner(first, ids);
            collect_ids_inner(second, ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    #[cfg(windows)]
    fn test_restore_shell() -> &'static str {
        "C:\\Windows\\System32\\whoami.exe"
    }

    #[cfg(not(windows))]
    fn test_restore_shell() -> &'static str {
        "/bin/sh"
    }

    struct RemovedEnvVarsGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl RemovedEnvVarsGuard {
        fn new(keys: &[&'static str]) -> Self {
            let values = keys
                .iter()
                .map(|key| {
                    let value = std::env::var_os(key);
                    std::env::remove_var(key);
                    (*key, value)
                })
                .collect();
            Self(values)
        }
    }

    impl Drop for RemovedEnvVarsGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn capture_and_restore_node_round_trip() {
        let node = Node::Split {
            direction: Direction::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Pane(PaneId::from_raw(0))),
            second: Box::new(Node::Split {
                direction: Direction::Vertical,
                ratio: 0.3,
                first: Box::new(Node::Pane(PaneId::from_raw(1))),
                second: Box::new(Node::Pane(PaneId::from_raw(2))),
            }),
        };

        let snap = super::super::snapshot::capture_node(&node);
        let (restored, id_map) = restore_node_remapped(&snap);

        assert_eq!(id_map.len(), 3);
        let ids = collect_pane_ids(&restored);
        assert_eq!(ids.len(), 3);
        let unique: std::collections::HashSet<u32> = ids.iter().map(|id| id.raw()).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn prune_restored_node_collapses_missing_branch() {
        let keep = PaneId::from_raw(11);
        let missing = PaneId::from_raw(12);
        let node = Node::Split {
            direction: Direction::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Pane(keep)),
            second: Box::new(Node::Pane(missing)),
        };
        let surviving = std::collections::HashSet::from([keep]);

        let pruned = prune_restored_node(node, &surviving).expect("remaining pane should survive");

        assert!(matches!(pruned, Node::Pane(id) if id == keep));
    }

    #[test]
    fn resolve_restored_pane_prefers_surviving_saved_id_and_falls_back_to_first_remaining() {
        let first = PaneId::from_raw(21);
        let second = PaneId::from_raw(22);
        let id_map = HashMap::from([(0_u32, first), (1_u32, second)]);
        let surviving = std::collections::HashSet::from([first]);
        let pane_ids = vec![first];

        assert_eq!(
            resolve_restored_pane(Some(0), &id_map, &surviving, &pane_ids),
            Some(first)
        );
        assert_eq!(
            resolve_restored_pane(Some(1), &id_map, &surviving, &pane_ids),
            Some(first)
        );
    }

    #[test]
    fn restored_worktree_space_membership_drops_missing_checkout() {
        let missing =
            std::env::temp_dir().join(format!("herdr-missing-worktree-{}", std::process::id()));
        let membership = crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: missing.join("repo"),
            checkout_path: missing.join("checkout"),
            is_linked_worktree: true,
        };

        assert_eq!(restored_worktree_space_membership(Some(membership)), None);
    }

    #[test]
    fn restored_worktree_space_memberships_preserve_a_coherent_missing_group() {
        let missing = std::env::temp_dir().join(format!(
            "herdr-missing-worktree-group-{}-{}",
            std::process::id(),
            crate::terminal::TerminalId::alloc()
        ));
        let parent = crate::workspace::WorktreeSpaceMembership {
            key: "saved-key".into(),
            label: "herdr".into(),
            repo_root: missing.join("repo"),
            checkout_path: missing.join("repo"),
            is_linked_worktree: false,
        };
        let child = crate::workspace::WorktreeSpaceMembership {
            key: "saved-key".into(),
            label: "herdr".into(),
            repo_root: missing.join("repo"),
            checkout_path: missing.join("child"),
            is_linked_worktree: true,
        };

        let restored =
            restored_worktree_space_memberships(&[Some(parent.clone()), Some(child.clone())]);

        assert_eq!(restored, vec![Some(parent), Some(child)]);
    }

    #[test]
    fn restored_worktree_space_memberships_use_registry_proof_after_parent_retarget() {
        let root = std::env::temp_dir().join(format!(
            "herdr-restored-worktree-registry-{}-{}",
            std::process::id(),
            crate::terminal::TerminalId::alloc()
        ));
        let old_container = root.join("old");
        let old_parent = old_container.join("repo");
        let old_child = old_container.join("child");
        std::fs::create_dir_all(&old_container).unwrap();
        run_git(&root, &["init", "--quiet", old_parent.to_str().unwrap()]);
        run_git(
            &old_parent,
            &["config", "user.email", "herdr@example.invalid"],
        );
        run_git(&old_parent, &["config", "user.name", "Herdr Test"]);
        run_git(
            &old_parent,
            &["commit", "--quiet", "--allow-empty", "-m", "initial"],
        );
        run_git(
            &old_parent,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "child-branch",
                old_child.to_str().unwrap(),
                "HEAD",
            ],
        );
        let old_parent_identity = std::fs::canonicalize(&old_parent).unwrap();
        let old_child_identity = std::fs::canonicalize(&old_child).unwrap();
        let old_key = std::fs::canonicalize(old_parent.join(".git"))
            .unwrap()
            .display()
            .to_string();
        let child_pointer_before = std::fs::read(old_child.join(".git")).unwrap();
        let new_container = root.join("new");
        std::fs::rename(&old_container, &new_container).unwrap();
        let new_parent = new_container.join("repo");
        let new_child = new_container.join("child");
        let new_parent_identity = std::fs::canonicalize(&new_parent).unwrap();
        let registry_before = git_registry(&new_parent);
        let parent = crate::workspace::WorktreeSpaceMembership {
            key: old_key.clone(),
            label: "repo".into(),
            repo_root: new_parent_identity.clone(),
            checkout_path: new_parent_identity,
            is_linked_worktree: false,
        };
        let child = crate::workspace::WorktreeSpaceMembership {
            key: old_key,
            label: "repo".into(),
            repo_root: old_parent_identity,
            checkout_path: old_child_identity,
            is_linked_worktree: true,
        };

        let restored =
            restored_worktree_space_memberships(&[Some(parent.clone()), Some(child.clone())]);

        assert_eq!(restored, vec![Some(parent), Some(child)]);
        assert_eq!(
            std::fs::read(new_child.join(".git")).unwrap(),
            child_pointer_before
        );
        assert_eq!(git_registry(&new_parent), registry_before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restored_worktree_space_memberships_reject_live_parent_without_registry_proof() {
        let root = std::env::temp_dir().join(format!(
            "herdr-restored-worktree-wrong-parent-{}-{}",
            std::process::id(),
            crate::terminal::TerminalId::alloc()
        ));
        let live_parent = root.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "--quiet", live_parent.to_str().unwrap()]);
        let live_parent = std::fs::canonicalize(live_parent).unwrap();
        let registry_before = git_registry(&live_parent);
        let config_before = std::fs::read(live_parent.join(".git").join("config")).unwrap();
        let parent = crate::workspace::WorktreeSpaceMembership {
            key: "unrelated-saved-key".into(),
            label: "repo".into(),
            repo_root: live_parent.clone(),
            checkout_path: live_parent.clone(),
            is_linked_worktree: false,
        };
        let child = crate::workspace::WorktreeSpaceMembership {
            key: "unrelated-saved-key".into(),
            label: "repo".into(),
            repo_root: root.join("old-repo"),
            checkout_path: root.join("missing-child"),
            is_linked_worktree: true,
        };

        assert_eq!(
            restored_worktree_space_memberships(&[Some(parent), Some(child)]),
            vec![None, None]
        );
        assert_eq!(git_registry(&live_parent), registry_before);
        assert_eq!(
            std::fs::read(live_parent.join(".git").join("config")).unwrap(),
            config_before
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restored_worktree_space_memberships_drop_unrelated_and_malformed_groups() {
        let root = std::env::temp_dir().join(format!(
            "herdr-restored-worktree-invalid-{}-{}",
            std::process::id(),
            crate::terminal::TerminalId::alloc()
        ));
        let unrelated = root.join("unrelated");
        std::fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "--quiet", unrelated.to_str().unwrap()]);
        let missing_parent = root.join("missing-parent");
        let parent = crate::workspace::WorktreeSpaceMembership {
            key: "saved-key".into(),
            label: "repo".into(),
            repo_root: missing_parent.clone(),
            checkout_path: missing_parent,
            is_linked_worktree: false,
        };
        let unrelated_child = crate::workspace::WorktreeSpaceMembership {
            key: "saved-key".into(),
            label: "repo".into(),
            repo_root: root.join("missing-parent"),
            checkout_path: unrelated,
            is_linked_worktree: true,
        };
        assert_eq!(
            restored_worktree_space_memberships(&[Some(parent.clone()), Some(unrelated_child),]),
            vec![None, None]
        );

        let duplicate_parent = crate::workspace::WorktreeSpaceMembership {
            checkout_path: root.join("second-parent"),
            repo_root: root.join("second-parent"),
            ..parent.clone()
        };
        let missing_child = crate::workspace::WorktreeSpaceMembership {
            checkout_path: root.join("missing-child"),
            is_linked_worktree: true,
            ..parent.clone()
        };
        assert_eq!(
            restored_worktree_space_memberships(&[
                Some(parent),
                Some(duplicate_parent),
                Some(missing_child),
            ]),
            vec![None, None, None]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_registry(repo: &std::path::Path) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }

    #[test]
    fn restore_plan_respects_opt_in_and_allowlist() {
        let pi_session_path = test_session_path("pi-session.jsonl");
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: pi_session_path.clone(),
        };

        assert!(restore_plan_for_snapshot(&session, false).is_none());
        assert_eq!(
            restore_plan_for_snapshot(&session, true).unwrap().argv,
            vec!["pi", "--session", pi_session_path.as_str()]
        );

        let unsupported_path = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("claude-session"),
        };
        assert!(restore_plan_for_snapshot(&unsupported_path, true).is_none());
    }

    #[test]
    fn restore_plan_selection_suppresses_duplicates() {
        let pi_session_path = test_session_path("pi-session.jsonl");
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: pi_session_path.clone(),
        };
        let mut resumed = HashSet::new();

        assert!(take_restore_plan_for_snapshot(&session, false, &mut resumed).is_none());
        assert!(resumed.is_empty());

        let first = take_restore_plan_for_snapshot(&session, true, &mut resumed)
            .expect("first restore should get a plan");
        assert_eq!(
            first.argv,
            vec!["pi", "--session", pi_session_path.as_str()]
        );
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_none());
    }

    #[test]
    fn pane_restore_startup_suppresses_history_for_native_agent_resume() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
            lines: 1,
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: true,
            resumed_sessions: &mut resumed,
        };

        let startup = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(startup.restore_plan.is_some());
        assert!(startup.initial_history_ansi.is_none());
        assert!(!startup.duplicate_agent_session);
    }

    #[test]
    fn pane_restore_startup_suppresses_history_for_duplicate_native_agent_session() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
            lines: 1,
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: true,
            resumed_sessions: &mut resumed,
        };

        let first = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);
        let duplicate = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(first.restore_plan.is_some());
        assert!(first.initial_history_ansi.is_none());
        assert!(duplicate.restore_plan.is_none());
        assert!(duplicate.initial_history_ansi.is_none());
        assert!(duplicate.duplicate_agent_session);
    }

    #[test]
    fn pane_restore_startup_keeps_history_without_native_agent_resume() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
            lines: 1,
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: false,
            resumed_sessions: &mut resumed,
        };

        let startup = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(startup.restore_plan.is_none());
        assert_eq!(startup.initial_history_ansi, Some("RESTORED_HISTORY\r\n"));
        assert!(!startup.duplicate_agent_session);
        assert!(resumed.is_empty());
    }

    #[test]
    fn machine_pane_keeps_history_and_does_not_claim_native_agent_session() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "MACHINE_HISTORY\r\n".into(),
            lines: 1,
        };
        let mut resumed = HashSet::new();

        let machine_startup = {
            let mut agent_restore = AgentRestoreState {
                enabled: native_agent_restore_enabled(true, true),
                resumed_sessions: &mut resumed,
            };
            pane_restore_startup(Some(&session), Some(&history), &mut agent_restore)
        };

        assert!(machine_startup.restore_plan.is_none());
        assert_eq!(
            machine_startup.initial_history_ansi,
            Some("MACHINE_HISTORY\r\n")
        );
        assert!(machine_startup.reserved_agent_session.is_none());
        assert!(resumed.is_empty());

        let local_startup = {
            let mut agent_restore = AgentRestoreState {
                enabled: native_agent_restore_enabled(true, false),
                resumed_sessions: &mut resumed,
            };
            pane_restore_startup(Some(&session), Some(&history), &mut agent_restore)
        };

        assert!(
            local_startup.restore_plan.is_some(),
            "the machine pane must leave the session available to a later local pane"
        );
        assert!(local_startup.initial_history_ansi.is_none());
        assert!(local_startup.reserved_agent_session.is_some());
        assert_eq!(resumed.len(), 1);
    }

    #[test]
    fn machine_workspace_restore_survives_when_home_is_unavailable() {
        let _env_lock = crate::config::test_config_env_lock().lock().unwrap();
        #[cfg(windows)]
        let _env_restore = RemovedEnvVarsGuard::new(&["HOME", "USERPROFILE"]);
        #[cfg(not(windows))]
        let _env_restore = RemovedEnvVarsGuard::new(&["HOME"]);

        let persisted_identity_cwd = std::env::current_dir().unwrap();
        let snapshot = WorkspaceSnapshot {
            id: Some("machine-workspace".into()),
            custom_name: None,
            machine: Some("missing-machine".into()),
            identity_cwd: persisted_identity_cwd.clone(),
            worktree_space: None,
            parent_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs: vec![TabSnapshot {
                custom_name: None,
                layout: LayoutSnapshot::Pane(0),
                panes: HashMap::from([(
                    0,
                    super::super::snapshot::PaneSnapshot {
                        cwd: persisted_identity_cwd.clone(),
                        label: None,
                        agent_name: None,
                        managed_agent_kind: None,
                        agent_session: None,
                        launch_argv: None,
                    },
                )]),
                zoomed: false,
                focused: Some(0),
                root_pane: Some(0),
            }],
            active_tab: 0,
        };
        let (events, _event_rx) = mpsc::channel(4);
        let runtime_context = RestoreRuntimeContext {
            scrollback_limit_bytes: 0,
            shell_config: crate::pane::PaneShellConfig::new(
                test_restore_shell(),
                crate::config::ShellModeConfig::NonLogin,
            ),
            resume_agents_on_restore: true,
            machines: &[],
            events,
            render_notify: Arc::new(Notify::new()),
            render_dirty: Arc::new(RenderSignal::new()),
        };
        let mut resumed_agent_sessions = HashSet::new();
        let mut imported_panes = HashMap::new();

        let (restored, failed_imports) = restore_workspace(
            &snapshot,
            None,
            None,
            24,
            80,
            &runtime_context,
            &mut resumed_agent_sessions,
            &mut imported_panes,
        );

        assert_eq!(failed_imports, 0);
        let (workspace, terminals, runtimes) =
            restored.expect("missing local home must not drop a machine workspace");
        assert_eq!(workspace.identity_cwd, persisted_identity_cwd);
        assert_eq!(workspace.machine_name(), Some("missing-machine"));
        assert_eq!(terminals.len(), 1);
        assert!(runtimes.is_empty());
    }

    #[test]
    fn restore_rehydrates_agent_session_metadata() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:hermes".into(),
            agent: "hermes".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Id,
            value: "hermes-session".into(),
        };

        let preserved = restored_terminal_agent_session(Some(&session), false)
            .expect("restore should preserve metadata");
        assert_eq!(preserved.source, "herdr:hermes");
        assert_eq!(preserved.agent, "hermes");
        assert_eq!(preserved.session_ref.value, "hermes-session");
    }

    #[test]
    fn restore_does_not_rehydrate_duplicate_agent_session_metadata() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let mut resumed = HashSet::new();
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_some());
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_none());

        assert!(restored_terminal_agent_session(Some(&session), true).is_none());
    }

    #[tokio::test]
    async fn restore_carries_persisted_agent_session_metadata() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                machine: None,
                identity_cwd: cwd.clone(),
                worktree_space: None,
                parent_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneSnapshot {
                            cwd,
                            label: Some("reviewer".into()),
                            agent_name: Some("reviewer".into()),
                            managed_agent_kind: Some("opencode".into()),
                            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                                source: "herdr:opencode".into(),
                                agent: "opencode".into(),
                                kind: crate::agent_resume::AgentSessionRefKind::Id,
                                value: "opencode-session".into(),
                            }),
                            launch_argv: None,
                        },
                    )]),
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (_workspaces, terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(false, &[]),
            events,
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        );

        let terminal = terminals
            .values()
            .next()
            .expect("restored terminal should exist");
        assert!(
            !terminal.respawn_shell_on_exit,
            "agent sessions should not use native restore lifecycle when resume_agents_on_restore is disabled"
        );
        assert_eq!(terminal.agent_name, None);
        assert_eq!(terminal.manual_label.as_deref(), Some("reviewer"));
        let session = terminal
            .persisted_agent_session
            .as_ref()
            .expect("persisted agent session should survive restore");
        assert_eq!(session.source, "herdr:opencode");
        assert_eq!(session.agent, "opencode");
        assert_eq!(session.session_ref.value, "opencode-session");
    }

    #[tokio::test]
    async fn machine_workspace_restore_respawns_ssh_from_live_registry() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: Some("build".into()),
                machine: Some("build".into()),
                identity_cwd: cwd.clone(),
                worktree_space: None,
                parent_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneSnapshot {
                            cwd,
                            label: None,
                            agent_name: None,
                            managed_agent_kind: None,
                            agent_session: None,
                            launch_argv: None,
                        },
                    )]),
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        };
        let machines = [crate::config::MachineConfig {
            name: "build".into(),
            target: "-V".into(),
            cwd: None,
        }];
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, terminals, runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(false, &machines),
            events,
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        );

        assert_eq!(workspaces[0].machine_name(), Some("build"));
        assert_eq!(
            terminals
                .values()
                .next()
                .unwrap()
                .launch_argv
                .as_ref()
                .unwrap(),
            &["ssh".to_string(), "-t".to_string(), "-V".to_string()]
        );
        for runtime in runtimes.into_values() {
            runtime.shutdown();
        }

        let (events, _event_rx) = mpsc::channel(4);
        let (workspaces, terminals, runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(false, &[]),
            events,
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        );
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].machine_name(), Some("build"));
        assert_eq!(terminals.len(), 1);
        assert_eq!(
            terminals
                .values()
                .next()
                .and_then(|terminal| terminal.manual_label.as_deref()),
            Some("machine \"build\" is not configured")
        );
        assert!(runtimes.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn machine_workspace_restore_preserves_pane_when_ssh_spawn_fails() {
        let cwd = std::env::current_dir().unwrap();
        let tab = TabSnapshot {
            custom_name: Some("build".into()),
            layout: LayoutSnapshot::Pane(0),
            panes: HashMap::from([(
                0,
                super::super::snapshot::PaneSnapshot {
                    cwd: cwd.clone(),
                    label: None,
                    agent_name: None,
                    managed_agent_kind: None,
                    agent_session: None,
                    launch_argv: None,
                },
            )]),
            zoomed: false,
            focused: Some(0),
            root_pane: Some(0),
        };
        let (events, _event_rx) = mpsc::channel(4);
        let runtime_context = RestoreRuntimeContext {
            scrollback_limit_bytes: 0,
            shell_config: crate::pane::PaneShellConfig::new(
                test_restore_shell(),
                crate::config::ShellModeConfig::NonLogin,
            ),
            resume_agents_on_restore: false,
            machines: &[],
            events,
            render_notify: Arc::new(Notify::new()),
            render_dirty: Arc::new(RenderSignal::new()),
        };
        let argv = vec!["/definitely/missing/herdr-ssh".to_string()];
        let mut resumed_agent_sessions = HashSet::new();
        let mut imported_panes = HashMap::new();
        let public_ids = HashMap::from([(0, "w1:p1".to_string())]);

        let (restored, failed_imports) = restore_tab(
            &tab,
            None,
            1,
            "w1",
            24,
            80,
            &runtime_context,
            MachineRestoreContext {
                argv: Some(&argv),
                error: None,
                is_machine: true,
                identity_cwd: Some(&cwd),
            },
            &mut resumed_agent_sessions,
            &mut imported_panes,
            &public_ids,
        );

        assert_eq!(failed_imports, 0);
        let (_tab, terminals, runtimes, _reverse_ids) =
            restored.expect("machine tab must survive a transient spawn failure");
        assert_eq!(terminals.len(), 1);
        assert!(runtimes.is_empty());
        assert!(
            terminals[0]
                .manual_label
                .as_deref()
                .is_some_and(|label| label.starts_with("machine pane unavailable:")),
            "spawn failure should be visible on an inert pane"
        );
    }

    #[tokio::test]
    async fn restore_preserves_public_id_mapping_after_pane_id_remap() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                machine: None,
                identity_cwd: cwd.clone(),
                worktree_space: None,
                parent_space: None,
                public_pane_numbers: HashMap::from([(10, 1), (20, 3)]),
                next_public_pane_number: 4,
                public_tab_numbers: vec![5],
                next_public_tab_number: 6,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Split {
                        direction: super::super::snapshot::DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(10)),
                        second: Box::new(LayoutSnapshot::Pane(20)),
                    },
                    panes: HashMap::from([
                        (
                            10,
                            super::super::snapshot::PaneSnapshot {
                                cwd: cwd.clone(),
                                label: None,
                                agent_name: None,
                                managed_agent_kind: None,
                                agent_session: None,
                                launch_argv: None,
                            },
                        ),
                        (
                            20,
                            super::super::snapshot::PaneSnapshot {
                                cwd: cwd.clone(),
                                label: None,
                                agent_name: None,
                                managed_agent_kind: None,
                                agent_session: None,
                                launch_argv: None,
                            },
                        ),
                    ]),
                    zoomed: false,
                    focused: Some(10),
                    root_pane: Some(10),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, _terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(false, &[]),
            events,
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        let mut public_numbers: Vec<_> = workspace.public_pane_numbers.values().copied().collect();
        public_numbers.sort_unstable();
        assert_eq!(public_numbers, vec![1, 3]);
        assert_eq!(workspace.next_public_pane_number, 4);
        assert_eq!(workspace.tabs[0].number, 5);
        assert_eq!(workspace.next_public_tab_number, 6);
    }

    #[tokio::test]
    async fn cold_restore_with_gapped_public_tab_numbers_drops_unmanaged_agent_name() {
        let cwd = std::env::current_dir().unwrap();
        let pane_snap = |id: &str| {
            (
                id.parse::<u32>().unwrap(),
                super::super::snapshot::PaneSnapshot {
                    cwd: cwd.clone(),
                    label: None,
                    agent_name: None,
                    managed_agent_kind: None,
                    agent_session: None,
                    launch_argv: None,
                },
            )
        };
        let final_pane = super::super::snapshot::PaneSnapshot {
            cwd: cwd.clone(),
            label: Some("planner".into()),
            agent_name: Some("planner".into()),
            managed_agent_kind: None,
            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: "herdr:codex".into(),
                agent: "codex".into(),
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: "codex-session".into(),
            }),
            launch_argv: None,
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                machine: None,
                identity_cwd: cwd.clone(),
                worktree_space: None,
                parent_space: None,
                public_pane_numbers: HashMap::from([(10, 1), (11, 2), (12, 3), (13, 4)]),
                next_public_pane_number: 5,
                public_tab_numbers: vec![1, 3, 4, 5],
                next_public_tab_number: 6,
                tabs: vec![
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(10),
                        panes: HashMap::from([pane_snap("10")]),
                        zoomed: false,
                        focused: Some(10),
                        root_pane: Some(10),
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(11),
                        panes: HashMap::from([pane_snap("11")]),
                        zoomed: false,
                        focused: Some(11),
                        root_pane: Some(11),
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(12),
                        panes: HashMap::from([pane_snap("12")]),
                        zoomed: false,
                        focused: Some(12),
                        root_pane: Some(12),
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(13),
                        panes: HashMap::from([(13, final_pane)]),
                        zoomed: false,
                        focused: Some(13),
                        root_pane: Some(13),
                    },
                ],
                active_tab: 3,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(false, &[]),
            events,
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        assert_eq!(workspace.active_tab, 3);
        assert_eq!(workspace.tabs[3].number, 5);
        let agent_pane = workspace.tabs[3].root_pane;
        let terminal_id = &workspace.tabs[3].panes[&agent_pane].attached_terminal_id;
        assert!(terminals[terminal_id].agent_name.is_none());
        assert_eq!(terminals[terminal_id].managed_agent_kind(), None);
        assert!(workspace
            .pane_details(&terminals)
            .into_iter()
            .all(|detail| detail.pane_id != agent_pane));
    }

    #[test]
    fn legacy_restore_precomputes_missing_public_pane_numbers() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = WorkspaceSnapshot {
            id: Some("w1".into()),
            custom_name: None,
            machine: None,
            identity_cwd: cwd,
            worktree_space: None,
            parent_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs: vec![TabSnapshot {
                custom_name: None,
                layout: LayoutSnapshot::Split {
                    direction: super::super::snapshot::DirectionSnapshot::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Pane(10)),
                    second: Box::new(LayoutSnapshot::Pane(20)),
                },
                panes: HashMap::new(),
                zoomed: false,
                focused: Some(10),
                root_pane: Some(10),
            }],
            active_tab: 0,
        };
        let mut next_public_pane_number = 1;

        let public_numbers =
            migrated_public_pane_numbers_by_old_raw(&snapshot, &mut next_public_pane_number);

        assert_eq!(public_numbers, HashMap::from([(10, 1), (20, 2)]));
        assert_eq!(next_public_pane_number, 3);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn native_agent_restore_defers_runtime_launch() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                machine: None,
                identity_cwd: cwd.clone(),
                worktree_space: None,
                parent_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneSnapshot {
                            cwd,
                            label: None,
                            agent_name: None,
                            managed_agent_kind: None,
                            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                                source: "herdr:codex".into(),
                                agent: "codex".into(),
                                kind: crate::agent_resume::AgentSessionRefKind::Id,
                                value: "codex-session".into(),
                            }),
                            launch_argv: None,
                        },
                    )]),
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (_workspaces, terminals, runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(true, &[]),
            events,
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        );

        let terminal = terminals
            .values()
            .next()
            .expect("native agent restore should create terminal state");
        assert!(
            terminal.pending_agent_resume_plan.is_some(),
            "restored native agent panes should defer resume until client terminal context is known"
        );
        assert!(
            !terminal.respawn_shell_on_exit,
            "deferred agent resume should not use native restore lifecycle before launch"
        );
        assert!(
            runtimes.is_empty(),
            "native agent restore should not spawn a fallback-size runtime during snapshot restore"
        );
        let mut imports = HashMap::new();
        let (_handoff_workspaces, handoff_terminals, handoff_runtimes) = restore_handoff(
            &snapshot,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            &[],
            &mut imports,
            mpsc::channel(4).0,
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        )
        .expect("handoff restore should preserve pending native agent resume");
        let handoff_terminal = handoff_terminals
            .values()
            .next()
            .expect("handoff restore should create terminal state");
        assert!(
            handoff_terminal.pending_agent_resume_plan.is_some(),
            "handoff restore should preserve pending native agent resume intent"
        );
        assert!(
            handoff_runtimes.is_empty(),
            "handoff restore should not replace pending native agent resume with a shell runtime"
        );
    }

    #[tokio::test]
    async fn restore_seeds_saved_pane_history_into_runtime() {
        let (snapshot, history) = snapshot_with_saved_pane_history();
        let (events, _events_rx) = mpsc::channel(8);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(RenderSignal::new());

        let (_workspaces, _terminals, runtimes) = restore(
            &snapshot,
            Some(&history),
            5,
            40,
            4096,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(false, &[]),
            events,
            render_notify,
            render_dirty,
        );
        let runtime = runtimes
            .values()
            .next()
            .expect("restored runtime should exist");

        let restored_text = runtime.recent_unwrapped_text(10);
        assert!(
            restored_text.contains("RESTORED_HISTORY 👨‍👩‍👧 LINK"),
            "styled Unicode and hyperlink text should survive history replay"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = runtime.try_send_bytes(bytes::Bytes::from_static(b"exit\n"));
    }

    #[tokio::test]
    async fn restore_without_history_snapshot_keeps_pane_contents_empty() {
        let (snapshot, _history) = snapshot_with_saved_pane_history();
        let (events, _events_rx) = mpsc::channel(8);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(RenderSignal::new());

        let (_workspaces, _terminals, runtimes) = restore(
            &snapshot,
            None,
            5,
            40,
            4096,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            RestorePolicy::new(false, &[]),
            events,
            render_notify,
            render_dirty,
        );
        let runtime = runtimes
            .values()
            .next()
            .expect("restored runtime should exist");

        assert!(
            !runtime
                .recent_unwrapped_text(10)
                .contains("RESTORED_HISTORY"),
            "pane history should not restore unless a history snapshot is supplied"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = runtime.try_send_bytes(bytes::Bytes::from_static(b"exit\n"));
    }

    fn snapshot_with_saved_pane_history() -> (SessionSnapshot, SessionHistorySnapshot) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut panes = HashMap::new();
        panes.insert(
            0,
            super::super::snapshot::PaneSnapshot {
                cwd: cwd.clone(),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );
        let history = SessionHistorySnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![super::super::snapshot::TabHistorySnapshot {
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneHistorySnapshot {
                            ansi: concat!(
                                "\x1b[31mRESTORED_HISTORY 👨‍👩‍👧\x1b[0m ",
                                "\x1b]8;;https://example.com\x1b\\LINK\x1b]8;;\x1b\\\r\n"
                            )
                            .to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                machine: None,
                identity_cwd: cwd,
                worktree_space: None,
                parent_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes,
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: Default::default(),
        };
        (snapshot, history)
    }
}
