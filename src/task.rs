use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Paused,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRun {
    pub id: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub cb_session_id: String,
    #[serde(default)]
    pub agent_session_id: String,
    /// First prompt sent in this session, captured from the agent's
    /// `UserPromptSubmit` hook (or seeded from a task prefill). The
    /// historical-session picker labels a run by its `title` and falls back to
    /// this when no agent-generated title exists yet.
    #[serde(default)]
    pub first_message: String,
    /// Absolute path to the agent's own transcript, captured from Claude hook
    /// payloads (`transcript_path`). Empty for agents that do not report one
    /// (e.g. Codex). Lets the broker read the agent-summarised title later.
    #[serde(default)]
    pub transcript_path: String,
    /// Agent-summarised conversation title — Claude's `ai-title`, Codex's
    /// `thread_name` — resolved lazily by the broker and shown in the picker in
    /// preference to `first_message`. Empty until the agent generates one.
    #[serde(default)]
    pub title: String,
    pub status: TaskStatus,
    #[serde(default = "epoch", with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default = "epoch", with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    #[serde(default)]
    pub scope: String,
    pub title: String,
    #[serde(default)]
    pub desc: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub runs: Vec<TaskRun>,
    /// True when this task was synthesized to record an ad-hoc agent session
    /// rather than being authored in the backlog. Auto tasks are hidden from
    /// the backlog and surfaced only in the historical-session picker.
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub cb_session_id: String,
    #[serde(default)]
    pub agent_session_id: String,
    #[serde(default = "epoch", with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default = "epoch", with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredTasks {
    #[serde(default)]
    tasks: Vec<Task>,
}

pub struct TaskStore {
    path: PathBuf,
    tasks: Vec<Task>,
}

impl TaskStore {
    pub fn load(path: PathBuf) -> Self {
        let mut store = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StoredTasks>(&bytes).ok())
            .map(|stored| Self {
                path: path.clone(),
                tasks: stored.tasks,
            })
            .unwrap_or_else(|| Self {
                path,
                tasks: Vec::new(),
            });
        store.migrate_runs();
        store
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn tasks_mut(&mut self) -> &mut [Task] {
        &mut self.tasks
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn add(&mut self, scope: String, title: String, desc: String) -> String {
        let now = OffsetDateTime::now_utc();
        let id = Uuid::new_v4().to_string();
        self.tasks.push(Task {
            id: id.clone(),
            scope,
            title,
            desc,
            status: TaskStatus::Pending,
            runs: Vec::new(),
            auto: false,
            agent: String::new(),
            cwd: String::new(),
            cb_session_id: String::new(),
            agent_session_id: String::new(),
            created_at: now,
            updated_at: now,
        });
        id
    }

    /// Records an ad-hoc agent session as an auto task carrying a single live
    /// run. Returns `(task_id, run_id)` so the caller can later reconcile or
    /// resume it. The run's `first_message` fills in from the first hook.
    pub fn add_auto_session(
        &mut self,
        scope: String,
        agent: String,
        cwd: String,
        cb_session_id: String,
    ) -> (String, String) {
        let now = OffsetDateTime::now_utc();
        let task_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        self.tasks.push(Task {
            id: task_id.clone(),
            scope,
            title: agent.clone(),
            desc: String::new(),
            status: TaskStatus::InProgress,
            runs: vec![TaskRun {
                id: run_id.clone(),
                agent,
                cwd,
                cb_session_id,
                agent_session_id: String::new(),
                first_message: String::new(),
                transcript_path: String::new(),
                title: String::new(),
                status: TaskStatus::InProgress,
                created_at: now,
                updated_at: now,
            }],
            auto: true,
            agent: String::new(),
            cwd: String::new(),
            cb_session_id: String::new(),
            agent_session_id: String::new(),
            created_at: now,
            updated_at: now,
        });
        (task_id, run_id)
    }

    pub fn delete(&mut self, id: &str) {
        self.tasks.retain(|task| task.id != id);
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&StoredTasks {
            tasks: self.tasks.clone(),
        })
        .map_err(io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, &self.path)
    }

    fn migrate_runs(&mut self) {
        for task in &mut self.tasks {
            if task.runs.is_empty()
                && (!task.cb_session_id.is_empty() || !task.agent_session_id.is_empty())
            {
                task.runs.push(TaskRun {
                    id: Uuid::new_v4().to_string(),
                    agent: task.agent.clone(),
                    cwd: task.cwd.clone(),
                    cb_session_id: task.cb_session_id.clone(),
                    agent_session_id: task.agent_session_id.clone(),
                    first_message: String::new(),
                    transcript_path: String::new(),
                    title: String::new(),
                    status: task.status,
                    created_at: task.created_at,
                    updated_at: task.updated_at,
                });
            }
        }
    }
}

pub fn derived_status(task: &Task) -> TaskStatus {
    if task.status == TaskStatus::Completed {
        TaskStatus::Completed
    } else if task
        .runs
        .iter()
        .any(|run| run.status == TaskStatus::InProgress)
    {
        TaskStatus::InProgress
    } else if task.runs.is_empty() {
        TaskStatus::Pending
    } else {
        TaskStatus::Paused
    }
}

fn epoch() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cb-task-{name}-{}.json", std::process::id()))
    }

    #[test]
    fn store_round_trips_and_migrates_legacy_run_fields() {
        let path = path("roundtrip");
        let mut store = TaskStore::load(path.clone());
        let id = store.add(
            "repo".to_owned(),
            "fix bug".to_owned(),
            "details".to_owned(),
        );
        store.save().unwrap();
        let loaded = TaskStore::load(path.clone());
        assert_eq!(loaded.tasks().len(), 1);
        assert_eq!(loaded.tasks()[0].id, id);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn derived_status_keeps_parallel_task_active() {
        let now = OffsetDateTime::now_utc();
        let mut task = Task {
            id: "task".to_owned(),
            scope: String::new(),
            title: "parallel".to_owned(),
            desc: String::new(),
            status: TaskStatus::Paused,
            runs: Vec::new(),
            auto: false,
            agent: String::new(),
            cwd: String::new(),
            cb_session_id: String::new(),
            agent_session_id: String::new(),
            created_at: now,
            updated_at: now,
        };
        task.runs.push(TaskRun {
            id: "run".to_owned(),
            agent: "claude".to_owned(),
            cwd: String::new(),
            cb_session_id: "session".to_owned(),
            agent_session_id: String::new(),
            first_message: String::new(),
            transcript_path: String::new(),
            title: String::new(),
            status: TaskStatus::InProgress,
            created_at: now,
            updated_at: now,
        });
        assert_eq!(derived_status(&task), TaskStatus::InProgress);
    }
}
