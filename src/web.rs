//! Authenticated mobile bridge for the existing React/xterm.js PWA.
//!
//! The bridge is intentionally a daemon client, not part of the daemon. It
//! translates semantic Ghostty cell frames into the PWA's stable ANSI snapshot
//! protocol and multiplexes one attach plus session/task snapshots per browser.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine;
use ratatui::style::Modifier;
use serde::{Deserialize, Serialize};

use crate::daemon::{socket_path, state_dir};
use crate::protocol::{
    Request, Response, SessionInfo, StreamDown, StreamUp, TerminalFrame, VERSION,
};
use crate::task::Task;
use crate::worktree;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

pub const DEFAULT_PORT: u16 = 8899;
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub token: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

impl Config {
    pub fn load_or_create() -> io::Result<Self> {
        let path = config_path();
        if let Ok(bytes) = fs::read(&path) {
            let config: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if !config.token.is_empty() {
                return Ok(config);
            }
        }
        Self::rotate_from(Self::default())
    }

    pub fn rotate() -> io::Result<Self> {
        let old = Self::load_or_create().unwrap_or_default();
        Self::rotate_from(old)
    }

    fn rotate_from(mut config: Self) -> io::Result<Self> {
        let mut bytes = [0u8; 32];
        fill_random(&mut bytes)?;
        config.token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        if let Some(parent) = config_path().parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = config_path().with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&config).map_err(io::Error::other)?,
        )?;
        fs::set_permissions(&temporary, permissions_600())?;
        fs::rename(temporary, config_path())?;
        Ok(config)
    }
}

#[cfg(unix)]
fn permissions_600() -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(0o600)
}

fn config_path() -> PathBuf {
    state_dir().join("web.json")
}

fn fill_random(output: &mut [u8]) -> io::Result<()> {
    let mut file = fs::File::open("/dev/urandom")?;
    file.read_exact(output)
}

#[derive(Debug, Clone, Serialize)]
struct WebSession {
    #[serde(flatten)]
    session: SessionInfo,
    scope: String,
    scope_name: String,
}

#[derive(Debug, Clone, Serialize)]
struct WebTask {
    #[serde(flatten)]
    task: Task,
    scope_name: String,
}

#[derive(Debug, Clone)]
struct Snapshot {
    sessions: Vec<WebSession>,
    tasks: Vec<WebTask>,
}

#[derive(Default)]
struct Poller {
    subscribers: Mutex<Vec<mpsc::SyncSender<Snapshot>>>,
    last: Mutex<Option<Snapshot>>,
}

impl Poller {
    fn start(self: &Arc<Self>) {
        let poller = Arc::clone(self);
        thread::spawn(move || poller.run());
    }

    fn subscribe(&self) -> mpsc::Receiver<Snapshot> {
        let (sender, receiver) = mpsc::sync_channel(1);
        if let Some(snapshot) = self.last.lock().ok().and_then(|last| last.clone()) {
            let _ = sender.try_send(snapshot);
        }
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(sender);
        }
        receiver
    }

    fn publish(&self, response: Response) {
        let snapshot = Snapshot {
            sessions: response
                .sessions
                .into_iter()
                .map(|session| {
                    let scope = scope_key(Path::new(&session.cwd));
                    WebSession {
                        session,
                        scope_name: scope_name(&scope),
                        scope,
                    }
                })
                .collect(),
            tasks: response
                .tasks
                .into_iter()
                .map(|task| WebTask {
                    scope_name: scope_name(&task.scope),
                    task,
                })
                .collect(),
        };
        if let Ok(mut last) = self.last.lock() {
            *last = Some(snapshot.clone());
        }
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|sender| match sender.try_send(snapshot.clone()) {
                Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
        }
    }

    fn run(&self) {
        loop {
            if self.watch().is_err() {
                if let Ok(response) = daemon_request(&Request {
                    kind: "list".to_owned(),
                    ..Request::default()
                }) {
                    self.publish(response);
                }
                thread::sleep(Duration::from_millis(500));
            }
        }
    }

    fn watch(&self) -> io::Result<()> {
        let mut stream = UnixStream::connect(socket_path())?;
        write_line(
            &mut stream,
            &Request {
                kind: "watch".to_owned(),
                ..Request::default()
            },
        )?;
        for line in BufReader::new(stream).lines() {
            let response: Response = serde_json::from_str(&line?).map_err(io::Error::other)?;
            if !response.ok {
                return Err(io::Error::other(response.error));
            }
            self.publish(response);
        }
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "daemon watch closed",
        ))
    }
}

