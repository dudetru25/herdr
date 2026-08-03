#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::*;

// Remote worker placement is available on Windows even though the interactive
// remote attach bridge remains Unix-only. Keep the worker transport helpers in
// a Windows-specific module so the API layer has the same typed surface on
// every target.
#[cfg(windows)]
mod worker {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use base64::Engine as _;

    use crate::api::schema::WorkerHarness;

    #[derive(Debug)]
    pub(crate) struct RemoteWorkerPrerequisiteError {
        pub(crate) code: &'static str,
        pub(crate) message: String,
    }

    pub(crate) fn verify_remote_worker_prerequisites(
        target: &str,
        cwd: &Path,
        harness: WorkerHarness,
    ) -> Result<(), RemoteWorkerPrerequisiteError> {
        let cwd = cwd.to_str().ok_or_else(|| RemoteWorkerPrerequisiteError {
            code: "worker_placement_blocked",
            message: "approved remote worker directory is not valid utf-8".into(),
        })?;
        if cwd.trim().is_empty() || cwd.chars().any(char::is_control) {
            return Err(RemoteWorkerPrerequisiteError {
                code: "worker_placement_blocked",
                message: "approved remote worker directory is empty or contains control characters"
                    .into(),
            });
        }
        let command = worker_command(harness);
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$remoteHome = $env:HOME
if ([string]::IsNullOrWhiteSpace($remoteHome)) {{ $remoteHome = $env:USERPROFILE }}
$remotePath = $env:PATH
$remoteTemp = $env:TMPDIR
if ([string]::IsNullOrWhiteSpace($remoteTemp)) {{ $remoteTemp = $env:TEMP }}
if ([string]::IsNullOrWhiteSpace($remoteTemp)) {{ $remoteTemp = $env:TMP }}
$remoteUser = $env:USER
if ([string]::IsNullOrWhiteSpace($remoteUser)) {{ $remoteUser = $env:USERNAME }}
if ([string]::IsNullOrWhiteSpace($remoteHome) -or [string]::IsNullOrWhiteSpace($remotePath) -or [string]::IsNullOrWhiteSpace($remoteTemp) -or [string]::IsNullOrWhiteSpace($remoteUser)) {{
    [Console]::WriteLine('HERDR-ENV-UNAVAILABLE')
    exit 43
}}
$cwd = {cwd}
if (-not (Test-Path -LiteralPath $cwd -PathType Container)) {{
    [Console]::WriteLine('HERDR-CWD-UNAVAILABLE')
    exit 42
}}
if (-not (Test-Path -LiteralPath (Join-Path $cwd '.git'))) {{
    [Console]::WriteLine('HERDR-CWD-NOT-CHECKOUT')
    exit 42
}}
Set-Location -LiteralPath $cwd
try {{
    Get-Command -Name {command} -CommandType Application,ExternalScript -ErrorAction Stop | Out-Null
}} catch {{
    [Console]::WriteLine('HERDR-HARNESS-UNAVAILABLE')
    exit 44
}}
[Console]::WriteLine('HERDR-REMOTE-READY')
"#,
            cwd = powershell_quote(cwd),
            command = powershell_quote(command),
        );
        let output = run_remote_powershell(target, &script).map_err(|error| {
            RemoteWorkerPrerequisiteError {
                code: "worker_placement_unavailable",
                message: format!("remote worker prerequisite probe failed: {error}"),
            }
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            && stdout
                .lines()
                .any(|line| line.trim() == "HERDR-REMOTE-READY")
        {
            return Ok(());
        }
        let (code, message) = if stdout.contains("HERDR-ENV-UNAVAILABLE") {
            (
                "worker_harness_unavailable",
                format!(
                    "remote worker environment is unavailable for the approved {harness:?} harness"
                ),
            )
        } else if stdout.contains("HERDR-CWD-") {
            (
                "worker_placement_unavailable",
                format!(
                    "approved remote worker directory {cwd:?} is unavailable or not a checkout"
                ),
            )
        } else if stdout.contains("HERDR-HARNESS-UNAVAILABLE") {
            (
                "worker_harness_unavailable",
                format!("the approved {harness:?} harness is unavailable on the remote device"),
            )
        } else {
            (
                "worker_placement_unavailable",
                command_failed("remote worker prerequisite probe failed", &output).to_string(),
            )
        };
        Err(RemoteWorkerPrerequisiteError { code, message })
    }

    fn worker_command(harness: WorkerHarness) -> &'static str {
        match harness {
            WorkerHarness::Codex => "codex",
            WorkerHarness::Claude => "claude",
        }
    }

