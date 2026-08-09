use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

const DEFAULT_WORKTREE_PREFIX: &str = "worktree";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
}

pub(crate) fn generated_branch_slug(seed: u64) -> String {
    let adjectives = [
        "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
    ];
    let nouns = [
        "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
    ];
    let adjective = adjectives[(seed as usize) % adjectives.len()];
    let noun = nouns[((seed / adjectives.len() as u64) as usize) % nouns.len()];
    let suffix = seed & 0xffff;
    format!("{DEFAULT_WORKTREE_PREFIX}/{adjective}-{noun}-{suffix:04x}")
}

pub(crate) fn branch_to_path_slug(branch: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in branch.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        DEFAULT_WORKTREE_PREFIX.to_string()
    } else {
        trimmed
    }
}

pub(crate) fn expand_tilde_path(path: &str) -> PathBuf {
    expand_tilde_path_from_env(path, cfg!(windows), |key| std::env::var_os(key))
}

fn expand_tilde_path_from_env(
    path: &str,
    is_windows: bool,
    env: impl Fn(&str) -> Option<OsString> + Copy,
) -> PathBuf {
    if path == "~" {
        return home_dir_from_env(is_windows, env).unwrap_or_else(|_| PathBuf::from(path));
    }

    let tilde_rest = path.strip_prefix("~/").or_else(|| {
        if is_windows {
            path.strip_prefix("~\\")
        } else {
            None
        }
    });
    if let Some(rest) = tilde_rest {
        return home_dir_from_env(is_windows, env)
            .map(|home| join_tilde_rest(home, rest, is_windows))
            .unwrap_or_else(|_| PathBuf::from(path));
    }

    PathBuf::from(path)
}

fn join_tilde_rest(home: PathBuf, rest: &str, is_windows: bool) -> PathBuf {
    if is_windows {
        rest.split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .fold(home, |path, component| path.join(component))
    } else {
        home.join(rest)
    }
}

