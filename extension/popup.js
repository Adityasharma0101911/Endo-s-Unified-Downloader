// Popup: shows what the background found on the active tab and hands it to the desktop app.
// Everything shown comes from web pages, so it is only ever put into the DOM as text.
import { formatBytes, formatDuration, isMediaSiteHost, sanitizeFilename, suggestFilename } from "./lib/detect.js";

const $ = (id) => document.getElementById(id);

let tab = null;
let httpTab = false;
let state = { items: [], app: null, recordings: [], settings: null };
/** Item id → its list row. Rows are kept across refreshes so typed names and chosen qualities survive. */
const rows = new Map();
let blockedKey = null;
let ticker = null;

/** Creates an element with the given properties and children. */
function el(tag, props = {}, ...children) {
  const node = Object.assign(document.createElement(tag), props);
  node.append(...children);
  return node;
}

/** Sends a message to the background; always resolves to an object so callers only check `ok`. */
async function ask(msg) {
  try {
    const reply = await chrome.runtime.sendMessage(msg);
    return reply && typeof reply === "object" ? reply : { ok: false, error: "No reply from the extension." };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  }
}

/** Writes a status line; `kind` is "ok", "err" or "" (neutral). */
function say(node, text, kind = "") {
  node.textContent = text;
  node.className = `msg ${kind}`;
}

/** Shows a background reply: `success` when it worked, else the reply's error text. */
function report(node, reply, success) {
  if (reply.ok) say(node, success, "ok");
  else say(node, reply.error || "Failed.", "err");
}

function hostOf(url) {
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
}

/** "1080p · 5.2 Mbps" for an HLS variant. */
function variantLabel(v) {
  const parts = [v.height ? `${v.height}p` : v.resolution ? v.resolution.replace("x", "×") : "Variant"];
  if (v.bandwidth > 0) parts.push(`${(v.bandwidth / 1e6).toFixed(1)} Mbps`);
  return parts.join(" · ");
}

// Refreshes never overlap: a request that arrives during one makes it run once more.
let refreshing = null;
let again = false;
function refresh() {
  if (refreshing) {
    again = true;
    return refreshing;
  }
  refreshing = (async () => {
    do {
      again = false;
      if (tab) tab = await chrome.tabs.get(tab.id).catch(() => tab);
      const reply = await ask({ cmd: "GET_STATE", tabId: tab?.id ?? -1 });
      state = Array.isArray(reply.items) ? reply : { ...state, app: { ok: false, error: reply.error } };
      render();
    } while (again);
  })().finally(() => {
    refreshing = null;
  });
  return refreshing;
}

function render() {
  const app = state.app || {};
  const pill = $("app-status");
  pill.textContent = app.ok ? `Connected · ${app.port}` : app.error ? "Extension error" : "Downloader not running";
  pill.title = app.ok ? `Version ${app.version || "?"}` : app.error || "";
  pill.className = `pill ${app.ok ? "ok" : "bad"}`;
  $("app-hint").hidden = !!app.ok;
  if (state.settings) renderSettings(state.settings);
  if (!httpTab) return;
  renderItems(state.items || []);
  renderRecordings(Array.isArray(state.recordings) ? state.recordings : [], state.armed === true);
}

// ---- Media list ----

function renderItems(items) {
  const list = $("items");
  const shown = items.filter((it) => it && !it.hidden).sort((a, b) => (b.time || 0) - (a.time || 0));
  const keep = new Set(shown.map((it) => it.id));
  for (const [id, row] of rows) {
    if (!keep.has(id)) {
      row.li.remove();
      rows.delete(id);
    }
  }
  shown.forEach((item, i) => {
    let row = rows.get(item.id);
    if (!row) {
      row = makeRow();
      rows.set(item.id, row);
    }
    updateRow(row, item);
    // Only move rows that are out of place, so a focused input is not detached.
    if (list.children[i] !== row.li) list.insertBefore(row.li, list.children[i] || null);
  });
  $("media-count").textContent = shown.length ? String(shown.length) : "";
  $("empty").hidden = shown.length > 0;
  $("clear").hidden = shown.length === 0;
}

