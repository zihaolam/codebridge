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

/// One resumable past session, flattened from a task run for the historical
/// picker. `first_message` is the human-readable label.
struct HistoryEntry {
    task_id: String,
    run_id: String,
    agent: String,
    first_message: String,
    auto: bool,
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
                    if model.focus == Focus::Screen {
                        dismiss_attached_toast(&mut model);
                    }
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

fn start_watch(sender: Sender<UiEvent>) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path())?;
    write_json(
        &mut stream,
        &Request {
            kind: "watch".to_owned(),
            ..Request::default()
        },
    )?;
    thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            match line
                .ok()
                .and_then(|line| serde_json::from_str::<Response>(&line).ok())
            {
                Some(response) if response.ok => {
                    if sender
                        .send(UiEvent::Snapshot(response.sessions, response.tasks))
                        .is_err()
                    {
                        break;
                    }
                }
                Some(response) => {
                    let _ = sender.send(UiEvent::Error(response.error));
                }
                None => break,
            }
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
                detect_transitions(model, &sessions);
                model.worktree_cwds = sessions
                    .iter()
                    .filter(|session| is_linked_worktree(Path::new(&session.cwd)))
                    .map(|session| session.cwd.clone())
                    .collect();
                model.sidebar.update(sessions);
                model.tasks = tasks;
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
    let transitions = transition_toasts(&mut model.previous_status, sessions);
    for toast in transitions {
        let allowed = match toast.status {
            Status::NeedsApproval => model.config.notifications.notify_approval,
            Status::WaitingUser => model.config.notifications.notify_done,
            _ => false,
        };
        if !allowed {
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
            session_label(session),
            scope_display_name(&session.cwd)
        ),
    )
}

