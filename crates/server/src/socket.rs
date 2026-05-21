//! Server-side socket protocol. Currently handles `ClientMessage` and drops
//! `Envelope::Hook` / `Envelope::Bridge` — Claude Code and Chrome bridge
//! integrations land in later PRs along with their senders.

use tmux_tabs_common::{ClientMessage, Envelope, HookNotification, read_frame, write_frame};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

use crate::state::AppState;
use crate::tmux;

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
            log_dropped_hook(&notif);
            Ok(())
        }
        Envelope::Bridge(_) => {
            debug!("dropped bridge message");
            Ok(())
        }
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
                    Some(Envelope::Hook(notif)) => log_dropped_hook(&notif),
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

fn log_dropped_hook(notif: &HookNotification) {
    debug!(
        "dropped hook: {:?} pane={}",
        notif.event, notif.tmux_pane_id
    );
}

async fn handle_client_command(cmd: ClientMessage, state: &AppState) {
    let result = match cmd {
        ClientMessage::Register { .. } => return,
        ClientMessage::SwitchSession { session_name } => tmux::switch_session(&session_name).await,
        ClientMessage::RenameSession { old_name, new_name } => {
            tmux::rename_session(&old_name, &new_name).await
        }
        ClientMessage::CloseSession { session_name } => tmux::kill_session(&session_name).await,
    };
    if let Err(e) = result {
        warn!("command failed: {e}");
    }
    if let Ok(sessions) = tmux::list_sessions().await {
        state.update_sessions(sessions).await;
        state.broadcast().await;
    }
}
