use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use time::OffsetDateTime;

use crate::session::Session;

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "type")]
    kind: String,
    payload: MetaPayload,
}

#[derive(Deserialize)]
struct MetaPayload {
    id: String,
    #[serde(with = "time::serde::rfc3339")]
    timestamp: OffsetDateTime,
    cwd: String,
}

pub fn sessions_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home).join("sessions"));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".codex/sessions"))
}

pub fn start_harvest(
    session: Arc<Session>,
    cwd: PathBuf,
    since: SystemTime,
    claimed: Arc<Mutex<HashSet<String>>>,
) {
    thread::spawn(move || {
        let Some(root) = sessions_dir() else {
            return;
        };
        let deadline = SystemTime::now() + Duration::from_secs(60);
        while SystemTime::now() < deadline && !session.exited() {
            if let Some(id) = find_session_id(&root, since, &cwd, &claimed) {
                let won = claimed
                    .lock()
                    .map(|mut claimed| claimed.insert(id.clone()))
                    .unwrap_or(false);
                if won {
                    session.set_harness_session_id(id);
                    return;
                }
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

pub fn find_session_id(
    root: &Path,
    since: SystemTime,
    cwd: &Path,
    claimed: &Mutex<HashSet<String>>,
) -> Option<String> {
    let cutoff = since
        .checked_sub(Duration::from_secs(1))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut files = Vec::new();
    collect_rollouts(root, 0, &mut files);
    let claimed = claimed.lock().ok()?;
    files
        .into_iter()
        .filter_map(|path| {
            let modified = path.metadata().ok()?.modified().ok()?;
            if modified < cutoff {
                return None;
            }
            let meta = read_meta(&path)?;
            let timestamp: SystemTime = meta.payload.timestamp.into();
            if timestamp < cutoff
                || !same_dir(Path::new(&meta.payload.cwd), cwd)
                || claimed.contains(&meta.payload.id)
            {
                return None;
            }
            Some((timestamp, meta.payload.id))
        })
        .min_by_key(|(timestamp, _)| *timestamp)
        .map(|(_, id)| id)
}

fn collect_rollouts(dir: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, depth + 1, output);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        {
            output.push(path);
        }
    }
}

fn read_meta(path: &Path) -> Option<Meta> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file)
        .take(4 * 1024 * 1024)
        .read_line(&mut line)
        .ok()?;
    let meta: Meta = serde_json::from_str(&line).ok()?;
    (meta.kind == "session_meta" && !meta.payload.id.is_empty()).then_some(meta)
}

fn same_dir(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use time::format_description::well_known::Rfc3339;

    #[test]
    fn finds_earliest_unclaimed_matching_rollout() {
        let root = std::env::temp_dir().join(format!("cb-codex-rollouts-{}", std::process::id()));
        let date = root.join("2026/07/20");
        let cwd = root.join("repo");
        fs::create_dir_all(&date).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let since = SystemTime::now();
        for (id, offset, rollout_cwd) in [
            ("claimed", 1, cwd.as_path()),
            ("wanted", 2, cwd.as_path()),
            ("later", 3, cwd.as_path()),
            ("other", 1, root.as_path()),
        ] {
            let timestamp = OffsetDateTime::now_utc() + time::Duration::seconds(offset);
            let path = date.join(format!("rollout-{id}.jsonl"));
            let mut file = fs::File::create(path).unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type":"session_meta",
                    "payload":{
                        "id":id,
                        "timestamp":timestamp.format(&Rfc3339).unwrap(),
                        "cwd":rollout_cwd
                    }
                })
            )
            .unwrap();
        }
        let claimed = Mutex::new(HashSet::from(["claimed".to_owned()]));
        assert_eq!(
            find_session_id(&root, since, &cwd, &claimed).as_deref(),
            Some("wanted")
        );
        let _ = fs::remove_dir_all(root);
    }
}
