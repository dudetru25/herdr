use std::{
    io::{ErrorKind, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::api::schema::{MachineAddParams, MachineInfo, ResponseResult};
use crate::app::App;
use crate::config::{MachineConfig, MachineConfigEditError};

use super::responses::{encode_error, encode_success};

fn write_config_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    let target = crate::config::resolve_config_write_target(path)?;
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let existing_permissions = match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "refusing to atomically replace config symlink {}",
                    target.display()
                ),
            ))
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };

    let mut last_collision = None;
    for _ in 0..16 {
        let nonce = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{file_name}.herdr-tmp-{}-{nonce}",
            std::process::id()
        ));
        let mut temp_file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                last_collision = Some(err);
                continue;
            }
            Err(err) => return Err(err),
        };

        let write_result = (|| {
            if let Some(permissions) = existing_permissions.clone() {
                temp_file.set_permissions(permissions)?;
            }
            temp_file.write_all(content.as_bytes())?;
            temp_file.sync_all()
        })();
        drop(temp_file);
        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(err);
        }
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "refusing to atomically replace config symlink {}",
                        target.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(err);
            }
        }
        let rename_result = crate::platform::replace_file(&temp_path, &target);
        if rename_result.is_err() {
            let _ = std::fs::remove_file(&temp_path);
        }
        return rename_result;
    }

    Err(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
            ErrorKind::AlreadyExists,
            "could not allocate a temporary config file",
        )
    }))
}

fn encode_machine_add_after_reload(
    app: &App,
    id: String,
    machine: &MachineConfig,
    report: crate::config::ConfigReloadReport,
) -> String {
    if report.status == crate::config::ConfigReloadStatus::Failed {
        let details = report.diagnostics.join("; ");
        return encode_error(
            id,
            "config_reload_failed",
            if details.is_empty() {
                "saved machine but failed to reload config".to_string()
            } else {
                format!("saved machine but failed to reload config: {details}")
            },
        );
    }
    let Some(live_machine) = app
        .state
        .machines
        .iter()
        .find(|live| **live == *machine)
        .map(MachineInfo::from)
    else {
        return encode_error(
            id,
            "config_reload_failed",
            "saved machine but it did not appear in the live registry",
        );
    };
    encode_success(
        id,
        ResponseResult::MachineAdded {
            machine: live_machine,
        },
    )
}