function makeRow() {
  const row = { edited: false, mp4Set: false, chipsKey: "", qualityKey: null, blockTimer: 0 };
  row.name = el("input", { className: "name", type: "text", spellcheck: false });
  row.name.setAttribute("aria-label", "File name");
  row.name.addEventListener("input", () => {
    row.edited = true;
    row.name.title = row.name.value;
  });
  row.chips = el("div", { className: "chips" });
  row.url = el("div", { className: "url" });
  row.quality = el("select", { className: "quality", hidden: true });
  row.quality.setAttribute("aria-label", "Quality");
  row.note = el("p", { className: "note", hidden: true });
  // HLS only: whether the app turns the saved stream into an MP4. Follows the setting until the user ticks it.
  row.mp4 = el("input", { type: "checkbox" });
  row.mp4.setAttribute("aria-label", "Convert to MP4");
  row.mp4.addEventListener("change", () => {
    row.mp4Set = true;
  });
  row.mp4Wrap = el("label", { className: "check", title: "Convert the stream to MP4 once downloaded", hidden: true }, row.mp4, "MP4");
  row.download = el("button", { type: "button", className: "btn primary", textContent: "Download" });
  row.copy = el("button", { type: "button", className: "btn", textContent: "Copy URL" });
  row.block = el("button", { type: "button", className: "btn ghost", textContent: "Block host" });
  row.msg = el("p", { className: "msg" });
  row.msg.setAttribute("role", "status");
  row.li = el(
    "li",
    { className: "item" },
    row.name,
    row.chips,
    row.url,
    row.quality,
    row.note,
    el("div", { className: "actions" }, row.download, row.copy, row.block, row.mp4Wrap, row.msg),
  );
  row.download.addEventListener("click", () => download(row));
  row.copy.addEventListener("click", () => copy(row));
  row.block.addEventListener("click", () => block(row));
  return row;
}

function updateRow(row, item) {
  row.item = item;
  const hls = item.hls && typeof item.hls === "object" ? item.hls : null;
  row.ext = item.kind === "hls" ? "mp4" : String(item.format || "mp4");
  row.defaultName = suggestFilename(item.title || tab?.title || "video", row.ext);
  if (!row.edited && row.name.value !== row.defaultName) {
    row.name.value = row.defaultName;
    row.name.title = row.defaultName;
  }

  const master = hls?.kind === "master" && Array.isArray(hls.variants) ? hls : null;
  const best = master?.variants[0];
  const chips = [[item.kind === "hls" ? "HLS" : String(item.format || "file").toUpperCase(), item.kind === "hls" ? "hls" : ""]];
  if (item.size > 0) chips.push([formatBytes(item.size), ""]);
  if (hls?.duration > 0 && !hls.live) chips.push([formatDuration(hls.duration), ""]);
  if (best && (best.resolution || best.height)) chips.push([best.resolution ? best.resolution.replace("x", "×") : `${best.height}p`, ""]);
  if (hls?.live) chips.push(["LIVE", "live"]);
  if (hls?.encrypted) chips.push(["AES-128", ""]);
  if (hls?.drm) chips.push(["DRM", "drm"]);
  const chipsKey = JSON.stringify(chips);
  if (chipsKey !== row.chipsKey) {
    row.chipsKey = chipsKey;
    row.chips.replaceChildren(...chips.map(([text, cls]) => el("span", { className: `chip ${cls}`, textContent: text })));
  }

  row.url.textContent = item.url;
  row.url.title = item.url;

  // Masters with separate audio go to the downloader whole: it picks the best video and merges the audio.
  const variants = master && !master.separateAudio ? master.variants : [];
  const qualityKey = variants.map((v) => v.url).join("\n");
  if (qualityKey !== row.qualityKey) {
    const chosen = row.quality.value;
    row.qualityKey = qualityKey;
    row.quality.replaceChildren(
      el("option", { value: item.url, textContent: "Best" }),
      ...variants.map((v) => el("option", { value: v.url, textContent: variantLabel(v) })),
    );
    if (variants.some((v) => v.url === chosen)) row.quality.value = chosen;
  }
  row.quality.hidden = variants.length === 0;

  const note = hls?.drm
    ? "DRM-protected (SAMPLE-AES or a key system): it can't be downloaded."
    : master?.separateAudio
      ? "Best quality, audio merged by the downloader."
      : hls?.live
        ? "Live stream: the downloader records it as it plays."
        : "";
  row.note.textContent = note;
  row.note.hidden = !note;
  row.download.disabled = !!hls?.drm;
  row.download.title = hls?.drm ? note : "";
  row.mp4Wrap.hidden = item.kind !== "hls";
  if (!row.mp4Set) row.mp4.checked = state.settings?.convertToMp4 !== false;

  row.host = hostOf(item.url);
  row.block.title = `Stop detecting media from ${row.host}`;
}

/** The selected variant URL, or the item's own URL. */
function chosenUrl(row) {
  return (!row.quality.hidden && row.quality.value) || row.item.url;
}

/** The typed file name, cleaned and with an extension; the suggested name when left empty. */
function chosenName(row) {
  const typed = row.name.value.trim();
  if (!typed) return row.defaultName;
  const name = sanitizeFilename(typed);
  return /\.[A-Za-z0-9]{2,5}$/.test(name) ? name : `${name}.${row.ext}`;
}

