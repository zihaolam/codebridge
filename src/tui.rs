use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal};
use time::OffsetDateTime;

use crate::conductor::{conductor_socket_path, ConductorRequest};
use crate::config::Config;
use crate::daemon::socket_path;
use crate::protocol::{
    Request, Response, SessionInfo, Status, StreamDown, StreamUp, TerminalFrame,
};
use crate::sidebar::{scope_display_name, Row, Sidebar};
use crate::task::{Task, TaskStatus};
use crate::theme::{Palette, THEME_NAMES};
use crate::worktree::{self, Agent as AgentChoice, Worktree};

const SIDEBAR_WIDTH: u16 = 30;
const SPINNERS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

enum UiEvent {
    Snapshot(Vec<SessionInfo>, Vec<Task>),
    Frame(String, TerminalFrame),
    Gone(String, bool),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Screen,
}

struct Attach {
    id: String,
    writer: BufWriter<UnixStream>,
}

struct Toast {
    session_id: String,
    status: Status,
    text: String,
}

struct PendingNotification {
    toast: Toast,
    deadline: Instant,
}

struct Rename {
    id: String,
    input: String,
}

enum PickerStage {
    Worktree,
    Agent,
}

struct WorktreePicker {
    stage: PickerStage,
    worktrees: Vec<Worktree>,
    agents: Vec<AgentChoice>,
    worktree_cursor: usize,
    agent_cursor: usize,
    chosen: Option<PathBuf>,
}

struct ConfigMenu {
    cursor: usize,
    capture: bool,
    error: String,
    theme_cursor: Option<usize>,
    notification_cursor: Option<usize>,
    original_theme: Option<String>,
}

#[derive(Clone)]
struct Selection {
    session_id: String,
    anchor: (u32, u16),
    cursor: (u32, u16),
    dragging: bool,
}

impl Selection {
    fn ordered(&self) -> ((u32, u16), (u32, u16)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    fn contains(&self, row: u32, col: u16) -> bool {
        if !self.dragging {
            return false;
        }
        let ((start_row, start_col), (end_row, end_col)) = self.ordered();
        row >= start_row
            && row <= end_row
            && if start_row == end_row {
                col >= start_col && col <= end_col
            } else if row == start_row {
                col >= start_col
            } else if row == end_row {
                col <= end_col
            } else {
                true
            }
    }
}

enum TaskStage {
    List,
    New {
        title: String,
        desc: String,
        title_active: bool,
    },
    Detail {
        id: String,
        title: String,
        desc: String,
        title_active: bool,
    },
    Agent {
        id: String,
        cursor: usize,
    },
    Runs {
        id: String,
        cursor: usize,
    },
}

struct TaskModal {
    stage: TaskStage,
    cursor: usize,
}

struct HistoryModal {
    cursor: usize,
}

/// One session surfaced by the historical picker, flattened from a task run.
/// The row is labelled by the agent-summarised `title` when present, falling
/// back to `first_message`. A `live` run is currently running — selecting it
/// jumps to that session by `cb_session_id`; otherwise the run is paused and
/// resumable via its agent-native identity.
struct HistoryEntry {
    task_id: String,
    run_id: String,
    agent: String,
    title: String,
    first_message: String,
    auto: bool,
    live: bool,
    cb_session_id: String,
    updated_at: OffsetDateTime,
}

struct Model {
    sidebar: Sidebar,
    launch_cwd: PathBuf,
    focus: Focus,
    prefix: bool,
    help: bool,
    scroll_mode: bool,
    frame: Option<TerminalFrame>,
    attach: Option<Attach>,
    error: String,
    previous_status: HashMap<String, Status>,
    pending_notifications: HashMap<String, PendingNotification>,
    outer_focused: bool,
    worktree_cwds: std::collections::HashSet<String>,
    hooks_ok: bool,
    toasts: Vec<Toast>,
    rename: Option<Rename>,
    worktree_picker: Option<WorktreePicker>,
    config: Config,
    palette: Palette,
    config_menu: Option<ConfigMenu>,
    selection: Option<Selection>,
    /// Time and cell of the last left-button press, for double-click detection.
    last_click: Option<(Instant, u16, u16)>,
    /// A double-click selected a word; the following button-up copies without
    /// letting the drag handler collapse the word back to the clicked cell.
    word_selecting: bool,
    tasks: Vec<Task>,
    task_modal: Option<TaskModal>,
    history_modal: Option<HistoryModal>,
    pending_jump: Option<String>,
    spin: usize,
    pane: Rect,
    screen: Rect,
}

impl Model {
    fn selected(&self) -> Option<&SessionInfo> {
        self.sidebar.selected_session()
    }

    fn selected_id(&self) -> Option<String> {
        self.selected().map(|session| session.id.clone())
    }

    fn attached_session(&self) -> Option<&SessionInfo> {
        self.attach
            .as_ref()
            .and_then(|attach| self.sidebar.session_by_id(&attach.id))
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    ensure_daemon()?;
    let (sender, receiver) = mpsc::channel();
    start_watch(sender.clone())?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Kitty keyboard state is screen-local, so enter the alternate screen
    // before enabling it. The guard reverses that order during restoration.
    let _guard = TerminalGuard;
    execute!(
        stdout,
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        EnableBracketedPaste,
        EnableFocusChange,
        EnableMouseCapture
    )?;
    // Detect the host terminal's default colors (now that raw mode is on) and
    // push them to the daemon, so each session answers an agent's OSC 10/11
    // color query with the real host colors — fixing e.g. Codex's invisible
    // input-box background. A no-op if the terminal does not reply in time.
    let host_theme = detect_host_theme(&mut stdout);
    if !host_theme.is_empty() {
        let _ = request(Request {
            kind: "set_host_theme".to_owned(),
            payload: serde_json::to_value(host_theme).unwrap_or_default(),
            ..Request::default()
        });
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let launch_cwd = std::env::current_dir()?;
    let config = Config::load();
    let palette = Palette::resolve(&config.theme);
    let mut model = Model {
        sidebar: Sidebar::new(&launch_cwd),
        launch_cwd,
        focus: Focus::Sidebar,
        prefix: false,
        help: false,
        scroll_mode: false,
        frame: None,
        attach: None,
        error: String::new(),
        previous_status: HashMap::new(),
        pending_notifications: HashMap::new(),
        outer_focused: true,
        worktree_cwds: std::collections::HashSet::new(),
        hooks_ok: matches!(
            crate::integration::status(crate::integration::Agent::Claude),
            Ok(crate::integration::Status::Current { .. })
        ),
        toasts: Vec::new(),
        rename: None,
        worktree_picker: None,
        config,
        palette,
        config_menu: None,
        selection: None,
        last_click: None,
        word_selecting: false,
        tasks: Vec::new(),
        history_modal: None,
        task_modal: None,
        pending_jump: None,
        spin: 0,
        pane: Rect::default(),
        screen: Rect::default(),
    };
    let mut last_spin = Instant::now();

    loop {
        drain_events(&mut model, &receiver);
        deliver_due_notifications(&mut model);
        let previous_pane = model.pane;
        let size = terminal.size()?;
        compute_view(&mut model, Rect::new(0, 0, size.width, size.height));
        sync_attach(&mut model, sender.clone())?;
        // Focusing a session's screen clears any toast still standing for it,
        // however focus arrived — keyboard, a click into the pane, or a jump.
        // Run it here (after `sync_attach` reconciles `attach` with the current
        // selection) rather than only after a keypress, so it always targets the
        // session actually on screen and never a stale attachment.
        if model.focus == Focus::Screen {
            dismiss_attached_toast(&mut model);
        }
        terminal.draw(|frame| render(&model, frame))?;
        if model.pane != previous_pane {
            resize_attached(&mut model)?;
        }

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    let quit = handle_key(&mut model, key)?;
                    if quit {
                        break;
                    }
                }
                Event::Resize(_, _) => resize_attached(&mut model)?,
                Event::Paste(text) => handle_paste(&mut model, &text)?,
                Event::FocusGained => {
                    model.outer_focused = true;
                    send_focus(&mut model, true)?;
                }
                Event::FocusLost => {
                    model.outer_focused = false;
                    send_focus(&mut model, false)?;
                }
                Event::Mouse(mouse) => handle_mouse(&mut model, mouse)?,
            }
        }
        if last_spin.elapsed() >= Duration::from_millis(100) {
            model.spin = model.spin.wrapping_add(1);
            last_spin = Instant::now();
        }
    }
    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

fn ensure_daemon() -> io::Result<()> {
    if UnixStream::connect(socket_path()).is_ok() {
        return Ok(());
    }
    let executable = std::env::current_exe()?;
    std::process::Command::new(executable)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    for _ in 0..50 {
        if UnixStream::connect(socket_path()).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "daemon did not become ready",
    ))
}

/// Opens a watch stream to the broker: connects to `daemon.sock` and issues the
/// streaming `watch` request. Shared by the initial connection and every
/// reconnect after the broker cycles.
fn watch_connect() -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path())?;
    write_json(
        &mut stream,
        &Request {
            kind: "watch".to_owned(),
            ..Request::default()
        },
    )?;
    Ok(stream)
}

fn start_watch(sender: Sender<UiEvent>) -> io::Result<()> {
    // Establish the first connection synchronously so startup fails fast if the
    // broker never comes up; reconnects thereafter live inside the thread.
    let initial = watch_connect()?;
    thread::spawn(move || {
        let mut stream = initial;
        loop {
            // Drain snapshots until the broker closes the stream. `cb restart`
            // cycles the broker, EOF-ing this control-plane connection even
            // though the conductor — and the live pane attached straight to it —
            // keep running, so the sidebar would otherwise freeze forever.
            for line in BufReader::new(&stream).lines() {
                match line
                    .ok()
                    .and_then(|line| serde_json::from_str::<Response>(&line).ok())
                {
                    Some(response) if response.ok => {
                        if sender
                            .send(UiEvent::Snapshot(response.sessions, response.tasks))
                            .is_err()
                        {
                            return; // Receiver dropped: the TUI is shutting down.
                        }
                    }
                    Some(response) => {
                        let _ = sender.send(UiEvent::Error(response.error));
                    }
                    None => break,
                }
            }
            // The broker went away (a `cb restart`, or a crash). Reconnect to its
            // replacement — resurrecting it if needed — with a capped backoff so a
            // slow restart never hot-loops. The bind guard in `Daemon::run` makes
            // racing `cb restart`'s own spawn safe: the loser gets `AddrInUse` and
            // exits. The fresh `watch` snapshot is a full state dump, so the
            // sidebar simply repopulates.
            let mut backoff = Duration::from_millis(50);
            stream = loop {
                thread::sleep(backoff);
                if ensure_daemon().is_ok() {
                    if let Ok(fresh) = watch_connect() {
                        break fresh;
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(2));
            };
        }
    });
    Ok(())
}

fn start_attach(id: String, pane: Rect, sender: Sender<UiEvent>) -> io::Result<Attach> {
    // The data plane goes straight to the conductor, not through the broker, so
    // the live pane keeps streaming across a broker restart (`cb restart`). The
    // broker is only used for the control plane (sidebar/tasks/watch).
    let mut stream = UnixStream::connect(conductor_socket_path())?;
    write_json(
        &mut stream,
        &ConductorRequest::Attach {
            id: id.clone(),
            rows: pane.height.max(1),
            cols: pane.width.max(1),
        },
    )?;
    let reader = stream.try_clone()?;
    let event_id = id.clone();
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else {
                break;
            };
            match serde_json::from_str::<StreamDown>(&line) {
                Ok(StreamDown::Frame { frame }) => {
                    if sender
                        .send(UiEvent::Frame(event_id.clone(), frame))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(StreamDown::Gone { clean }) => {
                    let _ = sender.send(UiEvent::Gone(event_id.clone(), clean));
                    break;
                }
                Ok(StreamDown::Error { message }) => {
                    let _ = sender.send(UiEvent::Error(message));
                }
                Err(_) => {}
            }
        }
    });
    Ok(Attach {
        id,
        writer: BufWriter::new(stream),
    })
}

fn sync_attach(model: &mut Model, sender: Sender<UiEvent>) -> io::Result<()> {
    let selected = model.selected_id();
    let attached = model.attach.as_ref().map(|attach| attach.id.clone());
    if selected.is_none()
        && attached
            .as_ref()
            .is_some_and(|id| model.sidebar.session_by_id(id).is_some())
    {
        return Ok(());
    }
    if selected == attached {
        return Ok(());
    }
    if let Some(mut attach) = model.attach.take() {
        let _ = write_json(
            &mut attach.writer,
            &StreamUp {
                kind: "detach".to_owned(),
                ..StreamUp::default()
            },
        );
    }
    model.frame = None;
    model.scroll_mode = false;
    model.selection = None;
    if let Some(id) = selected {
        model.attach = Some(start_attach(id, model.pane, sender)?);
    }
    Ok(())
}

fn drain_events(model: &mut Model, receiver: &Receiver<UiEvent>) {
    while let Ok(event) = receiver.try_recv() {
        match event {
            UiEvent::Snapshot(sessions, tasks) => {
                // Update tasks before detecting transitions so toast labels join
                // against this snapshot's freshly-resolved agent titles.
                model.tasks = tasks;
                detect_transitions(model, &sessions);
                model.worktree_cwds = sessions
                    .iter()
                    .filter(|session| is_linked_worktree(Path::new(&session.cwd)))
                    .map(|session| session.cwd.clone())
                    .collect();
                model.sidebar.update(sessions);
                if let Some(id) = model.pending_jump.clone() {
                    if model.sidebar.select_session(&id) {
                        model.pending_jump = None;
                        model.focus = Focus::Screen;
                    }
                }
                expire_toasts(model);
            }
            UiEvent::Frame(id, frame)
                if model.attach.as_ref().is_some_and(|attach| attach.id == id) =>
            {
                model.frame = Some(frame);
            }
            UiEvent::Gone(id, clean)
                if model.attach.as_ref().is_some_and(|attach| attach.id == id) =>
            {
                model.attach = None;
                // A deliberate `/exit` closes the session outright: advance to a
                // neighbour like `kill` does. A crash leaves the ended row in
                // place so it stays visible.
                if clean && !model.sidebar.select_previous_session(&id) {
                    model.focus = Focus::Sidebar;
                }
            }
            UiEvent::Error(error) => model.error = error,
            _ => {}
        }
    }
}

fn detect_transitions(model: &mut Model, sessions: &[SessionInfo]) {
    model.pending_notifications.retain(|id, pending| {
        sessions
            .iter()
            .find(|session| session.id == *id)
            .is_some_and(|session| session.status == pending.toast.status)
    });
    // Notify only for the launch workspace unless the accordion (global view) is
    // on. Both are stable across snapshots, so reading them before the sidebar
    // update below is safe. `transition_toasts` still records every session's new
    // status unconditionally, so an out-of-scope transition we drop here cannot
    // misfire once the accordion is later toggled on.
    let accordion = model.sidebar.accordion();
    let current_scope = model.sidebar.current_scope().to_owned();
    let transitions = transition_toasts(&mut model.previous_status, sessions, &model.tasks);
    for toast in transitions {
        let allowed = match toast.status {
            Status::NeedsApproval => model.config.notifications.notify_approval,
            Status::WaitingUser => model.config.notifications.notify_done,
            _ => false,
        };
        if !allowed {
            continue;
        }
        let cwd = sessions
            .iter()
            .find(|session| session.id == toast.session_id)
            .map(|session| session.cwd.as_str());
        if !toast_in_scope(accordion, &current_scope, cwd) {
            continue;
        }
        let delay = Duration::from_secs(model.config.notifications.bounded_delay_seconds());
        if delay.is_zero() {
            deliver_notification(model, toast);
        } else {
            model.pending_notifications.insert(
                toast.session_id.clone(),
                PendingNotification {
                    toast,
                    deadline: Instant::now() + delay,
                },
            );
        }
    }
}

