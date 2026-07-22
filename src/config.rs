use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::notify::NotificationConfig;
use crate::theme::ThemeConfig;

pub const DEFAULT_PREFIX: &str = "ctrl+a";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    pub id: &'static str,
    pub label: &'static str,
    pub default: &'static str,
}

pub const ACTIONS: &[Action] = &[
    Action {
        id: "new_claude",
        label: "new claude session",
        default: "n",
    },
    Action {
        id: "new_codex",
        label: "new codex session",
        default: "c",
    },
    Action {
        id: "new_worktree",
        label: "new session in worktree",
        default: "w",
    },
    Action {
        id: "kill",
        label: "kill session",
        default: "x",
    },
    Action {
        id: "rename",
        label: "rename session",
        default: "r",
    },
    Action {
        id: "jump_pending",
        label: "jump to pending approval",
        default: "g",
    },
    Action {
        id: "yank",
        label: "yank selection",
        default: "y",
    },
    Action {
        id: "scope_toggle",
        label: "toggle accordion / this-workspace",
        default: "a",
    },
    Action {
        id: "focus_screen",
        label: "focus screen pane",
        default: "l",
    },
    Action {
        id: "resize_pane",
        label: "resize session to this pane",
        default: "z",
    },
    Action {
        id: "scroll",
        label: "enter scroll mode",
        default: "[",
    },
    Action {
        id: "newline",
        label: "insert newline in session",
        default: "enter",
    },
    Action {
        id: "task_backlog",
        label: "task backlog",
        default: "t",
    },
    Action {
        id: "session_history",
        label: "resume past session",
        default: "m",
    },
    Action {
        id: "open_ide",
        label: "open dir in editor",
        default: "i",
    },
    Action {
        id: "config",
        label: "open config menu",
        default: "o",
    },
    Action {
        id: "quit",
        label: "quit cb",
        default: "q",
    },
];

/// Which editor `open_ide` (prefix `i`) launches for the focused pane's
/// directory. A bare string is a named preset; the object form is the escape
/// hatch for any command line. `{dir}` in a custom command is replaced with the
/// directory (and, for a non-popup command with no `{dir}`, the directory is
/// appended). See [`Ide::resolve`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Ide {
    /// A named preset: `nvim`/`vim`/`hx` (terminal editors, opened in a popup),
    /// `cursor`/`vscode`/`code` (GUI editors, launched detached), or any other
    /// binary name (treated as a GUI editor taking the directory).
    Preset(String),
    /// A fully explicit command. `popup` true runs it inside cb's editor popup
    /// (a PTY session); false launches it as a detached process.
    Custom {
        command: Vec<String>,
        #[serde(default)]
        popup: bool,
    },
}

impl Default for Ide {
    fn default() -> Self {
        Ide::Preset("nvim".to_owned())
    }
}

/// A resolved editor launch: the argv (before `{dir}` substitution) and whether
/// it runs inside cb's popup (a terminal editor) or as a detached GUI process.
pub struct ResolvedIde {
    pub command: Vec<String>,
    pub popup: bool,
}

