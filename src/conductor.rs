//! The session engine.
//!
//! The `Conductor` owns every agent PTY, its child process group, and the
//! libghostty-vt terminal state, plus the attach frame stream. It is the
//! durable half of Codebridge: it runs as its own `cb conductor` process on
//! `conductor.sock`, so the broker (`cb daemon`) can be rebuilt and restarted
//! against a still-running conductor and its agent sessions survive. Tests drive
//! the conductor in-process; production talks to it over the socket via
//! `ConductorClient`.
//!
//! The conductor deals only in session facts and behaviour (spawn, kill, attach,
//! extract, status/name/harness metadata). It never touches the task store or
//! the broker's watcher fan-out; callers that kill or reap a session perform
//! those side effects themselves using the harness id the conductor hands back.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::io::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{SessionInfo, Status, StreamDown, StreamUp, TerminalFrame};
use crate::session::Session;
use crate::terminal::MouseAction;

/// The conductor's wire protocol version. It is deliberately decoupled from the
/// client-facing `protocol::VERSION`: the broker can be rebuilt and restarted
/// against an already-running conductor as long as this stays unchanged, which
/// is what lets agent sessions survive a broker restart. Keep this protocol
/// minimal and stable.
pub const CONDUCTOR_VERSION: u32 = 1;

/// Path to the conductor's control/attach socket. Separate from the broker's
/// `daemon.sock` so the two processes have independent lifecycles.
pub fn conductor_socket_path() -> PathBuf {
    std::env::var_os("CB_CONDUCTOR_SOCK")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::daemon::state_dir().join("conductor.sock"))
}

/// Control-plane request from the broker (or `cb ctl`) to the conductor. The
/// data-plane attach stream reuses `protocol::StreamUp`/`StreamDown` unchanged
/// once an `Attach` op opens it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConductorRequest {
    Ping,
    Spawn {
        argv: Vec<String>,
        cwd: String,
        rows: u16,
        cols: u16,
        #[serde(default)]
        prefill: String,
    },
    Kill {
        id: String,
    },
    /// Signal every session's process group. Used by `cb stop`.
    KillAll,
    Extract {
        id: String,
        col_start: u16,
        line_start: u32,
        col_end: u16,
        line_end: u32,
    },
    /// Current raw session facts in spawn order.
    List,
    /// Reap cleanly-exited sessions, returning each `(id, harness)` so the
    /// broker can park the bound run.
    Reap,
    SetName {
        id: String,
        name: String,
    },
    /// Fold a hook observation into the session: harness id (if any), a prefill
    /// flush, and the derived status/message. Returns whether the session
    /// existed.
    ApplyHook {
        id: String,
        status: Status,
        message: String,
        #[serde(default)]
        harness: String,
        #[serde(default)]
        transcript: String,
    },
    /// Open the semantic frame stream for a session. After the conductor
    /// acknowledges, the connection carries `StreamUp`/`StreamDown` until close.
    Attach {
        id: String,
        rows: u16,
        cols: u16,
    },
    /// Hot-upgrade in place: snapshot every live session, then `execve` the
    /// current `cb` binary so the conductor runs new code without dropping its
    /// agents. The conductor acknowledges before re-exec'ing; confirm success
    /// by observing the `boot_id` change on a later `Ping`.
    Upgrade,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConductorResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// A fresh random id minted at conductor startup. It changes across a
    /// hot-upgrade `execve` (the successor is a new `Conductor`), which is how
    /// `cb upgrade` confirms the new build took over the same pid.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub harness: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "bool_false")]
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reaped: Vec<(String, String)>,
}

fn bool_false(value: &bool) -> bool {
    !*value
}

/// Every live session's durable state carried across a conductor hot-upgrade,
/// plus the conductor protocol version that wrote it so a mismatched successor
/// can refuse the stash instead of adopting corrupt state.
#[derive(Debug, Serialize, Deserialize)]
struct ResumeStash {
    version: u32,
    sessions: Vec<ResumeRecord>,
}

/// One session's hot-upgrade state: its facts, the raw PTY master fd that
/// survives the `execve` (CLOEXEC cleared beforehand), the child pid, the window
/// size, and the terminal serialized as base64 VT bytes.
#[derive(Debug, Serialize, Deserialize)]
struct ResumeRecord {
    info: SessionInfo,
    master_fd: RawFd,
    child_pid: u32,
    rows: u16,
    cols: u16,
    vt_base64: String,
}

/// Path to the hot-upgrade stash. One conductor exists per state dir, so a fixed
/// name is safe; `resume_from` deletes it once every session is adopted.
fn resume_stash_path() -> PathBuf {
    crate::daemon::state_dir().join("conductor-resume.json")
}