fn deliver_due_notifications(model: &mut Model) {
    let now = Instant::now();
    let due = model
        .pending_notifications
        .iter()
        .filter_map(|(id, pending)| (pending.deadline <= now).then_some(id.clone()))
        .collect::<Vec<_>>();
    for id in due {
        let Some(pending) = model.pending_notifications.remove(&id) else {
            continue;
        };
        let still_current = model
            .sidebar
            .session_by_id(&id)
            .is_some_and(|session| session.status == pending.toast.status);
        if still_current {
            deliver_notification(model, pending.toast);
        }
    }
}

fn deliver_notification(model: &mut Model, toast: Toast) {
    let delivery = model.config.notifications.delivery;
    let active = model
        .attach
        .as_ref()
        .is_some_and(|attach| attach.id == toast.session_id);
    let (show_in_app, show_external) = notification_channels(
        delivery,
        model.config.notifications.suppress_focused,
        active,
        model.outer_focused,
    );
    if show_in_app {
        model.toasts.push(Toast {
            session_id: toast.session_id.clone(),
            status: toast.status.clone(),
            text: toast.text.clone(),
        });
        if model.toasts.len() > 5 {
            model.toasts.drain(..model.toasts.len() - 5);
        }
    }
    if show_external {
        let (title, body) = notification_text(model, &toast);
        crate::notify::send(delivery, &title, &body);
    }
}

/// Whether a session's transition should notify given the current view. In the
/// default flat view only the launch workspace notifies; the accordion (global
/// view, `prefix a`) opts into cross-workspace notifications. A session with no
/// known cwd (already gone from the snapshot) never notifies when scoped.
fn toast_in_scope(accordion: bool, current_scope: &str, session_cwd: Option<&str>) -> bool {
    accordion || session_cwd.is_some_and(|cwd| crate::sidebar::scope_key(cwd) == current_scope)
}

fn notification_channels(
    delivery: crate::notify::Delivery,
    suppress_focused: bool,
    active: bool,
    outer_focused: bool,
) -> (bool, bool) {
    let in_app = delivery.shows_in_app() && !active;
    let external = matches!(
        delivery,
        crate::notify::Delivery::All
            | crate::notify::Delivery::Terminal
            | crate::notify::Delivery::System
    ) && !(suppress_focused && active && outer_focused);
    (in_app, external)
}

fn notification_text(model: &Model, toast: &Toast) -> (String, String) {
    let Some(session) = model.sidebar.session_by_id(&toast.session_id) else {
        return ("Codebridge".to_owned(), toast.text.clone());
    };
    let agent = session
        .argv
        .first()
        .and_then(|argument| Path::new(argument).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "agent".to_owned());
    let event = match toast.status {
        Status::NeedsApproval => "needs attention",
        Status::WaitingUser => "finished",
        _ => "updated",
    };
    (
        format!("{agent} {event}"),
        format!(
            "{} · {}",
            session_label(session, &model.tasks),
            scope_display_name(&session.cwd)
        ),
    )
}

fn transition_toasts(
    previous: &mut HashMap<String, Status>,
    sessions: &[SessionInfo],
    tasks: &[Task],
) -> Vec<Toast> {
    let mut toasts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for session in sessions {
        seen.insert(session.id.clone());
        let old = previous.insert(session.id.clone(), session.status.clone());
        if old.as_ref().is_some_and(|status| status != &session.status) {
            match session.status {
                Status::NeedsApproval => {
                    let detail = if session.last_message.is_empty() {
                        "needs your approval".to_owned()
                    } else {
                        session.last_message.clone()
                    };
                    toasts.push(Toast {
                        session_id: session.id.clone(),
                        status: Status::NeedsApproval,
                        text: format!("⚑ {} — {detail}", session_label(session, tasks)),
                    });
                }
                Status::WaitingUser => toasts.push(Toast {
                    session_id: session.id.clone(),
                    status: Status::WaitingUser,
                    text: format!("● {} — turn completed", session_label(session, tasks)),
                }),
                _ => {}
            }
        }
    }
    previous.retain(|id, _| seen.contains(id));
    toasts
}

fn expire_toasts(model: &mut Model) {
    model.toasts.retain(|toast| {
        model
            .sidebar
            .session_by_id(&toast.session_id)
            .is_some_and(|session| session.status == toast.status)
    });
}

fn dismiss_attached_toast(model: &mut Model) {
    if let Some(id) = model.attach.as_ref().map(|attach| attach.id.clone()) {
        model.toasts.retain(|toast| toast.session_id != id);
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn handle_key(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    if model.task_modal.is_some() {
        return handle_task_modal(model, key);
    }
    if model.history_modal.is_some() {
        return handle_history_modal(model, key);
    }
    if model.config_menu.is_some() {
        return handle_config_menu(model, key);
    }
    if model.worktree_picker.is_some() {
        return handle_worktree_picker(model, key);
    }
    if model.rename.is_some() {
        return handle_rename(model, key);
    }
    if model.help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
            model.help = false;
            return Ok(false);
        }
        model.help = false;
        return handle_prefix(model, key);
    }
    if key_name(key) == model.config.effective_prefix() {
        model.prefix = true;
        return Ok(false);
    }
    if model.prefix {
        model.prefix = false;
        return handle_prefix(model, key);
    }
    if model.scroll_mode {
        return handle_scroll(model, key);
    }
    if model.focus == Focus::Sidebar
        && key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return Ok(true);
    }
    match model.focus {
        Focus::Sidebar => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                model.sidebar.move_up();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                model.sidebar.move_down();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if !model.sidebar.toggle_current_scope() && model.attach.is_some() {
                    model.focus = Focus::Screen;
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if model.sidebar.step_right() && model.attach.is_some() {
                    model.focus = Focus::Screen;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                model.sidebar.step_left();
            }
            _ => {}
        },
        Focus::Screen => {
            if let Some(bytes) = encode_key(key) {
                // Claude fires no hook when the user interrupts a turn, so an
                // Escape would leave the sidebar spinner stuck. Flag it to the
                // conductor (which confirms it against Claude's transcript)
                // ahead of the Escape byte so the baseline length is captured
                // before the agent reacts.
                if key.code == KeyCode::Esc && attached_is_claude(model) {
                    send_interrupt_check(model)?;
                }
                send_input(model, &bytes)?;
            }
        }
    }
    Ok(false)
}

