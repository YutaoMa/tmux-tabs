//! Per-session × per-agent status tracking.
//!
//! Each tmux session can host independent Claude Code and Copilot CLI panes;
//! we keep separate state slots for each `(session_name, AgentKind)` pair so
//! the two never overwrite each other. A single `last_active` map records
//! which agent emitted the most-recent event in each session, driving the
//! unified UI status line.
//!
//! Phase 1 only ever inserts `AgentKind::Claude` entries; Phase 2 will wire
//! up Copilot via an `events.jsonl` tail task.

pub mod claude;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tmux_tabs_common::{AgentKind, AgentStatus};

const TIMEOUT_SECS: u64 = 120;

pub(crate) struct SessionState {
    pub(super) pane_id: String,
    pub(super) status: AgentStatus,
    pub(super) last_event: Instant,
    pub(super) topic: Option<String>,
    pub(super) transcript_path: Option<String>,
    pub(super) context_pct: Option<u8>,
}

impl SessionState {
    fn new(pane_id: &str, now: Instant) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            status: AgentStatus::None,
            last_event: now,
            topic: None,
            transcript_path: None,
            context_pct: None,
        }
    }
}

type Key = (String, AgentKind);

pub struct AgentTracker {
    sessions: HashMap<Key, SessionState>,
    /// Most-recent agent to emit an event in each tmux session.
    last_active: HashMap<String, AgentKind>,
}

