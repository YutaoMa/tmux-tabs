//! Copilot CLI agent: parses entries from a session's `events.jsonl` and
//! drives the same per-session state machine that Claude does, via an
//! internal `CopilotEvent` enum (not on the wire).
//!
//! Architecture: a single `sessionStart` hook (handled by socket.rs) tells
//! us a new Copilot session is alive and what its sessionId is. The server
//! then spawns a tail task ([`tail_loop`]) that reads new lines from
//! `~/.copilot/session-state/<sessionId>/events.jsonl`, parses them, and
//! dispatches typed events back into the tracker.
//!
//! That file is the source of truth for turn boundaries — Copilot has no
//! per-turn `Stop` hook, but the JSONL emits an explicit `assistant.turn_end`,
//! so we never need a stale-timeout heuristic. A long reasoning step can emit
//! nothing for many minutes, so reading silence as "idle" would drop the
//! spinner while the session is still working; [`OwnerWatch`] probes the
//! owning CLI process instead.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
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
/// How often to check that the Copilot process owning this events file is
/// still running, while the file is quiet.
const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Backstop for when the owning process can't be identified at all (no
/// `inuse.<pid>.lock` marker — e.g. a future Copilot release changes the
/// scheme). Far longer than any plausible reasoning pause so it can't fire
/// mid-turn, but bounded so a vanished CLI can't wedge the sidebar in
/// "processing" forever.
// `Duration::from_mins` would read better but is only stable since 1.91,
// above this workspace's MSRV.
#[allow(clippy::duration_suboptimal_units)]
const UNKNOWN_OWNER_TIMEOUT: Duration = Duration::from_secs(1_800);

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

/// Extract the pid from an `inuse.<pid>.lock` file name.
fn parse_lock_pid(file_name: &str) -> Option<u32> {
    file_name
        .strip_prefix("inuse.")?
        .strip_suffix(".lock")?
        .parse()
        .ok()
}

/// Copilot marks a live session with an `inuse.<pid>.lock` file in the
/// session directory, where the pid is the CLI process that owns it. Returns
/// `None` if no such marker is present (which we treat as "unknown", never as
/// "dead" — see [`UNKNOWN_OWNER_TIMEOUT`]).
async fn owner_pid(session_dir: &Path) -> Option<u32> {
    let mut entries = tokio::fs::read_dir(session_dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if let Some(pid) = name.to_str().and_then(parse_lock_pid) {
            return Some(pid);
        }
    }
    None
}

/// Whether `pid` still names a live process.
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    // SAFETY: `kill` with signal 0 performs the existence and permission
    // checks without delivering a signal, so it has no effect beyond errno.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // `EPERM` means the process exists but isn't ours; only `ESRCH` proves it
    // is gone, so treat every other error as still alive.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Watches whether the Copilot CLI that owns an events file is still running.
///
/// Silence in the file is normal — a long reasoning step emits nothing for
/// minutes — so the tail loop must never read it as "idle". This asks the OS
/// instead, throttled to [`LIVENESS_CHECK_INTERVAL`].
struct OwnerWatch {
    session_dir: Option<PathBuf>,
    pid: Option<u32>,
    quiet_since: tokio::time::Instant,
    last_check: tokio::time::Instant,
}

impl OwnerWatch {
    async fn new(events_path: &Path) -> Self {
        let session_dir = events_path.parent().map(Path::to_path_buf);
        let pid = match session_dir.as_deref() {
            Some(dir) => owner_pid(dir).await,
            None => None,
        };
        let now = tokio::time::Instant::now();
        Self {
            session_dir,
            pid,
            quiet_since: now,
            last_check: now,
        }
    }

    fn saw_event(&mut self) {
        self.quiet_since = tokio::time::Instant::now();
    }

