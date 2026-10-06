const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let settings = null;
let status = { running: false, state: "stopped" };
let os = "macos";
let paired = [];
let discovered = [];
let protocolVersion = 0;
const transfers = new Map(); // id -> { direction, label, done, total, state, reason, path }

const EDGE_WORDS = { left: "left", right: "right", top: "above", bottom: "below" };

// ---- settings ----

let saveTimer = null;
function save() {
  clearTimeout(saveTimer);
  // Saving applies the settings (the engine restarts if needed), so wait for typing to pause.
  saveTimer = setTimeout(() => invoke("save_settings", { settings }).catch(showError), 700);
}

function set(key, value) {
  settings[key] = value;
  save();
  render();
}

// ---- rendering ----

function statusText() {
  const peer = status.peer ? ` ${status.peer}` : "";
  const server = status.mode === "server";
  switch (status.state) {
    case "waiting": return "Waiting for the other computer";
    case "connecting": return `Connecting to ${settings.serverAddress || "server"}…`;
    case "connected": return server ? `Connected to${peer}` : "Connected";
    case "active": return server ? `Controlling${peer}` : "Being controlled";
    case "error": return "Needs attention";
    default: return settings.enabled ? "Starting…" : "Sharing is off";
  }
}

function render() {
  const running = status.running;
  const mode = settings.mode;
  $("enabled").checked = settings.enabled;
  document.body.classList.toggle("off", !settings.enabled);

  for (const m of ["server", "client"]) {
    $(`mode-${m}`).setAttribute("aria-selected", String(mode === m));
    $(`panel-${m}`).hidden = mode !== m;
  }

  document.querySelectorAll(".slot").forEach((el) => {
    const selected = el.dataset.edge === settings.edge;
    el.classList.toggle("selected", selected);
    el.setAttribute("aria-pressed", String(selected));
  });

  syncInput("code-server", settings.code);
  syncInput("code-client", settings.clientCode);
  syncInput("port", settings.port);
  syncInput("server-address", settings.serverAddress);
  syncInput("name", settings.name);
  $("swap").checked = settings.swapCtrlMeta;
  $("autostart").checked = settings.launchAtLogin;
  $("reveal-received").checked = settings.revealReceived;

  const pill = $("pill");
  pill.dataset.state = status.state || "stopped";
  $("pill-text").textContent = statusText();


  const msg = $("message");
  let text = status.message;
  if (!text && running && status.state === "waiting") {
    text = `On the other computer, open ShareKVM, choose “Be controlled”, and enter the address and code above. Then move your cursor off the ${EDGE_WORDS[settings.edge]} edge of this screen.`;
  }
  if (!text && status.state === "connected" && mode === "client") {
    text = "Move the mouse on the other computer to the edge of its screen to start controlling this one.";
  }
  msg.hidden = !text;
  msg.textContent = text || "";
  msg.classList.toggle("error", status.state === "error");

  const permissionProblem = /hook|Accessibility|permission/i.test(status.message || "");
  $("mac-hint").hidden = !(os === "macos" && (permissionProblem || status.state === "error"));

  $("log").textContent = (status.log || []).join("\n");
  renderPeers();
  renderFiles();
  renderDiscovered();
  $("visible-name").textContent = `“${settings.name || "this computer"}”`;
}

// ---- this computer's monitors ----

let layout = null;

async function loadScreens() {
  try {
    const fresh = await invoke("screens");
    if (JSON.stringify(fresh) !== JSON.stringify(layout)) {
      layout = fresh;
      renderScreens();
    }
  } catch (e) {
    /* keep the plain box */
  }
}

function renderScreens() {
  const box = $("monitors");
  if (!layout || layout.monitors.length < 2) {
    box.replaceChildren();
    $("me-label").textContent = "This computer";
    return;
  }
  const ms = layout.monitors;
  const x0 = Math.min(...ms.map((m) => m.x)), y0 = Math.min(...ms.map((m) => m.y));
  const bw = Math.max(...ms.map((m) => m.x + m.w)) - x0, bh = Math.max(...ms.map((m) => m.y + m.h)) - y0;
  const W = box.clientWidth || 160, H = box.clientHeight || 56;
  const scale = Math.min(W / bw, H / bh);
  const ox = (W - bw * scale) / 2, oy = (H - bh * scale) / 2;
  box.replaceChildren(...ms.map((m, i) => {
    const d = document.createElement("div");
    d.className = i === layout.primary ? "mon primary" : "mon";
    Object.assign(d.style, {
      left: `${ox + (m.x - x0) * scale + 1}px`, top: `${oy + (m.y - y0) * scale + 1}px`,
      width: `${Math.max(4, m.w * scale - 2)}px`, height: `${Math.max(4, m.h * scale - 2)}px`,
    });
    return d;
  }));
  $("me-label").textContent = `This computer · ${ms.length} monitors`;
}

