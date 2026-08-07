use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshHost {
    pub(crate) alias: String,
    pub(crate) target_hint: String,
}

#[derive(Debug)]
pub(crate) enum SshConfigError {
    HomeUnavailable,
    Read {
        path: PathBuf,
        source: io::Error,
    },
    IncludeCycle {
        path: PathBuf,
    },
    InvalidLine {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

impl fmt::Display for SshConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeUnavailable => {
                write!(
                    formatter,
                    "could not determine the home directory for SSH config"
                )
            }
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read SSH config {}: {source}",
                    path.display()
                )
            }
            Self::IncludeCycle { path } => {
                write!(
                    formatter,
                    "SSH config Include cycle detected at {}",
                    path.display()
                )
            }
            Self::InvalidLine {
                path,
                line,
                message,
            } => write!(
                formatter,
                "invalid SSH config line {line} in {}: {message}",
                path.display()
            ),
        }
    }
}

impl Error for SshConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::HomeUnavailable | Self::IncludeCycle { .. } | Self::InvalidLine { .. } => None,
        }
    }
}

pub(crate) fn user_config_path() -> Result<PathBuf, SshConfigError> {
    let home = home_directory()?;
    Ok(home.join(".ssh").join("config"))
}

pub(crate) fn parse_file(path: &Path) -> Result<Vec<SshHost>, SshConfigError> {
    let include_root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let mut state = ParseState {
        blocks: Vec::new(),
        include_root,
        stack: Vec::new(),
    };
    let mut context = ParseContext::Global;
    collect_file(path, &mut context, &mut state)?;
    Ok(resolve_hosts(&state.blocks))
}