fn home_dir_from_env(
    is_windows: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, ()> {
    if !is_windows {
        return env("HOME").map(PathBuf::from).ok_or(());
    }

    if let Some(path) = usable_home_path(env("USERPROFILE")) {
        return Ok(path);
    }
    if let (Some(drive), Some(path)) = (
        usable_home_component(env("HOMEDRIVE")),
        usable_home_component(env("HOMEPATH")),
    ) {
        let path = path.to_string_lossy();
        if !path.starts_with(['\\', '/']) {
            return usable_home_path(env("HOME")).ok_or(());
        }
        let combined = format!("{}{}", drive.to_string_lossy(), path);
        if let Some(path) = usable_home_path(Some(OsString::from(combined))) {
            return Ok(path);
        }
    }

    usable_home_path(env("HOME")).ok_or(())
}

fn usable_home_path(value: Option<OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() || value == "~" {
        return None;
    }
    Some(PathBuf::from(value))
}

fn usable_home_component(value: Option<OsString>) -> Option<OsString> {
    let value = value?;
    if value.is_empty() || value == "~" {
        return None;
    }
    Some(value)
}

pub(crate) fn expand_tilde_absolute_path(path: &str) -> PathBuf {
    let path = expand_tilde_path(path);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

#[derive(Debug)]
pub(crate) struct CanonicalPathError {
    path: PathBuf,
    source: io::Error,
}

impl CanonicalPathError {
    #[cfg(test)]
    pub(crate) fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }
}

impl fmt::Display for CanonicalPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to canonicalize {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for CanonicalPathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn canonical_path(path: &Path) -> Result<PathBuf, CanonicalPathError> {
    std::fs::canonicalize(path).map_err(|source| CanonicalPathError {
        path: path.to_path_buf(),
        source,
    })
}

/// Canonicalizes every existing ancestor and rejects a missing path containing `..`.
///
/// Safe for proving where a path *would* live when its leaf is legitimately absent, such as the
/// stale admin directory of a leftover worktree checkout.
pub(crate) fn canonical_new_path(path: &Path) -> Result<PathBuf, CanonicalPathError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => canonical_path(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if path
                .components()
                .any(|component| component == std::path::Component::ParentDir)
            {
                return Err(CanonicalPathError {
                    path: path.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot canonicalize a missing path containing '..'",
                    ),
                });
            }
            let Some(parent) = path.parent() else {
                return Err(CanonicalPathError {
                    path: path.to_path_buf(),
                    source: err,
                });
            };
            let Some(file_name) = path.file_name() else {
                return canonical_path(path);
            };
            canonical_new_path(parent).map(|parent| parent.join(file_name))
        }
        Err(source) => Err(CanonicalPathError {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Best-effort identity for non-destructive path comparisons.
///
/// Decisions that authorize removal must use [`canonical_path`] instead.
pub(crate) fn canonical_or_original(path: &Path) -> PathBuf {
    canonical_path(path).unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkedWorktreeRepair {
    pub key: String,
    pub repo_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkedWorktreeRepairErrorKind {
    PointerBroken,
    RepairFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkedWorktreeRepairError {
    pub kind: LinkedWorktreeRepairErrorKind,
    pub message: String,
}

impl LinkedWorktreeRepairError {
    fn pointer(path: &Path, detail: impl fmt::Display) -> Self {
        Self {
            kind: LinkedWorktreeRepairErrorKind::PointerBroken,
            message: format!(
                "broken linked-worktree pointer {}: {detail}",
                path.display()
            ),
        }
    }

    fn repair(path: &Path, detail: impl fmt::Display) -> Self {
        Self {
            kind: LinkedWorktreeRepairErrorKind::RepairFailed,
            message: format!(
                "failed to repair linked-worktree pointer {}: {detail}",
                path.display()
            ),
        }
    }
}

fn read_regular_pointer(path: &Path) -> Result<PathBuf, LinkedWorktreeRepairError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| LinkedWorktreeRepairError::pointer(path, err))?;
    if !metadata.file_type().is_file() {
        return Err(LinkedWorktreeRepairError::pointer(
            path,
            "expected a regular file",
        ));
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|err| LinkedWorktreeRepairError::pointer(path, err))?;
    let mut lines = contents.lines();
    let Some(line) = lines.next() else {
        return Err(LinkedWorktreeRepairError::pointer(path, "file is empty"));
    };
    if lines.next().is_some() {
        return Err(LinkedWorktreeRepairError::pointer(
            path,
            "expected one gitdir line",
        ));
    }
    let Some(value) = line.strip_prefix("gitdir:").map(str::trim) else {
        return Err(LinkedWorktreeRepairError::pointer(
            path,
            "expected a gitdir line",
        ));
    };
    if value.is_empty() {
        return Err(LinkedWorktreeRepairError::pointer(
            path,
            "gitdir path is empty",
        ));
    }
    let value = PathBuf::from(value);
    Ok(if value.is_absolute() {
        value
    } else {
        path.parent().unwrap_or(Path::new(".")).join(value)
    })
}

fn read_admin_checkout_pointer(path: &Path) -> Result<PathBuf, LinkedWorktreeRepairError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| LinkedWorktreeRepairError::pointer(path, err))?;
    if !metadata.file_type().is_file() {
        return Err(LinkedWorktreeRepairError::pointer(
            path,
            "expected a regular file",
        ));
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|err| LinkedWorktreeRepairError::pointer(path, err))?;
    let value = contents.trim();
    if value.is_empty() || contents.lines().count() != 1 {
        return Err(LinkedWorktreeRepairError::pointer(
            path,
            "expected one checkout path",
        ));
    }
    let value = PathBuf::from(value);
    Ok(if value.is_absolute() {
        value
    } else {
        path.parent().unwrap_or(Path::new(".")).join(value)
    })
}

fn canonical_pointer_target(
    pointer: &Path,
    target: &Path,
) -> Result<PathBuf, LinkedWorktreeRepairError> {
    canonical_new_path(target).map_err(|err| LinkedWorktreeRepairError::pointer(pointer, err))
}

fn has_linked_worktree_admin_shape(path: &Path, admin_name: &std::ffi::OsStr) -> bool {
    path.file_name() == Some(admin_name)
        && path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("worktrees"))
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            == Some(std::ffi::OsStr::new(".git"))
}

fn validated_main_worktree_common_dir(
    repo_root: &Path,
) -> Result<PathBuf, LinkedWorktreeRepairError> {
    let git_path = repo_root.join(".git");
    let metadata = std::fs::symlink_metadata(&git_path)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_path, err))?;
    if !metadata.file_type().is_dir() {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_path,
            "recovery parent is not a main checkout",
        ));
    }

    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "rev-parse",
            "--show-toplevel",
            "--git-common-dir",
            "--is-bare-repository",
        ])
        .output()
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_path, err))?;
    if !output.status.success() {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_path,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_path, err))?;
    let lines = stdout.lines().collect::<Vec<_>>();
    if lines.len() != 3 || lines[2] != "false" {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_path,
            "recovery parent is not a non-bare main checkout",
        ));
    }
    let reported_root = canonical_path(Path::new(lines[0]))
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_path, err))?;
    let expected_root = canonical_path(repo_root)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_path, err))?;
    if reported_root != expected_root {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_path,
            format!(
                "Git reports a different main checkout: {}",
                reported_root.display()
            ),
        ));
    }
    let reported_common = PathBuf::from(lines[1]);
    let reported_common = if reported_common.is_absolute() {
        reported_common
    } else {
        repo_root.join(reported_common)
    };
    let reported_common = canonical_path(&reported_common)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_path, err))?;
    let expected_common = canonical_path(&git_path)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_path, err))?;
    if reported_common != expected_common {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_path,
            format!(
                "Git reports a different common directory: {}",
                reported_common.display()
            ),
        ));
    }
    Ok(reported_common)
}

#[cfg(test)]
thread_local! {
    static FORCE_LINKED_WORKTREE_REPAIR_FAILURE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(test)]
