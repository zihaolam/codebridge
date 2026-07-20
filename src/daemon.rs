use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

use base64::Engine;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::protocol::{Request, Response, Status, StreamDown, StreamUp, TerminalFrame, VERSION};
use crate::session::Session;
use crate::task::{derived_status, TaskRun, TaskStatus, TaskStore};
use crate::terminal::MouseAction;

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
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    order: RwLock<Vec<String>>,
    watchers: Mutex<Vec<mpsc::Sender<()>>>,
    tasks: Mutex<TaskStore>,
    codex_claimed: Arc<Mutex<HashSet<String>>>,
    shutdown: AtomicBool,
}

impl Daemon {
    pub fn new() -> Arc<Self> {
        Self::new_with_task_path(state_dir().join("tasks.json"))
    }

    fn new_with_task_path(task_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            sessions: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            watchers: Mutex::new(Vec::new()),
            tasks: Mutex::new(TaskStore::load(task_path)),
            codex_claimed: Arc::new(Mutex::new(HashSet::new())),
            shutdown: AtomicBool::new(false),
        })
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
            if request.kind == "shutdown" {
                for session in self.all_sessions() {
                    let _ = session.kill();
                }
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
                "attach" => return self.attach(stream, reader, request),
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
            "list" => Response {
                ok: true,
                sessions: self.snapshot(),
                tasks: self.task_snapshot(),
                ..Response::default()
            },
            "spawn" => self.spawn(request),
            "kill" => self.kill(&request.id),
            "rename" => self.rename(&request.id, request.name),
            "extract" => self.extract(request),
            kind if kind.starts_with("task_") => self.task_dispatch(request),
            "hook" => self.hook(request),
            "shutdown" => {
                self.shutdown.store(true, Ordering::Release);
                for session in self.all_sessions() {
                    let _ = session.kill();
                }
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
        let response = self.spawn_session(request);
        if response.ok {
            if let Some(agent) = agent {
                let scope = crate::sidebar::scope_key(&cwd);
                if let Ok(mut tasks) = self.tasks.lock() {
                    tasks.add_auto_session(scope, agent.to_owned(), cwd, response.id.clone());
                    let _ = tasks.save();
                }
                self.notify_watchers();
            }
        }
        response
    }

    fn spawn_session(&self, request: Request) -> Response {
        let id = Uuid::new_v4().to_string();
        let rows = if request.rows == 0 { 24 } else { request.rows };
        let cols = if request.cols == 0 { 80 } else { request.cols };
        let prefill = request.prefill;
        let is_codex = request
            .argv
            .first()
            .and_then(|binary| Path::new(binary).file_name())
            .is_some_and(|binary| binary == "codex");
        let cwd = if request.cwd.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&request.cwd)
        };
        let cwd_string = cwd.to_string_lossy().into_owned();
        let spawned_at = SystemTime::now();
        match Session::spawn(id.clone(), request.argv, cwd_string, rows, cols) {
            Ok(session) => {
                Session::queue_prefill(&session, prefill);
                if is_codex {
                    crate::codex::start_harvest(
                        Arc::clone(&session),
                        cwd,
                        spawned_at,
                        Arc::clone(&self.codex_claimed),
                    );
                }
                if let Ok(mut sessions) = self.sessions.write() {
                    sessions.insert(id.clone(), session);
                }
                if let Ok(mut order) = self.order.write() {
                    order.push(id.clone());
                }
                self.notify_watchers();
                Response {
                    ok: true,
                    id,
                    ..Response::default()
                }
            }
            Err(error) => Response {
                error: error.to_string(),
                ..Response::default()
            },
        }
    }

    fn kill(&self, id: &str) -> Response {
        let session = self
            .sessions
            .write()
            .ok()
            .and_then(|mut sessions| sessions.remove(id));
        let Some(session) = session else {
            return Response {
                error: format!("no such session: {id}"),
                ..Response::default()
            };
        };
        if let Ok(mut order) = self.order.write() {
            order.retain(|session_id| session_id != id);
        }
        // Capture the agent-native resume id off the session before it is gone,
        // then park the bound run so the killed session is immediately
        // resumable without waiting for lazy reconciliation.
        let harness = session.snapshot().harness_session_id;
        let result = session.kill();
        self.park_runs_for_session(id, &harness);
        self.notify_watchers();
        match result {
            Ok(()) => Response {
                ok: true,
                ..Response::default()
            },
            Err(error) => Response {
                error: error.to_string(),
                ..Response::default()
            },
        }
    }

    fn rename(&self, id: &str, name: String) -> Response {
        let Some(session) = self.lookup(id) else {
            return Response {
                error: format!("no such session: {id}"),
                ..Response::default()
            };
        };
        session.set_name(name);
        self.notify_watchers();
        Response {
            ok: true,
            ..Response::default()
        }
    }

    fn extract(&self, request: Request) -> Response {
        let Some(session) = self.lookup(&request.id) else {
            return Response {
                error: format!("no such session: {}", request.id),
                ..Response::default()
            };
        };
        match session.extract_text(
            (request.col_start, request.line_start),
            (request.col_end, request.line_end),
        ) {
            Ok(text) => Response {
                ok: true,
                text,
                ..Response::default()
            },
            Err(error) => Response {
                error: error.to_string(),
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
        let response = self.spawn_session(Request {
            kind: "spawn".to_owned(),
            argv: vec![request.agent.clone()],
            cwd: request.cwd.clone(),
            rows: request.rows,
            cols: request.cols,
            prefill: prefill.clone(),
            ..Request::default()
        });
        if !response.ok {
            return response;
        }
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
                cb_session_id: response.id.clone(),
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
            id: response.id,
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
        let response = self.spawn_session(Request {
            kind: "spawn".to_owned(),
            argv,
            cwd,
            rows: request.rows,
            cols: request.cols,
            ..Request::default()
        });
        if !response.ok {
            return response;
        }
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return task_lock_error(),
        };
        if let Some(task) = tasks.get_mut(&request.id) {
            let now = OffsetDateTime::now_utc();
            if let Some(run) = task.runs.iter_mut().find(|run| run.id == request.run_id) {
                run.cb_session_id = response.id.clone();
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
            id: response.id,
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
        let live: HashMap<String, (bool, String)> = self
            .sessions
            .read()
            .map(|sessions| {
                sessions
                    .iter()
                    .map(|(id, session)| {
                        let snapshot = session.snapshot();
                        (
                            id.clone(),
                            (
                                snapshot.exited || snapshot.status == Status::Ended,
                                snapshot.harness_session_id,
                            ),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
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
        let Some(session) = self.lookup(&request.session) else {
            return Response {
                ok: true,
                ..Response::default()
            };
        };
        let (status, message) = status_for_event(&request.event, &request.payload);
        let harness_id = request
            .payload
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if let Some(id) = harness_id.clone() {
            session.set_harness_session_id(id);
        }
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
        let _ = session.flush_prefill();
        session.set_status(status, message);
        self.notify_watchers();
        Response {
            ok: true,
            ..Response::default()
        }
    }

    fn attach(
        self: &Arc<Self>,
        stream: UnixStream,
        mut reader: BufReader<UnixStream>,
        request: Request,
    ) -> io::Result<()> {
        let Some(session) = self.lookup(&request.id) else {
            return write_json(&mut BufWriter::new(stream), &StreamDown::Gone);
        };
        if request.rows > 0 && request.cols > 0 {
            let _ = session.resize(request.rows, request.cols);
        }

        let writer = Arc::new(Mutex::new(BufWriter::new(stream.try_clone()?)));
        let offset = Arc::new(AtomicUsize::new(0));
        let anchor_row = Arc::new(AtomicUsize::new(0));
        let anchored = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let changes = session.subscribe();
        let render_session = Arc::clone(&session);
        let render_writer = Arc::clone(&writer);
        let render_offset = Arc::clone(&offset);
        let render_anchor = Arc::clone(&anchor_row);
        let render_anchored = Arc::clone(&anchored);
        let render_stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut last: Option<TerminalFrame> = None;
            loop {
                if render_stop.load(Ordering::Acquire) {
                    break;
                }
                if render_session.synchronized_output_active() {
                    Session::wait_for_change(&changes, Duration::from_millis(16));
                    continue;
                }
                let frame = if render_anchored.load(Ordering::Acquire) {
                    render_session.render_at_row(render_anchor.load(Ordering::Acquire))
                } else {
                    render_session.render_at(render_offset.load(Ordering::Acquire))
                };
                if let Ok(frame) = frame {
                    render_offset.store(frame.offset, Ordering::Release);
                    if last.as_ref() != Some(&frame) {
                        if let Ok(mut writer) = render_writer.lock() {
                            if write_json(
                                &mut *writer,
                                &StreamDown::Frame {
                                    frame: frame.clone(),
                                },
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        last = Some(frame);
                    }
                }
                if render_session.exited() {
                    if let Ok(mut writer) = render_writer.lock() {
                        let _ = write_json(&mut *writer, &StreamDown::Gone);
                    }
                    break;
                }
                Session::wait_for_change(&changes, Duration::from_millis(250));
            }
        });

        let mut line = String::new();
        while reader.read_line(&mut line)? != 0 {
            if let Ok(up) = serde_json::from_str::<StreamUp>(line.trim_end()) {
                match up.kind.as_str() {
                    "input" => {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(up.data)
                        {
                            let _ = session.write_input(&bytes);
                        }
                    }
                    "paste" => {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(up.data)
                        {
                            let _ = session.paste(&bytes);
                        }
                    }
                    "focus" => {
                        let _ = session.report_focus(up.data == "1");
                    }
                    "mouse" => {
                        let action = match up.mouse_action {
                            0 => MouseAction::Press,
                            1 => MouseAction::Release,
                            _ => MouseAction::Motion,
                        };
                        let button = (up.mouse_button != 0).then_some(up.mouse_button);
                        let _ = session.mouse(
                            action,
                            button,
                            up.mouse_modifiers,
                            up.mouse_x,
                            up.mouse_y,
                            up.mouse_pressed,
                        );
                    }
                    "resize" if up.rows > 0 && up.cols > 0 => {
                        let _ = session.resize(up.rows, up.cols);
                    }
                    "scroll" => {
                        offset.store(up.offset, Ordering::Release);
                        if up.offset == 0 {
                            anchored.store(false, Ordering::Release);
                        } else if let Ok(frame) = session.render_at(up.offset) {
                            anchor_row.store(
                                frame.max_offset.saturating_sub(frame.offset),
                                Ordering::Release,
                            );
                            anchored.store(true, Ordering::Release);
                            if let Ok(mut writer) = writer.lock() {
                                let _ = write_json(&mut *writer, &StreamDown::Frame { frame });
                            }
                        }
                    }
                    "detach" => break,
                    _ => {}
                }
            }
            line.clear();
        }
        stop.store(true, Ordering::Release);
        Ok(())
    }

    fn watch(&self, stream: UnixStream) -> io::Result<()> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut watchers) = self.watchers.lock() {
            watchers.push(sender);
        }
        let mut writer = BufWriter::new(stream);
        loop {
            write_json(
                &mut writer,
                &Response {
                    ok: true,
                    sessions: self.snapshot(),
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

    fn lookup(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(id).cloned())
    }

    fn all_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions
            .read()
            .map(|sessions| sessions.values().cloned().collect())
            .unwrap_or_default()
    }

    fn snapshot(&self) -> Vec<crate::protocol::SessionInfo> {
        let sessions = match self.sessions.read() {
            Ok(sessions) => sessions,
            Err(_) => return Vec::new(),
        };
        self.order
            .read()
            .map(|order| {
                order
                    .iter()
                    .filter_map(|id| sessions.get(id))
                    .map(|session| session.snapshot())
                    .collect()
            })
            .unwrap_or_default()
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
        let session = daemon.lookup(&first.id).expect("first task session");
        let frame = session.render_at(0).expect("prefill frame");
        let text = frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
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
}
