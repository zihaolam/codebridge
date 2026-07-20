use std::io::{self, Write};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

pub const DELIVERY_NAMES: &[&str] = &["all", "codebridge", "terminal", "system", "off"];
pub const MAX_DELAY_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Codebridge toast plus a native system notification. This preserves the
    /// behavior from before notification delivery became configurable.
    #[default]
    All,
    Codebridge,
    Terminal,
    System,
    Off,
}

impl Delivery {
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Codebridge => "codebridge",
            Self::Terminal => "terminal",
            Self::System => "system",
            Self::Off => "off",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "all" => Self::All,
            "codebridge" | "in-app" | "in_app" => Self::Codebridge,
            "terminal" => Self::Terminal,
            "system" => Self::System,
            "off" | "none" => Self::Off,
            _ => return None,
        })
    }

    pub fn shows_in_app(self) -> bool {
        matches!(self, Self::All | Self::Codebridge)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NotificationConfig {
    pub delivery: Delivery,
    pub delay_seconds: u64,
    pub notify_approval: bool,
    pub notify_done: bool,
    pub suppress_focused: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            delivery: Delivery::All,
            delay_seconds: 1,
            notify_approval: true,
            notify_done: true,
            suppress_focused: true,
        }
    }
}

impl NotificationConfig {
    pub fn bounded_delay_seconds(&self) -> u64 {
        self.delay_seconds.min(MAX_DELAY_SECONDS)
    }
}

pub fn send(delivery: Delivery, title: &str, body: &str) -> bool {
    if external_disabled() {
        return false;
    }
    match delivery {
        Delivery::All | Delivery::System => send_system(title, body),
        Delivery::Terminal => send_terminal(title, body).unwrap_or(false),
        Delivery::Codebridge | Delivery::Off => false,
    }
}

fn external_disabled() -> bool {
    std::env::var("CB_NO_NOTIFY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

fn send_system(title: &str, body: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("osascript");
        command.args([
            "-e",
            &format!(
                "display notification {} with title {}",
                applescript_string(body),
                applescript_string(title)
            ),
        ]);
        command
    } else if cfg!(target_os = "linux") {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return false;
        }
        let mut command = Command::new("notify-send");
        command.arg("--").args([title, body]);
        command
    } else {
        return false;
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalBackend {
    Osc9,
    Kitty,
}

fn detect_terminal_backend() -> Option<TerminalBackend> {
    let program = std::env::var("TERM_PROGRAM").ok();
    let term = std::env::var("TERM").ok();
    if std::env::var_os("KITTY_WINDOW_ID").is_some() || term.as_deref() == Some("xterm-kitty") {
        return Some(TerminalBackend::Kitty);
    }
    match program.as_deref() {
        Some("ghostty" | "iTerm.app" | "WezTerm") => Some(TerminalBackend::Osc9),
        _ if term
            .as_deref()
            .is_some_and(|term| term == "xterm-ghostty" || term.contains("wezterm")) =>
        {
            Some(TerminalBackend::Osc9)
        }
        _ => None,
    }
}

fn send_terminal(title: &str, body: &str) -> io::Result<bool> {
    let Some(backend) = detect_terminal_backend() else {
        return Ok(false);
    };
    let sequence = match backend {
        TerminalBackend::Osc9 => build_osc9(title, body),
        TerminalBackend::Kitty => build_osc99(title, body),
    };
    let sequence = if std::env::var_os("TMUX").is_some() {
        wrap_tmux(&sequence)
    } else {
        sequence
    };
    let mut stdout = io::stdout();
    stdout.write_all(&sequence)?;
    stdout.flush()?;
    Ok(true)
}

fn build_osc9(title: &str, body: &str) -> Vec<u8> {
    let message = sanitize(&format!("{title}: {body}"));
    format!("\x1b]9;{message}\x1b\\").into_bytes()
}

fn build_osc99(title: &str, body: &str) -> Vec<u8> {
    let title = sanitize(title);
    let body = sanitize(body);
    format!("\x1b]99;i=1:d=0;{title}\x1b\\\x1b]99;i=1:p=body;{body}\x1b\\").into_bytes()
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|character| !matches!(character, '\u{1b}' | '\u{7}' | '\u{9c}'))
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            _ => character,
        })
        .collect()
}

fn wrap_tmux(sequence: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(sequence.len() + 16);
    wrapped.extend_from_slice(b"\x1bPtmux;");
    for byte in sequence {
        if *byte == 0x1b {
            wrapped.push(0x1b);
        }
        wrapped.push(*byte);
    }
    wrapped.extend_from_slice(b"\x1b\\");
    wrapped
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_literals_are_escaped_without_a_shell() {
        assert_eq!(applescript_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(applescript_string("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn delivery_names_round_trip() {
        for name in DELIVERY_NAMES {
            assert_eq!(Delivery::from_name(name).map(Delivery::name), Some(*name));
        }
    }

    #[test]
    fn terminal_messages_strip_control_characters() {
        let message = String::from_utf8(build_osc9("done\u{1b}", "one\n two")).unwrap();
        assert_eq!(message, "\u{1b}]9;done: one  two\u{1b}\\");
    }

    #[test]
    fn kitty_notification_has_structured_title_and_body() {
        let message = String::from_utf8(build_osc99("Claude done", "command-center")).unwrap();
        assert!(message.contains("]99;i=1:d=0;Claude done"));
        assert!(message.contains("]99;i=1:p=body;command-center"));
    }

    #[test]
    fn tmux_passthrough_escapes_nested_escape_bytes() {
        assert_eq!(
            wrap_tmux(b"\x1b]9;hi\x1b\\"),
            b"\x1bPtmux;\x1b\x1b]9;hi\x1b\x1b\\\x1b\\"
        );
    }
}
