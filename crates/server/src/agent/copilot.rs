//! Copilot CLI agent: parses entries from a session's `events.jsonl` and
//! drives the same per-session state machine that Claude does, via an
//! internal `CopilotEvent` enum (not on the wire).
//!
//! Architecture: a single `sessionStart` hook (handled by socket.rs) tells
//! us a new Copilot session is alive and what its sessionId is. The server
//! then spawns a tail task ([`tail_loop`]) that reads new lines from
//! `~/.copilot/session-state/<sessionId>/events.jsonl`, parses them, and
//! dispatches typed events back into the tracker. The file is the source
//! of truth for turn boundaries — Copilot has no per-turn `Stop` hook, but
//! the JSONL emits an explicit `assistant.turn_end`, so we never need a
//! stale-timeout heuristic.

use std::io::SeekFrom;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tmux_tabs_common::AgentStatus;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tracing::{debug, warn};

use super::SessionState;
use crate::state::AppState;

/// Typed view of the Copilot CLI event types we care about. There are more
/// in `events.jsonl` (hook lifecycle, system messages, etc.); unknowns
/// parse to [`CopilotEvent::Other`] and are ignored by the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotEvent {
    UserMessage {
        content: String,
    },
    AssistantTurnStart,
    AssistantTurnEnd,
    ToolExecutionStart {
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolExecutionComplete {
        success: bool,
    },
    PermissionRequested {
        /// Best-effort question text (shell command, intention, etc.).
        question: Option<String>,
    },
    PermissionCompleted,
    SessionShutdown,
    Other,
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// Parse one JSONL line into a [`CopilotEvent`]. Returns `None` for blank
/// or malformed lines; returns [`CopilotEvent::Other`] for event types we
/// don't care about (so the caller can treat "unknown" and "ignored"
/// uniformly).
pub fn parse_line(line: &str) -> Option<CopilotEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw: RawLine = serde_json::from_str(trimmed).ok()?;
    Some(match raw.event_type.as_str() {
        "user.message" => {
            let content = raw
                .data
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            CopilotEvent::UserMessage { content }
        }
        "assistant.turn_start" => CopilotEvent::AssistantTurnStart,
        "assistant.turn_end" => CopilotEvent::AssistantTurnEnd,
        "tool.execution_start" => {
            let tool_name = raw
                .data
                .get("toolName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = raw.data.get("arguments").cloned().unwrap_or_default();
            CopilotEvent::ToolExecutionStart {
                tool_name,
                arguments,
            }
        }
        "tool.execution_complete" => {
            let success = raw
                .data
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            CopilotEvent::ToolExecutionComplete { success }
        }
        "permission.requested" => CopilotEvent::PermissionRequested {
            question: extract_permission_question(&raw.data),
        },
        "permission.completed" => CopilotEvent::PermissionCompleted,
        "session.shutdown" => CopilotEvent::SessionShutdown,
        _ => CopilotEvent::Other,
    })
}

fn extract_permission_question(data: &serde_json::Value) -> Option<String> {
    let req = data.get("permissionRequest")?;
    // Shell permissions have the most actionable text: the command itself.
    let command = req
        .get("fullCommandText")
        .and_then(serde_json::Value::as_str);
    if let Some(cmd) = command {
        let first_line = cmd.lines().next().unwrap_or(cmd).trim();
        if !first_line.is_empty() {
            return Some(format!("Run: {first_line}"));
        }
    }
    // Fall back to the human-readable intention if present.
    req.get("intention")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            req.get("kind")
                .and_then(serde_json::Value::as_str)
                .map(|k| format!("{k} permission"))
        })
}

fn get_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(serde_json::Value::as_str)
}