fn handle_prefix(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    match key.code {
        // `Left` and `h` focus the sidebar; both are reserved keys (see
        // `reserved_binding`) so no action ever rebinds over them.
        KeyCode::Left | KeyCode::Char('h') => {
            model.focus = Focus::Sidebar;
            return Ok(false);
        }
        KeyCode::Right => {
            model.focus = Focus::Screen;
            return Ok(false);
        }
        KeyCode::Char('?') => {
            model.help = !model.help;
            return Ok(false);
        }
        _ => {}
    }
    let name = key_name(key);
    let action = model.config.action_for_key(&name).map(str::to_owned);
    match action.as_deref() {
        Some("focus_screen") => model.focus = Focus::Screen,
        // Re-claim the shared PTY at this terminal's pane size. A phone that
        // resized the session down to its own viewport leaves the desktop
        // rendering a tiny grid; this is the one-key reclaim (the phone's
        // equivalent is the top-bar resize button).
        Some("resize_pane") => resize_attached(model)?,
        Some("scroll") => {
            model.scroll_mode = true;
            scroll(model, model.pane.height.saturating_sub(1).max(1) as isize)?;
        }
        Some("new_claude") => spawn_agent(model, "claude")?,
        Some("new_codex") => spawn_agent(model, "codex")?,
        Some("scope_toggle") => model.sidebar.toggle_mode(),
        Some("jump_pending") => {
            if model.sidebar.jump_to_attention().is_some() {
                model.focus = Focus::Screen;
            }
        }
        Some("rename") => {
            let target = model
                .selected()
                .or_else(|| model.attached_session())
                .map(|session| (session.id.clone(), session_label(session, &model.tasks)));
            if let Some((id, input)) = target {
                model.rename = Some(Rename { id, input });
            }
        }
        Some("new_worktree") => open_worktree_picker(model),
        Some("kill") => {
            if let Some(id) = model.attach.as_ref().map(|attach| attach.id.clone()) {
                let response = request(Request {
                    kind: "kill".to_owned(),
                    id: id.clone(),
                    ..Request::default()
                })?;
                if response.ok {
                    if !model.sidebar.select_previous_session(&id) {
                        model.focus = Focus::Sidebar;
                    }
                } else {
                    model.error = response.error;
                }
            }
        }
        Some("newline") => send_input(model, b"\n")?,
        Some("quit") => return Ok(true),
        Some("config") => {
            model.config_menu = Some(ConfigMenu {
                cursor: match (prefix_overridden(), theme_overridden()) {
                    (false, _) => 0,
                    (true, false) => 1,
                    (true, true) => 2,
                },
                capture: false,
                error: String::new(),
                theme_cursor: None,
                notification_cursor: None,
                original_theme: None,
            });
        }
        Some("task_backlog") => {
            model.task_modal = Some(TaskModal {
                stage: TaskStage::List,
                cursor: 0,
            });
        }
        Some("session_history") => {
            model.history_modal = Some(HistoryModal { cursor: 0 });
        }
        Some("yank") => {
            copy_selection(model)?;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_rename(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    match key.code {
        KeyCode::Enter => {
            if let Some(rename) = model.rename.take() {
                let _ = request(Request {
                    kind: "rename".to_owned(),
                    id: rename.id,
                    name: rename.input.trim().to_owned(),
                    ..Request::default()
                });
            }
        }
        KeyCode::Esc => model.rename = None,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.rename = None;
        }
        KeyCode::Backspace | KeyCode::Delete => {
            if let Some(rename) = model.rename.as_mut() {
                rename.input.pop();
            }
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
        {
            if let Some(rename) = model.rename.as_mut() {
                rename.input.push(character);
            }
        }
        _ => {}
    }
    Ok(false)
}

fn prefix_overridden() -> bool {
    std::env::var("CB_PREFIX")
        .ok()
        .is_some_and(|prefix| !prefix.trim().is_empty())
}

fn theme_overridden() -> bool {
    std::env::var("CB_THEME")
        .ok()
        .is_some_and(|theme| !theme.trim().is_empty())
}

fn handle_config_menu(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    let row_count = crate::config::ACTIONS.len() + 4;
    if let Some(cursor) = model
        .config_menu
        .as_ref()
        .and_then(|menu| menu.notification_cursor)
    {
        match key.code {
            KeyCode::Esc => {
                if let Some(menu) = model.config_menu.as_mut() {
                    menu.notification_cursor = None;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(menu) = model.config_menu.as_mut() {
                    menu.notification_cursor = Some(
                        (cursor + crate::notify::DELIVERY_NAMES.len() - 1)
                            % crate::notify::DELIVERY_NAMES.len(),
                    );
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(menu) = model.config_menu.as_mut() {
                    menu.notification_cursor =
                        Some((cursor + 1) % crate::notify::DELIVERY_NAMES.len());
                }
            }
            KeyCode::Enter => {
                model.config.notifications.delivery =
                    crate::notify::Delivery::from_name(crate::notify::DELIVERY_NAMES[cursor])
                        .unwrap_or_default();
                if model.config.notifications.delivery == crate::notify::Delivery::Off {
                    model.pending_notifications.clear();
                    model.toasts.clear();
                }
                if let Some(menu) = model.config_menu.as_mut() {
                    menu.notification_cursor = None;
                    menu.error.clear();
                }
                if let Err(error) = model.config.save() {
                    model.error = format!("config save failed: {error}");
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    if model
        .config_menu
        .as_ref()
        .is_some_and(|menu| menu.theme_cursor.is_some())
    {
        let cursor = model
            .config_menu
            .as_ref()
            .and_then(|menu| menu.theme_cursor)
            .unwrap_or_default();
        match key.code {
            KeyCode::Esc => {
                if let Some(original) = model
                    .config_menu
                    .as_mut()
                    .and_then(|menu| menu.original_theme.take())
                {
                    model.config.theme.name = original;
                    model.palette = Palette::resolve(&model.config.theme);
                }
                if let Some(menu) = model.config_menu.as_mut() {
                    menu.theme_cursor = None;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let next = (cursor + THEME_NAMES.len() - 1) % THEME_NAMES.len();
                preview_theme(model, next);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                preview_theme(model, (cursor + 1) % THEME_NAMES.len());
            }
            KeyCode::Enter => {
                if let Some(menu) = model.config_menu.as_mut() {
                    menu.theme_cursor = None;
                    menu.original_theme = None;
                    menu.error.clear();
                }
                if let Err(error) = model.config.save() {
                    model.error = format!("config save failed: {error}");
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    let Some(menu) = model.config_menu.as_mut() else {
        return Ok(false);
    };
    if menu.capture {
        if key.code == KeyCode::Esc {
            menu.capture = false;
            menu.error.clear();
            return Ok(false);
        }
        let name = key_name(key);
        if name.is_empty() {
            return Ok(false);
        }
        if let Some(reason) = reserved_binding(&name) {
            menu.error = format!("reserved: {reason}");
            return Ok(false);
        }
        if menu.cursor == 0 {
            if prefix_overridden() {
                menu.error = "locked: unset CB_PREFIX to edit".to_owned();
                return Ok(false);
            }
            model.config.prefix = name;
        } else if (3..crate::config::ACTIONS.len() + 3).contains(&menu.cursor) {
            let action = crate::config::ACTIONS[menu.cursor - 3];
            if let Some(conflict) =
                model.config.bindings.iter().find_map(|(id, bound)| {
                    (id != action.id && bound == &name).then_some(id.clone())
                })
            {
                let label = crate::config::ACTIONS
                    .iter()
                    .find(|candidate| candidate.id == conflict)
                    .map(|candidate| candidate.label)
                    .unwrap_or(&conflict);
                menu.error = format!("already bound to {label:?}");
                return Ok(false);
            }
            model.config.bindings.insert(action.id.to_owned(), name);
        }
        menu.capture = false;
        menu.error.clear();
        if let Err(error) = model.config.save() {
            model.error = format!("config save failed: {error}");
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => model.config_menu = None,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.config_menu = None;
        }
        KeyCode::Up | KeyCode::Char('k') => loop {
            menu.cursor = (menu.cursor + row_count - 1) % row_count;
            if !(menu.cursor == 0 && prefix_overridden() || menu.cursor == 1 && theme_overridden())
            {
                break;
            }
        },
        KeyCode::Down | KeyCode::Char('j') => loop {
            menu.cursor = (menu.cursor + 1) % row_count;
            if !(menu.cursor == 0 && prefix_overridden() || menu.cursor == 1 && theme_overridden())
            {
                break;
            }
        },
        KeyCode::Enter => {
            if menu.cursor == row_count - 1 {
                model.config = Config::default();
                model.palette = Palette::resolve(&model.config.theme);
                menu.error.clear();
                if let Err(error) = model.config.save() {
                    model.error = format!("config save failed: {error}");
                }
            } else if menu.cursor == 1 {
                let cursor = THEME_NAMES
                    .iter()
                    .position(|name| *name == model.config.theme.name)
                    .unwrap_or_default();
                menu.theme_cursor = Some(cursor);
                menu.original_theme = Some(model.config.theme.name.clone());
            } else if menu.cursor == 2 {
                let cursor = crate::notify::DELIVERY_NAMES
                    .iter()
                    .position(|name| *name == model.config.notifications.delivery.name())
                    .unwrap_or_default();
                menu.notification_cursor = Some(cursor);
            } else {
                menu.capture = true;
                menu.error.clear();
            }
        }
        _ => {}
    }
    Ok(false)
}

fn preview_theme(model: &mut Model, cursor: usize) {
    let cursor = cursor.min(THEME_NAMES.len().saturating_sub(1));
    model.config.theme.name = THEME_NAMES[cursor].to_owned();
    model.palette = Palette::resolve(&model.config.theme);
    if let Some(menu) = model.config_menu.as_mut() {
        menu.theme_cursor = Some(cursor);
    }
}

fn reserved_binding(key: &str) -> Option<&'static str> {
    match key {
        "esc" => Some("modal escape"),
        "h" | "left" => Some("focus sidebar"),
        "right" => Some("focus screen pane"),
        "up" | "down" | "j" | "k" => Some("navigation"),
        "?" => Some("toggle hints panel"),
        "ctrl+c" => Some("quit / SIGINT"),
        _ => None,
    }
}

fn visible_task_indices(model: &Model) -> Vec<usize> {
    let mut indices: Vec<usize> = model
        .tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| {
            (!task.auto && task.scope == model.sidebar.current_scope()).then_some(index)
        })
        .collect();
    indices.sort_by(|left, right| {
        let left_task = &model.tasks[*left];
        let right_task = &model.tasks[*right];
        let rank = |status| match status {
            TaskStatus::InProgress => 0,
            TaskStatus::Paused => 1,
            TaskStatus::Pending => 2,
            TaskStatus::Completed => 3,
        };
        rank(left_task.status)
            .cmp(&rank(right_task.status))
            .then_with(|| {
                if left_task.status == TaskStatus::Completed {
                    right_task.updated_at.cmp(&left_task.updated_at)
                } else {
                    left_task.created_at.cmp(&right_task.created_at)
                }
            })
    });
    indices
}

fn handle_task_modal(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    let Some(mut modal) = model.task_modal.take() else {
        return Ok(false);
    };
    let mut keep_open = true;
    match &mut modal.stage {
        TaskStage::List => {
            let indices = visible_task_indices(model);
            modal.cursor = modal.cursor.min(indices.len().saturating_sub(1));
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => keep_open = false,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    keep_open = false;
                }
                KeyCode::Up | KeyCode::Char('k') if !indices.is_empty() => {
                    modal.cursor = (modal.cursor + indices.len() - 1) % indices.len();
                }
                KeyCode::Down | KeyCode::Char('j') if !indices.is_empty() => {
                    modal.cursor = (modal.cursor + 1) % indices.len();
                }
                KeyCode::Char('n') => {
                    modal.stage = TaskStage::New {
                        title: String::new(),
                        desc: String::new(),
                        title_active: true,
                    };
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    if let Some(task) = indices
                        .get(modal.cursor)
                        .and_then(|index| model.tasks.get(*index))
                        .cloned()
                    {
                        if key.code == KeyCode::Enter && task.status == TaskStatus::InProgress {
                            if let Some(run) = task
                                .runs
                                .iter()
                                .rev()
                                .find(|run| run.status == TaskStatus::InProgress)
                            {
                                jump_to_session(model, &run.cb_session_id);
                                keep_open = false;
                            }
                        } else {
                            modal.stage = TaskStage::Detail {
                                id: task.id,
                                title: task.title,
                                desc: task.desc,
                                title_active: true,
                            };
                        }
                    }
                }
                KeyCode::Char('s') => {
                    if let Some(task) = indices
                        .get(modal.cursor)
                        .and_then(|index| model.tasks.get(*index))
                        .filter(|task| task.status != TaskStatus::Completed)
                    {
                        if worktree::available_agents().is_empty() {
                            model.error = "no agent binaries found".to_owned();
                        } else {
                            modal.stage = TaskStage::Agent {
                                id: task.id.clone(),
                                cursor: 0,
                            };
                        }
                    }
                }
                KeyCode::Char('r') => {
                    let resume = indices
                        .get(modal.cursor)
                        .and_then(|index| model.tasks.get(*index))
                        .and_then(|task| {
                            task.runs
                                .iter()
                                .rev()
                                .find(|run| run.status == TaskStatus::Paused)
                                .map(|run| (task.id.clone(), run.id.clone()))
                        });
                    if let Some((task_id, run_id)) = resume {
                        let response = request(Request {
                            kind: "task_resume".to_owned(),
                            id: task_id,
                            run_id,
                            cwd: model.launch_cwd.display().to_string(),
                            rows: model.pane.height,
                            cols: model.pane.width,
                            ..Request::default()
                        })?;
                        apply_task_response(model, response);
                        keep_open = false;
                    }
                }
                KeyCode::Char('K') => {
                    if let Some(task) = indices
                        .get(modal.cursor)
                        .and_then(|index| model.tasks.get(*index))
                        .filter(|task| !task.runs.is_empty())
                    {
                        modal.stage = TaskStage::Runs {
                            id: task.id.clone(),
                            cursor: 0,
                        };
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(task) = indices
                        .get(modal.cursor)
                        .and_then(|index| model.tasks.get(*index))
                    {
                        let status = if task.status == TaskStatus::Completed {
                            "pending"
                        } else {
                            "completed"
                        };
                        let response = request(Request {
                            kind: "task_status".to_owned(),
                            id: task.id.clone(),
                            task_status: status.to_owned(),
                            ..Request::default()
                        })?;
                        apply_task_response(model, response);
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(id) = indices
                        .get(modal.cursor)
                        .and_then(|index| model.tasks.get(*index))
                        .map(|task| task.id.clone())
                    {
                        let response = request(Request {
                            kind: "task_delete".to_owned(),
                            id,
                            ..Request::default()
                        })?;
                        apply_task_response(model, response);
                    }
                }
                _ => {}
            }
        }
        TaskStage::New {
            title,
            desc,
            title_active,
        } => match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !title.trim().is_empty() {
                    let response = request(Request {
                        kind: "task_add".to_owned(),
                        scope: model.sidebar.current_scope().to_owned(),
                        title: title.trim().to_owned(),
                        desc: desc.trim().to_owned(),
                        ..Request::default()
                    })?;
                    apply_task_response(model, response);
                }
                modal.stage = TaskStage::List;
            }
            KeyCode::Esc => modal.stage = TaskStage::List,
            KeyCode::Tab => *title_active = !*title_active,
            KeyCode::Enter if *title_active => *title_active = false,
            KeyCode::Enter => desc.push('\n'),
            KeyCode::Backspace | KeyCode::Delete => {
                if *title_active {
                    title.pop();
                } else {
                    desc.pop();
                }
            }
            KeyCode::Char(character) => {
                if *title_active {
                    title.push(character);
                } else {
                    desc.push(character);
                }
            }
            _ => {}
        },
        TaskStage::Detail {
            id,
            title,
            desc,
            title_active,
        } => match key.code {
            KeyCode::Esc => {
                let response = request(Request {
                    kind: "task_edit".to_owned(),
                    id: id.clone(),
                    title: title.clone(),
                    desc: desc.clone(),
                    ..Request::default()
                })?;
                apply_task_response(model, response);
                modal.stage = TaskStage::List;
            }
            KeyCode::Tab => *title_active = !*title_active,
            KeyCode::Enter if *title_active => *title_active = false,
            KeyCode::Enter => desc.push('\n'),
            KeyCode::Backspace | KeyCode::Delete => {
                if *title_active {
                    title.pop();
                } else {
                    desc.pop();
                }
            }
            KeyCode::Char(character) => {
                if *title_active {
                    title.push(character);
                } else {
                    desc.push(character);
                }
            }
            _ => {}
        },
        TaskStage::Agent { id, cursor } => {
            let agents = worktree::available_agents();
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                    modal.stage = TaskStage::List;
                }
                KeyCode::Char('q') => keep_open = false,
                KeyCode::Up | KeyCode::Char('k') if !agents.is_empty() => {
                    *cursor = (*cursor + agents.len() - 1) % agents.len();
                }
                KeyCode::Down | KeyCode::Char('j') if !agents.is_empty() => {
                    *cursor = (*cursor + 1) % agents.len();
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    if let Some(agent) = agents.get(*cursor) {
                        let response = request(Request {
                            kind: "task_start".to_owned(),
                            id: id.clone(),
                            agent: agent.binary.to_owned(),
                            cwd: model.launch_cwd.display().to_string(),
                            rows: model.pane.height,
                            cols: model.pane.width,
                            ..Request::default()
                        })?;
                        apply_task_response(model, response);
                        keep_open = false;
                    }
                }
                _ => {}
            }
        }
        TaskStage::Runs { id, cursor } => {
            let runs = model
                .tasks
                .iter()
                .find(|task| task.id == *id)
                .map(|task| task.runs.clone())
                .unwrap_or_default();
            *cursor = (*cursor).min(runs.len().saturating_sub(1));
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                    modal.stage = TaskStage::List;
                }
                KeyCode::Char('q') => keep_open = false,
                KeyCode::Up | KeyCode::Char('k') if !runs.is_empty() => {
                    *cursor = (*cursor + runs.len() - 1) % runs.len();
                }
                KeyCode::Down | KeyCode::Char('j') if !runs.is_empty() => {
                    *cursor = (*cursor + 1) % runs.len();
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    if let Some(run) = runs
                        .get(*cursor)
                        .filter(|run| run.status == TaskStatus::InProgress)
                    {
                        jump_to_session(model, &run.cb_session_id);
                        keep_open = false;
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(run) = runs
                        .get(*cursor)
                        .filter(|run| run.status == TaskStatus::InProgress)
                    {
                        let response = request(Request {
                            kind: "kill".to_owned(),
                            id: run.cb_session_id.clone(),
                            ..Request::default()
                        })?;
                        if !response.ok {
                            model.error = response.error;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if keep_open {
        model.task_modal = Some(modal);
    }
    Ok(false)
}

fn apply_task_response(model: &mut Model, response: Response) {
    if response.ok {
        model.tasks = response.tasks;
        if !response.id.is_empty() {
            jump_to_session(model, &response.id);
        }
    } else {
        model.error = response.error;
    }
}

/// A compact "time ago" label (`now`, `5m`, `2h`, `3d`, `1w`, `4M`, `2y`) for a
/// past instant, capped at the largest whole unit so a row stays short.
fn relative_time(then: OffsetDateTime) -> String {
    let secs = (OffsetDateTime::now_utc() - then).whole_seconds().max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    if secs < MINUTE {
        "now".to_owned()
    } else if secs < HOUR {
        format!("{}m", secs / MINUTE)
    } else if secs < DAY {
        format!("{}h", secs / HOUR)
    } else if secs < WEEK {
        format!("{}d", secs / DAY)
    } else if secs < MONTH {
        format!("{}w", secs / WEEK)
    } else if secs < YEAR {
        format!("{}M", secs / MONTH)
    } else {
        format!("{}y", secs / YEAR)
    }
}

/// Every session in the current workspace scope, most recent first — both live
/// (running) runs and paused, resumable ones. These are the sessions surfaced
/// by the `session_history` action, covering ad-hoc auto sessions and task
/// runs. Pending runs carry no session and never appear.
fn history_entries(model: &Model) -> Vec<HistoryEntry> {
    let scope = model.sidebar.current_scope();
    let mut entries: Vec<HistoryEntry> = model
        .tasks
        .iter()
        .filter(|task| task.scope == scope)
        .flat_map(|task| {
            task.runs
                .iter()
                .filter(|run| matches!(run.status, TaskStatus::InProgress | TaskStatus::Paused))
                .map(|run| HistoryEntry {
                    task_id: task.id.clone(),
                    run_id: run.id.clone(),
                    agent: run.agent.clone(),
                    title: run.title.clone(),
                    first_message: run.first_message.clone(),
                    auto: task.auto,
                    live: run.status == TaskStatus::InProgress,
                    cb_session_id: run.cb_session_id.clone(),
                    updated_at: run.updated_at,
                })
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    entries
}

fn handle_history_modal(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    let Some(mut modal) = model.history_modal.take() else {
        return Ok(false);
    };
    let entries = history_entries(model);
    modal.cursor = modal.cursor.min(entries.len().saturating_sub(1));
    let mut keep_open = true;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => keep_open = false,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => keep_open = false,
        KeyCode::Up | KeyCode::Char('k') if !entries.is_empty() => {
            modal.cursor = (modal.cursor + entries.len() - 1) % entries.len();
        }
        KeyCode::Down | KeyCode::Char('j') if !entries.is_empty() => {
            modal.cursor = (modal.cursor + 1) % entries.len();
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            if let Some(entry) = entries.get(modal.cursor) {
                if entry.live {
                    // A running session is already in the sidebar; resuming it
                    // would spawn a duplicate, so jump to the live one instead.
                    jump_to_session(model, &entry.cb_session_id);
                } else {
                    let response = request(Request {
                        kind: "task_resume".to_owned(),
                        id: entry.task_id.clone(),
                        run_id: entry.run_id.clone(),
                        cwd: model.launch_cwd.display().to_string(),
                        rows: model.pane.height,
                        cols: model.pane.width,
                        ..Request::default()
                    })?;
                    apply_task_response(model, response);
                }
                keep_open = false;
            }
        }
        KeyCode::Char('x') => {
            // Only paused auto sessions are deletable here; live sessions must
            // be killed first, and real backlog tasks are managed from the task
            // modal, so leave both untouched.
            if let Some(entry) = entries
                .get(modal.cursor)
                .filter(|entry| entry.auto && !entry.live)
            {
                let response = request(Request {
                    kind: "task_delete".to_owned(),
                    id: entry.task_id.clone(),
                    ..Request::default()
                })?;
                apply_task_response(model, response);
            }
        }
        _ => {}
    }
    if keep_open {
        model.history_modal = Some(modal);
    }
    Ok(false)
}

fn jump_to_session(model: &mut Model, id: &str) {
    if model.sidebar.select_session(id) {
        model.focus = Focus::Screen;
    } else if !id.is_empty() {
        model.pending_jump = Some(id.to_owned());
    }
}

fn open_worktree_picker(model: &mut Model) {
    let agents = worktree::available_agents();
    if agents.is_empty() {
        model.error = "no agent binaries found (claude/codex/opencode)".to_owned();
        return;
    }
    let Ok(worktrees) = worktree::list(&model.launch_cwd) else {
        model.error = "no git worktrees here".to_owned();
        return;
    };
    if worktrees.is_empty() {
        model.error = "no git worktrees here".to_owned();
        return;
    }
    let launch = model
        .launch_cwd
        .canonicalize()
        .unwrap_or_else(|_| model.launch_cwd.clone());
    let worktree_cursor = worktrees
        .iter()
        .position(|worktree| {
            worktree
                .path
                .canonicalize()
                .unwrap_or_else(|_| worktree.path.clone())
                == launch
        })
        .unwrap_or_default();
    model.error.clear();
    model.worktree_picker = Some(WorktreePicker {
        stage: PickerStage::Worktree,
        worktrees,
        agents,
        worktree_cursor,
        agent_cursor: 0,
        chosen: None,
    });
}

fn handle_worktree_picker(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    let Some(picker) = model.worktree_picker.as_mut() else {
        return Ok(false);
    };
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        model.worktree_picker = None;
        return Ok(false);
    }
    match picker.stage {
        PickerStage::Worktree => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => model.worktree_picker = None,
            KeyCode::Up | KeyCode::Char('k') => {
                picker.worktree_cursor = picker.worktree_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                picker.worktree_cursor =
                    (picker.worktree_cursor + 1).min(picker.worktrees.len().saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                picker.chosen = picker
                    .worktrees
                    .get(picker.worktree_cursor)
                    .map(|worktree| worktree.path.clone());
                picker.stage = PickerStage::Agent;
                picker.agent_cursor = 0;
            }
            _ => {}
        },
        PickerStage::Agent => match key.code {
            KeyCode::Char('q') => model.worktree_picker = None,
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                picker.stage = PickerStage::Worktree;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                picker.agent_cursor = picker.agent_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                picker.agent_cursor =
                    (picker.agent_cursor + 1).min(picker.agents.len().saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let choice = picker
                    .agents
                    .get(picker.agent_cursor)
                    .copied()
                    .zip(picker.chosen.clone());
                model.worktree_picker = None;
                if let Some((agent, cwd)) = choice {
                    spawn_agent_at(model, agent.binary, cwd)?;
                }
            }
            _ => {}
        },
    }
    Ok(false)
}

fn handle_scroll(model: &mut Model, key: KeyEvent) -> io::Result<bool> {
    let page = model.pane.height.saturating_sub(1).max(1) as usize;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => scroll(model, 1)?,
        KeyCode::Down | KeyCode::Char('j') => scroll(model, -1)?,
        KeyCode::PageUp | KeyCode::Char('b') => scroll(model, page as isize)?,
        KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char(' ') => {
            scroll(model, -(page as isize))?
        }
        KeyCode::Char('g') => {
            let max = model
                .frame
                .as_ref()
                .map(|frame| frame.max_offset)
                .unwrap_or_default();
            set_scroll(model, max)?;
        }
        KeyCode::Char('G') => {
            set_scroll(model, 0)?;
            model.scroll_mode = false;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            set_scroll(model, 0)?;
            model.scroll_mode = false;
        }
        _ => {}
    }
    Ok(false)
}

fn scroll(model: &mut Model, delta: isize) -> io::Result<()> {
    let (offset, max) = model
        .frame
        .as_ref()
        .map(|frame| (frame.offset, frame.max_offset))
        .unwrap_or_default();
    let next = if delta >= 0 {
        offset.saturating_add(delta as usize)
    } else {
        offset.saturating_sub(delta.unsigned_abs())
    }
    .min(max);
    set_scroll(model, next)
}

fn set_scroll(model: &mut Model, offset: usize) -> io::Result<()> {
    if let Some(attach) = model.attach.as_mut() {
        write_json(
            &mut attach.writer,
            &StreamUp {
                kind: "scroll".to_owned(),
                offset,
                ..StreamUp::default()
            },
        )?;
    }
    Ok(())
}

fn send_input(model: &mut Model, bytes: &[u8]) -> io::Result<()> {
    if let Some(attach) = model.attach.as_mut() {
        write_json(
            &mut attach.writer,
            &StreamUp {
                kind: "input".to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                ..StreamUp::default()
            },
        )?;
    }
    Ok(())
}

/// Whether the attached session is a Claude agent. Interrupt confirmation only
/// applies to Claude, which alone reports a transcript path and fires no hook
/// on interrupt.
fn attached_is_claude(model: &Model) -> bool {
    model
        .attached_session()
        .and_then(|session| session.argv.first())
        .map(std::path::Path::new)
        .and_then(std::path::Path::file_name)
        .and_then(|name| name.to_str())
        == Some("claude")
}

/// Signal the conductor that the user pressed Escape on the attached session so
/// it can confirm the interrupt against the agent's transcript. Sent before the
/// Escape byte itself; see the `Focus::Screen` handler.
fn send_interrupt_check(model: &mut Model) -> io::Result<()> {
    if let Some(attach) = model.attach.as_mut() {
        write_json(
            &mut attach.writer,
            &StreamUp {
                kind: "interrupt_check".to_owned(),
                ..StreamUp::default()
            },
        )?;
    }
    Ok(())
}

fn handle_paste(model: &mut Model, text: &str) -> io::Result<()> {
    if let Some(modal) = model.task_modal.as_mut() {
        match &mut modal.stage {
            TaskStage::New {
                title,
                desc,
                title_active,
            } => {
                if *title_active {
                    let mut parts = text.splitn(2, '\n');
                    title.push_str(parts.next().unwrap_or_default().trim_end_matches('\r'));
                    if let Some(rest) = parts.next() {
                        desc.push_str(rest.trim_start_matches('\r'));
                        *title_active = false;
                    }
                } else {
                    desc.push_str(text);
                }
            }
            TaskStage::Detail {
                title,
                desc,
                title_active,
                ..
            } => {
                if *title_active {
                    title.push_str(&text.replace('\n', " "));
                } else {
                    desc.push_str(text);
                }
            }
            _ => {}
        }
    } else if let Some(rename) = model.rename.as_mut() {
        rename.input.push_str(text);
    } else if model.focus == Focus::Screen
        && model.config_menu.is_none()
        && model.worktree_picker.is_none()
    {
        if let Some(attach) = model.attach.as_mut() {
            write_json(
                &mut attach.writer,
                &StreamUp {
                    kind: "paste".to_owned(),
                    data: base64::engine::general_purpose::STANDARD.encode(text),
                    ..StreamUp::default()
                },
            )?;
        }
    }
    Ok(())
}

fn send_focus(model: &mut Model, focused: bool) -> io::Result<()> {
    if let Some(attach) = model.attach.as_mut() {
        write_json(
            &mut attach.writer,
            &StreamUp {
                kind: "focus".to_owned(),
                data: if focused { "1" } else { "0" }.to_owned(),
                ..StreamUp::default()
            },
        )?;
    }
    Ok(())
}

fn handle_mouse(model: &mut Model, mouse: MouseEvent) -> io::Result<()> {
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some(id) = toast_at(model, mouse.column, mouse.row) {
            if model.sidebar.select_session(&id) {
                model.focus = Focus::Screen;
                model.toasts.retain(|toast| toast.session_id != id);
            }
            return Ok(());
        }
    }
    let inside = mouse.column >= model.pane.x
        && mouse.column < model.pane.right()
        && mouse.row >= model.pane.y
        && mouse.row < model.pane.bottom();
    let child_mouse = inside
        && model.focus == Focus::Screen
        && !model.scroll_mode
        && !mouse.modifiers.contains(KeyModifiers::SHIFT)
        && model
            .frame
            .as_ref()
            .is_some_and(|frame| frame.mouse_reporting);
    if child_mouse {
        forward_mouse(model, mouse)?;
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::ScrollUp if inside => {
            model.scroll_mode = true;
            scroll(model, 3)?;
        }
        MouseEventKind::ScrollDown if inside => {
            scroll(model, -3)?;
            if model.frame.as_ref().is_none_or(|frame| frame.offset == 0) {
                model.scroll_mode = false;
            }
        }
        MouseEventKind::Down(MouseButton::Left) if inside => {
            model.focus = Focus::Screen;
            let Some(session_id) = model.attach.as_ref().map(|attach| attach.id.clone()) else {
                return Ok(());
            };
            let double = model.last_click.take().is_some_and(|(at, col, row)| {
                col == mouse.column && row == mouse.row && at.elapsed() <= DOUBLE_CLICK
            });
            if double && select_word(model, &session_id, mouse.column, mouse.row) {
                model.word_selecting = true;
                return Ok(());
            }
            model.word_selecting = false;
            model.last_click = Some((Instant::now(), mouse.column, mouse.row));
            if let Some(point) = selection_point(model, mouse.column, mouse.row) {
                model.selection = Some(Selection {
                    session_id,
                    anchor: point,
                    cursor: point,
                    dragging: false,
                });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            model.word_selecting = false;
            update_selection_drag(model, mouse.column, mouse.row)?;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if !std::mem::take(&mut model.word_selecting) {
                update_selection_drag(model, mouse.column, mouse.row)?;
            }
            copy_selection(model)?;
        }
        _ => {}
    }
    Ok(())
}

fn forward_mouse(model: &mut Model, mouse: MouseEvent) -> io::Result<()> {
    let (action, button, pressed) = match mouse.kind {
        MouseEventKind::Down(button) => (0, mouse_button(button), false),
        MouseEventKind::Up(button) => (1, mouse_button(button), true),
        MouseEventKind::Drag(button) => (2, mouse_button(button), true),
        MouseEventKind::Moved => (2, 0, false),
        MouseEventKind::ScrollUp => (0, 4, false),
        MouseEventKind::ScrollDown => (0, 5, false),
        MouseEventKind::ScrollLeft => (0, 6, false),
        MouseEventKind::ScrollRight => (0, 7, false),
    };
    let mut modifiers = 0u16;
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers |= 1;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers |= 2;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        modifiers |= 4;
    }
    if mouse.modifiers.contains(KeyModifiers::SUPER) {
        modifiers |= 8;
    }
    if let Some(attach) = model.attach.as_mut() {
        write_json(
            &mut attach.writer,
            &StreamUp {
                kind: "mouse".to_owned(),
                mouse_action: action,
                mouse_button: button,
                mouse_modifiers: modifiers,
                mouse_x: mouse.column.saturating_sub(model.pane.x),
                mouse_y: mouse.row.saturating_sub(model.pane.y),
                mouse_pressed: pressed,
                ..StreamUp::default()
            },
        )?;
    }
    Ok(())
}

fn mouse_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
    }
}

/// Sonner-style toast cards, stacked at the bottom-left with the newest
/// nearest the corner. Returns each visible toast's index into `model.toasts`
/// paired with the bordered card rect. Shared by rendering and hit-testing so
/// clicks always match what is drawn.
const TOAST_MAX_WIDTH: u16 = 46;
const TOAST_HEIGHT: u16 = 3;
// Stack toast cards flush against each other: a terminal cell is far taller
// than the few-pixel gap we want, so the tightest look is zero blank rows,
// letting each card's own border draw the thin seam between them.
const TOAST_GAP: u16 = 0;
const TOAST_MARGIN_X: u16 = 2;
const TOAST_MARGIN_Y: u16 = 1;
const TOAST_VISIBLE: usize = 5;

fn toast_cards(toasts: &[Toast], area: Rect) -> Vec<(usize, Rect)> {
    let mut cards = Vec::new();
    if area.width < 20 {
        return cards;
    }
    let inner_cap = TOAST_MAX_WIDTH
        .saturating_sub(4)
        .min(area.width.saturating_sub(TOAST_MARGIN_X + 4));
    for (slot, (index, toast)) in toasts
        .iter()
        .enumerate()
        .rev()
        .take(TOAST_VISIBLE)
        .enumerate()
    {
        let slot = slot as u16;
        let offset = TOAST_MARGIN_Y + (slot + 1) * TOAST_HEIGHT + slot * TOAST_GAP;
        if area.height < offset {
            break;
        }
        // Two extra columns carry the status icon in front of the text.
        let inner = (toast.text.chars().count() as u16 + 2)
            .min(inner_cap)
            .max(1);
        let width = inner + 4;
        let card = Rect::new(
            area.x + TOAST_MARGIN_X,
            area.bottom().saturating_sub(offset),
            width,
            TOAST_HEIGHT,
        );
        cards.push((index, card));
    }
    cards
}

fn truncate_ellipsis(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn toast_at(model: &Model, column: u16, row: u16) -> Option<String> {
    toast_cards(&model.toasts, model.screen)
        .into_iter()
        .find_map(|(index, card)| {
            (column >= card.x && column < card.right() && row >= card.y && row < card.bottom())
                .then(|| model.toasts[index].session_id.clone())
        })
}

fn update_selection_drag(model: &mut Model, column: u16, row: u16) -> io::Result<()> {
    if model.selection.is_none() {
        return Ok(());
    }
    if row <= model.pane.y {
        model.scroll_mode = true;
        scroll(model, 3)?;
    } else if row >= model.pane.bottom().saturating_sub(1) {
        scroll(model, -3)?;
    }
    let clamped_column = column.clamp(model.pane.x, model.pane.right().saturating_sub(1));
    let clamped_row = row.clamp(model.pane.y, model.pane.bottom().saturating_sub(1));
    let Some(point) = selection_point(model, clamped_column, clamped_row) else {
        return Ok(());
    };
    if let Some(selection) = model.selection.as_mut() {
        selection.cursor = point;
        selection.dragging |= selection.cursor != selection.anchor;
    }
    Ok(())
}

/// A second left-press on the same cell within this window is a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// The three cell categories a double-click extends over: a word grows over
/// word chars, whitespace over blanks, and any other run (e.g. `->`, `===`)
/// over its own punctuation. Matching iTerm's default, paths and flags stay
/// whole by treating `_-./+~` as word characters.
#[derive(Debug, PartialEq, Eq)]
enum CharClass {
    Word,
    Space,
    Other,
}

fn char_class(symbol: &str) -> CharClass {
    let mut chars = symbol.chars();
    match (chars.next(), chars.next()) {
        // A blank or empty cell counts as whitespace.
        (None, _) => CharClass::Space,
        // A multi-codepoint grapheme (emoji, combined glyph) is one word cell.
        (Some(_), Some(_)) => CharClass::Word,
        (Some(c), None) => {
            if c.is_whitespace() {
                CharClass::Space
            } else if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | '~') {
                CharClass::Word
            } else {
                CharClass::Other
            }
        }
    }
}

/// The inclusive `[start, end]` cell range of the same-class run around `x`,
/// or `None` when the cell is blank (double-clicking whitespace selects
/// nothing). Word and other-punctuation runs both extend over their own class.
fn word_run(classes: &[CharClass], x: usize) -> Option<(usize, usize)> {
    let target = classes.get(x)?;
    if *target == CharClass::Space {
        return None;
    }
    let mut start = x;
    while start > 0 && classes[start - 1] == *target {
        start -= 1;
    }
    let mut end = x;
    while end + 1 < classes.len() && classes[end + 1] == *target {
        end += 1;
    }
    Some((start, end))
}

/// Select the run of same-class cells under a double-click, reading the word
/// straight off the visible frame. Returns whether a word (not blank) was
/// selected — a double-click on whitespace makes no selection.
fn select_word(model: &mut Model, session_id: &str, column: u16, row: u16) -> bool {
    let Some(frame) = model.frame.as_ref() else {
        return false;
    };
    let cols = usize::from(frame.cols);
    if cols == 0 {
        return false;
    }
    let x = usize::from(column.saturating_sub(model.pane.x));
    let y = usize::from(row.saturating_sub(model.pane.y));
    if x >= cols || y >= usize::from(frame.rows) {
        return false;
    }
    let Some(cells) = frame.cells.get(y * cols..y * cols + cols) else {
        return false;
    };
    let classes: Vec<CharClass> = cells.iter().map(|cell| char_class(&cell.symbol)).collect();
    let Some((start, end)) = word_run(&classes, x) else {
        return false;
    };
    let Some((line, _)) = selection_point(model, column, row) else {
        return false;
    };
    model.selection = Some(Selection {
        session_id: session_id.to_owned(),
        anchor: (line, start as u16),
        cursor: (line, end as u16),
        dragging: true,
    });
    true
}

fn selection_point(model: &Model, column: u16, row: u16) -> Option<(u32, u16)> {
    let frame = model.frame.as_ref()?;
    let viewport_top = frame.max_offset.saturating_sub(frame.offset);
    Some((
        u32::try_from(viewport_top)
            .unwrap_or(u32::MAX)
            .saturating_add(u32::from(row.saturating_sub(model.pane.y))),
        column.saturating_sub(model.pane.x),
    ))
}

fn copy_selection(model: &mut Model) -> io::Result<()> {
    let Some(selection) = model
        .selection
        .as_ref()
        .filter(|selection| selection.dragging)
    else {
        return Ok(());
    };
    if model
        .attach
        .as_ref()
        .is_none_or(|attach| attach.id != selection.session_id)
    {
        return Ok(());
    }
    let ((line_start, col_start), (line_end, col_end)) = selection.ordered();
    let response = request(Request {
        kind: "extract".to_owned(),
        id: selection.session_id.clone(),
        line_start,
        line_end,
        col_start,
        col_end,
        ..Request::default()
    })?;
    if response.ok && !response.text.is_empty() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(response.text);
        let mut stdout = io::stdout();
        write!(stdout, "\x1b]52;c;{encoded}\x07")?;
        stdout.flush()?;
    } else if !response.ok {
        model.error = response.error;
    }
    Ok(())
}

fn resize_attached(model: &mut Model) -> io::Result<()> {
    if let Some(attach) = model.attach.as_mut() {
        write_json(
            &mut attach.writer,
            &StreamUp {
                kind: "resize".to_owned(),
                rows: model.pane.height.max(1),
                cols: model.pane.width.max(1),
                ..StreamUp::default()
            },
        )?;
    }
    Ok(())
}

fn spawn_agent(model: &mut Model, agent: &str) -> io::Result<()> {
    let cwd = model.launch_cwd.clone();
    spawn_agent_at(model, agent, cwd)
}

fn spawn_agent_at(model: &mut Model, agent: &str, cwd: PathBuf) -> io::Result<()> {
    let response = request(Request {
        kind: "spawn".to_owned(),
        argv: vec![agent.to_owned()],
        cwd: cwd.display().to_string(),
        rows: model.pane.height.max(1),
        cols: model.pane.width.max(1),
        ..Request::default()
    })?;
    if response.ok {
        jump_to_session(model, &response.id);
    } else {
        model.error = response.error;
    }
    Ok(())
}

fn request(value: Request) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket_path())?;
    write_json(&mut stream, &value)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

/// Queries the host terminal for its default foreground/background colors and
/// reads the `OSC 10/11` replies straight off fd 0 under a short deadline. Must
/// run in raw mode and before the crossterm event loop starts consuming stdin.
/// Returns an empty theme if the terminal does not reply (common — many do not),
/// which the caller treats as a graceful no-op.
fn detect_host_theme(stdout: &mut impl Write) -> crate::terminal_theme::TerminalTheme {
    use crate::terminal_theme::{absorb_color_responses, TerminalTheme, HOST_COLOR_QUERY_SEQUENCE};

    let mut theme = TerminalTheme::default();
    if stdout
        .write_all(HOST_COLOR_QUERY_SEQUENCE.as_bytes())
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return theme;
    }
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    while theme.foreground.is_none() || theme.background.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut poll_fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: polling a single fd with a valid pollfd pointer.
        if unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) } <= 0 {
            break; // timeout or error — stop and keep whatever we have
        }
        // SAFETY: reading into a buffer we own; `n` bounds the initialized range.
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                chunk.as_mut_ptr() as *mut libc::c_void,
                chunk.len(),
            )
        };
        if n <= 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n as usize]);
        let consumed = absorb_color_responses(&buffer, &mut theme);
        buffer.drain(..consumed);
    }
    theme
}

