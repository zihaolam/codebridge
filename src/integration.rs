//! Managed Claude Code and Codex hook registration.
//!
//! The registration model borrows the durable pieces of Herdr's integration
//! system: a versioned, product-owned hook file; explicit current/outdated
//! status; per-agent config paths; and structural JSON edits that preserve
//! unrelated user hooks. Codebridge still registers its lifecycle events
//! because those events are its status source of truth.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

const INTEGRATION_VERSION: u32 = 3;
const HOOK_NAME: &str = "codebridge-agent-state.sh";
const MARKER: &str = "CODEBRIDGE_INTEGRATION_ID=lifecycle";

const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolBatch",
    "Notification",
    "PermissionRequest",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn events(self) -> &'static [&'static str] {
        match self {
            Self::Claude => CLAUDE_EVENTS,
            Self::Codex => CODEX_EVENTS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    pub hook_path: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    NotInstalled,
    Outdated { found: u32, expected: u32 },
    Current { version: u32 },
}

pub fn config_dir(agent: Agent) -> io::Result<PathBuf> {
    let env_name = match agent {
        Agent::Claude => "CLAUDE_CONFIG_DIR",
        Agent::Codex => "CODEX_HOME",
    };
    if let Some(path) = std::env::var_os(env_name).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    Ok(home.join(match agent {
        Agent::Claude => ".claude",
        Agent::Codex => ".codex",
    }))
}

pub fn install(agent: Agent) -> io::Result<InstallPaths> {
    let dir = config_dir(agent)?;
    let binary = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "cb".to_owned());
    install_at(agent, &dir, &binary)
}

pub fn install_at(agent: Agent, dir: &Path, binary: &str) -> io::Result<InstallPaths> {
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} config directory not found at {}; install {} first",
                agent.label(),
                dir.display(),
                agent.label()
            ),
        ));
    }

    let hook_dir = match agent {
        Agent::Claude => dir.join("hooks"),
        Agent::Codex => dir.to_owned(),
    };
    fs::create_dir_all(&hook_dir)?;
    let hook_path = hook_dir.join(HOOK_NAME);
    write_atomic(&hook_path, hook_script(binary).as_bytes())?;
    make_executable(&hook_path)?;

    let config_path = match agent {
        Agent::Claude => dir.join("settings.json"),
        Agent::Codex => dir.join("hooks.json"),
    };
    let mut root = read_json_object(&config_path)?;
    let hooks = ensure_hooks(&mut root, &config_path)?;
    for event in agent.events() {
        remove_owned_commands(hooks, event);
        ensure_command(
            hooks,
            event,
            hook_command(&hook_path, event),
            matches!(agent, Agent::Claude).then_some("*"),
        )?;
    }
    write_json_with_backup(&config_path, &root)?;
    if agent == Agent::Codex {
        let codex_config = dir.join("config.toml");
        let existing = fs::read_to_string(&codex_config).unwrap_or_default();
        let enabled = codex_config_with_hooks(&existing);
        if enabled != existing {
            write_with_backup(&codex_config, enabled.as_bytes())?;
        }
    }

    Ok(InstallPaths {
        hook_path,
        config_path,
    })
}

pub fn status(agent: Agent) -> io::Result<Status> {
    status_at(agent, &config_dir(agent)?)
}

pub fn status_at(agent: Agent, dir: &Path) -> io::Result<Status> {
    let hook_path = match agent {
        Agent::Claude => dir.join("hooks").join(HOOK_NAME),
        Agent::Codex => dir.join(HOOK_NAME),
    };
    let content = match fs::read_to_string(&hook_path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Status::NotInstalled);
        }
        Err(error) => return Err(error),
    };
    let version = content
        .lines()
        .find_map(|line| line.strip_prefix("# CODEBRIDGE_INTEGRATION_VERSION="))
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    if version == INTEGRATION_VERSION && registration_current(agent, dir, &hook_path) {
        Ok(Status::Current { version })
    } else {
        Ok(Status::Outdated {
            found: version,
            expected: INTEGRATION_VERSION,
        })
    }
}

fn registration_current(agent: Agent, dir: &Path, hook_path: &Path) -> bool {
    let json_path = match agent {
        Agent::Claude => dir.join("settings.json"),
        Agent::Codex => dir.join("hooks.json"),
    };
    let root = fs::read_to_string(json_path)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok());
    let Some(hooks) = root
        .as_ref()
        .and_then(|root| root.get("hooks"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    let hook_name = hook_path.to_string_lossy();
    let events_current = agent.events().iter().all(|event| {
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|commands| {
                            commands.iter().any(|command| {
                                command.get("command").and_then(Value::as_str).is_some_and(
                                    |command| {
                                        command.contains(hook_name.as_ref())
                                            && command.trim_end_matches('\'').ends_with(event)
                                    },
                                )
                            })
                        })
                })
            })
    });
    events_current
        && (agent != Agent::Codex
            || fs::read_to_string(dir.join("config.toml"))
                .ok()
                .is_some_and(|content| codex_hooks_enabled(&content)))
}