impl Ide {
    pub fn resolve(&self) -> ResolvedIde {
        match self {
            Ide::Custom { command, popup } => ResolvedIde {
                command: command.clone(),
                popup: *popup,
            },
            Ide::Preset(name) => {
                let popup_editor = |binary: &str| ResolvedIde {
                    command: vec![binary.to_owned(), ".".to_owned()],
                    popup: true,
                };
                let gui_editor = |binary: &str| ResolvedIde {
                    command: vec![binary.to_owned(), "{dir}".to_owned()],
                    popup: false,
                };
                match name.trim().to_ascii_lowercase().as_str() {
                    "nvim" | "neovim" => popup_editor("nvim"),
                    "vim" => popup_editor("vim"),
                    "hx" | "helix" => popup_editor("hx"),
                    "cursor" => gui_editor("cursor"),
                    "code" | "vscode" => gui_editor("code"),
                    other => gui_editor(other),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub prefix: String,
    pub bindings: HashMap<String, String>,
    pub theme: ThemeConfig,
    pub notifications: NotificationConfig,
    pub ide: Ide,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: DEFAULT_PREFIX.to_owned(),
            bindings: ACTIONS
                .iter()
                .map(|action| (action.id.to_owned(), action.default.to_owned()))
                .collect(),
            theme: ThemeConfig::default(),
            notifications: NotificationConfig::default(),
            ide: Ide::default(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        Self::load_from(config_path())
    }

    pub fn load_from(path: Option<PathBuf>) -> Self {
        let mut config = Self::default();
        let Some(path) = path else {
            return config;
        };
        let Ok(bytes) = fs::read(path) else {
            return config;
        };
        let Ok(stored) = serde_json::from_slice::<Self>(&bytes) else {
            return config;
        };
        if !stored.prefix.trim().is_empty() {
            config.prefix = stored.prefix;
        }
        for (id, key) in stored.bindings {
            if config.bindings.contains_key(&id) && !key.trim().is_empty() {
                config.bindings.insert(id, key);
            }
        }
        config.theme = stored.theme;
        config.notifications = stored.notifications;
        config.ide = stored.ide;
        config
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| io::Error::other("no home directory"))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        fs::write(path, bytes)
    }

    pub fn effective_prefix(&self) -> String {
        std::env::var("CB_PREFIX")
            .ok()
            .filter(|prefix| !prefix.trim().is_empty())
            .unwrap_or_else(|| self.prefix.clone())
    }

    pub fn action_for_key(&self, key: &str) -> Option<&str> {
        self.bindings
            .iter()
            .find_map(|(action, bound)| (bound == key).then_some(action.as_str()))
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path).join("cb/config.json"));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config/cb/config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_inherits_defaults_and_drops_unknown_actions() {
        let path = std::env::temp_dir().join(format!(
            "cb-config-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(
            &path,
            br#"{"prefix":"ctrl+b","bindings":{"new_claude":"m","unknown":"z"}}"#,
        )
        .unwrap();
        let config = Config::load_from(Some(path.clone()));
        let _ = fs::remove_file(path);
        assert_eq!(config.prefix, "ctrl+b");
        assert_eq!(config.bindings["new_claude"], "m");
        assert_eq!(config.bindings["quit"], "q");
        assert!(!config.bindings.contains_key("unknown"));
        assert_eq!(config.theme.name, crate::theme::DEFAULT_THEME);
    }

    #[test]
    fn ide_defaults_to_nvim_popup_and_presets_resolve() {
        assert_eq!(Config::default().ide, Ide::Preset("nvim".to_owned()));

        let nvim = Ide::Preset("nvim".to_owned()).resolve();
        assert!(nvim.popup);
        assert_eq!(nvim.command, vec!["nvim".to_owned(), ".".to_owned()]);

        let cursor = Ide::Preset("cursor".to_owned()).resolve();
        assert!(!cursor.popup);
        assert_eq!(
            cursor.command,
            vec!["cursor".to_owned(), "{dir}".to_owned()]
        );

        // `vscode` maps to the `code` CLI; an unknown name is treated as a GUI
        // binary taking the directory.
        assert_eq!(
            Ide::Preset("vscode".to_owned()).resolve().command[0],
            "code"
        );
        let other = Ide::Preset("zed".to_owned()).resolve();
        assert!(!other.popup);
        assert_eq!(other.command, vec!["zed".to_owned(), "{dir}".to_owned()]);
    }

    #[test]
    fn ide_parses_preset_string_and_custom_object() {
        let path = std::env::temp_dir().join(format!("cb-ide-preset-{}.json", std::process::id()));
        fs::write(&path, br#"{"ide":"cursor"}"#).unwrap();
        let config = Config::load_from(Some(path.clone()));
        let _ = fs::remove_file(path);
        assert_eq!(config.ide, Ide::Preset("cursor".to_owned()));

        let path = std::env::temp_dir().join(format!("cb-ide-custom-{}.json", std::process::id()));
        fs::write(&path, br#"{"ide":{"command":["hx","{dir}"],"popup":true}}"#).unwrap();
        let config = Config::load_from(Some(path.clone()));
        let _ = fs::remove_file(path);
        let resolved = config.ide.resolve();
        assert!(resolved.popup);
        assert_eq!(resolved.command, vec!["hx".to_owned(), "{dir}".to_owned()]);
        // An absent `ide` key still keeps every binding default.
        assert_eq!(config.bindings["quit"], "q");
    }

    #[test]
    fn corrupt_config_falls_back_to_defaults() {
        let path =
            std::env::temp_dir().join(format!("cb-corrupt-config-{}.json", std::process::id()));
        fs::write(&path, b"not json").unwrap();
        let config = Config::load_from(Some(path.clone()));
        let _ = fs::remove_file(path);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn theme_config_loads_without_losing_binding_defaults() {
        let path =
            std::env::temp_dir().join(format!("cb-theme-config-{}.json", std::process::id()));
        fs::write(
            &path,
            br##"{"theme":{"name":"nord","custom":{"accent":"#abcdef"}}}"##,
        )
        .unwrap();
        let config = Config::load_from(Some(path.clone()));
        let _ = fs::remove_file(path);
        assert_eq!(config.theme.name, "nord");
        assert_eq!(config.theme.custom.accent.as_deref(), Some("#abcdef"));
        assert_eq!(config.bindings["quit"], "q");
    }

    #[test]
    fn notification_config_loads_and_old_configs_keep_compatible_defaults() {
        let path =
            std::env::temp_dir().join(format!("cb-notify-config-{}.json", std::process::id()));
        fs::write(
            &path,
            br#"{"notifications":{"delivery":"terminal","delay_seconds":3,"notify_done":false}}"#,
        )
        .unwrap();
        let config = Config::load_from(Some(path.clone()));
        let _ = fs::remove_file(path);
        assert_eq!(
            config.notifications.delivery,
            crate::notify::Delivery::Terminal
        );
        assert_eq!(config.notifications.delay_seconds, 3);
        assert!(!config.notifications.notify_done);
        assert!(config.notifications.notify_approval);

        let path = std::env::temp_dir().join(format!("cb-old-config-{}.json", std::process::id()));
        fs::write(&path, br#"{"prefix":"ctrl+b"}"#).unwrap();
        let config = Config::load_from(Some(path.clone()));
        let _ = fs::remove_file(path);
        assert_eq!(config.notifications.delivery, crate::notify::Delivery::All);
    }
}
