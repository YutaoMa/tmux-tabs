async function update() {
  const dot = document.getElementById("dot");
  const statusText = document.getElementById("status-text");
  const groups = await chrome.tabGroups.query({});
  const groupsList = document.getElementById("groups");
  groupsList.innerHTML = "";

  if (groups.length === 0) {
    statusText.textContent = "No tab groups";
    dot.className = "dot off";
    return;
  }

  dot.className = "dot on";
  statusText.textContent = `${groups.length} tab groups`;

  for (const g of groups) {
    const tabs = await chrome.tabs.query({ groupId: g.id });
    const li = document.createElement("li");
    const name = document.createElement("span");
    name.textContent = g.title || "(untitled)";
    const count = document.createElement("span");
    count.className = "count";
    count.textContent = `${tabs.length} tabs`;
    li.appendChild(name);
    li.appendChild(count);
    groupsList.appendChild(li);
  }
}

update();