pub fn uninstall(agent: Agent) -> io::Result<()> {
    let dir = config_dir(agent)?;
    uninstall_at(agent, &dir)
}

pub fn uninstall_at(agent: Agent, dir: &Path) -> io::Result<()> {
    let hook_path = match agent {
        Agent::Claude => dir.join("hooks").join(HOOK_NAME),
        Agent::Codex => dir.join(HOOK_NAME),
    };
    let config_path = match agent {
        Agent::Claude => dir.join("settings.json"),
        Agent::Codex => dir.join("hooks.json"),
    };
    if config_path.is_file() {
        let mut root = read_json_object(&config_path)?;
        if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            for event in agent.events() {
                remove_owned_commands(hooks, event);
            }
        }
        write_json_with_backup(&config_path, &root)?;
    }
    match fs::read_to_string(&hook_path) {
        Ok(content) if content.contains(MARKER) => fs::remove_file(hook_path),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn hook_script(binary: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # installed and managed by codebridge\n\
         # {MARKER}\n\
         # CODEBRIDGE_INTEGRATION_VERSION={INTEGRATION_VERSION}\n\
         event=\"${{1:-}}\"\n\
         [ -n \"$event\" ] || exit 0\n\
         exec {} hook \"$event\"\n",
        shell_quote(binary)
    )
}

fn hook_command(path: &Path, event: &str) -> String {
    format!(
        "sh {} {}",
        shell_quote(&path.display().to_string()),
        shell_quote(event)
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn read_json_object(path: &Path) -> io::Result<Value> {
    let value = match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse {}: {error}", path.display()),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error),
    };
    if !value.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} must contain a JSON object", path.display()),
        ));
    }
    Ok(value)
}

fn ensure_hooks<'a>(root: &'a mut Value, path: &Path) -> io::Result<&'a mut Map<String, Value>> {
    let object = root.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} must contain a JSON object", path.display()),
        )
    })?;
    object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("hooks in {} must be a JSON object", path.display()),
            )
        })
}

fn ensure_command(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: String,
    matcher: Option<&str>,
) -> io::Result<()> {
    let entries = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("hook entries for {event} must be an array"),
            )
        })?;
    let mut group = Map::new();
    if let Some(matcher) = matcher {
        group.insert("matcher".to_owned(), Value::String(matcher.to_owned()));
    }
    group.insert(
        "hooks".to_owned(),
        json!([{
            "type": "command",
            "command": command,
            "timeout": 10
        }]),
    );
    entries.push(Value::Object(group));
    Ok(())
}

fn remove_owned_commands(hooks: &mut Map<String, Value>, event: &str) {
    let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
        return;
    };
    entries.retain_mut(|entry| {
        let Some(commands) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        commands.retain(|hook| {
            let Some(command) = hook.get("command").and_then(Value::as_str) else {
                return true;
            };
            !is_owned_command(command)
        });
        !commands.is_empty()
    });
    if entries.is_empty() {
        hooks.remove(event);
    }
}

fn is_owned_command(command: &str) -> bool {
    command.contains(HOOK_NAME)
        || command
            .split_ascii_whitespace()
            .any(|part| matches!(part.trim_matches('\''), "cb" | "ccmgr"))
            && command.contains(" hook ")
}

fn write_json_with_backup(path: &Path, root: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(root)?;
    bytes.push(b'\n');
    write_with_backup(path, &bytes)
}

fn write_with_backup(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Ok(existing) = fs::read(path) {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}.bak"))
            .unwrap_or_else(|| "bak".to_owned());
        write_atomic(&path.with_extension(extension), &existing)?;
    }
    write_atomic(path, bytes)
}