/// Format a short "what is Copilot currently doing?" string from a
/// `tool.execution_start` event, using each tool's most descriptive arg.
fn format_tool_activity(tool_name: &str, args: &serde_json::Value) -> String {
    let base = tool_name
        .rsplit_once('-')
        .map_or(tool_name, |(_, last)| last);
    let short = match tool_name {
        "bash" => get_str(args, "command").map(|c| {
            let first_line = c.lines().next().unwrap_or(c);
            format!("Run: {first_line}")
        }),
        "view" | "edit" | "create" => get_str(args, "path")
            .and_then(|p| p.rsplit('/').next())
            .map(|f| format!("{base}: {f}")),
        "grep" => get_str(args, "pattern").map(|p| format!("grep: {p}")),
        "glob" => get_str(args, "pattern").map(|p| format!("glob: {p}")),
        "web_fetch" => get_str(args, "url").map(|u| format!("fetch: {u}")),
        "report_intent" => get_str(args, "intent").map(str::to_string),
        "ask_user" => get_str(args, "question").map(|q| format!("ask: {q}")),
        "task" => get_str(args, "description").map(|d| format!("agent: {d}")),
        _ => None,
    };
    short.unwrap_or_else(|| base.to_string())
}

/// Pure state-machine transition. Same shape as `claude::transition`.
fn transition(prior: &AgentStatus, event: &CopilotEvent) -> AgentStatus {
    match event {
        CopilotEvent::UserMessage { .. } | CopilotEvent::AssistantTurnStart => {
            AgentStatus::Processing { activity: None }
        }
        CopilotEvent::AssistantTurnEnd => AgentStatus::None,
        CopilotEvent::ToolExecutionStart {
            tool_name,
            arguments,
        } => AgentStatus::Processing {
            activity: Some(format_tool_activity(tool_name, arguments)),
        },
        // tool.execution_complete keeps Processing — the next turn_end or
        // tool_start is the canonical transition signal.
        // SessionShutdown is handled by the dispatcher (entry eviction);
        // the transition itself is a no-op.
        CopilotEvent::ToolExecutionComplete { .. }
        | CopilotEvent::SessionShutdown
        | CopilotEvent::Other => prior.clone(),
        CopilotEvent::PermissionRequested { question } => AgentStatus::WaitingForInput {
            question: question.clone().or_else(|| match prior {
                AgentStatus::Processing { activity } => activity.clone(),
                _ => None,
            }),
        },
        // After the user accepts/denies, flip back to Processing — the
        // following tool_complete + turn_end will drive the real teardown.
        CopilotEvent::PermissionCompleted => AgentStatus::Processing { activity: None },
    }
}

pub enum HandleOutcome {
    Updated,
    /// Tracker should evict the entry and the tail task should exit.
    Shutdown,
    /// Event was uninteresting; no state change needed.
    Ignored,
}

/// Apply a [`CopilotEvent`] to an existing per-session state slot. Caller
/// is responsible for timestamp/pane-id refresh (same contract as
/// `claude::apply_event`).
pub fn apply_event(entry: &mut SessionState, event: &CopilotEvent) -> HandleOutcome {
    if matches!(event, CopilotEvent::SessionShutdown) {
        return HandleOutcome::Shutdown;
    }
    if matches!(event, CopilotEvent::Other) {
        return HandleOutcome::Ignored;
    }
    if let CopilotEvent::UserMessage { content } = event
        && !content.is_empty()
    {
        entry.topic = Some(content.clone());
    }
    entry.status = transition(&entry.status, event);
    HandleOutcome::Updated
}

const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long to keep retrying when `events.jsonl` doesn't exist yet at
/// hook time — Copilot may emit `sessionStart` slightly before the file
/// is created.
const FILE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the on-disk path for a Copilot session's event log.
fn events_path(copilot_session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".copilot")
            .join("session-state")
            .join(copilot_session_id)
            .join("events.jsonl"),
    )
}

/// Open the events file, retrying briefly if it doesn't exist yet.
async fn open_events_file(path: &PathBuf) -> Option<File> {
    let deadline = tokio::time::Instant::now() + FILE_WAIT_TIMEOUT;
    loop {
        match File::open(path).await {
            Ok(f) => return Some(f),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if tokio::time::Instant::now() >= deadline {
                    warn!("copilot tail: events.jsonl never appeared at {path:?}");
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                warn!("copilot tail: open {path:?} failed: {e}");
                return None;
            }
        }
    }
}