impl App {
    pub(super) fn handle_machine_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::MachineList {
                machines: self.state.machines.iter().map(MachineInfo::from).collect(),
            },
        )
    }

    pub(super) fn handle_machine_add(&mut self, id: String, params: MachineAddParams) -> String {
        let machine = MachineConfig {
            name: params.name.trim().to_string(),
            target: params.target.trim().to_string(),
            cwd: params.cwd.map(|cwd| cwd.trim().to_string()),
        };
        let path = crate::config::config_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
            Err(err) => {
                return encode_error(
                    id,
                    "config_update_failed",
                    format!("failed to read {}: {err}", path.display()),
                )
            }
        };
        let updated = match crate::config::append_machine_config(&content, &machine) {
            Ok(updated) => updated,
            Err(MachineConfigEditError::InvalidMachine(err)) => {
                return encode_error(id, "invalid_machine", err.to_string())
            }
            Err(MachineConfigEditError::MachineAlreadyExists { name }) => {
                return encode_error(
                    id,
                    "machine_already_exists",
                    format!("machine {name:?} is already configured"),
                )
            }
            Err(err) => return encode_error(id, "config_update_failed", err.to_string()),
        };

        if let Err(err) = write_config_atomically(&path, &updated) {
            crate::logging::config_write_failed(&path, "machine", &err.to_string());
            return encode_error(
                id,
                "config_update_failed",
                format!("failed to write {}: {err}", path.display()),
            );
        }

        let report = self.apply_config_from_disk(false);
        encode_machine_add_after_reload(self, id, &machine, report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{ErrorResponse, MachineAddParams, SuccessResponse},
        config::{Config, MachineConfig},
    };

    fn temp_config_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("herdr-machine-add-{}-{name}", std::process::id()))
            .join("config.toml")
    }

    #[test]
    fn machine_list_returns_live_registry() {
        let config = Config {
            machines: vec![MachineConfig {
                name: "build".into(),
                target: "builder@example.com".into(),
                cwd: Some("~/src/herdr".into()),
            }],
            ..Config::default()
        };
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let response = app.handle_machine_list("machines".into());
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MachineList { machines } = success.result else {
            panic!("expected machine list");
        };
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].name, "build");
        assert_eq!(machines[0].target, "builder@example.com");
        assert_eq!(machines[0].cwd.as_deref(), Some("~/src/herdr"));

        app.state.machines[0].target = "replacement".into();
        let response = app.handle_machine_list("machines-live".into());
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MachineList { machines } = success.result else {
            panic!("expected machine list");
        };
        assert_eq!(machines[0].target, "replacement");
    }

    #[test]
    fn machine_add_preserves_config_and_updates_live_registry() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = temp_config_path("success");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "# keep this\n[ui]\nmouse_capture = false\n";
        std::fs::write(&path, original).unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let response = app.handle_machine_add(
            "add".into(),
            MachineAddParams {
                name: "  build  ".into(),
                target: "  builder@example.test  ".into(),
                cwd: Some("  ~/src/herdr  ".into()),
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MachineAdded { machine } = success.result else {
            panic!("expected machine added response: {response}");
        };

        assert_eq!(machine.name, "build");
        assert_eq!(
            app.state.machines,
            vec![MachineConfig {
                name: "build".into(),
                target: "builder@example.test".into(),
                cwd: Some("~/src/herdr".into()),
            }]
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(original));
        assert!(saved.contains("[[machines]]"));

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn machine_add_reports_validation_duplicate_and_config_shape_errors() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = temp_config_path("errors");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let response = app.handle_machine_add(
            "invalid".into(),
            MachineAddParams {
                name: "build".into(),
                target: "-oProxyCommand=bad".into(),
                cwd: None,
            },
        );
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_machine");
        assert!(!path.exists());

        std::fs::write(&path, "[[machines]]\nname = \"build\"\ntarget = \"one\"\n").unwrap();
        let response = app.handle_machine_add(
            "duplicate".into(),
            MachineAddParams {
                name: "build".into(),
                target: "two".into(),
                cwd: None,
            },
        );
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "machine_already_exists");

        let inline = "machines = [{ name = \"build\", target = \"one\" }]\n";
        std::fs::write(&path, inline).unwrap();
        let response = app.handle_machine_add(
            "shape".into(),
            MachineAddParams {
                name: "prod".into(),
                target: "two".into(),
                cwd: None,
            },
        );
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "config_update_failed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), inline);

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn machine_add_succeeds_when_an_unrelated_section_is_invalid() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = temp_config_path("partial-reload");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[ui]\nmouse_capture = \"not-a-bool\"\n").unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let response = app.handle_machine_add(
            "partial".into(),
            MachineAddParams {
                name: "build".into(),
                target: "host".into(),
                cwd: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MachineAdded { machine } = success.result else {
            panic!("expected machine added");
        };
        assert_eq!(machine.name, "build");
        assert_eq!(machine.target, "host");
        assert_eq!(app.state.machines[0].name, "build");
        assert!(app.state.config_diagnostic.is_some());

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn machine_add_reload_failures_use_stable_error_code() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let response = encode_machine_add_after_reload(
            &app,
            "reload".into(),
            &MachineConfig {
                name: "build".into(),
                target: "host".into(),
                cwd: None,
            },
            crate::config::ConfigReloadReport {
                status: crate::config::ConfigReloadStatus::Failed,
                diagnostics: vec!["config read failed".into()],
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "config_reload_failed");
        assert!(error.error.message.contains("config read failed"));
    }

    #[test]
    fn machine_add_does_not_treat_unreadable_config_as_empty() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = temp_config_path("unreadable");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let response = app.handle_machine_add(
            "unreadable".into(),
            MachineAddParams {
                name: "build".into(),
                target: "host".into(),
                cwd: None,
            },
        );
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "config_update_failed");
        assert!(path.is_dir());
        assert!(app.state.machines.is_empty());

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn atomic_config_write_replaces_complete_file_and_preserves_permissions() {
        let path = temp_config_path("atomic");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old content\n").unwrap();
        let permissions = std::fs::metadata(&path).unwrap().permissions();

        write_config_atomically(&path, "new complete content\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new complete content\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().readonly(),
            permissions.readonly()
        );
        assert!(!std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".herdr-tmp-")));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_config_write_preserves_symlink_and_updates_its_target() {
        use std::os::unix::fs::symlink;

        let path = temp_config_path("atomic-symlink");
        let parent = path.parent().unwrap();
        let target = parent.join("managed-config.toml");
        let _ = std::fs::remove_dir_all(parent);
        std::fs::create_dir_all(parent).unwrap();
        std::fs::write(&target, "old content\n").unwrap();
        symlink("managed-config.toml", &path).unwrap();

        write_config_atomically(&path, "new complete content\n").unwrap();

        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "new complete content\n"
        );

        let _ = std::fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn machine_add_rejects_long_symlink_chain_without_modifying_it() {
        use std::os::unix::fs::symlink;

        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = temp_config_path("long-symlink-chain");
        let parent = path.parent().unwrap();
        let target = parent.join("managed-config.toml");
        let _ = std::fs::remove_dir_all(parent);
        std::fs::create_dir_all(parent).unwrap();
        std::fs::write(&target, "# managed\n").unwrap();

        let links = (0..17)
            .map(|index| parent.join(format!("config-link-{index}.toml")))
            .collect::<Vec<_>>();
        for (index, link) in links.iter().enumerate() {
            let destination = links
                .get(index + 1)
                .and_then(|next| next.file_name())
                .unwrap_or_else(|| target.file_name().unwrap());
            symlink(destination, link).unwrap();
        }
        symlink(links[0].file_name().unwrap(), &path).unwrap();
        let original_destinations = std::iter::once(&path)
            .chain(links.iter())
            .map(|link| std::fs::read_link(link).unwrap())
            .collect::<Vec<_>>();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let response = app.handle_machine_add(
            "long-symlink-chain".into(),
            MachineAddParams {
                name: "build".into(),
                target: "host".into(),
                cwd: None,
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "config_update_failed");
        assert!(error.error.message.contains("symlink"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# managed\n");
        assert_eq!(
            std::iter::once(&path)
                .chain(links.iter())
                .map(|link| std::fs::read_link(link).unwrap())
                .collect::<Vec<_>>(),
            original_destinations
        );

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(parent);
    }
}
