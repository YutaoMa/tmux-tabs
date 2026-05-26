use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tmux_tabs_common::{GitInfo, PrState, TmuxSession};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

const PR_CACHE_SECS: u64 = 60;
const PR_TIMEOUT_SECS: u64 = 5;

struct SessionGit {
    cwd: String,
    branch: Option<String>,
    pr_number: Option<u32>,
    pr_state: Option<PrState>,
    last_pr_check: Option<Instant>,
    pr_inflight: bool,
}

pub struct GitTracker {
    sessions: HashMap<String, SessionGit>,
    pr_tx: mpsc::Sender<(String, Option<(u32, PrState)>)>,
    pr_rx: mpsc::Receiver<(String, Option<(u32, PrState)>)>,
}

impl GitTracker {
    pub fn new() -> Self {
        let (pr_tx, pr_rx) = mpsc::channel(64);
        Self {
            sessions: HashMap::new(),
            pr_tx,
            pr_rx,
        }
    }

    /// Drain completed PR results from background tasks. Returns true if any
    /// drained result differs from the cached value.
    pub fn drain_results(&mut self) -> bool {
        let mut any_changed = false;
        while let Ok((session_name, result)) = self.pr_rx.try_recv() {
            if let Some(sg) = self.sessions.get_mut(&session_name) {
                sg.pr_inflight = false;
                let (new_num, new_state) = match result {
                    Some((num, state)) => (Some(num), Some(state)),
                    None => (None, None),
                };
                if sg.pr_number != new_num || sg.pr_state != new_state {
                    sg.pr_number = new_num;
                    sg.pr_state = new_state;
                    any_changed = true;
                }
            }
        }
        any_changed
    }

    /// Refresh git state for all sessions. Branches resolve in parallel (each
    /// is a `git rev-parse`); PR checks are spawned as background tasks when
    /// the cache expires. Returns true if any session's branch or set of
    /// tracked sessions changed.
    pub async fn update(&mut self, sessions: &[TmuxSession]) -> bool {
        let branches = resolve_branches(sessions).await;

        let live: HashSet<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        let pruned = self.sessions.keys().any(|n| !live.contains(n.as_str()));
        self.sessions.retain(|name, _| live.contains(name.as_str()));

        let mut any_changed = pruned;

        for session in sessions {
            let Some(cwd) = session.cwd.as_deref() else {
                if self.sessions.remove(&session.name).is_some() {
                    any_changed = true;
                }
                continue;
            };
            let new_branch = branches.get(&session.name).cloned().flatten();

            let sg = self
                .sessions
                .entry(session.name.clone())
                .or_insert_with(|| SessionGit {
                    cwd: cwd.to_string(),
                    branch: None,
                    pr_number: None,
                    pr_state: None,
                    last_pr_check: None,
                    pr_inflight: false,
                });

            if sg.cwd != cwd {
                sg.cwd = cwd.to_string();
                sg.branch = None;
                sg.pr_number = None;
                sg.pr_state = None;
                sg.last_pr_check = None;
            }

            if sg.branch != new_branch {
                // Branch changed: invalidate the PR cache so the next tick re-checks.
                any_changed = true;
                sg.branch = new_branch;
                sg.pr_number = None;
                sg.pr_state = None;
                sg.last_pr_check = None;
            }

            if sg.branch.is_some() && !sg.pr_inflight {
                let needs_check = sg
                    .last_pr_check
                    .is_none_or(|t| t.elapsed().as_secs() > PR_CACHE_SECS);
                if needs_check {
                    sg.pr_inflight = true;
                    sg.last_pr_check = Some(Instant::now());
                    let cwd = cwd.to_string();
                    let name = session.name.clone();
                    let tx = self.pr_tx.clone();
                    tokio::spawn(async move {
                        let result = get_pr_status(&cwd).await;
                        let _ = tx.send((name, result)).await;
                    });
                }
            }
        }

        any_changed
    }

    pub fn info(&self, session_name: &str) -> GitInfo {
        self.sessions
            .get(session_name)
            .map(|sg| GitInfo {
                branch: sg.branch.clone(),
                pr_number: sg.pr_number,
                pr_state: sg.pr_state,
            })
            .unwrap_or_default()
    }
}

/// Resolve branch names for every session with a known cwd, in parallel.
async fn resolve_branches(sessions: &[TmuxSession]) -> HashMap<String, Option<String>> {
    let mut set = JoinSet::new();
    for session in sessions {
        if let Some(cwd) = session.cwd.as_deref() {
            let name = session.name.clone();
            let cwd = cwd.to_string();
            set.spawn(async move {
                let branch = get_branch(&cwd).await;
                (name, branch)
            });
        }
    }
    let mut branches = HashMap::new();
    while let Some(result) = set.join_next().await {
        if let Ok((name, branch)) = result {
            branches.insert(name, branch);
        }
    }
    branches
}

/// Returns the current branch name, or None if the cwd isn't a git repo.
/// Detached HEAD resolves to a short SHA.
async fn get_branch(cwd: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        let sha = Command::new("git")
            .args(["-C", cwd, "rev-parse", "--short", "HEAD"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .output()
            .await
            .ok()?;
        let s = String::from_utf8_lossy(&sha.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    } else {
        Some(branch)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum GhState {
    Open,
    Merged,
    Closed,
}

#[derive(serde::Deserialize)]
struct GhPr {
    number: u32,
    state: GhState,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
}

/// Returns PR status for the current branch via `gh`. Returns None on any failure.
async fn get_pr_status(cwd: &str) -> Option<(u32, PrState)> {
    let result = timeout(
        Duration::from_secs(PR_TIMEOUT_SECS),
        Command::new("gh")
            .args(["pr", "view", "--json", "number,state,isDraft"])
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !result.status.success() {
        return None;
    }

    let pr: GhPr = serde_json::from_slice(&result.stdout).ok()?;
    let state = match (pr.state, pr.is_draft) {
        (GhState::Open, true) => PrState::Draft,
        (GhState::Open, false) => PrState::Open,
        (GhState::Merged, _) => PrState::Merged,
        (GhState::Closed, _) => PrState::Closed,
    };
    Some((pr.number, state))
}
