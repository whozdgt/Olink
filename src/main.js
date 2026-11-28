// Frontend logic. Talks to the Rust backend through Tauri's invoke() and
// listens for "phone" events (QR, connected, battery, casting, apps,
// transfers, notifications, errors).
//
// Everything that comes from the phone (names, notification text, file
// names) is shown with textContent, never as HTML. Translated strings go
// through textContent too. The only markup inserted is the QR SVG, which
// our own backend generates, and <b> tags from the locale catalog, which
// i18n.js builds as elements (see applyDom).
//
// In a plain browser (no Tauri) the page shows sample data when opened with
// #demo, so the layout can be previewed.

import { applyDom, t, useLocale } from "./i18n.js";

const tauri = window.__TAURI__;
const core = tauri?.core;
const DEMO = !core && location.hash === "#demo";

function uiLog(msg) {
  try { core?.invoke("ui_log", { msg: String(msg) }); } catch (_) {}
}
window.addEventListener("error", (e) => uiLog(`error: ${e.message} @ ${e.filename}:${e.lineno}`));
window.addEventListener("unhandledrejection", (e) => uiLog(`rejection: ${e.reason}`));

async function invoke(cmd, args) {
  if (!core) return demo(cmd, args);
  return core.invoke(cmd, args);
}

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};

// The web view's locale, used only when the backend didn't resolve one
// (#demo, or the settings call failed). Same rules as the Rust side.
function browserLanguage() {
  const tag = (navigator.language || "en").replace(/-/g, "_").toLowerCase().split(".")[0];
  if (!tag || tag === "c" || tag === "posix") return "en";
  if (tag === "zh" || tag.startsWith("zh_")) {
    const parts = tag.split("_");
    const traditional = parts.some((p) => p === "hant" || p === "tw" || p === "hk" || p === "mo");
    const simplified = tag === "zh" || parts.some((p) => p === "hans" || p === "cn" || p === "sg" || p === "chs");
    if (traditional && !simplified) return "en";
    return "zh-CN";
  }
  return "en";
}

function resolveLang(s) {
  const requested = s?.language || "system";
  if (requested === "en" || requested === "zh-CN") return requested;
  if (s?.effective_language === "en" || s?.effective_language === "zh-CN") return s.effective_language;
  return browserLanguage();
}

// ---- Toast ------------------------------------------------------------

function toast(msg, kind = "info") {
  const node = $("toast");
  node.textContent = msg;
  node.className = `toast toast--${kind}`;
  node.hidden = false;
  clearTimeout(toast._t);
  toast._t = setTimeout(() => (node.hidden = true), kind === "error" ? 7000 : 4000);
}
const errorText = (e) => String(e?.message ?? e).replace(/^Error:\s*/, "");
const fail = (e) => toast(errorText(e), "error");

// ---- Connection, battery, casting ---------------------------------------

let connected = false;
let casting = false;
let peerLabel = "";
let batteryState = null;
let lastAddr = "";
let computerName = "";

function setConnected(on, peer) {
  connected = on;
  peerLabel = peer || "";
  $("conn").textContent = on ? t("conn.on") : t("conn.off");
  $("conn").title = on && peerLabel ? t("conn.peer", { peer: peerLabel }) : "";
  $("conn").className = `pill ${on ? "pill--on" : "pill--off"}`;
  $("pair-panel").hidden = on;
  $("screen-panel").hidden = !on;
  $("files-panel").hidden = !on;
  if (!on) {
    $("battery").hidden = true;
    setCasting(false);
  }
}

function setBattery(level, charging) {
  batteryState = { level, charging };
  const b = $("battery");
  b.hidden = false;
  b.textContent = t(charging ? "battery.charging" : "battery.level", { level });
  b.title = t("battery.title");
  b.classList.toggle("chip--low", level <= 15 && !charging);
}

function setCasting(on) {
  casting = on;
  $("casting").hidden = !on;
  $("cast").disabled = on;
  for (const b of $("apps").querySelectorAll("button")) b.disabled = on;
}

// ---- App picker -----------------------------------------------------------

let apps = [];

