use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use codebridge::daemon::{socket_path, Daemon};
use codebridge::integration::{self, Agent};
use codebridge::protocol::{Request, Response};

fn main() {
    if let Err(error) = run() {
        eprintln!("cb: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("daemon") => {
            ensure_conductor()?;
            codebridge::web::start_default();
            Daemon::new().run(&socket_path())?;
        }
        Some("conductor") => {
            let conductor = codebridge::conductor::Conductor::new();
            // `cb conductor --resume <stash>` is the hot-upgrade successor: adopt
            // the predecessor's sessions from their surviving fds before binding
            // the socket, so clients only reconnect once state is restored.
            if let Some(stash) = resume_stash_arg(&args[1..]) {
                conductor.resume_from(std::path::Path::new(&stash))?;
            }
            conductor.run(&codebridge::conductor::conductor_socket_path())?;
        }
        Some("ctl") => ctl(&args[1..])?,
        Some("web") => web_command(&args[1..])?,
        Some("stop") => {
            // Stop the broker, then tear the conductor (and its sessions) down.
            // Either being present counts as "was running".
            let broker = stop_daemon().is_ok();
            let conductor = stop_conductor();
            if !broker && !conductor {
                return Err("daemon not running".into());
            }
            println!("daemon stopped");
        }
        Some("restart") => restart_daemon()?,
        Some("upgrade") => upgrade_conductor()?,
        Some("hook") => hook(&args[1..])?,
        Some("integration") => integration_command(&args[1..])?,
        Some("install-hooks") => print_install(Agent::Claude)?,
        Some("install-codex") => print_install(Agent::Codex)?,
        Some("version" | "--version" | "-V" | "-v") => {
            println!(
                "cb {}, rust {}, protocol v{}",
                env!("CARGO_PKG_VERSION"),
                option_env!("RUSTC_VERSION").unwrap_or("unknown"),
                codebridge::protocol::VERSION
            );
        }
        Some("--all" | "-a") => run_dashboard()?,
        Some("-h" | "--help" | "help") => println!("{}", usage()),
        Some(command) => {
            return Err(format!("unknown command {command:?}\n{}", usage()).into());
        }
        None => run_dashboard()?,
    }
    Ok(())
}

fn ctl(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("usage: cb ctl <ping|list|spawn|kill|rename|shutdown>".into());
    };
    let mut request = Request {
        kind: command.to_owned(),
        ..Request::default()
    };
    match command {
        "spawn" => {
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--cwd" => {
                        index += 1;
                        request.cwd = args.get(index).ok_or("--cwd requires a directory")?.clone();
                    }
                    "--rows" => {
                        index += 1;
                        request.rows = args.get(index).ok_or("--rows requires a value")?.parse()?;
                    }
                    "--cols" => {
                        index += 1;
                        request.cols = args.get(index).ok_or("--cols requires a value")?.parse()?;
                    }
                    "--" => {
                        request.argv = args[index + 1..].to_vec();
                        break;
                    }
                    argument => request.argv.push(argument.to_owned()),
                }
                index += 1;
            }
        }
        "kill" => request.id = args.get(1).ok_or("kill requires a session id")?.clone(),
        "rename" => {
            request.id = args.get(1).ok_or("rename requires a session id")?.clone();
            request.name = args.get(2).ok_or("rename requires a name")?.clone();
        }
        "ping" | "list" | "shutdown" => {}
        _ => return Err(format!("unknown ctl command {command:?}").into()),
    }
    let response = send(&request, None)?;
    if !response.ok {
        return Err(response.error.into());
    }
    match command {
        "list" => {
            if response.sessions.is_empty() {
                println!("(no sessions)");
            }
            for session in response.sessions {
                println!(
                    "{}  {:<14} exited={}  {:?}  {}",
                    session.id.chars().take(8).collect::<String>(),
                    status_label(&session.status),
                    session.exited,
                    session.argv,
                    session.last_message
                );
            }
        }
        "spawn" => println!("{}", response.id),
        "ping" => println!("{}", serde_json::to_string_pretty(&response)?),
        _ => {}
    }
    Ok(())
}

fn status_label(status: &codebridge::protocol::Status) -> &'static str {
    use codebridge::protocol::Status;
    match status {
        Status::Starting => "starting",
        Status::Working => "working",
        Status::NeedsApproval => "needs_approval",
        Status::WaitingUser => "waiting_user",
        Status::Idle => "idle",
        Status::Ended => "ended",
    }
}

fn hook(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(session) = std::env::var_os("CB_SESSION").filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let event = args.first().cloned().unwrap_or_default();
    let codex_config = args
        .windows(2)
        .find(|args| args[0] == "--codex-config")
        .map(|args| std::path::Path::new(&args[1]));
    if event == "PermissionRequest"
        && codex_config.is_some_and(integration::codex_handles_approvals_automatically)
    {
        return Ok(());
    }
    let mut bytes = Vec::new();
    let _ = io::stdin().take(1024 * 1024).read_to_end(&mut bytes);
    let payload = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    let request = Request {
        kind: "hook".to_owned(),
        event,
        session: session.to_string_lossy().into_owned(),
        payload,
        ..Request::default()
    };
    // Hooks are no-op observers. A missing or slow daemon must never interfere
    // with the agent's own command lifecycle.
    let _ = send(&request, Some(Duration::from_secs(2)));
    Ok(())
}

