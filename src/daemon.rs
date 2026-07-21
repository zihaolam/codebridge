use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

#[cfg(test)]
use crate::conductor::Conductor;
use crate::conductor::{conductor_socket_path, ConductorClient, Engine};
use crate::protocol::{Request, Response, Status, VERSION};
use crate::task::{derived_status, TaskRun, TaskStatus, TaskStore};

pub fn state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("CB_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cb")
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("CB_SOCK")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir().join("daemon.sock"))
}

pub struct Daemon {
    conductor: Box<dyn Engine>,
    watchers: Mutex<Vec<mpsc::Sender<()>>>,
    tasks: Mutex<TaskStore>,
    shutdown: AtomicBool,
}

impl Daemon {
    /// The production broker talks to the conductor process over its socket.
    pub fn new() -> Arc<Self> {
        Self::with_engine(
            Box::new(ConductorClient::new(conductor_socket_path())),
            state_dir().join("tasks.json"),
        )
    }

    fn with_engine(conductor: Box<dyn Engine>, task_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            conductor,
            watchers: Mutex::new(Vec::new()),
            tasks: Mutex::new(TaskStore::load(task_path)),
            shutdown: AtomicBool::new(false),
        })
    }

    /// Tests drive an in-process conductor so they stay fast and hermetic.
    #[cfg(test)]
    fn new_with_task_path(task_path: PathBuf) -> Arc<Self> {
        Self::with_engine(Box::new(Conductor::new()), task_path)
    }

    pub fn run(self: &Arc<Self>, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("daemon already running at {}", path.display()),
                ));
            }
            fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        while !self.shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Accepted sockets inherit O_NONBLOCK from the listener on
                    // Unix. Client handlers use blocking line framing, so
                    // clear it before handing the stream to a worker.
                    stream.set_nonblocking(false)?;
                    let daemon = Arc::clone(self);
                    thread::spawn(move || {
                        if let Err(error) = daemon.handle(stream) {
                            if !matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe
                                    | io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::UnexpectedEof
                            ) {
                                eprintln!("cb daemon client error: {error}");
                            }
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        }
        let _ = fs::remove_file(path);
        Ok(())
    }

    fn handle(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        while reader.read_line(&mut line)? != 0 {
            let request = match serde_json::from_str::<Request>(line.trim_end()) {
                Ok(request) => request,
                Err(error) => {
                    write_json(
                        &mut BufWriter::new(stream.try_clone()?),
                        &Response {
                            error: format!("bad request: {error}"),
                            ..Response::default()
                        },
                    )?;
                    line.clear();
                    continue;
                }
            };
            // `shutdown` stops the broker only, leaving the conductor and its
            // sessions alive — this is what `cb restart` relies on so PTYs
            // survive. Tearing the conductor down is `cb stop`'s job, done by
            // signalling the conductor directly.
            if request.kind == "shutdown" {
                write_json(
                    &mut BufWriter::new(stream.try_clone()?),
                    &Response {
                        ok: true,
                        ..Response::default()
                    },
                )?;
                // Flush the acknowledgement before letting the listener loop
                // return and the daemon process tear down its worker threads.
                self.shutdown.store(true, Ordering::Release);
                return Ok(());
            }
            match request.kind.as_str() {
                // Attach is a data-plane op and no longer flows through the
                // broker: clients (TUI and web) open the frame stream straight
                // on the conductor socket, so a broker restart never interrupts
                // a live pane. The broker owns only the control plane below.
                "watch" => return self.watch(stream),
                _ => write_json(
                    &mut BufWriter::new(stream.try_clone()?),
                    &self.dispatch(request),
                )?,
            }
            line.clear();
        }
        Ok(())
    }

    pub fn dispatch(&self, request: Request) -> Response {
        match request.kind.as_str() {
            "ping" => Response {
                ok: true,
                version: Some(VERSION),
                pid: Some(std::process::id()),
                ..Response::default()
            },
            "list" => {
                self.reap_and_park();
                Response {
                    ok: true,
                    sessions: self.conductor.snapshot(),
                    tasks: self.task_snapshot(),
                    ..Response::default()
                }
            }
            "spawn" => self.spawn(request),
            "kill" => self.kill(&request.id),
            "rename" => self.rename(&request.id, request.name),
            "extract" => self.extract(request),
            kind if kind.starts_with("task_") => self.task_dispatch(request),
            "hook" => self.hook(request),
            "shutdown" => {
                // Broker-only stop; the conductor keeps its sessions alive.
                self.shutdown.store(true, Ordering::Release);
                Response {
                    ok: true,
                    ..Response::default()
                }
            }
            other => Response {
                error: format!("unknown request type: {other}"),
                ..Response::default()
            },
        }
    }

    /// Spawns a session and, when it launches a known agent, records it as an
    /// auto task so it can later be listed and resumed from the historical
    /// picker. `task_start`/`task_resume` bypass this by calling
    /// `spawn_session` directly, since they manage their own runs.
    fn spawn(&self, request: Request) -> Response {
        let agent = agent_name(&request.argv);
        let cwd = if request.cwd.is_empty() {
            std::env::current_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            request.cwd.clone()
        };
        match self.conductor.spawn_session(
            request.argv,
            request.cwd,
            request.rows,
            request.cols,
            request.prefill,
        ) {
            Ok(id) => {
                if let Some(agent) = agent {
                    let scope = crate::sidebar::scope_key(&cwd);
                    if let Ok(mut tasks) = self.tasks.lock() {
                        tasks.add_auto_session(scope, agent.to_owned(), cwd, id.clone());
                        let _ = tasks.save();
                    }
                }
                self.notify_watchers();
                Response {
                    ok: true,
                    id,
                    ..Response::default()
                }
            }
            Err(error) => Response {
                error,
                ..Response::default()
            },
        }
    }

    fn kill(&self, id: &str) -> Response {
        // The conductor removes the session and hands back the agent-native
        // resume id captured before it is gone; park the bound run here so the
        // killed session is immediately resumable without lazy reconciliation.
        let Some((harness, result)) = self.conductor.kill(id) else {
            return Response {
                error: format!("no such session: {id}"),
                ..Response::default()
            };
        };
        self.park_runs_for_session(id, &harness);
        self.notify_watchers();
        match result {
            Ok(()) => Response {
                ok: true,
                ..Response::default()
            },
            Err(error) => Response {
                error,
                ..Response::default()
            },
        }
    }

    /// Parks the runs of sessions the conductor reaped because their agent
    /// exited deliberately (`/exit`, normal quit): each leaves the engine and
    /// its bound run is parked so it stays resumable from the history picker.
    /// Sessions that crashed (non-zero exit or signal) are left in place so
    /// their ended row stays visible. A no-op when nothing exited cleanly.
    fn reap_and_park(&self) {
        let reaped = self.conductor.reap();
        if reaped.is_empty() {
            return;
        }
        for (id, harness) in &reaped {
            self.park_runs_for_session(id, harness);
        }
        self.notify_watchers();
    }

    fn rename(&self, id: &str, name: String) -> Response {
        if !self.conductor.set_name(id, name) {
            return Response {
                error: format!("no such session: {id}"),
                ..Response::default()
            };
        }
        self.notify_watchers();
        Response {
            ok: true,
            ..Response::default()
        }
    }

    fn extract(&self, request: Request) -> Response {
        match self.conductor.extract(
            &request.id,
            (request.col_start, request.line_start),
            (request.col_end, request.line_end),
        ) {
            Ok(text) => Response {
                ok: true,
                text,
                ..Response::default()
            },
            Err(error) => Response {
                error,
                ..Response::default()
            },
        }
    }

    fn task_dispatch(&self, request: Request) -> Response {
        match request.kind.as_str() {
            "task_list" => Response {
                ok: true,
                tasks: self.task_snapshot(),
                ..Response::default()
            },
            "task_add" => {
                let title = request.title.trim();
                if title.is_empty() {
                    return Response {
                        error: "task title required".to_owned(),
                        ..Response::default()
                    };
                }
                let mut tasks = match self.tasks.lock() {
                    Ok(tasks) => tasks,
                    Err(_) => return task_lock_error(),
                };
                let id = tasks.add(
                    request.scope,
                    title.to_owned(),
                    request.desc.trim().to_owned(),
                );
                if let Err(error) = tasks.save() {
                    return Response {
                        error: error.to_string(),
                        ..Response::default()
                    };
                }
                let snapshot = tasks.tasks().to_vec();
                drop(tasks);
                self.notify_watchers();
                Response {
                    ok: true,
                    id,
                    tasks: snapshot,
                    ..Response::default()
                }
            }
            "task_edit" => self.task_mutate(&request.id, |task| {
                if !request.title.trim().is_empty() {
                    task.title = request.title.trim().to_owned();
                }
                task.desc = request.desc.trim_end().to_owned();
            }),
            "task_status" => {
                let Some(status) = parse_task_status(&request.task_status) else {
                    return Response {
                        error: format!("invalid task status: {}", request.task_status),
                        ..Response::default()
                    };
                };
                self.task_mutate(&request.id, |task| task.status = status)
            }
            "task_delete" => {
                let mut tasks = match self.tasks.lock() {
                    Ok(tasks) => tasks,
                    Err(_) => return task_lock_error(),
                };
                tasks.delete(&request.id);
                if let Err(error) = tasks.save() {
                    return Response {
                        error: error.to_string(),
                        ..Response::default()
                    };
                }
                let snapshot = tasks.tasks().to_vec();
                drop(tasks);
                self.notify_watchers();
                Response {
                    ok: true,
                    tasks: snapshot,
                    ..Response::default()
                }
            }
            "task_start" => self.task_start(request),
            "task_resume" => self.task_resume(request),
            _ => Response {
                error: format!("unknown task request: {}", request.kind),
                ..Response::default()
            },
        }
    }

    fn task_mutate(&self, id: &str, mutate: impl FnOnce(&mut crate::task::Task)) -> Response {
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return task_lock_error(),
        };
        let Some(task) = tasks.get_mut(id) else {
            return Response {
                error: format!("no such task: {id}"),
                ..Response::default()
            };
        };
        mutate(task);
        task.updated_at = OffsetDateTime::now_utc();
        if let Err(error) = tasks.save() {
            return Response {
                error: error.to_string(),
                ..Response::default()
            };
        }
        let snapshot = tasks.tasks().to_vec();
        drop(tasks);
        self.notify_watchers();
        Response {
            ok: true,
            tasks: snapshot,
            ..Response::default()
        }
    }

    fn task_start(&self, request: Request) -> Response {
        if request.agent.is_empty() {
            return Response {
                error: "task_start requires an agent".to_owned(),
                ..Response::default()
            };
        }
        let task = match self.tasks.lock() {
            Ok(tasks) => tasks.get(&request.id).cloned(),
            Err(_) => return task_lock_error(),
        };
        let Some(task) = task else {
            return Response {
                error: format!("no such task: {}", request.id),
                ..Response::default()
            };
        };
        let prefill = if task.desc.is_empty() {
            task.title
        } else {
            format!("{}\n\n{}", task.title, task.desc)
        };
        let session_id = match self.conductor.spawn_session(
            vec![request.agent.clone()],
            request.cwd.clone(),
            request.rows,
            request.cols,
            prefill.clone(),
        ) {
            Ok(id) => id,
            Err(error) => {
                return Response {
                    error,
                    ..Response::default()
                }
            }
        };
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return task_lock_error(),
        };
        if let Some(task) = tasks.get_mut(&request.id) {
            let now = OffsetDateTime::now_utc();
            task.runs.push(TaskRun {
                id: Uuid::new_v4().to_string(),
                agent: request.agent,
                cwd: request.cwd,
                cb_session_id: session_id.clone(),
                agent_session_id: String::new(),
                first_message: prefill,
                status: TaskStatus::InProgress,
                created_at: now,
                updated_at: now,
            });
            task.status = derived_status(task);
            task.updated_at = now;
            let _ = tasks.save();
        }
        let snapshot = tasks.tasks().to_vec();
        drop(tasks);
        self.notify_watchers();
        Response {
            ok: true,
            id: session_id,
            tasks: snapshot,
            ..Response::default()
        }
    }

    fn task_resume(&self, request: Request) -> Response {
        let run = match self.tasks.lock() {
            Ok(tasks) => tasks
                .get(&request.id)
                .and_then(|task| task.runs.iter().find(|run| run.id == request.run_id))
                .filter(|run| run.status == TaskStatus::Paused)
                .cloned(),
            Err(_) => return task_lock_error(),
        };
        let Some(run) = run else {
            return Response {
                error: format!("no such paused task run: {}", request.run_id),
                ..Response::default()
            };
        };
        if run.agent.is_empty() {
            return Response {
                error: "task run has no agent".to_owned(),
                ..Response::default()
            };
        }
        let argv = match (run.agent.as_str(), run.agent_session_id.as_str()) {
            ("claude", id) if !id.is_empty() => {
                vec!["claude".to_owned(), "--resume".to_owned(), id.to_owned()]
            }
            ("codex", id) if !id.is_empty() => {
                vec!["codex".to_owned(), "resume".to_owned(), id.to_owned()]
            }
            ("codex", _) => vec!["codex".to_owned(), "resume".to_owned(), "--last".to_owned()],
            ("opencode", _) => vec!["opencode".to_owned(), "--continue".to_owned()],
            (agent, _) => vec![agent.to_owned()],
        };
        let cwd = if request.cwd.is_empty() {
            run.cwd.clone()
        } else {
            request.cwd
        };
        let session_id =
            match self
                .conductor
                .spawn_session(argv, cwd, request.rows, request.cols, String::new())
            {
                Ok(id) => id,
                Err(error) => {
                    return Response {
                        error,
                        ..Response::default()
                    }
                }
            };
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return task_lock_error(),
        };
        if let Some(task) = tasks.get_mut(&request.id) {
            let now = OffsetDateTime::now_utc();
            if let Some(run) = task.runs.iter_mut().find(|run| run.id == request.run_id) {
                run.cb_session_id = session_id.clone();
                run.status = TaskStatus::InProgress;
                run.updated_at = now;
            }
            task.status = derived_status(task);
            task.updated_at = now;
            let _ = tasks.save();
        }
        let snapshot = tasks.tasks().to_vec();
        drop(tasks);
        self.notify_watchers();
        Response {
            ok: true,
            id: session_id,
            tasks: snapshot,
            ..Response::default()
        }
    }

    /// Marks any live run bound to `session_id` as paused, retaining the
    /// agent-native resume id (refreshed from `harness` when non-empty) so the
    /// session can be resumed later. Invoked the moment a session is killed.
    fn park_runs_for_session(&self, session_id: &str, harness: &str) {
        let Ok(mut tasks) = self.tasks.lock() else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        let mut dirty = false;
        for task in tasks.tasks_mut() {
            for run in &mut task.runs {
                if run.cb_session_id == session_id && run.status == TaskStatus::InProgress {
                    if !harness.is_empty() {
                        run.agent_session_id = harness.to_owned();
                    }
                    run.status = TaskStatus::Paused;
                    run.cb_session_id.clear();
                    run.updated_at = now;
                    dirty = true;
                }
            }
            let status = derived_status(task);
            if task.status != TaskStatus::Completed && task.status != status {
                task.status = status;
                task.updated_at = now;
                dirty = true;
            }
        }
        if dirty {
            let _ = tasks.save();
        }
    }

    /// Applies `update` to every run bound to `session_id`, bumping the run's
    /// timestamp and persisting when the closure reports a change. Used to fold
    /// hook-delivered data (resume id, first message) into the persisted run.
    fn update_run_for_session(
        &self,
        session_id: &str,
        mut update: impl FnMut(&mut TaskRun) -> bool,
    ) {
        let Ok(mut tasks) = self.tasks.lock() else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        let mut dirty = false;
        for task in tasks.tasks_mut() {
            for run in &mut task.runs {
                if run.cb_session_id == session_id && update(run) {
                    run.updated_at = now;
                    dirty = true;
                }
            }
        }
        if dirty {
            let _ = tasks.save();
        }
    }

    fn task_snapshot(&self) -> Vec<crate::task::Task> {
        let live = self.conductor.session_liveness();
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return Vec::new(),
        };
        let now = OffsetDateTime::now_utc();
        let mut dirty = false;
        for task in tasks.tasks_mut() {
            for run in &mut task.runs {
                if run.status != TaskStatus::InProgress {
                    continue;
                }
                match live.get(&run.cb_session_id) {
                    Some((false, harness)) => {
                        if !harness.is_empty() && harness != &run.agent_session_id {
                            run.agent_session_id = harness.clone();
                            run.updated_at = now;
                            dirty = true;
                        }
                    }
                    _ if (now - run.updated_at).whole_seconds() >= 2 => {
                        run.status = TaskStatus::Paused;
                        run.cb_session_id.clear();
                        run.updated_at = now;
                        dirty = true;
                    }
                    _ => {}
                }
            }
            let status = derived_status(task);
            if task.status != TaskStatus::Completed && task.status != status {
                task.status = status;
                task.updated_at = now;
                dirty = true;
            }
        }
        if dirty {
            let _ = tasks.save();
        }
        tasks.tasks().to_vec()
    }

    fn hook(&self, request: Request) -> Response {
        let (status, message) = status_for_event(&request.event, &request.payload);
        let harness_id = request
            .payload
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        // Claude reports the absolute path to its own transcript on every hook
        // payload; capturing it lets us confirm user interrupts (which fire no
        // hook) against the agent's ground truth.
        let transcript = request
            .payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let first_message = (request.event == "UserPromptSubmit")
            .then(|| {
                request
                    .payload
                    .get("prompt")
                    .or_else(|| request.payload.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten();
        // Fold the observation into the session (harness id, prefill flush,
        // status). A missing session makes the hook a no-op, matching the
        // CB_SESSION-absent contract.
        let harness = harness_id.clone().unwrap_or_default();
        if !self
            .conductor
            .apply_hook(&request.session, status, message, &harness, transcript)
        {
            return Response {
                ok: true,
                ..Response::default()
            };
        }
        // Task-store side (broker-owned): fold the harness id and first message
        // into the bound run.
        if harness_id.is_some() || first_message.is_some() {
            self.update_run_for_session(&request.session, |run| {
                let mut changed = false;
                if let Some(id) = &harness_id {
                    if run.agent_session_id != *id {
                        run.agent_session_id = id.clone();
                        changed = true;
                    }
                }
                if let Some(message) = &first_message {
                    if run.first_message.is_empty() && !message.trim().is_empty() {
                        run.first_message = message.clone();
                        changed = true;
                    }
                }
                changed
            });
        }
        self.notify_watchers();
        Response {
            ok: true,
            ..Response::default()
        }
    }

    fn watch(&self, stream: UnixStream) -> io::Result<()> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut watchers) = self.watchers.lock() {
            watchers.push(sender);
        }
        let mut writer = BufWriter::new(stream);
        loop {
            self.reap_and_park();
            write_json(
                &mut writer,
                &Response {
                    ok: true,
                    sessions: self.conductor.snapshot(),
                    tasks: self.task_snapshot(),
                    ..Response::default()
                },
            )?;
            if receiver.recv_timeout(Duration::from_secs(1)).is_err()
                && self.shutdown.load(Ordering::Acquire)
            {
                return Ok(());
            }
        }
    }

    fn notify_watchers(&self) {
        if let Ok(mut watchers) = self.watchers.lock() {
            watchers.retain(|watcher| watcher.send(()).is_ok());
        }
    }
}