pub struct Server {
    config: Config,
    poller: Arc<Poller>,
}

impl Server {
    pub fn new(config: Config) -> Arc<Self> {
        let poller = Arc::new(Poller::default());
        poller.start();
        Arc::new(Self { config, poller })
    }

    pub fn run(self: &Arc<Self>, port: u16) -> io::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        println!("cb web listening on http://127.0.0.1:{port}");
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let server = Arc::clone(self);
                    thread::spawn(move || {
                        if let Err(error) = server.handle_http(stream) {
                            if !matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                            ) {
                                eprintln!("cb web client error: {error}");
                            }
                        }
                    });
                }
                Err(error) => eprintln!("cb web accept error: {error}"),
            }
        }
        Ok(())
    }

    fn handle_http(self: &Arc<Self>, mut stream: TcpStream) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let request = read_http_request(&mut stream)?;
        if request.path == "/ws" && request.header("upgrade") == Some("websocket") {
            return self.upgrade_websocket(stream, request);
        }
        serve_static(stream, &request.path)
    }

    fn upgrade_websocket(
        self: &Arc<Self>,
        mut stream: TcpStream,
        request: HttpRequest,
    ) -> io::Result<()> {
        if !origin_allowed(&request, &self.config.allowed_origins) {
            return write_http(&mut stream, "403 Forbidden", "text/plain", b"origin denied");
        }
        let key = request
            .header("sec-websocket-key")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing websocket key"))?;
        let mut input = key.as_bytes().to_vec();
        input.extend_from_slice(WS_GUID);
        let accept = base64::engine::general_purpose::STANDARD.encode(sha1(&input));
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )?;
        stream.flush()?;
        self.run_client(stream)
    }

    fn run_client(self: &Arc<Self>, mut reader: TcpStream) -> io::Result<()> {
        reader.set_read_timeout(Some(Duration::from_secs(5)))?;
        let first = ws_read(&mut reader)?;
        let auth: WsUp = serde_json::from_slice(&first.payload).map_err(io::Error::other)?;
        if first.opcode != 1
            || auth.kind != "auth"
            || !constant_time_eq(auth.token.as_bytes(), self.config.token.as_bytes())
        {
            let _ = ws_write_json(&mut reader, &WsDown::error("auth failed"), 1);
            let _ = ws_write(&mut reader, 8, &1008u16.to_be_bytes());
            return Ok(());
        }
        reader.set_read_timeout(None)?;

        let writer = Arc::new(Mutex::new(reader.try_clone()?));
        let (out, messages) = mpsc::sync_channel::<WsDown>(64);
        let writer_copy = Arc::clone(&writer);
        thread::spawn(move || {
            while let Ok(message) = messages.recv() {
                let Ok(mut stream) = writer_copy.lock() else {
                    break;
                };
                if ws_write_json(&mut stream, &message, 1).is_err() {
                    break;
                }
            }
        });

        let agents = worktree::available_agents()
            .into_iter()
            .map(|agent| agent.binary.to_owned())
            .collect();
        send_latest(
            &out,
            WsDown {
                kind: "hello".to_owned(),
                protocol: Some(VERSION),
                daemon: Some(ping_daemon()),
                agents,
                ..WsDown::default()
            },
        );

        let snapshots = self.poller.subscribe();
        let snapshot_out = out.clone();
        thread::spawn(move || {
            while let Ok(snapshot) = snapshots.recv() {
                send_latest(
                    &snapshot_out,
                    WsDown {
                        kind: "sessions".to_owned(),
                        sessions: snapshot.sessions,
                        ..WsDown::default()
                    },
                );
                send_latest(
                    &snapshot_out,
                    WsDown {
                        kind: "tasks".to_owned(),
                        tasks: snapshot.tasks,
                        ..WsDown::default()
                    },
                );
            }
        });

        let attach = Arc::new(Mutex::new(None::<UnixStream>));
        let attach_epoch = Arc::new(AtomicU64::new(0));
        let viewport = Arc::new(AtomicU64::new(0));
        loop {
            let frame = match ws_read(&mut reader) {
                Ok(frame) => frame,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    break
                }
                Err(error) => return Err(error),
            };
            match frame.opcode {
                8 => break,
                9 => {
                    if let Ok(mut writer) = writer.lock() {
                        let _ = ws_write(&mut writer, 10, &frame.payload);
                    }
                }
                1 => {
                    if let Ok(message) = serde_json::from_slice::<WsUp>(&frame.payload) {
                        dispatch_browser(message, &out, &attach, &attach_epoch, &viewport);
                    }
                }
                _ => {}
            }
        }
        attach_epoch.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut current) = attach.lock() {
            if let Some(stream) = current.take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        Ok(())
    }
}

