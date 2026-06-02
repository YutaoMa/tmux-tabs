use serde::{Deserialize, Serialize};

use crate::model::SessionEntry;

/// Client → Server
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Register { pane_id: String },
    SwitchSession { session_name: String },
    RenameSession { old_name: String, new_name: String },
    CloseSession { session_name: String },
    OpenTabGroup { session_name: String },
}

/// Server → Client
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    StateUpdate {
        sessions: Vec<SessionEntry>,
        current_session: String,
    },
    Shutdown,
}

/// Hook notifications → Server (one-shot connections from `tmux-tabs notify`).
#[derive(Debug, Serialize, Deserialize)]
pub struct HookNotification {
    pub tmux_pane_id: String,
    pub session_name: String,
    pub event: ClaudeEvent,
    #[serde(default)]
    pub payload: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClaudeEvent {
    SessionStart,
    UserPromptSubmit,
    ToolUse,
    Stop,
    SessionEnd,
    Notification,
}

#[derive(Debug, thiserror::Error)]
#[error("unknown ClaudeEvent: {0}")]
pub struct ParseClaudeEventError(pub String);

impl std::str::FromStr for ClaudeEvent {
    type Err = ParseClaudeEventError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session_start" => Ok(Self::SessionStart),
            "prompt_submit" => Ok(Self::UserPromptSubmit),
            "tool_use" => Ok(Self::ToolUse),
            "stop" => Ok(Self::Stop),
            "session_end" => Ok(Self::SessionEnd),
            "notification" => Ok(Self::Notification),
            other => Err(ParseClaudeEventError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabGroupInfo {
    pub title: String,
    pub tab_count: u32,
    pub collapsed: bool,
}

/// Chrome extension ↔ server, via the native messaging host.
#[derive(Debug, Serialize, Deserialize)]
pub enum BridgeMessage {
    Register,
    TabGroupState {
        groups: Vec<TabGroupInfo>,
    },
    SwitchSession {
        session_name: String,
    },
    SendToPane {
        text: String,
        url: String,
        title: String,
    },
}

/// Server → Bridge (sent over the bridge's mpsc channel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BridgeCommand {
    /// Full state pushed on every broadcast so the extension can diff.
    SyncState {
        sessions: Vec<String>,
        current_session: String,
    },
    /// Close the tab group that matches `session_name`.
    CloseTabGroup { session_name: String },
    /// Re-create (or expand) the tab group that matches `session_name`,
    /// clearing the extension's tombstone so a user-deleted group comes back.
    OpenTabGroup { session_name: String },
}

/// Discriminates between client messages, hook notifications, and bridge messages
/// on the shared server socket.
#[derive(Debug, Serialize, Deserialize)]
pub enum Envelope {
    Client(ClientMessage),
    Hook(HookNotification),
    Bridge(BridgeMessage),
}