/// Clears the close-on-exec flag on `fd` so it survives an `execve` into the
/// hot-upgrade successor.
fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a live fd with no pointer arguments.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub struct Conductor {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    order: RwLock<Vec<String>>,
    codex_claimed: Arc<Mutex<HashSet<String>>>,
    shutdown: AtomicBool,
    boot_id: String,
}

impl Conductor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            codex_claimed: Arc::new(Mutex::new(HashSet::new())),
            shutdown: AtomicBool::new(false),
            boot_id: Uuid::new_v4().to_string(),
        })
    }

    /// Spawns a session on a fresh PTY and records it in the engine. Returns the
    /// new session id. Codex sessions begin rollout-id harvesting; any prefill
    /// is queued for the first hook or the bounded fallback timer. Recording the
    /// session as a task and notifying watchers are the caller's concern.
    pub fn spawn_session(
        &self,
        argv: Vec<String>,
        cwd: String,
        rows: u16,
        cols: u16,
        prefill: String,
    ) -> Result<String, String> {
        let id = Uuid::new_v4().to_string();
        let rows = if rows == 0 { 24 } else { rows };
        let cols = if cols == 0 { 80 } else { cols };
        let is_codex = argv
            .first()
            .and_then(|binary| Path::new(binary).file_name())
            .is_some_and(|binary| binary == "codex");
        let cwd = if cwd.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&cwd)
        };
        let cwd_string = cwd.to_string_lossy().into_owned();
        let spawned_at = SystemTime::now();
        // The host terminal theme is discovered client-side and applied later
        // via `Session::apply_host_theme`; the conductor has no host terminal of
        // its own, so it spawns with an empty theme.
        match Session::spawn(
            id.clone(),
            argv,
            cwd_string,
            rows,
            cols,
            crate::terminal_theme::TerminalTheme::default(),
        ) {
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
                Ok(id)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Removes a session from the engine and signals its process group. Returns
    /// the session's captured harness id and the kill result, or `None` when no
    /// such session exists. The harness id is returned even if the kill itself
    /// fails so the caller can still park the bound run.
    pub fn kill(&self, id: &str) -> Option<(String, Result<(), String>)> {
        let session = self
            .sessions
            .write()
            .ok()
            .and_then(|mut sessions| sessions.remove(id))?;
        if let Ok(mut order) = self.order.write() {
            order.retain(|session_id| session_id != id);
        }
        let harness = session.snapshot().harness_session_id;
        let result = session.kill().map_err(|error| error.to_string());
        Some((harness, result))
    }

    /// Drops sessions whose agent exited deliberately (`/exit`, normal quit),
    /// returning each reaped `(id, harness_session_id)` so the caller can park
    /// the bound run. Sessions that crashed (non-zero exit or a signal) are left
    /// in place so their ended row stays visible. A no-op when nothing exited
    /// cleanly.
    pub fn reap(&self) -> Vec<(String, String)> {
        let reaped: Vec<(String, String)> = {
            let Ok(mut sessions) = self.sessions.write() else {
                return Vec::new();
            };
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, session)| session.exited() && session.exit_clean())
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    sessions
                        .remove(&id)
                        .map(|session| (id, session.snapshot().harness_session_id))
                })
                .collect()
        };
        if !reaped.is_empty() {
            if let Ok(mut order) = self.order.write() {
                order.retain(|id| !reaped.iter().any(|(reaped_id, _)| reaped_id == id));
            }
        }
        reaped
    }

    pub fn extract(&self, id: &str, start: (u16, u32), end: (u16, u32)) -> Result<String, String> {
        let Some(session) = self.lookup(id) else {
            return Err(format!("no such session: {id}"));
        };
        session
            .extract_text(start, end)
            .map_err(|error| error.to_string())
    }

    /// Renames a session. Returns whether the session existed.
    pub fn set_name(&self, id: &str, name: String) -> bool {
        match self.lookup(id) {
            Some(session) => {
                session.set_name(name);
                true
            }
            None => false,
        }
    }

    /// Folds a hook observation into a session: records the harness id (when
    /// non-empty), flushes any queued prefill, and applies the derived
    /// status/message. Returns whether the session existed so the broker can
    /// short-circuit hooks for unknown sessions exactly as before.
    pub fn apply_hook(
        &self,
        id: &str,
        status: Status,
        message: String,
        harness: &str,
        transcript: &str,
    ) -> bool {
        let Some(session) = self.lookup(id) else {
            return false;
        };
        if !harness.is_empty() {
            session.set_harness_session_id(harness.to_owned());
        }
        if !transcript.is_empty() {
            session.set_transcript_path(transcript.to_owned());
        }
        let _ = session.flush_prefill();
        session.set_status(status, message);
        true
    }

    pub fn lookup(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(id).cloned())
    }

    pub fn all_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions
            .read()
            .map(|sessions| sessions.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Raw session facts in stable spawn order. Status, name, and last message
    /// still ride on the session snapshot today; the broker will own those
    /// annotations once the process boundary lands.
    pub fn snapshot(&self) -> Vec<SessionInfo> {
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

    /// Per-session `(ended, harness_session_id)` used by the broker to
    /// reconcile task runs against the live session map.
    pub fn session_liveness(&self) -> HashMap<String, (bool, String)> {
        self.sessions
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
            .unwrap_or_default()
    }

    /// Collects the durable state of every live session for a hot-upgrade,
    /// clearing CLOEXEC on each PTY master fd so it survives the `execve` into
    /// the successor. Sessions that already exited are skipped — there is no
    /// live child or fd to carry over, and the broker keeps their run resumable.
    /// Spawn order is preserved.
    fn upgrade_records(&self) -> Vec<ResumeRecord> {
        let order = self
            .order
            .read()
            .map(|order| order.clone())
            .unwrap_or_default();
        let Ok(sessions) = self.sessions.read() else {
            return Vec::new();
        };
        let mut records = Vec::new();
        for id in &order {
            let Some(session) = sessions.get(id) else {
                continue;
            };
            if session.exited() {
                continue;
            }
            let (Some(fd), Some(pid)) = (session.master_raw_fd(), session.child_pid()) else {
                continue;
            };
            if clear_cloexec(fd).is_err() {
                continue;
            }
            let (rows, cols) = session.winsize().unwrap_or((24, 80));
            let vt = session.vt_snapshot().unwrap_or_default();
            records.push(ResumeRecord {
                info: session.snapshot(),
                master_fd: fd,
                child_pid: pid,
                rows,
                cols,
                vt_base64: base64::engine::general_purpose::STANDARD.encode(&vt),
            });
        }
        records
    }

    /// Snapshots every live session to a stash file, then `execve`s the current
    /// `cb` binary as `cb conductor --resume <stash>`. Running new code in the
    /// same pid keeps the agents (children of that pid) alive and their PTY
    /// master fds (CLOEXEC cleared) open. On success `exec` never returns; it
    /// only returns here — as the exec error — on failure, and since this reads
    /// session state without destroying it the caller keeps serving intact
    /// sessions on the current build.
    fn hot_upgrade(&self) -> io::Result<()> {
        use std::os::unix::process::CommandExt;

        let stash = ResumeStash {
            version: CONDUCTOR_VERSION,
            sessions: self.upgrade_records(),
        };
        let data = serde_json::to_vec(&stash)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let path = resume_stash_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, data)?;
        let executable = std::env::current_exe()?;
        // `exec` replaces this process image in place; everything above is
        // already flushed to disk. The returned value is always the failure.
        Err(std::process::Command::new(executable)
            .arg("conductor")
            .arg("--resume")
            .arg(&path)
            .exec())
    }

    /// Adopts every session in a resume stash written by a predecessor
    /// conductor's `hot_upgrade`, then deletes the stash. Each session is rebuilt
    /// from its surviving master fd + child pid with its terminal replayed from
    /// the stashed VT bytes. Called by `cb conductor --resume <path>` before
    /// `run`, so the socket only comes up once the sessions are restored.
    pub fn resume_from(&self, path: &Path) -> io::Result<()> {
        let bytes = fs::read(path)?;
        let _ = fs::remove_file(path);
        let stash: ResumeStash = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if stash.version != CONDUCTOR_VERSION {
            eprintln!(
                "cb conductor: ignoring resume stash v{} (want v{})",
                stash.version, CONDUCTOR_VERSION
            );
            return Ok(());
        }
        for record in stash.sessions {
            let id = record.info.id.clone();
            let vt = base64::engine::general_purpose::STANDARD
                .decode(record.vt_base64.as_bytes())
                .unwrap_or_default();
            match Session::adopt(
                record.info,
                record.master_fd,
                record.child_pid,
                record.rows,
                record.cols,
                &vt,
            ) {
                Ok(session) => {
                    if let Ok(mut sessions) = self.sessions.write() {
                        sessions.insert(id.clone(), session);
                    }
                    if let Ok(mut order) = self.order.write() {
                        order.push(id);
                    }
                }
                Err(error) => {
                    eprintln!("cb conductor: failed to adopt session {id}: {error}");
                }
            }
        }
        Ok(())
    }

    /// Streams semantic terminal frames for one session to an attached client
    /// and forwards the client's input/resize/scroll/mouse back to the PTY. The
    /// daemon sends an immediate frame and then only changed frames; an
    /// attachment entering history records an absolute row so new output cannot
    /// move that view, and offset zero resumes live follow.
    pub fn attach(
        self: &Arc<Self>,
        stream: UnixStream,
        mut reader: BufReader<UnixStream>,
        id: &str,
        rows: u16,
        cols: u16,
    ) -> io::Result<()> {
        let Some(session) = self.lookup(id) else {
            return write_json(
                &mut BufWriter::new(stream),
                &StreamDown::Gone { clean: false },
            );
        };
        if rows > 0 && cols > 0 {
            let _ = session.resize(rows, cols);
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
                        let _ = write_json(
                            &mut *writer,
                            &StreamDown::Gone {
                                clean: render_session.exit_clean(),
                            },
                        );
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
                    // The user hit Escape on the attached session. The Escape
                    // byte itself follows as ordinary `input`; this arrives
                    // first so the baseline transcript length is captured before
                    // the agent can react. Claude fires no hook on interrupt, so
                    // we confirm it against the transcript rather than guess.
                    "interrupt_check" => check_user_interrupt(Arc::clone(&session)),
                    "detach" => break,
                    _ => {}
                }
            }
            line.clear();
        }
        stop.store(true, Ordering::Release);
        Ok(())
    }

    /// Serves the conductor's control and attach protocol on `path`. Mirrors the
    /// broker's accept loop: nonblocking accept, one worker thread per client,
    /// and an atomic shutdown flag (set by `KillAll`) that stops the loop.
    pub fn run(self: &Arc<Self>, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("conductor already running at {}", path.display()),
                ));
            }
            fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        while !self.shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    let conductor = Arc::clone(self);
                    thread::spawn(move || {
                        if let Err(error) = conductor.handle_conn(stream) {
                            if !matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe
                                    | io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::UnexpectedEof
                            ) {
                                eprintln!("cb conductor client error: {error}");
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

    fn handle_conn(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        while reader.read_line(&mut line)? != 0 {
            let request = match serde_json::from_str::<ConductorRequest>(line.trim_end()) {
                Ok(request) => request,
                Err(error) => {
                    write_json(
                        &mut BufWriter::new(stream.try_clone()?),
                        &ConductorResponse {
                            error: format!("bad request: {error}"),
                            ..ConductorResponse::default()
                        },
                    )?;
                    line.clear();
                    continue;
                }
            };
            // Attach converts the connection into a bidirectional frame stream
            // for the rest of its life, so it must be the terminal op.
            if let ConductorRequest::Attach { id, rows, cols } = request {
                return self.attach(stream, reader, &id, rows, cols);
            }
            // Upgrade acks first so `cb upgrade` learns the request landed, then
            // re-execs in place. On success the exec never returns; on failure
            // we log and keep serving on the current build with sessions intact.
            if matches!(request, ConductorRequest::Upgrade) {
                write_json(
                    &mut BufWriter::new(stream.try_clone()?),
                    &ConductorResponse {
                        ok: true,
                        ..ConductorResponse::default()
                    },
                )?;
                if let Err(error) = self.hot_upgrade() {
                    eprintln!("cb conductor hot-upgrade failed, staying on current build: {error}");
                }
                line.clear();
                continue;
            }
            let response = self.dispatch_control(request);
            write_json(&mut BufWriter::new(stream.try_clone()?), &response)?;
            line.clear();
        }
        Ok(())
    }

    fn dispatch_control(self: &Arc<Self>, request: ConductorRequest) -> ConductorResponse {
        match request {
            ConductorRequest::Ping => ConductorResponse {
                ok: true,
                version: Some(CONDUCTOR_VERSION),
                pid: Some(std::process::id()),
                boot_id: self.boot_id.clone(),
                ..ConductorResponse::default()
            },
            ConductorRequest::Spawn {
                argv,
                cwd,
                rows,
                cols,
                prefill,
            } => match self.spawn_session(argv, cwd, rows, cols, prefill) {
                Ok(id) => ConductorResponse {
                    ok: true,
                    id,
                    ..ConductorResponse::default()
                },
                Err(error) => ConductorResponse {
                    error,
                    ..ConductorResponse::default()
                },
            },
            // `applied` reports whether the session existed, so the broker can
            // reproduce the in-process `Option` (None = no such session) and
            // still park the run even when the kill signal itself errored.
            ConductorRequest::Kill { id } => match self.kill(&id) {
                Some((harness, Ok(()))) => ConductorResponse {
                    ok: true,
                    applied: true,
                    harness,
                    ..ConductorResponse::default()
                },
                Some((harness, Err(error))) => ConductorResponse {
                    applied: true,
                    harness,
                    error,
                    ..ConductorResponse::default()
                },
                None => ConductorResponse {
                    error: format!("no such session: {id}"),
                    ..ConductorResponse::default()
                },
            },
            ConductorRequest::KillAll => {
                for session in self.all_sessions() {
                    let _ = session.kill();
                }
                self.shutdown.store(true, Ordering::Release);
                ConductorResponse {
                    ok: true,
                    ..ConductorResponse::default()
                }
            }
            ConductorRequest::Extract {
                id,
                col_start,
                line_start,
                col_end,
                line_end,
            } => match self.extract(&id, (col_start, line_start), (col_end, line_end)) {
                Ok(text) => ConductorResponse {
                    ok: true,
                    text,
                    ..ConductorResponse::default()
                },
                Err(error) => ConductorResponse {
                    error,
                    ..ConductorResponse::default()
                },
            },
            ConductorRequest::List => ConductorResponse {
                ok: true,
                sessions: self.snapshot(),
                ..ConductorResponse::default()
            },
            ConductorRequest::Reap => ConductorResponse {
                ok: true,
                reaped: self.reap(),
                ..ConductorResponse::default()
            },
            ConductorRequest::SetName { id, name } => ConductorResponse {
                ok: true,
                applied: self.set_name(&id, name),
                ..ConductorResponse::default()
            },
            ConductorRequest::ApplyHook {
                id,
                status,
                message,
                harness,
                transcript,
            } => ConductorResponse {
                ok: true,
                applied: self.apply_hook(&id, status, message, &harness, &transcript),
                ..ConductorResponse::default()
            },
            // Attach is intercepted before dispatch; reaching here is a bug.
            ConductorRequest::Attach { .. } => ConductorResponse {
                error: "attach must be the terminal request on a connection".to_owned(),
                ..ConductorResponse::default()
            },
            // Upgrade is intercepted before dispatch; reaching here is a bug.
            ConductorRequest::Upgrade => ConductorResponse {
                error: "upgrade must be handled before control dispatch".to_owned(),
                ..ConductorResponse::default()
            },
        }
    }
}

fn write_json(writer: &mut impl Write, value: &impl serde::Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// The prefix Claude writes to its transcript once it has actually torn down a
/// turn the user interrupted. Covers both `[Request interrupted by user]` and
/// `[Request interrupted by user for tool use]`.
const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// Confirm a user Escape against Claude's own transcript instead of guessing.
///
/// Claude fires no hook on interrupt, so a working session's spinner would
/// otherwise spin forever. The TUI sends `interrupt_check` the instant it
/// forwards an Escape — ahead of the `0x1b` input byte — so the baseline length
/// captured here precedes anything Claude writes in response. We then poll the
/// transcript briefly; if the marker lands in the bytes appended after the
/// baseline we clear the spinner to `WaitingUser`. If it never shows (a
/// still-running turn, or an Escape the agent ignored) we leave the status
/// untouched. Positive-only: this can only fix a stuck spinner, never invent a
/// false "done". Runs its poll off-thread so it never stalls the input path.
fn check_user_interrupt(session: Arc<Session>) {
    let info = session.snapshot();
    // Only a working turn can be interrupted, and only Claude reports a
    // transcript path to check against.
    if info.status != Status::Working || info.transcript_path.is_empty() {
        return;
    }
    if info
        .argv
        .first()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        != Some("claude")
    {
        return;
    }
    // Establish the baseline synchronously, before the caller processes the
    // Escape input byte, so a stale interrupt earlier in the transcript can
    // never match and a fast interrupt can never slip in below the baseline.
    let Ok(meta) = fs::metadata(&info.transcript_path) else {
        return;
    };
    let baseline = meta.len();
    let path = info.transcript_path;
    thread::spawn(move || {
        // ~3s of 200ms polls: Claude records the marker within a few hundred ms
        // of the interrupt; giving up quietly after that is exactly today's
        // (stuck-spinner) behaviour, so erring short is safe.
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(200));
            if interrupt_recorded_after(&path, baseline) {
                // A real hook may have already advanced the session; only clear
                // if it is still sitting in the stuck working state.
                if session.snapshot().status == Status::Working {
                    session.set_status(Status::WaitingUser, String::new());
                }
                return;
            }
        }
    });
}

/// Whether the interrupt marker appears in `path`'s content past byte offset
/// `baseline`. Scanning only the appended bytes means a stale interrupt earlier
/// in the transcript can never match. A missing/unreadable file (the marker has
/// not landed yet) reads as `false` so the caller simply polls again.
fn interrupt_recorded_after(path: &str, baseline: u64) -> bool {
    read_from(path, baseline)
        .map(|appended| String::from_utf8_lossy(&appended).contains(INTERRUPT_MARKER))
        .unwrap_or(false)
}

/// Read `path` from byte offset `from` to EOF, so only transcript content
/// written after an Escape is scanned for the interrupt marker.
fn read_from(path: &str, from: u64) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// The broker's view of the session engine. Two implementations back it: an
/// in-process `Arc<Conductor>` (used by tests, so they stay fast and hermetic)
/// and a `ConductorClient` that talks to the conductor process over its socket
/// (used in production, where the two run as separate processes). Keeping the
/// surface identical lets the broker code be agnostic to which is in play.
pub trait Engine: Send + Sync {
    fn spawn_session(
        &self,
        argv: Vec<String>,
        cwd: String,
        rows: u16,
        cols: u16,
        prefill: String,
    ) -> Result<String, String>;
    fn kill(&self, id: &str) -> Option<(String, Result<(), String>)>;
    fn kill_all(&self);
    fn reap(&self) -> Vec<(String, String)>;
    fn extract(&self, id: &str, start: (u16, u32), end: (u16, u32)) -> Result<String, String>;
    fn snapshot(&self) -> Vec<SessionInfo>;
    fn session_liveness(&self) -> HashMap<String, (bool, String)>;
    fn set_name(&self, id: &str, name: String) -> bool;
    fn apply_hook(
        &self,
        id: &str,
        status: Status,
        message: String,
        harness: &str,
        transcript: &str,
    ) -> bool;
}

impl Engine for Arc<Conductor> {
    fn spawn_session(
        &self,
        argv: Vec<String>,
        cwd: String,
        rows: u16,
        cols: u16,
        prefill: String,
    ) -> Result<String, String> {
        Conductor::spawn_session(self, argv, cwd, rows, cols, prefill)
    }
    fn kill(&self, id: &str) -> Option<(String, Result<(), String>)> {
        Conductor::kill(self, id)
    }
    fn kill_all(&self) {
        for session in Conductor::all_sessions(self) {
            let _ = session.kill();
        }
    }
    fn reap(&self) -> Vec<(String, String)> {
        Conductor::reap(self)
    }
    fn extract(&self, id: &str, start: (u16, u32), end: (u16, u32)) -> Result<String, String> {
        Conductor::extract(self, id, start, end)
    }
    fn snapshot(&self) -> Vec<SessionInfo> {
        Conductor::snapshot(self)
    }
    fn session_liveness(&self) -> HashMap<String, (bool, String)> {
        Conductor::session_liveness(self)
    }
    fn set_name(&self, id: &str, name: String) -> bool {
        Conductor::set_name(self, id, name)
    }
    fn apply_hook(
        &self,
        id: &str,
        status: Status,
        message: String,
        harness: &str,
        transcript: &str,
    ) -> bool {
        Conductor::apply_hook(self, id, status, message, harness, transcript)
    }
}

/// A broker-side client for the conductor's socket. Control ops dial the socket
/// per call — connects are cheap and this keeps the client stateless. The broker
/// never proxies the attach data plane: clients stream frames straight from the
/// conductor socket.
pub struct ConductorClient {
    socket: PathBuf,
}

impl ConductorClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub fn ping(&self) -> io::Result<ConductorResponse> {
        self.call(&ConductorRequest::Ping)
    }

    /// Best-effort request that the conductor kill every session and exit. Used
    /// by `cb stop`; ignores transport errors since a missing conductor is
    /// already stopped.
    pub fn stop(&self) {
        let _ = self.call(&ConductorRequest::KillAll);
    }

    /// Asks the conductor to hot-upgrade in place. The returned ack only means
    /// the request landed; the caller confirms the new build took over by
    /// watching the `boot_id` from `ping` change.
    pub fn upgrade(&self) -> io::Result<ConductorResponse> {
        self.call(&ConductorRequest::Upgrade)
    }

    fn call(&self, request: &ConductorRequest) -> io::Result<ConductorResponse> {
        let mut stream = UnixStream::connect(&self.socket)?;
        serde_json::to_writer(&mut stream, request)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        if line.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "conductor closed connection without a response",
            ));
        }
        Ok(serde_json::from_str(&line)?)
    }
}