pub(crate) fn with_forced_linked_worktree_repair_failure<T>(run: impl FnOnce() -> T) -> T {
    FORCE_LINKED_WORKTREE_REPAIR_FAILURE.with(|forced| {
        assert!(
            !forced.replace(true),
            "repair failure seam is already active"
        );
        let result = run();
        forced.set(false);
        result
    })
}

fn run_linked_worktree_repair_command(repo_root: &Path, checkout: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FORCE_LINKED_WORKTREE_REPAIR_FAILURE.with(std::cell::Cell::get) {
        return Err("forced native git worktree repair failure".to_string());
    }

    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "repair"])
        .arg(checkout)
        .output()
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub(crate) fn repair_linked_worktree_after_parent_move(
    checkout: &Path,
    previous_key: &str,
    previous_repo_root: &Path,
    previous_checkout: &Path,
    repo_root: &Path,
) -> Result<LinkedWorktreeRepair, LinkedWorktreeRepairError> {
    let checkout = canonical_path(checkout)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&checkout.join(".git"), err))?;
    let git_file = checkout.join(".git");
    let child_target = read_regular_pointer(&git_file)?;
    let child_target = canonical_pointer_target(&git_file, &child_target)?;

    let Some(admin_name) = child_target.file_name() else {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_file,
            "gitdir has no worktree name",
        ));
    };
    let common_dir = validated_main_worktree_common_dir(repo_root)?;
    let worktrees_dir = common_dir.join("worktrees");
    let worktrees_metadata = std::fs::symlink_metadata(&worktrees_dir)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&worktrees_dir, err))?;
    if !worktrees_metadata.file_type().is_dir() {
        return Err(LinkedWorktreeRepairError::pointer(
            &worktrees_dir,
            "expected a regular worktrees directory",
        ));
    }
    let raw_new_admin_dir = worktrees_dir.join(admin_name);
    let new_admin_identity = canonical_pointer_target(&git_file, &raw_new_admin_dir)?;
    let previous_admin_dir = canonical_pointer_target(
        &git_file,
        &previous_repo_root
            .join(".git")
            .join("worktrees")
            .join(admin_name),
    )?;
    let group_already_rekeyed = previous_key == common_dir.display().to_string();
    let legacy_admin_matches = if group_already_rekeyed {
        child_target != new_admin_identity
            && has_linked_worktree_admin_shape(&child_target, admin_name)
    } else {
        child_target == previous_admin_dir
    };
    if child_target != new_admin_identity && !legacy_admin_matches {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_file,
            "gitdir does not match the recorded old parent or the selected new parent",
        ));
    }
    let new_admin_metadata = std::fs::symlink_metadata(&raw_new_admin_dir)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&raw_new_admin_dir, err))?;
    if !new_admin_metadata.file_type().is_dir() {
        return Err(LinkedWorktreeRepairError::pointer(
            &raw_new_admin_dir,
            "expected a regular worktree administration directory",
        ));
    }
    let new_admin_dir = canonical_path(&raw_new_admin_dir)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&raw_new_admin_dir, err))?;
    let admin_gitdir = new_admin_dir.join("gitdir");
    let admin_target = read_admin_checkout_pointer(&admin_gitdir)?;
    let admin_target = canonical_pointer_target(&admin_gitdir, &admin_target)?;
    let previous_checkout_git =
        canonical_pointer_target(&admin_gitdir, &previous_checkout.join(".git"))?;
    let checkout_git = canonical_path(&git_file)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&admin_gitdir, err))?;

    let pointers_current = child_target == new_admin_dir && admin_target == checkout_git;
    let pointers_legacy = legacy_admin_matches && admin_target == previous_checkout_git;
    if !pointers_current && !pointers_legacy {
        return Err(LinkedWorktreeRepairError::pointer(
            &admin_gitdir,
            "pointer does not match the recorded old checkout or the selected new checkout",
        ));
    }

    if pointers_legacy {
        run_linked_worktree_repair_command(repo_root, &checkout)
            .map_err(|err| LinkedWorktreeRepairError::repair(&git_file, err))?;
    }

    let repaired_child_target = read_regular_pointer(&git_file)?;
    let repaired_child_target = canonical_pointer_target(&git_file, &repaired_child_target)?;
    if repaired_child_target != new_admin_dir {
        return Err(LinkedWorktreeRepairError::pointer(
            &git_file,
            format!(
                "expected {}, found {}",
                new_admin_dir.display(),
                repaired_child_target.display()
            ),
        ));
    }
    let repaired_admin_target = read_admin_checkout_pointer(&admin_gitdir)?;
    let repaired_admin_target = canonical_pointer_target(&admin_gitdir, &repaired_admin_target)?;
    if repaired_admin_target != checkout_git {
        return Err(LinkedWorktreeRepairError::pointer(
            &admin_gitdir,
            format!(
                "expected {}, found {}",
                checkout_git.display(),
                repaired_admin_target.display()
            ),
        ));
    }

    let status = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(&checkout)
        .args(["status", "--porcelain=v1"])
        .output()
        .map_err(|err| LinkedWorktreeRepairError::repair(&git_file, err))?;
    if !status.status.success() {
        return Err(LinkedWorktreeRepairError::repair(
            &git_file,
            String::from_utf8_lossy(&status.stderr).trim(),
        ));
    }

    let registry = list_existing_worktrees(repo_root)
        .map_err(|err| LinkedWorktreeRepairError::repair(&git_file, err))?;
    let repo_root_identity = canonical_path(repo_root)
        .map_err(|err| LinkedWorktreeRepairError::pointer(&git_file, err))?;
    let checkout_matches = registry
        .iter()
        .filter(|entry| {
            canonical_or_original(&entry.path) == checkout && !entry.is_prunable && !entry.is_bare
        })
        .count();
    let parent_is_registered = registry.iter().any(|entry| {
        canonical_or_original(&entry.path) == repo_root_identity && !entry.is_prunable
    });
    if checkout_matches != 1 || !parent_is_registered {
        return Err(LinkedWorktreeRepairError::repair(
            &git_file,
            format!(
                "git worktree list did not contain one repaired checkout and its parent (checkout matches: {checkout_matches})"
            ),
        ));
    }

    Ok(LinkedWorktreeRepair {
        key: common_dir.display().to_string(),
        repo_root: repo_root_identity,
    })
}

