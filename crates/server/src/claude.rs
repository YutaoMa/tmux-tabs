use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use tmux_tabs_common::{ClaudeEvent, ClaudeStatus};

const TIMEOUT_SECS: u64 = 120;
/// Token budget for Claude 1M-context models (Sonnet/Opus); Haiku and older
/// models have smaller windows and will appear over 100% — capped below.
const CONTEXT_WINDOW_TOKENS: u64 = 1_000_000;
const TRANSCRIPT_TAIL_BYTES: u64 = 65_536;

#[derive(serde::Deserialize)]
struct TranscriptLine {
    #[serde(rename = "type", default)]
    line_type: String,
    #[serde(default)]
    message: Option<TranscriptMessage>,
}

#[derive(serde::Deserialize)]
struct TranscriptMessage {
    #[serde(default)]
    usage: Option<TokenUsage>,
}

#[derive(serde::Deserialize, Default)]
struct TokenUsage {
    #[serde(default, rename = "input_tokens")]
    input: u64,
    #[serde(default, rename = "cache_creation_input_tokens")]
    cache_creation: u64,
    #[serde(default, rename = "cache_read_input_tokens")]
    cache_read: u64,
    #[serde(default, rename = "output_tokens")]
    output: u64,
}

#[derive(serde::Deserialize, Default)]
struct HookPayload {
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    notification_type: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

/// Read the last assistant entry from a Claude Code transcript JSONL file
/// and compute context-window usage as a percentage of `CONTEXT_WINDOW_TOKENS`.
fn read_context_pct(transcript_path: &str) -> Option<u8> {
    let mut file = std::fs::File::open(transcript_path).ok()?;
    let file_len = file.metadata().ok()?.len();

    let read_from = file_len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    if read_from > 0 {
        file.seek(SeekFrom::Start(read_from)).ok()?;
    }

    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;

    // Skip first partial line if we seeked into the middle.
    let start = if read_from > 0 {
        buf.find('\n').map_or(0, |i| i + 1)
    } else {
        0
    };

    let mut last_total: Option<u64> = None;
    for line in buf[start..].lines() {
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(line) else {
            continue;
        };
        if parsed.line_type != "assistant" {
            continue;
        }
        if let Some(usage) = parsed.message.and_then(|m| m.usage) {
            last_total = Some(usage.input + usage.cache_creation + usage.cache_read + usage.output);
        }
    }

    let total = last_total?;
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = ((total as f64 / CONTEXT_WINDOW_TOKENS as f64) * 100.0).min(100.0) as u8;
    Some(pct)
}

fn get_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(serde_json::Value::as_str)
}

/// Format a short tool description from a `PreToolUse` payload.
fn format_tool_description(tool_name: &str, tool_input: Option<&serde_json::Value>) -> String {
    let Some(input) = tool_input else {
        return tool_name.to_string();
    };
    let short = match tool_name {
        "Bash" => get_str(input, "command").map(|c| {
            let first_line = c.lines().next().unwrap_or(c);
            format!("Run: {first_line}")
        }),
        "Read" | "Edit" | "Write" => get_str(input, "file_path")
            .and_then(|p| p.rsplit('/').next())
            .map(|f| format!("{tool_name}: {f}")),
        "Grep" => get_str(input, "pattern").map(|p| format!("Grep: {p}")),
        "Glob" => get_str(input, "pattern").map(|p| format!("Glob: {p}")),
        "Agent" => get_str(input, "description").map(|d| format!("Agent: {d}")),
        "WebFetch" => get_str(input, "url").map(|u| format!("Fetch: {u}")),
        _ => None,
    };
    short.unwrap_or_else(|| tool_name.to_string())
}