impl Engine for ConductorClient {
    fn spawn_session(
        &self,
        argv: Vec<String>,
        cwd: String,
        rows: u16,
        cols: u16,
        prefill: String,
    ) -> Result<String, String> {
        match self.call(&ConductorRequest::Spawn {
            argv,
            cwd,
            rows,
            cols,
            prefill,
        }) {
            Ok(response) if response.ok => Ok(response.id),
            Ok(response) => Err(response.error),
            Err(error) => Err(error.to_string()),
        }
    }

    fn kill(&self, id: &str) -> Option<(String, Result<(), String>)> {
        let response = self
            .call(&ConductorRequest::Kill { id: id.to_owned() })
            .ok()?;
        if !response.applied {
            return None;
        }
        let result = if response.error.is_empty() {
            Ok(())
        } else {
            Err(response.error)
        };
        Some((response.harness, result))
    }

    fn kill_all(&self) {
        let _ = self.call(&ConductorRequest::KillAll);
    }

    fn reap(&self) -> Vec<(String, String)> {
        self.call(&ConductorRequest::Reap)
            .map(|response| response.reaped)
            .unwrap_or_default()
    }

    fn extract(&self, id: &str, start: (u16, u32), end: (u16, u32)) -> Result<String, String> {
        match self.call(&ConductorRequest::Extract {
            id: id.to_owned(),
            col_start: start.0,
            line_start: start.1,
            col_end: end.0,
            line_end: end.1,
        }) {
            Ok(response) if response.ok => Ok(response.text),
            Ok(response) => Err(response.error),
            Err(error) => Err(error.to_string()),
        }
    }

