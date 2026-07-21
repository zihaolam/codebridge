use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use ratatui::style::{Color, Modifier};
use thiserror::Error;

use crate::protocol::{Cell, SessionInfo, Status, TerminalFrame};
use crate::terminal::{MouseAction, RenderedTerminal, Terminal, TerminalError};

const DEFAULT_SCROLLBACK_LINES: usize = 10_000;
const SYNC_WATCHDOG_MS: u64 = 150;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("PTY error: {0}")]
    Pty(#[from] anyhow::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error("session lock is poisoned")]
    Poisoned,
}

struct Metadata {
    name: String,
    status: Status,
    last_message: String,
    harness_session_id: String,
    transcript_path: String,
    status_since_unix_ms: u64,
}

/// How a session's PTY master is backed. A session Codebridge spawned owns
/// portable_pty's master handle; a session adopted across a conductor
/// hot-upgrade holds only the raw master fd that survived the `execve`, since
/// portable_pty cannot rebuild its handle from a bare fd.
enum Master {
    Portable(Box<dyn MasterPty + Send>),
    Raw(OwnedFd),
}

/// How a session's child is reaped. Spawned children reap through portable_pty;
/// an adopted child is reaped by `waitpid` on its pid, which still yields the
/// exit status because a hot-upgraded conductor keeps the same pid and so stays
/// the child's parent.
enum Reaper {
    Child(Box<dyn Child + Send + Sync>),
    Pid(libc::pid_t),
}

pub struct Session {
    id: String,
    argv: Vec<String>,
    cwd: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Mutex<Master>,
    reaper: Mutex<Reaper>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    process_id: Option<u32>,
    terminal: Arc<Mutex<Terminal>>,
    metadata: RwLock<Metadata>,
    exited: AtomicBool,
    exit_clean: AtomicBool,
    sync_since_unix_ms: AtomicU64,
    generation: AtomicU64,
    /// Monotonic count of turns that entered `Working`. Bumped on every
    /// `Working` hook (each `UserPromptSubmit`/`PreToolUse`/... observation), so
    /// the interrupt confirmation can tell a still-stuck interrupted turn from a
    /// fresh turn that started right after the interrupt (queued-message
    /// steering: Claude tears the turn down *and* immediately submits the queued
    /// prompt). See `check_user_interrupt`.
    working_seq: AtomicU64,
    subscribers: Mutex<Vec<mpsc::Sender<u64>>>,
    pending_prefill: Mutex<Option<Vec<u8>>>,
}

