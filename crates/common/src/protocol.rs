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
///
/// `event` is a free-form string so the server can dispatch agent-specific
/// parsing (Claude uses `snake_case` names like `prompt_submit`; Copilot CLI
/// uses `camelCase` names like `sessionStart`). `agent` discriminates which
/// vocabulary to use; defaults to `Claude` for backward compatibility.
#[derive(Debug, Serialize, Deserialize)]
pub struct HookNotification {
    pub tmux_pane_id: String,
    pub session_name: String,
    #[serde(default)]
    pub agent: AgentKind,
    pub event: String,
    #[serde(default)]
    pub payload: Option<String>,
}

/// Which AI CLI emitted a hook notification or owns a per-session state slot.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AgentKind {
    #[default]
    Claude,
    Copilot,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_notification_defaults_to_claude_agent() {
        let json = serde_json::json!({
            "tmux_pane_id": "%1",
            "session_name": "main",
            "event": "prompt_submit"
        });
        let notif: HookNotification = serde_json::from_value(json).unwrap();
        assert_eq!(notif.agent, AgentKind::Claude);
        assert_eq!(notif.event, "prompt_submit");
        assert!(notif.payload.is_none());
    }

    #[test]
    fn hook_notification_preserves_explicit_agent() {
        let json = serde_json::json!({
            "tmux_pane_id": "%1",
            "session_name": "main",
            "agent": "Copilot",
            "event": "sessionStart"
        });
        let notif: HookNotification = serde_json::from_value(json).unwrap();
        assert_eq!(notif.agent, AgentKind::Copilot);
        assert_eq!(notif.event, "sessionStart");
    }

    #[test]
    fn agent_kind_serializes_as_pascal_case() {
        assert_eq!(
            serde_json::to_value(AgentKind::Claude).unwrap(),
            serde_json::json!("Claude")
        );
        assert_eq!(
            serde_json::to_value(AgentKind::Copilot).unwrap(),
            serde_json::json!("Copilot")
        );
    }
}
