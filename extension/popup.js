// Popup: shows what the background found on the active tab and hands it to the desktop app.
// Everything shown comes from web pages, so it is only ever put into the DOM as text.
import { formatBytes, formatDuration, isMediaSiteHost, sanitizeFilename, suggestFilename } from "./lib/detect.js";
import { LINK_KINDS, MAX_BATCH, largestSrcset, linkFilter, normalizeLinks, textMatcher } from "./lib/links.js";

const $ = (id) => document.getElementById(id);

let tab = null;
let httpTab = false;
let mediaSite = false;
let state = { items: [], app: null, recordings: [], settings: null };
/** Item id → its list row. Rows are kept across refreshes so typed names and chosen qualities survive. */
const rows = new Map();
/** Recording key → its row, kept across refreshes so a focused Stop button is not replaced every second. */
const recRows = new Map();
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
  pill.textContent = app.ok
    ? `Connected · ${app.port}`
    : app.outdated
      ? "Update needed"
      : app.error
        ? "Extension error"
        : "Downloader not running";
  pill.title = app.ok ? `Version ${app.version || "?"}` : app.outdated ? `Older app on port ${app.port}` : app.error || "";
  pill.className = `pill ${app.ok ? "ok" : "bad"}`;
  $("app-hint").hidden = !!app.ok || !!app.outdated;
  $("app-outdated").hidden = !!app.ok || !app.outdated;
  // Without host access (Firefox grants it separately) the background sees no requests at all.
  $("host-access").hidden = state.hostAccess !== false;
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
  // What loaded before the extension was installed or updated was never seen; a reload replays it.
  $("reload-hint").hidden = shown.length > 0 || mediaSite;
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
  // Fallback for servers that refuse the app (TLS fingerprint, bot checks): the page itself fetches the media.
  row.via = el("button", {
    type: "button",
    className: "btn",
    textContent: "Via browser",
    title: "Download inside the page, for sites that block the downloader; progress shows under Record",
  });
  row.viaTitle = row.via.title;
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
    el("div", { className: "actions" }, row.download, row.via, row.copy, row.block, row.mp4Wrap, row.msg),
  );
  row.download.addEventListener("click", () => download(row));
  row.via.addEventListener("click", () => viaBrowser(row));
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
  const stream = item.kind === "hls" || item.kind === "dash";
  const chips = [[stream ? item.kind.toUpperCase() : String(item.format || "file").toUpperCase(), stream ? "hls" : ""]];
  if (item.inline) chips.push(["Inline", ""]);
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

  // An inline playlist was built by the page (blob:/data:); its URL is only the frame it came from.
  row.url.textContent = item.inline ? `Playlist built by the page · ${item.url}` : item.url;
  row.url.title = item.url;

  // Masters with separate audio go to the downloader whole (it merges the audio), so a choice there is a height:
  // only variants with one are offered, one per height. Inline masters can only be sent whole.
  const heights = new Set();
  const variants = !master || item.inline
    ? []
    : master.separateAudio
      ? master.variants.filter((v) => v.height > 0 && !heights.has(v.height) && heights.add(v.height))
      : master.variants;
  row.variants = variants;
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
      ? "Separate audio: the downloader merges it with the chosen quality."
      : item.kind === "dash"
        ? "DASH stream: the downloader fetches it and merges the audio."
        : hls?.live
          ? "Live stream: the downloader records it as it plays."
          : "";
  row.note.textContent = note;
  row.note.hidden = !note;
  row.download.disabled = !!hls?.drm;
  row.download.title = hls?.drm ? note : "";
  // The page downloads only what it can fetch once and whole: not inline, DASH, live or DRM streams, nor a file its
  // server lets only the <video> read (no CORS).
  row.via.hidden = !!item.inline || !(item.kind === "hls" || item.kind === "file") || item.pageCanFetch === false;
  row.via.disabled = !!(hls?.drm || hls?.live);
  row.via.title = hls?.live ? "Live streams can't be downloaded through the browser." : hls?.drm ? note : row.viaTitle;
  row.mp4Wrap.hidden = item.kind !== "hls";
  if (!row.mp4Set) row.mp4.checked = state.settings?.convertToMp4 !== false;

  row.host = hostOf(item.url);
  row.block.title = `Stop detecting media from ${row.host}`;
}

