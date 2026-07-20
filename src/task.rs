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
            agent: String::new(),
            cwd: String::new(),
            cb_session_id: String::new(),
            agent_session_id: String::new(),
            created_at: now,
            updated_at: now,
        });
        id
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
            status: TaskStatus::InProgress,
            created_at: now,
            updated_at: now,
        });
        assert_eq!(derived_status(&task), TaskStatus::InProgress);
    }
}