// "com.instagram.android" → "Instagram" when the phone hasn't told us a name.
function guessName(pkg) {
  const parts = pkg.split(".").filter((p) => !["com", "org", "android", "app", "net", "www"].includes(p));
  const word = parts[parts.length - 1] || pkg;
  return word.charAt(0).toUpperCase() + word.slice(1);
}
const appName = (a) => a.name || guessName(a.pkg);

function appTile(a) {
  const b = el("button", "tile");
  b.type = "button";
  b.title = t("apps.show", { name: appName(a), pkg: a.pkg });
  b.disabled = casting;
  // Icons are data: URLs the backend checked and re-encoded (WebP/PNG/JPEG).
  if (a.icon && a.icon.startsWith("data:image/")) {
    const img = el("img", "tile__icon");
    img.src = a.icon;
    img.alt = "";
    img.loading = "lazy";
    b.appendChild(img);
  } else {
    b.appendChild(el("span", "tile__icon tile__icon--letter", appName(a).charAt(0).toUpperCase()));
  }
  b.appendChild(el("span", "tile__name", appName(a)));
  b.addEventListener("click", () => castApp(a));
  return b;
}

function renderApps() {
  const box = $("apps");
  const q = $("app-search").value.trim().toLowerCase();
  const match = (a) => !q || appName(a).toLowerCase().includes(q) || a.pkg.toLowerCase().includes(q);
  box.replaceChildren();
  if (!apps.length) {
    box.appendChild(el("p", "apps__empty", t("apps.waiting")));
    return;
  }
  const recent = apps.filter((a) => a.recent && match(a));
  const rest = apps.filter((a) => !a.recent && match(a));
  if (!recent.length && !rest.length) {
    box.appendChild(el("p", "apps__empty", t("apps.none", { query: $("app-search").value.trim() })));
    return;
  }
  const group = (title, list) => {
    if (!list.length) return;
    box.appendChild(el("div", "apps__group", title));
    const grid = el("div", "apps__grid");
    for (const a of list) grid.appendChild(appTile(a));
    box.appendChild(grid);
  };
  group(t("apps.recent"), recent);
  group(t(recent.length ? "apps.all" : "apps.apps"), rest);
}

async function castApp(a) {
  try {
    await invoke("start_cast", { app: a.pkg });
    toast(t("apps.asking", { name: appName(a) }));
  } catch (e) {
    fail(e);
  }
}

// ---- Notifications --------------------------------------------------------

const MAX_NOTIFS = 20;
function addNotification({ app, title, text }) {
  $("notif-panel").hidden = false;
  const li = el("li", "notif");
  const head = el("div", "notif__head");
  head.append(
    el("span", "notif__app", app),
    el("span", "notif__time", new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })),
  );
  li.append(head, el("div", "notif__title", title), el("div", "notif__text", text));
  const ul = $("notifs");
  ul.prepend(li);
  while (ul.children.length > MAX_NOTIFS) ul.lastChild.remove();
}

// ---- Transfers -------------------------------------------------------------

function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