struct View {
    sidebar: Rect,
    header: Option<Rect>,
    pane: Rect,
    footer: Option<Rect>,
    scrollbar: Option<Rect>,
}

/// Split the screen into chrome and the agent pane. The footer keybar spans
/// the full width; the header bar spans the main column only. Both give way
/// on tiny terminals so the agent pane always survives.
fn view(scrollback: bool, area: Rect) -> View {
    let footer_height = u16::from(area.height >= 6);
    let [content, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(footer_height)])
        .areas(area);
    let [sidebar, main] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)])
        .areas(content);
    let header_height = u16::from(main.height >= 4);
    let [header, pane_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(1)])
        .areas(main);
    let has_scrollbar = scrollback && pane_area.width > 1;
    View {
        sidebar,
        header: (header_height > 0).then_some(header),
        pane: Rect {
            width: pane_area.width.saturating_sub(u16::from(has_scrollbar)),
            ..pane_area
        },
        footer: (footer_height > 0).then_some(footer),
        scrollbar: has_scrollbar
            .then(|| Rect::new(pane_area.right() - 1, pane_area.y, 1, pane_area.height)),
    }
}

fn model_view(model: &Model, area: Rect) -> View {
    let scrollback = model
        .frame
        .as_ref()
        .is_some_and(|terminal| terminal.max_offset > 0);
    view(scrollback, area)
}

