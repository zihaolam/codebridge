//! Resolving the agent-generated conversation title for a recorded session.
//!
//! Claude writes a summarised title into its transcript JSONL as recurring
//! `{"type":"ai-title","aiTitle":"…"}` entries — the same title its own
//! `--resume` picker shows. The value is regenerated as the conversation
//! evolves and its latest occurrence sits within the final few percent of the
//! file, so a bounded tail read finds the current title regardless of transcript
//! size. Current Codex versions keep thread titles in `state_5.sqlite`; older
//! versions used `{"id":"…","thread_name":"…"}` rows in
//! `session_index.jsonl`, which remains a fallback for historical installs.
//!
//! Both are keyed by an id Codebridge already stores, so the historical-session
//! picker (`prefix m`) can label rows with the agent's own summary and fall back
//! to the first prompt when none exists yet — a title is generated lazily,
//! several turns in, so it is absent at the moment a session is first recorded.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Never read more than this many bytes from the end of a transcript. The
/// current `ai-title` recurs and its last occurrence sits near the end, so this
/// bound keeps the read cheap even for very large transcripts.
const TAIL_BYTES: u64 = 256 * 1024;

/// Caches resolved titles so a snapshot only re-reads a source file when it has
/// actually changed (by mtime + length). Steady state costs one `stat` per run.
#[derive(Default)]
pub struct TitleCache {
    /// Claude: keyed by transcript path -> (source fingerprint, title).
    claude: Mutex<HashMap<PathBuf, ClaudeEntry>>,
    /// Codex: current SQLite titles merged over the legacy JSONL index.
    codex: Mutex<CodexEntry>,
}

struct ClaudeEntry {
    mtime: Option<SystemTime>,
    len: u64,
    title: Option<String>,
}

#[derive(Default)]
struct CodexEntry {
    fingerprint: CodexFingerprint,
    names: HashMap<String, String>,
}

#[derive(Default, PartialEq, Eq)]
struct CodexFingerprint {
    database: FileFingerprint,
    wal: FileFingerprint,
    index: FileFingerprint,
}

#[derive(Default, PartialEq, Eq)]
struct FileFingerprint {
    mtime: Option<SystemTime>,
    len: u64,
}

impl TitleCache {
    /// Best-effort agent-summarised title for a recorded run. Returns `None`
    /// when the agent has not produced one yet (or does not produce one at all),
    /// so the caller keeps whatever it already had and falls back to the first
    /// prompt. Never returns an empty string.
    pub fn resolve(
        &self,
        agent: &str,
        transcript_path: &str,
        agent_session_id: &str,
        cwd: &str,
    ) -> Option<String> {
        match agent {
            "codex" => self.codex_title(agent_session_id),
            "claude" => {
                let path = claude_transcript_path(transcript_path, agent_session_id, cwd)?;
                self.claude_title(&path)
            }
            _ => None,
        }
    }

    fn claude_title(&self, path: &Path) -> Option<String> {
        let meta = fs::metadata(path).ok();
        let mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mut cache = self.claude.lock().ok()?;
        if let Some(entry) = cache.get(path) {
            if entry.mtime == mtime && entry.len == len {
                return entry.title.clone();
            }
        }
        let title = read_tail(path, TAIL_BYTES)
            .as_deref()
            .and_then(last_ai_title);
        cache.insert(
            path.to_path_buf(),
            ClaudeEntry {
                mtime,
                len,
                title: title.clone(),
            },
        );
        title
    }

    fn codex_title(&self, id: &str) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        let base = codex_home()?;
        let database = base.join("state_5.sqlite");
        let wal = base.join("state_5.sqlite-wal");
        let index = base.join("session_index.jsonl");
        let fingerprint = CodexFingerprint {
            database: file_fingerprint(&database),
            wal: file_fingerprint(&wal),
            index: file_fingerprint(&index),
        };
        let mut cache = self.codex.lock().ok()?;
        if cache.fingerprint != fingerprint {
            let mut names = fs::read(&index)
                .ok()
                .map(|bytes| parse_codex_index(&bytes))
                .unwrap_or_default();
            // SQLite is the current source of truth. Insert it last so it also
            // wins when an old index row exists for the same thread.
            names.extend(read_codex_database(&database));
            cache.names = names;
            cache.fingerprint = fingerprint;
        }
        cache.names.get(id).cloned()
    }
}

fn file_fingerprint(path: &Path) -> FileFingerprint {
    let meta = fs::metadata(path).ok();
    FileFingerprint {
        mtime: meta.as_ref().and_then(|meta| meta.modified().ok()),
        len: meta.as_ref().map(|meta| meta.len()).unwrap_or(0),
    }
}

/// Locate the Claude transcript for a run: prefer the path the hook reported,
/// else derive it from the project directory + session id (covers runs recorded
/// before the path was persisted). Returns `None` unless the file exists, so a
/// wrong guess can never be used.
fn claude_transcript_path(transcript_path: &str, session_id: &str, cwd: &str) -> Option<PathBuf> {
    if !transcript_path.is_empty() {
        let path = PathBuf::from(transcript_path);
        if path.is_file() {
            return Some(path);
        }
    }
    if session_id.is_empty() {
        return None;
    }
    let path = claude_projects_dir()?
        .join(encode_project(cwd))
        .join(format!("{session_id}.jsonl"));
    path.is_file().then_some(path)
}

