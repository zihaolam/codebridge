use crate::task::Task;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 16;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Working,
    NeedsApproval,
    WaitingUser,
    Idle,
    Ended,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub status: Status,
    pub last_message: String,
    pub harness_session_id: String,
    pub exited: bool,
    pub status_since_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Request {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub prefill: String,
    #[serde(default)]
    pub line_start: u32,
    #[serde(default)]
    pub line_end: u32,
    #[serde(default)]
    pub col_start: u16,
    #[serde(default)]
    pub col_end: u16,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub task_status: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<Task>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamUp {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub mouse_action: u8,
    #[serde(default)]
    pub mouse_button: u8,
    #[serde(default)]
    pub mouse_modifiers: u16,
    #[serde(default)]
    pub mouse_x: u16,
    #[serde(default)]
    pub mouse_y: u16,
    #[serde(default)]
    pub mouse_pressed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cell {
    pub symbol: String,
    pub fg: u32,
    pub bg: u32,
    pub modifiers: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalFrame {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Cell>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    #[serde(default)]
    pub mouse_reporting: bool,
    pub offset: usize,
    pub max_offset: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamDown {
    Frame { frame: TerminalFrame },
    Gone,
    Error { message: String },
}
