use std::collections::HashMap;
use std::sync::Arc;

use tmux_tabs_common::{
    AgentKind, BridgeCommand, ServerMessage, SessionEntry, TabGroupInfo, TmuxSession,
};
use tokio::sync::{Notify, RwLock, mpsc};
use tokio::task::JoinHandle;

use crate::agent::{AgentTracker, copilot};
use crate::browser::BrowserTracker;
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
    agent: AgentTracker,
    browser: BrowserTracker,
    clients: HashMap<String, Client>,
    /// At most one bridge is connected at a time.
    bridge: Option<mpsc::Sender<BridgeCommand>>,
    /// `pane_id` → `session_name`, refreshed by the tmux poller. Lets the
    /// server resolve panes (e.g. on client register or hook event) without
    /// spawning a tmux subprocess on the hot path.
    pane_sessions: HashMap<String, String>,
    /// Per-Copilot-session tail-task handles, keyed by the Copilot CLI's
    /// own `sessionId`. Value carries the owning tmux pane so we can abort
    /// stale tasks when a pane dies. Aborting a finished task is a no-op,
    /// so the tail task itself calls back into `unregister_copilot_session`
    /// on a `session.shutdown` event to free the slot.
    copilot_tasks: HashMap<String, (String, JoinHandle<()>)>,
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
                agent: AgentTracker::new(),
                browser: BrowserTracker::new(),
                clients: HashMap::new(),
                bridge: None,
                pane_sessions: HashMap::new(),
                copilot_tasks: HashMap::new(),
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

    /// Best-effort: ask the bridge (if connected) to re-open the tab group
    /// matching `session_name`. Drops the bridge if the channel is dead.
    pub async fn open_tab_group(&self, session_name: &str) {
        let mut state = self.state.write().await;
        try_send_bridge(
            &mut state,
            BridgeCommand::OpenTabGroup {
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

    /// Pane id (with agent kind) of the most-recently-active AI in the
    /// currently attached tmux session. Used by the Chrome "Send selection"
    /// path so the message lands on whichever agent the user is actually
    /// using — Claude or Copilot.
    pub async fn active_agent_pane(&self) -> Option<(AgentKind, String)> {
        let state = self.state.read().await;
        let active = state.sessions.iter().find(|s| s.attached)?;
        state
            .agent
            .active_agent_pane(&active.name)
            .map(|(k, p)| (k, p.to_string()))
    }

    /// Process a hook event for one of the supported AI CLIs. Returns true if
    /// any visible state changed.
    pub async fn handle_agent_event(
        &self,
        session_name: &str,
        pane_id: &str,
        kind: AgentKind,
        event: &str,
        payload: Option<&str>,
    ) -> bool {
        let mut state = self.state.write().await;
        let changed = state
            .agent
            .handle_event(session_name, pane_id, kind, event, payload);
        if changed {
            self.notify.notify_waiters();
        }
        changed
    }

    /// Spawn a tail task for a freshly-started Copilot CLI session. Idempotent
    /// per `copilot_session_id` — a duplicate hook (e.g. on `copilot --resume`)
    /// is ignored so we don't double-dispatch events from the same file.
    pub async fn register_copilot_session(
        &self,
        session_name: String,
        pane_id: String,
        copilot_session_id: String,
    ) {
        {
            let state = self.state.read().await;
            if state.copilot_tasks.contains_key(&copilot_session_id) {
                return;
            }
        }
        let handle = tokio::spawn(copilot::tail_loop(
            self.clone(),
            session_name,
            pane_id.clone(),
            copilot_session_id.clone(),
        ));
        let mut state = self.state.write().await;
        // Re-check after re-acquiring the lock to close the TOCTOU window;
        // if a concurrent register won, abort the new handle instead.
        if state.copilot_tasks.contains_key(&copilot_session_id) {
            handle.abort();
            return;
        }
        state
            .copilot_tasks
            .insert(copilot_session_id, (pane_id, handle));
        self.notify.notify_waiters();
    }

    /// Drop a Copilot tail task from the registry. Called from the tail loop
    /// itself on `session.shutdown` (where `abort` is a no-op since the task
    /// is about to return) and from sweep paths when the owning pane dies.
    pub async fn unregister_copilot_session(&self, copilot_session_id: &str) {
        let mut state = self.state.write().await;
        if let Some((_, handle)) = state.copilot_tasks.remove(copilot_session_id) {
            handle.abort();
        }
    }

    /// Apply a parsed Copilot event to the tracker and broadcast if anything
    /// changed. Called by the per-session tail task for each new line in
    /// `events.jsonl`.
    pub async fn dispatch_copilot_event(
        &self,
        session_name: &str,
        pane_id: &str,
        event: copilot::CopilotEvent,
    ) {
        let changed = {
            let mut state = self.state.write().await;
            let (changed, _outcome) =
                state
                    .agent
                    .handle_copilot_event(session_name, pane_id, &event);
            if changed {
                self.notify.notify_waiters();
            }
            changed
        };
        if changed {
            self.broadcast().await;
        }
    }

    /// Replace the pane→session map and prune agent state for vanished panes.
    /// Returns true if anything changed.
    pub async fn refresh_pane_map(&self, panes: HashMap<String, String>) -> bool {
        let mut state = self.state.write().await;
        let pane_map_changed = state.pane_sessions != panes;

        let dead_copilot: Vec<String> = state
            .copilot_tasks
            .iter()
            .filter(|(_, (pane, _))| !panes.contains_key(pane))
            .map(|(sid, _)| sid.clone())
            .collect();
        let copilot_killed = !dead_copilot.is_empty();
        for sid in dead_copilot {
            if let Some((_, handle)) = state.copilot_tasks.remove(&sid) {
                handle.abort();
            }
        }

        let swept = state.agent.sweep_dead_panes(&panes);
        let expired = state.agent.expire_stale();
        if pane_map_changed {
            state.pane_sessions = panes;
        }
        pane_map_changed || swept || expired || copilot_killed
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
                agent: state.agent.status(&s.name),
                topic: state.agent.topic(&s.name).map(String::from),
                context_pct: state.agent.context_pct(&s.name),
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
            let session_names: Vec<String> =
                entries.iter().map(|e| e.session.name.clone()).collect();
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
