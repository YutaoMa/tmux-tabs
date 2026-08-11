//! Server connection for the sidebar TUI: owns the socket, starts the daemon
//! when it isn't running, and reconnects on its own so the TUI never has to
//! care that the link dropped.
//!
//! Both halves matter for a sidebar that outlives individual tmux sessions.
//! [`spawn_server`] detaches the daemon with `setsid`: a server left in the
//! spawning pane's process group is SIGHUP'd when that one session is killed,
//! which used to take every other session's sidebar down with it. The
//! reconnect loop then covers the server dying anyway (crash, upgrade,
//! `tmux-tabs kill`).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tmux_tabs_common::{
    ClientMessage, Envelope, ServerMessage, SessionEntry, read_frame, write_frame,
};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Delay before the first reconnect attempt, and the ceiling the backoff grows
/// to. Kept short: reconnecting is cheap and the sidebar shows stale state
/// until it succeeds.
const RECONNECT_DELAY: Duration = Duration::from_millis(250);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(2);

/// How long to wait for a just-spawned server to start listening.
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(2);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What the connection task reports up to the TUI.
#[derive(Debug)]
pub enum ConnEvent {
    State {
        sessions: Vec<SessionEntry>,
        current_session: String,
    },
    /// The link dropped and the task is retrying. The TUI keeps its last known
    /// state on screen instead of exiting.
    Disconnected,
    /// The server asked clients to stop.
    Shutdown,
}

pub struct Config {
    pub socket: PathBuf,
    pub pane_id: String,
    /// Start the server when a connect attempt fails.
    pub autostart: bool,
}

/// What to do once a connection ends.
enum Outcome {
    /// The link dropped; reconnect.
    Retry,
    /// Terminal: the TUI is gone, or the server asked clients to stop.
    Stop,
}

/// Start the connection task. The returned channels stay valid across
/// reconnects.
pub fn spawn(config: Config) -> (mpsc::Receiver<ConnEvent>, mpsc::Sender<ClientMessage>) {
    let (evt_tx, evt_rx) = mpsc::channel(16);
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    tokio::spawn(run(config, evt_tx, cmd_rx));
    (evt_rx, cmd_tx)
}

async fn run(
    config: Config,
    evt_tx: mpsc::Sender<ConnEvent>,
    mut cmd_rx: mpsc::Receiver<ClientMessage>,
) {
    let mut delay = RECONNECT_DELAY;
    loop {
        if let Some(stream) = connect(&config).await {
            delay = RECONNECT_DELAY;
            match serve(stream, &config.pane_id, &evt_tx, &mut cmd_rx).await {
                Outcome::Stop => return,
                Outcome::Retry => {}
            }
            if evt_tx.send(ConnEvent::Disconnected).await.is_err() {
                return;
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_DELAY_MAX);
    }
}

/// Connect to the server, starting it if it isn't listening yet.
async fn connect(config: &Config) -> Option<UnixStream> {
    if let Ok(stream) = UnixStream::connect(&config.socket).await {
        return Some(stream);
    }
    if !config.autostart {
        return None;
    }
    spawn_server();

    let deadline = tokio::time::Instant::now() + SERVER_START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
        if let Ok(stream) = UnixStream::connect(&config.socket).await {
            return Some(stream);
        }
    }
    None
}

/// Drive one connection until it ends.
async fn serve(
    stream: UnixStream,
    pane_id: &str,
    evt_tx: &mpsc::Sender<ConnEvent>,
    cmd_rx: &mut mpsc::Receiver<ClientMessage>,
) -> Outcome {
    let (mut reader, mut writer) = stream.into_split();

    let reg = Envelope::Client(ClientMessage::Register {
        pane_id: pane_id.to_string(),
    });
    if write_frame(&mut writer, &reg).await.is_err() {
        return Outcome::Retry;
    }

    // `read_frame` is not cancel-safe — it reads a length, then the body — so
    // it gets its own task rather than a `select!` branch that could drop it
    // mid-frame and desynchronise the stream.
    let tx = evt_tx.clone();
    let mut reader_task = tokio::spawn(async move {
        while let Ok(Some(msg)) = read_frame::<_, ServerMessage>(&mut reader).await {
            let evt = match msg {
                ServerMessage::StateUpdate {
                    sessions,
                    current_session,
                } => ConnEvent::State {
                    sessions,
                    current_session,
                },
                ServerMessage::Shutdown => {
                    let _ = tx.send(ConnEvent::Shutdown).await;
                    return Outcome::Stop;
                }
            };
            if tx.send(evt).await.is_err() {
                return Outcome::Stop;
            }
        }
        Outcome::Retry
    });

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    reader_task.abort();
                    return Outcome::Stop;
                };
                if write_frame(&mut writer, &Envelope::Client(cmd)).await.is_err() {
                    reader_task.abort();
                    return Outcome::Retry;
                }
            }
            res = &mut reader_task => return res.unwrap_or(Outcome::Retry),
        }
    }
}