fn home_directory() -> Result<PathBuf, SshConfigError> {
    #[cfg(windows)]
    let variables = ["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let variables = ["HOME", "USERPROFILE"];

    variables
        .into_iter()
        .find_map(|variable| {
            std::env::var_os(variable)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .ok_or(SshConfigError::HomeUnavailable)
}

struct ParseState {
    blocks: Vec<HostBlock>,
    include_root: PathBuf,
    stack: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
enum ParseContext {
    Global,
    Host(usize),
    Match,
}

struct HostBlock {
    patterns: Vec<String>,
    options: HashMap<String, String>,
}

fn collect_file(
    path: &Path,
    context: &mut ParseContext,
    state: &mut ParseState,
) -> Result<(), SshConfigError> {
    let identity = match std::fs::canonicalize(path) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(source) => {
            return Err(SshConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if state.stack.iter().any(|current| current == &identity) {
        return Err(SshConfigError::IncludeCycle {
            path: path.to_path_buf(),
        });
    }

    let content = std::fs::read_to_string(path).map_err(|source| SshConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    state.stack.push(identity);
    let original_context = *context;
    let result = collect_content(path, &content, context, state);
    *context = original_context;
    state.stack.pop();
    result
}

fn collect_content(
    path: &Path,
    content: &str,
    context: &mut ParseContext,
    state: &mut ParseState,
) -> Result<(), SshConfigError> {
    let mut logical_line = String::new();

    for (line_index, raw_line) in content.lines().enumerate() {
        let line_number = line_index + 1;
        let raw_line = raw_line.trim_end();
        if let Some(line) = raw_line.strip_suffix('\\') {
            logical_line.push_str(line);
            logical_line.push(' ');
            continue;
        }
        logical_line.push_str(raw_line);
        process_line(path, line_number, &logical_line, context, state)?;
        logical_line.clear();
    }
    if !logical_line.is_empty() {
        process_line(
            path,
            content.lines().count().max(1),
            &logical_line,
            context,
            state,
        )?;
    }
    Ok(())
}

fn process_line(
    path: &Path,
    line_number: usize,
    line: &str,
    context: &mut ParseContext,
    state: &mut ParseState,
) -> Result<(), SshConfigError> {
    let tokens = tokenize(line).map_err(|message| SshConfigError::InvalidLine {
        path: path.to_path_buf(),
        line: line_number,
        message,
    })?;
    if tokens.is_empty() {
        return Ok(());
    }

    let (keyword, values) = split_directive(&tokens);
    match keyword.to_ascii_lowercase().as_str() {
        "host" => {
            if values.is_empty() {
                return Err(SshConfigError::InvalidLine {
                    path: path.to_path_buf(),
                    line: line_number,
                    message: "Host requires at least one alias or pattern".into(),
                });
            }
            state.blocks.push(HostBlock {
                patterns: values,
                options: HashMap::new(),
            });
            *context = ParseContext::Host(state.blocks.len() - 1);
        }
        "match" => {
            if values.is_empty() {
                return Err(SshConfigError::InvalidLine {
                    path: path.to_path_buf(),
                    line: line_number,
                    message: "Match requires at least one condition".into(),
                });
            }
            *context = ParseContext::Match;
        }
        "include" => {
            if values.is_empty() {
                return Err(SshConfigError::InvalidLine {
                    path: path.to_path_buf(),
                    line: line_number,
                    message: "Include requires at least one path".into(),
                });
            }
            for pattern in values {
                for included_path in include_paths(&state.include_root, &pattern)? {
                    match collect_file(&included_path, context, state) {
                        Err(SshConfigError::Read { source, .. })
                            if source.kind() == io::ErrorKind::NotFound => {}
                        result => result?,
                    }
                }
            }
        }
        "hostname" | "user" | "port" => {
            if values.is_empty() {
                return Err(SshConfigError::InvalidLine {
                    path: path.to_path_buf(),
                    line: line_number,
                    message: format!("{keyword} requires a value"),
                });
            }
            let index = match *context {
                ParseContext::Host(index) => Some(index),
                ParseContext::Global => {
                    state.blocks.push(HostBlock {
                        patterns: vec!["*".into()],
                        options: HashMap::new(),
                    });
                    Some(state.blocks.len() - 1)
                }
                ParseContext::Match => None,
            };
            if let Some(index) = index {
                state.blocks[index]
                    .options
                    .entry(keyword.to_ascii_lowercase())
                    .or_insert_with(|| values.join(" "));
            }
        }
        _ => {}
    }
    Ok(())
}

fn split_directive(tokens: &[String]) -> (String, Vec<String>) {
    let first = &tokens[0];
    if let Some((keyword, value)) = first.split_once('=') {
        let mut values = Vec::with_capacity(tokens.len());
        if !value.is_empty() {
            values.push(value.to_string());
        }
        values.extend(tokens.iter().skip(1).cloned());
        (keyword.to_string(), values)
    } else if let Some(second) = tokens.get(1) {
        if second == "=" {
            (first.clone(), tokens.iter().skip(2).cloned().collect())
        } else if let Some(value) = second.strip_prefix('=') {
            let mut values = Vec::with_capacity(tokens.len().saturating_sub(1));
            if !value.is_empty() {
                values.push(value.to_string());
            }
            values.extend(tokens.iter().skip(2).cloned());
            (first.clone(), values)
        } else {
            (first.clone(), tokens.iter().skip(1).cloned().collect())
        }
    } else {
        (first.clone(), Vec::new())
    }
}

fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in line.chars() {
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    token.push(character);
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    escaped = true;
                } else {
                    token.push(character);
                }
            }
            None => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => escaped = true,
                '#' => break,
                character if character.is_whitespace() => {
                    if !token.is_empty() {
                        tokens.push(std::mem::take(&mut token));
                    }
                }
                character => token.push(character),
            },
            Some(_) => unreachable!("tokenizer only tracks single and double quotes"),
        }
    }

    if escaped {
        token.push('\\');
    }
    if quote.is_some() {
        return Err("unterminated quote".into());
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

fn include_paths(include_root: &Path, pattern: &str) -> Result<Vec<PathBuf>, SshConfigError> {
    let pattern = expand_home(pattern)?;
    let include_path = if Path::new(&pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        include_root.join(pattern)
    };
    if !contains_glob(&include_path) {
        return match std::fs::metadata(&include_path) {
            Ok(_) => Ok(vec![include_path]),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(SshConfigError::Read {
                path: include_path,
                source,
            }),
        };
    }

    expand_glob(&include_path).map_err(|source| SshConfigError::Read {
        path: include_path,
        source,
    })
}

fn expand_home(pattern: &str) -> Result<String, SshConfigError> {
    if pattern == "~" {
        return home_directory().map(|home| home.to_string_lossy().into_owned());
    }
    if let Some(rest) = pattern.strip_prefix("~/") {
        return home_directory().map(|home| home.join(rest).to_string_lossy().into_owned());
    }
    Ok(pattern.to_string())
}

fn contains_glob(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(|character| matches!(character, '*' | '?' | '['))
    })
}

fn expand_glob(path: &Path) -> io::Result<Vec<PathBuf>> {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect::<Vec<_>>();
    let first_glob = components
        .iter()
        .position(|component| {
            component
                .to_string_lossy()
                .chars()
                .any(|c| matches!(c, '*' | '?' | '['))
        })
        .unwrap_or(components.len());
    let mut prefix = PathBuf::new();
    for component in components.iter().take(first_glob) {
        prefix.push(component);
    }

    let mut matches = Vec::new();
    expand_glob_components(&prefix, &components, first_glob, &mut matches)?;
    matches.sort();
    Ok(matches)
}

fn expand_glob_components(
    current: &Path,
    components: &[std::ffi::OsString],
    index: usize,
    matches: &mut Vec<PathBuf>,
) -> io::Result<()> {
    if index == components.len() {
        match std::fs::metadata(current) {
            Ok(_) => matches.push(current.to_path_buf()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        return Ok(());
    }

    let component = components[index].to_string_lossy();
    if component.chars().any(|c| matches!(c, '*' | '?' | '[')) {
        let entries = match std::fs::read_dir(current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            if glob_matches(&component, &name.to_string_lossy()) {
                expand_glob_components(&entry.path(), components, index + 1, matches)?;
            }
        }
    } else {
        expand_glob_components(
            &current.join(components[index].as_os_str()),
            components,
            index + 1,
            matches,
        )?;
    }
    Ok(())
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    if value.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut table = vec![vec![None; value.len() + 1]; pattern.len() + 1];
    glob_matches_at(&pattern, &value, 0, 0, &mut table)
}

fn glob_matches_at(
    pattern: &[char],
    value: &[char],
    pattern_index: usize,
    value_index: usize,
    table: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = table[pattern_index][value_index] {
        return result;
    }
    let result = if pattern_index == pattern.len() {
        value_index == value.len()
    } else if pattern[pattern_index] == '*' {
        glob_matches_at(pattern, value, pattern_index + 1, value_index, table)
            || (value_index < value.len()
                && glob_matches_at(pattern, value, pattern_index, value_index + 1, table))
    } else if value_index == value.len() {
        false
    } else if pattern[pattern_index] == '?' {
        glob_matches_at(pattern, value, pattern_index + 1, value_index + 1, table)
    } else if pattern[pattern_index] == '[' {
        match bracket_match(pattern, value[value_index], pattern_index) {
            Some((matched, next_index)) if matched => {
                glob_matches_at(pattern, value, next_index, value_index + 1, table)
            }
            Some(_) => false,
            None => {
                pattern[pattern_index] == value[value_index]
                    && glob_matches_at(pattern, value, pattern_index + 1, value_index + 1, table)
            }
        }
    } else {
        pattern[pattern_index] == value[value_index]
            && glob_matches_at(pattern, value, pattern_index + 1, value_index + 1, table)
    };
    table[pattern_index][value_index] = Some(result);
    result
}

fn bracket_match(pattern: &[char], value: char, start: usize) -> Option<(bool, usize)> {
    let mut index = start + 1;
    if index >= pattern.len() {
        return None;
    }
    let negated = matches!(pattern[index], '!' | '^');
    if negated {
        index += 1;
    }
    let mut matched = false;
    let mut has_character = false;
    let mut previous = None;
    while index < pattern.len() {
        if pattern[index] == ']' && has_character {
            return Some((if negated { !matched } else { matched }, index + 1));
        }
        let character = pattern[index];
        if character == '\\' && index + 1 < pattern.len() {
            index += 1;
            previous = Some(pattern[index]);
            has_character = true;
            matched |= value == pattern[index];
            index += 1;
            continue;
        }
        if character == '-'
            && previous.is_some()
            && index + 1 < pattern.len()
            && pattern[index + 1] != ']'
        {
            let end = pattern[index + 1];
            let Some(start) = previous else {
                index += 1;
                continue;
            };
            matched |= start <= value && value <= end;
            previous = Some(end);
            index += 2;
            has_character = true;
            continue;
        }
        previous = Some(character);
        has_character = true;
        matched |= value == character;
        index += 1;
    }
    None
}

fn host_pattern_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut table = vec![vec![None; value.len() + 1]; pattern.len() + 1];
    host_pattern_matches_at(&pattern, &value, 0, 0, &mut table)
}

fn host_pattern_matches_at(
    pattern: &[char],
    value: &[char],
    pattern_index: usize,
    value_index: usize,
    table: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = table[pattern_index][value_index] {
        return result;
    }
    let result = if pattern_index == pattern.len() {
        value_index == value.len()
    } else if pattern[pattern_index] == '*' {
        host_pattern_matches_at(pattern, value, pattern_index + 1, value_index, table)
            || (value_index < value.len()
                && host_pattern_matches_at(pattern, value, pattern_index, value_index + 1, table))
    } else if value_index == value.len() {
        false
    } else {
        (pattern[pattern_index] == '?' || pattern[pattern_index] == value[value_index])
            && host_pattern_matches_at(pattern, value, pattern_index + 1, value_index + 1, table)
    };
    table[pattern_index][value_index] = Some(result);
    result
}

fn resolve_hosts(blocks: &[HostBlock]) -> Vec<SshHost> {
    let mut aliases = Vec::new();
    for block in blocks {
        for pattern in &block.patterns {
            if pattern.is_empty()
                || pattern
                    .chars()
                    .any(|character| matches!(character, '*' | '?' | '!'))
            {
                continue;
            }
            if !aliases.iter().any(|alias: &String| alias == pattern) {
                aliases.push(pattern.clone());
            }
        }
    }

    aliases
        .into_iter()
        .map(|alias| {
            let mut options = HashMap::new();
            for block in blocks {
                if !block_matches(block, &alias) {
                    continue;
                }
                for key in ["hostname", "user", "port"] {
                    if let Some(value) = block.options.get(key) {
                        options.entry(key).or_insert_with(|| value.clone());
                    }
                }
            }

            let hostname = options
                .get("hostname")
                .map(|value| expand_hostname(value, &alias))
                .unwrap_or_else(|| alias.clone());
            let user = options.get("user");
            let mut target_hint = match user {
                Some(user) => format!("{user}@{hostname}"),
                None => hostname,
            };
            if let Some(port) = options.get("port") {
                target_hint.push(':');
                target_hint.push_str(port);
            }
            SshHost { alias, target_hint }
        })
        .collect()
}

fn expand_hostname(value: &str, alias: &str) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            expanded.push(character);
            continue;
        }
        match characters.next() {
            Some('%') => expanded.push('%'),
            Some('h') => expanded.push_str(alias),
            Some(next) => {
                expanded.push('%');
                expanded.push(next);
            }
            None => expanded.push('%'),
        }
    }
    expanded
}

