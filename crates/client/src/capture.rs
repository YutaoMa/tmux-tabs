use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

pub const DEFAULT_LINES: u32 = 200;

/// Panes running this command are the tmux-tabs TUI sidebar — always exclude
/// them from auto-selection candidates.
const SIDEBAR_COMMAND: &str = "tmux-tabs";

const PANE_FORMAT: &str = "#{pane_id}\t#{pane_current_command}\t#{window_name}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_activity}";

#[derive(Debug, Serialize)]
struct PaneGeometry {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
}

#[derive(Debug, Serialize)]
struct PaneInfo {
    id: String,
    command: String,
    window: String,
    geometry: PaneGeometry,
    activity: u64,
}

#[derive(Debug, Serialize)]
struct ProbeOutput {
    caller: String,
    window: String,
    panes: Vec<PaneInfo>,
}

/// Run a tmux subcommand, capturing stdout and discarding stderr.
async fn run_tmux(args: &[&str]) -> anyhow::Result<std::process::Output> {
    let output = Command::new("tmux")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;
    Ok(output)
}

/// Returns (`window_id`, `session_name`) for a given pane.
async fn resolve_caller_context(pane_id: &str) -> anyhow::Result<(String, String)> {
    let output = run_tmux(&[
        "display-message",
        "-t",
        pane_id,
        "-p",
        "#{window_id}\t#{session_name}",
    ])
    .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = text.trim().split('\t').collect();
    if parts.len() < 2 {
        anyhow::bail!("failed to resolve pane context for {pane_id}");
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn parse_pane_line(line: &str) -> Option<PaneInfo> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 8 {
        return None;
    }
    Some(PaneInfo {
        id: parts[0].to_string(),
        command: parts[1].to_string(),
        window: parts[2].to_string(),
        geometry: PaneGeometry {
            left: parts[3].parse().unwrap_or(0),
            top: parts[4].parse().unwrap_or(0),
            width: parts[5].parse().unwrap_or(0),
            height: parts[6].parse().unwrap_or(0),
        },
        activity: parts[7].parse().unwrap_or(0),
    })
}

async fn list_window_panes(window_id: &str) -> anyhow::Result<Vec<PaneInfo>> {
    let output = run_tmux(&["list-panes", "-t", window_id, "-F", PANE_FORMAT]).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().filter_map(parse_pane_line).collect())
}

async fn list_session_panes(session_name: &str) -> anyhow::Result<Vec<PaneInfo>> {
    let output = run_tmux(&["list-panes", "-s", "-t", session_name, "-F", PANE_FORMAT]).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().filter_map(parse_pane_line).collect())
}

/// Capture pane content. Uses `-S -<lines>` to include scrollback.
async fn capture_pane_content(pane_id: &str, lines: u32) -> anyhow::Result<String> {
    let start = format!("-{lines}");
    let output = run_tmux(&["capture-pane", "-t", pane_id, "-p", "-S", &start]).await?;
    if !output.status.success() {
        anyhow::bail!("tmux capture-pane failed for {pane_id}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn capture_and_print(pane_id: &str, lines: u32) -> anyhow::Result<()> {
    let content = capture_pane_content(pane_id, lines).await?;
    print!("{content}");
    Ok(())
}

pub async fn cmd_capture(
    pane: Option<String>,
    probe: bool,
    lines: u32,
) -> anyhow::Result<()> {
    // Explicit pane → just capture it. Works even outside tmux.
    if let Some(pane) = pane.as_deref() {
        return capture_and_print(pane, lines).await;
    }

    let Some(caller_id) = std::env::var("TMUX_PANE").ok() else {
        eprintln!("error: not running inside tmux (TMUX_PANE not set)");
        std::process::exit(1);
    };
    let (window_id, session_name) = resolve_caller_context(&caller_id).await?;

    // Auto mode skips the caller and the tmux-tabs sidebar; probe mode only
    // skips the caller (so the sidebar pane is selectable for explicit grabs).
    let is_capturable = |p: &PaneInfo| p.id != caller_id && p.command != SIDEBAR_COMMAND;

    if probe {
        let candidates: Vec<PaneInfo> = list_session_panes(&session_name)
            .await?
            .into_iter()
            .filter(|p| p.id != caller_id)
            .collect();
        let output = ProbeOutput {
            caller: caller_id,
            window: window_id,
            panes: candidates,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Auto mode — try same window first, then fall back to session.
    let mut candidates: Vec<PaneInfo> = list_window_panes(&window_id)
        .await?
        .into_iter()
        .filter(&is_capturable)
        .collect();

    if candidates.len() == 1 {
        return capture_and_print(&candidates[0].id, lines).await;
    }

    if candidates.is_empty() {
        candidates = list_session_panes(&session_name)
            .await?
            .into_iter()
            .filter(&is_capturable)
            .collect();

        if candidates.len() == 1 {
            return capture_and_print(&candidates[0].id, lines).await;
        }

        if candidates.is_empty() {
            eprintln!("error: no other panes found in session");
            std::process::exit(1);
        }
    }

    // Multiple candidates — emit probe JSON and exit 2 for the caller (e.g. an
    // LLM running /grab) to disambiguate.
    let output = ProbeOutput {
        caller: caller_id,
        window: window_id,
        panes: candidates,
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    std::process::exit(2);
}