async function download(row) {
  row.download.disabled = true;
  say(row.msg, "Sending…");
  const reply = await ask({ cmd: "SEND", tabId: tab.id, id: row.item.id, url: chosenUrl(row), filename: chosenName(row), mp4: row.mp4.checked });
  row.download.disabled = !!row.item.hls?.drm;
  report(row.msg, reply, "Sent to the downloader.");
}

async function copy(row) {
  try {
    await navigator.clipboard.writeText(chosenUrl(row));
    say(row.msg, "URL copied.", "ok");
  } catch (e) {
    say(row.msg, `Couldn't copy: ${e?.message || e}`, "err");
  }
}

/** First click asks for confirmation; a second click within 4 s blocks the host. */
async function block(row) {
  if (!row.host) return say(row.msg, "This URL has no host to block.", "err");
  clearTimeout(row.blockTimer);
  const reset = () => {
    row.blockTimer = 0;
    row.block.textContent = "Block host";
    row.block.classList.remove("confirm");
  };
  if (!row.blockTimer) {
    row.block.textContent = "Confirm block";
    row.block.classList.add("confirm");
    say(row.msg, `Media from ${row.host} will no longer be detected.`);
    row.blockTimer = setTimeout(reset, 4000);
    return;
  }
  reset();
  const reply = await ask({ cmd: "BLOCK_HOST", host: row.host });
  report(row.msg, reply, `Blocked ${row.host}.`);
  if (reply.ok) refresh();
}

// ---- Recording ----

/** Runs inside every frame of the tab (isolated world); must not reference anything outside itself. */
function listVideos() {
  return Array.from(document.querySelectorAll("video"), (v, index) => {
    const src = v.currentSrc || v.src || "";
    return {
      index,
      width: v.videoWidth,
      height: v.videoHeight,
      duration: Number.isFinite(v.duration) ? v.duration : null,
      live: v.duration === Infinity,
      blob: src.startsWith("blob:") || !!v.srcObject,
      muted: v.muted,
      paused: v.paused,
    };
  });
}

async function scanVideos() {
  let results;
  try {
    results = await chrome.scripting.executeScript({ target: { tabId: tab.id, allFrames: true }, func: listVideos });
  } catch (e) {
    $("videos").replaceChildren();
    $("no-videos").hidden = true;
    return say($("rec-msg"), `Can't look for videos on this page: ${e?.message || e}`, "err");
  }
  const videos = results.flatMap((r) =>
    Array.isArray(r?.result) ? r.result.filter((v) => Number.isInteger(v?.index)).map((v) => ({ ...v, frameId: r.frameId })) : [],
  );
  $("videos").replaceChildren(...videos.map(videoRow));
  $("no-videos").hidden = videos.length > 0;
}

function videoRow(v, n) {
  const bits = [`Video ${n + 1}`];
  if (v.width > 0 && v.height > 0) bits.push(`${v.width}×${v.height}`);
  bits.push(v.live ? "LIVE" : v.duration > 0 ? formatDuration(v.duration) : "not loaded");
  if (v.blob) bits.push("blob");
  if (v.muted) bits.push("muted");
  bits.push(v.paused ? "paused" : "playing");
  if (v.frameId) bits.push("in a frame");
  const label = bits.join(" · ");
  const button = el("button", { type: "button", className: "btn small", textContent: "Record playback" });
  button.setAttribute("aria-label", `Record playback of video ${n + 1}`);
  button.addEventListener("click", async () => {
    button.disabled = true;
    const reply = await ask({ cmd: "MSR_START", tabId: tab.id, frameId: v.frameId, index: v.index });
    button.disabled = false;
    report($("rec-msg"), reply, `Recording video ${n + 1}. Let it play to the end or press Stop.`);
    if (reply.ok) refresh();
  });
  return el("li", {}, el("span", { className: "label", title: label, textContent: label }), button);
}

/** The recordings of the tab, or that "Capture from start" is armed and waits for the video to play. */
function renderRecordings(recs, armed) {
  $("recs").hidden = recs.length === 0 && !armed;
  $("armed").hidden = !armed || recs.length > 0;
  $("rec-list").replaceChildren(
    ...recs.map((r) => {
      const title = `${r.mode === "msr" ? "Playback" : "Buffers"} · ${r.title || "Untitled"}`;
      const tracks = Array.isArray(r.tracks) ? r.tracks.length : Number(r.tracks) || 0;
      return el(
        "li",
        {},
        el("span", { className: "label", title, textContent: title }),
        el("span", { textContent: `${formatBytes(Number(r.bytes) || 0)} · ${tracks} ${tracks === 1 ? "track" : "tracks"}` }),
      );
    }),
  );
  $("speed-wrap").hidden = !recs.some((r) => r.mode === "mse");
  // Byte counts only move while something records, so poll only then.
  if (recs.length && !ticker) ticker = setInterval(refresh, 1000);
  else if (!recs.length && ticker) {
    clearInterval(ticker);
    ticker = null;
  }
}