    /// Probe whether the owning process is gone. Rate-limited: reports `false`
    /// until the next check is due.
    async fn check_gone(&mut self) -> bool {
        if self.last_check.elapsed() < LIVENESS_CHECK_INTERVAL {
            return false;
        }
        self.last_check = tokio::time::Instant::now();

        // The marker can appear after the events file does, so keep looking
        // until we find it.
        if self.pid.is_none()
            && let Some(dir) = self.session_dir.as_deref()
        {
            self.pid = owner_pid(dir).await;
        }

        match self.pid {
            Some(pid) => !process_alive(pid),
            None => self.quiet_since.elapsed() >= UNKNOWN_OWNER_TIMEOUT,
        }
    }
}

/// Evict this session's tracker entry and drop its task registration. Used
/// both for an explicit `session.shutdown` event and when we detect that the
/// owning process disappeared without emitting one.
async fn finish_session(
    state: &AppState,
    session_name: &str,
    pane_id: &str,
    copilot_session_id: &str,
) {
    state
        .dispatch_copilot_event(session_name, pane_id, CopilotEvent::SessionShutdown)
        .await;
    state.unregister_copilot_session(copilot_session_id).await;
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
    tail_loop_at(state, session_name, pane_id, copilot_session_id, path).await;
}

/// [`tail_loop`] with the events-file path supplied directly, so tests can
/// drive the loop against a temporary directory instead of the real `$HOME`.
async fn tail_loop_at(
    state: AppState,
    session_name: String,
    pane_id: String,
    copilot_session_id: String,
    path: PathBuf,
) {
    let Some(mut file) = open_events_file(&path).await else {
        return;
    };
    if let Err(e) = file.seek(SeekFrom::End(0)).await {
        warn!("copilot tail: seek to EOF failed: {e}");
        return;
    }

    let mut owner = OwnerWatch::new(&path).await;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    debug!("copilot tail: started for session {copilot_session_id}");

    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => {
                // EOF — wait for new lines. Quiet is not idle, so the only
                // thing that ends the session here is a dead owner.
                tokio::time::sleep(TAIL_POLL_INTERVAL).await;
                if owner.check_gone().await {
                    debug!("copilot tail: owner process gone for {copilot_session_id}, exiting");
                    finish_session(&state, &session_name, &pane_id, &copilot_session_id).await;
                    return;
                }
            }
            Ok(_) => {
                owner.saw_event();
                let Some(event) = parse_line(&buf) else {
                    continue;
                };
                if matches!(event, CopilotEvent::SessionShutdown) {
                    debug!("copilot tail: session.shutdown for {copilot_session_id}, exiting");
                    finish_session(&state, &session_name, &pane_id, &copilot_session_id).await;
                    return;
                }
                state
                    .dispatch_copilot_event(&session_name, &pane_id, event)
                    .await;
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};
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
        let line =
            r#"{"type":"user.message","data":{"content":"hello world","transformedContent":"x"}}"#;
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

    #[test]
    fn parse_lock_pid_reads_inuse_marker() {
        assert_eq!(parse_lock_pid("inuse.57402.lock"), Some(57402));
        assert_eq!(parse_lock_pid("inuse.0.lock"), Some(0));
    }

    #[test]
    fn parse_lock_pid_rejects_other_files() {
        assert_eq!(parse_lock_pid("events.jsonl"), None);
        assert_eq!(parse_lock_pid("session.db"), None);
        assert_eq!(parse_lock_pid("inuse.lock"), None);
        assert_eq!(parse_lock_pid("inuse.notapid.lock"), None);
        assert_eq!(parse_lock_pid("inuse.57402.lock.bak"), None);
    }

    #[test]
    fn process_alive_detects_this_process() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn process_alive_is_conservative_for_unrepresentable_pids() {
        // Anything we can't even convert to a `pid_t` must not be reported as
        // dead — a false "dead" evicts a live session from the sidebar.
        assert!(process_alive(u32::MAX));
    }

    /// A session directory that cleans itself up, including when a test panics
    /// — cleanup at the end of a test body is skipped on unwind.
    struct TempSessionDir(PathBuf);

    impl TempSessionDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tmux-tabs-copilot-test-{}-{tag}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp session dir");
            Self(dir)
        }

        fn join(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempSessionDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn append_line(path: &Path, line: &str) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open events file");
        writeln!(f, "{line}").expect("append event");
    }

    /// A pid that is guaranteed not to be running: spawn a child, reap it (so
    /// it isn't left as a zombie, which would still answer `kill(pid, 0)`),
    /// and use its pid before the OS recycles it.
    fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn helper process");
        let pid = child.id();
        child.wait().expect("reap helper process");
        pid
    }

    /// Let the spawned tail task make progress. Tests run with `start_paused`,
    /// so sleeping advances virtual time instantly and costs no wall clock.
    async fn settle(rounds: u32, step: Duration) {
        for _ in 0..rounds {
            tokio::task::yield_now().await;
            tokio::time::sleep(step).await;
        }
    }

    async fn wait_for_processing(state: &AppState) -> bool {
        for _ in 0..200 {
            if matches!(
                state.agent_status("main").await,
                AgentStatus::Processing { .. }
            ) {
                return true;
            }
            settle(1, Duration::from_millis(100)).await;
        }
        false
    }

    /// Regression: Copilot emits nothing during a long reasoning step. The
    /// tail loop must sit through that quietly without downgrading the
    /// session, or the sidebar spinner disappears mid-turn.
    #[tokio::test(start_paused = true)]
    async fn tail_loop_holds_processing_through_a_long_silence() {
        let dir = TempSessionDir::new("alive");
        let events = dir.join("events.jsonl");
        std::fs::write(&events, "").expect("create events file");
        std::fs::write(
            dir.join(format!("inuse.{}.lock", std::process::id())),
            "lock",
        )
        .expect("create lock file");

        let state = AppState::new();
        let task = tokio::spawn(tail_loop_at(
            state.clone(),
            "main".into(),
            "%1".into(),
            "sid-alive".into(),
            events.clone(),
        ));

        settle(10, Duration::from_millis(50)).await;
        append_line(
            &events,
            r#"{"type":"user.message","data":{"content":"hi"}}"#,
        );
        assert!(
            wait_for_processing(&state).await,
            "user.message should drive the session to Processing"
        );

        // Go quiet for much longer than the old 120s status timeout.
        settle(300, Duration::from_secs(1)).await;

        assert!(
            matches!(
                state.agent_status("main").await,
                AgentStatus::Processing { .. }
            ),
            "a silent reasoning pause must not clear Processing"
        );
        assert!(!task.is_finished(), "tail loop should still be tailing");

        task.abort();
    }

    /// The flip side: silence plus a dead owner *is* the end of the session,
    /// so the entry must be evicted rather than spinning forever.
    #[tokio::test(start_paused = true)]
    async fn tail_loop_evicts_when_owner_process_is_gone() {
        let dir = TempSessionDir::new("dead");
        let events = dir.join("events.jsonl");
        std::fs::write(&events, "").expect("create events file");
        std::fs::write(dir.join(format!("inuse.{}.lock", reaped_pid())), "lock")
            .expect("create lock file");

        let state = AppState::new();
        let task = tokio::spawn(tail_loop_at(
            state.clone(),
            "main".into(),
            "%1".into(),
            "sid-dead".into(),
            events.clone(),
        ));

        settle(10, Duration::from_millis(50)).await;
        append_line(
            &events,
            r#"{"type":"user.message","data":{"content":"hi"}}"#,
        );
        assert!(wait_for_processing(&state).await);

        for _ in 0..200 {
            if task.is_finished() {
                break;
            }
            settle(1, Duration::from_secs(1)).await;
        }

        assert!(
            task.is_finished(),
            "tail loop should exit once the owning CLI is gone"
        );
        assert!(
            matches!(state.agent_status("main").await, AgentStatus::None),
            "a dead owner should clear the session, not leave it spinning"
        );
    }
}
