use std::collections::HashMap;
use std::sync::Arc;

use tmux_tabs_common::{
    BridgeCommand, ClaudeEvent, ServerMessage, SessionEntry, TabGroupInfo, TmuxSession,
};
use tokio::sync::{Notify, RwLock, mpsc};

use crate::browser::BrowserTracker;
use crate::claude::ClaudeTracker;
use crate::git::GitTracker;

struct Client {
    tx: mpsc::Sender<ServerMessage>,
    /// Fallback session name resolved at register time. `broadcast` overwrites
    /// it from `pane_sessions` each tick so tmux renames propagate, but for a
    /// brand-new pane the poller hasn't seen yet, this fallback is what the
    /// client receives in `current_session`.
    session_name: String,
}

struct State {
    sessions: Vec<TmuxSession>,
    git: GitTracker,
    claude: ClaudeTracker,
    browser: BrowserTracker,
    clients: HashMap<String, Client>,
    /// At most one bridge is connected at a time.
    bridge: Option<mpsc::Sender<BridgeCommand>>,
    /// `pane_id` → `session_name`, refreshed by the tmux poller. Lets the
    /// server resolve panes (e.g. on client register or hook event) without
    /// spawning a tmux subprocess on the hot path.
    pane_sessions: HashMap<String, String>,
}