fn codex_config_with_hooks(content: &str) -> String {
    let trailing_newline = content.ends_with('\n');
    let mut lines = content.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut in_features = false;
    let mut features = None;
    let mut hooks = None;
    let mut deprecated = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_features = trimmed == "[features]";
            if in_features && features.is_none() {
                features = Some(index);
            }
            continue;
        }
        if !in_features {
            continue;
        }
        let key = trimmed
            .split_once('=')
            .map(|(key, _)| key.trim())
            .unwrap_or_default();
        match key {
            "hooks" => hooks = Some(index),
            "codex_hooks" => deprecated.push(index),
            _ => {}
        }
    }
    if let Some(index) = hooks {
        lines[index] = "hooks = true".to_owned();
    }
    for index in deprecated.into_iter().rev() {
        lines.remove(index);
    }
    if hooks.is_none() {
        if let Some(index) = features {
            lines.insert(index + 1, "hooks = true".to_owned());
        } else {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.extend(["[features]".to_owned(), "hooks = true".to_owned()]);
        }
    }
    let mut output = lines.join("\n");
    if trailing_newline || output.is_empty() || !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn codex_hooks_enabled(content: &str) -> bool {
    let mut in_features = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_features = trimmed == "[features]";
            continue;
        }
        if in_features {
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim() == "hooks" {
                    return value
                        .split('#')
                        .next()
                        .is_some_and(|value| value.trim() == "true");
                }
            }
        }
    }
    false
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, bytes)?;
    fs::rename(temp, path)
}

fn make_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cb-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("temp config dir");
        path
    }

    #[test]
    fn claude_install_is_versioned_idempotent_and_preserves_user_hooks() {
        let dir = temp_dir("claude-hooks");
        fs::write(
            dir.join("settings.json"),
            r#"{"theme":"dark","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"user-hook"}]}]}}"#,
        )
        .expect("settings");

        let installed = install_at(Agent::Claude, &dir, "/opt/cb's/bin").expect("first install");
        install_at(Agent::Claude, &dir, "/opt/cb's/bin").expect("second install");

        assert_eq!(
            status_at(Agent::Claude, &dir).expect("status"),
            Status::Current {
                version: INTEGRATION_VERSION
            }
        );
        assert!(fs::read_to_string(&installed.hook_path)
            .expect("hook")
            .contains(MARKER));
        let root = read_json_object(&installed.config_path).expect("settings json");
        assert_eq!(root["theme"], "dark");
        let entries = root["hooks"]["SessionStart"]
            .as_array()
            .expect("session hooks");
        assert_eq!(entries.len(), 2);
        let entries_json = serde_json::to_string(entries).expect("serialize entries");
        assert!(entries_json.contains("user-hook"));
        assert_eq!(entries_json.matches(HOOK_NAME).count(), 1);
    }

    #[test]
    fn codex_install_uses_its_native_event_set_and_uninstalls_only_ours() {
        let dir = temp_dir("codex-hooks");
        fs::write(
            dir.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}"#,
        )
        .expect("hooks");
        fs::write(
            dir.join("config.toml"),
            "profile = \"work\"\n\n[profiles.work.features]\nhooks = false\n\
             codex_hooks = false\n\n[features]\ncodex_hooks = false\nother = true\n",
        )
        .expect("codex config");

        install_at(Agent::Codex, &dir, "cb").expect("install");
        let installed = read_json_object(&dir.join("hooks.json")).expect("installed hooks");
        assert!(installed["hooks"].get("Notification").is_none());
        assert!(installed["hooks"]["PermissionRequest"].is_array());
        let config = fs::read_to_string(dir.join("config.toml")).expect("codex config");
        assert!(config.contains("[features]\nhooks = true\nother = true"));
        assert!(config.contains("[profiles.work.features]\nhooks = false\ncodex_hooks = false"));
        assert_eq!(config.matches("hooks = true").count(), 1);

        uninstall_at(Agent::Codex, &dir).expect("uninstall");
        let uninstalled = read_json_object(&dir.join("hooks.json")).expect("uninstalled hooks");
        assert!(uninstalled["hooks"]["Stop"].to_string().contains("keep-me"));
        assert!(!uninstalled["hooks"]["Stop"].to_string().contains(HOOK_NAME));
        assert!(!dir.join(HOOK_NAME).exists());
        assert!(fs::read_to_string(dir.join("config.toml"))
            .expect("preserved config")
            .contains("hooks = true"));
    }

    #[test]
    fn old_managed_hook_is_reported_as_outdated() {
        let dir = temp_dir("outdated-hook");
        let hooks = dir.join("hooks");
        fs::create_dir_all(&hooks).expect("hooks dir");
        fs::write(
            hooks.join(HOOK_NAME),
            format!("# {MARKER}\n# CODEBRIDGE_INTEGRATION_VERSION=0\n"),
        )
        .expect("old hook");

        assert_eq!(
            status_at(Agent::Claude, &dir).expect("status"),
            Status::Outdated {
                found: 0,
                expected: INTEGRATION_VERSION
            }
        );
    }
}
