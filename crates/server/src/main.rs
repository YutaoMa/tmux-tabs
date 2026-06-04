mod agent;
mod browser;
mod git;
mod socket;
mod state;
mod tmux;

use std::fs;
use std::time::Duration;

use state::AppState;
use tmux_tabs_common::{pid_path, socket_dir, socket_path};
use tokio::net::UnixListener;
use tokio::signal;
use tracing::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let foreground = std::env::args().any(|a| a == "--foreground");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tmux_tabs_server=info".into()),
        )
        .with_target(false)
        .init();

    let dir = socket_dir();
    fs::create_dir_all(&dir)?;

    let sock = socket_path();
    if sock.exists() {
        // Probe connectability to distinguish a running server from a stale socket file.
        match tokio::net::UnixStream::connect(&sock).await {
            Ok(_) => {
                error!("another tmux-tabs-server is already running");
                std::process::exit(1);
            }
            Err(_) => {
                fs::remove_file(&sock)?;
            }
        }
    }

    fs::write(pid_path(), std::process::id().to_string())?;
    let listener = UnixListener::bind(&sock)?;
    info!("listening on {}", sock.display());

    let state = AppState::new();

    let poll_state = state.clone();
    let poll_task = tokio::spawn(async move {
        loop {
            let mut changed = false;
            match tmux::list_sessions().await {
                Ok(sessions) => {
                    changed |= poll_state.update_sessions(sessions).await;
                }
                Err(e) => {
                    tracing::warn!("tmux poll error: {e}");
                }
            }
            if let Ok(panes) = tmux::list_panes_with_sessions().await {
                changed |= poll_state.refresh_pane_map(panes).await;
            }
            changed |= poll_state.update_git().await;
            if changed {
                poll_state.broadcast().await;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    });

    let socket_state = state.clone();
    let socket_task = tokio::spawn(async move {
        socket::listen(listener, socket_state).await;
    });

    if foreground {
        signal::ctrl_c().await?;
    } else {
        // Daemon mode also shuts down on SIGTERM so systemd / brew services exit cleanly.
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    info!("shutting down");

    poll_task.abort();
    socket_task.abort();
    let _ = fs::remove_file(socket_path());
    let _ = fs::remove_file(pid_path());

    Ok(())
}
