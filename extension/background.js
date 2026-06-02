const NATIVE_HOST = "com.tmux_tabs.bridge";

let port = null;
// Tab groups the user deleted by hand — remembered so handleSync doesn't
// recreate them on the next sync.
const deletedGroups = new Set();
let lastSessions = [];
let lastCurrentSession = "";
// Counter (not boolean) so concurrent guarded sections don't clear each other.
// `> 0` means "we're programmatically changing tab groups, ignore onUpdated."
let switchingCount = 0;

function isSwitching() {
  return switchingCount > 0;
}

// Run `fn` with the switching guard active. `settleMs` keeps the guard up
// after `fn` resolves so chrome events fired by our own changes still fall
// inside the window.
async function withSwitchingGuard(fn, settleMs = 0) {
  switchingCount++;
  try {
    await fn();
  } finally {
    if (settleMs > 0) {
      setTimeout(() => { switchingCount--; }, settleMs);
    } else {
      switchingCount--;
    }
  }
}

function connect() {
  port = chrome.runtime.connectNative(NATIVE_HOST);

  port.onMessage.addListener((msg) => {
    if (msg.type === "sync") {
      handleSync(msg.sessions, msg.current_session);
    } else if (msg.type === "close_tab_group") {
      handleCloseTabGroup(msg.session_name);
    } else if (msg.type === "open_tab_group") {
      handleOpenTabGroup(msg.session_name);
    }
  });

  port.onDisconnect.addListener(() => {
    console.log("tmux-tabs bridge disconnected:", chrome.runtime.lastError?.message);
    port = null;
    setTimeout(connect, 5000);
  });
}

connect();

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: "send-selection",
    title: "Send selection to Claude Code",
    contexts: ["selection"],
  });
  chrome.contextMenus.create({
    id: "send-page",
    title: "Send page to Claude Code",
    contexts: ["page"],
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "send-selection") {
    if (info.selectionText && port) {
      port.postMessage({
        type: "send_to_claude",
        text: info.selectionText,
        url: tab?.url || "",
        title: tab?.title || "",
      });
    }
  } else if (info.menuItemId === "send-page") {
    if (!tab?.id || !port) return;
    try {
      const results = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        func: () =>
          document.querySelector('main, article, [role="main"]')?.innerText ||
          document.body.innerText,
      });
      const text = results?.[0]?.result;
      if (text) {
        port.postMessage({
          type: "send_to_claude",
          text,
          url: tab.url || "",
          title: tab.title || "",
        });
      }
    } catch (e) {
      console.error("tmux-tabs: failed to capture page content:", e);
    }
  }
});

// Close every tab in the group whose title matches `sessionName` (case-insensitive).
// The group itself dissolves automatically once empty.
async function handleCloseTabGroup(sessionName) {
  const key = (sessionName || "").toLowerCase();
  if (!key) return;
  await withSwitchingGuard(async () => {
    try {
      const groups = await chrome.tabGroups.query({});
      const group = groups.find((g) => g.title && g.title.toLowerCase() === key);
      if (!group) return;
      const tabs = await chrome.tabs.query({ groupId: group.id });
      const ids = tabs.map((t) => t.id).filter((id) => typeof id === "number");
      if (ids.length > 0) {
        await chrome.tabs.remove(ids);
      }
    } catch (e) {
      console.error("close_tab_group failed:", e);
    }
  });
}

// Re-create (or just expand) the tab group whose title matches `sessionName`,
// clearing the user-deleted tombstone so future syncs keep it too. The group
// is recreated expanded when it's the current session, collapsed otherwise.
async function handleOpenTabGroup(sessionName) {
  const key = (sessionName || "").toLowerCase();
  if (!key) return;
  deletedGroups.delete(key);
  const isCurrent = key === lastCurrentSession.toLowerCase();
  await withSwitchingGuard(async () => {
    try {
      const groups = await chrome.tabGroups.query({});
      const existing = groups.find((g) => g.title && g.title.toLowerCase() === key);
      if (existing) {
        await chrome.tabGroups.update(existing.id, { collapsed: !isCurrent });
        return;
      }
      // Preserve the tmux session's original casing for the group title.
      const title = lastSessions.find((s) => s.toLowerCase() === key) || sessionName;
      const tab = await chrome.tabs.create({ active: false });
      const groupId = await chrome.tabs.group({ tabIds: [tab.id] });
      await chrome.tabGroups.update(groupId, {
        title,
        collapsed: !isCurrent,
        color: "grey",
      });
    } catch (e) {
      console.error("open_tab_group failed:", e);
    }
  }, 200);
  await sortManagedGroups(lastSessions);
  reportState();
}

function arraysEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