/** The selected variant URL, or the item's own URL. */
function chosenUrl(row) {
  return (!row.quality.hidden && row.quality.value) || row.item.url;
}

/** The chosen variant's height for a separate-audio master (sent with the master URL); null for "Best". */
function chosenHeight(row) {
  if (!row.item.hls?.separateAudio || row.quality.hidden) return null;
  return row.variants.find((v) => v.url === row.quality.value)?.height || null;
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
  const height = chosenHeight(row);
  const msg = { cmd: "SEND", tabId: tab.id, id: row.item.id, filename: chosenName(row), mp4: row.mp4.checked };
  // A height goes with the master URL; otherwise the chosen variant (or the item itself) is sent.
  if (height) Object.assign(msg, { url: row.item.url, height });
  else msg.url = chosenUrl(row);
  const reply = await ask(msg);
  row.download.disabled = !!row.item.hls?.drm;
  report(row.msg, reply, "Sent to the downloader.");
}

async function viaBrowser(row) {
  row.via.disabled = true;
  say(row.msg, "Starting…");
  const reply = await ask({ cmd: "BROWSER_DL", tabId: tab.id, id: row.item.id, url: chosenUrl(row), filename: chosenName(row) });
  row.via.disabled = !!(row.item.hls?.drm || row.item.hls?.live);
  report(row.msg, reply, "Downloading in the page; progress shows under Record. Keep the tab open.");
  if (reply.ok) refresh();
}

async function copy(row) {
  try {
    // An inline item's playlist text stays in the background (GET_STATE leaves it out), so its
    // frame URL is what is copied.
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
    const muted = reply.silent
      ? " Without sound: the browser won't unmute it before the page is clicked. Unmute it on the page to record its sound from then on."
      : v.muted
        ? " It was muted: it plays at near-zero volume so its sound is recorded, and is muted again after."
        : "";
    report($("rec-msg"), reply, `Recording video ${n + 1}. Let it play to the end or press Stop.${muted}`);
    if (reply.ok) refresh();
  });
  return el("li", {}, el("span", { className: "label", title: label, textContent: label }), button);
}

/**
 * The recordings and browser downloads of the tab, or that "Capture from start" is armed and waits for the video to
 * play. Browser downloads have their own progress and Stop; the shared Stop button ends the recordings.
 */
function renderRecordings(recs, armed) {
  recs = recs.filter((r) => r && typeof r.key === "string");
  $("recs").hidden = recs.length === 0 && !armed;
  $("armed").hidden = !armed || recs.length > 0;
  const list = $("rec-list");
  const keep = new Set(recs.map((r) => r.key));
  for (const [key, row] of recRows) {
    if (!keep.has(key)) {
      row.li.remove();
      recRows.delete(key);
    }
  }
  recs.forEach((r, i) => {
    let row = recRows.get(r.key);
    if (!row) {
      row = makeRecRow(r.key);
      recRows.set(r.key, row);
    }
    updateRecRow(row, r);
    if (list.children[i] !== row.li) list.insertBefore(row.li, list.children[i] || null);
  });
  $("stop").hidden = !armed && !recs.some((r) => r.mode !== "browser");
  $("speed-wrap").hidden = !recs.some((r) => r.mode === "mse");
  // Byte counts only move while something records (a failed browser download stays only to show why), so poll only then.
  const moving = recs.some((r) => !r.error);
  if (moving && !ticker) ticker = setInterval(refresh, 1000);
  else if (!moving && ticker) {
    clearInterval(ticker);
    ticker = null;
  }
}

function makeRecRow(key) {
  const row = {};
  row.label = el("span", { className: "label" });
  row.progress = el("progress", { max: 1, value: 0, hidden: true });
  row.info = el("span");
  row.stop = el("button", { type: "button", className: "btn small danger", textContent: "Stop", hidden: true });
  row.stop.addEventListener("click", async () => {
    row.stop.disabled = true;
    // A failed download's button dismisses it.
    const failed = row.failed;
    const reply = await ask({ cmd: "BROWSER_DL_CANCEL", tabId: tab.id, key });
    row.stop.disabled = false;
    report($("rec-msg"), reply, failed ? "" : "Browser download stopped.");
    refresh();
  });
  row.li = el("li", {}, row.label, row.progress, row.info, row.stop);
  return row;
}