fn transition_toasts(
    previous: &mut HashMap<String, Status>,
    sessions: &[SessionInfo],
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
                        text: format!("⚑ {} — {detail}", session_label(session)),
                    });
                }
                Status::WaitingUser => toasts.push(Toast {
                    session_id: session.id.clone(),
                    status: Status::WaitingUser,
                    text: format!("● {} — turn completed", session_label(session)),
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
        // `Left` still focuses the sidebar; `h` falls through to the action
        // table so it can drive `session_history` (or any user rebinding).
        KeyCode::Left => {
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
                dismiss_attached_toast(model);
            }
        }
        Some("rename") => {
            let target = model
                .selected()
                .or_else(|| model.attached_session())
                .map(|session| (session.id.clone(), session_label(session)));
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

/// Paused (resumable) runs in the current workspace scope, most recent first.
/// These are the "historical sessions" surfaced by the `session_history`
/// action — both ad-hoc auto sessions and paused task runs.
fn history_entries(model: &Model) -> Vec<HistoryEntry> {
    let scope = model.sidebar.current_scope();
    let mut entries: Vec<HistoryEntry> = model
        .tasks
        .iter()
        .filter(|task| task.scope == scope)
        .flat_map(|task| {
            task.runs
                .iter()
                .filter(|run| run.status == TaskStatus::Paused)
                .map(|run| HistoryEntry {
                    task_id: task.id.clone(),
                    run_id: run.id.clone(),
                    agent: run.agent.clone(),
                    first_message: run.first_message.clone(),
                    auto: task.auto,
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
                keep_open = false;
            }
        }
        KeyCode::Char('x') => {
            // Only auto sessions are deletable here; real backlog tasks are
            // managed from the task modal, so leave those untouched.
            if let Some(entry) = entries.get(modal.cursor).filter(|entry| entry.auto) {
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
            update_selection_drag(model, mouse.column, mouse.row)?;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            update_selection_drag(model, mouse.column, mouse.row)?;
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
const TOAST_GAP: u16 = 1;
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
        let inner = (toast.text.chars().count() as u16).min(inner_cap).max(1);
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
    main: Rect,
    scrollbar: Option<Rect>,
}

fn view(model: &Model, area: Rect) -> View {
    let [sidebar, main] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)])
        .areas(area);
    let has_scrollbar = model
        .frame
        .as_ref()
        .is_some_and(|terminal| terminal.max_offset > 0 && main.width > 1);
    View {
        sidebar,
        main,
        scrollbar: has_scrollbar.then(|| Rect::new(main.right() - 1, main.y, 1, main.height)),
    }
}

fn compute_view(model: &mut Model, area: Rect) {
    let view = view(model, area);
    model.screen = area;
    model.pane = Rect {
        width: view
            .main
            .width
            .saturating_sub(u16::from(view.scrollbar.is_some())),
        ..view.main
    };
}

fn render(model: &Model, frame: &mut Frame) {
    let view = view(model, frame.area());
    render_sidebar(model, frame, view.sidebar);

    render_terminal(model, frame);
    if let Some(scrollbar) = view.scrollbar {
        render_scrollbar(model, frame, scrollbar);
    }
    if !model.error.is_empty() && frame.area().height > 0 {
        let area = Rect::new(0, frame.area().bottom() - 1, frame.area().width, 1);
        frame.render_widget(
            Paragraph::new(model.error.clone()).style(Style::default().fg(model.palette.red)),
            area,
        );
    }
    render_overlays(model, frame);
}

fn render_overlays(model: &Model, frame: &mut Frame) {
    let area = frame.area();
    if let Some(modal) = model.history_modal.as_ref() {
        let entries = history_entries(model);
        let mut lines = Vec::new();
        if entries.is_empty() {
            lines.push(Line::styled(
                "no past sessions in this workspace",
                Style::default().fg(model.palette.overlay0),
            ));
        }
        for (cursor, entry) in entries.iter().enumerate() {
            let selected = cursor == modal.cursor;
            let label = entry.first_message.replace(['\n', '\t'], " ");
            let label = label.trim();
            let label: String = if label.is_empty() {
                "(no message yet)".to_owned()
            } else {
                label.chars().take(64).collect()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "▌ " } else { "  " },
                    Style::default().fg(model.palette.accent),
                ),
                Span::styled(
                    format!("{:<8} ", entry.agent),
                    Style::default().fg(model.palette.overlay1),
                ),
                Span::raw(label),
            ]));
        }
        lines.push(Line::styled(
            "enter resume · x delete · esc close",
            Style::default().fg(model.palette.overlay0),
        ));
        let title = format!(
            "history — {}",
            scope_display_name(model.sidebar.current_scope())
        );
        let height = (lines.len() as u16 + 2).min(area.height).max(3);
        let width = area.width.saturating_sub(4).clamp(1, 82);
        let panel = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Paragraph::new(lines)
                .style(
                    Style::default()
                        .fg(model.palette.text)
                        .bg(model.palette.panel_bg),
                )
                .block(
                    Block::default()
                        .title(format!(" {title} "))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(model.palette.accent)),
                ),
            panel,
        );
        return;
    }
    if let Some(modal) = model.task_modal.as_ref() {
        let mut lines = Vec::new();
        let title = match &modal.stage {
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
                    lines.push(Line::from(vec![
                        Span::styled(
                            if cursor == modal.cursor { "▌ " } else { "  " },
                            Style::default().fg(model.palette.accent),
                        ),
                        Span::styled(glyph.to_string(), Style::default().fg(color)),
                        Span::raw(format!(
                            " {}{}",
                            task.title,
                            if task.runs.is_empty() {
                                String::new()
                            } else {
                                format!("  {} session(s)", task.runs.len())
                            }
                        )),
                    ]));
                }
                lines.push(Line::styled(
                    "n new · enter open · e edit · s start · r resume · K sessions · c done · x delete",
                    Style::default().fg(model.palette.overlay0),
                ));
                format!(
                    "tasks — {}",
                    scope_display_name(model.sidebar.current_scope())
                )
            }
            TaskStage::New {
                title,
                desc,
                title_active,
            } => {
                lines.push(Line::styled(
                    "title",
                    Style::default().fg(model.palette.overlay0),
                ));
                lines.push(Line::from(format!(
                    "{title}{}",
                    if *title_active { "▎" } else { "" }
                )));
                lines.push(Line::styled(
                    "description",
                    Style::default().fg(model.palette.overlay0),
                ));
                lines.extend(desc.lines().map(|line| Line::from(line.to_owned())));
                if !*title_active {
                    lines.push(Line::from("▎"));
                }
                lines.push(Line::styled(
                    "tab switch · ctrl+enter add · esc cancel",
                    Style::default().fg(model.palette.overlay0),
                ));
                "new task".to_owned()
            }
            TaskStage::Detail {
                title,
                desc,
                title_active,
                ..
            } => {
                lines.push(Line::styled(
                    "title",
                    Style::default().fg(model.palette.overlay0),
                ));
                lines.push(Line::from(format!(
                    "{title}{}",
                    if *title_active { "▎" } else { "" }
                )));
                lines.push(Line::styled(
                    "description",
                    Style::default().fg(model.palette.overlay0),
                ));
                lines.extend(desc.lines().map(|line| Line::from(line.to_owned())));
                if !*title_active {
                    lines.push(Line::from("▎"));
                }
                lines.push(Line::styled(
                    "tab switch · esc save",
                    Style::default().fg(model.palette.overlay0),
                ));
                "edit task".to_owned()
            }
            TaskStage::Agent { cursor, .. } => {
                for (index, agent) in worktree::available_agents().iter().enumerate() {
                    lines.push(Line::from(format!(
                        "{} {}",
                        if index == *cursor { "▌" } else { " " },
                        agent.label
                    )));
                }
                lines.push(Line::styled(
                    "enter start · esc back",
                    Style::default().fg(model.palette.overlay0),
                ));
                "choose task agent".to_owned()
            }
            TaskStage::Runs { id, cursor } => {
                if let Some(task) = model.tasks.iter().find(|task| task.id == *id) {
                    for (index, run) in task.runs.iter().enumerate() {
                        lines.push(Line::from(format!(
                            "{} {} · {:?} · {}",
                            if index == *cursor { "▌" } else { " " },
                            run.agent,
                            run.status,
                            short_id(&run.cb_session_id)
                        )));
                    }
                }
                lines.push(Line::styled(
                    "enter jump · x kill · esc back",
                    Style::default().fg(model.palette.overlay0),
                ));
                "task sessions".to_owned()
            }
        };
        let height = (lines.len() as u16 + 2).min(area.height).max(3);
        let width = area.width.saturating_sub(4).clamp(1, 82);
        let panel = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Paragraph::new(lines)
                .style(
                    Style::default()
                        .fg(model.palette.text)
                        .bg(model.palette.panel_bg),
                )
                .block(
                    Block::default()
                        .title(format!(" {title} "))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(model.palette.accent)),
                ),
            panel,
        );
        return;
    }
    if let Some(menu) = model.config_menu.as_ref() {
        if let Some(cursor) = menu.theme_cursor {
            let mut lines = vec![Line::styled(
                "↑↓ preview · enter apply · esc cancel",
                Style::default().fg(model.palette.overlay0),
            )];
            let visible = usize::from(area.height.saturating_sub(3).max(1));
            let start = cursor
                .saturating_sub(visible.saturating_sub(1))
                .min(THEME_NAMES.len().saturating_sub(visible));
            lines.extend(
                THEME_NAMES
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(visible)
                    .map(|(index, name)| {
                        let selected = index == cursor;
                        Line::styled(
                            format!("{} {name}", if selected { "▌" } else { " " }),
                            Style::default()
                                .fg(if selected {
                                    model.palette.text
                                } else {
                                    model.palette.subtext0
                                })
                                .bg(if selected {
                                    model.palette.surface0
                                } else {
                                    model.palette.panel_bg
                                })
                                .add_modifier(if selected {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        )
                    }),
            );
            let height = (lines.len() as u16 + 2).min(area.height).max(3);
            let width = area.width.saturating_sub(4).clamp(1, 42);
            let panel = Rect::new(
                area.x + area.width.saturating_sub(width) / 2,
                area.y + area.height.saturating_sub(height) / 2,
                width,
                height,
            );
            frame.render_widget(Clear, panel);
            frame.render_widget(
                Paragraph::new(lines)
                    .style(
                        Style::default()
                            .fg(model.palette.text)
                            .bg(model.palette.panel_bg),
                    )
                    .block(
                        Block::default()
                            .title(" choose theme ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(model.palette.accent)),
                    ),
                panel,
            );
            return;
        }
        if let Some(cursor) = menu.notification_cursor {
            let mut lines = vec![Line::styled(
                "↑↓ select · enter apply · esc cancel",
                Style::default().fg(model.palette.overlay0),
            )];
            lines.extend(
                crate::notify::DELIVERY_NAMES
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
                        Line::styled(
                            format!(
                                "{} {name:<12} {description}",
                                if selected { "▌" } else { " " }
                            ),
                            Style::default()
                                .fg(if selected {
                                    model.palette.text
                                } else {
                                    model.palette.subtext0
                                })
                                .bg(if selected {
                                    model.palette.surface0
                                } else {
                                    model.palette.panel_bg
                                })
                                .add_modifier(if selected {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        )
                    }),
            );
            let height = (lines.len() as u16 + 2).min(area.height).max(3);
            let width = area.width.saturating_sub(4).clamp(1, 58);
            let panel = Rect::new(
                area.x + area.width.saturating_sub(width) / 2,
                area.y + area.height.saturating_sub(height) / 2,
                width,
                height,
            );
            frame.render_widget(Clear, panel);
            frame.render_widget(
                Paragraph::new(lines)
                    .style(
                        Style::default()
                            .fg(model.palette.text)
                            .bg(model.palette.panel_bg),
                    )
                    .block(
                        Block::default()
                            .title(" notification delivery ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(model.palette.accent)),
                    ),
                panel,
            );
            return;
        }
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
        let mut lines = vec![Line::styled(
            "enter edits · theme previews live · saved automatically",
            Style::default().fg(model.palette.overlay0),
        )];
        lines.extend(rows.into_iter().enumerate().map(|(index, (label, value))| {
            let value = if menu.capture && menu.cursor == index {
                "[press key · esc cancel]".to_owned()
            } else {
                value
            };
            Line::styled(
                format!(
                    "{} {label:<30} {value}",
                    if index == menu.cursor { "▌" } else { " " }
                ),
                Style::default()
                    .fg(if index == menu.cursor {
                        model.palette.text
                    } else {
                        model.palette.subtext0
                    })
                    .bg(if index == menu.cursor {
                        model.palette.surface0
                    } else {
                        model.palette.panel_bg
                    })
                    .add_modifier(if index == menu.cursor {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )
        }));
        if !menu.error.is_empty() {
            lines.push(Line::styled(
                menu.error.clone(),
                Style::default().fg(model.palette.red),
            ));
        }
        let height = (lines.len() as u16 + 2).min(area.height).max(3);
        let width = area.width.saturating_sub(4).clamp(1, 66);
        let panel = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Paragraph::new(lines)
                .style(
                    Style::default()
                        .fg(model.palette.text)
                        .bg(model.palette.panel_bg),
                )
                .block(
                    Block::default()
                        .title(" codebridge config ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(model.palette.accent)),
                ),
            panel,
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
        let height = (choices.len() as u16 + 4).min(area.height).max(3);
        let width = area.width.saturating_sub(4).clamp(1, 60);
        let panel = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let lines = std::iter::once(Line::styled(
            subtitle.to_owned(),
            Style::default().fg(model.palette.overlay0),
        ))
        .chain(choices.into_iter().enumerate().map(|(index, choice)| {
            Line::styled(
                format!("{} {choice}", if index == cursor { "▌" } else { " " }),
                Style::default()
                    .fg(if index == cursor {
                        model.palette.text
                    } else {
                        model.palette.subtext0
                    })
                    .bg(if index == cursor {
                        model.palette.surface0
                    } else {
                        model.palette.panel_bg
                    })
                    .add_modifier(if index == cursor {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )
        }))
        .collect::<Vec<_>>();
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Paragraph::new(lines)
                .style(
                    Style::default()
                        .fg(model.palette.text)
                        .bg(model.palette.panel_bg),
                )
                .block(
                    Block::default()
                        .title(format!(" {title} "))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(model.palette.accent)),
                ),
            panel,
        );
        return;
    }
    if let Some(rename) = model.rename.as_ref() {
        let width = area.width.saturating_sub(4).clamp(1, 54);
        let prompt = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(3) / 2,
            width,
            3.min(area.height),
        );
        frame.render_widget(Clear, prompt);
        frame.render_widget(
            Paragraph::new(rename.input.clone())
                .block(
                    Block::default()
                        .title(" rename ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(model.palette.accent)),
                )
                .style(
                    Style::default()
                        .fg(model.palette.text)
                        .bg(model.palette.panel_bg),
                ),
            prompt,
        );
        return;
    }
    if model.prefix || model.help {
        let mut lines = vec![Line::styled(
            format!("prefix = {}", model.config.effective_prefix()),
            Style::default().fg(model.palette.overlay0),
        )];
        let actions = crate::config::ACTIONS;
        for pair in actions.chunks(2) {
            let text = pair
                .iter()
                .map(|action| {
                    format!(
                        "{:>8}  {:<24}",
                        model.config.bindings[action.id], action.label
                    )
                })
                .collect::<Vec<_>>()
                .join("  ");
            lines.push(Line::from(text));
        }
        lines.push(Line::styled(
            "h/← sidebar  → screen  ? close",
            Style::default().fg(model.palette.overlay0),
        ));
        let height = (lines.len() as u16 + 2).min(area.height).max(3);
        let width = area.width.saturating_sub(4).clamp(1, 78);
        let panel = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.bottom().saturating_sub(height + 1),
            width,
            height,
        );
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Paragraph::new(lines)
                .style(
                    Style::default()
                        .fg(model.palette.text)
                        .bg(model.palette.panel_bg),
                )
                .block(
                    Block::default()
                        .title(" prefix commands ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(model.palette.accent)),
                ),
            panel,
        );
        return;
    }
    if !model.hooks_ok && area.width > 2 && area.height > 2 {
        let warning = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
        frame.render_widget(Clear, warning);
        frame.render_widget(
            Paragraph::new("⚠ hooks not installed — run: cb install-hooks").style(
                Style::default()
                    .fg(model.palette.peach)
                    .add_modifier(Modifier::BOLD),
            ),
            warning,
        );
    }
    for (index, card) in toast_cards(&model.toasts, model.screen) {
        let toast = &model.toasts[index];
        let color = match toast.status {
            Status::NeedsApproval => model.palette.red,
            Status::WaitingUser => model.palette.green,
            _ => model.palette.text,
        };
        let text = truncate_ellipsis(&toast.text, card.width.saturating_sub(4) as usize);
        frame.render_widget(Clear, card);
        frame.render_widget(
            Paragraph::new(Line::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ))
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
        .border_style(Style::default().fg(model.palette.overlay0))
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

    let scope = if model.sidebar.accordion() {
        "scope: all".to_owned()
    } else {
        format!(
            "scope: {}",
            scope_display_name(model.sidebar.current_scope())
        )
    };
    frame.render_widget(
        Paragraph::new(scope).style(Style::default().fg(model.palette.overlay0)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

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
                " no sessions",
                Style::default().fg(model.palette.overlay0),
            )),
        ));
    }
    let list_height = inner.height.saturating_sub(3) as usize;
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
    if inner.height >= 2 {
        frame.render_widget(
            Paragraph::new(status_counts(model)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
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
    match row {
        Row::Scope {
            key,
            count,
            expanded,
        } => {
            let glyph = if *expanded { '▾' } else { '▸' };
            let trailer = format!("{count} {glyph}");
            let trailer_width = Line::from(trailer.as_str()).width();
            let name_width = usize::from(width)
                .saturating_sub(1 + trailer_width)
                .saturating_sub(1);
            let name = truncate_with_ellipsis(scope_display_name(key), name_width);
            let left_width = 1 + Line::from(name.as_str()).width();
            let gap = usize::from(width)
                .saturating_sub(left_width + trailer_width)
                .max(1);
            Line::from(vec![
                Span::styled(gutter.to_owned(), Style::default().fg(gutter_color)),
                Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" ".repeat(gap)),
                Span::raw(trailer),
            ])
        }
        Row::Session { session, .. } => {
            let (glyph, color) = indicator(session, model.spin, &model.palette);
            let mut spans = vec![
                Span::styled(gutter.to_owned(), Style::default().fg(gutter_color)),
                Span::raw(" "),
                Span::styled(glyph.to_string(), Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    session_label(session),
                    Style::default()
                        .fg(model.palette.text)
                        .bg(if selected {
                            model.palette.surface0
                        } else {
                            model.palette.panel_bg
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            if model.worktree_cwds.contains(&session.cwd) {
                spans.push(Span::styled(
                    " ⎇",
                    Style::default().fg(model.palette.overlay0),
                ));
            }
            Line::from(spans)
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

fn status_counts(model: &Model) -> Line<'static> {
    let count = |status| {
        model
            .sidebar
            .sessions()
            .iter()
            .filter(|session| session.status == status)
            .count()
    };
    Line::from(vec![
        Span::styled("⠴", Style::default().fg(model.palette.green)),
        Span::raw(format!(" {} ", count(Status::Working))),
        Span::styled("⚑", Style::default().fg(model.palette.red)),
        Span::raw(format!(" {} ", count(Status::NeedsApproval))),
        Span::styled("●", Style::default().fg(model.palette.green)),
        Span::raw(format!(" {} ", count(Status::WaitingUser))),
        Span::styled("●", Style::default().fg(model.palette.yellow)),
        Span::raw(format!(" {}", count(Status::Idle))),
    ])
}

fn render_terminal(model: &Model, frame: &mut Frame) {
    let Some(terminal) = model.frame.as_ref() else {
        frame.render_widget(
            Paragraph::new("No sessions. Ctrl-a n starts Claude; Ctrl-a c starts Codex.")
                .style(Style::default().fg(model.palette.overlay0)),
            model.pane,
        );
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
                model.palette.mauve
            } else {
                model.palette.overlay0
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

fn session_label(session: &SessionInfo) -> String {
    if !session.name.is_empty() {
        return session.name.clone();
    }
    std::path::Path::new(&session.cwd)
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .or_else(|| {
            (!session.id.is_empty()).then(|| session.id.chars().take(8).collect::<String>())
        })
        .unwrap_or_else(|| "session".to_owned())
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
        assert!(transition_toasts(&mut previous, &[session(Status::Working, "")]).is_empty());
        let approval = transition_toasts(
            &mut previous,
            &[session(Status::NeedsApproval, "approve command?")],
        );
        assert_eq!(approval.len(), 1);
        assert_eq!(approval[0].status, Status::NeedsApproval);
        assert!(approval[0].text.contains("approve command?"));
        let waiting = transition_toasts(&mut previous, &[session(Status::WaitingUser, "")]);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].status, Status::WaitingUser);
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
}
