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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum ClaudeStatus {
    #[default]
    None,
    Processing { activity: Option<String> },
    WaitingForInput { question: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session: TmuxSession,
    #[serde(default)]
    pub claude: ClaudeStatus,
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