    const REMOTE_WORKER_ENVIRONMENT_SETUP: &str = r#"
function Set-HerdrWorkerEnvironment {
    $h=$env:HOME; if ([string]::IsNullOrWhiteSpace($h)) {$h=$env:USERPROFILE}
    $p=$env:PATH
    $t=$env:TMPDIR; if ([string]::IsNullOrWhiteSpace($t)) {$t=$env:TEMP}; if ([string]::IsNullOrWhiteSpace($t)) {$t=$env:TMP}
    $u=$env:USER; if ([string]::IsNullOrWhiteSpace($u)) {$u=$env:USERNAME}
    if ([string]::IsNullOrWhiteSpace($h) -or [string]::IsNullOrWhiteSpace($p) -or [string]::IsNullOrWhiteSpace($t) -or [string]::IsNullOrWhiteSpace($u)) {return $false}
    $up=$env:USERPROFILE; if ([string]::IsNullOrWhiteSpace($up)) {$up=$h}
    $cx=$env:CODEX_HOME; if ([string]::IsNullOrWhiteSpace($cx)) {$cx=Join-Path $h '.codex'}
    $cc=$env:CLAUDE_CONFIG_DIR; if ([string]::IsNullOrWhiteSpace($cc)) {$cc=Join-Path $h '.claude'}
    $v=@{HOME=$h;PATH=$p;TMPDIR=$t;TEMP=$t;TMP=$t;USER=$u;USERNAME=$u;USERPROFILE=$up;CODEX_HOME=$cx;CLAUDE_CONFIG_DIR=$cc;SystemRoot=$env:SystemRoot;WINDIR=$env:WINDIR;ComSpec=$env:ComSpec;PATHEXT=$env:PATHEXT;PSModulePath=$env:PSModulePath}
    foreach($n in @([Environment]::GetEnvironmentVariables('Process').Keys)){[Environment]::SetEnvironmentVariable([string]$n,$null,'Process')}
    foreach($n in $v.Keys){if($null -ne $v[$n] -and $v[$n] -ne ''){[Environment]::SetEnvironmentVariable($n,[string]$v[$n],'Process')}}
    return $true
}
"#;