/// Start the daemon in its own session, detached from this process's terminal
/// and process group, so no pane's death can signal it.
fn spawn_server() {
    let mut cmd = tokio::process::Command::new(which_server());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure runs in the forked child before `exec`, so it may
    // only call async-signal-safe functions. `setsid` is one.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Tokio reaps orphaned children, so a server that exits immediately
    // (because another one already holds the socket) leaves no zombie.
    let _ = cmd.spawn();
}

fn which_server() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let sibling = parent.join("tmux-tabs-server");
        if sibling.exists() {
            return sibling;
        }
    }
    // Bare filename — `Command` searches PATH at spawn time.
    PathBuf::from("tmux-tabs-server")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    use tmux_tabs_common::TmuxSession;
    use tokio::net::UnixListener;

    use super::*;

    /// A socket path that cleans itself up, including when a test panics —
    /// cleanup at the end of a test body is skipped on unwind and leaks into
    /// `/tmp` on every failing run.
    struct TempSocket(PathBuf);

    impl TempSocket {
        /// Short path on purpose: unix socket paths are capped near 104 bytes.
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from("/tmp")
                .join(format!("tt-conn-{}-{tag}-{n}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn test_config(socket: &Path) -> Config {
        Config {
            socket: socket.to_path_buf(),
            pane_id: "%1".to_string(),
            // Never let a unit test spawn a real daemon.
            autostart: false,
        }
    }
    fn sample_update() -> ServerMessage {
        ServerMessage::StateUpdate {
            sessions: vec![SessionEntry {
                session: TmuxSession {
                    id: "$0".into(),
                    name: "main".into(),
                    windows: 1,
                    attached: true,
                    activity: 0,
                    cwd: None,
                },
                agent: tmux_tabs_common::AgentStatus::None,
                topic: None,
                context_pct: None,
                git: tmux_tabs_common::GitInfo::default(),
                browser: None,
            }],
            current_session: "main".into(),
        }
    }

    /// Accept one client, check it registers, and push a state update.
    async fn accept_and_register(listener: &UnixListener) -> UnixStream {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        let envelope: Option<Envelope> = read_frame(&mut stream).await.expect("read register");
        assert!(
            matches!(
                envelope,
                Some(Envelope::Client(ClientMessage::Register { ref pane_id })) if pane_id == "%1"
            ),
            "client must register on every (re)connection"
        );
        write_frame(&mut stream, &sample_update())
            .await
            .expect("send state");
        stream
    }

    #[tokio::test]
    async fn reconnects_after_the_server_disappears() {
        let socket = TempSocket::new("reconnect");
        let listener = UnixListener::bind(socket.path()).expect("bind test socket");

        let (mut events, _commands) = spawn(test_config(socket.path()));

        let first = accept_and_register(&listener).await;
        assert!(matches!(events.recv().await, Some(ConnEvent::State { .. })));

        drop(first);
        assert!(
            matches!(events.recv().await, Some(ConnEvent::Disconnected)),
            "the TUI should be told the link dropped, not be shut down"
        );

        let _second = accept_and_register(&listener).await;
        assert!(
            matches!(events.recv().await, Some(ConnEvent::State { .. })),
            "sidebar should recover on its own once the server is back"
        );
    }

    #[tokio::test]
    async fn forwards_outbound_commands() {
        let socket = TempSocket::new("outbound");
        let listener = UnixListener::bind(socket.path()).expect("bind test socket");

        let (mut events, commands) = spawn(test_config(socket.path()));

        let mut stream = accept_and_register(&listener).await;
        assert!(matches!(events.recv().await, Some(ConnEvent::State { .. })));

        commands
            .send(ClientMessage::SwitchSession {
                session_name: "other".into(),
            })
            .await
            .expect("queue command");

        let envelope: Option<Envelope> = read_frame(&mut stream).await.expect("read command");
        assert!(matches!(
            envelope,
            Some(Envelope::Client(ClientMessage::SwitchSession { ref session_name }))
                if session_name == "other"
        ));
    }

    #[tokio::test]
    async fn server_shutdown_ends_the_task() {
        let socket = TempSocket::new("shutdown");
        let listener = UnixListener::bind(socket.path()).expect("bind test socket");

        let (mut events, _commands) = spawn(test_config(socket.path()));

        let mut stream = accept_and_register(&listener).await;
        assert!(matches!(events.recv().await, Some(ConnEvent::State { .. })));

        write_frame(&mut stream, &ServerMessage::Shutdown)
            .await
            .expect("send shutdown");

        assert!(matches!(events.recv().await, Some(ConnEvent::Shutdown)));
        assert!(
            events.recv().await.is_none(),
            "task should stop after an explicit shutdown"
        );
    }
}
