mod app;
mod capture;
mod input;
mod ui;

use std::io::{IsTerminal, Read, Write};
use std::process::Stdio;
use std::time::Duration;

use tmux_tabs_common::{
    ClaudeEvent, ClientMessage, Envelope, HookNotification, ServerMessage, SessionEntry,
    read_frame, socket_path, write_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Hot path: Claude Code hook notifications fire frequently. Dispatch the
    // sync path before building a tokio runtime so the hook returns ASAP.
    if args.get(1).map(String::as_str) == Some("notify") {
        cmd_notify_sync(&args[2..]);
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main(args))
}

async fn async_main(args: Vec<String>) -> anyhow::Result<()> {
    let sub = Subcommand::try_from(&args[1..]).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    match sub {
        Subcommand::Server { foreground } => cmd_server(foreground).await,
        Subcommand::Kill => cmd_kill(),
        Subcommand::Switch { index } => cmd_switch(index).await,
        Subcommand::Close { session } => cmd_close(session).await,
        Subcommand::OpenTabs { session } => cmd_open_tabs(session).await,
        Subcommand::Capture { pane, probe, lines } => {
            capture::cmd_capture(pane, probe, lines).await
        }
        Subcommand::Tui => cmd_tui().await,
    }
}

enum Subcommand {
    Server { foreground: bool },
    Kill,
    Switch { index: usize },
    Close { session: Option<String> },
    OpenTabs { session: Option<String> },
    Capture { pane: Option<String>, probe: bool, lines: u32 },
    Tui,
}

impl TryFrom<&[String]> for Subcommand {
    type Error = anyhow::Error;

    fn try_from(args: &[String]) -> Result<Self, Self::Error> {
        match args.first().map(String::as_str) {
            Some("server") => {
                let foreground = args[1..].iter().any(|a| a == "--foreground");
                Ok(Self::Server { foreground })
            }
            Some("kill") => Ok(Self::Kill),
            Some("switch") => {
                let index: usize = args
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .filter(|&n| n >= 1)
                    .ok_or_else(|| anyhow::anyhow!("usage: tmux-tabs switch <N>"))?;
                Ok(Self::Switch { index })
            }
            Some("close") => {
                let session = parse_session_flag(&args[1..]);
                Ok(Self::Close { session })
            }
            Some("open-tabs") => {
                let session = parse_session_flag(&args[1..]);
                Ok(Self::OpenTabs { session })
            }
            Some("capture") => {
                let (pane, probe, lines) = parse_capture_flags(&args[1..])?;
                Ok(Self::Capture { pane, probe, lines })
            }
            _ => Ok(Self::Tui),
        }
    }
}

fn parse_session_flag(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--session" {
            return args.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
        i += 1;
    }
    None
}

fn parse_capture_flags(args: &[String]) -> anyhow::Result<(Option<String>, bool, u32)> {
    let mut pane = None;
    let mut probe = false;
    let mut lines = capture::DEFAULT_LINES;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane = args.get(i + 1).cloned();
                i += 2;
            }
            "--probe" => {
                probe = true;
                i += 1;
            }
            "--lines" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--lines requires a value"))?;
                lines = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid --lines value: {value}"))?;
                i += 2;
            }
            _ => i += 1,
        }
    }
    Ok((pane, probe, lines))
}

