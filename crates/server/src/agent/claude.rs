//! Claude Code agent: hook event types, payload schema, state transition,
//! transcript-derived context-window calculation, and tool-description
//! formatting. Pure per-event logic lives here; per-session state and
//! cross-agent dispatch live in [`super`].

use std::io::{Read, Seek, SeekFrom};

use tmux_tabs_common::AgentStatus;

use super::SessionState;

/// Token budget for Claude 1M-context models (Sonnet/Opus); Haiku and older
/// models have smaller windows and will appear over 100% — capped below.
const CONTEXT_WINDOW_TOKENS: u64 = 1_000_000;
const TRANSCRIPT_TAIL_BYTES: u64 = 65_536;

/// Hook event names emitted by Claude Code. Wire form is `snake_case` (what
/// the `scripts/tmux-tabs-hook.sh` dispatcher passes on the CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeEvent {
    SessionStart,
    UserPromptSubmit,
    ToolUse,
    Stop,
    SessionEnd,
    Notification,
}

#[derive(Debug)]
pub struct ParseClaudeEventError(pub String);

impl std::fmt::Display for ParseClaudeEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown ClaudeEvent: {}", self.0)
    }
}

impl std::error::Error for ParseClaudeEventError {}

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
pub struct ClaudePayload {
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
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

/// Pure state-machine transition: derive the new [`AgentStatus`] from the
/// prior status, the incoming event, and the parsed payload.
fn transition(prior: &AgentStatus, event: ClaudeEvent, payload: &ClaudePayload) -> AgentStatus {
    let is_ask_user = matches!(event, ClaudeEvent::ToolUse)
        && payload.tool_name.as_deref() == Some("AskUserQuestion");
    let is_input_notification = matches!(event, ClaudeEvent::Notification)
        && matches!(
            payload.notification_type.as_deref(),
            Some("permission_prompt" | "elicitation_dialog")
        );

    match event {
        ClaudeEvent::SessionStart | ClaudeEvent::Stop => AgentStatus::None,
        ClaudeEvent::ToolUse if is_ask_user => {
            let question = payload
                .tool_input
                .as_ref()
                .and_then(|v| v.get("questions"))
                .and_then(|v| v.get(0))
                .and_then(|v| get_str(v, "question"))
                .map(String::from);
            AgentStatus::WaitingForInput { question }
        }
        ClaudeEvent::ToolUse => {
            let activity = payload
                .tool_name
                .as_deref()
                .map(|tn| format_tool_description(tn, payload.tool_input.as_ref()));
            AgentStatus::Processing { activity }
        }
        ClaudeEvent::UserPromptSubmit => AgentStatus::Processing { activity: None },
        ClaudeEvent::Notification if is_input_notification => {
            // Permission/elicitation prompt: promote the prior tool description
            // (set during the preceding PreToolUse) into the question slot.
            let question = match prior {
                AgentStatus::Processing { activity } => activity.clone(),
                _ => None,
            };
            AgentStatus::WaitingForInput { question }
        }
        ClaudeEvent::Notification => prior.clone(),
        ClaudeEvent::SessionEnd => unreachable!(),
    }
}

/// Returned by [`apply_event`] so the tracker knows whether the entry should
/// be evicted entirely (`SessionEnd`).
pub enum HandleOutcome {
    /// State updated in place; tracker should keep the entry.
    Updated,
    /// Session ended; tracker should drop the entry.
    Evict,
    /// Event string didn't parse as a Claude event — caller may log/skip.
    Unknown,
}

/// Update `entry` for an incoming Claude hook event. Caller is responsible
/// for timestamping (`last_event`) and pane-id refresh. Returns whether the
/// tracker should keep or evict the entry.
pub fn apply_event(
    entry: &mut SessionState,
    event_str: &str,
    payload: Option<&str>,
) -> HandleOutcome {
    let Ok(event) = event_str.parse::<ClaudeEvent>() else {
        return HandleOutcome::Unknown;
    };

    if matches!(event, ClaudeEvent::SessionEnd) {
        return HandleOutcome::Evict;
    }

    let parsed: ClaudePayload = payload
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

    HandleOutcome::Updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_documented_event_names() {
        // Hook-script wire contract: scripts/tmux-tabs-hook.sh forwards these
        // exact snake_case strings. They must keep parsing across refactors.
        for (name, expected) in [
            ("session_start", ClaudeEvent::SessionStart),
            ("prompt_submit", ClaudeEvent::UserPromptSubmit),
            ("tool_use", ClaudeEvent::ToolUse),
            ("stop", ClaudeEvent::Stop),
            ("session_end", ClaudeEvent::SessionEnd),
            ("notification", ClaudeEvent::Notification),
        ] {
            let parsed = name.parse::<ClaudeEvent>().unwrap();
            assert_eq!(parsed, expected, "event name `{name}` parsed incorrectly");
        }
    }

    #[test]
    fn rejects_unknown_event_names() {
        assert!("nope".parse::<ClaudeEvent>().is_err());
        assert!("sessionStart".parse::<ClaudeEvent>().is_err());
    }
}