pub fn run(port: u16) -> io::Result<()> {
    Server::new(Config::load_or_create()?).run(port)
}

pub fn start_default() {
    thread::spawn(|| {
        if let Err(error) = run(DEFAULT_PORT) {
            if error.kind() != io::ErrorKind::AddrInUse {
                eprintln!("cb web: {error}");
            }
        }
    });
}

#[derive(Debug, Default, Deserialize)]
struct WsUp {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    data: String,
    #[serde(default)]
    rows: u16,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    task_status: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    run_id: String,
}

#[derive(Debug, Default, Serialize)]
struct WsDown {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<WebSession>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tasks: Vec<WebTask>,
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    screen: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor_x: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor_y: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cols: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_offset: Option<usize>,
    #[serde(skip_serializing_if = "String::is_empty")]
    cwd: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    worktrees: Vec<WebWorktree>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agents: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
}

impl WsDown {
    fn error(error: impl Into<String>) -> Self {
        Self {
            kind: "error".to_owned(),
            error: error.into(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Serialize)]
struct WebWorktree {
    path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    branch: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    detached: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    bare: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    main: bool,
}

fn dispatch_browser(
    up: WsUp,
    out: &mpsc::SyncSender<WsDown>,
    attach: &Arc<Mutex<Option<UnixStream>>>,
    epoch: &Arc<AtomicU64>,
    viewport: &Arc<AtomicU64>,
) {
    match up.kind.as_str() {
        "attach" => attach_browser(up.id, out, attach, epoch, viewport),
        "detach" => {
            epoch.fetch_add(1, Ordering::AcqRel);
            if let Ok(mut attach) = attach.lock() {
                if let Some(stream) = attach.take() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }
        "input" | "paste" | "scroll" | "resize" => {
            let message = StreamUp {
                kind: up.kind,
                data: up.data,
                rows: up.rows,
                cols: up.cols,
                offset: up.offset,
                ..StreamUp::default()
            };
            if let Ok(mut guard) = attach.lock() {
                if let Some(stream) = guard.as_mut() {
                    let _ = write_line(stream, &message);
                }
            }
        }
        "viewport" => {
            viewport.store(pack_size(up.rows, up.cols), Ordering::Release);
        }
        "interrupt" => {
            if let Ok(mut guard) = attach.lock() {
                if let Some(stream) = guard.as_mut() {
                    let _ = write_line(
                        stream,
                        &StreamUp {
                            kind: "input".to_owned(),
                            data: base64::engine::general_purpose::STANDARD.encode([3]),
                            ..StreamUp::default()
                        },
                    );
                }
            }
        }
        "spawn" => proxy_spawn(up, out),
        "kill" => proxy_simple(
            Request {
                kind: "kill".to_owned(),
                id: up.id,
                ..Request::default()
            },
            out,
        ),
        "worktrees" => send_worktrees(&up.cwd, out),
        kind if kind.starts_with("task_") => proxy_task(up, out),
        _ => {}
    }
}

fn attach_browser(
    id: String,
    out: &mpsc::SyncSender<WsDown>,
    attach: &Arc<Mutex<Option<UnixStream>>>,
    epoch: &Arc<AtomicU64>,
    viewport: &Arc<AtomicU64>,
) {
    if id.is_empty() {
        return;
    }
    let mut stream = match UnixStream::connect(socket_path()) {
        Ok(stream) => stream,
        Err(error) => {
            send_latest(out, WsDown::error(format!("attach: {error}")));
            return;
        }
    };
    if let Err(error) = write_line(
        &mut stream,
        &Request {
            kind: "attach".to_owned(),
            id: id.clone(),
            ..Request::default()
        },
    ) {
        send_latest(out, WsDown::error(format!("attach: {error}")));
        return;
    }
    let reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            send_latest(out, WsDown::error(format!("attach: {error}")));
            return;
        }
    };
    let generation = epoch.fetch_add(1, Ordering::AcqRel) + 1;
    if let Ok(mut current) = attach.lock() {
        if let Some(old) = current.replace(stream) {
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
    }
    let out = out.clone();
    let epoch = Arc::clone(epoch);
    let viewport = Arc::clone(viewport);
    thread::spawn(move || {
        let mut crop = (0u16, 0u16);
        for line in BufReader::new(reader).lines() {
            if epoch.load(Ordering::Acquire) != generation {
                return;
            }
            let Ok(line) = line else { return };
            let Ok(message) = serde_json::from_str::<StreamDown>(&line) else {
                continue;
            };
            match message {
                StreamDown::Frame { frame } => {
                    let (frame, x, y) =
                        crop_frame(frame, unpack_size(viewport.load(Ordering::Acquire)), crop);
                    crop = (x, y);
                    send_latest(
                        &out,
                        WsDown {
                            kind: "frame".to_owned(),
                            id: id.clone(),
                            screen: frame_to_ansi(&frame),
                            cursor_x: Some(frame.cursor_x),
                            cursor_y: Some(frame.cursor_y),
                            rows: Some(frame.rows),
                            cols: Some(frame.cols),
                            offset: Some(frame.offset),
                            max_offset: Some(frame.max_offset),
                            ..WsDown::default()
                        },
                    );
                }
                StreamDown::Gone { .. } => {
                    send_latest(
                        &out,
                        WsDown {
                            kind: "gone".to_owned(),
                            id: id.clone(),
                            ..WsDown::default()
                        },
                    );
                    return;
                }
                StreamDown::Error { message } => {
                    send_latest(&out, WsDown::error(message));
                    return;
                }
            }
        }
    });
}

fn proxy_spawn(up: WsUp, out: &mpsc::SyncSender<WsDown>) {
    match daemon_request(&Request {
        kind: "spawn".to_owned(),
        argv: up.argv,
        cwd: up.cwd,
        ..Request::default()
    }) {
        Ok(response) if response.ok => send_latest(
            out,
            WsDown {
                kind: "spawned".to_owned(),
                id: response.id,
                ..WsDown::default()
            },
        ),
        Ok(response) => send_latest(out, WsDown::error(response.error)),
        Err(error) => send_latest(out, WsDown::error(error.to_string())),
    }
}

fn proxy_simple(request: Request, out: &mpsc::SyncSender<WsDown>) {
    match daemon_request(&request) {
        Ok(response) if response.ok => {}
        Ok(response) => send_latest(out, WsDown::error(response.error)),
        Err(error) => send_latest(out, WsDown::error(error.to_string())),
    }
}

fn proxy_task(up: WsUp, out: &mpsc::SyncSender<WsDown>) {
    let kind = up.kind.clone();
    match daemon_request(&Request {
        kind,
        id: up.id,
        scope: up.scope,
        title: up.title,
        desc: up.desc,
        task_status: up.task_status,
        agent: up.agent,
        run_id: up.run_id,
        cwd: up.cwd,
        ..Request::default()
    }) {
        Ok(response) if response.ok => {
            let id = response.id.clone();
            send_latest(
                out,
                WsDown {
                    kind: "tasks".to_owned(),
                    tasks: response
                        .tasks
                        .into_iter()
                        .map(|task| WebTask {
                            scope_name: scope_name(&task.scope),
                            task,
                        })
                        .collect(),
                    ..WsDown::default()
                },
            );
            if !id.is_empty() {
                send_latest(
                    out,
                    WsDown {
                        kind: "spawned".to_owned(),
                        id,
                        ..WsDown::default()
                    },
                );
            }
        }
        Ok(response) => send_latest(out, WsDown::error(response.error)),
        Err(error) => send_latest(out, WsDown::error(error.to_string())),
    }
}

fn send_worktrees(cwd: &str, out: &mpsc::SyncSender<WsDown>) {
    let mut entries = worktree::list(Path::new(cwd)).unwrap_or_default();
    if entries.is_empty() {
        entries.push(worktree::Worktree {
            path: PathBuf::from(cwd),
            branch: String::new(),
            detached: false,
            bare: false,
            main: true,
        });
    }
    send_latest(
        out,
        WsDown {
            kind: "worktrees".to_owned(),
            cwd: cwd.to_owned(),
            worktrees: entries
                .into_iter()
                .map(|entry| WebWorktree {
                    path: entry.path.to_string_lossy().into_owned(),
                    branch: entry.branch,
                    detached: entry.detached,
                    bare: entry.bare,
                    main: entry.main,
                })
                .collect(),
            agents: worktree::available_agents()
                .into_iter()
                .map(|agent| agent.binary.to_owned())
                .collect(),
            ..WsDown::default()
        },
    );
}

fn daemon_request(request: &Request) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket_path())?;
    write_line(&mut stream, request)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "daemon closed without response",
        ));
    }
    serde_json::from_str(&line).map_err(io::Error::other)
}