/// Normalizes the first argv entry to a known agent name, or `None` for
/// arbitrary commands. Only known agents are auto-recorded as resumable
/// sessions, since resume depends on the agent's own `--resume`/`resume`.
fn agent_name(argv: &[String]) -> Option<&'static str> {
    let base = argv
        .first()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())?;
    match base {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

fn status_for_event(event: &str, payload: &Value) -> (Status, String) {
    match event {
        "SessionStart" => (Status::Idle, String::new()),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolBatch" => {
            (Status::Working, String::new())
        }
        "PermissionRequest" => (
            Status::NeedsApproval,
            payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        "Notification" => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if is_approval_message(message) {
                (Status::NeedsApproval, message.to_owned())
            } else {
                (Status::WaitingUser, String::new())
            }
        }
        "Stop" | "StopFailure" => (Status::WaitingUser, String::new()),
        "SessionEnd" => (Status::Ended, String::new()),
        _ => (Status::Working, String::new()),
    }
}

fn is_approval_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    ["permission", "approve", "approval", "allow"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn write_json(writer: &mut impl Write, value: &impl serde::Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn parse_task_status(status: &str) -> Option<TaskStatus> {
    match status {
        "pending" => Some(TaskStatus::Pending),
        "in_progress" => Some(TaskStatus::InProgress),
        "paused" => Some(TaskStatus::Paused),
        "completed" => Some(TaskStatus::Completed),
        _ => None,
    }
}

fn task_lock_error() -> Response {
    Response {
        error: "task store lock is poisoned".to_owned(),
        ..Response::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_mapping_preserves_codebridge_semantics() {
        assert_eq!(
            status_for_event("SessionStart", &Value::Null).0,
            Status::Idle
        );
        assert_eq!(
            status_for_event("Stop", &Value::Null).0,
            Status::WaitingUser
        );
        assert_eq!(
            status_for_event("StopFailure", &Value::Null).0,
            Status::WaitingUser
        );
        assert_eq!(
            status_for_event("PostToolBatch", &Value::Null).0,
            Status::Working
        );
        assert_eq!(
            status_for_event(
                "Notification",
                &json!({"message":"Permission required to run command"})
            )
            .0,
            Status::NeedsApproval
        );
        assert_eq!(
            status_for_event("Notification", &json!({"message":"waiting for input"})).0,
            Status::WaitingUser
        );
    }

    #[test]
    fn task_crud_is_daemon_owned_and_persisted() {
        let path =
            std::env::temp_dir().join(format!("cb-daemon-tasks-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        let daemon = Daemon::new_with_task_path(path.clone());
        let added = daemon.dispatch(Request {
            kind: "task_add".to_owned(),
            scope: "repo".to_owned(),
            title: "  fix bug  ".to_owned(),
            desc: "details".to_owned(),
            ..Request::default()
        });
        assert!(added.ok);
        assert_eq!(added.tasks[0].title, "fix bug");
        let edited = daemon.dispatch(Request {
            kind: "task_edit".to_owned(),
            id: added.id.clone(),
            title: "fix the bug".to_owned(),
            desc: "more detail\n".to_owned(),
            ..Request::default()
        });
        assert!(edited.ok);
        assert_eq!(edited.tasks[0].desc, "more detail");
        let completed = daemon.dispatch(Request {
            kind: "task_status".to_owned(),
            id: added.id.clone(),
            task_status: "completed".to_owned(),
            ..Request::default()
        });
        assert_eq!(completed.tasks[0].status, TaskStatus::Completed);
        drop(daemon);
        assert_eq!(
            TaskStore::load(path.clone()).tasks()[0].title,
            "fix the bug"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn task_runs_prefill_in_parallel_and_reconcile_after_kill() {
        let path =
            std::env::temp_dir().join(format!("cb-daemon-task-runs-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        let daemon = Daemon::new_with_task_path(path.clone());
        let added = daemon.dispatch(Request {
            kind: "task_add".to_owned(),
            scope: "repo".to_owned(),
            title: "prefilled task".to_owned(),
            ..Request::default()
        });
        let start = || {
            daemon.dispatch(Request {
                kind: "task_start".to_owned(),
                id: added.id.clone(),
                agent: "/bin/cat".to_owned(),
                cwd: "/tmp".to_owned(),
                rows: 4,
                cols: 40,
                ..Request::default()
            })
        };
        let first = start();
        let second = start();
        assert!(first.ok && second.ok);
        assert_eq!(second.tasks[0].runs.len(), 2);
        thread::sleep(Duration::from_millis(1_200));
        let text = daemon
            .conductor
            .extract(&first.id, (0, 0), (39, 2))
            .expect("prefill text");
        assert!(text.contains("prefilled task"));

        assert!(daemon.kill(&first.id).ok);
        assert!(daemon.kill(&second.id).ok);
        thread::sleep(Duration::from_millis(2_100));
        let reconciled = daemon.dispatch(Request {
            kind: "task_list".to_owned(),
            ..Request::default()
        });
        assert_eq!(reconciled.tasks[0].status, TaskStatus::Paused);
        assert!(reconciled.tasks[0]
            .runs
            .iter()
            .all(|run| run.status == TaskStatus::Paused));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn agent_name_recognizes_known_agents_by_basename() {
        assert_eq!(agent_name(&["claude".to_owned()]), Some("claude"));
        assert_eq!(
            agent_name(&["/usr/local/bin/codex".to_owned()]),
            Some("codex")
        );
        assert_eq!(agent_name(&["opencode".to_owned()]), Some("opencode"));
        assert_eq!(agent_name(&["/bin/bash".to_owned()]), None);
        assert_eq!(agent_name(&[]), None);
    }

    #[test]
    fn spawning_an_agent_records_a_resumable_session_and_parks_on_kill() {
        let path = std::env::temp_dir().join(format!("cb-daemon-auto-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        // Give the spawned binary a known-agent basename so it is auto-recorded,
        // while still pointing at a real executable that stays alive on a PTY.
        let bindir = std::env::temp_dir().join(format!("cb-agentbin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&bindir);
        fs::create_dir_all(&bindir).unwrap();
        let claude = bindir.join("claude");
        std::os::unix::fs::symlink("/bin/cat", &claude).unwrap();

        let daemon = Daemon::new_with_task_path(path.clone());
        let spawned = daemon.dispatch(Request {
            kind: "spawn".to_owned(),
            argv: vec![claude.to_string_lossy().into_owned()],
            cwd: "/tmp".to_owned(),
            rows: 4,
            cols: 40,
            ..Request::default()
        });
        assert!(spawned.ok);

        let listed = daemon.dispatch(Request {
            kind: "task_list".to_owned(),
            ..Request::default()
        });
        assert_eq!(listed.tasks.len(), 1);
        let task = &listed.tasks[0];
        assert!(task.auto);
        assert_eq!(task.scope, crate::sidebar::scope_key("/tmp"));
        assert_eq!(task.runs.len(), 1);
        assert_eq!(task.runs[0].agent, "claude");
        assert_eq!(task.runs[0].status, TaskStatus::InProgress);

        // A hook delivers the agent-native resume id and first prompt.
        daemon.dispatch(Request {
            kind: "hook".to_owned(),
            event: "UserPromptSubmit".to_owned(),
            session: spawned.id.clone(),
            payload: json!({"session_id": "sess-123", "prompt": "hello world"}),
            ..Request::default()
        });

        // Killing frees the process but the run is immediately resumable.
        assert!(daemon.kill(&spawned.id).ok);
        let parked = daemon.dispatch(Request {
            kind: "task_list".to_owned(),
            ..Request::default()
        });
        let run = &parked.tasks[0].runs[0];
        assert_eq!(run.status, TaskStatus::Paused);
        assert_eq!(run.agent_session_id, "sess-123");
        assert_eq!(run.first_message, "hello world");
        assert!(run.cb_session_id.is_empty());

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(&bindir);
    }

    #[test]
    fn clean_exit_reaps_the_session_while_a_crash_stays_visible() {
        let path = std::env::temp_dir().join(format!("cb-daemon-reap-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        let bindir = std::env::temp_dir().join(format!("cb-reapbin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&bindir);
        fs::create_dir_all(&bindir).unwrap();
        // A known-agent basename so each session is auto-recorded, pointing at a
        // shell we can make exit with a chosen status.
        let claude = bindir.join("claude");
        std::os::unix::fs::symlink("/bin/sh", &claude).unwrap();
        let claude = claude.to_string_lossy().into_owned();

        let daemon = Daemon::new_with_task_path(path.clone());
        let clean = daemon.dispatch(Request {
            kind: "spawn".to_owned(),
            argv: vec![claude.clone(), "-c".to_owned(), "exit 0".to_owned()],
            cwd: "/tmp".to_owned(),
            rows: 4,
            cols: 40,
            ..Request::default()
        });
        let crash = daemon.dispatch(Request {
            kind: "spawn".to_owned(),
            argv: vec![claude.clone(), "-c".to_owned(), "exit 3".to_owned()],
            cwd: "/tmp".to_owned(),
            rows: 4,
            cols: 40,
            ..Request::default()
        });
        assert!(clean.ok && crash.ok);

        // Label each run before the children exit; the sessions linger in the
        // map until a list/watch reaps them, so the hook still resolves.
        for (id, message) in [(&clean.id, "clean run"), (&crash.id, "crash run")] {
            daemon.dispatch(Request {
                kind: "hook".to_owned(),
                event: "UserPromptSubmit".to_owned(),
                session: id.clone(),
                payload: json!({"session_id": format!("sess-{message}"), "prompt": message}),
                ..Request::default()
            });
        }

        // Let both children exit and the waiter record their exit status.
        thread::sleep(Duration::from_millis(400));

        let listed = daemon.dispatch(Request {
            kind: "list".to_owned(),
            ..Request::default()
        });
        let ids: Vec<&str> = listed.sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(
            !ids.contains(&clean.id.as_str()),
            "clean exit should be reaped"
        );
        let crashed = listed
            .sessions
            .iter()
            .find(|s| s.id == crash.id)
            .expect("crashed session stays visible");
        assert!(crashed.exited && crashed.status == Status::Ended);

        // The reaped session's run is parked (resumable); the crashed session's
        // run is still bound and live (not yet reconciled).
        let run_for = |message: &str| {
            listed
                .tasks
                .iter()
                .flat_map(|task| &task.runs)
                .find(|run| run.first_message == message)
                .cloned()
                .unwrap()
        };
        let clean_run = run_for("clean run");
        assert_eq!(clean_run.status, TaskStatus::Paused);
        assert_eq!(clean_run.agent_session_id, "sess-clean run");
        assert!(clean_run.cb_session_id.is_empty());
        let crash_run = run_for("crash run");
        assert_eq!(crash_run.status, TaskStatus::InProgress);
        assert_eq!(crash_run.cb_session_id, crash.id);

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(&bindir);
    }
}