/// Stable identity for coordinating create and remove operations.
///
/// Resolving the deepest existing ancestor keeps a missing checkout beneath a symlink keyed the
/// same way after Git materializes it. This key only coordinates operations and does not authorize
/// filesystem removal.
pub(crate) fn worktree_operation_key(path: &Path) -> Result<PathBuf, CanonicalPathError> {
    canonical_new_path(path)
}

pub(crate) fn default_checkout_path(root: &Path, repo_name: &str, branch: &str) -> PathBuf {
    root.join(repo_name).join(branch_to_path_slug(branch))
}

pub(crate) fn build_worktree_remove_command(
    repo_root: &Path,
    path: &Path,
    force: bool,
) -> WorktreeCommand {
    let mut args = vec![
        "-C".to_string(),
        repo_root.display().to_string(),
        "worktree".to_string(),
        "remove".to_string(),
    ];
    if force {
        args.push("--force".to_string());
    }
    args.push(path.display().to_string());

    WorktreeCommand {
        program: "git".to_string(),
        args,
    }
}

pub(crate) fn is_dirty_worktree_remove_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("contains modified or untracked files")
        && lower.contains("use --force to delete it")
}

pub(crate) fn is_not_working_tree_remove_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("is not a working tree") || lower.contains("is not a worktree")
}

#[cfg(windows)]
pub(crate) fn worktree_dirty_remove_message(path: &Path) -> String {
    format!(
        "fatal: '{}' contains modified or untracked files, use --force to delete it",
        path.display()
    )
}

#[cfg(any(windows, test))]
#[derive(Debug)]
pub(crate) struct CheckoutStatusError {
    path: PathBuf,
    message: String,
}

#[cfg(any(windows, test))]
impl fmt::Display for CheckoutStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not determine whether {} is clean: {}",
            self.path.display(),
            self.message
        )
    }
}

#[cfg(any(windows, test))]
impl std::error::Error for CheckoutStatusError {}