fn ping_daemon() -> bool {
    daemon_request(&Request {
        kind: "ping".to_owned(),
        ..Request::default()
    })
    .is_ok_and(|response| response.ok && response.version == Some(VERSION))
}

fn write_line(mut writer: impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn send_latest(sender: &mpsc::SyncSender<WsDown>, message: WsDown) {
    let _ = sender.try_send(message);
}

fn pack_size(rows: u16, cols: u16) -> u64 {
    (u64::from(rows) << 16) | u64::from(cols)
}

fn unpack_size(value: u64) -> (u16, u16) {
    ((value >> 16) as u16, value as u16)
}

fn crop_frame(
    frame: TerminalFrame,
    viewport: (u16, u16),
    old_origin: (u16, u16),
) -> (TerminalFrame, u16, u16) {
    let (view_rows, view_cols) = viewport;
    if view_rows == 0 || view_cols == 0 || (view_rows >= frame.rows && view_cols >= frame.cols) {
        return (frame, 0, 0);
    }
    let rows = view_rows.min(frame.rows);
    let cols = view_cols.min(frame.cols);
    let mut x = old_origin.0.min(frame.cols.saturating_sub(cols));
    let mut y = old_origin.1.min(frame.rows.saturating_sub(rows));
    if frame.cursor_x < x {
        x = frame.cursor_x;
    } else if frame.cursor_x >= x.saturating_add(cols) {
        x = frame.cursor_x.saturating_sub(cols.saturating_sub(1));
    }
    if frame.cursor_y < y {
        y = frame.cursor_y;
    } else if frame.cursor_y >= y.saturating_add(rows) {
        y = frame.cursor_y.saturating_sub(rows.saturating_sub(1));
    }
    let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
    for row in y..y + rows {
        let start = usize::from(row) * usize::from(frame.cols) + usize::from(x);
        cells.extend_from_slice(&frame.cells[start..start + usize::from(cols)]);
    }
    (
        TerminalFrame {
            rows,
            cols,
            cells,
            cursor_x: frame.cursor_x.saturating_sub(x),
            cursor_y: frame.cursor_y.saturating_sub(y),
            cursor_visible: frame.cursor_visible,
            mouse_reporting: frame.mouse_reporting,
            offset: frame.offset,
            max_offset: frame.max_offset,
        },
        x,
        y,
    )
}

fn frame_to_ansi(frame: &TerminalFrame) -> String {
    let mut output = String::new();
    let mut previous = None;
    for row in 0..frame.rows {
        if row > 0 {
            output.push('\n');
        }
        for col in 0..frame.cols {
            let cell = &frame.cells[usize::from(row) * usize::from(frame.cols) + usize::from(col)];
            let style = (cell.fg, cell.bg, cell.modifiers);
            if previous != Some(style) {
                output.push_str("\x1b[0");
                append_color(&mut output, cell.fg, true);
                append_color(&mut output, cell.bg, false);
                let modifiers = Modifier::from_bits_truncate(cell.modifiers);
                if modifiers.contains(Modifier::BOLD) {
                    output.push_str(";1");
                }
                if modifiers.contains(Modifier::DIM) {
                    output.push_str(";2");
                }
                if modifiers.contains(Modifier::ITALIC) {
                    output.push_str(";3");
                }
                if modifiers.contains(Modifier::UNDERLINED) {
                    output.push_str(";4");
                }
                if modifiers.contains(Modifier::SLOW_BLINK) {
                    output.push_str(";5");
                }
                if modifiers.contains(Modifier::REVERSED) {
                    output.push_str(";7");
                }
                if modifiers.contains(Modifier::HIDDEN) {
                    output.push_str(";8");
                }
                if modifiers.contains(Modifier::CROSSED_OUT) {
                    output.push_str(";9");
                }
                output.push('m');
                previous = Some(style);
            }
            output.push_str(&cell.symbol);
        }
        output.push_str("\x1b[0m");
        previous = None;
    }
    output
}

fn append_color(output: &mut String, color: u32, foreground: bool) {
    let base = if foreground { 30 } else { 40 };
    match color {
        0 => output.push_str(if foreground { ";39" } else { ";49" }),
        1..=8 => output.push_str(&format!(";{}", base + color - 1)),
        9..=16 => output.push_str(&format!(";{}", base + 60 + color - 9)),
        value if value & 0xF000_0000 == 0x1000_0000 => output.push_str(&format!(
            ";{};5;{}",
            if foreground { 38 } else { 48 },
            value & 0xff
        )),
        value if value & 0xF000_0000 == 0x2000_0000 => output.push_str(&format!(
            ";{};2;{};{};{}",
            if foreground { 38 } else { 48 },
            (value >> 16) & 0xff,
            (value >> 8) & 0xff,
            value & 0xff
        )),
        _ => {}
    }
}

#[derive(Debug)]
struct HttpRequest {
    path: String,
    headers: HashMap<String, String>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

fn read_http_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut bytes = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    while bytes.len() < 64 * 1024 && !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
    }
    if !bytes.ends_with(b"\r\n\r\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "oversized HTTP headers",
        ));
    }
    let text = String::from_utf8(bytes).map_err(io::Error::other)?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    if request_parts.next() != Some("GET") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only GET is supported",
        ));
    }
    let path = request_parts
        .next()
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    Ok(HttpRequest { path, headers })
}