async function handleSync(sessions, currentSession) {
  const sessionChanged = currentSession !== lastCurrentSession;
  const sessionsChanged = !arraysEqual(sessions, lastSessions);
  lastSessions = sessions;
  lastCurrentSession = currentSession;

  // Fast path: nothing changed. Skip the work AND the trailing reportState()
  // so we don't echo identical state back to the server (which would re-broadcast,
  // triggering another sync, ad infinitum).
  if (!sessionsChanged && !sessionChanged) return;

  // Create missing tab groups for new sessions, in parallel.
  if (sessionsChanged) {
    const existingGroups = await chrome.tabGroups.query({});
    const existingTitles = new Set(existingGroups.map((g) => g.title.toLowerCase()));
    const toCreate = sessions.filter((s) => {
      const key = s.toLowerCase();
      return !existingTitles.has(key) && !deletedGroups.has(key);
    });
    await Promise.all(
      toCreate.map(async (session) => {
        const tab = await chrome.tabs.create({ active: false });
        const groupId = await chrome.tabs.group({ tabIds: [tab.id] });
        await chrome.tabGroups.update(groupId, {
          title: session,
          collapsed: true,
          color: "grey",
        });
      })
    );
  }

  // Only switch tab groups when the tmux session actually changes.
  if (sessionChanged) {
    await withSwitchingGuard(async () => {
      const currentKey = currentSession.toLowerCase();
      const allGroups = await chrome.tabGroups.query({});
      const sessionKeys = new Set(sessions.map((s) => s.toLowerCase()));

      for (const g of allGroups) {
        const key = g.title.toLowerCase();
        if (!sessionKeys.has(key)) continue;
        if (key === currentKey) {
          await chrome.tabGroups.update(g.id, { collapsed: false });
          const tabs = await chrome.tabs.query({ groupId: g.id });
          if (tabs.length > 0) {
            await chrome.tabs.update(tabs[0].id, { active: true });
          }
        } else {
          await chrome.tabGroups.update(g.id, { collapsed: true });
        }
      }
    }, 200);
  }

  // Sort managed tab groups alphabetically (like tmux).
  if (sessionsChanged || sessionChanged) {
    await sortManagedGroups(sessions);
  }

  reportState();
}

async function sortManagedGroups(sessions) {
  const allGroups = await chrome.tabGroups.query({});
  const sessionKeys = new Set(sessions.map((s) => s.toLowerCase()));

  const managed = allGroups
    .filter((g) => sessionKeys.has(g.title.toLowerCase()))
    .sort((a, b) => a.title.toLowerCase().localeCompare(b.title.toLowerCase()));

  if (managed.length < 2) return;

  // Already sorted? Compare current tab-strip indices to the alphabetical
  // order of the managed groups.
  let sorted = true;
  for (let i = 1; i < managed.length; i++) {
    const prev = allGroups.findIndex((g) => g.id === managed[i - 1].id);
    const curr = allGroups.findIndex((g) => g.id === managed[i].id);
    if (curr <= prev) {
      sorted = false;
      break;
    }
  }
  if (sorted) return;

  for (const group of managed) {
    await chrome.tabGroups.move(group.id, { index: -1 });
  }
}

let reportTimer = null;

// Coalesce bursts of tab events into one report.
function reportState() {
  if (reportTimer) return;
  reportTimer = setTimeout(() => {
    reportTimer = null;
    reportStateNow();
  }, 50);
}

async function reportStateNow() {
  if (!port) return;

  // One tabs.query + groups.query is enough to compute counts. The previous
  // approach ran tabs.query per group, which was N+1 and fired a flurry of
  // calls during session switches.
  const [groups, allTabs] = await Promise.all([
    chrome.tabGroups.query({}),
    chrome.tabs.query({}),
  ]);
  const counts = new Map();
  for (const t of allTabs) {
    if (typeof t.groupId === "number" && t.groupId >= 0) {
      counts.set(t.groupId, (counts.get(t.groupId) || 0) + 1);
    }
  }
  const result = groups.map((g) => ({
    title: g.title,
    tab_count: counts.get(g.id) || 0,
    collapsed: g.collapsed,
  }));

  port.postMessage({ type: "state", groups: result });
}

// Remember user-deleted groups so handleSync doesn't re-create them on the next sync.
chrome.tabGroups.onRemoved.addListener((group) => {
  if (group.title) {
    deletedGroups.add(group.title.toLowerCase());
  }
});

// Detect user-initiated tab group expansion → switch tmux session.
chrome.tabGroups.onUpdated.addListener((group) => {
  if (isSwitching() || !port) return;
  if (group.collapsed) return; // Only care about expansions.
  const key = group.title.toLowerCase();
  const sessionKeys = new Set(lastSessions.map((s) => s.toLowerCase()));
  if (!sessionKeys.has(key)) return; // Not a managed group.
  if (key === lastCurrentSession.toLowerCase()) return; // Already current.
  const session = lastSessions.find((s) => s.toLowerCase() === key);
  if (session) {
    port.postMessage({ type: "switch_session", session });
  }
  reportState();
});

// Report state whenever tab groups change.
chrome.tabGroups.onCreated.addListener(() => reportState());
chrome.tabs.onCreated.addListener(() => reportState());
chrome.tabs.onRemoved.addListener(() => reportState());
chrome.tabs.onAttached.addListener(() => reportState());
chrome.tabs.onDetached.addListener(() => reportState());
