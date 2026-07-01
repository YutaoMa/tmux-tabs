use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TmuxSession {
    pub id: String,
    pub name: String,
    pub windows: u32,
    pub attached: bool,
    pub activity: u64,
    /// Working directory of the session's active pane.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: Option<String>,
    pub pr_number: Option<u32>,
    pub pr_state: Option<PrState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BrowserInfo {
    pub tab_count: u32,
    pub collapsed: bool,
}

/// Per-session AI agent status, unified across agents (Claude Code, Copilot CLI).
/// Drives the single status line in the sidebar card.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum AgentStatus {
    #[default]
    None,
    Processing {
        activity: Option<String>,
    },
    WaitingForInput {
        question: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session: TmuxSession,
    #[serde(default, rename = "claude")]
    pub agent: AgentStatus,
    #[serde(default)]
    pub topic: Option<String>,
    /// Context window usage percentage (0–100).
    #[serde(default)]
    pub context_pct: Option<u8>,
    #[serde(default)]
    pub git: GitInfo,
    #[serde(default)]
    pub browser: Option<BrowserInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> TmuxSession {
        TmuxSession {
            id: "$0".to_string(),
            name: "main".to_string(),
            windows: 1,
            attached: true,
            activity: 0,
            cwd: None,
        }
    }

    #[test]
    fn session_entry_deserializes_legacy_claude_key() {
        let json = serde_json::json!({
            "session": {
                "id": "$0",
                "name": "main",
                "windows": 1,
                "attached": true,
                "activity": 0,
            },
            "claude": {
                "Processing": { "activity": "Run: ls" }
            },
            "topic": "hi",
            "git": {}
        });
        let entry: SessionEntry = serde_json::from_value(json).unwrap();
        assert!(matches!(
            entry.agent,
            AgentStatus::Processing { activity: Some(ref a) } if a == "Run: ls"
        ));
        assert_eq!(entry.topic.as_deref(), Some("hi"));
    }

    #[test]
    fn session_entry_serializes_with_claude_key() {
        let entry = SessionEntry {
            session: sample_session(),
            agent: AgentStatus::WaitingForInput {
                question: Some("ok?".to_string()),
            },
            topic: None,
            context_pct: None,
            git: GitInfo::default(),
            browser: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(
            json.get("claude").is_some(),
            "wire key must remain `claude`"
        );
        assert!(json.get("agent").is_none(), "must not emit new `agent` key");
    }
}