fn origin_allowed(request: &HttpRequest, allowed: &[String]) -> bool {
    let Some(origin) = request.header("origin") else {
        return true;
    };
    let host = request.header("host").unwrap_or_default();
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .unwrap_or_default();
    origin_host.eq_ignore_ascii_case(host)
        || allowed
            .iter()
            .any(|pattern| pattern == "*" || origin.eq_ignore_ascii_case(pattern))
}

fn serve_static(mut stream: TcpStream, requested: &str) -> io::Result<()> {
    let clean = requested
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("/");
    let clean = if clean.is_empty() {
        "index.html"
    } else {
        &clean
    };
    let asset = embedded_asset(clean).or_else(|| {
        Path::new(clean)
            .extension()
            .is_none()
            .then(|| embedded_asset("index.html"))
            .flatten()
    });
    let Some(asset) = asset else {
        return write_http(&mut stream, "404 Not Found", "text/plain", b"not found");
    };
    let content_type = content_type(clean);
    let cache = if clean.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nCache-Control: {cache}\r\nConnection: close\r\n\r\n",
        asset.len()
    )?;
    stream.write_all(asset)
}

fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|value| value.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "webmanifest") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

struct WebSocketFrame {
    opcode: u8,
    payload: Vec<u8>,
}

fn ws_read(stream: &mut TcpStream) -> io::Result<WebSocketFrame> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header)?;
    if header[0] & 0x80 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fragmented websocket message unsupported",
        ));
    }
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    if !masked {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "browser websocket frame is not masked",
        ));
    }
    let mut length = u64::from(header[1] & 0x7f);
    if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended)?;
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0u8; 8];
        stream.read_exact(&mut extended)?;
        length = u64::from_be_bytes(extended);
    }
    if length > 8 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "websocket message too large",
        ));
    }
    let mut mask = [0u8; 4];
    stream.read_exact(&mut mask)?;
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload)?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Ok(WebSocketFrame { opcode, payload })
}