/** Browser downloads show how far they got (done of total, as the page reports it); recordings their bytes. */
function updateRecRow(row, r) {
  const browser = r.mode === "browser";
  const kind = browser ? "Browser download" : r.mode === "msr" ? "Playback" : "Buffers";
  const title = `${kind} · ${r.title || "Untitled"}`;
  row.label.textContent = title;
  row.label.title = title;
  const bytes = formatBytes(Number(r.bytes) || 0);
  const tracks = Array.isArray(r.tracks) ? r.tracks.length : Number(r.tracks) || 0;
  const done = Math.max(0, Number(r.done) || 0);
  const total = Math.max(0, Number(r.total) || 0);
  const percent = browser && total > 0 ? Math.min(100, Math.floor((done / total) * 100)) : null;
  // A browser download that failed was thrown away; it stays to say why until dismissed.
  row.failed = browser && typeof r.error === "string" && r.error !== "";
  row.info.textContent = row.failed
    ? `Failed: ${r.error}`
    : percent !== null
      ? `${percent}% · ${bytes}`
      : `${bytes} · ${tracks} ${tracks === 1 ? "track" : "tracks"}`;
  row.info.className = row.failed ? "msg err" : "";
  row.progress.hidden = percent === null || row.failed;
  row.progress.max = total || 1;
  row.progress.value = Math.min(done, total);
  row.progress.setAttribute("aria-label", `${title}: ${percent ?? 0}%`);
  row.stop.hidden = !browser;
  row.stop.textContent = row.failed ? "Dismiss" : "Stop";
  row.stop.setAttribute("aria-label", `${row.failed ? "Dismiss" : "Stop"} ${title}`);
}

// ---- Links ----

/** The page's links (see normalizeLinks), the ones ticked, and the chips chosen; null until the page is read. */
let links = null;
const picked = new Set();
const kinds = new Set();

/**
 * Runs in the tab's top frame (isolated world); must not reference anything outside itself. The links and media of
 * the page and its same-origin frames, as the URLs the browser resolved, and srcsets with the base they resolve against.
 */
function collectPageLinks() {
  const urls = [];
  const srcsets = [];
  const rels = new Set(["alternate", "enclosure", "preload", "prefetch", "image_src"]);
  const visit = (doc) => {
    if (!doc) return;
    for (const node of doc.querySelectorAll("a[href], area[href]")) if (typeof node.href === "string") urls.push(node.href);
    for (const node of doc.querySelectorAll("img[src], video[src], audio[src], source[src]")) urls.push(node.src);
    for (const node of doc.querySelectorAll("img[srcset], source[srcset]")) srcsets.push([node.getAttribute("srcset"), doc.baseURI]);
    for (const node of doc.querySelectorAll("link[href][rel]")) {
      if (node.rel.toLowerCase().split(/\s+/).some((rel) => rels.has(rel))) urls.push(node.href);
    }
    // A cross-origin frame's document is null.
    for (const frame of doc.querySelectorAll("iframe, frame")) {
      try {
        visit(frame.contentDocument);
      } catch {
        // Not readable: skipped.
      }
    }
  };
  visit(document);
  return { urls, srcsets };
}

async function scanLinks() {
  say($("links-msg"), "Reading the page…");
  let found;
  try {
    [found] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: collectPageLinks });
  } catch (e) {
    links = [];
    renderLinks();
    return say($("links-msg"), `Can't read this page's links: ${e?.message || e}`, "err");
  }
  const { urls = [], srcsets = [] } = found?.result || {};
  links = normalizeLinks([...urls, ...srcsets.map(([srcset, base]) => largestSrcset(srcset, base))]);
  picked.clear();
  say($("links-msg"), "");
  renderLinks();
}

