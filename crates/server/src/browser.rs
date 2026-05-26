use std::collections::HashMap;

use tmux_tabs_common::{BrowserInfo, TabGroupInfo};

pub struct BrowserTracker {
    /// Tab groups keyed by title (lowercased for case-insensitive matching).
    groups: HashMap<String, TabGroupInfo>,
}

impl BrowserTracker {
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
        }
    }

    /// Replace all tab group state with a fresh report from the extension.
    /// Returns true if the new state differs from what was previously tracked.
    pub fn update(&mut self, groups: Vec<TabGroupInfo>) -> bool {
        let mut new_groups: HashMap<String, TabGroupInfo> = HashMap::with_capacity(groups.len());
        for g in groups {
            new_groups.insert(g.title.to_lowercase(), g);
        }
        if new_groups == self.groups {
            return false;
        }
        self.groups = new_groups;
        true
    }

    /// Clear all state (called when the bridge disconnects).
    pub fn clear(&mut self) {
        self.groups.clear();
    }

    /// Get browser info for a session by name (case-insensitive match).
    pub fn info(&self, session_name: &str) -> Option<BrowserInfo> {
        self.groups
            .get(&session_name.to_lowercase())
            .map(|g| BrowserInfo {
                tab_count: g.tab_count,
                collapsed: g.collapsed,
            })
    }
}