    pub(crate) fn remote_worker_argv(
        target: &str,
        cwd: &Path,
        worker_argv: &[String],
        run_id: &str,
    ) -> io::Result<Vec<String>> {
        if target.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote worker target must not be empty",
            ));
        }
        if worker_argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote worker argv must not be empty",
            ));
        }
        let component = remote_worker_component(run_id)?;
        let mut args = String::new();
        for (index, argument) in worker_argv.iter().enumerate() {
            args.push_str("    ");
            args.push_str(&powershell_quote(argument));
            if index + 1 < worker_argv.len() {
                args.push(',');
            }
            args.push('\n');
        }
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
{environment}
$root = Join-Path ([IO.Path]::GetTempPath()) {artifact_subpath}
New-Item -ItemType Directory -Force -Path $root | Out-Null
Set-Location -LiteralPath {cwd}
$workerArgs = @(
{args})
$workerArgs = @($workerArgs | ForEach-Object {{ $_.Replace({marker}, $root) }})
if (-not (Set-HerdrWorkerEnvironment)) {{ throw 'remote worker environment is unavailable' }}
$command = Get-Command -Name $workerArgs[0] -CommandType Application,ExternalScript -ErrorAction Stop
$env:HERDR_WORKER_ARTIFACT_DIR = $root
& $command.Source $workerArgs[1..($workerArgs.Count - 1)]
$exitCode = $LASTEXITCODE
if ($null -eq $exitCode) {{ $exitCode = 0 }}
exit $exitCode
"#,
            artifact_subpath = powershell_quote(&format!("herdr-worker-runs\\{component}")),
            cwd = powershell_quote(&cwd.to_string_lossy()),
            marker = powershell_quote(crate::worker_adapters::REMOTE_WORKER_ARTIFACT_MARKER),
            environment = REMOTE_WORKER_ENVIRONMENT_SETUP,
            args = args,
        );
        Ok(vec![
            "ssh".into(),
            "-o".into(),
            "SendEnv=NONE".into(),
            "-t".into(),
            target.into(),
            "powershell.exe".into(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-EncodedCommand".into(),
            encode_powershell(&script),
        ])
    }

    pub(crate) fn fetch_remote_worker_artifacts(
        target: &str,
        run_id: &str,
        local_root: &Path,
    ) -> io::Result<()> {
        let component = remote_worker_component(run_id)?;
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$WarningPreference = 'SilentlyContinue'
$InformationPreference = 'SilentlyContinue'
$root = Join-Path ([IO.Path]::GetTempPath()) {artifact_subpath}
if (-not (Test-Path -LiteralPath $root -PathType Container)) {{ exit 41 }}
try {{
    Get-ChildItem -LiteralPath $root -File -Recurse | ForEach-Object {{
        $relative = $_.FullName.Substring($root.Length).TrimStart([char[]]"/\\").Replace('\\', '/')
        $name = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($relative))
        $bytes = [IO.File]::ReadAllBytes($_.FullName)
        [Console]::WriteLine("HERDR-FILE:$name")
        [Console]::WriteLine([Convert]::ToBase64String($bytes))
    }}
    [Console]::WriteLine('HERDR-END')
}} finally {{
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}}
"#,
            artifact_subpath = powershell_quote(&format!("herdr-worker-runs\\{component}")),
        );
        let output = run_remote_powershell(target, &script)?;
        if !output.status.success() {
            return Err(command_failed(
                "remote worker artifact fetch failed",
                &output,
            ));
        }

        fs::create_dir_all(local_root)?;
        let stream = String::from_utf8(output.stdout).map_err(|error| {
            io::Error::other(format!("remote artifact stream was not utf-8: {error}"))
        })?;
        let mut lines = stream.lines().map(str::trim).peekable();
        let mut saw_end = false;
        while let Some(line) = lines.next() {
            if line == "HERDR-END" {
                saw_end = true;
                break;
            }
            let Some(encoded_name) = line.strip_prefix("HERDR-FILE:") else {
                return Err(io::Error::other(
                    "remote worker artifact stream contained an unexpected frame",
                ));
            };
            let name_bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded_name)
                .map_err(|error| {
                    io::Error::other(format!("invalid remote artifact name: {error}"))
                })?;
            let name = String::from_utf8(name_bytes).map_err(|error| {
                io::Error::other(format!("remote artifact name was not utf-8: {error}"))
            })?;
            let relative = PathBuf::from(&name);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(io::Error::other(format!(
                    "remote worker artifact path escapes its root: {name:?}"
                )));
            }
            let encoded_bytes = lines.next().ok_or_else(|| {
                io::Error::other("remote artifact stream ended before file bytes")
            })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded_bytes)
                .map_err(|error| {
                    io::Error::other(format!("invalid remote artifact bytes: {error}"))
                })?;
            let destination = local_root.join(&relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, bytes)?;
        }
        if !saw_end {
            return Err(io::Error::other(
                "remote worker artifact stream did not terminate",
            ));
        }
        Ok(())
    }

    fn run_remote_powershell(target: &str, script: &str) -> io::Result<Output> {
        Command::new("ssh")
            .arg("-o")
            .arg("SendEnv=NONE")
            .arg("-T")
            .arg(target)
            .arg("powershell.exe")
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-EncodedCommand")
            .arg(encode_powershell(script))
            .output()
    }

    pub(crate) fn encode_powershell(script: &str) -> String {
        let mut utf16 = Vec::with_capacity(script.len() * 2);
        for unit in script.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        base64::engine::general_purpose::STANDARD.encode(utf16)
    }

    fn powershell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    pub(crate) fn remote_worker_component(run_id: &str) -> io::Result<String> {
        let component = run_id
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if component.is_empty() || component.len() > 200 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker run id cannot form a remote artifact directory",
            ));
        }
        Ok(component)
    }

    fn command_failed(context: &str, output: &Output) -> io::Error {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            io::Error::other(format!("{context}: {}", output.status))
        } else {
            io::Error::other(format!("{context}: {stderr}"))
        }
    }
}