// One row per transfer, updated in place as events arrive (keyed by id).
const MAX_TRANSFERS = 12;
const transferRows = new Map();
function onTransfer(transfer, opts = {}) {
  let row = transferRows.get(transfer.id);
  if (!row) {
    const li = el("li", "transfer");
    const dir = el("span", "transfer__dir");
    dir.setAttribute("aria-hidden", "true");
    const body = el("div", "transfer__body");
    const name = el("div", "transfer__name");
    const state = el("div", "transfer__state");
    const bar = el("div", "transfer__bar");
    const fill = el("div", "transfer__fill");
    bar.appendChild(fill);
    body.append(name, state, bar);
    const cancel = el("button", "btn btn--small transfer__cancel", t("transfer.cancel"));
    cancel.type = "button";
    cancel.addEventListener("click", async () => {
      cancel.disabled = true;
      try {
        await invoke("cancel_transfer", { id: transfer.id });
      } catch (e) {
        fail(e);
        cancel.disabled = false;
      }
    });
    li.append(dir, body, cancel);
    row = { li, dir, name, state, bar, fill, cancel };
    transferRows.set(transfer.id, row);
    const ul = $("transfers");
    ul.prepend(li);
    while (ul.children.length > MAX_TRANSFERS) {
      const last = ul.lastChild;
      for (const [id, r] of transferRows) if (r.li === last) transferRows.delete(id);
      last.remove();
    }
  }
  row.payload = transfer;
  const files = transfer.files || [];
  row.dir.textContent = transfer.outgoing ? "↑" : "↓";
  row.li.title = t(transfer.outgoing ? "transfer.to" : "transfer.from", { files: files.join("\n") });
  row.name.textContent = files.length > 1 ? t("transfer.more", { name: files[0], count: files.length - 1 }) : files[0] || t("transfer.files");
  const pct = transfer.bytes > 0 ? Math.min(100, Math.round((transfer.done / transfer.bytes) * 100)) : 0;
  const size = fmtBytes(transfer.bytes);
  const stateText = {
    waiting: t("transfer.waiting", { size }),
    sending: t("transfer.sending", { pct, size }),
    receiving: t("transfer.receiving", { pct, size }),
    done: t(transfer.outgoing ? "transfer.sent" : "transfer.saved", { size }),
    failed: t(transfer.outgoing ? "transfer.not_sent" : "transfer.not_saved"),
  }[transfer.state] || transfer.state;
  row.state.textContent = stateText;
  row.cancel.textContent = t("transfer.cancel");
  row.li.dataset.state = transfer.state;
  row.bar.hidden = !(transfer.state === "sending" || transfer.state === "receiving");
  // Transfers to the phone can be stopped from here (the phone can stop any).
  row.cancel.hidden = !(transfer.outgoing && (transfer.state === "waiting" || transfer.state === "sending"));
  row.fill.style.width = `${pct}%`;
  if (opts.quiet) return;
  if (transfer.state === "done") toast(t(transfer.outgoing ? "transfer.toast_sent" : "transfer.toast_received"));
  if (transfer.state === "failed") toast(stateText, "error");
}

// ---- Events ------------------------------------------------------------------

function onEvent(ev) {
  switch (ev.kind) {
    case "qr":
      // SVG generated by our own backend (qrcode crate), not by the phone.
      $("qr").innerHTML = ev.svg;
      lastAddr = ev.addr || "";
      $("addr").textContent = t("pair.addr", { addr: lastAddr });
      break;
    case "connected":
      setConnected(true, ev.peer);
      toast(t("toast.connected"));
      break;
    case "disconnected":
      setConnected(false);
      toast(t("toast.disconnected"));
      break;
    case "battery":
      setBattery(ev.level, ev.charging);
      break;
    case "casting":
      setCasting(ev.on);
      break;
    case "notification":
      addNotification(ev);
      break;
    case "apps":
      apps = ev.apps || [];
      renderApps();
      break;
    case "transfer":
      onTransfer(ev);
      break;
    case "error":
      toast(ev.msg, "error");
      break;
    case "fatal":
      $("fatal").textContent = ev.msg;
      $("fatal").hidden = false;
      break;
    case "pair_request":
      askPairing(ev);
      break;
    case "pair_request_done":
      if (pairAsk && pairAsk.id === ev.id) closePairAsk();
      if (!ev.allowed) toast(t("pair.ask.denied"));
      break;
  }
}

// ---- Pairing approval --------------------------------------------------------------

// A phone finished the pairing handshake; nothing is saved until the user
// allows it here (the backend times out after 20 s and treats that as "no").
let pairAsk = null;

function askPairing({ id, name, model, addr }) {
  closePairAsk();
  let left = 20;
  const render = () => {
    $("pair-ask-name").textContent = name || t("pair.ask.unknown");
    const parts = [];
    if (model) parts.push(t("pair.ask.model", { model }));
    if (addr) parts.push(t("pair.ask.at", { addr }));
    $("pair-ask-meta").textContent = parts.join(" · ");
    $("pair-ask-timer").textContent = t("pair.ask.timer", { seconds: left });
  };
  render();
  pairAsk = {
    id,
    render,
    timer: setInterval(() => { left = Math.max(0, left - 1); render(); }, 1000),
  };
  $("pair-ask").showModal();
  $("pair-deny").focus(); // the safe choice is the default
}