impl Session {
    pub fn spawn(
        id: String,
        argv: Vec<String>,
        cwd: String,
        rows: u16,
        cols: u16,
        host_theme: crate::terminal_theme::TerminalTheme,
    ) -> Result<Arc<Self>, SessionError> {
        let argv = if argv.is_empty() {
            vec!["claude".to_owned()]
        } else {
            argv
        };
        let pair = native_pty_system().openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(&argv[0]);
        for argument in &argv[1..] {
            command.arg(argument);
        }
        if !cwd.is_empty() {
            command.cwd(Path::new(&cwd));
        }
        command.env("CB_SESSION", &id);
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command)?;
        let process_id = child.process_id();
        let killer = child.clone_killer();
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let replies = Arc::clone(&writer);
        let mut terminal = Terminal::new(
            cols.max(1),
            rows.max(1),
            DEFAULT_SCROLLBACK_LINES,
            move |bytes| {
                if let Ok(mut writer) = replies.lock() {
                    let _ = writer.write_all(bytes);
                    let _ = writer.flush();
                }
            },
        )?;
        // Seed the embedded terminal with the host terminal's default colors
        // before the child produces any output, so libghostty answers the
        // agent's `OSC 10/11 ;?` queries with the real terminal colors. This is
        // what lets Codex derive an input box shade that contrasts with what
        // Codebridge renders. Fed before `start_reader` so it is applied ahead
        // of the child's own early color query.
        if !host_theme.is_empty() {
            terminal.feed(&host_theme.set_sequences());
        }
        let now = unix_ms();
        let session = Arc::new(Self {
            id,
            argv,
            cwd,
            writer,
            master: Mutex::new(Master::Portable(pair.master)),
            reaper: Mutex::new(Reaper::Child(child)),
            killer: Mutex::new(Some(killer)),
            process_id,
            terminal: Arc::new(Mutex::new(terminal)),
            metadata: RwLock::new(Metadata {
                name: String::new(),
                status: Status::Starting,
                last_message: String::new(),
                harness_session_id: String::new(),
                transcript_path: String::new(),
                status_since_unix_ms: now,
            }),
            exited: AtomicBool::new(false),
            exit_clean: AtomicBool::new(false),
            sync_since_unix_ms: AtomicU64::new(0),
            generation: AtomicU64::new(1),
            working_seq: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
            pending_prefill: Mutex::new(None),
        });
        Self::start_reader(&session, reader);
        Self::start_waiter(&session);
        Ok(session)
    }

    /// Reconstructs a live session from a PTY master fd and child pid that
    /// survived a conductor hot-upgrade (`execve`), replaying `vt_bytes` to
    /// rebuild the terminal's scrollback, screen, cursor, and styles. The
    /// predecessor conductor cleared CLOEXEC on `master_fd` so it outlived the
    /// exec; `child_pid` is still our child because the exec kept the same pid.
    /// Fresh reader/writer handles are duped from the surviving fd, so resize
    /// and reaping run through raw libc rather than portable_pty.
    pub fn adopt(
        info: SessionInfo,
        master_fd: RawFd,
        child_pid: u32,
        rows: u16,
        cols: u16,
        vt_bytes: &[u8],
    ) -> Result<Arc<Self>, SessionError> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        // SAFETY: master_fd is a live PTY master handed over by the predecessor
        // conductor with CLOEXEC cleared; this process now owns it.
        let owned = unsafe { OwnedFd::from_raw_fd(master_fd) };
        let writer_fd = owned.try_clone()?;
        let reader_fd = owned.try_clone()?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(File::from(writer_fd))));
        let replies = Arc::clone(&writer);
        let mut terminal = Terminal::new(cols, rows, DEFAULT_SCROLLBACK_LINES, move |bytes| {
            if let Ok(mut writer) = replies.lock() {
                let _ = writer.write_all(bytes);
                let _ = writer.flush();
            }
        })?;
        if !vt_bytes.is_empty() {
            terminal.feed(vt_bytes);
        }
        let session = Arc::new(Self {
            id: info.id,
            argv: info.argv,
            cwd: info.cwd,
            writer,
            master: Mutex::new(Master::Raw(owned)),
            reaper: Mutex::new(Reaper::Pid(child_pid as libc::pid_t)),
            killer: Mutex::new(None),
            process_id: Some(child_pid),
            terminal: Arc::new(Mutex::new(terminal)),
            metadata: RwLock::new(Metadata {
                name: info.name,
                status: info.status,
                last_message: info.last_message,
                harness_session_id: info.harness_session_id,
                transcript_path: info.transcript_path,
                status_since_unix_ms: info.status_since_unix_ms,
            }),
            exited: AtomicBool::new(false),
            exit_clean: AtomicBool::new(false),
            sync_since_unix_ms: AtomicU64::new(0),
            generation: AtomicU64::new(1),
            working_seq: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
            pending_prefill: Mutex::new(None),
        });
        Self::start_reader(&session, Box::new(File::from(reader_fd)));
        Self::start_waiter(&session);
        Ok(session)
    }

    fn start_reader(session: &Arc<Self>, mut reader: Box<dyn Read + Send>) {
        let weak = Arc::downgrade(session);
        thread::spawn(move || {
            let mut bytes = vec![0u8; 32 * 1024];
            loop {
                let count = match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let Some(session) = weak.upgrade() else {
                    break;
                };
                let synchronized = if let Ok(mut terminal) = session.terminal.lock() {
                    terminal.feed(&bytes[..count]);
                    terminal.synchronized_output_active().unwrap_or(false)
                } else {
                    false
                };
                if synchronized {
                    let _ = session.sync_since_unix_ms.compare_exchange(
                        0,
                        unix_ms(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                } else {
                    session.sync_since_unix_ms.store(0, Ordering::Release);
                }
                session.mark_dirty();
            }
        });
    }

    fn start_waiter(session: &Arc<Self>) {
        let weak = Arc::downgrade(session);
        thread::spawn(move || {
            let Some(session) = weak.upgrade() else {
                return;
            };
            // A zero exit code with no terminating signal is a deliberate quit
            // (Claude/Codex `/exit`, a normal shutdown). Anything else — a
            // crash, a signal, or a lost handle — is treated as unclean so the
            // daemon leaves the session visible instead of auto-closing it.
            let clean = session.wait_for_child_exit();
            // Publish `exit_clean` before `exited`: any reader that observes
            // `exited() == true` with an Acquire load then also sees the flag.
            session.exit_clean.store(clean, Ordering::Release);
            session.exited.store(true, Ordering::Release);
            session.set_status(Status::Ended, String::new());
            session.mark_dirty();
        });
    }

    /// Blocks until the child exits and reports whether the exit was clean
    /// (status 0, no signal). Spawned children reap through portable_pty; an
    /// adopted child is reaped with `waitpid` on its pid. The reaper lock is held
    /// across the blocking wait, but nothing else contends for it, so resize
    /// (master) and kill (killer) stay responsive.
    fn wait_for_child_exit(&self) -> bool {
        let mut reaper = match self.reaper.lock() {
            Ok(reaper) => reaper,
            Err(_) => return false,
        };
        match &mut *reaper {
            Reaper::Child(child) => child.wait().map(|status| status.success()).unwrap_or(false),
            Reaper::Pid(pid) => waitpid_clean(*pid),
        }
    }

    /// Feed the host terminal's default colors into an already-running
    /// session's terminal so future agent color queries are answered with
    /// them. Best-effort; an already-started agent will not re-query, so this
    /// mainly benefits sessions that outlive the color detection.
    pub fn apply_host_theme(&self, theme: &crate::terminal_theme::TerminalTheme) {
        if theme.is_empty() {
            return;
        }
        if let Ok(mut terminal) = self.terminal.lock() {
            terminal.feed(&theme.set_sequences());
        }
    }

    pub fn subscribe(&self) -> mpsc::Receiver<u64> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(sender);
        }
        receiver
    }

    fn mark_dirty(&self) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(generation).is_ok());
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn synchronized_output_active(&self) -> bool {
        let since = self.sync_since_unix_ms.load(Ordering::Acquire);
        since != 0 && unix_ms().saturating_sub(since) <= SYNC_WATCHDOG_MS
    }

    pub fn write_input(&self, bytes: &[u8]) -> Result<(), SessionError> {
        let mut writer = self.writer.lock().map_err(|_| SessionError::Poisoned)?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn paste(&self, bytes: &[u8]) -> Result<(), SessionError> {
        let encoded = self
            .terminal
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .encode_paste(bytes)?;
        self.write_input(&encoded)
    }

    pub fn queue_prefill(session: &Arc<Self>, text: String) {
        if text.is_empty() {
            return;
        }
        if let Ok(mut pending) = session.pending_prefill.lock() {
            *pending = Some(text.into_bytes());
        }
        let weak = Arc::downgrade(session);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            if let Some(session) = weak.upgrade() {
                let _ = session.flush_prefill();
            }
        });
    }

    pub fn flush_prefill(&self) -> Result<(), SessionError> {
        let pending = self
            .pending_prefill
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .take();
        if let Some(bytes) = pending {
            self.paste(&bytes)?;
        }
        Ok(())
    }

    pub fn report_focus(&self, focused: bool) -> Result<(), SessionError> {
        let enabled = self
            .terminal
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .mode_enabled(1004)?;
        if enabled {
            self.write_input(if focused { b"\x1b[I" } else { b"\x1b[O" })?;
        }
        Ok(())
    }

    pub fn mouse(
        &self,
        action: MouseAction,
        button: Option<u8>,
        modifiers: u16,
        x: u16,
        y: u16,
        any_button_pressed: bool,
    ) -> Result<(), SessionError> {
        let bytes = self
            .terminal
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .encode_mouse(action, button, modifiers, x, y, any_button_pressed)?;
        if !bytes.is_empty() {
            self.write_input(&bytes)?;
        }
        Ok(())
    }

    pub fn extract_text(&self, start: (u16, u32), end: (u16, u32)) -> Result<String, SessionError> {
        Ok(self
            .terminal
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .read_text_screen(start, end)?)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), SessionError> {
        match &*self.master.lock().map_err(|_| SessionError::Poisoned)? {
            Master::Portable(master) => master.resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })?,
            Master::Raw(fd) => set_winsize(fd.as_raw_fd(), rows.max(1), cols.max(1))?,
        }
        self.terminal
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .resize(cols.max(1), rows.max(1))?;
        self.mark_dirty();
        Ok(())
    }

    pub fn render_at(&self, offset: usize) -> Result<TerminalFrame, SessionError> {
        let mut terminal = self.terminal.lock().map_err(|_| SessionError::Poisoned)?;
        terminal.set_scroll_offset_from_bottom(offset);
        let rendered = terminal.render()?;
        terminal.scroll_to_bottom();
        Ok(frame_from_rendered(rendered))
    }

    pub fn render_at_row(&self, row: usize) -> Result<TerminalFrame, SessionError> {
        let mut terminal = self.terminal.lock().map_err(|_| SessionError::Poisoned)?;
        terminal.set_scroll_row(row);
        let rendered = terminal.render()?;
        terminal.scroll_to_bottom();
        Ok(frame_from_rendered(rendered))
    }

    pub fn kill(&self) -> Result<(), SessionError> {
        // The waiter has already reaped a session that exited on its own, so
        // its pid is free and may have been recycled. Never signal in that
        // case; the process is gone and `-pid` could hit an unrelated group.
        if self.exited() {
            return Ok(());
        }
        #[cfg(unix)]
        if let Some(process_id) = self.process_id {
            // forkpty makes the child a process-group/session leader. Signal
            // the negative pid so agent tool subprocesses cannot outlive the
            // Codebridge session. Fall back to the portable killer if the
            // platform did not establish that group as expected.
            if unsafe { libc::kill(-(process_id as i32), libc::SIGHUP) } == 0 {
                return Ok(());
            }
        }
        // Fall back to portable_pty's killer for spawned sessions whose group
        // signal failed. Adopted sessions have no portable killer and rely
        // solely on the pid path above.
        if let Some(killer) = self
            .killer
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .as_mut()
        {
            killer.kill()?;
        }
        Ok(())
    }

    pub fn set_name(&self, name: String) {
        if let Ok(mut metadata) = self.metadata.write() {
            metadata.name = name;
        }
        self.mark_dirty();
    }

    pub fn set_status(&self, status: Status, message: String) {
        // Every observation that keeps or puts the session in `Working` counts as
        // turn activity. The interrupt confirmation captures this counter as a
        // baseline and only clears a stuck spinner if it has not advanced, so a
        // fresh turn started by a queued steering message (which fires its own
        // `Working` hook) is never mistaken for the interrupted turn.
        if status == Status::Working {
            self.working_seq.fetch_add(1, Ordering::AcqRel);
        }
        if let Ok(mut metadata) = self.metadata.write() {
            metadata.status = status;
            metadata.last_message = message;
            metadata.status_since_unix_ms = unix_ms();
        }
        self.mark_dirty();
    }

    /// Monotonic count of `Working` observations for this session. Used by the
    /// interrupt confirmation to distinguish a still-stuck interrupted turn from
    /// a fresh turn started right after the interrupt (queued-message steering).
    pub fn working_seq(&self) -> u64 {
        self.working_seq.load(Ordering::Acquire)
    }

    pub fn set_harness_session_id(&self, id: String) {
        if let Ok(mut metadata) = self.metadata.write() {
            metadata.harness_session_id = id;
        }
        self.mark_dirty();
    }

    pub fn set_transcript_path(&self, path: String) {
        if let Ok(mut metadata) = self.metadata.write() {
            metadata.transcript_path = path;
        }
    }

    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Whether the child exited deliberately (status 0, no signal). Only
    /// meaningful once `exited()` is true.
    pub fn exit_clean(&self) -> bool {
        self.exit_clean.load(Ordering::Acquire)
    }

    /// The raw PTY master fd. The conductor clears CLOEXEC on it and hands it to
    /// its `execve` successor so the session survives a hot-upgrade. Valid only
    /// while this session is alive.
    pub fn master_raw_fd(&self) -> Option<RawFd> {
        match &*self.master.lock().ok()? {
            Master::Portable(master) => master.as_raw_fd(),
            Master::Raw(fd) => Some(fd.as_raw_fd()),
        }
    }

    /// The child's pid, preserved verbatim across a hot-upgrade so the successor
    /// can keep reaping it.
    pub fn child_pid(&self) -> Option<u32> {
        self.process_id
    }

    /// Serializes the terminal state as replayable VT bytes so it can be carried
    /// across a conductor hot-upgrade and fed into a fresh terminal.
    pub fn vt_snapshot(&self) -> Option<Vec<u8>> {
        self.terminal.lock().ok()?.snapshot_vt().ok()
    }

    /// Current PTY window size as `(rows, cols)`, read from the master fd. The
    /// terminal and PTY are kept in lockstep by `resize`, so this is also the
    /// terminal's size — the size a VT snapshot must be replayed into.
    pub fn winsize(&self) -> Option<(u16, u16)> {
        get_winsize(self.master_raw_fd()?)
    }

    pub fn snapshot(&self) -> SessionInfo {
        let metadata = self.metadata.read().ok();
        SessionInfo {
            id: self.id.clone(),
            name: metadata
                .as_ref()
                .map(|value| value.name.clone())
                .unwrap_or_default(),
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            status: metadata
                .as_ref()
                .map(|value| value.status.clone())
                .unwrap_or(Status::Ended),
            last_message: metadata
                .as_ref()
                .map(|value| value.last_message.clone())
                .unwrap_or_default(),
            harness_session_id: metadata
                .as_ref()
                .map(|value| value.harness_session_id.clone())
                .unwrap_or_default(),
            exited: self.exited(),
            status_since_unix_ms: metadata
                .as_ref()
                .map(|value| value.status_since_unix_ms)
                .unwrap_or_default(),
            transcript_path: metadata
                .as_ref()
                .map(|value| value.transcript_path.clone())
                .unwrap_or_default(),
        }
    }

    pub fn wait_for_change(receiver: &mpsc::Receiver<u64>, timeout: Duration) -> bool {
        receiver.recv_timeout(timeout).is_ok()
    }
}