// ---- computers on the network ----

const DEFAULT_PORT = 24801;

function renderDiscovered() {
  const list = $("discovered");
  $("discovered-empty").hidden = discovered.length > 0;
  list.replaceChildren(...discovered.map(discoveredItem));
}

function discoveredItem(f) {
  const li = document.createElement("li");
  li.className = "peer";
  const isPaired = paired.some((p) => p.role === "server" && p.id === f.id);
  const current = settings.serverId === f.id;
  const live = current && connected();
  li.classList.toggle("online", live);
  li.classList.toggle("current", current && status.running);

  const icon = document.createElement("span");
  icon.className = "peer-icon";
  const text = document.createElement("div");
  text.className = "peer-text";
  const name = document.createElement("div");
  name.className = "peer-name";
  name.textContent = f.name;
  const tag = document.createElement("span");
  const outdated = f.version !== protocolVersion;
  tag.className = outdated ? "tag warn" : "tag";
  tag.textContent = outdated ? "Different version" : isPaired ? "Paired" : "New";
  name.append(tag);
  const meta = document.createElement("div");
  meta.className = "peer-meta";
  meta.textContent = live
    ? "Connected now"
    : outdated
      ? "Update ShareKVM on both computers to connect"
      : `${f.addresses[0]}${isPaired ? "" : " · needs the pairing code"}`;
  text.append(name, meta);
  li.append(icon, text);

  if (!live && !outdated) {
    const go = document.createElement("button");
    go.className = "go";
    go.textContent = "Connect";
    go.addEventListener("click", () => connectTo(f, isPaired));
    li.append(go);
  }
  return li;
}

async function connectTo(f, isPaired) {
  const host = f.addresses[0].includes(":") ? `[${f.addresses[0]}]` : f.addresses[0];
  settings.serverAddress = f.port === DEFAULT_PORT ? host : `${host}:${f.port}`;
  settings.serverId = f.id;
  $("server-address").value = settings.serverAddress;
  if (!isPaired && !settings.clientCode) {
    status = { ...status, state: "stopped", message: `Enter the pairing code shown on ${f.name}, then press Start sharing.` };
    save();
    render();
    $("code-client").focus();
    return;
  }
  await saveNow({ enabled: true });
}

// ---- file transfer ----

const connected = () => status.running && (status.state === "connected" || status.state === "active");