/** The links the filters let through. */
function shownLinks() {
  const filter = linkFilter({ kinds: [...kinds], query: $("links-query").value, sameSite: $("same-site").checked, pageHost: hostOf(tab.url) });
  return (links || []).filter(filter);
}

function renderLinks() {
  const shown = shownLinks();
  const chosen = shown.filter((link) => picked.has(link.url)).length;
  $("links-count").textContent = links?.length ? String(links.length) : "";
  $("links").replaceChildren(
    ...shown.map((link) => {
      const box = el("input", { type: "checkbox", checked: picked.has(link.url) });
      box.addEventListener("change", () => {
        if (box.checked) picked.add(link.url);
        else picked.delete(link.url);
        renderCount();
      });
      const label = el("label", { title: link.url }, box, el("span", { className: "label", textContent: link.url }));
      return el("li", {}, label, ...(link.kind ? [el("span", { className: "chip", textContent: link.kind })] : []));
    }),
  );
  $("no-links").hidden = links === null || shown.length > 0;
  $("no-links").textContent = textMatcher($("links-query").value) === null ? "That /regex/ is not valid." : "No links match.";
  renderCount(shown, chosen);
}

/** The count and the Send button, for the ticked links the filters show (those are what is sent). */
function renderCount(shown = shownLinks(), chosen = shown.filter((link) => picked.has(link.url)).length) {
  $("selected-count").textContent = `${chosen} of ${shown.length} selected`;
  $("send-links").textContent = `Send ${chosen} to app`;
  $("send-links").disabled = chosen === 0;
}

async function sendLinks() {
  const urls = shownLinks().filter((link) => picked.has(link.url)).map((link) => link.url);
  if (urls.length > MAX_BATCH) return say($("links-msg"), `Send at most ${MAX_BATCH} links at once.`, "err");
  $("send-links").disabled = true;
  say($("links-msg"), "Sending…");
  const reply = await ask({ cmd: "SEND_LINKS", tabId: tab.id, urls });
  report($("links-msg"), reply, `Sent ${urls.length} ${urls.length === 1 ? "link" : "links"} to the downloader.`);
  renderCount();
}

function showView(id) {
  for (const button of document.querySelectorAll(".views .view")) button.setAttribute("aria-pressed", String(button.dataset.view === id));
  $("media-view").hidden = id !== "media-view";
  $("links-view").hidden = id !== "links-view";
  if (id === "links-view" && links === null) scanLinks();
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
    mediaSite = isMediaSiteHost(hostOf(tab.url));
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
  $("reload").addEventListener("click", () => chrome.tabs.reload(tab.id).catch(() => {}));
  $("grant").addEventListener("click", async () => {
    // Called straight from the click: browsers only show the prompt during a user gesture.
    const granted = await chrome.permissions.request({ origins: ["<all_urls>"] }).catch((e) => {
      say($("grant-msg"), `Couldn't ask for access: ${e?.message || e}`, "err");
      return null;
    });
    if (granted === false) say($("grant-msg"), "Access was not granted.", "err");
    if (granted) refresh();
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
  for (const button of document.querySelectorAll(".views .view")) button.addEventListener("click", () => showView(button.dataset.view));
  $("kinds").replaceChildren(
    ...Object.keys(LINK_KINDS).map((kind) => {
      const chip = el("button", { type: "button", className: "chip", textContent: kind });
      chip.setAttribute("aria-pressed", "false");
      chip.addEventListener("click", () => {
        if (kinds.has(kind)) kinds.delete(kind);
        else kinds.add(kind);
        chip.setAttribute("aria-pressed", String(kinds.has(kind)));
        renderLinks();
      });
      return chip;
    }),
  );
  $("links-query").addEventListener("input", renderLinks);
  $("same-site").addEventListener("change", renderLinks);
  $("links-rescan").addEventListener("click", scanLinks);
  $("send-links").addEventListener("click", sendLinks);
  for (const [id, tick] of [["select-all", true], ["select-none", false]]) {
    $(id).addEventListener("click", () => {
      for (const link of shownLinks()) {
        if (tick) picked.add(link.url);
        else picked.delete(link.url);
      }
      renderLinks();
    });
  }
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