fn compute_view(model: &mut Model, area: Rect) {
    let view = model_view(model, area);
    model.screen = area;
    model.pane = view.pane;
}

fn render(model: &Model, frame: &mut Frame) {
    let view = model_view(model, frame.area());
    render_sidebar(model, frame, view.sidebar);
    if let Some(header) = view.header {
        render_header(model, frame, header);
    }
    render_terminal(model, frame);
    if let Some(scrollbar) = view.scrollbar {
        render_scrollbar(model, frame, scrollbar);
    }
    if let Some(footer) = view.footer {
        render_footer(model, frame, footer);
    } else if !model.error.is_empty() && frame.area().height > 0 {
        // Tiny terminal without a footer bar: overlay the error on the last row.
        let area = Rect::new(0, frame.area().bottom() - 1, frame.area().width, 1);
        frame.render_widget(
            Paragraph::new(model.error.clone()).style(Style::default().fg(model.palette.red)),
            area,
        );
    }
    render_overlays(model, frame);
}

/// `key desc · key desc` hint spans: keys in accent, descriptions dim. An
/// empty key renders the description alone as a plain dim note.
fn hint_spans(palette: &Palette, hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (key, description)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(palette.overlay0)));
        }
        if !key.is_empty() {
            spans.push(Span::styled(
                (*key).to_owned(),
                Style::default().fg(palette.accent),
            ));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            (*description).to_owned(),
            Style::default().fg(palette.overlay1),
        ));
    }
    spans
}

/// Compact display form of a binding: `ctrl+a` reads as `^a` everywhere the
/// chrome references the prefix.
fn key_display(name: &str) -> String {
    name.strip_prefix("ctrl+")
        .map(|rest| format!("^{rest}"))
        .unwrap_or_else(|| name.to_owned())
}

/// Home-relative, tail-biased path for the header bar: long paths keep their
/// most specific segments.
fn shorten_path(path: &str, home: Option<&str>, max: usize) -> String {
    let display = match home.filter(|home| !home.is_empty()) {
        Some(home) if path == home => "~".to_owned(),
        Some(home) => path
            .strip_prefix(home)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| format!("~/{rest}"))
            .unwrap_or_else(|| path.to_owned()),
        None => path.to_owned(),
    };
    let width = display.chars().count();
    if width <= max {
        return display;
    }
    if max == 0 {
        return String::new();
    }
    let tail: String = display.chars().skip(width - (max - 1)).collect();
    format!("…{tail}")
}

fn agent_name(session: &SessionInfo) -> Option<String> {
    session
        .argv
        .first()
        .map(|argv0| {
            Path::new(argv0)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| argv0.clone())
        })
        .filter(|name| !name.is_empty())
}

fn render_header(model: &Model, frame: &mut Frame, area: Rect) {
    let palette = &model.palette;
    let bar = Style::default()
        .bg(palette.surface_dim)
        .fg(palette.subtext0);
    let Some(session) = model.attached_session() else {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " ◇ codebridge",
                Style::default().fg(palette.overlay0),
            ))
            .style(bar),
            area,
        );
        return;
    };
    let (glyph, glyph_color) = indicator(session, model.spin, palette);
    let agent = agent_name(session).unwrap_or_default();
    let right = match model.frame.as_ref().filter(|terminal| terminal.offset > 0) {
        Some(terminal) => Span::styled(
            format!("⇅ {}/{} ", terminal.offset, terminal.max_offset),
            Style::default()
                .fg(palette.yellow)
                .add_modifier(Modifier::BOLD),
        ),
        None => Span::styled(
            format!(
                "{} ",
                shorten_path(
                    &session.cwd,
                    std::env::var("HOME").ok().as_deref(),
                    usize::from(area.width / 3),
                )
            ),
            Style::default().fg(palette.overlay1),
        ),
    };
    let right_width = Line::from(right.clone()).width();
    let fixed = 3 + if agent.is_empty() {
        0
    } else {
        agent.chars().count() + 2
    };
    let title_max = usize::from(area.width)
        .saturating_sub(fixed + right_width)
        .saturating_sub(2);
    let title = truncate_with_ellipsis(session_label(session, &model.tasks), title_max);
    let title_color = if model.focus == Focus::Screen {
        palette.text
    } else {
        palette.subtext0
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyph.to_string(), Style::default().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(
            title,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !agent.is_empty() {
        spans.push(Span::styled(
            format!("  {agent}"),
            Style::default().fg(palette.overlay1),
        ));
    }
    let used = Line::from(spans.clone()).width();
    let gap = usize::from(area.width).saturating_sub(used + right_width);
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(right);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

/// Colored per-status counters for the footer, non-zero counts only.
fn status_count_spans(model: &Model) -> Vec<Span<'static>> {
    let palette = &model.palette;
    let count = |status| {
        model
            .sidebar
            .sessions()
            .iter()
            .filter(|session| session.status == status)
            .count()
    };
    let mut spans = Vec::new();
    let states = [
        (
            SPINNERS[model.spin % SPINNERS.len()],
            palette.green,
            count(Status::Working),
        ),
        ('⚑', palette.red, count(Status::NeedsApproval)),
        ('●', palette.green, count(Status::WaitingUser)),
        ('●', palette.yellow, count(Status::Idle)),
        ('✗', palette.overlay0, count(Status::Ended)),
    ];
    for (glyph, color, total) in states {
        if total == 0 {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{glyph} "),
            Style::default().fg(color),
        ));
        spans.push(Span::styled(
            total.to_string(),
            Style::default().fg(palette.subtext0),
        ));
    }
    spans
}

fn badge(text: &str, background: Color, palette: &Palette) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default()
            .bg(background)
            .fg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    )
}

