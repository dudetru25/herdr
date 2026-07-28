use super::App;

impl App {
    pub(super) fn update_config_file<F>(&mut self, error_context: &str, update: F) -> bool
    where
        F: FnOnce(&str) -> String,
    {
        #[cfg(test)]
        if std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR).is_none() {
            return false;
        }

        let path = crate::config::config_path();
        let target = match crate::config::resolve_config_write_target(&path) {
            Ok(target) => target,
            Err(err) => {
                crate::logging::config_write_failed(&path, error_context, &err.to_string());
                self.state.config_diagnostic =
                    Some(format!("failed to save {error_context}: {err}"));
                self.config_diagnostic_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                return false;
            }
        };
        if let Some(parent) = target.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                crate::logging::config_write_failed(&target, error_context, &err.to_string());
                self.state.config_diagnostic =
                    Some(format!("failed to save {error_context}: {err}"));
                self.config_diagnostic_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                return false;
            }
        }

        let content = match std::fs::read_to_string(&target) {
            Ok(content) => content,
            // A missing config is the normal first-write case, so there is no prior
            // user content to preserve.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                crate::logging::config_write_failed(&target, error_context, &err.to_string());
                self.state.config_diagnostic =
                    Some(format!("failed to save {error_context}: {err}"));
                self.config_diagnostic_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                return false;
            }
        };
        let new_content = update(&content);
        if let Err(err) = std::fs::write(&target, new_content) {
            crate::logging::config_write_failed(&target, error_context, &err.to_string());
            self.state.config_diagnostic = Some(format!("failed to save {error_context}: {err}"));
            self.config_diagnostic_deadline =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
            return false;
        }

        true
    }

    pub(super) fn mark_onboarding_complete(&mut self) {
        self.update_config_file("onboarding setting", |content| {
            crate::config::upsert_top_level_bool(content, "onboarding", false)
        });
    }

    pub(super) fn save_theme(&mut self, name: &str) {
        if self.update_config_file("theme", |content| {
            let content = crate::config::upsert_section_value(
                content,
                "theme",
                "name",
                &format!("\"{name}\""),
            );
            crate::config::upsert_section_bool(&content, "theme", "auto_switch", false)
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_sound(&mut self, enabled: bool) {
        if self.update_config_file("sound setting", |content| {
            crate::config::upsert_section_bool(content, "ui.sound", "enabled", enabled)
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_toast_delivery(&mut self, delivery: crate::config::ToastDelivery) {
        let value = match delivery {
            crate::config::ToastDelivery::Off => "\"off\"",
            crate::config::ToastDelivery::Herdr => "\"herdr\"",
            crate::config::ToastDelivery::Terminal => "\"terminal\"",
            crate::config::ToastDelivery::System => "\"system\"",
        };
        if self.update_config_file("toast setting", |content| {
            let content =
                crate::config::upsert_section_value(content, "ui.toast", "delivery", value);
            crate::config::remove_section_key(&content, "ui.toast", "enabled")
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_agent_border_labels(&mut self, enabled: bool) {
        if self.update_config_file("agent border labels", |content| {
            crate::config::upsert_section_bool(
                content,
                "ui",
                "show_agent_labels_on_pane_borders",
                enabled,
            )
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_pane_history_persistence(&mut self, enabled: bool) {
        if self.update_config_file("pane screen history", |content| {
            crate::config::upsert_section_bool(content, "experimental", "pane_history", enabled)
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_switch_ascii_input_source_in_prefix(&mut self, enabled: bool) {
        if self.update_config_file("prefix ascii input source", |content| {
            crate::config::upsert_section_bool(
                content,
                "experimental",
                "switch_ascii_input_source_in_prefix",
                enabled,
            )
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_agent_panel_sort(&mut self, sort: crate::app::state::AgentPanelSort) {
        let value = match sort {
            crate::app::state::AgentPanelSort::Spaces => {
                crate::config::AgentPanelSortConfig::Spaces.as_str()
            }
            crate::app::state::AgentPanelSort::Priority => {
                crate::config::AgentPanelSortConfig::Priority.as_str()
            }
        };
        if self.update_config_file("agent panel sort", |content| {
            crate::config::upsert_section_value(
                content,
                "ui",
                "agent_panel_sort",
                &format!("\"{value}\""),
            )
        }) {
            self.apply_config_from_disk(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn config_update_does_not_treat_read_failure_as_empty_content() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir()
            .join(format!("herdr-config-update-read-{}", std::process::id()))
            .join("config.toml");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        let mut app = test_app();

        assert!(!app.update_config_file("theme", |_| "replacement".to_string()));

        assert!(path.is_dir());
        assert!(app
            .state
            .config_diagnostic
            .as_deref()
            .is_some_and(|message| message.starts_with("failed to save theme:")));
        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn config_update_rejects_long_symlink_chain() {
        use std::os::unix::fs::symlink;

        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir()
            .join(format!("herdr-config-update-links-{}", std::process::id()))
            .join("config.toml");
        let parent = path.parent().unwrap();
        let _ = std::fs::remove_dir_all(parent);
        std::fs::create_dir_all(parent).unwrap();
        let target = parent.join("target.toml");
        std::fs::write(&target, "original\n").unwrap();
        let links = (0..17)
            .map(|index| parent.join(format!("link-{index}.toml")))
            .collect::<Vec<_>>();
        for (index, link) in links.iter().enumerate() {
            let destination = links
                .get(index + 1)
                .and_then(|next| next.file_name())
                .unwrap_or_else(|| target.file_name().unwrap());
            symlink(destination, link).unwrap();
        }
        symlink(links[0].file_name().unwrap(), &path).unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        let mut app = test_app();

        assert!(!app.update_config_file("theme", |_| "replacement".to_string()));

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");
        assert!(app
            .state
            .config_diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("symlink")));
        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(parent);
    }
}
