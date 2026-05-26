use std::collections::HashMap;
use std::process::Stdio;

use tmux_tabs_common::TmuxSession;
use tokio::process::Command;

/// Run a tmux subcommand with stdout/stderr suppressed. Used for fire-and-forget
/// commands like switch-client, rename-session, kill-session.
async fn tmux_run(args: &[&str]) -> anyhow::Result<()> {
    Command::new("tmux")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    Ok(())
}

/// Run a tmux subcommand and return its captured stdout.
async fn tmux_capture(args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = Command::new("tmux")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;
    Ok(output.stdout)
}

/// Poll tmux for the current list of sessions.
///
/// # Errors
/// Returns an error if the `tmux list-sessions` subprocess fails to spawn or run.
pub async fn list_sessions() -> anyhow::Result<Vec<TmuxSession>> {
    let stdout = tmux_capture(&[
        "list-sessions",
        "-F",
        "#{session_id}\t#{session_name}\t#{session_windows}\t#{?session_attached,1,0}\t#{session_activity}\t#{pane_current_path}",
    ])
    .await?;

    let text = String::from_utf8_lossy(&stdout);
    let mut sessions = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 6 {
            continue;
        }
        let cwd = if parts[5].is_empty() {
            None
        } else {
            Some(parts[5].to_string())
        };
        sessions.push(TmuxSession {
            id: parts[0].to_string(),
            name: parts[1].to_string(),
            windows: parts[2].parse().unwrap_or(0),
            attached: parts[3] == "1",
            activity: parts[4].parse().unwrap_or(0),
            cwd,
        });
    }
    Ok(sessions)
}

pub async fn switch_session(name: &str) -> anyhow::Result<()> {
    tmux_run(&["switch-client", "-t", name]).await
}

pub async fn rename_session(old: &str, new: &str) -> anyhow::Result<()> {
    tmux_run(&["rename-session", "-t", old, new]).await
}

pub async fn kill_session(name: &str) -> anyhow::Result<()> {
    tmux_run(&["kill-session", "-t", name]).await
}

/// Send literal text to a tmux pane via `send-keys -l` (no implicit Enter,
/// so the user can review and edit before submitting).
pub async fn send_keys(pane_id: &str, text: &str) -> anyhow::Result<()> {
    tmux_run(&["send-keys", "-t", pane_id, "-l", text]).await
}

/// Get all active panes mapped to their session name. One tmux call serves
/// both dead-pane sweeps and the pane→session cache used by hook handlers.
///
/// # Errors
/// Returns an error if the `tmux list-panes` subprocess fails.
pub async fn list_panes_with_sessions() -> anyhow::Result<HashMap<String, String>> {
    let stdout = tmux_capture(&["list-panes", "-a", "-F", "#{pane_id}\t#{session_name}"]).await?;
    let text = String::from_utf8_lossy(&stdout);
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut parts = line.splitn(2, '\t');
        if let (Some(p), Some(s)) = (parts.next(), parts.next()) {
            map.insert(p.trim().to_string(), s.trim().to_string());
        }
    }
    Ok(map)
}

/// Resolve which session a pane belongs to.
///
/// # Errors
/// Returns an error if the `tmux display-message` subprocess fails.
pub async fn pane_session_name(pane_id: &str) -> anyhow::Result<String> {
    let stdout = tmux_capture(&["display-message", "-t", pane_id, "-p", "#{session_name}"]).await?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}