#[cfg(windows)]
pub(crate) use worker::*;

#[cfg(windows)]
pub(crate) const REATTACH_COMMAND_ENV_VAR: &str = "HERDR_REATTACH_COMMAND";
#[cfg(windows)]
pub(crate) const REMOTE_KEYBINDINGS_ENV_VAR: &str = "HERDR_REMOTE_KEYBINDINGS";

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKeybindings {
    Local,
    Server,
}

#[cfg(windows)]
impl RemoteKeybindings {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            _ => Err("--remote-keybindings must be 'local' or 'server'".to_string()),
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteLaunch {
    pub(crate) target: String,
    pub(crate) keybindings: RemoteKeybindings,
    pub(crate) live_handoff: bool,
}

#[cfg(windows)]
pub(crate) fn extract_remote_args(
    args: &[String],
) -> Result<(Vec<String>, Option<RemoteLaunch>), String> {
    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }

    let mut remote_target = None;
    let mut keybindings = RemoteKeybindings::Local;
    let mut keybindings_seen = false;
    let mut live_handoff = false;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--handoff" {
            live_handoff = true;
            index += 1;
            continue;
        }
        if arg == "--remote" {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote".to_string());
            };
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote=") {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 1;
            continue;
        }
        if arg == "--remote-keybindings" {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote-keybindings".to_string());
            };
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote-keybindings=") {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 1;
            continue;
        }

        cleaned.push(arg.clone());
        index += 1;
    }

    let remote = remote_target.map(|target| RemoteLaunch {
        target,
        keybindings,
        live_handoff,
    });
    if remote.is_none() && keybindings_seen {
        return Err("--remote-keybindings requires --remote".to_string());
    }
    if remote.is_none() && live_handoff {
        cleaned.push("--handoff".to_string());
    }

    Ok((cleaned, remote))
}

#[cfg(windows)]
fn validate_remote_target(target: &str) -> Result<&str, String> {
    if target.is_empty() {
        return Err("missing value for --remote".to_string());
    }
    if target.starts_with('-') {
        return Err("--remote target must not start with '-'".to_string());
    }
    Ok(target)
}

#[cfg(windows)]
pub(crate) fn run_remote(_remote: RemoteLaunch) -> std::io::Result<()> {
    debug_assert!(!crate::platform::capabilities().remote_attach);
    Err(std::io::Error::other(
        "remote mode is not supported on Windows yet",
    ))
}

#[cfg(windows)]
pub(crate) fn run_remote_client_bridge() -> std::io::Result<()> {
    debug_assert!(!crate::platform::capabilities().remote_attach);
    Err(std::io::Error::other(
        "remote client bridge is not supported on Windows yet",
    ))
}

pub(crate) fn print_remote_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_auth_error(err) {
        eprintln!(
            "hint: verify SSH access first with `{}`.",
            ssh_check_command(target)
        );
        eprintln!(
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before running `herdr --remote`."
        );
    }
}

fn is_remote_auth_error(err: &std::io::Error) -> bool {
    let message = err.to_string();
    message.contains("Permission denied")
        && (message.contains("(publickey")
            || message.contains("(keyboard-interactive")
            || message.contains("(password"))
}

fn ssh_check_command(target: &str) -> String {
    format!("ssh {}", shell_quote(target))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let err = std::io::Error::other(
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let err = std::io::Error::other(
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let err = std::io::Error::other("remote platform detection failed: unsupported platform");

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn ssh_check_command_quotes_remote_target() {
        assert_eq!(ssh_check_command("host name"), "ssh 'host name'");
    }
}
