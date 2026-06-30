use std::io::{Read as _, Write as _};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tmux_tabs_common::{
    BridgeCommand, BridgeMessage, Envelope, TabGroupInfo, read_frame, socket_path, write_frame,
};
use tokio::sync::mpsc;
use tracing::info;

const MAX_NATIVE_MESSAGE_BYTES: usize = 1_048_576;

/// Read one Chrome native-messaging frame from stdin (4-byte little-endian
/// length prefix + JSON body).
fn read_native_message() -> anyhow::Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    std::io::stdin()
        .read_exact(&mut len_buf)
        .context("read native message length")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_NATIVE_MESSAGE_BYTES {
        anyhow::bail!("native message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    std::io::stdin()
        .read_exact(&mut buf)
        .context("read native message body")?;
    serde_json::from_slice(&buf).context("parse native message")
}

/// Write one Chrome native-messaging frame to stdout.
fn write_native_message(msg: &impl Serialize) -> anyhow::Result<()> {
    let data = serde_json::to_vec(msg)?;
    let len = u32::try_from(data.len()).context("native message exceeds u32 length")?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&len.to_le_bytes())?;
    stdout.write_all(&data)?;
    stdout.flush()?;
    Ok(())
}

/// Forward a message to the extension on the blocking-IO pool so the async
/// runtime isn't stalled by the stdout write. Errors are swallowed — Chrome
/// disconnect is detected separately by the stdin reader thread.
async fn send_to_extension(msg: ToExtension) {
    let _ = tokio::task::spawn_blocking(move || write_native_message(&msg)).await;
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ToExtension {
    #[serde(rename = "sync")]
    Sync {
        sessions: Vec<String>,
        current_session: String,
    },
    #[serde(rename = "close_tab_group")]
    CloseTabGroup { session_name: String },
    #[serde(rename = "open_tab_group")]
    OpenTabGroup { session_name: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum FromExtension {
    #[serde(rename = "state")]
    State { groups: Vec<ExtensionTabGroup> },
    #[serde(rename = "switch_session")]
    SwitchSession { session: String },
    #[serde(rename = "send_to_pane")]
    SendToPane {
        text: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        title: String,
    },
}

#[derive(Debug, Deserialize)]
struct ExtensionTabGroup {
    title: String,
    tab_count: u32,
    collapsed: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tmux_tabs_bridge=info".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    info!("bridge starting");

    let sock = socket_path();
    let stream = tokio::net::UnixStream::connect(&sock)
        .await
        .context("connect to tmux-tabs server")?;

    let (mut reader, mut writer) = stream.into_split();

    let reg = Envelope::Bridge(BridgeMessage::Register);
    write_frame(&mut writer, &reg).await?;

    info!("registered with server");

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Envelope>(16);

    // Stdin reads block, so they run on a dedicated OS thread.
    std::thread::spawn(move || {
        loop {
            let Ok(msg) = read_native_message() else {
                break;
            };
            let envelope = match serde_json::from_value::<FromExtension>(msg) {
                Ok(FromExtension::State { groups }) => {
                    let tab_groups = groups
                        .into_iter()
                        .map(|g| TabGroupInfo {
                            title: g.title,
                            tab_count: g.tab_count,
                            collapsed: g.collapsed,
                        })
                        .collect();
                    Envelope::Bridge(BridgeMessage::TabGroupState { groups: tab_groups })
                }
                Ok(FromExtension::SwitchSession { session }) => {
                    Envelope::Bridge(BridgeMessage::SwitchSession {
                        session_name: session,
                    })
                }
                Ok(FromExtension::SendToPane { text, url, title }) => {
                    Envelope::Bridge(BridgeMessage::SendToPane { text, url, title })
                }
                Err(_) => continue,
            };
            if stdin_tx.blocking_send(envelope).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            frame = read_frame::<_, BridgeCommand>(&mut reader) => {
                match frame? {
                    Some(BridgeCommand::SyncState { sessions, current_session }) => {
                        send_to_extension(ToExtension::Sync { sessions, current_session }).await;
                    }
                    Some(BridgeCommand::CloseTabGroup { session_name }) => {
                        send_to_extension(ToExtension::CloseTabGroup { session_name }).await;
                    }
                    Some(BridgeCommand::OpenTabGroup { session_name }) => {
                        send_to_extension(ToExtension::OpenTabGroup { session_name }).await;
                    }
                    None => {
                        info!("server disconnected");
                        break;
                    }
                }
            }
            envelope = stdin_rx.recv() => {
                if let Some(msg) = envelope {
                    write_frame(&mut writer, &msg).await?;
                } else {
                    info!("stdin closed");
                    break;
                }
            }
        }
    }

    Ok(())
}
