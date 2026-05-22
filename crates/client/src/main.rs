mod app;
mod input;
mod ui;

use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

use tmux_tabs_common::{
    ClientMessage, Envelope, ServerMessage, SessionEntry, read_frame, socket_path, write_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => cmd_server(&args[2..]).await,
        Some("kill") => cmd_kill(),
        Some("switch") => cmd_switch(&args[2..]).await,
        Some("close") => cmd_close(&args[2..]).await,
        _ => cmd_tui().await,
    }
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

/// Jump to the Nth session in tmux-tabs order (1-based).
async fn cmd_switch(args: &[String]) -> anyhow::Result<()> {
    let idx: usize = match args.first().and_then(|s| s.parse().ok()) {
        Some(n) if n >= 1 => n,
        _ => {
            eprintln!("usage: tmux-tabs switch <N>");
            std::process::exit(2);
        }
    };

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
async fn cmd_close(args: &[String]) -> anyhow::Result<()> {
    let Some(session_name) = resolve_target_session(args).await else {
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

/// Resolve the target session name for `cmd_close`. Prefers an explicit
/// `--session <name>` flag and falls back to the current tmux session.
async fn resolve_target_session(args: &[String]) -> Option<String> {
    let mut session_name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--session" {
            session_name = args.get(i + 1).cloned();
            i += 2;
        } else {
            i += 1;
        }
    }
    if let Some(s) = session_name.filter(|s| !s.is_empty()) {
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

async fn cmd_server(args: &[String]) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("tmux-tabs-server");
    if args.iter().any(|a| a == "--foreground") {
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