    fn snapshot(&self) -> Vec<SessionInfo> {
        self.call(&ConductorRequest::List)
            .map(|response| response.sessions)
            .unwrap_or_default()
    }

    fn session_liveness(&self) -> HashMap<String, (bool, String)> {
        // Derived from the same snapshot the broker already relies on: a session
        // is "ended" when it has exited or reports the Ended status.
        self.snapshot()
            .into_iter()
            .map(|info| {
                (
                    info.id,
                    (
                        info.exited || info.status == Status::Ended,
                        info.harness_session_id,
                    ),
                )
            })
            .collect()
    }

    fn set_name(&self, id: &str, name: String) -> bool {
        self.call(&ConductorRequest::SetName {
            id: id.to_owned(),
            name,
        })
        .map(|response| response.applied)
        .unwrap_or(false)
    }

    fn apply_hook(
        &self,
        id: &str,
        status: Status,
        message: String,
        harness: &str,
        transcript: &str,
    ) -> bool {
        self.call(&ConductorRequest::ApplyHook {
            id: id.to_owned(),
            status,
            message,
            harness: harness.to_owned(),
            transcript: transcript.to_owned(),
        })
        .map(|response| response.applied)
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_cat(conductor: &Arc<Conductor>) -> String {
        conductor
            .spawn_session(
                vec!["/bin/cat".to_owned()],
                "/tmp".to_owned(),
                4,
                40,
                String::new(),
            )
            .expect("spawn")
    }

    #[test]
    fn spawn_records_session_in_stable_order() {
        let conductor = Conductor::new();
        let first = spawn_cat(&conductor);
        let second = spawn_cat(&conductor);
        let ids: Vec<String> = conductor.snapshot().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![first.clone(), second]);
        assert!(conductor.lookup(&first).is_some());
    }