fn render_footer(model: &Model, frame: &mut Frame, area: Rect) {
    let palette = &model.palette;
    let bar = Style::default()
        .bg(palette.surface_dim)
        .fg(palette.subtext0);
    let prefix = key_display(&model.config.effective_prefix());

    let mut left: Vec<Span> = vec![Span::raw(" ")];
    if !model.error.is_empty() {
        left.push(Span::styled(
            format!("⚠ {}", model.error),
            Style::default().fg(palette.red),
        ));
    } else if model.prefix {
        left.push(badge(&prefix, palette.accent, palette));
        left.push(Span::raw(" "));
        left.extend(hint_spans(palette, &[("?", "commands"), ("esc", "cancel")]));
    } else if model.scroll_mode {
        left.push(badge("SCROLL", palette.yellow, palette));
        left.push(Span::raw(" "));
        left.extend(hint_spans(
            palette,
            &[
                ("↑/↓", "line"),
                ("b/f", "page"),
                ("g", "top"),
                ("q", "live"),
            ],
        ));
    } else if !model.hooks_ok {
        left.push(Span::styled(
            "⚠ hooks not installed — run: cb install-hooks",
            Style::default()
                .fg(palette.peach)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        let prefix_help = format!("{prefix} ?");
        let hints: &[(&str, &str)] = match model.focus {
            Focus::Sidebar => &[
                ("j/k", "move"),
                ("enter", "attach"),
                (prefix_help.as_str(), "commands"),
            ],
            Focus::Screen => &[
                (prefix.as_str(), "prefix"),
                ("shift+drag", "select"),
                (prefix_help.as_str(), "commands"),
            ],
        };
        left.extend(hint_spans(palette, hints));
    }

    let mut right = status_count_spans(model);
    let scope = if model.sidebar.accordion() {
        "all workspaces".to_owned()
    } else {
        scope_display_name(model.sidebar.current_scope())
    };
    if !right.is_empty() {
        right.push(Span::styled("  │  ", Style::default().fg(palette.overlay0)));
    }
    right.push(Span::styled(scope, Style::default().fg(palette.overlay1)));
    right.push(Span::raw(" "));

    let left_width = Line::from(left.clone()).width();
    let right_width = Line::from(right.clone()).width();
    let gap = usize::from(area.width).saturating_sub(left_width + right_width);
    let mut spans = left;
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

/// A floating chrome panel: rounded dim border, accent title, padded body,
/// and a structured key-hint footer. Every modal and picker renders through
/// this so the chrome reads as one system.
struct Panel {
    title: String,
    lines: Vec<Line<'static>>,
    hints: Vec<(&'static str, &'static str)>,
    max_width: u16,
    bottom: bool,
}

fn panel_width(area: Rect, max_width: u16) -> u16 {
    area.width.saturating_sub(4).clamp(1, max_width)
}

/// Content columns available inside a panel of `max_width` (borders + padding).
fn panel_inner_width(area: Rect, max_width: u16) -> usize {
    usize::from(panel_width(area, max_width).saturating_sub(4))
}

fn render_panel(model: &Model, frame: &mut Frame, area: Rect, panel: Panel) {
    let palette = &model.palette;
    let mut lines = panel.lines;
    if !panel.hints.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(hint_spans(palette, &panel.hints)));
    }
    let height = (lines.len() as u16 + 2).min(area.height).max(3);
    let width = panel_width(area, panel.max_width);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = if panel.bottom {
        area.bottom().saturating_sub(height + 1)
    } else {
        area.y + area.height.saturating_sub(height) / 2
    };
    let rect = Rect::new(x, y, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(palette.text).bg(palette.panel_bg))
            .block(
                Block::default()
                    .title(Line::styled(
                        format!(" {} ", panel.title),
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(palette.overlay0))
                    .padding(Padding::horizontal(1)),
            ),
        rect,
    );
}

/// A cursor-selectable row: accent gutter bar when selected, plus a
/// full-width surface highlight padded to `width` content columns.
fn select_line(
    palette: &Palette,
    selected: bool,
    spans: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let mut all = vec![Span::styled(
        if selected { "▌ " } else { "  " }.to_owned(),
        Style::default().fg(palette.accent),
    )];
    all.extend(spans);
    let mut line = Line::from(all);
    if selected {
        let used = line.width();
        if width > used {
            line.push_span(Span::raw(" ".repeat(width - used)));
        }
        line = line.style(
            Style::default()
                .bg(palette.surface0)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

fn render_overlays(model: &Model, frame: &mut Frame) {
    let area = frame.area();
    if let Some(modal) = model.history_modal.as_ref() {
        let inner = panel_inner_width(area, 82);
        let entries = history_entries(model);
        let mut lines = Vec::new();
        if entries.is_empty() {
            lines.push(Line::styled(
                "no sessions in this workspace",
                Style::default().fg(model.palette.overlay0),
            ));
        }
        for (cursor, entry) in entries.iter().enumerate() {
            let selected = cursor == modal.cursor;
            // Prefer the agent-summarised title; fall back to the first prompt
            // until (or unless) the agent generates one.
            let raw = if entry.title.trim().is_empty() {
                &entry.first_message
            } else {
                &entry.title
            };
            let label = raw.replace(['\n', '\t'], " ");
            let label = label.trim();
            let label: String = if label.is_empty() {
                "(no message yet)".to_owned()
            } else {
                label.chars().take(64).collect()
            };
            // A live session shows the working glyph; a paused one the parked
            // glyph, matching the task modal's vocabulary.
            let (glyph, glyph_color) = if entry.live {
                ('●', model.palette.green)
            } else {
                ('‖', model.palette.yellow)
            };
            lines.push(select_line(
                &model.palette,
                selected,
                vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(glyph_color)),
                    Span::styled(
                        format!("{:<8} ", entry.agent),
                        Style::default().fg(model.palette.overlay1),
                    ),
                    Span::styled(
                        format!("{:>4} ", relative_time(entry.updated_at)),
                        Style::default().fg(model.palette.overlay0),
                    ),
                    Span::raw(label),
                ],
                inner,
            ));
        }
        render_panel(
            model,
            frame,
            area,
            Panel {
                title: format!(
                    "history — {}",
                    scope_display_name(model.sidebar.current_scope())
                ),
                lines,
                hints: vec![("enter", "open/resume"), ("x", "delete"), ("esc", "close")],
                max_width: 82,
                bottom: false,
            },
        );
        return;
    }
    if let Some(modal) = model.task_modal.as_ref() {
        let inner = panel_inner_width(area, 82);
        let mut lines = Vec::new();
        // Shared title/description editor body for the New and Detail stages.
        let editor = |lines: &mut Vec<Line<'static>>, title: &str, desc: &str, title_active| {
            lines.push(Line::styled(
                "title",
                Style::default().fg(model.palette.overlay0),
            ));
            lines.push(Line::from(vec![
                Span::raw(title.to_owned()),
                Span::styled(
                    if title_active { "▎" } else { "" },
                    Style::default().fg(model.palette.accent),
                ),
            ]));
            lines.push(Line::default());
            lines.push(Line::styled(
                "description",
                Style::default().fg(model.palette.overlay0),
            ));
            lines.extend(desc.lines().map(|line| Line::from(line.to_owned())));
            if !title_active {
                lines.push(Line::styled("▎", Style::default().fg(model.palette.accent)));
            }
        };
        let (title, hints): (String, Vec<(&'static str, &'static str)>) = match &modal.stage {
            TaskStage::List => {
                let indices = visible_task_indices(model);
                if indices.is_empty() {
                    lines.push(Line::styled(
                        "no tasks — press n to create one",
                        Style::default().fg(model.palette.overlay0),
                    ));
                }
                let mut prior = None;
                for (cursor, index) in indices.into_iter().enumerate() {
                    let task = &model.tasks[index];
                    if prior != Some(task.status) {
                        if prior.is_some() {
                            lines.push(Line::default());
                        }
                        lines.push(Line::styled(
                            format!("{:?}", task.status).to_ascii_lowercase(),
                            Style::default().fg(model.palette.overlay0),
                        ));
                        prior = Some(task.status);
                    }
                    let (glyph, color) = match task.status {
                        TaskStatus::InProgress => ('●', model.palette.green),
                        TaskStatus::Paused => ('‖', model.palette.yellow),
                        TaskStatus::Pending => ('○', model.palette.overlay1),
                        TaskStatus::Completed => ('✓', model.palette.overlay0),
                    };
                    let mut spans = vec![
                        Span::styled(glyph.to_string(), Style::default().fg(color)),
                        Span::raw(format!(" {}", task.title)),
                    ];
                    if !task.runs.is_empty() {
                        spans.push(Span::styled(
                            format!("  {} session(s)", task.runs.len()),
                            Style::default().fg(model.palette.overlay1),
                        ));
                    }
                    lines.push(select_line(
                        &model.palette,
                        cursor == modal.cursor,
                        spans,
                        inner,
                    ));
                }
                (
                    format!(
                        "tasks — {}",
                        scope_display_name(model.sidebar.current_scope())
                    ),
                    vec![
                        ("n", "new"),
                        ("enter", "open"),
                        ("e", "edit"),
                        ("s", "start"),
                        ("r", "resume"),
                        ("K", "sessions"),
                        ("c", "done"),
                        ("x", "delete"),
                    ],
                )
            }
            TaskStage::New {
                title,
                desc,
                title_active,
            } => {
                editor(&mut lines, title, desc, *title_active);
                (
                    "new task".to_owned(),
                    vec![("tab", "switch"), ("ctrl+enter", "add"), ("esc", "cancel")],
                )
            }
            TaskStage::Detail {
                title,
                desc,
                title_active,
                ..
            } => {
                editor(&mut lines, title, desc, *title_active);
                (
                    "edit task".to_owned(),
                    vec![("tab", "switch"), ("esc", "save")],
                )
            }
            TaskStage::Agent { cursor, .. } => {
                for (index, agent) in worktree::available_agents().iter().enumerate() {
                    lines.push(select_line(
                        &model.palette,
                        index == *cursor,
                        vec![Span::raw(agent.label.to_owned())],
                        inner,
                    ));
                }
                (
                    "choose task agent".to_owned(),
                    vec![("enter", "start"), ("esc", "back")],
                )
            }
            TaskStage::Runs { id, cursor } => {
                if let Some(task) = model.tasks.iter().find(|task| task.id == *id) {
                    for (index, run) in task.runs.iter().enumerate() {
                        lines.push(select_line(
                            &model.palette,
                            index == *cursor,
                            vec![
                                Span::raw(run.agent.clone()),
                                Span::styled(
                                    format!(
                                        " · {:?} · {}",
                                        run.status,
                                        short_id(&run.cb_session_id)
                                    ),
                                    Style::default().fg(model.palette.overlay1),
                                ),
                            ],
                            inner,
                        ));
                    }
                }
                (
                    "task sessions".to_owned(),
                    vec![("enter", "jump"), ("x", "kill"), ("esc", "back")],
                )
            }
        };
        render_panel(
            model,
            frame,
            area,
            Panel {
                title,
                lines,
                hints,
                max_width: 82,
                bottom: false,
            },
        );
        return;
    }
    if let Some(menu) = model.config_menu.as_ref() {
        if let Some(cursor) = menu.theme_cursor {
            let inner = panel_inner_width(area, 42);
            let visible = usize::from(area.height.saturating_sub(6).max(1));
            let start = cursor
                .saturating_sub(visible.saturating_sub(1))
                .min(THEME_NAMES.len().saturating_sub(visible));
            let lines = THEME_NAMES
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(index, name)| {
                    select_line(
                        &model.palette,
                        index == cursor,
                        vec![Span::styled(
                            (*name).to_owned(),
                            Style::default().fg(if index == cursor {
                                model.palette.text
                            } else {
                                model.palette.subtext0
                            }),
                        )],
                        inner,
                    )
                })
                .collect();
            render_panel(
                model,
                frame,
                area,
                Panel {
                    title: "choose theme".to_owned(),
                    lines,
                    hints: vec![("↑/↓", "preview"), ("enter", "apply"), ("esc", "cancel")],
                    max_width: 42,
                    bottom: false,
                },
            );
            return;
        }
        if let Some(cursor) = menu.notification_cursor {
            let inner = panel_inner_width(area, 58);
            let lines = crate::notify::DELIVERY_NAMES
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    let selected = index == cursor;
                    let description = match *name {
                        "all" => "in-app + native system",
                        "codebridge" => "clickable in-app toast",
                        "terminal" => "Ghostty/iTerm/Kitty/WezTerm OSC",
                        "system" => "native OS notification",
                        "off" => "disable notifications",
                        _ => "",
                    };
                    select_line(
                        &model.palette,
                        selected,
                        vec![
                            Span::styled(
                                format!("{name:<12} "),
                                Style::default().fg(if selected {
                                    model.palette.text
                                } else {
                                    model.palette.subtext0
                                }),
                            ),
                            Span::styled(
                                description.to_owned(),
                                Style::default().fg(model.palette.overlay1),
                            ),
                        ],
                        inner,
                    )
                })
                .collect();
            render_panel(
                model,
                frame,
                area,
                Panel {
                    title: "notification delivery".to_owned(),
                    lines,
                    hints: vec![("↑/↓", "select"), ("enter", "apply"), ("esc", "cancel")],
                    max_width: 58,
                    bottom: false,
                },
            );
            return;
        }
        let inner = panel_inner_width(area, 66);
        let mut rows = Vec::with_capacity(crate::config::ACTIONS.len() + 4);
        rows.push((
            "prefix".to_owned(),
            if prefix_overridden() {
                format!("{} (CB_PREFIX)", model.config.effective_prefix())
            } else {
                model.config.prefix.clone()
            },
        ));
        rows.push((
            "theme".to_owned(),
            if theme_overridden() {
                format!(
                    "{} (CB_THEME)",
                    std::env::var("CB_THEME").unwrap_or_default()
                )
            } else {
                model.config.theme.name.clone()
            },
        ));
        rows.push((
            "notifications".to_owned(),
            model.config.notifications.delivery.name().to_owned(),
        ));
        rows.extend(crate::config::ACTIONS.iter().map(|action| {
            (
                action.label.to_owned(),
                model.config.bindings[action.id].clone(),
            )
        }));
        rows.push(("reset all to defaults".to_owned(), String::new()));
        let mut lines: Vec<Line<'static>> = rows
            .into_iter()
            .enumerate()
            .map(|(index, (label, value))| {
                let selected = index == menu.cursor;
                let value = if menu.capture && selected {
                    "[press key · esc cancel]".to_owned()
                } else {
                    value
                };
                select_line(
                    &model.palette,
                    selected,
                    vec![
                        Span::styled(
                            format!("{label:<30} "),
                            Style::default().fg(if selected {
                                model.palette.text
                            } else {
                                model.palette.subtext0
                            }),
                        ),
                        Span::styled(value, Style::default().fg(model.palette.accent)),
                    ],
                    inner,
                )
            })
            .collect();
        if !menu.error.is_empty() {
            lines.push(Line::styled(
                menu.error.clone(),
                Style::default().fg(model.palette.red),
            ));
        }
        render_panel(
            model,
            frame,
            area,
            Panel {
                title: "codebridge config".to_owned(),
                lines,
                hints: vec![
                    ("enter", "edit"),
                    ("esc", "close"),
                    ("", "previews live · saved automatically"),
                ],
                max_width: 66,
                bottom: false,
            },
        );
        return;
    }
    if let Some(picker) = model.worktree_picker.as_ref() {
        let (title, subtitle, choices, cursor) = match picker.stage {
            PickerStage::Worktree => (
                "start session in worktree",
                "git worktree list",
                picker
                    .worktrees
                    .iter()
                    .map(|worktree| {
                        let name = worktree
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| worktree.path.display().to_string());
                        format!("{name:<24} {}", worktree::tag(worktree))
                    })
                    .collect::<Vec<_>>(),
                picker.worktree_cursor,
            ),
            PickerStage::Agent => (
                "choose agent",
                "agent is selected for this launch only",
                picker
                    .agents
                    .iter()
                    .map(|agent| agent.label.to_owned())
                    .collect::<Vec<_>>(),
                picker.agent_cursor,
            ),
        };
        let inner = panel_inner_width(area, 60);
        let lines = std::iter::once(Line::styled(
            subtitle.to_owned(),
            Style::default().fg(model.palette.overlay0),
        ))
        .chain(std::iter::once(Line::default()))
        .chain(choices.into_iter().enumerate().map(|(index, choice)| {
            select_line(
                &model.palette,
                index == cursor,
                vec![Span::styled(
                    choice,
                    Style::default().fg(if index == cursor {
                        model.palette.text
                    } else {
                        model.palette.subtext0
                    }),
                )],
                inner,
            )
        }))
        .collect::<Vec<_>>();
        render_panel(
            model,
            frame,
            area,
            Panel {
                title: title.to_owned(),
                lines,
                hints: vec![("enter", "select"), ("esc", "cancel")],
                max_width: 60,
                bottom: false,
            },
        );
        return;
    }
    if let Some(rename) = model.rename.as_ref() {
        render_panel(
            model,
            frame,
            area,
            Panel {
                title: "rename session".to_owned(),
                lines: vec![Line::from(vec![
                    Span::raw(rename.input.clone()),
                    Span::styled("▎", Style::default().fg(model.palette.accent)),
                ])],
                hints: vec![("enter", "save"), ("esc", "cancel")],
                max_width: 54,
                bottom: false,
            },
        );
        return;
    }
    if model.prefix || model.help {
        let mut lines = Vec::new();
        let actions = crate::config::ACTIONS;
        let label_width = actions
            .iter()
            .map(|action| action.label.chars().count())
            .max()
            .unwrap_or(0);
        for pair in actions.chunks(2) {
            let mut spans = Vec::new();
            for action in pair {
                spans.push(Span::styled(
                    format!("{:>10}", key_display(&model.config.bindings[action.id])),
                    Style::default().fg(model.palette.accent),
                ));
                spans.push(Span::styled(
                    format!("  {:<label_width$}", action.label),
                    Style::default().fg(model.palette.subtext0),
                ));
            }
            lines.push(Line::from(spans));
        }
        render_panel(
            model,
            frame,
            area,
            Panel {
                title: format!(
                    "commands — prefix {}",
                    key_display(&model.config.effective_prefix())
                ),
                lines,
                hints: vec![("h/←", "sidebar"), ("→", "screen"), ("?", "close")],
                max_width: (label_width as u16 + 12) * 2 + 4,
                bottom: true,
            },
        );
        return;
    }
    for (index, card) in toast_cards(&model.toasts, model.screen) {
        let toast = &model.toasts[index];
        let color = match toast.status {
            Status::NeedsApproval => model.palette.red,
            Status::WaitingUser => model.palette.green,
            _ => model.palette.text,
        };
        let glyph = match toast.status {
            Status::NeedsApproval => '⚑',
            _ => '●',
        };
        let text = truncate_ellipsis(&toast.text, card.width.saturating_sub(6) as usize);
        frame.render_widget(Clear, card);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(text, Style::default().fg(model.palette.text)),
            ]))
            .style(Style::default().bg(model.palette.surface0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(color))
                    .padding(Padding::horizontal(1))
                    .style(Style::default().bg(model.palette.surface0)),
            ),
            card,
        );
    }
}

