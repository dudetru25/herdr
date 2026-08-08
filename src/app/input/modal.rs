use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
#[cfg(test)]
use ratatui::layout::Direction;
use ratatui::layout::Rect;

use crate::{
    app::{
        state::{
            AppState, ContextMenuKind, ContextMenuState, MachineCreateField, MachineCreateState,
            MenuListState, Mode, NavigatorStateFilter,
        },
        App,
    },
    input::TerminalKey,
    layout::NavDirection,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModalAction {
    Continue,
    Save,
    Clear,
    Cancel,
    Confirm,
    Apply,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModalKeyBinding {
    Enter,
    Esc,
    CtrlC,
}

impl ModalKeyBinding {
    fn matches(self, key: &KeyEvent) -> bool {
        match self {
            Self::Enter => key.code == KeyCode::Enter,
            Self::Esc => key.code == KeyCode::Esc,
            Self::CtrlC => {
                key.code == KeyCode::Char('c')
                    && key.modifiers == crossterm::event::KeyModifiers::CONTROL
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ModalActionSpec<A> {
    pub action: A,
    pub bindings: &'static [ModalKeyBinding],
}

pub(super) fn modal_action_from_key<A: Copy>(
    key: &KeyEvent,
    specs: &[ModalActionSpec<A>],
) -> Option<A> {
    specs
        .iter()
        .find(|spec| spec.bindings.iter().any(|binding| binding.matches(key)))
        .map(|spec| spec.action)
}

pub(super) fn modal_action_from_buttons<A: Copy>(
    col: u16,
    row: u16,
    buttons: &[(Rect, A)],
) -> Option<A> {
    buttons.iter().find_map(|(rect, action)| {
        (col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height)
            .then_some(*action)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalMenuAction {
    Detach,
    WhatsNew,
    Keybinds,
    ReloadConfig,
    Settings,
}

pub(super) fn global_menu_actions(state: &AppState) -> Vec<GlobalMenuAction> {
    let mut actions = vec![
        GlobalMenuAction::Settings,
        GlobalMenuAction::Keybinds,
        GlobalMenuAction::ReloadConfig,
    ];
    if state.update_available.is_some() || state.latest_release_notes_available {
        actions.push(GlobalMenuAction::WhatsNew);
    }
    actions.push(GlobalMenuAction::Detach);
    actions
}

pub(super) fn open_global_menu(state: &mut AppState) {
    state.global_menu = MenuListState::new(0);
    state.mode = Mode::GlobalMenu;
}

pub(super) fn open_keybind_help(state: &mut AppState) {
    state.keybind_help.scroll = 0;
    state.keybind_help.query.clear();
    state.keybind_help.search_focused = false;
    state.mode = Mode::KeybindHelp;
}

fn open_update_release_notes(state: &mut AppState) {
    let Some(notes) = crate::release_notes::load_latest() else {
        return;
    };

    state.release_notes = Some(crate::app::state::ReleaseNotesState {
        version: notes.version,
        body: notes.body,
        scroll: 0,
        preview: notes.preview,
    });
    state.mode = Mode::ReleaseNotes;
}

pub(super) fn request_detach(state: &mut AppState) {
    if state.detach_exits {
        state.should_quit = true;
    } else {
        state.detach_requested = true;
    }
}

pub(super) fn apply_global_menu_action(state: &mut AppState, action: GlobalMenuAction) {
    match action {
        GlobalMenuAction::Detach => {
            leave_modal(state);
            request_detach(state);
        }
        GlobalMenuAction::WhatsNew => open_update_release_notes(state),
        GlobalMenuAction::Keybinds => open_keybind_help(state),
        GlobalMenuAction::ReloadConfig => {
            state.request_reload_config = true;
            leave_modal(state);
        }
        GlobalMenuAction::Settings => super::settings::open_settings(state),
    }
}

pub(crate) fn handle_global_menu_key(state: &mut AppState, key: KeyEvent) {
    let actions = global_menu_actions(state);
    match key.code {
        KeyCode::Esc => leave_modal(state),
        KeyCode::Up | KeyCode::Char('k') => state.global_menu.move_prev(),
        KeyCode::Down | KeyCode::Char('j') => state.global_menu.move_next(actions.len()),
        KeyCode::Enter => {
            if let Some(action) = actions.get(state.global_menu.highlighted).copied() {
                apply_global_menu_action(state, action);
            }
        }
        _ => {}
    }
}

pub(crate) fn handle_navigator_key(
    state: &mut AppState,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
    key: KeyEvent,
) {
    if state.navigator.search_focused {
        match key.code {
            KeyCode::Esc => {
                state.navigator.search_focused = false;
            }
            KeyCode::Enter => {
                state.accept_navigator_selection_from(terminal_runtimes);
            }
            KeyCode::Backspace => {
                state.navigator.state_filter = None;
                state.navigator.query.pop();
                state.select_first_navigator_match_from(terminal_runtimes);
            }
            KeyCode::Up => state.move_navigator_selection_from(terminal_runtimes, -1),
            KeyCode::Down => state.move_navigator_selection_from(terminal_runtimes, 1),
            KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                state.move_navigator_selection_from(terminal_runtimes, 1)
            }
            KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                state.move_navigator_selection_from(terminal_runtimes, -1)
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                state.navigator.query.clear();
                state.navigator.state_filter = None;
                state.clamp_navigator_selection_from(terminal_runtimes);
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                insert_navigator_search_text(state, terminal_runtimes, &c.to_string());
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            leave_modal(state);
        }
        KeyCode::Enter => {
            state.accept_navigator_selection_from(terminal_runtimes);
        }
        KeyCode::Char('/') => {
            state.navigator.state_filter = None;
            state.navigator.search_focused = true;
            state.clamp_navigator_selection_from(terminal_runtimes);
        }
        KeyCode::Backspace if state.navigator.state_filter.is_some() => {
            state.navigator.state_filter = None;
            state.clamp_navigator_selection_from(terminal_runtimes);
        }
        KeyCode::Char('a') if key.modifiers.is_empty() => {
            state.navigator.query.clear();
            state.navigator.state_filter = None;
            state.clamp_navigator_selection_from(terminal_runtimes);
        }
        KeyCode::Char('b') if key.modifiers.is_empty() => {
            state.navigator.query.clear();
            state.navigator.state_filter = Some(NavigatorStateFilter::Blocked);
            state.select_first_navigator_match_from(terminal_runtimes);
        }
        KeyCode::Char('w') if key.modifiers.is_empty() => {
            state.navigator.query.clear();
            state.navigator.state_filter = Some(NavigatorStateFilter::Working);
            state.select_first_navigator_match_from(terminal_runtimes);
        }
        KeyCode::Char('i') if key.modifiers.is_empty() => {
            state.navigator.query.clear();
            state.navigator.state_filter = Some(NavigatorStateFilter::Idle);
            state.select_first_navigator_match_from(terminal_runtimes);
        }
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            state.navigator.query.clear();
            state.navigator.state_filter = Some(NavigatorStateFilter::Done);
            state.select_first_navigator_match_from(terminal_runtimes);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.move_navigator_selection_from(terminal_runtimes, 1)
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.move_navigator_selection_from(terminal_runtimes, -1)
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => state
            .move_navigator_selection_by_lines_from(
                terminal_runtimes,
                (state.navigator_body_rect().height / 2).max(1) as isize,
            ),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => state
            .move_navigator_selection_by_lines_from(
                terminal_runtimes,
                -((state.navigator_body_rect().height / 2).max(1) as isize),
            ),
        KeyCode::Char(' ') => state.toggle_selected_navigator_workspace_from(terminal_runtimes),
        KeyCode::Home => {
            state.navigator.selected = 0;
            state.ensure_navigator_selection_visible_from(terminal_runtimes);
        }
        KeyCode::End | KeyCode::Char('G') => {
            state.navigator.selected = state
                .navigator_rows_from(terminal_runtimes)
                .len()
                .saturating_sub(1);
            state.ensure_navigator_selection_visible_from(terminal_runtimes);
        }
        _ => {}
    }
}

pub(crate) fn insert_navigator_search_text(
    state: &mut AppState,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
    text: &str,
) {
    if !state.navigator.search_focused {
        return;
    }
    state.navigator.state_filter = None;
    state.navigator.query.push_str(text);
    state.select_first_navigator_match_from(terminal_runtimes);
}

pub(crate) fn insert_keybind_help_query_text(state: &mut AppState, text: &str) {
    if !state.keybind_help.search_focused {
        return;
    }
    state
        .keybind_help
        .query
        .extend(text.chars().filter(|ch| !ch.is_control()));
    state.keybind_help.scroll = 0;
}

pub(super) fn keybind_help_back(state: &mut AppState) {
    if state.keybind_help.search_focused {
        state.keybind_help.query.clear();
        state.keybind_help.search_focused = false;
        state.keybind_help.scroll = 0;
    } else {
        leave_modal(state);
    }
}

pub(crate) fn handle_keybind_help_key(state: &mut AppState, key: TerminalKey) {
    if state.keybind_help.search_focused {
        let text_char = keybind_help_text_char(key.clone());
        match key.code {
            KeyCode::Up => state.scroll_keybind_help(-1),
            KeyCode::Down => state.scroll_keybind_help(1),
            KeyCode::PageUp => state.scroll_keybind_help(-8),
            KeyCode::PageDown => state.scroll_keybind_help(8),
            KeyCode::Home => state.keybind_help.scroll = 0,
            KeyCode::End => state.keybind_help.scroll = state.keybind_help_max_scroll(),
            KeyCode::Backspace => {
                state.keybind_help.query.pop();
                state.keybind_help.scroll = 0;
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                state.keybind_help.query.clear();
                state.keybind_help.scroll = 0;
            }
            KeyCode::Esc => keybind_help_back(state),
            KeyCode::Enter => leave_modal(state),
            _ => {
                if let Some(character) = text_char {
                    insert_keybind_help_query_text(state, &character.to_string());
                }
            }
        }
        return;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => state.scroll_keybind_help(-1),
        KeyCode::Down | KeyCode::Char('j') => state.scroll_keybind_help(1),
        KeyCode::PageUp => state.scroll_keybind_help(-8),
        KeyCode::PageDown => state.scroll_keybind_help(8),
        KeyCode::Home => state.keybind_help.scroll = 0,
        KeyCode::End => state.keybind_help.scroll = state.keybind_help_max_scroll(),
        _ if keybind_help_text_char(key.clone()) == Some('/') => {
            state.keybind_help.search_focused = true;
            state.keybind_help.scroll = 0;
        }
        KeyCode::Esc => keybind_help_back(state),
        KeyCode::Enter => leave_modal(state),
        _ if keybind_help_text_char(key.clone()) == Some('?') => leave_modal(state),
        _ => {}
    }
}

fn keybind_help_text_char(key: TerminalKey) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    if let Some(character) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(character);
    }
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    Some(character)
}

pub(super) fn open_rename_workspace(
    state: &mut AppState,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
    ws_idx: usize,
) {
    state.pending_workspace_create_cwd = None;
    state.selected = ws_idx;
    state.rename_pane_target = None;
    state.name_input =
        state.workspaces[ws_idx].display_name_from(&state.terminals, terminal_runtimes);
    state.name_input_replace_on_type = false;
    state.mode = Mode::RenameWorkspace;
}

pub(crate) fn open_new_workspace_dialog(state: &mut AppState, cwd: std::path::PathBuf) {
    let suggested_name = crate::workspace::derive_label_from_cwd(&cwd);
    state.pending_workspace_create_cwd = Some(cwd);
    state.rename_pane_target = None;
    state.name_input = suggested_name;
    state.name_input_replace_on_type = true;
    state.mode = Mode::RenameWorkspace;
}

pub(super) fn open_rename_active_tab(state: &mut AppState, replace_on_type: bool) {
    state.pending_workspace_create_cwd = None;
    state.rename_pane_target = None;
    if let Some(ws) = state.active.and_then(|i| state.workspaces.get(i)) {
        if let Some(name) = ws.active_tab_display_name() {
            state.name_input = name;
            state.name_input_replace_on_type = replace_on_type;
            state.mode = Mode::RenameTab;
        }
    }
}

pub(super) fn open_rename_pane(state: &mut AppState, pane_id: crate::layout::PaneId) {
    let Some(ws) = state.active.and_then(|i| state.workspaces.get(i)) else {
        return;
    };
    let Some(pane) = ws.pane_state(pane_id) else {
        return;
    };
    let terminal = state.terminals.get(&pane.attached_terminal_id);
    state.pending_workspace_create_cwd = None;
    state.rename_pane_target = Some(pane_id);
    state.name_input = terminal
        .and_then(|t| t.manual_label.clone())
        .unwrap_or_default();
    state.name_input_replace_on_type = terminal.and_then(|t| t.manual_label.as_ref()).is_none();
    state.mode = Mode::RenamePane;
}

fn workspace_create_label(input: &str, suggested_name: &str) -> Option<String> {
    let name = input.trim();
    (!name.is_empty() && name != suggested_name).then(|| name.to_string())
}

pub(super) fn leave_modal(state: &mut AppState) {
    if state.active.is_some() {
        state.mode = Mode::Terminal;
    } else {
        state.mode = Mode::Navigate;
    }
}

pub(super) fn open_add_remote_machine(state: &mut AppState) {
    state.context_menu = None;
    state.machine_create = Some(MachineCreateState::default());
    state.mode = Mode::AddRemoteMachine;
}

fn cancel_add_remote_machine(state: &mut AppState) {
    state.machine_create = None;
    leave_modal(state);
}

fn machine_create_input_mut(create: &mut MachineCreateState) -> Option<&mut String> {
    match create.focused {
        MachineCreateField::Name => Some(&mut create.name),
        MachineCreateField::Target => Some(&mut create.target),
        MachineCreateField::Cwd => Some(&mut create.cwd),
        MachineCreateField::Import => None,
    }
}

fn insert_machine_create_text(state: &mut AppState, text: &str) -> bool {
    let Some(create) = state.machine_create.as_mut() else {
        return false;
    };
    let Some(input) = machine_create_input_mut(create) else {
        return false;
    };
    input.push_str(text);
    create.error = None;
    true
}

fn machine_add_response(response: &str) -> Result<String, String> {
    if let Ok(success) = serde_json::from_str::<crate::api::schema::SuccessResponse>(response) {
        return match success.result {
            crate::api::schema::ResponseResult::MachineAdded { machine } => Ok(machine.name),
            _ => Err("Herdr returned an unexpected response while adding the machine.".into()),
        };
    }
    if let Ok(error) = serde_json::from_str::<crate::api::schema::ErrorResponse>(response) {
        return Err(error.error.message);
    }
    Err("Herdr could not read the response while adding the machine.".into())
}

fn machine_ssh_hosts_response(
    response: &str,
) -> Result<Vec<crate::api::schema::SshHostInfo>, String> {
    if let Ok(success) = serde_json::from_str::<crate::api::schema::SuccessResponse>(response) {
        return match success.result {
            crate::api::schema::ResponseResult::MachineSshHosts { hosts } => Ok(hosts),
            _ => Err("Herdr returned an unexpected response while listing SSH hosts.".into()),
        };
    }
    if let Ok(error) = serde_json::from_str::<crate::api::schema::ErrorResponse>(response) {
        return Err(error.error.message);
    }
    Err("Herdr could not read the response while listing SSH hosts.".into())
}

fn machine_import_response(
    response: &str,
) -> Result<Vec<crate::api::schema::MachineImportOutcome>, String> {
    if let Ok(success) = serde_json::from_str::<crate::api::schema::SuccessResponse>(response) {
        return match success.result {
            crate::api::schema::ResponseResult::MachineImported { outcomes } => Ok(outcomes),
            _ => Err("Herdr returned an unexpected response while importing SSH hosts.".into()),
        };
    }
    if let Ok(error) = serde_json::from_str::<crate::api::schema::ErrorResponse>(response) {
        return Err(error.error.message);
    }
    Err("Herdr could not read the response while importing SSH hosts.".into())
}

pub(super) const ONBOARDING_WELCOME_ACTIONS: &[ModalActionSpec<ModalAction>] = &[ModalActionSpec {
    action: ModalAction::Continue,
    bindings: &[ModalKeyBinding::Enter],
}];

pub(super) const RELEASE_NOTES_ACTIONS: &[ModalActionSpec<ModalAction>] = &[ModalActionSpec {
    action: ModalAction::Close,
    bindings: &[ModalKeyBinding::Enter, ModalKeyBinding::Esc],
}];

pub(super) const RENAME_ACTIONS: &[ModalActionSpec<ModalAction>] = &[
    ModalActionSpec {
        action: ModalAction::Save,
        bindings: &[ModalKeyBinding::Enter],
    },
    ModalActionSpec {
        action: ModalAction::Clear,
        bindings: &[ModalKeyBinding::CtrlC],
    },
    ModalActionSpec {
        action: ModalAction::Cancel,
        bindings: &[ModalKeyBinding::Esc],
    },
];

pub(super) const CONFIRM_CLOSE_ACTIONS: &[ModalActionSpec<ModalAction>] = &[
    ModalActionSpec {
        action: ModalAction::Confirm,
        bindings: &[ModalKeyBinding::Enter],
    },
    ModalActionSpec {
        action: ModalAction::Cancel,
        bindings: &[ModalKeyBinding::Esc],
    },
];

pub(super) const SETTINGS_ACTIONS: &[ModalActionSpec<ModalAction>] = &[
    ModalActionSpec {
        action: ModalAction::Apply,
        bindings: &[ModalKeyBinding::Enter],
    },
    ModalActionSpec {
        action: ModalAction::Close,
        bindings: &[ModalKeyBinding::Esc],
    },
];

#[cfg(test)]
pub(super) fn apply_rename_action(state: &mut AppState, action: ModalAction) {
    match action {
        ModalAction::Save => {
            let new_name = if state.name_input.trim().is_empty() {
                state.name_input.clone()
            } else {
                state.name_input.trim().to_string()
            };
            match state.mode {
                Mode::RenameWorkspace
                    if state.pending_workspace_create_cwd.is_none()
                        && !state.workspaces.is_empty()
                        && !new_name.is_empty() =>
                {
                    let workspace_id = state.workspaces[state.selected].id.clone();
                    state.workspaces[state.selected].set_custom_name(new_name);
                    crate::logging::workspace_renamed(&workspace_id);
                    state.mark_session_dirty();
                }
                Mode::RenameTab => {
                    if let Some(ws_idx) = state.active {
                        if let Some(ws) = state.workspaces.get_mut(ws_idx) {
                            let workspace_id = ws.id.clone();
                            let active_tab = ws.active_tab;
                            let keep_auto_name = ws
                                .tabs
                                .get(active_tab)
                                .is_some_and(|tab| tab.is_auto_named())
                                && ws
                                    .tab_display_name(active_tab)
                                    .is_some_and(|name| new_name == name);
                            if let Some(tab) = ws.active_tab_mut() {
                                if !new_name.is_empty() && !keep_auto_name {
                                    tab.set_custom_name(new_name);
                                    let tab_id = ws
                                        .public_tab_number(active_tab)
                                        .map(|number| {
                                            crate::workspace::public_tab_id_for_number(
                                                &workspace_id,
                                                number,
                                            )
                                        })
                                        .unwrap_or_else(|| workspace_id.clone());
                                    crate::logging::tab_renamed(&workspace_id, &tab_id);
                                    state.mark_session_dirty();
                                }
                            }
                        }
                    }
                }
                Mode::RenamePane => {
                    if let (Some(ws_idx), Some(pane_id)) = (state.active, state.rename_pane_target)
                    {
                        if let Some(ws) = state.workspaces.get(ws_idx) {
                            if let Some(pane) = ws.pane_state(pane_id) {
                                let terminal_id = pane.attached_terminal_id.clone();
                                if let Some(terminal) = state.terminals.get_mut(&terminal_id) {
                                    terminal.set_manual_label(new_name);
                                    state.mark_session_dirty();
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            state.pending_workspace_create_cwd = None;
            state.rename_pane_target = None;
            state.name_input.clear();
            state.name_input_replace_on_type = false;
            leave_modal(state);
        }
        ModalAction::Clear => {
            state.name_input.clear();
            state.name_input_replace_on_type = false;
        }
        ModalAction::Cancel => {
            state.pending_workspace_create_cwd = None;
            state.rename_pane_target = None;
            state.name_input.clear();
            state.name_input_replace_on_type = false;
            leave_modal(state);
        }
        _ => {}
    }
}

fn clear_rename_input(state: &mut AppState) {
    state.name_input.clear();
    state.name_input_replace_on_type = false;
}

pub(crate) fn insert_rename_input_text(state: &mut AppState, text: &str) {
    if state.name_input_replace_on_type {
        clear_rename_input(state);
    }
    state.name_input.push_str(text);
}

fn delete_rename_input_char(state: &mut AppState) {
    if state.name_input_replace_on_type {
        clear_rename_input(state);
    } else {
        state.name_input.pop();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameWordDeleteClass {
    Word,
    Separator,
}

fn rename_word_delete_class(ch: char) -> RenameWordDeleteClass {
    if ch.is_alphanumeric() || ch == '_' {
        RenameWordDeleteClass::Word
    } else {
        RenameWordDeleteClass::Separator
    }
}

fn delete_rename_input_word(state: &mut AppState) {
    if state.name_input_replace_on_type {
        clear_rename_input(state);
        return;
    }

    while state
        .name_input
        .chars()
        .last()
        .is_some_and(char::is_whitespace)
    {
        state.name_input.pop();
    }

    let Some(class) = state
        .name_input
        .chars()
        .last()
        .map(rename_word_delete_class)
    else {
        return;
    };

    while state
        .name_input
        .chars()
        .last()
        .is_some_and(|ch| !ch.is_whitespace() && rename_word_delete_class(ch) == class)
    {
        state.name_input.pop();
    }
}

fn handle_rename_edit_key(state: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            clear_rename_input(state);
        }
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::SUPER) => {
            clear_rename_input(state);
        }
        KeyCode::Backspace
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            delete_rename_input_word(state);
        }
        KeyCode::Char('h' | 'w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            delete_rename_input_word(state);
        }
        KeyCode::Backspace => delete_rename_input_char(state),
        KeyCode::Char(c) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
            insert_rename_input_text(state, &c.to_string());
        }
        _ => {}
    }
}

#[cfg(test)]
pub(crate) fn handle_rename_key(state: &mut AppState, key: KeyEvent) {
    if let Some(action) = modal_action_from_key(&key, RENAME_ACTIONS) {
        apply_rename_action(state, action);
        return;
    }

    handle_rename_edit_key(state, key);
}

#[cfg(test)]
pub(crate) fn handle_resize_key(state: &mut AppState, raw_key: TerminalKey) {
    let key = raw_key.as_key_event();
    if key.code == KeyCode::Esc
        || key.code == KeyCode::Enter
        || state.keybinds.resize_mode.matches_prefix_key(&raw_key)
        || state.keybinds.resize_mode.matches_direct_key(&raw_key)
    {
        if state.active.is_some() {
            state.mode = Mode::Terminal;
        } else {
            state.mode = Mode::Navigate;
        }
        return;
    }

    match key.code {
        KeyCode::Char('h') | KeyCode::Left => state.resize_pane(NavDirection::Left),
        KeyCode::Char('l') | KeyCode::Right => state.resize_pane(NavDirection::Right),
        KeyCode::Char('j') | KeyCode::Down => state.resize_pane(NavDirection::Down),
        KeyCode::Char('k') | KeyCode::Up => state.resize_pane(NavDirection::Up),
        _ => {}
    }
}

pub(super) fn open_confirm_close(state: &mut AppState) {
    state.mode = Mode::ConfirmClose;
}

#[cfg(test)]
pub(super) fn confirm_close_accept(state: &mut AppState) {
    state.close_selected_workspace();
    if state.workspaces.is_empty() {
        state.mode = Mode::Navigate;
    } else {
        state.mode = Mode::Terminal;
    }
}

pub(super) fn confirm_close_cancel(state: &mut AppState) {
    state.mode = Mode::Navigate;
}

#[cfg(test)]
pub(crate) fn handle_confirm_close_key(state: &mut AppState, key: KeyEvent) {
    match modal_action_from_key(&key, CONFIRM_CLOSE_ACTIONS) {
        Some(ModalAction::Confirm) => confirm_close_accept(state),
        Some(ModalAction::Cancel) => confirm_close_cancel(state),
        _ => {}
    }
}

#[cfg(test)]
pub(super) fn apply_context_menu_action(
    state: &mut AppState,
    terminal_runtimes: &mut crate::terminal::TerminalRuntimeRegistry,
    menu: ContextMenuState,
    idx: usize,
) {
    if let ContextMenuKind::TabCreateTarget { .. } = &menu.kind {
        if idx == 0 {
            state.request_new_tab = true;
        }
        leave_modal(state);
        return;
    }
    if let ContextMenuKind::WorkspaceCreateTarget { machines } = &menu.kind {
        if idx == 0 {
            state.request_new_workspace = true;
        } else if let Some(machine) = machines.get(idx - 1) {
            state.request_new_workspace_machine = Some(machine.clone());
        } else if idx == machines.len() + 1 + usize::from(machines.is_empty()) {
            open_add_remote_machine(state);
            return;
        }
        leave_modal(state);
        return;
    }
    let item = menu.items().get(idx).map(|item| (*item).to_string());
    match (menu.kind, item.as_deref()) {
        (ContextMenuKind::GitWorkspace { ws_idx, .. }, Some("New worktree")) => {
            state.request_new_linked_worktree = Some(ws_idx);
            leave_modal(state);
        }
        (ContextMenuKind::GitWorkspace { ws_idx, .. }, Some("Delete worktree checkout...")) => {
            state.request_remove_linked_worktree = Some(ws_idx);
            leave_modal(state);
        }
        (ContextMenuKind::GitWorkspace { ws_idx, .. }, Some("Open worktree...")) => {
            state.request_open_existing_worktree = Some(ws_idx);
            leave_modal(state);
        }
        (
            ContextMenuKind::Workspace { ws_idx, .. }
            | ContextMenuKind::GitWorkspace { ws_idx, .. },
            Some("Become parent space"),
        ) => {
            state.request_parent_space_action =
                Some((ws_idx, crate::app::state::ParentSpaceAction::Become));
            leave_modal(state);
        }
        (
            ContextMenuKind::Workspace { ws_idx, .. }
            | ContextMenuKind::GitWorkspace { ws_idx, .. },
            Some("Re-scan sub-spaces"),
        ) => {
            state.request_parent_space_action =
                Some((ws_idx, crate::app::state::ParentSpaceAction::Rescan));
            leave_modal(state);
        }
        (
            ContextMenuKind::Workspace { ws_idx, .. }
            | ContextMenuKind::GitWorkspace { ws_idx, .. },
            Some("Stop being parent"),
        ) => {
            state.request_parent_space_action =
                Some((ws_idx, crate::app::state::ParentSpaceAction::Stop));
            leave_modal(state);
        }
        (
            ContextMenuKind::GitWorkspace {
                ws_idx, collapsed, ..
            },
            Some("Collapse" | "Expand"),
        ) => {
            if let Some(key) = state
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.worktree_space())
                .map(|space| space.key.clone())
            {
                if collapsed {
                    state.collapsed_space_keys.remove(&key);
                } else {
                    state.collapsed_space_keys.insert(key);
                }
                state.mark_session_dirty();
            }
            leave_modal(state);
        }
        (
            ContextMenuKind::Workspace { ws_idx, .. }
            | ContextMenuKind::GitWorkspace { ws_idx, .. },
            Some("Rename"),
        ) => {
            open_rename_workspace(state, terminal_runtimes, ws_idx);
        }
        (
            ContextMenuKind::Workspace { ws_idx, .. }
            | ContextMenuKind::GitWorkspace { ws_idx, .. },
            Some("Close" | "Close group"),
        ) => {
            state.selected = ws_idx;
            if state.confirm_close {
                open_confirm_close(state);
            } else {
                state.close_selected_workspace();
                state.mode = Mode::Navigate;
            }
        }
        (ContextMenuKind::Tab { ws_idx, tab_idx }, Some("New tab")) => {
            state.selected = ws_idx;
            state.active = Some(ws_idx);
            state.switch_tab(tab_idx);
            state.request_new_tab = true;
            leave_modal(state);
        }
        (ContextMenuKind::Tab { ws_idx, tab_idx }, Some("Rename")) => {
            state.selected = ws_idx;
            state.active = Some(ws_idx);
            state.switch_tab(tab_idx);
            open_rename_active_tab(state, false);
        }
        (ContextMenuKind::Tab { ws_idx, tab_idx }, Some("Close")) => {
            state.selected = ws_idx;
            state.active = Some(ws_idx);
            state.switch_tab(tab_idx);
            if !state.close_tab() {
                state.mode = if state.active.is_some() {
                    Mode::Terminal
                } else {
                    Mode::Navigate
                };
            }
        }
        (ContextMenuKind::Pane { pane_id, .. }, Some("Rename pane")) => {
            open_rename_pane(state, pane_id);
        }
        (
            ContextMenuKind::Pane {
                ws_idx, pane_id, ..
            },
            Some("Clear pane name"),
        ) => {
            if let Some(ws) = state.workspaces.get(ws_idx) {
                if let Some(pane) = ws.pane_state(pane_id) {
                    let terminal_id = pane.attached_terminal_id.clone();
                    if let Some(terminal) = state.terminals.get_mut(&terminal_id) {
                        terminal.clear_manual_label();
                        state.mark_session_dirty();
                    }
                }
            }
            state.mode = Mode::Terminal;
        }
        (
            ContextMenuKind::Pane {
                ws_idx,
                tab_idx,
                pane_id,
                source_pane_id,
                ..
            },
            Some("Swap with focused pane"),
        ) => {
            if let Some(source_pane_id) = source_pane_id {
                state.selected = ws_idx;
                state.active = Some(ws_idx);
                state.switch_tab(tab_idx);
                if let Some(tab) = state
                    .workspaces
                    .get_mut(ws_idx)
                    .and_then(|ws| ws.tabs.get_mut(tab_idx))
                {
                    if tab.layout.swap_panes(source_pane_id, pane_id) {
                        tab.layout.focus_pane(source_pane_id);
                        state.mark_session_dirty();
                    }
                }
            }
            state.mode = Mode::Terminal;
        }
        (
            ContextMenuKind::Pane {
                ws_idx,
                tab_idx,
                pane_id,
                ..
            },
            Some("Split right"),
        ) => {
            state.selected = ws_idx;
            state.active = Some(ws_idx);
            state.switch_tab(tab_idx);
            state.focus_pane_in_workspace(ws_idx, pane_id);
            state.split_pane(terminal_runtimes, Direction::Horizontal);
            state.mode = Mode::Terminal;
        }
        (
            ContextMenuKind::Pane {
                ws_idx,
                tab_idx,
                pane_id,
                ..
            },
            Some("Split down"),
        ) => {
            state.selected = ws_idx;
            state.active = Some(ws_idx);
            state.switch_tab(tab_idx);
            state.focus_pane_in_workspace(ws_idx, pane_id);
            state.split_pane(terminal_runtimes, Direction::Vertical);
            state.mode = Mode::Terminal;
        }
        (
            ContextMenuKind::Pane {
                ws_idx,
                tab_idx,
                pane_id,
                ..
            },
            Some("Zoom"),
        ) => {
            state.selected = ws_idx;
            state.active = Some(ws_idx);
            state.switch_tab(tab_idx);
            state.focus_pane_in_workspace(ws_idx, pane_id);
            state.toggle_zoom();
            state.mode = Mode::Terminal;
        }
        (
            ContextMenuKind::Pane {
                ws_idx,
                tab_idx,
                pane_id,
                ..
            },
            Some("Close pane"),
        ) => {
            state.selected = ws_idx;
            state.active = Some(ws_idx);
            state.switch_tab(tab_idx);
            state.focus_pane_in_workspace(ws_idx, pane_id);
            if !state.close_pane() {
                state.mode = if state.active.is_some() {
                    Mode::Terminal
                } else {
                    Mode::Navigate
                };
            }
        }
        _ => leave_modal(state),
    }
}

#[cfg(test)]
pub(crate) fn handle_context_menu_key(
    state: &mut AppState,
    terminal_runtimes: &mut crate::terminal::TerminalRuntimeRegistry,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Esc => {
            state.context_menu = None;
            leave_modal(state);
        }
        KeyCode::Up => {
            if let Some(menu) = &mut state.context_menu {
                menu.list.move_prev();
            }
        }
        KeyCode::Down => {
            if let Some(menu) = &mut state.context_menu {
                menu.list.move_next(menu.items().len());
            }
        }
        KeyCode::Enter => {
            if let Some(menu) = state.context_menu.take() {
                let idx = menu.list.highlighted;
                apply_context_menu_action(state, terminal_runtimes, menu, idx);
            }
        }
        _ => {}
    }
}

impl App {
    pub(crate) fn handle_add_remote_machine_key(&mut self, key: KeyEvent) {
        if self
            .state
            .machine_create
            .as_ref()
            .is_some_and(|create| create.import.is_some())
        {
            match key.code {
                KeyCode::Esc => {
                    if let Some(create) = self.state.machine_create.as_mut() {
                        create.import = None;
                    }
                }
                KeyCode::Enter => self.submit_machine_import(),
                KeyCode::Up | KeyCode::BackTab => {
                    if let Some(import) = self
                        .state
                        .machine_create
                        .as_mut()
                        .and_then(|create| create.import.as_mut())
                    {
                        import.move_previous();
                    }
                }
                KeyCode::Down | KeyCode::Tab => {
                    if let Some(import) = self
                        .state
                        .machine_create
                        .as_mut()
                        .and_then(|create| create.import.as_mut())
                    {
                        import.move_next();
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some(import) = self
                        .state
                        .machine_create
                        .as_mut()
                        .and_then(|create| create.import.as_mut())
                    {
                        import.toggle(import.highlighted);
                    }
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc => cancel_add_remote_machine(&mut self.state),
            KeyCode::Enter => {
                if self
                    .state
                    .machine_create
                    .as_ref()
                    .is_some_and(|create| create.focused == MachineCreateField::Import)
                {
                    self.open_machine_import();
                } else {
                    self.submit_add_remote_machine();
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(create) = self.state.machine_create.as_mut() {
                    create.focused = create.focused.next();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(create) = self.state.machine_create.as_mut() {
                    create.focused = create.focused.previous();
                }
            }
            KeyCode::Backspace => {
                if let Some(create) = self.state.machine_create.as_mut() {
                    if let Some(input) = machine_create_input_mut(create) {
                        input.pop();
                    }
                    create.error = None;
                }
            }
            KeyCode::Char(character)
                if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() =>
            {
                insert_machine_create_text(&mut self.state, &character.to_string());
            }
            _ => {}
        }
    }

    pub(super) fn submit_add_remote_machine(&mut self) {
        let Some(create) = self.state.machine_create.as_ref() else {
            return;
        };
        let params = crate::api::schema::MachineAddParams {
            name: create.name.trim().to_string(),
            target: create.target.trim().to_string(),
            cwd: (!create.cwd.trim().is_empty()).then(|| create.cwd.trim().to_string()),
        };
        let response = self.runtime_machine_add("tui.machine.add", params);
        self.apply_machine_add_response(&response);
    }

    pub(super) fn open_machine_import(&mut self) {
        let response = self.runtime_machine_ssh_hosts("tui.machine.ssh_hosts");
        self.apply_machine_ssh_hosts_response(&response);
    }

    fn apply_machine_ssh_hosts_response(&mut self, response: &str) {
        let Some(create) = self.state.machine_create.as_mut() else {
            return;
        };
        match machine_ssh_hosts_response(response) {
            Ok(hosts) => {
                create.import = Some(crate::app::state::MachineImportState::from_hosts(hosts));
                create.error = None;
            }
            Err(message) => create.error = Some(message),
        }
    }

    pub(super) fn submit_machine_import(&mut self) {
        let Some(import) = self
            .state
            .machine_create
            .as_ref()
            .and_then(|create| create.import.as_ref())
        else {
            return;
        };
        let aliases = import.selected_aliases();
        if aliases.is_empty() {
            if let Some(import) = self
                .state
                .machine_create
                .as_mut()
                .and_then(|create| create.import.as_mut())
            {
                import.error = Some("Select at least one SSH host to import.".into());
            }
            return;
        }

        let response = self.runtime_machine_import("tui.machine.import", aliases);
        self.apply_machine_import_response(&response);
    }

    fn apply_machine_import_response(&mut self, response: &str) {
        let Some(import) = self
            .state
            .machine_create
            .as_mut()
            .and_then(|create| create.import.as_mut())
        else {
            return;
        };
        match machine_import_response(response) {
            Ok(outcomes) => {
                import.error = None;
                for outcome in outcomes {
                    let alias = match &outcome {
                        crate::api::schema::MachineImportOutcome::Added { alias }
                        | crate::api::schema::MachineImportOutcome::AlreadyExists { alias }
                        | crate::api::schema::MachineImportOutcome::Failed { alias, .. } => alias,
                    };
                    let Some(host) = import.hosts.iter_mut().find(|host| host.alias == *alias)
                    else {
                        continue;
                    };
                    host.selected = false;
                    if matches!(
                        outcome,
                        crate::api::schema::MachineImportOutcome::Added { .. }
                            | crate::api::schema::MachineImportOutcome::AlreadyExists { .. }
                    ) {
                        host.already_configured = true;
                    }
                    host.outcome = Some(outcome);
                }
            }
            Err(message) => import.error = Some(message),
        }
    }

    fn apply_machine_add_response(&mut self, response: &str) {
        match machine_add_response(response) {
            Ok(machine_name) => {
                self.state.machine_create = None;
                self.state.request_new_workspace_machine = Some(machine_name);
                leave_modal(&mut self.state);
            }
            Err(message) => {
                if let Some(create) = self.state.machine_create.as_mut() {
                    create.error = Some(message);
                }
            }
        }
    }

    pub(crate) fn insert_add_remote_machine_text(&mut self, text: &str) -> bool {
        insert_machine_create_text(&mut self.state, text)
    }

    pub(crate) fn handle_rename_key_via_api(&mut self, key: KeyEvent) {
        if let Some(action) = modal_action_from_key(&key, RENAME_ACTIONS) {
            self.apply_rename_mouse_action_via_api(action);
            return;
        }

        handle_rename_edit_key(&mut self.state, key);
    }

    fn save_rename_modal_via_api(&mut self) {
        let new_name = if self.state.name_input.trim().is_empty() {
            self.state.name_input.clone()
        } else {
            self.state.name_input.trim().to_string()
        };

        match self.state.mode {
            Mode::RenameWorkspace => {
                if let Some(cwd) = self.state.pending_workspace_create_cwd.take() {
                    let suggested_name = crate::workspace::derive_label_from_cwd(&cwd);
                    let label = workspace_create_label(&new_name, &suggested_name);
                    let response = self.runtime_workspace_create(
                        "tui.workspace.create_named",
                        crate::api::schema::WorkspaceCreateParams {
                            cwd: Some(cwd.display().to_string()),
                            machine: None,
                            focus: true,
                            label,
                            env: Default::default(),
                        },
                    );
                    self.apply_workspace_create_response(&response);
                } else if !self.state.workspaces.is_empty() && !new_name.is_empty() {
                    let workspace_id = self.public_workspace_id(self.state.selected);
                    self.runtime_workspace_rename(
                        "tui.workspace.rename",
                        crate::api::schema::WorkspaceRenameParams {
                            workspace_id,
                            label: new_name,
                        },
                    );
                }
            }
            Mode::RenameTab if !new_name.is_empty() => {
                let Some(ws_idx) = self.state.active else {
                    cancel_rename_modal(&mut self.state);
                    return;
                };
                let tab_idx = self.state.workspaces[ws_idx].active_tab;
                let keep_auto_name = self.state.workspaces[ws_idx]
                    .tabs
                    .get(tab_idx)
                    .is_some_and(|tab| tab.is_auto_named())
                    && self.state.workspaces[ws_idx]
                        .tab_display_name(tab_idx)
                        .is_some_and(|name| new_name == name);
                if !keep_auto_name {
                    if let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) {
                        self.runtime_tab_rename(
                            "tui.tab.rename",
                            crate::api::schema::TabRenameParams {
                                tab_id,
                                label: new_name,
                            },
                        );
                    }
                }
            }
            Mode::RenamePane => {
                if let (Some(ws_idx), Some(pane_id)) =
                    (self.state.active, self.state.rename_pane_target)
                {
                    if let Some(pane_id) = self.public_pane_id(ws_idx, pane_id) {
                        self.runtime_pane_rename(
                            "tui.pane.rename",
                            crate::api::schema::PaneRenameParams {
                                pane_id,
                                label: Some(new_name),
                            },
                        );
                    }
                }
            }
            _ => {}
        }

        cancel_rename_modal(&mut self.state);
    }

    pub(super) fn apply_rename_mouse_action_via_api(&mut self, action: ModalAction) {
        match action {
            ModalAction::Save => self.save_rename_modal_via_api(),
            ModalAction::Clear => {
                self.state.name_input.clear();
                self.state.name_input_replace_on_type = false;
            }
            ModalAction::Cancel => cancel_rename_modal(&mut self.state),
            _ => {}
        }
    }

    pub(super) fn confirm_close_accept_via_api(&mut self) {
        let ws_idx = self.state.selected;
        if ws_idx < self.state.workspaces.len() {
            self.close_workspace_idx_via_api(ws_idx);
        }
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    pub(crate) fn handle_resize_key_via_api(&mut self, raw_key: TerminalKey) {
        let key = raw_key.as_key_event();
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Enter
            || self.state.keybinds.resize_mode.matches_prefix_key(&raw_key)
            || self.state.keybinds.resize_mode.matches_direct_key(&raw_key)
        {
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
            return;
        }

        let direction = match key.code {
            KeyCode::Char('h') | KeyCode::Left => Some(NavDirection::Left),
            KeyCode::Char('l') | KeyCode::Right => Some(NavDirection::Right),
            KeyCode::Char('j') | KeyCode::Down => Some(NavDirection::Down),
            KeyCode::Char('k') | KeyCode::Up => Some(NavDirection::Up),
            _ => None,
        };
        if let Some(direction) = direction {
            self.runtime_pane_resize(
                "tui.pane.resize",
                crate::api::schema::PaneResizeParams {
                    pane_id: None,
                    direction: super::navigate::api_pane_direction(direction),
                    amount: None,
                },
            );
        }
    }

    pub(crate) fn handle_confirm_close_key_via_api(&mut self, key: KeyEvent) {
        match modal_action_from_key(&key, CONFIRM_CLOSE_ACTIONS) {
            Some(ModalAction::Confirm) => {
                self.confirm_close_accept_via_api();
            }
            Some(ModalAction::Cancel) => confirm_close_cancel(&mut self.state),
            _ => {}
        }
    }

    pub(crate) fn handle_context_menu_key_via_api(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state.context_menu = None;
                leave_modal(&mut self.state);
            }
            KeyCode::Up => {
                if let Some(menu) = &mut self.state.context_menu {
                    menu.list.move_prev();
                }
            }
            KeyCode::Down => {
                if let Some(menu) = &mut self.state.context_menu {
                    menu.list.move_next(menu.items().len());
                }
            }
            KeyCode::Enter => {
                if let Some(menu) = self.state.context_menu.take() {
                    let idx = menu.list.highlighted;
                    self.apply_context_menu_action_via_api(menu, idx);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn apply_context_menu_action_via_api(&mut self, menu: ContextMenuState, idx: usize) {
        if let ContextMenuKind::TabCreateTarget { .. } = &menu.kind {
            if idx == 0 {
                self.runtime_tab_create(
                    "tui.tab.create",
                    crate::api::schema::TabCreateParams {
                        workspace_id: None,
                        cwd: None,
                        focus: true,
                        label: None,
                        env: Default::default(),
                    },
                );
            }
            leave_modal(&mut self.state);
            return;
        }
        if let ContextMenuKind::WorkspaceCreateTarget { machines } = &menu.kind {
            if idx == 0 {
                self.begin_tui_local_workspace_create("tui.workspace.create_local");
            } else if let Some(machine) = machines.get(idx - 1) {
                self.state.request_new_workspace_machine = Some(machine.clone());
                leave_modal(&mut self.state);
            } else if idx == machines.len() + 1 + usize::from(machines.is_empty()) {
                open_add_remote_machine(&mut self.state);
            } else {
                leave_modal(&mut self.state);
            }
            return;
        }
        let item = menu.items().get(idx).map(|item| (*item).to_string());
        match (menu.kind, item.as_deref()) {
            (ContextMenuKind::GitWorkspace { ws_idx, .. }, Some("New worktree")) => {
                self.state.request_new_linked_worktree = Some(ws_idx);
                leave_modal(&mut self.state);
            }
            (ContextMenuKind::GitWorkspace { ws_idx, .. }, Some("Delete worktree checkout...")) => {
                self.state.request_remove_linked_worktree = Some(ws_idx);
                leave_modal(&mut self.state);
            }
            (ContextMenuKind::GitWorkspace { ws_idx, .. }, Some("Open worktree...")) => {
                self.state.request_open_existing_worktree = Some(ws_idx);
                leave_modal(&mut self.state);
            }
            (
                ContextMenuKind::Workspace { ws_idx, .. }
                | ContextMenuKind::GitWorkspace { ws_idx, .. },
                Some("Become parent space"),
            ) => {
                self.state.request_parent_space_action =
                    Some((ws_idx, crate::app::state::ParentSpaceAction::Become));
                leave_modal(&mut self.state);
            }
            (
                ContextMenuKind::Workspace { ws_idx, .. }
                | ContextMenuKind::GitWorkspace { ws_idx, .. },
                Some("Re-scan sub-spaces"),
            ) => {
                self.state.request_parent_space_action =
                    Some((ws_idx, crate::app::state::ParentSpaceAction::Rescan));
                leave_modal(&mut self.state);
            }
            (
                ContextMenuKind::Workspace { ws_idx, .. }
                | ContextMenuKind::GitWorkspace { ws_idx, .. },
                Some("Stop being parent"),
            ) => {
                self.state.request_parent_space_action =
                    Some((ws_idx, crate::app::state::ParentSpaceAction::Stop));
                leave_modal(&mut self.state);
            }
            (
                ContextMenuKind::GitWorkspace {
                    ws_idx, collapsed, ..
                },
                Some("Collapse" | "Expand"),
            ) => {
                if let Some(key) = self
                    .state
                    .workspaces
                    .get(ws_idx)
                    .and_then(|ws| ws.worktree_space())
                    .map(|space| space.key.clone())
                {
                    if collapsed {
                        self.state.collapsed_space_keys.remove(&key);
                    } else {
                        self.state.collapsed_space_keys.insert(key);
                    }
                    self.state.mark_session_dirty();
                }
                leave_modal(&mut self.state);
            }
            (
                ContextMenuKind::Workspace { ws_idx, .. }
                | ContextMenuKind::GitWorkspace { ws_idx, .. },
                Some("Rename"),
            ) => open_rename_workspace(&mut self.state, &self.terminal_runtimes, ws_idx),
            (
                ContextMenuKind::Workspace { ws_idx, .. }
                | ContextMenuKind::GitWorkspace { ws_idx, .. },
                Some("Close" | "Close group"),
            ) => {
                self.state.selected = ws_idx;
                if self.state.confirm_close {
                    open_confirm_close(&mut self.state);
                } else {
                    self.close_workspace_idx_via_api(ws_idx);
                    self.state.mode = Mode::Navigate;
                }
            }
            (ContextMenuKind::Tab { ws_idx, tab_idx }, Some("New tab")) => {
                self.focus_workspace_idx_via_api(ws_idx);
                self.focus_tab_idx_via_api(tab_idx);
                self.runtime_tab_create(
                    "tui.context_menu.tab.create",
                    crate::api::schema::TabCreateParams {
                        workspace_id: None,
                        cwd: None,
                        focus: true,
                        label: None,
                        env: Default::default(),
                    },
                );
                leave_modal(&mut self.state);
            }
            (ContextMenuKind::Tab { ws_idx, tab_idx }, Some("Rename")) => {
                self.focus_workspace_idx_via_api(ws_idx);
                self.focus_tab_idx_via_api(tab_idx);
                open_rename_active_tab(&mut self.state, false);
            }
            (ContextMenuKind::Tab { ws_idx, tab_idx }, Some("Close")) => {
                self.focus_workspace_idx_via_api(ws_idx);
                self.focus_tab_idx_via_api(tab_idx);
                if !self.close_active_tab_via_api_requires_confirmation() {
                    leave_modal(&mut self.state);
                }
            }
            (ContextMenuKind::Pane { pane_id, .. }, Some("Rename pane")) => {
                open_rename_pane(&mut self.state, pane_id);
            }
            (
                ContextMenuKind::Pane {
                    ws_idx, pane_id, ..
                },
                Some("Clear pane name"),
            ) => {
                if let Some(pane_id) = self.public_pane_id(ws_idx, pane_id) {
                    self.runtime_pane_rename(
                        "tui.pane.clear_name",
                        crate::api::schema::PaneRenameParams {
                            pane_id,
                            label: None,
                        },
                    );
                }
                self.state.mode = Mode::Terminal;
            }
            (
                ContextMenuKind::Pane {
                    ws_idx,
                    pane_id,
                    source_pane_id: Some(source_pane_id),
                    ..
                },
                Some("Swap with focused pane"),
            ) => {
                let source_public_id = self.public_pane_id(ws_idx, source_pane_id);
                let target_public_id = self.public_pane_id(ws_idx, pane_id);
                if let (Some(source_public_id), Some(target_public_id)) =
                    (source_public_id, target_public_id)
                {
                    self.runtime_pane_swap(
                        "tui.pane.swap_exact",
                        crate::api::schema::PaneSwapParams {
                            pane_id: None,
                            direction: None,
                            source_pane_id: Some(source_public_id),
                            target_pane_id: Some(target_public_id),
                        },
                    );
                    self.focus_pane_internal_via_api(ws_idx, source_pane_id);
                }
                self.state.mode = Mode::Terminal;
            }
            (
                ContextMenuKind::Pane {
                    ws_idx, pane_id, ..
                },
                Some("Split right"),
            ) => {
                self.focus_pane_internal_via_api(ws_idx, pane_id);
                self.split_focused_pane_via_api(crate::api::schema::SplitDirection::Right);
                self.state.mode = Mode::Terminal;
            }
            (
                ContextMenuKind::Pane {
                    ws_idx, pane_id, ..
                },
                Some("Split down"),
            ) => {
                self.focus_pane_internal_via_api(ws_idx, pane_id);
                self.split_focused_pane_via_api(crate::api::schema::SplitDirection::Down);
                self.state.mode = Mode::Terminal;
            }
            (
                ContextMenuKind::Pane {
                    ws_idx, pane_id, ..
                },
                Some("Zoom"),
            ) => {
                self.focus_pane_internal_via_api(ws_idx, pane_id);
                self.zoom_focused_pane_via_api();
                self.state.mode = Mode::Terminal;
            }
            (
                ContextMenuKind::Pane {
                    ws_idx, pane_id, ..
                },
                Some("Close pane"),
            ) => {
                self.focus_pane_internal_via_api(ws_idx, pane_id);
                if !self.close_focused_pane_via_api_requires_confirmation() {
                    self.state.mode = if self.state.active.is_some() {
                        Mode::Terminal
                    } else {
                        Mode::Navigate
                    };
                }
            }
            _ => leave_modal(&mut self.state),
        }
    }
}

fn cancel_rename_modal(state: &mut AppState) {
    state.pending_workspace_create_cwd = None;
    state.rename_pane_target = None;
    state.name_input.clear();
    state.name_input_replace_on_type = false;
    leave_modal(state);
}

impl AppState {
    pub(super) fn global_menu_item_at(&self, col: u16, row: u16) -> Option<GlobalMenuAction> {
        let rect = self.global_menu_rect();
        if col <= rect.x
            || col >= rect.x + rect.width.saturating_sub(1)
            || row <= rect.y
            || row >= rect.y + rect.height.saturating_sub(1)
        {
            return None;
        }
        let idx = (row - rect.y - 1) as usize;
        global_menu_actions(self).get(idx).copied()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use super::super::{capture_snapshot, state_with_workspaces};
    use super::*;
    use crate::workspace::Workspace;

    fn config_env_lock() -> &'static std::sync::Mutex<()> {
        crate::config::test_config_env_lock()
    }

    fn temp_config_path(name: &str) -> std::path::PathBuf {
        let unique = format!(
            "herdr-modal-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("config.toml")
    }

    fn app_with_test_workspaces(names: &[&str]) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        app.state.ensure_test_terminals();
        app.state.active = (!app.state.workspaces.is_empty()).then_some(0);
        app.state.selected = 0;
        app
    }

    #[test]
    fn workspace_create_label_preserves_auto_name_for_suggestion_or_blank() {
        assert_eq!(workspace_create_label("project", "project"), None);
        assert_eq!(workspace_create_label("", "project"), None);
        assert_eq!(workspace_create_label("   ", "project"), None);
        assert_eq!(
            workspace_create_label("  logs  ", "project").as_deref(),
            Some("logs")
        );
    }

    #[tokio::test]
    async fn named_local_workspace_creation_error_is_visible() {
        let mut app = app_with_test_workspaces(&[]);
        let missing_shell =
            std::env::temp_dir().join(format!("herdr-missing-named-shell-{}", std::process::id()));
        let _ = std::fs::remove_file(&missing_shell);
        app.state.prompt_new_workspace_name = true;
        app.state.default_shell = missing_shell.display().to_string();

        app.begin_tui_workspace_create("test.workspace.create");
        let menu = app.state.context_menu.take().unwrap();
        app.apply_context_menu_action_via_api(menu, 0);
        app.handle_rename_key_via_api(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));

        assert!(app.state.workspaces.is_empty());
        let toast = app.state.toast.as_ref().unwrap();
        assert_eq!(toast.kind, crate::app::state::ToastKind::NeedsAttention);
        assert_eq!(toast.title, "workspace creation failed");
        assert!(!toast.context.is_empty());
        assert!(app.toast_deadline.is_some());
    }

    #[test]
    fn machine_workspace_picker_selection_queues_registry_name() {
        let mut state = AppState::test_new();
        state.mode = Mode::ContextMenu;
        let menu = ContextMenuState {
            kind: ContextMenuKind::WorkspaceCreateTarget {
                machines: vec!["build".into(), "prod".into()],
            },
            x: 0,
            y: 0,
            list: MenuListState::new(2),
        };
        let mut runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        apply_context_menu_action(&mut state, &mut runtimes, menu, 2);

        assert_eq!(state.request_new_workspace_machine.as_deref(), Some("prod"));
        assert_eq!(state.mode, Mode::Navigate);
    }

    #[test]
    fn machine_workspace_picker_add_selection_opens_form() {
        let mut state = AppState::test_new();
        state.mode = Mode::ContextMenu;
        let menu = ContextMenuState {
            kind: ContextMenuKind::WorkspaceCreateTarget {
                machines: vec!["build".into()],
            },
            x: 0,
            y: 0,
            list: MenuListState::new(2),
        };
        let mut runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        apply_context_menu_action(&mut state, &mut runtimes, menu, 2);

        assert_eq!(state.mode, Mode::AddRemoteMachine);
        assert_eq!(state.machine_create, Some(MachineCreateState::default()));
        assert!(state.request_new_workspace_machine.is_none());
    }

    #[test]
    fn add_remote_machine_form_edits_navigates_and_cancels() {
        let mut app = app_with_test_workspaces(&[]);
        open_add_remote_machine(&mut app.state);

        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        app.insert_add_remote_machine_text("dev@example.com");
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        app.insert_add_remote_machine_text("~/src");

        let create = app.state.machine_create.as_ref().unwrap();
        assert_eq!(create.name, "d");
        assert_eq!(create.target, "dev@example.com");
        assert_eq!(create.cwd, "~/src");
        assert_eq!(create.focused, MachineCreateField::Cwd);

        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(
            app.state.machine_create.as_ref().unwrap().focused,
            MachineCreateField::Import
        );
        assert!(!app.insert_add_remote_machine_text("ignored"));

        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(
            app.state.machine_create.as_ref().unwrap().focused,
            MachineCreateField::Cwd
        );
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(
            app.state.machine_create.as_ref().unwrap().focused,
            MachineCreateField::Target
        );

        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.state.mode, Mode::Navigate);
        assert!(app.state.machine_create.is_none());
    }

    #[test]
    fn successful_machine_add_response_queues_workspace_once_and_closes_form() {
        let mut app = app_with_test_workspaces(&[]);
        open_add_remote_machine(&mut app.state);
        let response = serde_json::to_string(&crate::api::schema::SuccessResponse {
            id: "tui.machine.add".into(),
            result: crate::api::schema::ResponseResult::MachineAdded {
                machine: crate::api::schema::MachineInfo {
                    name: "dev".into(),
                    target: "dev@example.com".into(),
                    cwd: None,
                },
            },
        })
        .unwrap();

        app.apply_machine_add_response(&response);

        assert_eq!(
            app.state.request_new_workspace_machine.as_deref(),
            Some("dev")
        );
        assert!(app.state.machine_create.is_none());
        assert_eq!(app.state.mode, Mode::Navigate);
    }

    #[test]
    fn failed_machine_add_response_preserves_form_and_shows_message() {
        let mut app = app_with_test_workspaces(&[]);
        open_add_remote_machine(&mut app.state);
        app.state.machine_create.as_mut().unwrap().name = "dev".into();
        let response = serde_json::to_string(&crate::api::schema::ErrorResponse {
            id: "tui.machine.add".into(),
            error: crate::api::schema::ErrorBody {
                code: "machine_already_exists".into(),
                message: "machine name \"dev\" must be unique".into(),
            },
        })
        .unwrap();

        app.apply_machine_add_response(&response);

        let create = app.state.machine_create.as_ref().unwrap();
        assert_eq!(create.name, "dev");
        assert_eq!(
            create.error.as_deref(),
            Some("machine name \"dev\" must be unique")
        );
        assert!(app.state.request_new_workspace_machine.is_none());
        assert_eq!(app.state.mode, Mode::AddRemoteMachine);
    }

    #[test]
    fn ssh_host_import_view_preserves_form_and_skips_already_added_hosts() {
        let mut app = app_with_test_workspaces(&[]);
        open_add_remote_machine(&mut app.state);
        app.state.machine_create.as_mut().unwrap().name = "manual draft".into();
        let response = serde_json::to_string(&crate::api::schema::SuccessResponse {
            id: "tui.machine.ssh_hosts".into(),
            result: crate::api::schema::ResponseResult::MachineSshHosts {
                hosts: vec![
                    crate::api::schema::SshHostInfo {
                        alias: "existing".into(),
                        target: "ops@example.test".into(),
                        already_configured: true,
                    },
                    crate::api::schema::SshHostInfo {
                        alias: "build".into(),
                        target: "builder@example.test".into(),
                        already_configured: false,
                    },
                    crate::api::schema::SshHostInfo {
                        alias: "deploy".into(),
                        target: "deploy@example.test".into(),
                        already_configured: false,
                    },
                ],
            },
        })
        .unwrap();

        app.apply_machine_ssh_hosts_response(&response);
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()));
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()));
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()));

        let create = app.state.machine_create.as_ref().unwrap();
        assert_eq!(create.name, "manual draft");
        let import = create.import.as_ref().unwrap();
        assert!(!import.hosts[0].selected);
        assert!(import.hosts[1].selected);
        assert!(import.hosts[2].selected);
        assert_eq!(import.selected_aliases(), vec!["build", "deploy"]);

        app.handle_add_remote_machine_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        let create = app.state.machine_create.as_ref().unwrap();
        assert_eq!(create.name, "manual draft");
        assert!(create.import.is_none());
        assert_eq!(app.state.mode, Mode::AddRemoteMachine);
    }

    #[test]
    fn machine_import_response_keeps_each_host_outcome_visible() {
        let mut app = app_with_test_workspaces(&[]);
        open_add_remote_machine(&mut app.state);
        app.state.machine_create.as_mut().unwrap().import =
            Some(crate::app::state::MachineImportState::from_hosts(vec![
                crate::api::schema::SshHostInfo {
                    alias: "build".into(),
                    target: "builder@example.test".into(),
                    already_configured: false,
                },
                crate::api::schema::SshHostInfo {
                    alias: "deploy".into(),
                    target: "deploy@example.test".into(),
                    already_configured: false,
                },
            ]));
        let response = serde_json::to_string(&crate::api::schema::SuccessResponse {
            id: "tui.machine.import".into(),
            result: crate::api::schema::ResponseResult::MachineImported {
                outcomes: vec![
                    crate::api::schema::MachineImportOutcome::Failed {
                        alias: "build".into(),
                        reason: "permission denied".into(),
                    },
                    crate::api::schema::MachineImportOutcome::Added {
                        alias: "deploy".into(),
                    },
                ],
            },
        })
        .unwrap();

        app.apply_machine_import_response(&response);

        let import = app
            .state
            .machine_create
            .as_ref()
            .unwrap()
            .import
            .as_ref()
            .unwrap();
        assert!(matches!(
            import.hosts[0].outcome,
            Some(crate::api::schema::MachineImportOutcome::Failed { ref reason, .. })
                if reason == "permission denied"
        ));
        assert!(!import.hosts[0].already_configured);
        assert!(matches!(
            import.hosts[1].outcome,
            Some(crate::api::schema::MachineImportOutcome::Added { .. })
        ));
        assert!(import.hosts[1].already_configured);
        assert_eq!(app.state.mode, Mode::AddRemoteMachine);
    }

    fn mark_worktree_space_member(state: &mut AppState, ws_idx: usize, key: &str) {
        state.workspaces[ws_idx].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: key.into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: format!("/repo/worktree-{ws_idx}").into(),
            is_linked_worktree: ws_idx != 0,
        });
    }

    #[test]
    fn custom_resize_key_exits_resize_mode() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::Resize;
        state.keybinds.resize_mode = crate::config::ActionKeybinds::prefix("g");

        handle_resize_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('g'), KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn direct_resize_key_exits_resize_mode() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::Resize;
        state.keybinds.resize_mode = crate::config::ActionKeybinds::direct("ctrl+alt+r");

        handle_resize_key(
            &mut state,
            TerminalKey::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        );

        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn resize_key_exit_matches_enhanced_shifted_punctuation() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::Resize;
        state.keybinds.resize_mode = crate::config::ActionKeybinds::prefix("?");

        handle_resize_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
                .with_shifted_codepoint('?' as u32),
        );

        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn detach_requests_client_detach_in_persistence_mode() {
        let mut state = state_with_workspaces(&["test"]);
        state.detach_exits = false;

        request_detach(&mut state);

        assert!(state.detach_requested);
        assert!(!state.should_quit);
    }

    #[test]
    fn detach_exits_in_no_session_mode() {
        let mut state = state_with_workspaces(&["test"]);
        state.detach_exits = true;

        request_detach(&mut state);

        assert!(state.should_quit);
        assert!(!state.detach_requested);
    }

    #[test]
    fn global_menu_whats_new_opens_saved_release_notes() {
        let _guard = config_env_lock().lock().unwrap();
        let path = temp_config_path("whats-new-saved-release-notes");
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        crate::release_notes::save_pending(env!("CARGO_PKG_VERSION"), "### Changed\n- Menu")
            .unwrap();

        let mut state = state_with_workspaces(&["test"]);
        state.latest_release_notes_available = true;

        assert!(global_menu_actions(&state).contains(&GlobalMenuAction::WhatsNew));

        apply_global_menu_action(&mut state, GlobalMenuAction::WhatsNew);

        assert_eq!(state.mode, Mode::ReleaseNotes);
        assert_eq!(
            state
                .release_notes
                .as_ref()
                .map(|notes| notes.body.as_str()),
            Some("### Changed\n- Menu")
        );

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rename_modal_keyboard_and_mouse_share_actions() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::RenameWorkspace;
        state.name_input = "hello".into();

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(state.name_input.is_empty());

        state.name_input = "renamed".into();
        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(state.mode, Mode::Terminal);
        assert_eq!(state.workspaces[0].display_name(), "renamed");
        let snapshot = capture_snapshot(&state);
        assert_eq!(
            snapshot.workspaces[0].custom_name.as_deref(),
            Some("renamed")
        );

        state.view.sidebar_rect = Rect::new(0, 0, 26, 20);
        state.view.terminal_area = Rect::new(26, 0, 80, 20);
        state.mode = Mode::RenameWorkspace;
        state.name_input = "mouse".into();
        let inner = state.rename_modal_inner().unwrap();
        let (save, _, _) = crate::ui::rename_button_rects(inner);
        let action = modal_action_from_buttons(save.x, save.y, &[(save, ModalAction::Save)]);
        assert_eq!(action, Some(ModalAction::Save));
    }

    #[test]
    fn tab_rename_updates_captured_snapshot() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::RenameTab;
        state.name_input = "logs".into();

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        let snapshot = capture_snapshot(&state);
        assert_eq!(
            snapshot.workspaces[0].tabs[0].custom_name.as_deref(),
            Some("logs")
        );
    }

    #[test]
    fn rename_cancel_returns_to_terminal_when_workspace_is_active() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::RenameTab;
        state.name_input = "test".into();

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Terminal);
        assert!(state.name_input.is_empty());
    }

    #[test]
    fn rename_modal_replaces_prefilled_text_on_first_type() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::RenameTab;
        state.name_input = "2".into();
        state.name_input_replace_on_type = true;

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
        );
        assert_eq!(state.name_input, "n");
        assert!(!state.name_input_replace_on_type);

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty()),
        );
        assert_eq!(state.name_input, "ne");
    }

    #[test]
    fn rename_modal_replaces_prefilled_text_on_paste() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::RenameTab;
        state.name_input = "2".into();
        state.name_input_replace_on_type = true;

        insert_rename_input_text(&mut state, "feature/logs");

        assert_eq!(state.name_input, "feature/logs");
        assert!(!state.name_input_replace_on_type);

        insert_rename_input_text(&mut state, "-copy");

        assert_eq!(state.name_input, "feature/logs-copy");
    }

    #[test]
    fn rename_modal_handles_line_editing_shortcuts() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::RenameWorkspace;
        state.name_input = "website zero".into();

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
        );
        assert_eq!(state.name_input, "website zer");

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        );
        assert_eq!(state.name_input, "website ");

        state.name_input = "website-zero".into();
        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
        );
        assert_eq!(state.name_input, "website-");

        state.name_input = "website-zero".into();
        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.name_input, "website-");

        state.name_input = "website-zero".into();
        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.name_input, "website-");

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER),
        );
        assert!(state.name_input.is_empty());

        state.name_input = "website zero".into();
        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert!(state.name_input.is_empty());
    }

    #[test]
    fn rename_modal_does_not_insert_modified_shortcut_chars() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::RenameWorkspace;
        state.name_input = "website".into();

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.name_input, "website");

        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT),
        );
        assert_eq!(state.name_input, "websiteZ");
    }

    #[test]
    fn keybind_help_slash_focuses_filter_and_preserves_vim_scroll() {
        let mut state = state_with_workspaces(&["test"]);
        state.keybind_help.query = "stale".into();
        state.keybind_help.search_focused = true;
        state.view.terminal_area = Rect::new(0, 0, 100, 30);

        open_keybind_help(&mut state);
        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty()),
        );
        assert_eq!(state.keybind_help.scroll, 1);
        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('k'), KeyModifiers::empty()),
        );
        assert_eq!(state.keybind_help.scroll, 0);

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('w'), KeyModifiers::empty()),
        );
        assert!(state.keybind_help.query.is_empty());

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('/'), KeyModifiers::empty()),
        );
        for character in "work".chars() {
            state.keybind_help.scroll = 2;
            handle_keybind_help_key(
                &mut state,
                TerminalKey::new(KeyCode::Char(character), KeyModifiers::empty()),
            );
        }

        assert!(state.keybind_help.search_focused);
        assert_eq!(state.keybind_help.query, "work");
        assert_eq!(state.keybind_help.scroll, 0);
    }

    #[test]
    fn keybind_help_query_supports_backspace_clear_and_sanitized_paste() {
        let mut state = state_with_workspaces(&["test"]);
        open_keybind_help(&mut state);
        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('/'), KeyModifiers::empty()),
        );

        insert_keybind_help_query_text(&mut state, "work\nspace");
        assert_eq!(state.keybind_help.query, "workspace");

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Backspace, KeyModifiers::empty()),
        );
        assert_eq!(state.keybind_help.query, "workspac");

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert!(state.keybind_help.query.is_empty());
    }

    #[test]
    fn keybind_help_escape_leaves_search_before_closing() {
        let mut state = state_with_workspaces(&["test"]);
        open_keybind_help(&mut state);
        state.keybind_help.search_focused = true;
        state.keybind_help.query = "work".into();

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
        );
        assert_eq!(state.mode, Mode::KeybindHelp);
        assert!(!state.keybind_help.search_focused);
        assert!(state.keybind_help.query.is_empty());

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
        );
        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn enhanced_shifted_slash_focuses_keybind_help_filter() {
        let mut state = state_with_workspaces(&["test"]);
        open_keybind_help(&mut state);

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('7'), KeyModifiers::SHIFT)
                .with_shifted_codepoint('/' as u32),
        );

        assert!(state.keybind_help.search_focused);
    }

    #[test]
    fn enhanced_shifted_question_mark_closes_keybind_help_when_not_searching() {
        let mut state = state_with_workspaces(&["test"]);
        open_keybind_help(&mut state);

        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
                .with_shifted_codepoint('?' as u32),
        );

        assert_eq!(state.mode, Mode::Terminal);

        open_keybind_help(&mut state);
        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('/'), KeyModifiers::empty()),
        );
        handle_keybind_help_key(
            &mut state,
            TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
                .with_shifted_codepoint('?' as u32),
        );

        assert_eq!(state.keybind_help.query, "?");
    }

    #[test]
    fn navigator_search_accepts_pasted_text_when_focused() {
        let mut state = state_with_workspaces(&["alpha", "beta"]);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        state.mode = Mode::Navigator;
        state.navigator.search_focused = true;
        state.navigator.state_filter = Some(NavigatorStateFilter::Working);

        insert_navigator_search_text(&mut state, &terminal_runtimes, "beta");

        assert_eq!(state.navigator.query, "beta");
        assert_eq!(state.navigator.state_filter, None);
    }

    #[test]
    fn navigator_search_ignores_paste_when_search_is_not_focused() {
        let mut state = state_with_workspaces(&["alpha", "beta"]);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        state.mode = Mode::Navigator;
        state.navigator.search_focused = false;

        insert_navigator_search_text(&mut state, &terminal_runtimes, "beta");

        assert!(state.navigator.query.is_empty());
    }

    #[test]
    fn navigator_empty_search_escape_returns_to_commands() {
        let mut state = state_with_workspaces(&["alpha", "beta"]);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        state.mode = Mode::Navigator;
        state.navigator.search_focused = true;

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Navigator);
        assert!(!state.navigator.search_focused);
        assert!(state.navigator.query.is_empty());

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::empty()),
        );

        assert_eq!(
            state.navigator.state_filter,
            Some(NavigatorStateFilter::Working)
        );
        assert!(state.navigator.query.is_empty());

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn navigator_search_escape_blurs_then_next_escape_closes() {
        let mut state = state_with_workspaces(&["alpha", "beta"]);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        state.mode = Mode::Navigator;
        state.navigator.search_focused = true;
        state.navigator.query = "a".into();

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Navigator);
        assert!(!state.navigator.search_focused);
        assert_eq!(state.navigator.query, "a");

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()),
        );

        assert_eq!(state.navigator.selected, 1);
        assert_eq!(state.navigator.query, "a");

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Navigator);
        assert!(state.navigator.search_focused);
        assert_eq!(state.navigator.query, "a");

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::empty()),
        );

        assert_eq!(state.navigator.query, "al");

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Navigator);
        assert!(!state.navigator.search_focused);

        handle_navigator_key(
            &mut state,
            &terminal_runtimes,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn open_rename_active_tab_prefills_auto_name() {
        let mut state = state_with_workspaces(&["test"]);
        state.workspaces[0].test_add_tab(None);
        state.workspaces[0].switch_tab(1);

        open_rename_active_tab(&mut state, true);

        assert_eq!(state.mode, Mode::RenameTab);
        assert_eq!(state.name_input, "2");
        assert!(state.name_input_replace_on_type);
    }

    #[test]
    fn tab_context_menu_rename_opens_rename_dialog() {
        let mut state = state_with_workspaces(&["test"]);
        let menu = ContextMenuState {
            kind: ContextMenuKind::Tab {
                ws_idx: 0,
                tab_idx: 0,
            },
            x: 0,
            y: 0,
            list: MenuListState::new(1),
        };
        let mut terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        apply_context_menu_action(&mut state, &mut terminal_runtimes, menu, 1);

        assert_eq!(state.mode, Mode::RenameTab);
        assert_eq!(state.name_input, "1");
        assert!(!state.name_input_replace_on_type);
    }

    #[test]
    fn tab_type_terminal_selection_requests_terminal_tab() {
        let mut state = state_with_workspaces(&["test"]);
        state.mode = Mode::ContextMenu;
        let menu = ContextMenuState {
            kind: ContextMenuKind::TabCreateTarget {
                plugin_panes: Vec::new(),
            },
            x: 0,
            y: 0,
            list: MenuListState::new(0),
        };
        let mut terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        apply_context_menu_action(&mut state, &mut terminal_runtimes, menu, 0);

        assert!(state.request_new_tab);
        assert_eq!(state.mode, Mode::Terminal);
    }

    #[tokio::test]
    async fn api_tab_type_terminal_selection_creates_terminal_tab() {
        let mut app = app_with_test_workspaces(&["test"]);
        app.state.mode = Mode::ContextMenu;
        let menu = ContextMenuState {
            kind: ContextMenuKind::TabCreateTarget {
                plugin_panes: Vec::new(),
            },
            x: 0,
            y: 0,
            list: MenuListState::new(0),
        };

        app.apply_context_menu_action_via_api(menu, 0);

        assert_eq!(app.state.workspaces[0].tabs.len(), 2);
        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[test]
    fn renaming_auto_tab_to_its_default_number_keeps_it_auto_named() {
        let mut state = state_with_workspaces(&["test"]);
        state.workspaces[0].test_add_tab(None);
        state.workspaces[0].switch_tab(1);

        open_rename_active_tab(&mut state, false);
        handle_rename_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Terminal);
        assert!(state.workspaces[0].tabs[1].custom_name.is_none());
        assert_eq!(
            state.workspaces[0].tab_display_name(1).as_deref(),
            Some("2")
        );
    }

    #[test]
    fn confirm_close_keyboard_actions_are_direct_not_focused() {
        let mut state = state_with_workspaces(&["a", "b"]);
        state.mode = Mode::ConfirmClose;
        state.selected = 1;

        handle_confirm_close_key(
            &mut state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );
        assert_eq!(state.mode, Mode::Navigate);
        assert_eq!(state.workspaces.len(), 2);

        state.mode = Mode::ConfirmClose;
        handle_confirm_close_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(state.workspaces.len(), 1);
    }

    #[test]
    fn confirm_close_for_linked_worktree_closes_workspace_only() {
        let mut state = state_with_workspaces(&["main", "issue"]);
        state.mode = Mode::ConfirmClose;
        state.selected = 1;
        state.workspaces[1].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });

        handle_confirm_close_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        assert_eq!(state.request_remove_linked_worktree, None);
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].display_name(), "main");
        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn context_menu_close_group_opens_group_close_confirmation() {
        let mut state = state_with_workspaces(&["main", "issue"]);
        state.active = Some(0);
        state.selected = 1;
        state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr".into(),
            is_linked_worktree: false,
        });
        state.workspaces[1].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        let menu = ContextMenuState {
            kind: ContextMenuKind::GitWorkspace {
                ws_idx: 0,
                is_linked_worktree: false,
                has_worktree_children: true,
                collapsed: false,
                parent_space: crate::app::state::ParentSpaceMenu::Unavailable,
            },
            x: 0,
            y: 0,
            list: MenuListState::new(0),
        };
        let mut terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        apply_context_menu_action(&mut state, &mut terminal_runtimes, menu, 1);

        assert_eq!(state.selected, 0);
        assert_eq!(state.mode, Mode::ConfirmClose);

        confirm_close_accept(&mut state);

        assert!(state.workspaces.is_empty());
        assert_eq!(state.mode, Mode::Navigate);
    }

    #[test]
    fn context_menu_parent_space_action_is_queued_without_runtime_work() {
        let mut state = state_with_workspaces(&["parent"]);
        let menu = ContextMenuState {
            kind: ContextMenuKind::Workspace {
                ws_idx: 0,
                parent_space: crate::app::state::ParentSpaceMenu::Manage,
            },
            x: 0,
            y: 0,
            list: MenuListState::new(0),
        };
        let mut terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        apply_context_menu_action(&mut state, &mut terminal_runtimes, menu, 2);

        assert_eq!(
            state.request_parent_space_action,
            Some((0, crate::app::state::ParentSpaceAction::Rescan))
        );
        assert_eq!(state.mode, Mode::Terminal);
    }

    #[test]
    fn context_menu_close_pane_last_parent_group_pane_keeps_confirmation_mode() {
        let mut state = state_with_workspaces(&["main", "issue"]);
        state.active = Some(0);
        state.selected = 1;
        state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr".into(),
            is_linked_worktree: false,
        });
        state.workspaces[1].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        let pane_id = state.workspaces[0].tabs[0].root_pane;
        let menu = ContextMenuState {
            kind: ContextMenuKind::Pane {
                ws_idx: 0,
                tab_idx: 0,
                pane_id,
                source_pane_id: None,
                has_manual_label: false,
            },
            x: 0,
            y: 0,
            list: MenuListState::new(0),
        };
        let idx = menu
            .items()
            .iter()
            .position(|item| *item == "Close pane")
            .expect("close pane item");
        let mut terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        apply_context_menu_action(&mut state, &mut terminal_runtimes, menu, idx);

        assert_eq!(state.selected, 0);
        assert_eq!(state.mode, Mode::ConfirmClose);
        assert_eq!(state.workspaces.len(), 2);
    }

    #[test]
    fn api_context_menu_close_tab_last_parent_group_workspace_keeps_confirmation_mode() {
        let mut app = app_with_test_workspaces(&["main", "issue"]);
        mark_worktree_space_member(&mut app.state, 0, "repo-key");
        mark_worktree_space_member(&mut app.state, 1, "repo-key");
        app.state.active = Some(0);
        app.state.selected = 1;
        app.state.mode = Mode::ContextMenu;
        let menu = ContextMenuState {
            kind: ContextMenuKind::Tab {
                ws_idx: 0,
                tab_idx: 0,
            },
            x: 0,
            y: 0,
            list: MenuListState::new(0),
        };
        let idx = menu
            .items()
            .iter()
            .position(|item| *item == "Close")
            .expect("close tab item");

        app.apply_context_menu_action_via_api(menu, idx);

        assert_eq!(app.state.selected, 0);
        assert_eq!(app.state.mode, Mode::ConfirmClose);
        assert_eq!(app.state.workspaces.len(), 2);
    }

    #[test]
    fn api_context_menu_enter_close_pane_last_parent_group_pane_keeps_confirmation_mode() {
        let mut app = app_with_test_workspaces(&["main", "issue"]);
        mark_worktree_space_member(&mut app.state, 0, "repo-key");
        mark_worktree_space_member(&mut app.state, 1, "repo-key");
        app.state.active = Some(0);
        app.state.selected = 1;
        app.state.mode = Mode::ContextMenu;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let mut menu = ContextMenuState {
            kind: ContextMenuKind::Pane {
                ws_idx: 0,
                tab_idx: 0,
                pane_id,
                source_pane_id: None,
                has_manual_label: false,
            },
            x: 0,
            y: 0,
            list: MenuListState::new(0),
        };
        let close_idx = menu
            .items()
            .iter()
            .position(|item| *item == "Close pane")
            .expect("close pane item");
        menu.list.highlighted = close_idx;
        app.state.context_menu = Some(menu);

        app.handle_context_menu_key_via_api(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));

        assert_eq!(app.state.selected, 0);
        assert_eq!(app.state.mode, Mode::ConfirmClose);
        assert_eq!(app.state.workspaces.len(), 2);
        assert!(app.state.context_menu.is_none());
    }
}