// ---- Settings ----

function renderSettings(s) {
  for (const [id, key] of [["min-size", "minSizeKB"], ["max-size", "maxSizeKB"]]) {
    const input = $(id);
    if (document.activeElement !== input) input.value = String(s[key] ?? 0);
  }
  $("convert-mp4").checked = s.convertToMp4 !== false;
  const hosts = Array.isArray(s.blockedHosts) ? s.blockedHosts : [];
  const key = hosts.join("\n");
  if (key === blockedKey) return;
  blockedKey = key;
  $("blocked").replaceChildren(
    ...hosts.map((host) => {
      const remove = el("button", { type: "button", className: "btn small ghost", textContent: "Remove" });
      remove.setAttribute("aria-label", `Unblock ${host}`);
      remove.addEventListener("click", () =>
        saveSettings({ blockedHosts: (state.settings.blockedHosts || []).filter((h) => h !== host) }, `Unblocked ${host}.`),
      );
      return el("li", {}, el("span", { className: "label", title: host, textContent: host }), remove);
    }),
  );
  $("no-blocked").hidden = hosts.length > 0;
}

async function saveSettings(patch, success) {
  if (!state.settings) return say($("settings-msg"), "Settings haven't loaded; reopen the popup.", "err");
  const settings = { ...state.settings, ...patch };
  const reply = await ask({ cmd: "SET_SETTINGS", settings });
  if (reply.ok) state.settings = settings;
  report($("settings-msg"), reply, success);
  renderSettings(state.settings);
}

function sizeInput(id, key) {
  const input = $(id);
  input.addEventListener("change", () => {
    const n = Number(input.value);
    const next = { ...state.settings, [key]: n };
    let error = "";
    if (input.value.trim() === "" || !Number.isInteger(n) || n < 0) error = "Enter a whole number of KB (0 = no limit).";
    else if (next.minSizeKB > 0 && next.maxSizeKB > 0 && next.maxSizeKB < next.minSizeKB) error = "Max size must be at least the min size.";
    if (error) {
      say($("settings-msg"), error, "err");
      input.value = String(state.settings?.[key] ?? 0);
      return;
    }
    if (n !== state.settings?.[key]) saveSettings({ [key]: n }, "Saved.");
  });
}

// ---- Wiring ----

async function init() {
  [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  httpTab = /^https?:\/\//i.test(tab?.url || "");
  $("unsupported").hidden = httpTab;
  $("tab-ui").hidden = !httpTab;

  if (httpTab) {
    const mediaSite = isMediaSiteHost(hostOf(tab.url));
    $("send-page").classList.toggle("primary", mediaSite);
    $("page-note").hidden = !mediaSite;
    if (mediaSite) $("empty").textContent = "Streams aren't collected on this site: use Download this page.";
    scanVideos();
  }

  $("send-page").addEventListener("click", async () => {
    const button = $("send-page");
    button.disabled = true;
    say($("page-msg"), "Sending…");
    const reply = await ask({ cmd: "SEND_URL", url: tab.url, referer: tab.url });
    button.disabled = false;
    report($("page-msg"), reply, "Page sent to the downloader.");
  });
  $("clear").addEventListener("click", async () => {
    const reply = await ask({ cmd: "CLEAR", tabId: tab.id });
    if (reply.ok) say($("media-msg"), "");
    else report($("media-msg"), reply, "");
    refresh();
  });
  $("rescan").addEventListener("click", () => {
    say($("rec-msg"), "");
    scanVideos();
  });
  $("arm").addEventListener("click", async () => {
    const reply = await ask({ cmd: "REC_ARM", tabId: tab.id });
    report($("rec-msg"), reply, "Armed: the page reloads. Play the video; captured bytes show here. Rescan after it loads.");
  });
  $("stop").addEventListener("click", async () => {
    const reply = await ask({ cmd: "REC_STOP", tabId: tab.id });
    report($("rec-msg"), reply, "Stopped. The app joins the tracks and saves the file.");
    refresh();
  });
  $("speed").addEventListener("change", async () => {
    const rate = Number($("speed").value);
    const reply = await ask({ cmd: "REC_SPEED", tabId: tab.id, rate });
    report($("rec-msg"), reply, `Playback speed ${rate}×.`);
  });
  sizeInput("min-size", "minSizeKB");
  sizeInput("max-size", "maxSizeKB");
  $("convert-mp4").addEventListener("change", (event) =>
    saveSettings({ convertToMp4: event.target.checked }, "Saved.").then(() => refresh()),
  );

  chrome.runtime.onMessage.addListener((msg) => {
    if (msg?.cmd === "STATE_CHANGED" && msg.tabId === tab?.id) refresh();
  });
  await refresh();
}

init();
