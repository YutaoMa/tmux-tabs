#[cfg(feature = "screenshots")]
mod apng;
mod app;
mod capture;
mod conn;
mod input;
#[cfg(feature = "screenshots")]
mod screenshot;
mod ui;

use std::io::{IsTerminal, Read, Write};
use std::time::Duration;

use tmux_tabs_common::{
    AgentKind, ClientMessage, Envelope, HookNotification, ServerMessage, SessionEntry, read_frame,
    socket_path, write_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Hot path: AI CLI hook notifications fire frequently. Dispatch the
    // sync path before building a tokio runtime so the hook returns ASAP.
    if args.get(1).map(String::as_str) == Some("notify") {
        cmd_notify_sync(&args[2..]);
        return Ok(());
    }

    // Dev-only README screenshot generator.
    #[cfg(feature = "screenshots")]
    if args.get(1).map(String::as_str) == Some("__screenshot") {
        let out = args.get(2).map_or("docs/images", String::as_str);
        return screenshot::run(std::path::Path::new(out));
    }

    #[cfg(feature = "screenshots")]
    if args.get(1).map(String::as_str) == Some("__apng") {
        return apng::main(&args[2..]);
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
        Subcommand::Block {
            session,
            clear,
            note,
        } => cmd_block(session, clear, note).await,
        Subcommand::OpenTabs { session } => cmd_open_tabs(session).await,
        Subcommand::Capture { pane, probe, lines } => {
            capture::cmd_capture(pane, probe, lines).await
        }
        Subcommand::Tui => cmd_tui().await,
    }
}

enum Subcommand {
    Server {
        foreground: bool,
    },
    Kill,
    Switch {
        index: usize,
    },
    Close {
        session: Option<String>,
    },
    Block {
        session: Option<String>,
        clear: bool,
        note: Option<String>,
    },
    OpenTabs {
        session: Option<String>,
    },
    Capture {
        pane: Option<String>,
        probe: bool,
        lines: u32,
    },
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
            Some("block") => {
                let (session, clear, note) = parse_block_flags(&args[1..]);
                Ok(Self::Block {
                    session,
                    clear,
                    note,
                })
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

/// Parse `block` arguments: `[--session NAME] [--clear] [note words…]`.
/// Free-standing words are joined into the note so the shell doesn't have to
/// quote it.
fn parse_block_flags(args: &[String]) -> (Option<String>, bool, Option<String>) {
    let mut session = None;
    let mut clear = false;
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session = args.get(i + 1).cloned().filter(|s| !s.is_empty());
                i += 2;
            }
            "--clear" => {
                clear = true;
                i += 1;
            }
            other => {
                words.push(other);
                i += 1;
            }
        }
    }
    let note = (!words.is_empty()).then(|| words.join(" "));
    (session, clear, note)
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

/// Mark the target session blocked on something outside the terminal, or
/// clear an existing mark. Bound to a tmux key so a blocker can be recorded
/// from whatever pane you're in when you discover it, without first hopping to
/// the sidebar.
///
/// With no note and no `--clear` it prompts, which is what makes it usable
/// from `display-popup -E`; the prompt toggles, mirroring `b` in the sidebar.
async fn cmd_block(
    session: Option<String>,
    clear: bool,
    note: Option<String>,
) -> anyhow::Result<()> {
    let Some(session_name) = resolve_target_session(session).await else {
        eprintln!("could not resolve target session (not in tmux?)");
        std::process::exit(2);
    };

    let Some((_reader, mut writer, sessions)) = open_state_conn().await else {
        eprintln!("tmux-tabs server is not running");
        std::process::exit(1);
    };

    let existing = sessions
        .iter()
        .find(|e| e.session.name == session_name)
        .and_then(|e| e.blocker.clone());

    let note = if clear {
        if existing.is_none() {
            println!("'{session_name}' is not blocked.");
            return Ok(());
        }
        None
    } else if let Some(note) = note {
        Some(clamp_note(&note))
    } else {
        match prompt_blocker(&session_name, existing.as_deref()) {
            // Nothing to say and nothing to clear — leave state untouched
            // rather than writing a blank overlay.
            PromptOutcome::Cancel => return Ok(()),
            PromptOutcome::Clear => None,
            PromptOutcome::Note(note) => Some(note),
        }
    };

    let msg = Envelope::Client(ClientMessage::SetBlocker {
        session_name,
        note: note.clone(),
    });
    if let Err(e) = write_frame(&mut writer, &msg).await {
        eprintln!("failed to send: {e}");
        return Ok(());
    }
    // The popup closes the moment this returns, so confirm what landed.
    match note {
        Some(note) => println!("Blocked: {note}"),
        None => println!("Blocker cleared."),
    }
    Ok(())
}

enum PromptOutcome {
    Cancel,
    Clear,
    Note(String),
}