fn frame_from_rendered(rendered: RenderedTerminal) -> TerminalFrame {
    let cursor = rendered.cursor;
    let cells = rendered
        .buffer
        .content
        .iter()
        .map(|cell| Cell {
            symbol: cell.symbol().to_owned(),
            fg: color(cell.fg),
            bg: color(cell.bg),
            modifiers: modifiers(cell.modifier),
        })
        .collect();
    TerminalFrame {
        rows: rendered.buffer.area.height,
        cols: rendered.buffer.area.width,
        cells,
        cursor_x: cursor.map(|value| value.x).unwrap_or_default(),
        cursor_y: cursor.map(|value| value.y).unwrap_or_default(),
        cursor_visible: cursor.is_some_and(|value| value.visible),
        mouse_reporting: rendered.mouse_reporting,
        offset: rendered.scroll.offset_from_bottom,
        max_offset: rendered.scroll.max_offset_from_bottom,
    }
}

fn color(color: Color) -> u32 {
    match color {
        Color::Reset => 0,
        Color::Black => 1,
        Color::Red => 2,
        Color::Green => 3,
        Color::Yellow => 4,
        Color::Blue => 5,
        Color::Magenta => 6,
        Color::Cyan => 7,
        Color::Gray => 8,
        Color::DarkGray => 9,
        Color::LightRed => 10,
        Color::LightGreen => 11,
        Color::LightYellow => 12,
        Color::LightBlue => 13,
        Color::LightMagenta => 14,
        Color::LightCyan => 15,
        Color::White => 16,
        Color::Indexed(index) => 0x1000_0000 | u32::from(index),
        Color::Rgb(red, green, blue) => {
            0x2000_0000 | (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
        }
    }
}

fn modifiers(modifier: Modifier) -> u16 {
    modifier.bits()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Sets a PTY master's window size directly, for adopted sessions where there
/// is no portable_pty handle to resize through.
fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> std::io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is a live PTY master and `ws` is valid for the call.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Reads a PTY master's window size as `(rows, cols)`.
fn get_winsize(fd: RawFd) -> Option<(u16, u16)> {
    // SAFETY: zeroed winsize is a valid initial value; fd is a live PTY master.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } == 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// Reaps an adopted child by pid and reports whether it exited cleanly (code 0,
/// no terminating signal). `ECHILD` (already reaped, or not our child) counts as
/// unclean so a mystery disappearance stays visible rather than auto-closing.
fn waitpid_clean(pid: libc::pid_t) -> bool {
    let mut status: libc::c_int = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        }
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_frame_preserves_cells_and_scrollback_metrics() {
        let mut terminal = Terminal::new(6, 2, 100, |_| {}).expect("terminal");
        terminal.feed(b"one\r\ntwo\r\nthree");
        terminal.scroll_up(1);

        let frame = frame_from_rendered(terminal.render().expect("render"));

        assert_eq!(frame.rows, 2);
        assert_eq!(frame.cols, 6);
        assert_eq!(frame.offset, 1);
        assert!(frame.max_offset >= 1);
        assert!(!frame.cursor_visible);
        assert_eq!(frame.cells[0].symbol, "o");
    }

    #[cfg(unix)]
    #[test]
    fn adopt_reconstructs_a_live_session_from_a_surviving_fd() {
        // Stand in for a hot-upgrade: build a PTY + child and a terminal with
        // known content, snapshot the terminal to VT, then adopt a fresh session
        // from a dup of the master fd + the child pid and confirm the screen and
        // live I/O both survived.
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 4,
                cols: 40,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let child = pair
            .slave
            .spawn_command(CommandBuilder::new("/bin/cat"))
            .expect("spawn cat");
        let pid = child.process_id().expect("pid");
        drop(pair.slave);
        std::mem::forget(child); // the adopted session's waiter reaps the pid.

        let mut terminal =
            Terminal::new(40, 4, DEFAULT_SCROLLBACK_LINES, |_| {}).expect("terminal");
        terminal.feed(b"hello world");
        let vt = terminal.snapshot_vt().expect("snapshot");

        // Dup so the adopted session owns an fd independent of `pair.master`.
        let dup = unsafe { libc::dup(pair.master.as_raw_fd().expect("raw fd")) };
        assert!(dup >= 0, "dup failed");

        let info = SessionInfo {
            id: "adopted".to_owned(),
            name: "restored".to_owned(),
            argv: vec!["/bin/cat".to_owned()],
            cwd: "/tmp".to_owned(),
            status: Status::Working,
            last_message: String::new(),
            harness_session_id: "sess-keep".to_owned(),
            exited: false,
            status_since_unix_ms: 0,
            transcript_path: String::new(),
        };
        let session = Session::adopt(info, dup, pid, 4, 40, &vt).expect("adopt");
        drop(pair.master); // only the adopted session's dup remains.

        // The replayed VT put "hello world" back on the screen, and metadata
        // rode across the adopt.
        let screen = session.extract_text((0, 0), (39, 3)).expect("extract");
        assert!(
            screen.contains("hello world"),
            "restored screen: {screen:?}"
        );
        assert_eq!(session.snapshot().harness_session_id, "sess-keep");
        assert_eq!(session.snapshot().name, "restored");

        // Live I/O still works: cat echoes what we write through the adopted fd.
        session.write_input(b"ping\n").expect("write");
        let deadline = SystemTime::now() + Duration::from_secs(2);
        loop {
            let screen = session.extract_text((0, 0), (39, 3)).expect("extract");
            if screen.contains("ping") {
                break;
            }
            assert!(
                SystemTime::now() < deadline,
                "adopted session never echoed input: {screen:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }

        session.kill().expect("kill");
    }

    #[cfg(unix)]
    #[test]
    fn kill_terminates_the_child_process_group() {
        let session = Session::spawn(
            "group-test".to_owned(),
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "sleep 30 & echo $!; wait".to_owned(),
            ],
            "/tmp".to_owned(),
            4,
            40,
            crate::terminal_theme::TerminalTheme::default(),
        )
        .expect("spawn");
        let deadline = SystemTime::now() + Duration::from_secs(2);
        let child_pid = loop {
            let frame = session.render_at(0).expect("frame");
            let text = frame
                .cells
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>();
            if let Some(pid) = text
                .split_whitespace()
                .find_map(|word| word.parse::<i32>().ok())
            {
                break pid;
            }
            assert!(SystemTime::now() < deadline, "child pid was not printed");
            thread::sleep(Duration::from_millis(20));
        };
        session.kill().expect("group kill");
        let deadline = SystemTime::now() + Duration::from_secs(2);
        while unsafe { libc::kill(child_pid, 0) } == 0 && SystemTime::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(
            unsafe { libc::kill(child_pid, 0) },
            0,
            "background child survived session kill"
        );
    }
}
