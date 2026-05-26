use std::collections::HashMap;
use std::sync::Arc;

use tmux_tabs_common::{ClaudeEvent, ServerMessage, SessionEntry, TmuxSession};
use tokio::sync::{Notify, RwLock, mpsc};

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
    clients: HashMap<String, Client>,
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
                clients: HashMap::new(),
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
                browser: None,
            })
            .collect();

        let mut dead = Vec::new();
        for (pane_id, client) in &state.clients {
            let msg = ServerMessage::StateUpdate {
                sessions: entries.clone(),
                current_session: client.session_name.clone(),
            };
            if client.tx.try_send(msg).is_err() {
                dead.push(pane_id.clone());
            }
        }

        for pane_id in dead {
            state.clients.remove(&pane_id);
        }
    }
}