function closePairAsk() {
  if (!pairAsk) return;
  clearInterval(pairAsk.timer);
  pairAsk = null;
  if ($("pair-ask").open) $("pair-ask").close();
}

async function answerPairing(allow) {
  if (!pairAsk) return;
  const { id } = pairAsk;
  closePairAsk();
  try {
    await invoke("answer_pairing", { id, allow });
    if (allow) toast(t("pair.ask.allowed"));
  } catch (e) {
    fail(e);
  }
}

$("pair-allow").addEventListener("click", () => answerPairing(true));
$("pair-deny").addEventListener("click", () => answerPairing(false));
// Escape closes the dialog: that's a "no".
$("pair-ask").addEventListener("cancel", (e) => {
  e.preventDefault();
  answerPairing(false);
});

// ---- Actions -------------------------------------------------------------------

$("cast").addEventListener("click", async () => {
  try {
    await invoke("start_cast", { app: null });
    toast(t("screen.asking"));
  } catch (e) {
    fail(e);
  }
});

$("app-search").addEventListener("input", renderApps);

$("send-files").addEventListener("click", async () => {
  try {
    await invoke("pick_files_to_send", { folders: false });
  } catch (e) {
    fail(e);
  }
});

$("send-folder").addEventListener("click", async () => {
  try {
    await invoke("pick_files_to_send", { folders: true });
  } catch (e) {
    fail(e);
  }
});

$("open-received").addEventListener("click", async () => {
  try {
    await invoke("open_received_folder");
  } catch (e) {
    fail(e);
  }
});

// ---- Settings ----------------------------------------------------------------------

const settingsForm = $("settings").querySelector("form");

function readSettingsForm() {
  const f = settingsForm.elements;
  return {
    cast_quality: f.cast_quality.value,
    scroll_mode: f.scroll_mode.value,
    desktop_notifications: f.desktop_notifications.checked,
    share_clipboard: f.share_clipboard.checked,
    start_on_login: f.start_on_login.checked,
    theme: f.theme.value || "system",
    pc_name: f.pc_name.value.trim(),
    language: f.language.value || "system",
  };
}

function fillSettingsForm(s) {
  const f = settingsForm.elements;
  f.cast_quality.value = s.cast_quality;
  f.scroll_mode.value = s.scroll_mode;
  f.desktop_notifications.checked = !!s.desktop_notifications;
  f.share_clipboard.checked = !!s.share_clipboard;
  f.start_on_login.checked = !!s.start_on_login;
  f.theme.value = s.theme || "system";
  f.pc_name.value = s.pc_name || "";
  f.language.value = s.language || "system";
  computerName = s.computer_name || "";
  f.pc_name.placeholder = computerName;
}

$("open-settings").addEventListener("click", async () => {
  try {
    fillSettingsForm(await invoke("get_settings"));
    $("settings").showModal();
  } catch (e) {
    fail(e);
  }
});

// Settings → Look: "system" leaves it to prefers-color-scheme; the other
// two force a theme (styles.css keys on <html data-theme>), header icon too.
function applyTheme(theme) {
  const root = document.documentElement;
  if (theme === "midnight" || theme === "classic") root.dataset.theme = theme;
  else delete root.dataset.theme;
  const light = $("brand-light");
  if (light) {
    light.media = theme === "classic" ? "all" : theme === "midnight" ? "not all" : "(prefers-color-scheme: light)";
  }
}

async function applyLanguage(s) {
  await useLocale(resolveLang(s));
  refreshDynamic();
}

settingsForm.addEventListener("change", async () => {
  const form = readSettingsForm();
  applyTheme(form.theme);
  try {
    await invoke("set_settings", { settings: form });
    const fresh = await invoke("get_settings");
    if (fresh) await applyLanguage(fresh);
  } catch (e) {
    fail(e);
  }
});