fn block_matches(block: &HostBlock, alias: &str) -> bool {
    let mut positive_match = false;
    for pattern in &block.patterns {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern.as_str()), |pattern| (true, pattern));
        if host_pattern_matches(pattern, alias) {
            if negated {
                return false;
            }
            positive_match = true;
        }
    }
    positive_match
}

#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture_dir(label: &str) -> PathBuf {
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-ssh-config-{label}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_fixture(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn include_directives_are_followed_and_target_hints_are_resolved() {
        let directory = fixture_dir("include");
        let config = directory.join("config");
        write_fixture(
            &directory.join("conf.d/included"),
            "Host included\n  HostName included.example.test\n  User alice\n",
        );
        write_fixture(
            &config,
            "Include conf.d/*\nHost direct\n  HostName direct.example.test\n  Port 2222\n",
        );

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![
                SshHost {
                    alias: "included".into(),
                    target_hint: "alice@included.example.test".into(),
                },
                SshHost {
                    alias: "direct".into(),
                    target_hint: "direct.example.test:2222".into(),
                },
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn includes_keep_their_host_context_and_are_read_each_time() {
        let directory = fixture_dir("include-context-repeat");
        let config = directory.join("config");
        write_fixture(&directory.join("shared.conf"), "User shared\n");
        write_fixture(
            &config,
            "Host app\n  Include shared.conf\nHost other\n  User other\n  Include shared.conf\n",
        );

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![
                SshHost {
                    alias: "app".into(),
                    target_hint: "shared@app".into(),
                },
                SshHost {
                    alias: "other".into(),
                    target_hint: "other@other".into(),
                },
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn nested_relative_includes_use_the_user_ssh_directory() {
        let directory = fixture_dir("include-root");
        let config = directory.join("config");
        write_fixture(
            &directory.join("conf.d/first.conf"),
            "Include nested/second.conf\n",
        );
        write_fixture(
            &directory.join("nested/second.conf"),
            "Host nested\n  HostName nested.example.test\n",
        );
        write_fixture(&config, "Include conf.d/first.conf\n");

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![SshHost {
                alias: "nested".into(),
                target_hint: "nested.example.test".into(),
            }]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_include_is_ignored_like_openssh() {
        let directory = fixture_dir("include-missing");
        let config = directory.join("config");
        write_fixture(&config, "Include does-not-exist.conf\nHost available\n");

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![SshHost {
                alias: "available".into(),
                target_hint: "available".into(),
            }]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn include_globs_support_character_classes_in_lexical_order() {
        let directory = fixture_dir("include-glob-class");
        let config = directory.join("config");
        write_fixture(
            &directory.join("conf.2"),
            "Host second\n  HostName second.example.test\n",
        );
        write_fixture(
            &directory.join("conf.1"),
            "Host first\n  HostName first.example.test\n",
        );
        write_fixture(
            &directory.join("conf.a"),
            "Host lowercase\n  HostName lowercase.example.test\n",
        );
        write_fixture(&config, "Include conf.[0-9]\n");

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![
                SshHost {
                    alias: "first".into(),
                    target_hint: "first.example.test".into(),
                },
                SshHost {
                    alias: "second".into(),
                    target_hint: "second.example.test".into(),
                },
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn match_blocks_do_not_modify_the_preceding_host() {
        let directory = fixture_dir("match-context");
        let config = directory.join("config");
        write_fixture(
            &config,
            "Host valid\n  User actual\nMatch all\n  User conditional\n  HostName conditional.example.test\n  Port 2200\n",
        );

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![SshHost {
                alias: "valid".into(),
                target_hint: "actual@valid".into(),
            }]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn include_inside_match_does_not_modify_the_preceding_host() {
        let directory = fixture_dir("match-include-context");
        let config = directory.join("config");
        write_fixture(&directory.join("conditional.conf"), "User conditional\n");
        write_fixture(
            &config,
            "Host valid\n  User actual\nMatch all\n  Include conditional.conf\n",
        );

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![SshHost {
                alias: "valid".into(),
                target_hint: "actual@valid".into(),
            }]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn equals_syntax_and_hostname_tokens_match_supported_ssh_forms() {
        let directory = fixture_dir("directive-syntax");
        let config = directory.join("config");
        write_fixture(
            &config,
            "Host = equals\n  HostName = %h.example.test/%%/%n\n",
        );

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![SshHost {
                alias: "equals".into(),
                target_hint: "equals.example.test/%/%n".into(),
            }]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_supported_directive_values_return_typed_errors() {
        let directory = fixture_dir("directive-errors");
        let config = directory.join("config");
        write_fixture(&config, "Host alias\n  Port =\n");

        let error = parse_file(&config).unwrap_err();

        assert!(matches!(error, SshConfigError::InvalidLine { line: 2, .. }));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn pattern_only_host_entries_are_excluded() {
        let directory = fixture_dir("patterns");
        let config = directory.join("config");
        write_fixture(
            &config,
            "Host *.example ?wildcard !excluded\n  HostName ignored.example.test\nHost concrete\n  HostName concrete.example.test\n",
        );

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![SshHost {
                alias: "concrete".into(),
                target_hint: "concrete.example.test".into(),
            }]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn multiple_aliases_on_one_host_line_become_entries() {
        let directory = fixture_dir("aliases");
        let config = directory.join("config");
        write_fixture(&config, "Host one two\n  HostName shared.example.test\n");

        let hosts = parse_file(&config).unwrap();

        assert_eq!(
            hosts,
            vec![
                SshHost {
                    alias: "one".into(),
                    target_hint: "shared.example.test".into(),
                },
                SshHost {
                    alias: "two".into(),
                    target_hint: "shared.example.test".into(),
                },
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_or_unreadable_config_returns_typed_error() {
        let directory = fixture_dir("errors");
        let missing = directory.join("missing");
        let missing_error = parse_file(&missing).unwrap_err();
        assert!(matches!(
            missing_error,
            SshConfigError::Read { source, .. }
                if source.kind() == io::ErrorKind::NotFound
        ));

        let unreadable = directory.join("directory");
        std::fs::create_dir_all(&unreadable).unwrap();
        let unreadable_error = parse_file(&unreadable).unwrap_err();
        assert!(matches!(unreadable_error, SshConfigError::Read { .. }));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let permission_denied = directory.join("permission-denied");
            write_fixture(&permission_denied, "Host denied\n");
            let mut permissions = std::fs::metadata(&permission_denied).unwrap().permissions();
            permissions.set_mode(0o000);
            std::fs::set_permissions(&permission_denied, permissions).unwrap();
            let permission_error = parse_file(&permission_denied).unwrap_err();

            let mut restored = std::fs::metadata(&permission_denied).unwrap().permissions();
            restored.set_mode(0o600);
            std::fs::set_permissions(&permission_denied, restored).unwrap();
            assert!(matches!(
                permission_error,
                SshConfigError::Read { source, .. }
                    if source.kind() == io::ErrorKind::PermissionDenied
            ));
        }
        let _ = std::fs::remove_dir_all(directory);
    }
}