    #[test]
    fn interrupt_confirmation_is_positive_only_and_ignores_stale_markers() {
        let path = std::env::temp_dir().join(format!(
            "cb-transcript-interrupt-{}.jsonl",
            std::process::id()
        ));
        let path_str = path.to_string_lossy().into_owned();
        let _ = fs::remove_file(&path);

        // A transcript that already carries an interrupt from earlier in the
        // session; the baseline is captured just after it, as if the user hit
        // Escape now.
        fs::write(
            &path,
            "{\"content\":\"[Request interrupted by user]\"}\n{\"type\":\"assistant\"}\n",
        )
        .expect("seed transcript");
        let baseline = fs::metadata(&path).expect("meta").len();

        // The stale marker lives before the baseline and must never match.
        assert!(!interrupt_recorded_after(&path_str, baseline));

        // Claude keeps working (appends a tool call): still no fresh interrupt.
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append handle");
        file.write_all(b"{\"type\":\"tool_use\"}\n").expect("write");
        assert!(!interrupt_recorded_after(&path_str, baseline));

        // The user's interrupt now lands after the baseline: it is confirmed.
        file.write_all(b"{\"content\":\"[Request interrupted by user for tool use]\"}\n")
            .expect("write marker");
        assert!(interrupt_recorded_after(&path_str, baseline));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn interrupt_confirmation_reads_false_when_transcript_is_missing() {
        assert!(!interrupt_recorded_after(
            "/nonexistent/cb-transcript.jsonl",
            0
        ));
    }

    #[test]
    fn kill_removes_the_session_and_returns_its_harness_id() {
        let conductor = Conductor::new();
        let id = spawn_cat(&conductor);
        conductor
            .lookup(&id)
            .expect("session")
            .set_harness_session_id("sess-42".to_owned());

        let (harness, result) = conductor.kill(&id).expect("killed");
        assert_eq!(harness, "sess-42");
        assert!(result.is_ok());
        assert!(conductor.lookup(&id).is_none());
        assert!(conductor.snapshot().is_empty());
        assert!(conductor.kill(&id).is_none(), "second kill finds nothing");
    }

    #[test]
    fn extract_reads_terminal_text_by_range() {
        let conductor = Conductor::new();
        let id = conductor
            .spawn_session(
                vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "printf 'alpha beta'; sleep 5".to_owned(),
                ],
                "/tmp".to_owned(),
                4,
                40,
                String::new(),
            )
            .expect("spawn");
        let deadline = SystemTime::now() + Duration::from_secs(2);
        loop {
            let text = conductor.extract(&id, (0, 0), (3, 0)).expect("extract");
            if text == "alph" {
                break;
            }
            assert!(SystemTime::now() < deadline, "expected text never rendered");
            thread::sleep(Duration::from_millis(20));
        }
        let _ = conductor.kill(&id);
    }