/// Tail the events file for a Copilot session, dispatching each parsed
/// event back into [`AppState`]. Returns when the session shuts down, the
/// file disappears, or the task is aborted by the caller.
///
/// Seek to EOF on first open: we only care about events emitted after we
/// attached. Pre-existing state (mid-turn at resume time) is lost, but the
/// next event will catch us up.
pub async fn tail_loop(
    state: AppState,
    session_name: String,
    pane_id: String,
    copilot_session_id: String,
) {
    let Some(path) = events_path(&copilot_session_id) else {
        warn!("copilot tail: HOME unset, cannot locate events.jsonl");
        return;
    };
    let Some(mut file) = open_events_file(&path).await else {
        return;
    };
    if let Err(e) = file.seek(SeekFrom::End(0)).await {
        warn!("copilot tail: seek to EOF failed: {e}");
        return;
    }

    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    debug!("copilot tail: started for session {copilot_session_id}");

    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => {
                // EOF — wait for new lines.
                tokio::time::sleep(TAIL_POLL_INTERVAL).await;
            }
            Ok(_) => {
                let Some(event) = parse_line(&buf) else {
                    continue;
                };
                let shutdown = matches!(event, CopilotEvent::SessionShutdown);
                state
                    .dispatch_copilot_event(&session_name, &pane_id, event)
                    .await;
                if shutdown {
                    debug!("copilot tail: session.shutdown for {copilot_session_id}, exiting");
                    state.unregister_copilot_session(&copilot_session_id).await;
                    return;
                }
            }
            Err(e) => {
                warn!("copilot tail: read error on {path:?}: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn fresh_state(pane: &str) -> SessionState {
        SessionState {
            pane_id: pane.to_string(),
            status: AgentStatus::None,
            last_event: Instant::now(),
            topic: None,
            transcript_path: None,
            context_pct: None,
        }
    }

    #[test]
    fn parses_user_message() {
        let line = r#"{"type":"user.message","data":{"content":"hello world","transformedContent":"x"}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(
            event,
            CopilotEvent::UserMessage {
                content: "hello world".into()
            }
        );
    }

    #[test]
    fn parses_turn_boundaries() {
        assert_eq!(
            parse_line(r#"{"type":"assistant.turn_start","data":{"turnId":"0"}}"#).unwrap(),
            CopilotEvent::AssistantTurnStart
        );
        assert_eq!(
            parse_line(r#"{"type":"assistant.turn_end","data":{"turnId":"0"}}"#).unwrap(),
            CopilotEvent::AssistantTurnEnd
        );
    }

    #[test]
    fn parses_tool_execution() {
        let start = parse_line(
            r#"{"type":"tool.execution_start","data":{"toolName":"bash","arguments":{"command":"ls -la"}}}"#,
        )
        .unwrap();
        let CopilotEvent::ToolExecutionStart {
            tool_name,
            arguments,
        } = start
        else {
            panic!("expected ToolExecutionStart, got {start:?}");
        };
        assert_eq!(tool_name, "bash");
        assert_eq!(arguments["command"], "ls -la");

        let complete = parse_line(
            r#"{"type":"tool.execution_complete","data":{"toolCallId":"x","success":false}}"#,
        )
        .unwrap();
        assert_eq!(
            complete,
            CopilotEvent::ToolExecutionComplete { success: false }
        );
    }

    #[test]
    fn parses_permission_with_shell_command() {
        let line = r#"{"type":"permission.requested","data":{"permissionRequest":{"kind":"shell","fullCommandText":"rm -rf /tmp/x","intention":"cleanup"}}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(
            event,
            CopilotEvent::PermissionRequested {
                question: Some("Run: rm -rf /tmp/x".into())
            }
        );
    }

    #[test]
    fn parses_permission_falls_back_to_intention() {
        let line = r#"{"type":"permission.requested","data":{"permissionRequest":{"kind":"file","intention":"write to /etc/hosts"}}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(
            event,
            CopilotEvent::PermissionRequested {
                question: Some("write to /etc/hosts".into())
            }
        );
    }

    #[test]
    fn parses_session_shutdown() {
        let line = r#"{"type":"session.shutdown","data":{"shutdownType":"routine"}}"#;
        assert_eq!(parse_line(line).unwrap(), CopilotEvent::SessionShutdown);
    }

    #[test]
    fn unknown_event_types_become_other() {
        let line = r#"{"type":"hook.start","data":{}}"#;
        assert_eq!(parse_line(line).unwrap(), CopilotEvent::Other);
    }

    #[test]
    fn blank_and_malformed_lines_return_none() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("not json").is_none());
    }

    #[test]
    fn transition_user_message_starts_processing() {
        let next = transition(
            &AgentStatus::None,
            &CopilotEvent::UserMessage {
                content: "hi".into(),
            },
        );
        assert!(matches!(next, AgentStatus::Processing { activity: None }));
    }

    #[test]
    fn transition_turn_end_clears_to_none() {
        let prior = AgentStatus::Processing {
            activity: Some("Run: ls".into()),
        };
        let next = transition(&prior, &CopilotEvent::AssistantTurnEnd);
        assert!(matches!(next, AgentStatus::None));
    }

    #[test]
    fn transition_tool_start_sets_activity_label() {
        let args = serde_json::json!({"command": "cargo test"});
        let event = CopilotEvent::ToolExecutionStart {
            tool_name: "bash".into(),
            arguments: args,
        };
        let next = transition(&AgentStatus::None, &event);
        assert!(matches!(
            next,
            AgentStatus::Processing { activity: Some(ref a) } if a == "Run: cargo test"
        ));
    }

    #[test]
    fn transition_permission_promotes_prior_activity_when_no_question() {
        let prior = AgentStatus::Processing {
            activity: Some("Run: foo".into()),
        };
        let next = transition(
            &prior,
            &CopilotEvent::PermissionRequested { question: None },
        );
        assert!(matches!(
            next,
            AgentStatus::WaitingForInput { question: Some(ref q) } if q == "Run: foo"
        ));
    }

    #[test]
    fn transition_permission_prefers_explicit_question() {
        let prior = AgentStatus::Processing {
            activity: Some("Run: foo".into()),
        };
        let next = transition(
            &prior,
            &CopilotEvent::PermissionRequested {
                question: Some("Custom question".into()),
            },
        );
        assert!(matches!(
            next,
            AgentStatus::WaitingForInput { question: Some(ref q) } if q == "Custom question"
        ));
    }

    #[test]
    fn format_tool_activity_handles_known_tools() {
        assert_eq!(
            format_tool_activity("bash", &serde_json::json!({"command": "ls"})),
            "Run: ls"
        );
        assert_eq!(
            format_tool_activity("view", &serde_json::json!({"path": "/a/b/file.rs"})),
            "view: file.rs"
        );
        assert_eq!(
            format_tool_activity("grep", &serde_json::json!({"pattern": "TODO"})),
            "grep: TODO"
        );
        // Unknown tool falls back to last segment of the name (strips
        // mcp prefixes like "github-mcp-server-list_issues" → "list_issues").
        assert_eq!(
            format_tool_activity("github-mcp-server-list_issues", &serde_json::json!({})),
            "list_issues"
        );
    }

    #[test]
    fn apply_event_user_message_sets_topic() {
        let mut state = fresh_state("%1");
        let outcome = apply_event(
            &mut state,
            &CopilotEvent::UserMessage {
                content: "fix the bug".into(),
            },
        );
        assert!(matches!(outcome, HandleOutcome::Updated));
        assert_eq!(state.topic.as_deref(), Some("fix the bug"));
        assert!(matches!(state.status, AgentStatus::Processing { .. }));
    }

    #[test]
    fn apply_event_shutdown_signals_eviction() {
        let mut state = fresh_state("%1");
        let outcome = apply_event(&mut state, &CopilotEvent::SessionShutdown);
        assert!(matches!(outcome, HandleOutcome::Shutdown));
    }

    #[test]
    fn apply_event_other_is_ignored() {
        let mut state = fresh_state("%1");
        let outcome = apply_event(&mut state, &CopilotEvent::Other);
        assert!(matches!(outcome, HandleOutcome::Ignored));
        assert!(matches!(state.status, AgentStatus::None));
    }
}