/// Interactive half of `cmd_block`. Returns what the user chose to do.
fn prompt_blocker(session_name: &str, existing: Option<&str>) -> PromptOutcome {
    if let Some(existing) = existing {
        println!("'{session_name}' is blocked on:");
        println!("  {existing}");
        print!("Clear it? (y/n) ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        return if matches!(answer.trim(), "y" | "Y" | "yes") {
            PromptOutcome::Clear
        } else {
            println!("Left as-is.");
            PromptOutcome::Cancel
        };
    }

    println!("Block '{session_name}' on what? (empty to cancel)");
    print!("> ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    let note = clamp_note(&line);
    if note.is_empty() {
        println!("Cancelled.");
        return PromptOutcome::Cancel;
    }
    PromptOutcome::Note(note)
}

/// Trim a note and cap it to the sidebar's input budget. Measured in terminal
/// columns, so a note reads the same here as it will on the card; the overlay
/// ellipsizes anything that still doesn't fit once wrapped.
fn clamp_note(note: &str) -> String {
    ui::truncate_note(note.trim())
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
    let (events, commands) = conn::spawn(conn::Config {
        socket: socket_path(),
        pane_id: current_pane_id(),
        autostart: true,
    });

    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
    let result = app::run(&mut terminal, events, commands).await;
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();

    result.map_err(Into::into)
}

/// Current tmux pane ID from the environment, falling back to a sentinel
/// value when not running inside tmux.
fn current_pane_id() -> String {
    std::env::var("TMUX_PANE").unwrap_or_else(|_| "%0".to_string())
}

/// Send a one-shot AI CLI hook notification to the server. Sync so it skips
/// the tokio runtime init — hook scripts invoke this on every event and need
/// to return immediately. Best-effort: any I/O failure is silently swallowed
/// so the hook never blocks the calling AI CLI. Set `TMUX_TABS_HOOK_DEBUG=1`
/// to log failure points to stderr.
fn cmd_notify_sync(args: &[String]) {
    let event = args.first().cloned().unwrap_or_else(|| "stop".to_string());

    let debug = std::env::var_os("TMUX_TABS_HOOK_DEBUG").is_some();
    let log = |msg: &str| {
        if debug {
            eprintln!("tmux-tabs notify: {msg}");
        }
    };

    // All flags are optional: the server resolves session name from pane_id
    // via its cached pane map.
    let mut pane_id = String::new();
    let mut session_name = String::new();
    let mut agent = AgentKind::Claude;
    let mut copilot_session_id: Option<String> = None;
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
            "--agent" => {
                let value = args.get(i + 1).map_or("", String::as_str);
                agent = match value {
                    "copilot" => AgentKind::Copilot,
                    "claude" | "" => AgentKind::Claude,
                    other => {
                        eprintln!("unknown agent: {other}");
                        std::process::exit(1);
                    }
                };
                i += 2;
            }
            "--copilot-session-id" => {
                copilot_session_id = args.get(i + 1).cloned().filter(|v| !v.is_empty());
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
        agent,
        event,
        payload,
        copilot_session_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn block_parses_a_bare_note_without_quoting() {
        let (session, clear, note) = parse_block_flags(&argv("waiting on jane"));
        assert!(session.is_none());
        assert!(!clear);
        assert_eq!(note.as_deref(), Some("waiting on jane"));
    }

    #[test]
    fn block_parses_flags_alongside_a_note() {
        let (session, clear, note) = parse_block_flags(&argv("--session api ask bob"));
        assert_eq!(session.as_deref(), Some("api"));
        assert!(!clear);
        assert_eq!(note.as_deref(), Some("ask bob"));
    }

    #[test]
    fn block_recognises_the_clear_flag() {
        let (_, clear, note) = parse_block_flags(&argv("--clear"));
        assert!(clear);
        assert!(note.is_none());
    }

    /// No note and no flag is the popup path — it must not be mistaken for a
    /// request to store an empty note.
    #[test]
    fn block_with_no_arguments_asks_for_a_prompt() {
        let (session, clear, note) = parse_block_flags(&[]);
        assert!(session.is_none() && !clear && note.is_none());
    }

    #[test]
    fn block_subcommand_dispatches() {
        let args = argv("block --clear --session api");
        match Subcommand::try_from(&args[..]).expect("parse") {
            Subcommand::Block {
                session,
                clear,
                note,
            } => {
                assert_eq!(session.as_deref(), Some("api"));
                assert!(clear);
                assert!(note.is_none());
            }
            _ => panic!("expected the block subcommand"),
        }
    }

    #[test]
    fn notes_are_trimmed_and_capped_to_what_the_overlay_can_show() {
        assert_eq!(clamp_note("  ask jane \n"), "ask jane");
        let long = "x".repeat(ui::BLOCKER_MAX_CELLS + 25);
        assert_eq!(clamp_note(&long).chars().count(), ui::BLOCKER_MAX_CELLS);
    }

    /// Capping by chars, not bytes: a multi-byte note must not be split
    /// mid-character.
    #[test]
    fn capping_a_multibyte_note_does_not_corrupt_it() {
        let long = "é".repeat(ui::BLOCKER_MAX_CELLS + 10);
        let capped = clamp_note(&long);
        assert_eq!(capped.chars().count(), ui::BLOCKER_MAX_CELLS);
        assert!(capped.chars().all(|c| c == 'é'));
    }
}