function renderDrop() {
  $("drop-title").textContent = connected ? t("drop.title") : t("drop.need_title");
  $("drop-sub").textContent = connected ? t("drop.sub") : t("drop.need_sub");
  $("drop").classList.toggle("drop--off", !connected);
}

// Static catalog text, then anything built in JS (so a language change
// updates the open window, including rows already on screen).
function refreshDynamic() {
  applyDom();
  setConnected(connected, peerLabel);
  if (batteryState) setBattery(batteryState.level, batteryState.charging);
  renderApps();
  for (const row of transferRows.values()) {
    if (row.payload) onTransfer(row.payload, { quiet: true });
  }
  if (lastAddr) $("addr").textContent = t("pair.addr", { addr: lastAddr });
  if (pairAsk) pairAsk.render();
  if ($("pc-name")) $("pc-name").placeholder = computerName;
  renderDrop();
}

// ---- Drag and drop ---------------------------------------------------------------------

// The backend gets the dropped paths from the window event and sends them;
// the page only shows the overlay.
function setupDrop() {
  const wv = tauri?.webview?.getCurrentWebview?.();
  if (!wv) return;
  wv.onDragDropEvent((e) => {
    const type = e.payload.type;
    if (type === "enter") {
      renderDrop();
      $("drop").hidden = false;
    } else if (type === "leave" || type === "drop") {
      $("drop").hidden = true;
    }
  });
}

// ---- Demo (browser preview only) ---------------------------------------------------------

let demoSettings = {
  cast_quality: "balanced",
  scroll_mode: "swipe",
  desktop_notifications: true,
  share_clipboard: true,
  start_on_login: true,
  theme: "system",
  pc_name: "",
  language: "system",
  computer_name: "my-computer",
};
function demo(cmd, args) {
  if (cmd === "get_settings") {
    const language = demoSettings.language || "system";
    const effective_language = language === "en" || language === "zh-CN" ? language : browserLanguage();
    return { ...demoSettings, language, effective_language };
  }
  if (cmd === "set_settings") {
    demoSettings = { ...demoSettings, ...args.settings };
    return null;
  }
  if (cmd === "phone_state") {
    if (!DEMO) return [];
    const app = (pkg, name, recent = false) => ({ pkg, name, icon: "", recent });
    return [
      { kind: "connected", peer: "192.168.1.156" },
      { kind: "battery", level: 82, charging: true },
      { kind: "apps", apps: [
        app("com.whatsapp", "WhatsApp", true), app("com.spotify.music", "Spotify", true),
        app("com.android.chrome", "Chrome"), app("com.google.android.youtube", "YouTube"),
        app("com.instagram.android", "Instagram"), app("org.telegram.messenger", "Telegram"),
        app("com.google.android.apps.maps", "Maps"), app("com.coloros.gallery3d", "Photos"),
        app("com.netflix.mediaclient", "Netflix"), app("com.discord", "Discord"),
      ] },
      { kind: "transfer", id: "1", outgoing: true, state: "sending", files: ["holiday-video.mp4"], bytes: 734003200, done: 301989888 },
      { kind: "transfer", id: "2", outgoing: false, state: "done", files: ["IMG_2041.jpg", "IMG_2042.jpg", "IMG_2043.jpg"], bytes: 9437184, done: 9437184 },
      { kind: "notification", app: "WhatsApp", title: "Sam", text: "Are you still coming tonight?" },
    ];
  }
  if (cmd === "start_cast" || cmd === "pick_files_to_send" || cmd === "open_received_folder") {
    throw new Error(t("demo.need_app"));
  }
  return null;
}

(async function init() {
  let settings = null;
  try {
    settings = await invoke("get_settings");
  } catch (_) {}
  if (settings) {
    applyTheme(settings.theme);
    computerName = settings.computer_name || "";
  }
  try {
    await useLocale(resolveLang(settings));
  } catch (e) {
    uiLog(`i18n: ${e}`);
  }
  refreshDynamic();
  setupDrop();
  if (tauri?.event) {
    await tauri.event.listen("phone", (e) => onEvent(e.payload));
  }
  for (const ev of await invoke("phone_state")) onEvent(ev);
})();