/// Pure state-machine transition: derive the new [`ClaudeStatus`] from the
/// prior status, the incoming event, and the parsed payload.
fn transition(prior: &ClaudeStatus, event: &ClaudeEvent, payload: &HookPayload) -> ClaudeStatus {
    let is_ask_user = matches!(event, ClaudeEvent::ToolUse)
        && payload.tool_name.as_deref() == Some("AskUserQuestion");
    let is_input_notification = matches!(event, ClaudeEvent::Notification)
        && matches!(
            payload.notification_type.as_deref(),
            Some("permission_prompt" | "elicitation_dialog")
        );

    match event {
        ClaudeEvent::SessionStart | ClaudeEvent::Stop => ClaudeStatus::None,
        ClaudeEvent::ToolUse if is_ask_user => {
            let question = payload
                .tool_input
                .as_ref()
                .and_then(|v| v.get("questions"))
                .and_then(|v| v.get(0))
                .and_then(|v| get_str(v, "question"))
                .map(String::from);
            ClaudeStatus::WaitingForInput { question }
        }
        ClaudeEvent::ToolUse => {
            let activity = payload
                .tool_name
                .as_deref()
                .map(|tn| format_tool_description(tn, payload.tool_input.as_ref()));
            ClaudeStatus::Processing { activity }
        }
        ClaudeEvent::UserPromptSubmit => ClaudeStatus::Processing { activity: None },
        ClaudeEvent::Notification if is_input_notification => {
            // Permission/elicitation prompt: promote the prior tool description
            // (set during the preceding PreToolUse) into the question slot.
            let question = match prior {
                ClaudeStatus::Processing { activity } => activity.clone(),
                _ => None,
            };
            ClaudeStatus::WaitingForInput { question }
        }
        ClaudeEvent::Notification => prior.clone(),
        ClaudeEvent::SessionEnd => unreachable!(),
    }
}

struct SessionState {
    pane_id: String,
    status: ClaudeStatus,
    last_event: Instant,
    topic: Option<String>,
    transcript_path: Option<String>,
    context_pct: Option<u8>,
}

pub struct ClaudeTracker {
    sessions: HashMap<String, SessionState>,
}

impl ClaudeTracker {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Process a Claude Code hook event for a session. Returns true if state changed.
    pub fn handle_event(
        &mut self,
        session_name: &str,
        pane_id: &str,
        event: &ClaudeEvent,
        payload: Option<&str>,
    ) -> bool {
        let now = Instant::now();

        // SessionEnd removes the entry entirely.
        if matches!(event, ClaudeEvent::SessionEnd) {
            return self.sessions.remove(session_name).is_some();
        }

        let entry = self
            .sessions
            .entry(session_name.to_string())
            .or_insert(SessionState {
                pane_id: pane_id.to_string(),
                status: ClaudeStatus::None,
                last_event: now,
                topic: None,
                transcript_path: None,
                context_pct: None,
            });

        let old_status = entry.status.clone();
        let old_topic = entry.topic.clone();
        let old_context_pct = entry.context_pct;
        entry.last_event = now;
        entry.pane_id = pane_id.to_string();

        let parsed: HookPayload = payload
            .and_then(|p| serde_json::from_str(p).ok())
            .unwrap_or_default();

        if let Some(path) = &parsed.transcript_path {
            entry.transcript_path = Some(path.clone());
        }

        entry.status = transition(&entry.status, event, &parsed);

        match event {
            ClaudeEvent::UserPromptSubmit => {
                if let Some(prompt) = &parsed.prompt {
                    entry.topic = Some(prompt.clone());
                }
            }
            ClaudeEvent::Stop => {
                if let Some(path) = &entry.transcript_path {
                    entry.context_pct = read_context_pct(path);
                }
            }
            _ => {}
        }

        entry.status != old_status
            || entry.topic != old_topic
            || entry.context_pct != old_context_pct
    }

    pub fn status(&self, session_name: &str) -> ClaudeStatus {
        self.sessions
            .get(session_name)
            .map_or(ClaudeStatus::None, |s| s.status.clone())
    }

    pub fn topic(&self, session_name: &str) -> Option<&str> {
        self.sessions
            .get(session_name)
            .and_then(|s| s.topic.as_deref())
    }

    pub fn context_pct(&self, session_name: &str) -> Option<u8> {
        self.sessions.get(session_name).and_then(|s| s.context_pct)
    }

    /// Remove sessions whose pane no longer exists. Returns true if state changed.
    pub fn sweep_dead_panes(&mut self, live: &HashMap<String, String>) -> bool {
        let before = self.sessions.len();
        self.sessions
            .retain(|_, state| live.contains_key(&state.pane_id));
        self.sessions.len() != before
    }

    /// Revert stuck transient states (Processing/WaitingForInput) back to None
    /// after no events for `TIMEOUT_SECS`. Returns true if state changed.
    pub fn expire_stale(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for state in self.sessions.values_mut() {
            let is_transient = matches!(
                state.status,
                ClaudeStatus::Processing { .. } | ClaudeStatus::WaitingForInput { .. }
            );
            if is_transient && now.duration_since(state.last_event).as_secs() > TIMEOUT_SECS {
                state.status = ClaudeStatus::None;
                changed = true;
            }
        }
        changed
    }
}