#[cfg(any(windows, test))]
pub(crate) fn checkout_has_dirty_files(path: &Path) -> Result<bool, CheckoutStatusError> {
    let path_arg = path.display().to_string();
    let output = crate::noninteractive_process::command("git")
        .args([
            "-C",
            &path_arg,
            "status",
            "--porcelain",
            "--untracked-files=all",
        ])
        .output()
        .map_err(|err| CheckoutStatusError {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;

    if output.status.success() {
        return Ok(!output.stdout.is_empty());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        Err(CheckoutStatusError {
            path: path.to_path_buf(),
            message: stderr,
        })
    } else if !stdout.is_empty() {
        Err(CheckoutStatusError {
            path: path.to_path_buf(),
            message: stdout,
        })
    } else {
        Err(CheckoutStatusError {
            path: path.to_path_buf(),
            message: format!("git status failed with status {}", output.status),
        })
    }
}

pub(crate) fn build_worktree_add_new_branch_command(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> WorktreeCommand {
    WorktreeCommand {
        program: "git".to_string(),
        args: vec![
            "-C".to_string(),
            repo_root.display().to_string(),
            "worktree".to_string(),
            "add".to_string(),
            "-b".to_string(),
            branch.to_string(),
            path.display().to_string(),
            base.to_string(),
        ],
    }
}

pub(crate) fn build_worktree_add_existing_branch_command(
    repo_root: &Path,
    path: &Path,
    branch: &str,
) -> WorktreeCommand {
    WorktreeCommand {
        program: "git".to_string(),
        args: vec![
            "-C".to_string(),
            repo_root.display().to_string(),
            "worktree".to_string(),
            "add".to_string(),
            path.display().to_string(),
            branch.to_string(),
        ],
    }
}

pub(crate) fn local_branch_exists(repo_root: &Path, branch: &str) -> Result<bool, String> {
    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        Err(stderr)
    } else if !stdout.is_empty() {
        Err(stdout)
    } else {
        Err(format!("git show-ref failed with status {}", output.status))
    }
}

pub(crate) fn run_worktree_add_command(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<(), String> {
    let command = if local_branch_exists(repo_root, branch)? {
        build_worktree_add_existing_branch_command(repo_root, path, branch)
    } else {
        build_worktree_add_new_branch_command(repo_root, path, branch, base)
    };
    run_worktree_command(&command)
}

pub(crate) fn run_worktree_command(command: &WorktreeCommand) -> Result<(), String> {
    let output = crate::noninteractive_process::command(&command.program)
        .args(&command.args)
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        format!("{} failed with status {}", command.program, output.status)
    } else {
        message
    })
}

pub(crate) fn run_worktree_remove_command_with_recovery(
    command: &WorktreeCommand,
    repo_root: &Path,
    path: &Path,
    force: bool,
) -> Result<(), String> {
    match run_worktree_command(command) {
        Ok(()) => Ok(()),
        Err(err) if force && is_not_working_tree_remove_error(&err) => {
            if worktree_list_contains_path(repo_root, path)? {
                return Err(err);
            }
            if path.exists() {
                if !leftover_worktree_checkout_matches_repo(repo_root, path) {
                    return Err(err);
                }
                std::fs::remove_dir_all(path).map_err(|remove_err| {
                    format!(
                        "{err}; failed to remove leftover checkout {}: {remove_err}",
                        path.display()
                    )
                })?;
            }
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn leftover_worktree_checkout_matches_repo(repo_root: &Path, path: &Path) -> bool {
    let git_file = path.join(".git");
    let Ok(metadata) = std::fs::symlink_metadata(&git_file) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(&git_file) else {
        return false;
    };
    let Some(gitdir) = content.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let gitdir = PathBuf::from(gitdir.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        path.join(gitdir)
    };
    let Some(worktrees_dir) = git_common_worktrees_dir(repo_root) else {
        return false;
    };
    let (Ok(gitdir), Ok(worktrees_dir)) = (
        canonical_new_path(&gitdir),
        canonical_new_path(&worktrees_dir),
    ) else {
        return false;
    };
    if gitdir.parent() != Some(worktrees_dir.as_path()) {
        return false;
    }
    std::fs::symlink_metadata(&gitdir)
        .is_ok_and(|_| worktree_admin_record_matches_checkout(&gitdir, path))
}

fn worktree_admin_record_matches_checkout(gitdir: &Path, checkout: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(gitdir.join("gitdir")) else {
        return false;
    };
    let admin_checkout = PathBuf::from(content.trim());
    let admin_checkout = if admin_checkout.is_absolute() {
        admin_checkout
    } else {
        gitdir.join(admin_checkout)
    };
    let Ok(expected_checkout) = canonical_path(checkout).map(|path| path.join(".git")) else {
        return false;
    };
    let Ok(admin_checkout) = canonical_path(&admin_checkout) else {
        return false;
    };
    admin_checkout == expected_checkout
}

fn git_common_worktrees_dir(repo_root: &Path) -> Option<PathBuf> {
    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let common_dir = stdout.trim();
    if common_dir.is_empty() {
        None
    } else {
        let common_dir = PathBuf::from(common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            repo_root.join(common_dir)
        };
        Some(common_dir.join("worktrees"))
    }
}

pub(crate) fn parse_worktree_list_porcelain(output: &str) -> Vec<ExistingWorktree> {
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = None;
    let mut is_bare = false;
    let mut is_detached = false;
    let mut is_prunable = false;

    let finish = |entries: &mut Vec<ExistingWorktree>,
                  path: &mut Option<PathBuf>,
                  branch: &mut Option<String>,
                  is_bare: &mut bool,
                  is_detached: &mut bool,
                  is_prunable: &mut bool| {
        if let Some(path) = path.take() {
            entries.push(ExistingWorktree {
                path,
                branch: branch.take(),
                is_bare: *is_bare,
                is_detached: *is_detached,
                is_prunable: *is_prunable,
            });
        }
        *is_bare = false;
        *is_detached = false;
        *is_prunable = false;
    };

    for line in output.lines() {
        if line.trim().is_empty() {
            finish(
                &mut entries,
                &mut path,
                &mut branch,
                &mut is_bare,
                &mut is_detached,
                &mut is_prunable,
            );
            continue;
        }
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch ") {
            branch = Some(
                value
                    .strip_prefix("refs/heads/")
                    .unwrap_or(value)
                    .to_string(),
            );
        } else if line == "detached" {
            is_detached = true;
        } else if line == "bare" {
            is_bare = true;
        } else if line.starts_with("prunable") {
            is_prunable = true;
        }
    }

    finish(
        &mut entries,
        &mut path,
        &mut branch,
        &mut is_bare,
        &mut is_detached,
        &mut is_prunable,
    );
    entries
}

pub(crate) fn list_existing_worktrees(repo_root: &Path) -> Result<Vec<ExistingWorktree>, String> {
    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(parse_worktree_list_porcelain(&stdout));
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("git worktree list failed with status {}", output.status)
    } else {
        stderr
    })
}

pub(crate) fn worktree_list_contains_path(repo_root: &Path, path: &Path) -> Result<bool, String> {
    let expected = canonical_path(path).map_err(|err| err.to_string())?;
    for entry in list_existing_worktrees(repo_root)? {
        let entry = canonical_path(&entry.path).map_err(|err| err.to_string())?;
        if entry == expected {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git command failed: git -C {} {}",
            repo.display(),
            args.join(" ")
        );
    }

    fn create_committed_repo(name: &str) -> PathBuf {
        let repo = unique_temp_path(name);
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet"]);
        run_git(&repo, &["config", "user.email", "herdr@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Herdr Test"]);
        std::fs::write(repo.join("README.md"), "test\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    #[test]
    fn generated_branch_slug_is_worktree_namespaced_and_stable() {
        assert_eq!(generated_branch_slug(0), "worktree/brave-river-0000");
        assert_eq!(generated_branch_slug(9), "worktree/calm-cloud-0009");
    }

    #[test]
    fn parses_git_worktree_list_porcelain() {
        let output = "\
worktree /repo/main
HEAD abc
branch refs/heads/main

worktree /repo/issue
HEAD def
branch refs/heads/worktree/issue

worktree /repo/detached
HEAD fed
detached
prunable stale

";

        assert_eq!(
            parse_worktree_list_porcelain(output),
            vec![
                ExistingWorktree {
                    path: PathBuf::from("/repo/main"),
                    branch: Some("main".into()),
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                },
                ExistingWorktree {
                    path: PathBuf::from("/repo/issue"),
                    branch: Some("worktree/issue".into()),
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                },
                ExistingWorktree {
                    path: PathBuf::from("/repo/detached"),
                    branch: None,
                    is_bare: false,
                    is_detached: true,
                    is_prunable: true,
                },
            ]
        );
    }

    #[test]
    fn branch_to_path_slug_makes_branch_safe_folder_name() {
        assert_eq!(
            branch_to_path_slug("worktree/brave-river"),
            "worktree-brave-river"
        );
        assert_eq!(
            branch_to_path_slug("issue/137 Worktree Spaces"),
            "issue-137-worktree-spaces"
        );
        assert_eq!(branch_to_path_slug("///"), "worktree");
    }

    #[test]
    fn expand_tilde_path_uses_home_when_available() {
        assert_eq!(
            expand_tilde_path_from_env("~/.herdr/worktrees", false, |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from("/home/me/.herdr/worktrees")
        );
        assert_eq!(
            expand_tilde_path_from_env("/tmp/worktrees", false, |_| None),
            PathBuf::from("/tmp/worktrees")
        );
    }

    #[test]
    fn home_dir_uses_windows_profile_before_literal_home() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOME" => Some("~".into()),
                "USERPROFILE" => Some(r"C:\Users\herdr".into()),
                _ => None,
            }),
            Ok(PathBuf::from(r"C:\Users\herdr"))
        );
    }

    #[test]
    fn home_dir_uses_windows_drive_and_path_when_profile_is_missing() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some(r"\Users\herdr".into()),
                _ => None,
            }),
            Ok(PathBuf::from(r"C:\Users\herdr"))
        );
    }

    #[test]
    fn home_dir_rejects_incomplete_windows_drive_and_path() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some("".into()),
                _ => None,
            }),
            Err(())
        );
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some("Users\\herdr".into()),
                _ => None,
            }),
            Err(())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_tilde_expansion_keeps_windows_separator_literal() {
        assert_eq!(
            expand_tilde_path_from_env(r"~\.herdr\worktrees", false, |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from(r"~\.herdr\worktrees")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_tilde_expansion_normalizes_separators() {
        fn env(key: &str) -> Option<OsString> {
            match key {
                "HOME" => Some("~".into()),
                "USERPROFILE" => Some(r"C:\Users\herdr".into()),
                _ => None,
            }
        }

        let default_path = expand_tilde_path_from_env("~/.herdr/worktrees", true, env);
        assert_eq!(
            default_path,
            PathBuf::from(r"C:\Users\herdr\.herdr\worktrees")
        );
        assert_eq!(
            default_path.display().to_string(),
            r"C:\Users\herdr\.herdr\worktrees"
        );
        assert_eq!(
            expand_tilde_path_from_env(r"~\.herdr\worktrees", true, env),
            PathBuf::from(r"C:\Users\herdr\.herdr\worktrees")
        );
    }

    #[test]
    fn default_checkout_path_appends_repo_and_branch_slug() {
        assert_eq!(
            default_checkout_path(
                Path::new("/home/me/.herdr/worktrees"),
                "herdr",
                "worktree/brave-river",
            ),
            PathBuf::from("/home/me/.herdr/worktrees/herdr/worktree-brave-river")
        );
    }

    #[test]
    fn canonical_path_reports_missing_path_instead_of_using_lexical_identity() {
        let path = unique_temp_path("missing-canonical-path");

        let err = canonical_path(&path).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn worktree_membership_check_errors_when_path_identity_is_unknown() {
        let path = unique_temp_path("missing-membership-path");

        let err = worktree_list_contains_path(Path::new("."), &path).unwrap_err();

        assert!(err.contains("failed to canonicalize"));
        assert!(err.contains(&path.display().to_string()));
    }

    #[test]
    fn canonical_new_path_resolves_existing_parent_for_missing_checkout() {
        let root = unique_temp_path("canonical-new-path");
        std::fs::create_dir_all(&root).unwrap();
        let checkout = root.join("missing").join("checkout");

        let resolved = canonical_new_path(&checkout).unwrap();

        assert_eq!(
            resolved,
            root.canonicalize()
                .unwrap()
                .join("missing")
                .join("checkout")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn canonical_new_path_rejects_broken_symlink() {
        let root = unique_temp_path("canonical-broken-link");
        std::fs::create_dir_all(&root).unwrap();
        let link = root.join("checkout");
        std::os::unix::fs::symlink(root.join("missing-target"), &link).unwrap();

        let err = canonical_new_path(&link).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_new_path_rejects_unresolved_parent_components() {
        let root = unique_temp_path("canonical-parent-component");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("missing").join("..").join("checkout");

        let err = canonical_new_path(&path).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn worktree_operation_key_resolves_symlink_before_parent_component() {
        let root = unique_temp_path("operation-key-symlink-parent");
        let checkout_root = root.join("checkouts");
        let outside = root.join("outside");
        let nested = outside.join("nested");
        let outside_checkout = outside.join("checkout");
        let lexical_checkout = checkout_root.join("checkout");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&outside_checkout).unwrap();
        std::fs::create_dir_all(&lexical_checkout).unwrap();
        std::os::unix::fs::symlink(&nested, checkout_root.join("link")).unwrap();
        let alias = checkout_root.join("link").join("..").join("checkout");

        let alias_key = worktree_operation_key(&alias).unwrap();

        assert_eq!(
            alias_key,
            worktree_operation_key(&outside_checkout).unwrap()
        );
        assert_ne!(
            alias_key,
            worktree_operation_key(&lexical_checkout).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn worktree_operation_key_rejects_missing_symlink_parent_alias() {
        let root = unique_temp_path("operation-key-missing-symlink-parent");
        let checkout_root = root.join("checkouts");
        let nested = root.join("outside").join("nested");
        std::fs::create_dir_all(&checkout_root).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::os::unix::fs::symlink(&nested, checkout_root.join("link")).unwrap();
        let alias = checkout_root.join("link").join("..").join("checkout");

        let err = worktree_operation_key(&alias).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_dirty_detection_reports_clean_and_dirty_worktrees() {
        let repo = create_committed_repo("worktree-dirty-detection-repo");
        let checkout = unique_temp_path("worktree-dirty-detection-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/dirty-detection",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );

        assert!(matches!(checkout_has_dirty_files(&checkout), Ok(false)));
        std::fs::write(checkout.join("README.md"), "dirty\n").unwrap();
        assert!(matches!(checkout_has_dirty_files(&checkout), Ok(true)));

        let remove = build_worktree_remove_command(&repo, &checkout, true);
        run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn checkout_dirty_detection_reports_unknown_for_missing_checkout() {
        let checkout = unique_temp_path("missing-dirty-detection-checkout");

        let err = checkout_has_dirty_files(&checkout).unwrap_err();

        assert!(err.to_string().contains("could not determine whether"));
    }

    #[test]
    fn worktree_remove_command_preserves_branch_by_not_deleting_it() {
        let command = build_worktree_remove_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/issue-137"),
            false,
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "remove",
                "/w/herdr/issue-137"
            ]
        );
    }

    #[test]
    fn forced_worktree_remove_command_uses_git_force_flag() {
        let command = build_worktree_remove_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/issue-137"),
            true,
        );
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "remove",
                "--force",
                "/w/herdr/issue-137"
            ]
        );
    }

    #[test]
    fn dirty_remove_error_detection_matches_git_force_hint() {
        assert!(is_dirty_worktree_remove_error(
            "fatal: '/w/herdr' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_dirty_worktree_remove_error(
            "fatal: '/w/herdr' is a missing but already registered worktree"
        ));
        assert!(!is_dirty_worktree_remove_error(
            "fatal: '/w/herdr' contains a locked worktree, use --force only if you know why"
        ));
    }

    #[test]
    fn worktree_add_command_creates_new_branch_from_base() {
        let command = build_worktree_add_new_branch_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/worktree-brave-river"),
            "worktree/brave-river",
            "HEAD",
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "add",
                "-b",
                "worktree/brave-river",
                "/w/herdr/worktree-brave-river",
                "HEAD"
            ]
        );
    }

    #[test]
    fn worktree_add_command_checks_out_existing_branch() {
        let command = build_worktree_add_existing_branch_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/worktree-brave-river"),
            "worktree/brave-river",
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "add",
                "/w/herdr/worktree-brave-river",
                "worktree/brave-river"
            ]
        );
    }

    #[test]
    fn run_worktree_add_and_remove_create_and_delete_checkout() {
        let repo = create_committed_repo("worktree-run-repo");
        let checkout = unique_temp_path("worktree-run-checkout");
        let branch = "worktree/test-create-remove";

        let add = build_worktree_add_new_branch_command(&repo, &checkout, branch, "HEAD");
        run_worktree_command(&add).unwrap();

        assert!(checkout.join("README.md").exists());
        let branch_name = std::process::Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert!(branch_name.status.success());
        assert_eq!(
            String::from_utf8(branch_name.stdout).unwrap().trim(),
            branch
        );

        let remove = build_worktree_remove_command(&repo, &checkout, false);
        run_worktree_command(&remove).unwrap();
        assert!(!checkout.exists());

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn forced_worktree_remove_recovery_rejects_missing_admin_record() {
        let repo = create_committed_repo("worktree-recovery-repo");
        let checkout = unique_temp_path("worktree-recovery-checkout");
        let branch = "worktree/recovery";

        let add = build_worktree_add_new_branch_command(&repo, &checkout, branch, "HEAD");
        run_worktree_command(&add).unwrap();
        let remove = build_worktree_remove_command(&repo, &checkout, true);
        run_worktree_command(&remove).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        let stale_admin_dir = git_common_worktrees_dir(&repo).unwrap().join("stale");
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", stale_admin_dir.display()),
        )
        .unwrap();
        std::fs::write(checkout.join("leftover"), "leftover\n").unwrap();

        let err = run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true)
            .expect_err("a missing admin record cannot prove checkout identity");

        assert!(is_not_working_tree_remove_error(&err));
        assert!(checkout.join("leftover").exists());
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn forced_worktree_remove_recovery_keeps_unrelated_replacement_directory() {
        let repo = create_committed_repo("worktree-recovery-unrelated-repo");
        let checkout = unique_temp_path("worktree-recovery-unrelated-checkout");
        let branch = "worktree/recovery-unrelated";

        let add = build_worktree_add_new_branch_command(&repo, &checkout, branch, "HEAD");
        run_worktree_command(&add).unwrap();
        let remove = build_worktree_remove_command(&repo, &checkout, true);
        run_worktree_command(&remove).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("unrelated"), "do not delete\n").unwrap();

        let err = run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true)
            .expect_err("unrelated replacement directory should not be removed");

        assert!(is_not_working_tree_remove_error(&err));
        assert!(checkout.join("unrelated").exists());
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn forced_worktree_remove_recovery_rejects_another_active_worktree_admin_record() {
        let repo = create_committed_repo("worktree-recovery-active-admin-repo");
        let checkout = unique_temp_path("worktree-recovery-active-admin-checkout");
        let other_checkout = unique_temp_path("worktree-recovery-active-admin-other");
        let add = build_worktree_add_new_branch_command(
            &repo,
            &other_checkout,
            "worktree/recovery-active-admin",
            "HEAD",
        );
        run_worktree_command(&add).unwrap();
        let other_git_file = std::fs::read_to_string(other_checkout.join(".git")).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join(".git"), other_git_file).unwrap();
        std::fs::write(checkout.join("unrelated"), "do not delete\n").unwrap();
        let remove = build_worktree_remove_command(&repo, &checkout, true);

        let err = run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true)
            .expect_err("another active worktree admin record must be rejected");

        assert!(is_not_working_tree_remove_error(&err));
        assert!(checkout.join("unrelated").exists());
        let remove_other = build_worktree_remove_command(&repo, &other_checkout, true);
        run_worktree_command(&remove_other).unwrap();
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[cfg(unix)]
    #[test]
    fn forced_worktree_remove_recovery_rejects_symlinked_active_worktree_git_file() {
        let repo = create_committed_repo("worktree-recovery-symlinked-git-repo");
        let checkout = unique_temp_path("worktree-recovery-symlinked-git-checkout");
        let other_checkout = unique_temp_path("worktree-recovery-symlinked-git-other");
        let add = build_worktree_add_new_branch_command(
            &repo,
            &other_checkout,
            "worktree/recovery-symlinked-git",
            "HEAD",
        );
        run_worktree_command(&add).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::os::unix::fs::symlink(other_checkout.join(".git"), checkout.join(".git")).unwrap();
        std::fs::write(checkout.join("unrelated"), "do not delete\n").unwrap();
        let remove = build_worktree_remove_command(&repo, &checkout, true);

        let err = run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true)
            .expect_err("a symlinked active worktree admin record must be rejected");

        assert!(is_not_working_tree_remove_error(&err));
        assert!(checkout.join("unrelated").exists());
        let remove_other = build_worktree_remove_command(&repo, &other_checkout, true);
        run_worktree_command(&remove_other).unwrap();
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }
}