impl AgentTracker {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            last_active: HashMap::new(),
        }
    }

    /// Process a hook event for a given (session, agent). Returns true if any
    /// observable state changed (status / topic / `context_pct` / `last_active`).
    pub fn handle_event(
        &mut self,
        session_name: &str,
        pane_id: &str,
        kind: AgentKind,
        event_str: &str,
        payload: Option<&str>,
    ) -> bool {
        match kind {
            AgentKind::Claude => self.handle_claude_event(session_name, pane_id, event_str, payload),
            // Phase 2 will dispatch to a copilot handler here.
            AgentKind::Copilot => false,
        }
    }

    fn handle_claude_event(
        &mut self,
        session_name: &str,
        pane_id: &str,
        event_str: &str,
        payload: Option<&str>,
    ) -> bool {
        let now = Instant::now();
        let key = (session_name.to_string(), AgentKind::Claude);

        // Peek before we mutate to detect SessionEnd-style evictions vs updates.
        let outcome_is_evict =
            event_str.parse::<claude::ClaudeEvent>().ok().is_some_and(|e| {
                matches!(e, claude::ClaudeEvent::SessionEnd)
            });

        if outcome_is_evict {
            let removed = self.sessions.remove(&key).is_some();
            let cleared_active = self.refresh_last_active(session_name);
            return removed || cleared_active;
        }

        let entry = self
            .sessions
            .entry(key)
            .or_insert_with(|| SessionState::new(pane_id, now));

        let old_status = entry.status.clone();
        let old_topic = entry.topic.clone();
        let old_context_pct = entry.context_pct;
        entry.last_event = now;
        entry.pane_id = pane_id.to_string();

        let outcome = claude::apply_event(entry, event_str, payload);

        let state_changed = entry.status != old_status
            || entry.topic != old_topic
            || entry.context_pct != old_context_pct;

        let active_changed = matches!(outcome, claude::HandleOutcome::Updated)
            && self.last_active.insert(session_name.to_string(), AgentKind::Claude)
                != Some(AgentKind::Claude);

        state_changed || active_changed
    }

    /// Status from the most-recently-active agent in `session_name`.
    pub fn status(&self, session_name: &str) -> AgentStatus {
        self.last_active
            .get(session_name)
            .and_then(|kind| self.sessions.get(&(session_name.to_string(), *kind)))
            .map_or(AgentStatus::None, |s| s.status.clone())
    }

    /// Topic from the most-recently-active agent in `session_name`.
    pub fn topic(&self, session_name: &str) -> Option<&str> {
        let kind = self.last_active.get(session_name)?;
        self.sessions
            .get(&(session_name.to_string(), *kind))
            .and_then(|s| s.topic.as_deref())
    }

    /// Context-window % from the most-recently-active agent in `session_name`.
    pub fn context_pct(&self, session_name: &str) -> Option<u8> {
        let kind = self.last_active.get(session_name)?;
        self.sessions
            .get(&(session_name.to_string(), *kind))
            .and_then(|s| s.context_pct)
    }

    /// Pane id for a specific agent in `session_name`. Used by the Chrome
    /// "Send selection" routing, which always targets a specific AI.
    pub fn agent_pane(&self, session_name: &str, kind: AgentKind) -> Option<&str> {
        self.sessions
            .get(&(session_name.to_string(), kind))
            .map(|s| s.pane_id.as_str())
    }

    /// Drop entries whose pane no longer exists. Returns true if state changed.
    pub fn sweep_dead_panes(&mut self, live: &HashMap<String, String>) -> bool {
        let mut affected_sessions: HashSet<String> = HashSet::new();
        let before = self.sessions.len();
        self.sessions.retain(|(name, _), state| {
            let keep = live.contains_key(&state.pane_id);
            if !keep {
                affected_sessions.insert(name.clone());
            }
            keep
        });
        let sessions_changed = self.sessions.len() != before;

        let mut active_changed = false;
        for session in affected_sessions {
            active_changed |= self.refresh_last_active(&session);
        }
        sessions_changed || active_changed
    }

    /// Revert stuck transient states back to `None` after `TIMEOUT_SECS` of
    /// silence. Returns true if state changed.
    pub fn expire_stale(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for state in self.sessions.values_mut() {
            let is_transient = matches!(
                state.status,
                AgentStatus::Processing { .. } | AgentStatus::WaitingForInput { .. }
            );
            if is_transient && now.duration_since(state.last_event).as_secs() > TIMEOUT_SECS {
                state.status = AgentStatus::None;
                changed = true;
            }
        }
        changed
    }

    /// After removing the most-recent agent for a session, pick a remaining
    /// agent (if any) or drop the entry. Returns true if `last_active` changed.
    fn refresh_last_active(&mut self, session_name: &str) -> bool {
        let prior = self.last_active.get(session_name).copied();
        let remaining = [AgentKind::Claude, AgentKind::Copilot]
            .iter()
            .copied()
            .find(|k| self.sessions.contains_key(&(session_name.to_string(), *k)));
        match remaining {
            Some(k) if prior != Some(k) => {
                self.last_active.insert(session_name.to_string(), k);
                true
            }
            None if prior.is_some() => {
                self.last_active.remove(session_name);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(
        tracker: &mut AgentTracker,
        session: &str,
        kind: AgentKind,
        pane_id: &str,
        status: AgentStatus,
    ) {
        tracker.sessions.insert(
            (session.to_string(), kind),
            SessionState {
                pane_id: pane_id.to_string(),
                status,
                last_event: Instant::now(),
                topic: None,
                transcript_path: None,
                context_pct: None,
            },
        );
        tracker.last_active.insert(session.to_string(), kind);
    }

    #[test]
    fn sweep_repoints_last_active_when_active_agents_pane_dies() {
        let mut tracker = AgentTracker::new();
        seed(
            &mut tracker,
            "main",
            AgentKind::Claude,
            "%1",
            AgentStatus::Processing {
                activity: Some("claude-work".into()),
            },
        );
        seed(
            &mut tracker,
            "main",
            AgentKind::Copilot,
            "%2",
            AgentStatus::Processing {
                activity: Some("copilot-work".into()),
            },
        );

        assert_eq!(tracker.last_active.get("main"), Some(&AgentKind::Copilot));

        let live = HashMap::from([("%1".to_string(), "main".to_string())]);
        let changed = tracker.sweep_dead_panes(&live);

        assert!(changed, "sweep should report state change");
        assert_eq!(
            tracker.last_active.get("main"),
            Some(&AgentKind::Claude),
            "last_active should fall back to the surviving sibling"
        );
        assert!(
            matches!(
                tracker.status("main"),
                AgentStatus::Processing { activity: Some(ref a) } if a == "claude-work"
            ),
            "status should surface the surviving agent's live state"
        );
    }

    #[test]
    fn sweep_clears_last_active_when_session_loses_all_agents() {
        let mut tracker = AgentTracker::new();
        seed(
            &mut tracker,
            "alpha",
            AgentKind::Claude,
            "%9",
            AgentStatus::Processing { activity: None },
        );

        let live = HashMap::new();
        let changed = tracker.sweep_dead_panes(&live);

        assert!(changed);
        assert!(!tracker.last_active.contains_key("alpha"));
        assert!(matches!(tracker.status("alpha"), AgentStatus::None));
    }
}