/// Connect to the server, register, and read the initial state snapshot.
/// Returns None if the server isn't running or the handshake fails — callers
/// decide whether to fall back silently or print an error.
async fn open_state_conn() -> Option<(OwnedReadHalf, OwnedWriteHalf, Vec<SessionEntry>)> {
    let sock = socket_path();
    if !sock.exists() {
        return None;
    }
    let stream = UnixStream::connect(&sock).await.ok()?;
    let (mut reader, mut writer) = stream.into_split();
    let reg = Envelope::Client(ClientMessage::Register {
        pane_id: current_pane_id(),
    });
    write_frame(&mut writer, &reg).await.ok()?;
    let sessions = tokio::time::timeout(Duration::from_secs(1), async {
        match read_frame::<_, ServerMessage>(&mut reader).await {
            Ok(Some(ServerMessage::StateUpdate { sessions, .. })) => Some(sessions),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    Some((reader, writer, sessions))
}

/// Jump to the Nth session in tmux-tabs order (1-based; caller validates index >= 1).
async fn cmd_switch(idx: usize) -> anyhow::Result<()> {
    let Some((_reader, mut writer, sessions)) = open_state_conn().await else {
        // Silently exit — typically bound to a hot-key, so noise is bad.
        return Ok(());
    };

    let Some(entry) = sessions.get(idx - 1) else {
        return Ok(());
    };

    let switch = Envelope::Client(ClientMessage::SwitchSession {
        session_name: entry.session.name.clone(),
    });
    let _ = write_frame(&mut writer, &switch).await;
    Ok(())
}

/// Close a tmux session (and its associated Chrome tab group) after a y/n
/// prompt. Intended to be invoked inside a tmux `display-popup`.
async fn cmd_close(session: Option<String>) -> anyhow::Result<()> {
    let Some(session_name) = resolve_target_session(session).await else {
        eprintln!("could not resolve target session (not in tmux?)");
        std::process::exit(2);
    };

    let Some((mut reader, mut writer, sessions)) = open_state_conn().await else {
        eprintln!("tmux-tabs server is not running");
        std::process::exit(1);
    };

    let tab_count: u32 = sessions
        .iter()
        .find(|e| e.session.name == session_name)
        .and_then(|e| e.browser.as_ref().map(|b| b.tab_count))
        .unwrap_or(0);

    println!("Close session '{session_name}'?");
    if tab_count > 0 {
        let noun = if tab_count == 1 { "tab" } else { "tabs" };
        println!("⚠  {tab_count} Chrome {noun} will also be closed.");
    }
    print!("Continue? (y/n) ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).ok();
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        println!("Cancelled.");
        return Ok(());
    }

    // Move the current client to another session before killing this one —
    // otherwise tmux detaches the client to the parent shell.
    let other_session: Option<String> = tokio::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#S"])
        .output()
        .await
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .find(|s| s != &session_name)
        });
    if let Some(next) = other_session {
        let _ = tokio::process::Command::new("tmux")
            .args(["switch-client", "-t", &next])
            .status()
            .await;
    }

    let close = Envelope::Client(ClientMessage::CloseSession { session_name });
    if let Err(e) = write_frame(&mut writer, &close).await {
        eprintln!("failed to send close: {e}");
        return Ok(());
    }
    // Ensure the close frame is flushed before exiting: shut the write half,
    // then read until the server closes its side.
    let _ = writer.shutdown().await;
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink).await;
    Ok(())
}

/// Re-open the Chrome tab group for the target session (the current session
/// when `--session` is omitted). Best-effort and silent — typically bound to a
/// tmux key — so a missing server or absent Chrome bridge is a no-op.
async fn cmd_open_tabs(session: Option<String>) -> anyhow::Result<()> {
    let Some(session_name) = resolve_target_session(session).await else {
        eprintln!("could not resolve target session (not in tmux?)");
        std::process::exit(2);
    };

    let Ok(mut stream) = UnixStream::connect(socket_path()).await else {
        // Server not running — stay silent so a hotkey doesn't spew errors.
        return Ok(());
    };

    // A lone command frame (no Register) is handled by the server's catch-all
    // `Envelope::Client` arm. write_frame flushes, so the kernel has the bytes
    // before the stream drops — same one-shot pattern as `notify`.
    let msg = Envelope::Client(ClientMessage::OpenTabGroup { session_name });
    let _ = write_frame(&mut stream, &msg).await;
    Ok(())
}