#[derive(Clone)]
pub struct AppState {
    state: Arc<RwLock<State>>,
    pub notify: Arc<Notify>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(State {
                sessions: Vec::new(),
                git: GitTracker::new(),
                claude: ClaudeTracker::new(),
                browser: BrowserTracker::new(),
                clients: HashMap::new(),
                bridge: None,
                pane_sessions: HashMap::new(),
            })),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Update the tmux session list. Returns true if the list changed.
    pub async fn update_sessions(&self, sessions: Vec<TmuxSession>) -> bool {
        let mut state = self.state.write().await;
        if state.sessions == sessions {
            return false;
        }
        state.sessions = sessions;
        self.notify.notify_waiters();
        true
    }

    pub async fn register_client(
        &self,
        pane_id: String,
        session_name: String,
    ) -> mpsc::Receiver<ServerMessage> {
        let (tx, rx) = mpsc::channel(16);
        let mut state = self.state.write().await;
        state.clients.insert(pane_id, Client { tx, session_name });
        self.notify.notify_waiters();
        rx
    }

    pub async fn remove_client(&self, pane_id: &str) {
        let mut state = self.state.write().await;
        state.clients.remove(pane_id);
    }

    /// Register the bridge. Returns the receiver the connection task uses to
    /// forward [`BridgeCommand`]s out to the Chrome extension.
    pub async fn register_bridge(&self) -> mpsc::Receiver<BridgeCommand> {
        let (tx, rx) = mpsc::channel(16);
        let mut state = self.state.write().await;
        state.bridge = Some(tx);
        self.notify.notify_waiters();
        rx
    }

    pub async fn remove_bridge(&self) {
        let mut state = self.state.write().await;
        state.bridge = None;
        state.browser.clear();
    }

    /// Best-effort: ask the bridge (if connected) to close the tab group
    /// matching `session_name`. Drops the bridge if the channel is dead.
    pub async fn close_tab_group(&self, session_name: &str) {
        let mut state = self.state.write().await;
        try_send_bridge(
            &mut state,
            BridgeCommand::CloseTabGroup {
                session_name: session_name.to_string(),
            },
        );
    }

    /// Update tab group state reported by the bridge. Returns true if the
    /// tracked state actually changed.
    pub async fn update_tab_groups(&self, groups: Vec<TabGroupInfo>) -> bool {
        let mut state = self.state.write().await;
        let changed = state.browser.update(groups);
        if changed {
            self.notify.notify_waiters();
        }
        changed
    }

    /// Get the Claude Code pane ID for the currently attached tmux session.
    pub async fn active_claude_pane(&self) -> Option<String> {
        let state = self.state.read().await;
        let active = state.sessions.iter().find(|s| s.attached)?;
        state.claude.pane_id(&active.name).map(String::from)
    }

    /// Process a Claude Code hook event. Returns true if state changed.
    pub async fn handle_claude_event(
        &self,
        session_name: &str,
        pane_id: &str,
        event: &ClaudeEvent,
        payload: Option<&str>,
    ) -> bool {
        let mut state = self.state.write().await;
        let changed = state
            .claude
            .handle_event(session_name, pane_id, event, payload);
        if changed {
            self.notify.notify_waiters();
        }
        changed
    }

    /// Replace the pane→session map and prune Claude state for vanished panes.
    /// Returns true if anything changed.
    pub async fn refresh_pane_map(&self, panes: HashMap<String, String>) -> bool {
        let mut state = self.state.write().await;
        let pane_map_changed = state.pane_sessions != panes;
        let swept = state.claude.sweep_dead_panes(&panes);
        let expired = state.claude.expire_stale();
        if pane_map_changed {
            state.pane_sessions = panes;
        }
        pane_map_changed || swept || expired
    }

    pub async fn session_for_pane(&self, pane_id: &str) -> Option<String> {
        let state = self.state.read().await;
        state.pane_sessions.get(pane_id).cloned()
    }

    /// Drain PR results and refresh git state. Returns true if anything changed.
    pub async fn update_git(&self) -> bool {
        let mut state = self.state.write().await;
        let drained = state.git.drain_results();
        let sessions = state.sessions.clone();
        let branch_changed = state.git.update(&sessions).await;
        drained || branch_changed
    }

    pub async fn broadcast(&self) {
        let mut state = self.state.write().await;

        // Refresh client cached session names from the pane→session map
        // so tmux renames propagate without spawning a subprocess per client.
        let refreshed: Vec<(String, String)> = state
            .clients
            .keys()
            .filter_map(|p| state.pane_sessions.get(p).map(|s| (p.clone(), s.clone())))
            .collect();
        for (pane_id, name) in refreshed {
            if let Some(client) = state.clients.get_mut(&pane_id) {
                client.session_name = name;
            }
        }

        let entries: Vec<SessionEntry> = state
            .sessions
            .iter()
            .map(|s| SessionEntry {
                session: s.clone(),
                claude: state.claude.status(&s.name),
                topic: state.claude.topic(&s.name).map(String::from),
                context_pct: state.claude.context_pct(&s.name),
                git: state.git.info(&s.name),
                browser: state.browser.info(&s.name),
            })
            .collect();

        let mut dead_clients = Vec::new();
        for (pane_id, client) in &state.clients {
            let msg = ServerMessage::StateUpdate {
                sessions: entries.clone(),
                current_session: client.session_name.clone(),
            };
            if client.tx.try_send(msg).is_err() {
                dead_clients.push(pane_id.clone());
            }
        }

        let bridge_payload = state.bridge.as_ref().map(|_| {
            let active_session = state
                .sessions
                .iter()
                .find(|s| s.attached)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            let session_names: Vec<String> = entries.iter().map(|e| e.session.name.clone()).collect();
            BridgeCommand::SyncState {
                sessions: session_names,
                current_session: active_session,
            }
        });

        for pane_id in dead_clients {
            state.clients.remove(&pane_id);
        }
        if let Some(cmd) = bridge_payload {
            try_send_bridge(&mut state, cmd);
        }
    }
}

/// Try to deliver a command to the bridge. If the bridge is missing or its
/// channel is dead, drop the handle and clear browser state. Returns true
/// only when the message was queued.
fn try_send_bridge(state: &mut State, cmd: BridgeCommand) -> bool {
    let Some(tx) = state.bridge.as_ref() else {
        return false;
    };
    if tx.try_send(cmd).is_err() {
        state.bridge = None;
        state.browser.clear();
        return false;
    }
    true
}