fn claude_projects_dir() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".claude"))
        })?;
    Some(base.join("projects"))
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".codex"))
        })
}

/// Claude encodes a project directory into its `projects/` folder name by
/// replacing path separators and dots with `-`.
fn encode_project(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// Read up to `max` bytes from the end of `path`.
fn read_tail(path: &Path, max: u64) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The last non-empty `aiTitle` among the `ai-title` records in `bytes`. A
/// partial first line (from seeking into the middle of a file) simply fails to
/// parse and is skipped.
fn last_ai_title(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut found = None;
    for line in text.lines() {
        // Cheap prefilter before the JSON parse; the type value is `"ai-title"`.
        if !line.contains("\"ai-title\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("ai-title") {
            continue;
        }
        if let Some(title) = value.get("aiTitle").and_then(serde_json::Value::as_str) {
            let title = title.trim();
            if !title.is_empty() {
                found = Some(title.to_owned());
            }
        }
    }
    found
}

/// Parse `session_index.jsonl` into id -> thread_name. Later rows win, so a
/// thread renamed across sessions resolves to its most recent name.
fn parse_codex_index(bytes: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let id = value.get("id").and_then(serde_json::Value::as_str);
        let name = value.get("thread_name").and_then(serde_json::Value::as_str);
        if let (Some(id), Some(name)) = (id, name) {
            let name = name.trim();
            if !id.is_empty() && !name.is_empty() {
                map.insert(id.to_owned(), name.to_owned());
            }
        }
    }
    map
}

fn read_codex_database(path: &Path) -> HashMap<String, String> {
    use rusqlite::{Connection, OpenFlags};

    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return HashMap::new();
    };
    let Ok(mut statement) = connection.prepare("SELECT id, title FROM threads WHERE title != ''")
    else {
        return HashMap::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return HashMap::new();
    };
    rows.filter_map(Result::ok)
        .filter_map(|(id, title)| {
            let title = title.trim();
            (!id.is_empty() && !title.is_empty()).then(|| (id, title.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_ai_title_takes_the_latest_and_ignores_other_records() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"First guess","sessionId":"s"}"#,
            "\n",
            // A SendMessage tool call carries its own unrelated `summary` field.
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"SendMessage","input":{"summary":"not a title"}}]}}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"Final title","sessionId":"s"}"#,
            "\n",
        );
        assert_eq!(
            last_ai_title(jsonl.as_bytes()),
            Some("Final title".to_owned())
        );
    }

    #[test]
    fn last_ai_title_skips_partial_leading_line() {
        // Simulates a tail read that begins mid-record.
        let jsonl = concat!(
            r#"itle":"garbage partial line"}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"Whole title"}"#,
            "\n",
        );
        assert_eq!(
            last_ai_title(jsonl.as_bytes()),
            Some("Whole title".to_owned())
        );
    }

    #[test]
    fn last_ai_title_absent_returns_none() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        assert_eq!(last_ai_title(jsonl.as_bytes()), None);
    }

    #[test]
    fn parse_codex_index_last_row_wins() {
        let jsonl = concat!(
            r#"{"id":"a","thread_name":"old name","updated_at":"2026-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"id":"b","thread_name":"other","updated_at":"2026-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"id":"a","thread_name":"new name","updated_at":"2026-02-01T00:00:00Z"}"#,
            "\n",
        );
        let map = parse_codex_index(jsonl.as_bytes());
        assert_eq!(map.get("a"), Some(&"new name".to_owned()));
        assert_eq!(map.get("b"), Some(&"other".to_owned()));
    }

    #[test]
    fn reads_codex_titles_from_current_database() {
        let path = std::env::temp_dir().join(format!(
            "cb-codex-title-{}-{}.sqlite",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, title) VALUES (?1, ?2), (?3, ?4)",
                ("current", "Summarised title", "blank", "  "),
            )
            .unwrap();
        drop(connection);

        let names = read_codex_database(&path);
        assert_eq!(names.get("current"), Some(&"Summarised title".to_owned()));
        assert!(!names.contains_key("blank"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn encode_project_replaces_slashes_and_dots() {
        assert_eq!(
            encode_project("/Users/x/Projects/command-center"),
            "-Users-x-Projects-command-center"
        );
        assert_eq!(
            encode_project("/Users/x/work/engagekit.io/app"),
            "-Users-x-work-engagekit-io-app"
        );
    }

    #[test]
    fn resolve_reads_claude_title_from_explicit_transcript() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cb-title-{}.jsonl", std::process::id()));
        fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"Resolved from disk"}"#,
                "\n",
            ),
        )
        .unwrap();
        let cache = TitleCache::default();
        let title = cache.resolve("claude", path.to_str().unwrap(), "", "");
        assert_eq!(title, Some("Resolved from disk".to_owned()));
        // A cache hit (unchanged file) returns the same value.
        let again = cache.resolve("claude", path.to_str().unwrap(), "", "");
        assert_eq!(again, Some("Resolved from disk".to_owned()));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn resolve_missing_transcript_is_none() {
        let cache = TitleCache::default();
        assert_eq!(
            cache.resolve("claude", "/nonexistent/cb-missing.jsonl", "", ""),
            None
        );
    }

    #[test]
    fn resolve_unknown_agent_is_none() {
        let cache = TitleCache::default();
        assert_eq!(cache.resolve("opencode", "", "some-id", ""), None);
    }
}
