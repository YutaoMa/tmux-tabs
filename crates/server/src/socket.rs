//! Server-side socket protocol. Handles `ClientMessage`, `Envelope::Hook`
//! (Claude Code / Copilot CLI), and `Envelope::Bridge` (Chrome extension).

use std::time::{SystemTime, UNIX_EPOCH};

use tmux_tabs_common::{
    AgentKind, BridgeMessage, ClientMessage, Envelope, HookNotification, read_frame, socket_dir,
    write_frame,
};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

use crate::state::AppState;
use crate::tmux;

/// Browser payloads up to this size go inline in the prompt; larger payloads
/// are written to a temp file and referenced by path.
const INLINE_CHAR_LIMIT: usize = 8_000;

pub async fn listen(listener: UnixListener, state: AppState) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state).await {
                        warn!("connection error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("accept error: {e}");
            }
        }
    }
}

async fn handle_connection(stream: UnixStream, state: AppState) -> anyhow::Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    let Some(envelope): Option<Envelope> = read_frame(&mut reader).await? else {
        return Ok(());
    };

    match envelope {
        Envelope::Hook(notif) => {
            handle_hook(notif, &state).await;
            Ok(())
        }
        Envelope::Bridge(BridgeMessage::Register) => {
            info!("bridge registered");
            let mut rx = state.register_bridge().await;
            state.broadcast().await;

            let write_task = tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    if write_frame(&mut writer, &cmd).await.is_err() {
                        break;
                    }
                }
            });

            loop {
                let msg: Option<Envelope> = read_frame(&mut reader).await?;
                match msg {
                    Some(Envelope::Bridge(BridgeMessage::TabGroupState { groups })) => {
                        if state.update_tab_groups(groups).await {
                            state.broadcast().await;
                        }
                    }
                    Some(Envelope::Bridge(BridgeMessage::SwitchSession { session_name })) => {
                        info!("bridge switch: {session_name}");
                        if let Err(e) = tmux::switch_session(&session_name).await {
                            warn!("bridge switch failed: {e}");
                        }
                        if let Ok(sessions) = tmux::list_sessions().await {
                            state.update_sessions(sessions).await;
                            state.broadcast().await;
                        }
                    }
                    Some(Envelope::Bridge(BridgeMessage::SendToPane { text, url, title })) => {
                        if let Err(e) = handle_send_to_pane(&state, &text, &url, &title).await {
                            warn!("send-to-pane failed: {e}");
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }

            write_task.abort();
            state.remove_bridge().await;
            info!("bridge disconnected");
            Ok(())
        }
        Envelope::Bridge(_) => Ok(()),
        Envelope::Client(ClientMessage::Register { pane_id }) => {
            // Prefer the cached pane→session map; only spawn a tmux subprocess
            // for brand-new panes the poller hasn't seen yet.
            let session_name = match state.session_for_pane(&pane_id).await {
                Some(name) => name,
                None => tmux::pane_session_name(&pane_id).await.unwrap_or_default(),
            };
            info!("client registered: pane={pane_id} session={session_name}");

            let mut rx = state.register_client(pane_id.clone(), session_name).await;
            state.broadcast().await;

            let write_task = tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    if write_frame(&mut writer, &msg).await.is_err() {
                        break;
                    }
                }
            });

            loop {
                let msg: Option<Envelope> = read_frame(&mut reader).await?;
                match msg {
                    Some(Envelope::Client(cmd)) => {
                        handle_client_command(cmd, &state).await;
                    }
                    Some(Envelope::Hook(notif)) => handle_hook(notif, &state).await,
                    Some(Envelope::Bridge(_)) => {}
                    None => break,
                }
            }

            write_task.abort();
            state.remove_client(&pane_id).await;
            info!("client disconnected: pane={pane_id}");
            Ok(())
        }
        Envelope::Client(cmd) => {
            handle_client_command(cmd, &state).await;
            Ok(())
        }
    }
}

async fn handle_hook(notif: HookNotification, state: &AppState) {
    let session_name = if !notif.session_name.is_empty() {
        notif.session_name.clone()
    } else if let Some(name) = state.session_for_pane(&notif.tmux_pane_id).await {
        name
    } else {
        match tmux::pane_session_name(&notif.tmux_pane_id).await {
            Ok(name) if !name.is_empty() => name,
            _ => {
                warn!(
                    "hook: could not resolve session for pane {}",
                    notif.tmux_pane_id
                );
                return;
            }
        }
    };

    debug!(
        "hook: agent={:?} event={} session={} pane={}",
        notif.agent, notif.event, session_name, notif.tmux_pane_id
    );

    if notif.agent == AgentKind::Copilot && notif.event == "sessionStart" {
        match notif.copilot_session_id {
            Some(sid) if !sid.is_empty() => {
                state
                    .register_copilot_session(session_name, notif.tmux_pane_id, sid)
                    .await;
            }
            _ => warn!("copilot sessionStart hook missing copilot_session_id"),
        }
        return;
    }

    let changed = state
        .handle_agent_event(
            &session_name,
            &notif.tmux_pane_id,
            notif.agent,
            &notif.event,
            notif.payload.as_deref(),
        )
        .await;
    if changed {
        state.broadcast().await;
    }
}

async fn handle_send_to_pane(
    state: &AppState,
    text: &str,
    url: &str,
    title: &str,
) -> anyhow::Result<()> {
    let pane_id = state
        .agent_pane(AgentKind::Claude)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Claude Code pane found for the active session"))?;

    let source = if !title.is_empty() && !url.is_empty() {
        format!("{title}\n{url}")
    } else if !url.is_empty() {
        url.to_string()
    } else {
        String::new()
    };

    let prompt = if text.len() <= INLINE_CHAR_LIMIT {
        if source.is_empty() {
            text.to_string()
        } else {
            format!("From {source}:\n---\n{text}\n---")
        }
    } else {
        // Write to a temp file under the socket dir and reference it by path.
        let dir = socket_dir();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = dir.join(format!("page-{ts}.txt"));
        let mut buf = Vec::with_capacity(source.len() + text.len() + 32);
        if !source.is_empty() {
            buf.extend_from_slice(format!("Source: {source}\n---\n").as_bytes());
        }
        buf.extend_from_slice(text.as_bytes());
        tokio::fs::write(&path, &buf).await?;
        let path_str = path.display();
        format!("Read {path_str} for context from the browser and use it for your current task")
    };

    info!("send-to-pane: {} chars to {pane_id}", text.len());
    tmux::send_keys(&pane_id, &prompt).await?;
    Ok(())
}

async fn handle_client_command(cmd: ClientMessage, state: &AppState) {
    let result = match cmd {
        ClientMessage::Register { .. } => return,
        ClientMessage::SwitchSession { session_name } => tmux::switch_session(&session_name).await,
        ClientMessage::RenameSession { old_name, new_name } => {
            tmux::rename_session(&old_name, &new_name).await
        }
        ClientMessage::CloseSession { session_name } => {
            // Ask the bridge to close the matching tab group first (best-effort),
            // then kill the tmux session.
            state.close_tab_group(&session_name).await;
            tmux::kill_session(&session_name).await
        }
        ClientMessage::OpenTabGroup { session_name } => {
            // Re-open the tab group only — no tmux change, so skip the
            // list/broadcast below; the extension reports the recreated group.
            state.open_tab_group(&session_name).await;
            return;
        }
    };
    if let Err(e) = result {
        warn!("command failed: {e}");
    }
    if let Ok(sessions) = tmux::list_sessions().await {
        state.update_sessions(sessions).await;
        state.broadcast().await;
    }
}
