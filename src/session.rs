use std::io::{Read, Write};
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
    status_since_unix_ms: u64,
}

pub struct Session {
    id: String,
    argv: Vec<String>,
    cwd: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    process_id: Option<u32>,
    terminal: Arc<Mutex<Terminal>>,
    metadata: RwLock<Metadata>,
    exited: AtomicBool,
    sync_since_unix_ms: AtomicU64,
    generation: AtomicU64,
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
        let terminal = Terminal::new(
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
        let now = unix_ms();
        let session = Arc::new(Self {
            id,
            argv,
            cwd,
            writer,
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
            killer: Mutex::new(killer),
            process_id,
            terminal: Arc::new(Mutex::new(terminal)),
            metadata: RwLock::new(Metadata {
                name: String::new(),
                status: Status::Starting,
                last_message: String::new(),
                harness_session_id: String::new(),
                status_since_unix_ms: now,
            }),
            exited: AtomicBool::new(false),
            sync_since_unix_ms: AtomicU64::new(0),
            generation: AtomicU64::new(1),
            subscribers: Mutex::new(Vec::new()),
            pending_prefill: Mutex::new(None),
        });
        Self::start_reader(&session, reader);
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
            if let Ok(mut child) = session.child.lock() {
                let _ = child.wait();
            }
            session.exited.store(true, Ordering::Release);
            session.set_status(Status::Ended, String::new());
            session.mark_dirty();
        });
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
        self.master
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })?;
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
        self.killer
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .kill()?;
        Ok(())
    }

    pub fn set_name(&self, name: String) {
        if let Ok(mut metadata) = self.metadata.write() {
            metadata.name = name;
        }
        self.mark_dirty();
    }

    pub fn set_status(&self, status: Status, message: String) {
        if let Ok(mut metadata) = self.metadata.write() {
            metadata.status = status;
            metadata.last_message = message;
            metadata.status_since_unix_ms = unix_ms();
        }
        self.mark_dirty();
    }

    pub fn set_harness_session_id(&self, id: String) {
        if let Ok(mut metadata) = self.metadata.write() {
            metadata.harness_session_id = id;
        }
        self.mark_dirty();
    }

    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
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