fn render_sidebar(model: &Model, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(model.palette.surface1))
        .style(
            Style::default()
                .fg(model.palette.text)
                .bg(model.palette.panel_bg),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let (icon, scope) = if model.sidebar.accordion() {
        ('⌗', "all workspaces".to_owned())
    } else {
        ('⌂', scope_display_name(model.sidebar.current_scope()))
    };
    let scope = truncate_with_ellipsis(scope, usize::from(inner.width).saturating_sub(3));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {icon} "),
                Style::default().fg(model.palette.accent),
            ),
            Span::styled(
                scope,
                Style::default()
                    .fg(model.palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if inner.height >= 2 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(usize::from(inner.width)),
                Style::default().fg(model.palette.surface1),
            )),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }

    let mut display_rows: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    for (index, row) in model.sidebar.rows().iter().enumerate() {
        display_rows.push((Some(index), sidebar_row(model, row, index, inner.width)));
        if matches!(row, Row::Session { .. })
            && model
                .sidebar
                .rows()
                .get(index + 1)
                .is_some_and(|next| matches!(next, Row::Scope { .. }))
        {
            display_rows.push((None, Line::default()));
        }
    }
    if display_rows.is_empty() {
        display_rows.push((
            None,
            Line::from(Span::styled(
                " no sessions yet",
                Style::default().fg(model.palette.overlay0),
            )),
        ));
    }
    let list_height = inner.height.saturating_sub(2) as usize;
    let cursor_row = display_rows
        .iter()
        .position(|(index, _)| *index == Some(model.sidebar.cursor()))
        .unwrap_or_default();
    let top = cursor_row
        .saturating_sub(list_height.saturating_sub(1))
        .min(display_rows.len().saturating_sub(list_height));
    for (row, (_, line)) in display_rows
        .into_iter()
        .skip(top)
        .take(list_height)
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + 2 + row as u16, inner.width, 1),
        );
    }
}