fn integration_command(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let action = args.first().map(String::as_str).unwrap_or("status");
    let agent = args.get(1).map(|value| parse_agent(value)).transpose()?;
    match action {
        "install" => print_install(agent.ok_or("install requires claude or codex")?)?,
        "uninstall" => {
            let agent = agent.ok_or("uninstall requires claude or codex")?;
            integration::uninstall(agent)?;
            println!("uninstalled {}", agent_label(agent));
        }
        "status" => {
            let agents = agent.map_or_else(|| vec![Agent::Claude, Agent::Codex], |a| vec![a]);
            for agent in agents {
                println!("{}: {:?}", agent_label(agent), integration::status(agent)?);
            }
        }
        _ => return Err("usage: cb integration <install|uninstall|status> [claude|codex]".into()),
    }
    Ok(())
}

fn web_command(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match args.first().map(String::as_str) {
        Some("token") => {
            let config = if args.get(1).is_some_and(|argument| argument == "rotate") {
                codebridge::web::Config::rotate()?
            } else {
                codebridge::web::Config::load_or_create()?
            };
            println!("{}", config.token);
        }
        Some("qr") => {
            let mut base = format!("http://127.0.0.1:{}", codebridge::web::DEFAULT_PORT);
            if let Some(index) = args.iter().position(|argument| argument == "--url") {
                base = args.get(index + 1).ok_or("--url requires a value")?.clone();
            }
            let config = codebridge::web::Config::load_or_create()?;
            codebridge::web::print_qr(&base, &config.token)?;
        }
        Some("-h" | "--help" | "help") => {
            println!("usage: cb web [--port N] | cb web token [rotate] | cb web qr [--url URL]");
        }
        _ => {
            let mut port = codebridge::web::DEFAULT_PORT;
            if let Some(index) = args.iter().position(|argument| argument == "--port") {
                port = args
                    .get(index + 1)
                    .ok_or("--port requires a value")?
                    .parse()?;
            }
            ensure_daemon()?;
            if port == codebridge::web::DEFAULT_PORT {
                println!("cb web is running at http://127.0.0.1:{port}");
            } else {
                codebridge::web::run(port)?;
            }
        }
    }
    Ok(())
}

fn run_dashboard() -> Result<(), Box<dyn std::error::Error>> {
    ensure_daemon()?;
    codebridge::tui::run()?;
    Ok(())
}

fn stop_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let response = send(
        &Request {
            kind: "shutdown".to_owned(),
            ..Request::default()
        },
        Some(Duration::from_secs(2)),
    )?;
    if response.ok {
        Ok(())
    } else {
        Err(response.error.into())
    }
}