    #[test]
    fn reap_drops_clean_exits_and_keeps_crashes() {
        let conductor = Conductor::new();
        let clean = conductor
            .spawn_session(
                vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()],
                "/tmp".to_owned(),
                4,
                40,
                String::new(),
            )
            .expect("spawn clean");
        let crash = conductor
            .spawn_session(
                vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()],
                "/tmp".to_owned(),
                4,
                40,
                String::new(),
            )
            .expect("spawn crash");
        conductor
            .lookup(&clean)
            .expect("clean session")
            .set_harness_session_id("sess-clean".to_owned());

        // Let both children exit and the waiter record their status.
        let deadline = SystemTime::now() + Duration::from_secs(2);
        while conductor.lookup(&clean).is_some_and(|s| !s.exited())
            || conductor.lookup(&crash).is_some_and(|s| !s.exited())
        {
            assert!(SystemTime::now() < deadline, "children never exited");
            thread::sleep(Duration::from_millis(20));
        }

        let reaped = conductor.reap();
        assert_eq!(reaped, vec![(clean.clone(), "sess-clean".to_owned())]);
        assert!(conductor.lookup(&clean).is_none(), "clean exit reaped");
        assert!(conductor.lookup(&crash).is_some(), "crash stays visible");
    }

    #[test]
    fn serves_control_protocol_over_a_socket() {
        let socket =
            std::env::temp_dir().join(format!("cb-conductor-sock-{}.sock", std::process::id()));
        let _ = fs::remove_file(&socket);
        let conductor = Conductor::new();
        let server = Arc::clone(&conductor);
        let path = socket.clone();
        let handle = thread::spawn(move || {
            let _ = server.run(&path);
        });
        let deadline = SystemTime::now() + Duration::from_secs(2);
        while UnixStream::connect(&socket).is_err() {
            assert!(
                SystemTime::now() < deadline,
                "conductor socket never came up"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let call = |request: &ConductorRequest| -> ConductorResponse {
            let mut stream = UnixStream::connect(&socket).expect("connect");
            serde_json::to_writer(&mut stream, request).expect("write request");
            stream.write_all(b"\n").expect("newline");
            stream.flush().expect("flush");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("read response");
            serde_json::from_str(&line).expect("parse response")
        };

        let ping = call(&ConductorRequest::Ping);
        assert!(ping.ok);
        assert_eq!(ping.version, Some(CONDUCTOR_VERSION));

        let spawned = call(&ConductorRequest::Spawn {
            argv: vec!["/bin/cat".to_owned()],
            cwd: "/tmp".to_owned(),
            rows: 4,
            cols: 40,
            prefill: String::new(),
        });
        assert!(spawned.ok, "spawn failed: {}", spawned.error);
        let id = spawned.id;

        let listed = call(&ConductorRequest::List);
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].id, id);

        assert!(
            call(&ConductorRequest::SetName {
                id: id.clone(),
                name: "renamed".to_owned(),
            })
            .applied
        );
        assert_eq!(call(&ConductorRequest::List).sessions[0].name, "renamed");

        assert!(call(&ConductorRequest::Kill { id: id.clone() }).ok);
        assert!(call(&ConductorRequest::List).sessions.is_empty());

        // KillAll trips the shutdown flag; the serve loop must return.
        let _ = call(&ConductorRequest::KillAll);
        let deadline = SystemTime::now() + Duration::from_secs(2);
        while UnixStream::connect(&socket).is_ok() {
            assert!(SystemTime::now() < deadline, "conductor did not shut down");
            thread::sleep(Duration::from_millis(10));
        }
        let _ = handle.join();
        let _ = fs::remove_file(&socket);
    }
}