fn ws_write_json(stream: &mut TcpStream, value: &impl Serialize, opcode: u8) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    ws_write(stream, opcode, &bytes)
}

fn ws_write(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    stream.write_all(&[0x80 | opcode])?;
    match payload.len() {
        0..=125 => stream.write_all(&[payload.len() as u8])?,
        126..=65535 => {
            stream.write_all(&[126])?;
            stream.write_all(&(payload.len() as u16).to_be_bytes())?;
        }
        _ => {
            stream.write_all(&[127])?;
            stream.write_all(&(payload.len() as u64).to_be_bytes())?;
        }
    }
    stream.write_all(payload)?;
    stream.flush()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |different, (left, right)| different | (left ^ right))
        == 0
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let bit_length = (input.len() as u64) * 8;
    let mut bytes = input.to_vec();
    bytes.push(0x80);
    while bytes.len() % 64 != 56 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&bit_length.to_be_bytes());
    let mut h = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for chunk in bytes.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes(word.try_into().expect("four byte word"));
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut output = [0u8; 20];
    for (index, word) in h.into_iter().enumerate() {
        output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    output
}

fn scope_key(cwd: &Path) -> String {
    git_common_dir(cwd)
        .unwrap_or_else(|| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn scope_name(scope: &str) -> String {
    let path = Path::new(scope);
    if path.file_name().is_some_and(|name| name == ".git") {
        return path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| scope.to_owned());
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "(unknown)".to_owned())
}

fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd;
    let git = loop {
        let candidate = current.join(".git");
        if candidate.exists() {
            break candidate;
        }
        current = current.parent()?;
    };
    if git.is_dir() {
        return fs::canonicalize(&git).ok().or(Some(git));
    }
    let data = fs::read_to_string(&git).ok()?;
    let pointer = data.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = if Path::new(pointer).is_absolute() {
        PathBuf::from(pointer)
    } else {
        git.parent()?.join(pointer)
    };
    let common = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|value| value.trim().to_owned())
        .map(|value| {
            let value = PathBuf::from(value);
            if value.is_absolute() {
                value
            } else {
                git_dir.join(value)
            }
        })
        .unwrap_or(git_dir);
    fs::canonicalize(&common).ok().or(Some(common))
}