fn sidebar_row(model: &Model, row: &Row, index: usize, width: u16) -> Line<'static> {
    let selected = index == model.sidebar.cursor();
    let gutter = if selected { "▌" } else { " " };
    let gutter_color = if model.focus == Focus::Sidebar {
        model.palette.accent
    } else {
        model.palette.overlay0
    };
    let width = usize::from(width);
    // A selected row is highlighted across the sidebar's full width; the line
    // style supplies the surface so span backgrounds stay untouched.
    let fill = |mut line: Line<'static>| {
        if !selected {
            return line;
        }
        let used = line.width();
        if width > used {
            line.push_span(Span::raw(" ".repeat(width - used)));
        }
        line.style(Style::default().bg(model.palette.surface0))
    };
    match row {
        Row::Scope {
            key,
            count,
            expanded,
        } => {
            let glyph = if *expanded { '▾' } else { '▸' };
            let trailer = count.to_string();
            let trailer_width = trailer.chars().count();
            let name_width = width.saturating_sub(3 + trailer_width).saturating_sub(1);
            let name = truncate_with_ellipsis(scope_display_name(key), name_width);
            let left_width = 3 + Line::from(name.as_str()).width();
            let gap = width.saturating_sub(left_width + trailer_width).max(1);
            fill(Line::from(vec![
                Span::styled(gutter.to_owned(), Style::default().fg(gutter_color)),
                Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(model.palette.overlay1),
                ),
                Span::styled(
                    name,
                    Style::default()
                        .fg(model.palette.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ".repeat(gap)),
                Span::styled(trailer, Style::default().fg(model.palette.overlay1)),
            ]))
        }
        Row::Session { session, .. } => {
            let (glyph, color) = indicator(session, model.spin, &model.palette);
            let ended = session.status == Status::Ended;
            let marker = model.worktree_cwds.contains(&session.cwd);
            let label_max = width.saturating_sub(4 + if marker { 2 } else { 0 });
            let label = truncate_with_ellipsis(session_label(session, &model.tasks), label_max);
            let mut spans = vec![
                Span::styled(gutter.to_owned(), Style::default().fg(gutter_color)),
                Span::raw(" "),
                Span::styled(glyph.to_string(), Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    label,
                    Style::default()
                        .fg(if ended {
                            model.palette.overlay1
                        } else {
                            model.palette.text
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            if marker {
                spans.push(Span::styled(
                    " ⎇",
                    Style::default().fg(model.palette.overlay0),
                ));
            }
            fill(Line::from(spans))
        }
    }
}

fn truncate_with_ellipsis(mut value: String, width: usize) -> String {
    if Line::from(value.as_str()).width() <= width {
        return value;
    }
    if width == 0 {
        return String::new();
    }
    while !value.is_empty() && Line::from(format!("{value}…")).width() > width {
        value.pop();
    }
    format!("{value}…")
}

fn is_linked_worktree(cwd: &Path) -> bool {
    let mut current = cwd;
    loop {
        let git = current.join(".git");
        if git.exists() {
            return git.is_file();
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        current = parent;
    }
}

/// Welcome screen for the pane while no session is attached: the real
/// bindings, centred, instead of a lone grey sentence.
fn render_empty_state(model: &Model, frame: &mut Frame) {
    let pane = model.pane;
    let palette = &model.palette;
    let prefix = key_display(&model.config.effective_prefix());
    if pane.width < 34 || pane.height < 10 {
        frame.render_widget(
            Paragraph::new(format!("no sessions — {prefix} n starts one"))
                .style(Style::default().fg(palette.overlay0)),
            pane,
        );
        return;
    }
    let binding = |id: &str| model.config.bindings.get(id).cloned().unwrap_or_default();
    let entries = [
        (binding("new_claude"), "new claude session"),
        (binding("new_codex"), "new codex session"),
        (binding("new_worktree"), "session in a worktree"),
        (binding("session_history"), "resume a past session"),
        (binding("task_backlog"), "task backlog"),
        ("?".to_owned(), "all commands"),
    ];
    let mut lines = vec![
        Line::from(vec![
            Span::styled("◇ ", Style::default().fg(palette.accent)),
            Span::styled(
                "codebridge",
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::default(),
    ];
    for (key, label) in &entries {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{prefix} {key}"),
                Style::default().fg(palette.accent),
            ),
            Span::styled(format!("   {label}"), Style::default().fg(palette.subtext0)),
        ]));
    }
    let block_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let block_height = lines.len() as u16;
    let rect = Rect::new(
        pane.x + pane.width.saturating_sub(block_width) / 2,
        pane.y + pane.height.saturating_sub(block_height) / 2,
        block_width.min(pane.width),
        block_height.min(pane.height),
    );
    frame.render_widget(Paragraph::new(lines), rect);
}

fn render_terminal(model: &Model, frame: &mut Frame) {
    let Some(terminal) = model.frame.as_ref() else {
        render_empty_state(model, frame);
        return;
    };
    let width = model.pane.width.min(terminal.cols);
    let height = model.pane.height.min(terminal.rows);
    let buffer = frame.buffer_mut();
    for y in 0..height {
        for x in 0..width {
            let source = usize::from(y) * usize::from(terminal.cols) + usize::from(x);
            let Some(cell) = terminal.cells.get(source) else {
                continue;
            };
            let target = &mut buffer[(model.pane.x + x, model.pane.y + y)];
            target.reset();
            target.set_symbol(&cell.symbol);
            target.set_fg(decode_color(cell.fg));
            target.set_bg(decode_color(cell.bg));
            target.set_style(
                Style::default().add_modifier(Modifier::from_bits_truncate(cell.modifiers)),
            );
            let absolute_row = u32::try_from(
                terminal
                    .max_offset
                    .saturating_sub(terminal.offset)
                    .saturating_add(usize::from(y)),
            )
            .unwrap_or(u32::MAX);
            if model.selection.as_ref().is_some_and(|selection| {
                model
                    .attach
                    .as_ref()
                    .is_some_and(|attach| attach.id == selection.session_id)
                    && selection.contains(absolute_row, x)
            }) {
                target.set_bg(model.palette.surface1);
            }
        }
    }
    if model.focus == Focus::Screen
        && !model.scroll_mode
        && terminal.cursor_visible
        && terminal.cursor_x < width
        && terminal.cursor_y < height
    {
        frame.set_cursor_position((
            model.pane.x + terminal.cursor_x,
            model.pane.y + terminal.cursor_y,
        ));
    }
}

fn render_scrollbar(model: &Model, frame: &mut Frame, area: Rect) {
    let Some(terminal) = model.frame.as_ref() else {
        return;
    };
    let total = terminal.max_offset.saturating_add(terminal.rows as usize);
    let thumb_len = ((area.height as usize * terminal.rows as usize) / total.max(1))
        .max(1)
        .min(area.height as usize);
    let travel = area.height as usize - thumb_len;
    let from_top = terminal.max_offset.saturating_sub(terminal.offset);
    let thumb_top = (travel * from_top)
        .checked_div(terminal.max_offset)
        .unwrap_or(travel);
    for y in 0..area.height {
        let active = (thumb_top..thumb_top + thumb_len).contains(&(y as usize));
        frame.buffer_mut()[(area.x, area.y + y)]
            .set_symbol(if active { "┃" } else { "│" })
            .set_fg(if active {
                model.palette.accent
            } else {
                model.palette.surface1
            });
    }
}

fn indicator(session: &SessionInfo, spin: usize, palette: &Palette) -> (char, Color) {
    match session.status {
        Status::Working => (SPINNERS[spin % SPINNERS.len()], palette.green),
        Status::WaitingUser => ('●', palette.green),
        Status::Idle => ('●', palette.yellow),
        Status::Starting => ('…', palette.teal),
        Status::NeedsApproval => ('⚑', palette.red),
        Status::Ended => ('✗', palette.overlay0),
    }
}

fn session_label(session: &SessionInfo, tasks: &[Task]) -> String {
    // An explicit rename always wins over the agent's own summary.
    if !session.name.is_empty() {
        return session.name.clone();
    }
    // Then the agent-summarised conversation title (Claude's `ai-title`,
    // Codex's `thread_name`), resolved by the broker onto this session's live
    // run — the same title the history picker shows.
    if let Some(title) = session_title(tasks, &session.id) {
        return title.to_owned();
    }
    std::path::Path::new(&session.cwd)
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .or_else(|| {
            (!session.id.is_empty()).then(|| session.id.chars().take(8).collect::<String>())
        })
        .unwrap_or_else(|| "session".to_owned())
}

/// The agent-summarised title for a live session, if the broker has resolved
/// one. A run's `cb_session_id` is cleared when it parks, so a non-empty match
/// only ever hits the currently-running run for this session.
fn session_title<'a>(tasks: &'a [Task], session_id: &str) -> Option<&'a str> {
    if session_id.is_empty() {
        return None;
    }
    tasks
        .iter()
        .flat_map(|task| &task.runs)
        .find(|run| run.cb_session_id == session_id)
        .map(|run| run.title.trim())
        .filter(|title| !title.is_empty())
}

fn decode_color(value: u32) -> Color {
    match value {
        0 => Color::Reset,
        1 => Color::Black,
        2 => Color::Red,
        3 => Color::Green,
        4 => Color::Yellow,
        5 => Color::Blue,
        6 => Color::Magenta,
        7 => Color::Cyan,
        8 => Color::Gray,
        9 => Color::DarkGray,
        10 => Color::LightRed,
        11 => Color::LightGreen,
        12 => Color::LightYellow,
        13 => Color::LightBlue,
        14 => Color::LightMagenta,
        15 => Color::LightCyan,
        16 => Color::White,
        value if value & 0xF000_0000 == 0x1000_0000 => Color::Indexed(value as u8),
        value if value & 0xF000_0000 == 0x2000_0000 => Color::Rgb(
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
        ),
        _ => Color::Reset,
    }
}

fn key_name(key: KeyEvent) -> String {
    let base = match key.code {
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => "shift+tab".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pgup".to_owned(),
        KeyCode::PageDown => "pgdown".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        _ => return String::new(),
    };
    let mut modifiers = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        modifiers.push("super");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(key.code, KeyCode::Char(_) | KeyCode::BackTab)
    {
        modifiers.push("shift");
    }
    if modifiers.is_empty() || base.starts_with("shift+") {
        base
    } else {
        format!("{}+{base}", modifiers.join("+"))
    }
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let super_key = key.modifiers.contains(KeyModifiers::SUPER);
    if key.code == KeyCode::Enter && shift {
        return Some(b"\x1b[13;2u".to_vec());
    }
    if key.code == KeyCode::Tab && shift {
        return Some(b"\x1b[Z".to_vec());
    }
    let only_alt = alt && !ctrl && !shift && !super_key;
    if only_alt {
        match key.code {
            KeyCode::Left => return Some(b"\x1bb".to_vec()),
            KeyCode::Right => return Some(b"\x1bf".to_vec()),
            _ => {}
        }
    }
    if super_key && !alt && !ctrl && !shift {
        match key.code {
            KeyCode::Left => return Some(vec![0x01]),
            KeyCode::Right => return Some(vec![0x05]),
            _ => {}
        }
    }
    // A Cmd/Super-modified character (Cmd+C, Cmd+V, Cmd+A, …) is a host- or
    // terminal-level shortcut, never literal text. Swallow it so the bare
    // character can't leak into the agent's input box.
    if super_key && matches!(key.code, KeyCode::Char(_)) {
        return None;
    }
    if let Some(parameter) = xterm_modifier_parameter(key.modifiers) {
        let sequence = match key.code {
            KeyCode::Up => Some(format!("\x1b[1;{parameter}A")),
            KeyCode::Down => Some(format!("\x1b[1;{parameter}B")),
            KeyCode::Right => Some(format!("\x1b[1;{parameter}C")),
            KeyCode::Left => Some(format!("\x1b[1;{parameter}D")),
            KeyCode::Home => Some(format!("\x1b[1;{parameter}H")),
            KeyCode::End => Some(format!("\x1b[1;{parameter}F")),
            KeyCode::Insert => Some(format!("\x1b[2;{parameter}~")),
            KeyCode::Delete => Some(format!("\x1b[3;{parameter}~")),
            KeyCode::PageUp => Some(format!("\x1b[5;{parameter}~")),
            KeyCode::PageDown => Some(format!("\x1b[6;{parameter}~")),
            _ => None,
        };
        if let Some(sequence) = sequence {
            return Some(sequence.into_bytes());
        }
    }
    let mut bytes = match key.code {
        KeyCode::Char(character) if ctrl => ctrl_byte(character).map(|byte| vec![byte])?,
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        _ => return None,
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn ctrl_byte(character: char) -> Option<u8> {
    match character {
        'a'..='z' => Some(character as u8 - b'a' + 1),
        'A'..='Z' => Some(character as u8 - b'A' + 1),
        ' ' | '@' => Some(0),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

fn xterm_modifier_parameter(modifiers: KeyModifiers) -> Option<u8> {
    let mut bits = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        bits |= 8;
    }
    (bits != 0).then_some(bits + 1)
}

fn write_json(writer: &mut impl Write, value: &impl serde::Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_wire_format_round_trips_rgb_and_palette() {
        assert_eq!(decode_color(0x1000_00f0), Color::Indexed(0xf0));
        assert_eq!(decode_color(0x2012_3456), Color::Rgb(0x12, 0x34, 0x56));
    }

    #[test]
    fn view_reserves_header_and_footer_around_the_pane() {
        let area = Rect::new(0, 0, 120, 40);
        let live = view(false, area);
        // Footer spans the full width on the last row; header tops the main
        // column; the pane fills the rest of the main column.
        assert_eq!(live.footer, Some(Rect::new(0, 39, 120, 1)));
        assert_eq!(live.header, Some(Rect::new(SIDEBAR_WIDTH, 0, 90, 1)));
        assert_eq!(live.pane, Rect::new(SIDEBAR_WIDTH, 1, 90, 38));
        assert_eq!(live.sidebar, Rect::new(0, 0, SIDEBAR_WIDTH, 39));
        assert!(live.scrollbar.is_none());

        // Scrollback claims the pane's rightmost column, below the header.
        let scrolled = view(true, area);
        assert_eq!(scrolled.pane.width, 89);
        assert_eq!(scrolled.scrollbar, Some(Rect::new(119, 1, 1, 38)));
    }

    #[test]
    fn view_degrades_gracefully_on_tiny_terminals() {
        // Too short for any chrome: the pane takes everything.
        let tiny = view(false, Rect::new(0, 0, 80, 3));
        assert!(tiny.footer.is_none());
        assert!(tiny.header.is_none());
        assert_eq!(tiny.pane, Rect::new(SIDEBAR_WIDTH, 0, 50, 3));

        // Four rows fit a header but not yet a footer.
        let short = view(false, Rect::new(0, 0, 80, 4));
        assert!(short.footer.is_none());
        assert!(short.header.is_some());
        assert_eq!(short.pane, Rect::new(SIDEBAR_WIDTH, 1, 50, 3));
    }

    fn test_model() -> Model {
        Model {
            sidebar: Sidebar::new(Path::new("/tmp")),
            launch_cwd: PathBuf::from("/tmp"),
            focus: Focus::Sidebar,
            prefix: false,
            help: false,
            scroll_mode: false,
            frame: None,
            attach: None,
            error: String::new(),
            previous_status: HashMap::new(),
            pending_notifications: HashMap::new(),
            outer_focused: true,
            worktree_cwds: Default::default(),
            hooks_ok: true,
            toasts: Vec::new(),
            rename: None,
            worktree_picker: None,
            config: Config::default(),
            palette: Palette::catppuccin(),
            config_menu: None,
            selection: None,
            last_click: None,
            word_selecting: false,
            tasks: Vec::new(),
            task_modal: None,
            history_modal: None,
            pending_jump: None,
            spin: 0,
            pane: Rect::default(),
            screen: Rect::default(),
        }
    }

    /// Drive `render` across every chrome surface — bars, empty state, live
    /// session, and each modal — so a geometry regression panics here rather
    /// than in a live terminal.
    #[test]
    fn chrome_renders_every_surface_without_panicking() {
        use ratatui::backend::TestBackend;

        let mut model = test_model();
        let area = Rect::new(0, 0, 100, 30);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut draw = |model: &mut Model| {
            compute_view(model, area);
            terminal.draw(|frame| render(model, frame)).unwrap();
        };

        // Empty state with header/footer bars.
        draw(&mut model);

        // A live attached session with scrollback drives the header session
        // path, sidebar rows, terminal cells, and the scrollbar.
        let session = SessionInfo {
            id: "abcd1234-rest".to_owned(),
            name: String::new(),
            argv: vec!["claude".to_owned()],
            cwd: "/tmp".to_owned(),
            status: Status::Working,
            last_message: String::new(),
            harness_session_id: String::new(),
            exited: false,
            status_since_unix_ms: 0,
            transcript_path: String::new(),
        };
        model.sidebar.update(vec![session]);
        let (stream, _peer) = UnixStream::pair().unwrap();
        model.attach = Some(Attach {
            id: "abcd1234-rest".to_owned(),
            writer: BufWriter::new(stream),
        });
        model.frame = Some(TerminalFrame {
            rows: 5,
            cols: 10,
            cells: Vec::new(),
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            mouse_reporting: false,
            offset: 3,
            max_offset: 40,
        });
        draw(&mut model);

        // Footer variants: error, armed prefix, scroll mode, hook warning.
        model.error = "broker unreachable".to_owned();
        draw(&mut model);
        model.error.clear();
        model.prefix = true;
        draw(&mut model);
        model.prefix = false;
        model.scroll_mode = true;
        draw(&mut model);
        model.scroll_mode = false;
        model.hooks_ok = false;
        draw(&mut model);
        model.hooks_ok = true;

        // Every modal panel.
        model.history_modal = Some(HistoryModal { cursor: 0 });
        draw(&mut model);
        model.history_modal = None;
        model.task_modal = Some(TaskModal {
            stage: TaskStage::List,
            cursor: 0,
        });
        draw(&mut model);
        model.task_modal = Some(TaskModal {
            stage: TaskStage::New {
                title: "ship it".to_owned(),
                desc: "details".to_owned(),
                title_active: true,
            },
            cursor: 0,
        });
        draw(&mut model);
        model.task_modal = None;
        model.rename = Some(Rename {
            id: "abcd".to_owned(),
            input: "new name".to_owned(),
        });
        draw(&mut model);
        model.rename = None;
        for (theme_cursor, notification_cursor) in [(None, None), (Some(2), None), (None, Some(1))]
        {
            model.config_menu = Some(ConfigMenu {
                cursor: 0,
                capture: false,
                error: String::new(),
                theme_cursor,
                notification_cursor,
                original_theme: None,
            });
            draw(&mut model);
        }
        model.config_menu = None;
        model.toasts.push(Toast {
            session_id: "abcd1234-rest".to_owned(),
            status: Status::NeedsApproval,
            text: "approve command?".to_owned(),
        });
        draw(&mut model);

        // A terminal too small for any chrome still renders.
        let tiny_area = Rect::new(0, 0, 10, 2);
        let mut tiny = Terminal::new(TestBackend::new(tiny_area.width, tiny_area.height)).unwrap();
        compute_view(&mut model, tiny_area);
        tiny.draw(|frame| render(&model, frame)).unwrap();
    }

    #[test]
    fn key_display_compacts_ctrl_bindings() {
        assert_eq!(key_display("ctrl+a"), "^a");
        assert_eq!(key_display("enter"), "enter");
        assert_eq!(key_display("["), "[");
    }

    #[test]
    fn shorten_path_prefers_home_and_keeps_the_tail() {
        assert_eq!(shorten_path("/home/dev", Some("/home/dev"), 20), "~");
        assert_eq!(
            shorten_path("/home/dev/src/app", Some("/home/dev"), 20),
            "~/src/app"
        );
        assert_eq!(
            shorten_path("/home/devotion", Some("/home/dev"), 20),
            "/home/devotion"
        );
        assert_eq!(
            shorten_path("/home/dev/very/deep/nested/dir", Some("/home/dev"), 12),
            "…/nested/dir"
        );
        assert_eq!(shorten_path("/x", None, 0), "");
    }

    #[test]
    fn key_encoding_keeps_agent_input_raw() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::SHIFT | KeyModifiers::CONTROL,
            )),
            Some(b"\x1b[1;6D".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            Some(vec![0x1d])
        );
        // Cmd/Super shortcuts must never leak their bare character into the
        // agent PTY (e.g. Cmd+C appending "c" while copying a selection).
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER)),
            None
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER)),
            None
        );
    }

    #[test]
    fn toast_cards_stack_bottom_left_newest_nearest_corner() {
        let toast = |id: &str| Toast {
            session_id: id.to_owned(),
            status: Status::WaitingUser,
            text: "● session — turn completed".to_owned(),
        };
        let toasts = vec![toast("older"), toast("newest")];
        let area = Rect::new(0, 0, 100, 40);
        let cards = toast_cards(&toasts, area);
        assert_eq!(cards.len(), 2);

        // Newest (last pushed, index 1) sits nearest the bottom-left corner.
        let (newest_index, newest) = cards[0];
        assert_eq!(newest_index, 1);
        assert_eq!(newest.bottom(), area.bottom() - TOAST_MARGIN_Y);
        assert_eq!(newest.x, area.x + TOAST_MARGIN_X);
        assert_eq!(newest.height, TOAST_HEIGHT);

        // Older toast stacks directly above it with the configured gap.
        let (older_index, older) = cards[1];
        assert_eq!(older_index, 0);
        assert_eq!(older.x, area.x + TOAST_MARGIN_X);
        assert_eq!(older.bottom(), newest.y - TOAST_GAP);
    }

    #[test]
    fn toast_card_width_caps_and_hit_test_matches_render() {
        use ratatui::backend::TestBackend;

        let long = "●".to_owned() + &" verbose session status message".repeat(6);
        let toasts = vec![Toast {
            session_id: "abc".to_owned(),
            status: Status::NeedsApproval,
            text: long,
        }];
        let area = Rect::new(0, 0, 120, 30);
        let cards = toast_cards(&toasts, area);
        let (_, card) = cards[0];
        assert!(card.width <= TOAST_MAX_WIDTH);

        // Render into a test buffer and confirm the rounded border is drawn at
        // the card corners (i.e. the toast is an actual bordered card).
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Clear, card);
                frame.render_widget(
                    Paragraph::new("x").block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded),
                    ),
                    card,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(card.x, card.y)].symbol(), "╭");
        assert_eq!(buffer[(card.right() - 1, card.bottom() - 1)].symbol(), "╯");
    }

    #[test]
    fn transition_toasts_ignore_first_observation_and_stay_status_driven() {
        let session = |status, message: &str| SessionInfo {
            id: "12345678-rest".to_owned(),
            name: String::new(),
            argv: vec!["claude".to_owned()],
            cwd: "/tmp".to_owned(),
            status,
            last_message: message.to_owned(),
            harness_session_id: String::new(),
            exited: false,
            status_since_unix_ms: 0,
            transcript_path: String::new(),
        };
        let mut previous = HashMap::new();
        assert!(transition_toasts(&mut previous, &[session(Status::Working, "")], &[]).is_empty());
        let approval = transition_toasts(
            &mut previous,
            &[session(Status::NeedsApproval, "approve command?")],
            &[],
        );
        assert_eq!(approval.len(), 1);
        assert_eq!(approval[0].status, Status::NeedsApproval);
        assert!(approval[0].text.contains("approve command?"));
        let waiting = transition_toasts(&mut previous, &[session(Status::WaitingUser, "")], &[]);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].status, Status::WaitingUser);
    }

    #[test]
    fn session_label_prefers_rename_then_agent_title_then_cwd() {
        let run = |cb_session_id: &str, title: &str| crate::task::TaskRun {
            id: "run".to_owned(),
            agent: "claude".to_owned(),
            cwd: String::new(),
            cb_session_id: cb_session_id.to_owned(),
            agent_session_id: String::new(),
            first_message: String::new(),
            transcript_path: String::new(),
            title: title.to_owned(),
            status: TaskStatus::InProgress,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let task = crate::task::Task {
            id: "task".to_owned(),
            scope: String::new(),
            title: String::new(),
            desc: String::new(),
            status: TaskStatus::InProgress,
            runs: vec![run("sess", "Fix the login bug")],
            auto: true,
            agent: "claude".to_owned(),
            cwd: String::new(),
            cb_session_id: String::new(),
            agent_session_id: String::new(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let base = |name: &str| SessionInfo {
            id: "sess".to_owned(),
            name: name.to_owned(),
            argv: vec!["claude".to_owned()],
            cwd: "/tmp/my-project".to_owned(),
            status: Status::Idle,
            last_message: String::new(),
            harness_session_id: String::new(),
            exited: false,
            status_since_unix_ms: 0,
            transcript_path: String::new(),
        };
        let tasks = std::slice::from_ref(&task);
        // An explicit rename wins over everything.
        assert_eq!(session_label(&base("Renamed"), tasks), "Renamed");
        // No rename -> the agent-summarised title.
        assert_eq!(session_label(&base(""), tasks), "Fix the login bug");
        // No matching live run -> fall back to the cwd basename.
        assert_eq!(session_label(&base(""), &[]), "my-project");
    }

    #[test]
    fn notification_channels_follow_delivery_and_focus() {
        use crate::notify::Delivery;

        assert_eq!(
            notification_channels(Delivery::All, true, false, true),
            (true, true)
        );
        assert_eq!(
            notification_channels(Delivery::All, true, true, true),
            (false, false)
        );
        assert_eq!(
            notification_channels(Delivery::All, true, true, false),
            (false, true)
        );
        assert_eq!(
            notification_channels(Delivery::Codebridge, true, false, true),
            (true, false)
        );
        assert_eq!(
            notification_channels(Delivery::Terminal, true, false, true),
            (false, true)
        );
        assert_eq!(
            notification_channels(Delivery::Off, false, false, false),
            (false, false)
        );
    }

    #[test]
    fn toasts_scoped_to_current_workspace_unless_accordion() {
        let here = crate::sidebar::scope_key("/tmp");
        let elsewhere = crate::sidebar::scope_key("/");
        assert_ne!(
            here, elsewhere,
            "test paths must resolve to distinct scopes"
        );

        // Flat view: only the current workspace notifies.
        assert!(toast_in_scope(false, &here, Some("/tmp")));
        assert!(!toast_in_scope(false, &here, Some("/")));
        // A session already gone from the snapshot has no cwd and never notifies.
        assert!(!toast_in_scope(false, &here, None));

        // Accordion (global view) notifies regardless of scope.
        assert!(toast_in_scope(true, &here, Some("/")));
        assert!(toast_in_scope(true, &here, None));
    }

    #[test]
    fn selection_uses_absolute_rows_and_reading_order() {
        let selection = Selection {
            session_id: "session".to_owned(),
            anchor: (12, 8),
            cursor: (10, 4),
            dragging: true,
        };
        assert_eq!(selection.ordered(), ((10, 4), (12, 8)));
        assert!(selection.contains(10, 4));
        assert!(selection.contains(11, 0));
        assert!(selection.contains(12, 8));
        assert!(!selection.contains(10, 3));
        assert!(!selection.contains(12, 9));
    }

    #[test]
    fn char_class_keeps_path_and_flag_chars_in_words() {
        assert_eq!(char_class("a"), CharClass::Word);
        assert_eq!(char_class("7"), CharClass::Word);
        assert_eq!(char_class("_"), CharClass::Word);
        assert_eq!(char_class("-"), CharClass::Word);
        assert_eq!(char_class("."), CharClass::Word);
        assert_eq!(char_class("/"), CharClass::Word);
        assert_eq!(char_class(" "), CharClass::Space);
        assert_eq!(char_class(""), CharClass::Space);
        assert_eq!(char_class("="), CharClass::Other);
        assert_eq!(char_class(">"), CharClass::Other);
        // A multi-codepoint grapheme is treated as one word cell.
        assert_eq!(char_class("👍🏽"), CharClass::Word);
    }

    fn classes(row: &str) -> Vec<CharClass> {
        row.chars().map(|c| char_class(&c.to_string())).collect()
    }

    #[test]
    fn word_run_extends_over_same_class() {
        // "cd src/main.rs" — path chars stay one word.
        let row = classes("cd src/main.rs");
        assert_eq!(word_run(&row, 0), Some((0, 1))); // "cd"
        assert_eq!(word_run(&row, 5), Some((3, 13))); // "src/main.rs"
        assert_eq!(word_run(&row, 13), Some((3, 13))); // last char of the path
        assert_eq!(word_run(&row, 2), None); // the space

        // A run of the same "other" punctuation selects together.
        let arrow = classes("a => b");
        assert_eq!(word_run(&arrow, 2), Some((2, 3))); // "=>"

        // Single-cell word at the very end.
        let tail = classes("go");
        assert_eq!(word_run(&tail, 1), Some((0, 1)));
    }
}