fn restart_daemon() -> Result<(), Box<dyn std::error::Error>> {
    if stop_daemon().is_ok() {
        for _ in 0..250 {
            if UnixStream::connect(socket_path()).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    ensure_daemon()?;
    println!("daemon restarted");
    Ok(())
}

fn ensure_daemon() -> Result<(), Box<dyn std::error::Error>> {
    // The broker depends on the conductor, so bring it up first.
    ensure_conductor()?;
    if UnixStream::connect(socket_path()).is_ok() {
        let response = send(
            &Request {
                kind: "ping".to_owned(),
                ..Request::default()
            },
            Some(Duration::from_secs(2)),
        )?;
        if response.ok && response.version == Some(codebridge::protocol::VERSION) {
            return Ok(());
        }
        return Err(format!(
            "a stale cb daemon is running (protocol v{}, want v{}, pid {}).\n\
             run `cb restart` before reopening the dashboard",
            response.version.unwrap_or_default(),
            codebridge::protocol::VERSION,
            response.pid.unwrap_or_default()
        )
        .into());
    }

    fs::create_dir_all(codebridge::daemon::state_dir())?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(codebridge::daemon::state_dir().join("daemon.log"))?;
    let error_log = log.try_clone()?;
    let executable = std::env::current_exe()?;
    Command::new(executable)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()?;

    for _ in 0..100 {
        if UnixStream::connect(socket_path()).is_ok() {
            let response = send(
                &Request {
                    kind: "ping".to_owned(),
                    ..Request::default()
                },
                Some(Duration::from_secs(1)),
            )?;
            if response.version == Some(codebridge::protocol::VERSION) {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!(
        "daemon did not become ready; inspect {}",
        codebridge::daemon::state_dir().join("daemon.log").display()
    )
    .into())
}

/// Ensures the durable conductor process is running and speaks our protocol.
/// Unlike a stale broker, a stale conductor cannot be auto-restarted here: doing
/// so would kill live agent sessions, so a version mismatch is surfaced for the
/// user to resolve with `cb stop`.
fn ensure_conductor() -> Result<(), Box<dyn std::error::Error>> {
    use codebridge::conductor::{conductor_socket_path, ConductorClient, CONDUCTOR_VERSION};

    let socket = conductor_socket_path();
    let client = ConductorClient::new(socket.clone());
    if UnixStream::connect(&socket).is_ok() {
        if let Ok(response) = client.ping() {
            if response.version == Some(CONDUCTOR_VERSION) {
                return Ok(());
            }
            return Err(format!(
                "a stale cb conductor is running (protocol v{}, want v{}, pid {}).\n\
                 stop it with `cb stop` (this ends its live sessions) and retry",
                response.version.unwrap_or_default(),
                CONDUCTOR_VERSION,
                response.pid.unwrap_or_default()
            )
            .into());
        }
    }

    fs::create_dir_all(codebridge::daemon::state_dir())?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(codebridge::daemon::state_dir().join("conductor.log"))?;
    let error_log = log.try_clone()?;
    let executable = std::env::current_exe()?;
    Command::new(executable)
        .arg("conductor")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()?;

    for _ in 0..100 {
        if let Ok(response) = client.ping() {
            if response.version == Some(CONDUCTOR_VERSION) {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!(
        "conductor did not become ready; inspect {}",
        codebridge::daemon::state_dir()
            .join("conductor.log")
            .display()
    )
    .into())
}

/// Best-effort teardown of the conductor process (and its sessions). Returns
/// whether a conductor was reachable.
fn stop_conductor() -> bool {
    let socket = codebridge::conductor::conductor_socket_path();
    if UnixStream::connect(&socket).is_err() {
        return false;
    }
    codebridge::conductor::ConductorClient::new(socket).stop();
    true
}

/// Extracts the `--resume <path>` (or `--resume=<path>`) argument that the
/// hot-upgrade successor is launched with.
fn resume_stash_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--resume" {
            return iter.next().cloned();
        }
        if let Some(path) = arg.strip_prefix("--resume=") {
            return Some(path.to_owned());
        }
    }
    None
}

/// Hot-upgrades the running conductor in place: it snapshots its sessions and
/// re-execs the current `cb` binary under the same pid, so its agents keep
/// running on the new build with their scrollback intact. Success is confirmed
/// by watching the conductor's boot id change (same pid, fresh `Conductor`).
/// The broker dials the conductor per call, so it needs no restart; live panes
/// briefly blip and reattach.
fn upgrade_conductor() -> Result<(), Box<dyn std::error::Error>> {
    use codebridge::conductor::{conductor_socket_path, ConductorClient, CONDUCTOR_VERSION};

    let client = ConductorClient::new(conductor_socket_path());
    let before = match client.ping() {
        Ok(response) => response,
        Err(_) => {
            return Err("no conductor is running to upgrade (start one with `cb daemon`)".into())
        }
    };
    if before.version != Some(CONDUCTOR_VERSION) {
        return Err(format!(
            "conductor speaks protocol v{} but this build wants v{}; run `cb stop` and start fresh",
            before.version.unwrap_or_default(),
            CONDUCTOR_VERSION
        )
        .into());
    }
    let boot_before = before.boot_id;

    client.upgrade()?;

    // The successor keeps the pid but mints a fresh boot id, so a changed boot
    // id proves the new build took over. Poll across the brief window where the
    // socket is unbound during the exec.
    for _ in 0..250 {
        if let Ok(response) = client.ping() {
            if response.ok && !response.boot_id.is_empty() && response.boot_id != boot_before {
                println!(
                    "conductor upgraded (pid {})",
                    response.pid.unwrap_or_default()
                );
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!(
        "conductor did not come back after upgrade; inspect {}",
        codebridge::daemon::state_dir()
            .join("conductor.log")
            .display()
    )
    .into())
}

fn print_install(agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
    let paths = integration::install(agent)?;
    println!(
        "installed {} hook to {} and updated {}",
        agent_label(agent),
        paths.hook_path.display(),
        paths.config_path.display()
    );
    Ok(())
}

fn parse_agent(value: &str) -> Result<Agent, String> {
    match value {
        "claude" => Ok(Agent::Claude),
        "codex" => Ok(Agent::Codex),
        _ => Err(format!("unknown integration target {value:?}")),
    }
}

fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude Code",
        Agent::Codex => "Codex",
    }
}

fn send(
    request: &Request,
    timeout: Option<Duration>,
) -> Result<Response, Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(socket_path())?;
    if let Some(timeout) = timeout {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
    }
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err("daemon closed connection without a response".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn usage() -> &'static str {
    "usage:
  cb [--all]
  cb daemon
  cb conductor
  cb ctl ping|list|spawn|kill|rename|shutdown
  cb web [--port N] | token [rotate] | qr [--url URL]
  cb stop | restart | upgrade | version
  cb hook <event>
  cb integration install|uninstall|status [claude|codex]
  cb install-hooks
  cb install-codex"
}