/// Resolve the target session name for `cmd_close`: prefer the explicit value
/// from the `--session` flag (already parsed), fall back to the current tmux
/// session via `display-message`.
async fn resolve_target_session(provided: Option<String>) -> Option<String> {
    if let Some(s) = provided {
        return Some(s);
    }
    let out = tokio::process::Command::new("tmux")
        .args(["display-message", "-p", "#S"])
        .output()
        .await
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

async fn cmd_tui() -> anyhow::Result<()> {
    ensure_server().await?;

    let stream = UnixStream::connect(socket_path()).await?;
    let (reader, writer) = stream.into_split();

    let pane_id = current_pane_id();

    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
    let result = app::run(&mut terminal, reader, writer, pane_id).await;
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();

    result.map_err(Into::into)
}

/// Current tmux pane ID from the environment, falling back to a sentinel
/// value when not running inside tmux.
fn current_pane_id() -> String {
    std::env::var("TMUX_PANE").unwrap_or_else(|_| "%0".to_string())
}

/// Send a one-shot Claude Code hook notification to the server. Sync so it
/// skips the tokio runtime init — the hook script invokes this on every
/// Claude Code event and needs to return immediately. Best-effort: any I/O
/// failure is silently swallowed so the hook never blocks Claude Code. Set
/// `TMUX_TABS_HOOK_DEBUG=1` to log failure points to stderr.
fn cmd_notify_sync(args: &[String]) {
    let event_name = args.first().map_or("stop", String::as_str);
    let event = event_name.parse::<ClaudeEvent>().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let debug = std::env::var_os("TMUX_TABS_HOOK_DEBUG").is_some();
    let log = |msg: &str| {
        if debug {
            eprintln!("tmux-tabs notify: {msg}");
        }
    };

    // Both flags are optional: the server resolves session name from pane_id
    // via its cached pane map.
    let mut pane_id = String::new();
    let mut session_name = String::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane_id = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--session" => {
                session_name = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            _ => i += 1,
        }
    }

    if pane_id.is_empty() {
        pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    }
    if pane_id.is_empty() {
        log("no TMUX_PANE set");
        return;
    }

    let sock = socket_path();
    if !sock.exists() {
        log("server socket missing — is tmux-tabs-server running?");
        return;
    }

    let payload = if std::io::stdin().is_terminal() {
        None
    } else {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).ok();
        if buf.is_empty() { None } else { Some(buf) }
    };

    let notif = Envelope::Hook(HookNotification {
        tmux_pane_id: pane_id,
        session_name,
        event,
        payload,
    });

    let Ok(buf) = tmux_tabs_common::encode_frame(&notif) else {
        log("frame encoding failed");
        return;
    };

    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        log("connect to server failed");
        return;
    };
    if let Err(e) = stream.write_all(&buf) {
        log(&format!("write to server failed: {e}"));
    }
}

async fn cmd_server(foreground: bool) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("tmux-tabs-server");
    if foreground {
        cmd.arg("--foreground");
    }
    cmd.status().await?;
    Ok(())
}

fn cmd_kill() -> anyhow::Result<()> {
    let pid_path = tmux_tabs_common::pid_path();
    if pid_path.exists() {
        let pid_str = std::fs::read_to_string(&pid_path)?;
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            // SAFETY: libc::kill is FFI-safe; pid is a parsed i32 and SIGTERM is a valid signal.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
    Ok(())
}

/// Ensure the server is running; spawn it from the sibling binary if not.
async fn ensure_server() -> anyhow::Result<()> {
    let sock = socket_path();
    if UnixStream::connect(&sock).await.is_ok() {
        return Ok(());
    }

    let server_bin = which_server();
    std::process::Command::new(&server_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if UnixStream::connect(&sock).await.is_ok() {
            return Ok(());
        }
    }

    anyhow::bail!("failed to start tmux-tabs-server");
}

fn which_server() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let sibling = parent.join("tmux-tabs-server");
        if sibling.exists() {
            return sibling;
        }
    }
    // Bare filename — std::process::Command searches PATH at spawn time.
    std::path::PathBuf::from("tmux-tabs-server")
}