pub fn pairing_url(base: &str, token: &str) -> String {
    format!("{}#token={token}", base.trim_end_matches('/'))
}

pub fn print_qr(base: &str, token: &str) -> io::Result<()> {
    let url = pairing_url(base, token);
    let code = qrcode::QrCode::new(url.as_bytes()).map_err(io::Error::other)?;
    println!(
        "{}",
        code.render::<qrcode::render::unicode::Dense1x2>().build()
    );
    println!("{url}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Cell;

    #[test]
    fn websocket_accept_sha1_matches_rfc_example() {
        let mut input = b"dGhlIHNhbXBsZSBub25jZQ==".to_vec();
        input.extend_from_slice(WS_GUID);
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(sha1(&input)),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn semantic_frame_converts_to_colored_ansi() {
        let frame = TerminalFrame {
            rows: 1,
            cols: 2,
            cells: vec![
                Cell {
                    symbol: "A".to_owned(),
                    fg: 2,
                    bg: 0,
                    modifiers: Modifier::BOLD.bits(),
                },
                Cell {
                    symbol: "界".to_owned(),
                    fg: 0x2011_2233,
                    bg: 0,
                    modifiers: 0,
                },
            ],
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            mouse_reporting: false,
            offset: 0,
            max_offset: 0,
        };
        let ansi = frame_to_ansi(&frame);
        assert!(ansi.contains("\u{1b}[0;31;49;1mA"));
        assert!(ansi.contains(";38;2;17;34;51"));
    }

    #[test]
    fn crop_follows_cursor_without_resizing_canonical_frame() {
        let cells = (0..24)
            .map(|index| Cell {
                symbol: index.to_string(),
                fg: 0,
                bg: 0,
                modifiers: 0,
            })
            .collect();
        let frame = TerminalFrame {
            rows: 4,
            cols: 6,
            cells,
            cursor_x: 5,
            cursor_y: 3,
            cursor_visible: true,
            mouse_reporting: false,
            offset: 7,
            max_offset: 9,
        };
        let (cropped, x, y) = crop_frame(frame, (2, 3), (0, 0));
        assert_eq!((cropped.rows, cropped.cols), (2, 3));
        assert_eq!((cropped.cursor_x, cropped.cursor_y), (2, 1));
        assert_eq!((x, y), (3, 2));
        assert_eq!(cropped.offset, 7);
    }
}