function bytes(n) {
  if (n < 1024) return `${n} B`;
  const u = ["KB", "MB", "GB", "TB"];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < u.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${u[i]}`;
}

function renderFiles() {
  const show = connected() || transfers.size > 0;
  $("files").hidden = !show;
  if (!show) return;
  const peer = status.peer || "the other computer";
  $("drop-title").textContent = connected() ? `Drop files here to send them to ${peer}` : "Not connected";
  $("drop-hint").textContent = os === "macos"
    ? "Or drag files from Finder straight across the screen edge."
    : "Or drag files from a Mac across the screen edge onto this computer.";

  const items = [...transfers.entries()].slice(-6).reverse();
  $("transfers").replaceChildren(...items.map(([id, t]) => transferItem(id, t)));
}

function transferItem(id, t) {
  const li = document.createElement("li");
  li.className = `transfer ${t.state}`;
  const sending = t.direction === "send";
  const dir = document.createElement("span");
  dir.className = "dir";
  dir.textContent = t.state === "done" ? "✓" : t.state === "failed" ? "!" : sending ? "↑" : "↓";
  const name = document.createElement("div");
  name.className = "name";
  name.textContent = t.label;
  const meta = document.createElement("div");
  meta.className = "meta";
  if (t.state === "active") {
    meta.textContent = `${sending ? "Sending" : "Receiving"} · ${bytes(t.done)} of ${bytes(t.total)}`;
  } else if (t.state === "done") {
    meta.textContent = sending ? `Sent · ${bytes(t.total)}` : `Received · ${bytes(t.total)}`;
  } else {
    meta.textContent = `${sending ? "Not sent" : "Not received"}: ${t.reason}`;
  }

  const act = document.createElement("div");
  act.className = "act";
  if (t.state === "active") {
    const b = document.createElement("button");
    b.textContent = "Cancel";
    b.addEventListener("click", () => invoke("cancel_transfer", { id }).catch(showError));
    act.append(b);
  } else if (t.state === "done" && t.path) {
    const b = document.createElement("button");
    b.textContent = "Show";
    b.addEventListener("click", () => invoke("reveal", { path: t.path }).catch(showError));
    act.append(b);
  }

  const bar = document.createElement("div");
  bar.className = "bar";
  const fill = document.createElement("i");
  const pct = t.total ? (100 * t.done) / t.total : t.state === "done" ? 100 : 0;
  fill.style.width = `${t.state === "done" ? 100 : pct}%`;
  bar.append(fill);
  bar.hidden = t.state === "failed";
  li.append(dir, name, act, meta, bar);
  return li;
}

function onTransfer(ev) {
  const t = transfers.get(ev.id) || { done: 0, total: 0, state: "active" };
  t.direction = ev.direction;
  t.label = ev.label;
  if (ev.type === "transfer_progress") {
    t.done = ev.done;
    t.total = ev.total;
  } else if (ev.type === "transfer_done") {
    t.state = "done";
    t.done = t.total;
    t.path = ev.path;
    if (ev.direction === "receive" && ev.path && settings.revealReceived) {
      invoke("reveal", { path: ev.path }).catch(() => {});
    }
  } else if (ev.type === "transfer_failed") {
    t.state = "failed";
    t.reason = ev.reason;
  }
  transfers.delete(ev.id); // re-insert so the newest activity sorts first
  transfers.set(ev.id, t);
  while (transfers.size > 20) transfers.delete(transfers.keys().next().value);
  renderFiles();
}

async function sendDropped(paths) {
  if (!paths || paths.length === 0) return;
  if (!connected()) {
    status = { ...status, message: "Connect to the other computer first, then drop files to send them." };
    render();
    return;
  }
  try {
    await invoke("send_files", { paths });
  } catch (e) {
    showError(e);
  }
}

function setDragging(on) {
  document.body.classList.toggle("dragging", on);
  $("drop-overlay").hidden = !(on && connected());
}

// ---- remembered computers ----

function ago(secs) {
  const d = Math.max(0, Date.now() / 1000 - secs);
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  const days = Math.floor(d / 86400);
  return days === 1 ? "yesterday" : `${days} days ago`;
}

async function loadPeers() {
  try {
    paired = await invoke("list_paired");
  } catch (e) {
    paired = [];
  }
  renderPeers();
  renderDiscovered();
}

function renderPeers() {
  // Server mode lists computers we control (role "client"); client mode lists our servers.
  for (const role of ["client", "server"]) {
    const list = $(`peers-${role}-role`);
    const peers = paired.filter((p) => p.role === role);
    $(`peers-${role}-role-empty`).hidden = peers.length > 0;
    list.replaceChildren(...peers.map((p) => peerItem(p)));
  }
}

function peerItem(p) {
  const li = document.createElement("li");
  li.className = "peer";
  const online = status.running && status.state !== "waiting" && status.state !== "connecting" && status.peer === p.name;
  li.classList.toggle("online", online);

  const icon = document.createElement("span");
  icon.className = "peer-icon";
  const text = document.createElement("div");
  text.className = "peer-text";
  const name = document.createElement("div");
  name.className = "peer-name";
  name.textContent = p.name;
  const meta = document.createElement("div");
  meta.className = "peer-meta";
  const where = p.role === "server" && p.address ? ` · ${p.address.replace(/:24801$/, "")}` : "";
  meta.textContent = (online ? "Connected now" : `Last connected ${ago(p.lastSeen)}`) + where;
  text.append(name, meta);
  li.append(icon, text);

  // Client mode: one click to reconnect to a remembered computer.
  if (p.role === "server" && p.address && p.address.replace(/:24801$/, "") !== settings.serverAddress.replace(/:24801$/, "")) {
    const use = document.createElement("button");
    use.textContent = "Use";
    use.title = "Connect to this computer";
    use.addEventListener("click", () => {
      set("serverAddress", p.address.replace(/:24801$/, ""));
      $("server-address").value = settings.serverAddress;
    });
    li.append(use);
  }

  // Two-step Forget: no native dialogs needed.
  const forget = document.createElement("button");
  forget.className = "danger";
  forget.textContent = "Forget";
  forget.title = p.role === "client"
    ? "Remove this computer. A new pairing code is generated so it can't reconnect with the old one."
    : "Remove the saved key. You'll need the pairing code to connect again.";
  let armed = null;
  forget.addEventListener("click", async () => {
    if (!armed) {
      forget.textContent = "Click to confirm";
      forget.classList.add("confirm");
      armed = setTimeout(() => {
        armed = null;
        forget.textContent = "Forget";
        forget.classList.remove("confirm");
      }, 3000);
      return;
    }
    clearTimeout(armed);
    try {
      await invoke("forget_paired", { id: p.id, role: p.role });
      if (p.role === "client") {
        status = { ...status, message: `${p.name} was forgotten and the pairing code was changed. Give the new code to any computer you want to pair.` };
      }
      await loadPeers();
      render();
    } catch (e) {
      showError(e);
    }
  });
  li.append(forget);
  return li;
}

// Don't clobber what the user is typing.
function syncInput(id, value) {
  const el = $(id);
  if (document.activeElement !== el) el.value = value ?? "";
}

function showError(e) {
  status = { ...status, state: status.running ? status.state : "error", message: String(e) };
  render();
}

// ---- wiring ----

document.querySelectorAll(".segmented button").forEach((b) =>
  b.addEventListener("click", () => {
    if (settings.mode !== b.dataset.mode) status = { ...status, message: null };
    set("mode", b.dataset.mode);
  }));
document.querySelectorAll(".slot").forEach((b) =>
  b.addEventListener("click", () => set("edge", b.dataset.edge)));

$("code-server").addEventListener("input", (e) => { settings.code = e.target.value.trim(); save(); });
$("code-client").addEventListener("input", (e) => { settings.clientCode = e.target.value.trim(); save(); });
$("server-address").addEventListener("input", (e) => {
  settings.serverAddress = e.target.value.trim();
  settings.serverId = ""; // typed by hand: no longer tied to a discovered computer
  save();
});
$("name").addEventListener("input", (e) => { settings.name = e.target.value; save(); });
$("port").addEventListener("change", (e) => {
  const p = parseInt(e.target.value, 10);
  if (p >= 1024 && p <= 65535) set("port", p); else render();
});
$("swap").addEventListener("change", (e) => set("swapCtrlMeta", e.target.checked));
$("autostart").addEventListener("change", (e) => saveNow({ launchAtLogin: e.target.checked }));
$("enabled").addEventListener("change", (e) => saveNow({ enabled: e.target.checked }));
$("reveal-received").addEventListener("change", (e) => set("revealReceived", e.target.checked));
$("open-downloads").addEventListener("click", () => invoke("reveal", { path: null }).catch(showError));
$("regen").addEventListener("click", async () => set("code", await invoke("new_code")));

/// Switches apply immediately rather than after the typing delay.
async function saveNow(changes) {
  Object.assign(settings, changes);
  clearTimeout(saveTimer);
  render();
  try {
    await invoke("save_settings", { settings });
  } catch (e) {
    showError(e);
  }
}

listen("engine-status", (e) => { status = e.payload; render(); });
listen("paired-changed", () => loadPeers());
listen("discovered", (e) => { discovered = e.payload; renderDiscovered(); });
listen("transfer", (e) => onTransfer(e.payload));

// Files dragged onto the window (Tauri gives us real paths).
window.__TAURI__.webview?.getCurrentWebview().onDragDropEvent((e) => {
  const p = e.payload;
  if (p.type === "enter" || p.type === "over") setDragging(true);
  else if (p.type === "leave") setDragging(false);
  else if (p.type === "drop") {
    setDragging(false);
    sendDropped(p.paths);
  }
});
listen("settings-changed", async () => { settings = await invoke("get_settings"); render(); });
listen("engine-log", (e) => {
  status.log = [...(status.log || []), e.payload].slice(-200);
  const pre = $("log");
  pre.textContent = status.log.join("\n");
  pre.scrollTop = pre.scrollHeight;
});

(async function init() {
  [settings, status, os, discovered, protocolVersion] = await Promise.all([
    invoke("get_settings"), invoke("engine_status"), invoke("platform"),
    invoke("list_discovered"), invoke("protocol_version"),
  ]);
  if (!status.state) status.state = "stopped";
  $("meta-name").textContent = os === "macos" ? "Cmd" : "Win";
  const trayName = os === "macos" ? "menu bar" : "system tray";
  $("tray-name").textContent = trayName;
  $("tray-name-2").textContent = trayName;
  $("file-manager").textContent = os === "macos" ? "Finder" : os === "windows" ? "Explorer" : "your file manager";
  $("my-address").textContent = (await invoke("local_address")) || "Not on a network";
  const port = settings.port !== 24801 ? `:${settings.port}` : "";
  if (port) $("my-address").textContent += port;
  render();
  loadPeers();
  loadScreens();
  setInterval(renderPeers, 60_000); // keep "x min ago" fresh
  setInterval(loadScreens, 5_000); // monitors plugged in or out
  window.addEventListener("resize", renderScreens);
})();
