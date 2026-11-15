// Popup: shows what the background found on the active tab and hands it to the desktop app.
// Everything shown comes from web pages, so it is only ever put into the DOM as text.
import { parseSites, parseTypes } from "./lib/capture.js";
import { NO_YOUTUBE, formatDuration, isMediaSiteHost, isOffLimits, sanitizeFilename, suggestFilename } from "./lib/detect.js";
import { LINK_KINDS, MAX_BATCH, fullSizeImages, largestSrcset, linkFilter, normalizeLinks, siteOf, textMatcher } from "./lib/links.js";
import { isMediaPage, mediaPayload, pickQuality, rememberedChoice, subtitleChoices, videoChoices } from "./lib/media.js";
import { drmNote, isProtected, itemChips, itemMeta, itemNote, linkName, logView, queueView, recordingView, variantLabel } from "./lib/view.js";

const $ = (id) => document.getElementById(id);
/** The tabs of the popup; each has a `tab-<name>` button and a `<name>-view` panel. */
const VIEWS = ["media", "links", "record", "downloads", "settings"];
const VIA_HINT = "For sites that block the app";

let tab = null;
let httpTab = false;
let mediaSite = false;
/** The tab shows a media site's video, list or channel: the media card stands in for the page card. */
let mediaPage = false;
let state = { items: [], app: null, recordings: [], settings: null };
/** Item id → its card. Cards are kept across refreshes so typed names and chosen qualities survive. */
const rows = new Map();
/** Recording key → its row, kept across refreshes so a focused Stop button is not replaced every second. */
const recRows = new Map();
let blockedKey = null;
let ticker = null;
/** Where the last tab shown in this window is remembered. */
let viewKey = "";
/** The last request that failed because the app wasn't running: "Start the app" sends it again. */
let lastFailed = null;

/** Creates an element with the given properties and children. */
function el(tag, props = {}, ...children) {
  const node = Object.assign(document.createElement(tag), props);
  node.append(...children);
  return node;
}

/** A Phosphor icon of the sprite in popup.html. */
function icon(name) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

/** A button with an optional icon; its text is in `.label`, so it can change without losing the icon. */
function button(text, className, iconName = "") {
  const label = el("span", { textContent: text });
  const node = el("button", { type: "button", className }, ...(iconName ? [icon(iconName)] : []), label);
  node.label = label;
  return node;
}

/** An icon-only button, named for screen readers and in its tooltip. */
function iconButton(iconName, name, className = "btn ghost icon-btn") {
  const node = el("button", { type: "button", className, title: name }, icon(iconName));
  node.setAttribute("aria-label", name);
  return node;
}

/** Sends a message to the background; always resolves to an object so callers only check `ok`. */
async function ask(msg) {
  try {
    const reply = await chrome.runtime.sendMessage(msg);
    if (!reply || typeof reply !== "object") return { ok: false, error: "No reply from the extension." };
    // The app went away: its banner shows, with "Start the app".
    if (reply.notRunning) {
      lastFailed = msg.cmd === "START_APP" ? msg.retry : msg;
      refresh();
    }
    return reply;
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  }
}

/** Writes a status line; `kind` is "ok", "err" or "" (neutral). An error stays until replaced or dismissed. */
function say(node, text, kind = "") {
  node.textContent = text;
  node.className = `msg ${kind}`;
  if (kind === "err" && text) {
    const close = iconButton("x", "Dismiss");
    close.addEventListener("click", () => say(node, ""));
    node.append(close);
  }
}

/** Shows a background reply: `success` when it worked, else the reply's error text. */
function report(node, reply, success) {
  if (reply.ok) say(node, success, "ok");
  else say(node, reply.error || "Failed.", "err");
}

/** A tab's count badge; empty (hidden) at zero. */
function setCount(id, n) {
  $(id).textContent = n > 0 ? String(n) : "";
}

function hostOf(url) {
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
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
  const [tone, text] = app.ok
    ? ["green", app.version ? `Connected · v${app.version}` : "Connected"]
    : app.outdated
      ? ["amber", "Update the app"]
      : app.error
        ? ["red", "Extension error"]
        : ["red", "Not running"];
  const pill = $("app-status");
  // Unchanged text is left alone so the live region does not repeat it every second.
  if (pill.textContent !== text) pill.textContent = text;
  pill.className = `pill tone-${tone}`;
  pill.title = app.ok ? `Desktop app on port ${app.port}` : app.outdated ? `Older app on port ${app.port}` : app.error || "Start the desktop app";
  $("about-app").textContent = app.ok ? `v${app.version || "?"} · port ${app.port}` : app.outdated ? "An older version: update it" : "Not running";
  // A download of the app's whose link expired took a new link the browser came upon.
  if (state.rescued && !$("start-msg").textContent) say($("start-msg"), `Rescued: ${state.rescued}, resuming`, "ok");
  $("app-hint").hidden = !!app.ok || !!app.outdated;
  $("app-outdated").hidden = !!app.ok || !app.outdated;
  // Without host access (Firefox grants it separately) the background sees no requests at all.
  $("host-access").hidden = state.hostAccess !== false;
  if (state.settings) renderSettings(state.settings);
  renderLog(Array.isArray(state.log) ? state.log : []);
  if (!httpTab) return;
  $("page-title").textContent = tab.title || hostOf(tab.url);
  $("page-host").textContent = hostOf(tab.url);
  // A video of the page was given DRM keys: nothing of it can be downloaded or recorded, so the popup says so first.
  $("drm-note").hidden = !state.drm?.on;
  $("drm-text").textContent = state.drm?.on ? drmNote(state.drm.system) : "";
  renderItems(state.items || []);
  renderRecordings(Array.isArray(state.recordings) ? state.recordings : [], state.armed === true);
}

// ---- Tabs ----

/** Shows one view; `remember` keeps it as this window's tab for the next time the popup opens. */
function showView(name, { focus = false, remember = false } = {}) {
  for (const view of VIEWS) {
    const button = $(`tab-${view}`);
    button.setAttribute("aria-selected", String(view === name));
    button.tabIndex = view === name ? 0 : -1;
    $(`${view}-view`).hidden = view !== name;
  }
  if (focus) $(`tab-${name}`).focus();
  if (remember) {
    try {
      localStorage.setItem(viewKey, name);
    } catch {
      // No storage (private window, blocked site data): the popup opens on Media.
    }
  }
  if (name === "links" && links === null && httpTab) scanLinks();
  pollQueue(name === "downloads");
}

/** Arrow keys, Home and End move between the enabled tabs (the WAI-ARIA tabs pattern). */
function onTabKey(event) {
  const enabled = VIEWS.filter((view) => !$(`tab-${view}`).disabled);
  const at = enabled.indexOf(event.target.id?.replace(/^tab-/, ""));
  const to = { ArrowRight: at + 1, ArrowLeft: at - 1, Home: 0, End: enabled.length - 1 }[event.key];
  if (at < 0 || to === undefined) return;
  event.preventDefault();
  showView(enabled[(to + enabled.length) % enabled.length], { focus: true, remember: true });
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
  setCount("media-count", shown.length);
  $("media-head").hidden = shown.length === 0;
  $("empty").hidden = shown.length > 0 || mediaPage;
}

/** A menu entry: icon, label and a smaller line under it. */
function menuItem(iconName, text) {
  const node = el("button", { type: "button", className: "menu-item" }, icon(iconName), el("span", { textContent: text }), el("small"));
  [node.label, node.sub] = [node.children[1], node.children[2]];
  return node;
}

function closeMenu(row) {
  if (row.menu.matches(":popover-open")) row.menu.hidePopover();
}

function makeRow() {
  const row = { edited: false, mp4Set: false, chipsKey: "", qualityKey: null, blockTimer: 0 };
  row.chips = el("div", { className: "chips" });
  row.meta = el("span", { className: "meta" });
  row.name = el("input", { className: "name", type: "text", spellcheck: false, title: "File name: type to rename" });
  row.name.setAttribute("aria-label", "File name");
  row.name.addEventListener("input", () => {
    row.edited = true;
  });
  row.url = el("div", { className: "url" });
  row.note = el("p", { className: "note", hidden: true });
  row.quality = el("select", { className: "quality", hidden: true });
  row.quality.setAttribute("aria-label", "Quality");
  // HLS only: whether the app turns the saved stream into an MP4. Follows the setting until the user ticks it.
  row.mp4 = el("input", { type: "checkbox", className: "switch" });
  row.mp4.setAttribute("role", "switch");
  row.mp4.setAttribute("aria-label", "Convert to MP4");
  row.mp4.addEventListener("change", () => {
    row.mp4Set = true;
  });
  row.mp4Wrap = el("label", { className: "check", title: "Convert the stream to MP4 once downloaded", hidden: true }, row.mp4, "MP4");
  // The page's subtitle files (see SEND_SUBS), downloaded beside the video under its name when ticked.
  row.subs = el("input", { type: "checkbox", className: "switch" });
  row.subs.setAttribute("role", "switch");
  row.subsText = el("span");
  row.subsWrap = el("label", { className: "check", title: "Also download the page's subtitle files, named after the video", hidden: true }, row.subs, row.subsText);
  row.download = button("Download", "btn primary", "download");
  row.more = iconButton("dots", "More actions", "btn icon-btn");
  // Fallback for servers that refuse the app (TLS fingerprint, bot checks): the page itself fetches the media.
  row.via = menuItem("globe", "Download via browser");
  row.copy = menuItem("copy", "Copy URL");
  row.block = menuItem("prohibit", "Block host");
  row.block.sub.textContent = "Stop detecting media from it";
  row.menu = el("div", { className: "menu" }, row.via, row.copy, row.block);
  row.menu.popover = "auto";
  row.more.popoverTargetElement = row.menu;
  // ponytail: placed under the button, or over it when ~170 px (the tallest menu: a live item's) don't fit below;
  // measure the menu if it grows.
  row.menu.addEventListener("beforetoggle", (event) => {
    if (event.newState !== "open") return;
    const at = row.more.getBoundingClientRect();
    const up = at.bottom + 170 > innerHeight;
    Object.assign(row.menu.style, {
      right: `${Math.max(4, innerWidth - at.right)}px`,
      top: up ? "auto" : `${at.bottom + 4}px`,
      bottom: up ? `${innerHeight - at.top + 4}px` : "auto",
    });
  });
  row.msg = el("p", { className: "msg" });
  row.msg.setAttribute("role", "status");
  row.li = el(
    "li",
    { className: "card item" },
    el("div", { className: "item-head" }, row.chips, row.meta),
    row.name,
    row.url,
    row.note,
    el("div", { className: "item-actions" }, row.quality, el("span", { className: "spacer" }), row.subsWrap, row.mp4Wrap, row.download, row.more),
    row.msg,
    row.menu,
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
  if (!row.edited && row.name.value !== row.defaultName) row.name.value = row.defaultName;

  const chips = itemChips(item);
  const chipsKey = JSON.stringify(chips);
  if (chipsKey !== row.chipsKey) {
    row.chipsKey = chipsKey;
    row.chips.replaceChildren(...chips.map(([text, tone]) => el("span", { className: tone ? `chip tone-${tone}` : "chip", textContent: text })));
  }
  row.meta.textContent = itemMeta(item);

  // An inline playlist was built by the page (blob:/data:); its URL is only the frame it came from.
  row.url.textContent = item.inline ? `Playlist built by the page · ${item.url}` : item.url;
  row.url.title = item.url;

  // Masters with separate audio go to the downloader whole (it merges the audio), so a choice there is a height:
  // only variants with one are offered, one per height. Inline masters can only be sent whole.
  const master = hls?.kind === "master" && Array.isArray(hls.variants) ? hls : null;
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
      el("option", { value: item.url, textContent: "Best quality" }),
      ...variants.map((v) => el("option", { value: v.url, textContent: variantLabel(v) })),
    );
    if (variants.some((v) => v.url === chosen)) row.quality.value = chosen;
  }
  row.quality.hidden = variants.length === 0;

  const note = itemNote(item);
  row.note.textContent = note;
  row.note.hidden = !note;
  const locked = isProtected(item);
  row.download.disabled = locked;
  row.download.title = locked ? note : "";
  // The page downloads only what it can fetch once and whole: not inline, DASH, live or DRM streams, nor a file its
  // server lets only the <video> read (no CORS).
  row.via.hidden = !!item.inline || !(item.kind === "hls" || item.kind === "file") || item.pageCanFetch === false;
  row.via.disabled = locked || !!hls?.live;
  row.via.sub.textContent = hls?.live ? "Live streams can't be downloaded through the browser." : locked ? "DRM-protected: it can't be downloaded." : VIA_HINT;
  row.mp4Wrap.hidden = item.kind !== "hls" || locked;
  if (!row.mp4Set) row.mp4.checked = state.settings?.convertToMp4 !== false;
  const subs = Array.isArray(state.subs) ? state.subs.length : 0;
  row.subsWrap.hidden = subs === 0 || locked;
  row.subsText.textContent = `Subs (${subs})`;

  row.host = hostOf(item.url);
  if (!row.blockTimer) row.block.label.textContent = row.host ? `Block ${row.host}` : "Block host";
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
  if (reply.ok && row.subs.checked && !row.subsWrap.hidden) {
    // Named after the video: "<name>.<language>.vtt" beside it, which players pick up.
    const subs = await ask({ cmd: "SEND_SUBS", tabId: tab.id, base: chosenName(row).replace(/\.[^.]+$/, "") });
    report(row.msg, subs, `Sent to the downloader, with ${subs.count} subtitle ${subs.count === 1 ? "file" : "files"}.`);
  } else report(row.msg, reply, "Sent to the downloader.");
  row.download.disabled = isProtected(row.item);
}

async function viaBrowser(row) {
  closeMenu(row);
  row.via.disabled = true;
  say(row.msg, "Starting…");
  const reply = await ask({ cmd: "BROWSER_DL", tabId: tab.id, id: row.item.id, url: chosenUrl(row), filename: chosenName(row) });
  row.via.disabled = isProtected(row.item) || !!row.item.hls?.live;
  report(row.msg, reply, "Downloading in the page; progress shows under Record. Keep the tab open.");
  if (reply.ok) refresh();
}

async function copy(row) {
  closeMenu(row);
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
  if (!row.host) {
    closeMenu(row);
    return say(row.msg, "This URL has no host to block.", "err");
  }
  clearTimeout(row.blockTimer);
  const reset = () => {
    row.blockTimer = 0;
    row.block.label.textContent = `Block ${row.host}`;
    row.block.sub.textContent = "Stop detecting media from it";
    row.block.classList.remove("confirm");
  };
  if (!row.blockTimer) {
    row.block.label.textContent = "Click again to block";
    row.block.sub.textContent = row.host;
    row.block.classList.add("confirm");
    say(row.msg, `Media from ${row.host} will no longer be detected.`);
    row.blockTimer = setTimeout(reset, 4000);
    return;
  }
  reset();
  closeMenu(row);
  const reply = await ask({ cmd: "BLOCK_HOST", host: row.host });
  report(row.msg, reply, `Blocked ${row.host}.`);
  if (reply.ok) refresh();
}

// ---- Media card ----

/** What /info said of the tab's page, and the choice the card opened with (the last one sent). */
let info = null;
let last = rememberedChoice(null);

/** Asks the app what the page holds (a skeleton meanwhile), then shows it, or why it can't and a plain download. */
async function loadMediaCard() {
  const card = $("media-card");
  card.hidden = false;
  $("mc-error").hidden = true;
  say($("mc-msg"), "Looking into this page…");
  const [reply, stored] = await Promise.all([ask({ cmd: "MEDIA_INFO", tabId: tab.id, url: tab.url }), chrome.storage.local.get("mediaLast").catch(() => ({}))]);
  last = rememberedChoice(stored.mediaLast);
  card.classList.remove("loading");
  card.removeAttribute("aria-busy");
  say($("mc-msg"), "");
  if (reply.ok && reply.info && typeof reply.info === "object") return renderMediaCard((info = reply.info));
  $("mc-title").textContent = tab.title || hostOf(tab.url);
  $("mc-channel").textContent = hostOf(tab.url);
  $("mc-error-text").textContent = reply.error || "The downloader couldn't look into this page.";
  $("mc-error").hidden = false;
}

/** A segment of a segmented control: a radio named `name`, its label and its small marks (60, HDR). */
function segment(name, value, text, marks = []) {
  const input = el("input", { type: "radio", name, value });
  return el("label", { className: "seg" }, input, el("span", { textContent: text }), ...marks.map((mark) => el("small", { textContent: mark })));
}

/** Ticks the radio of `name` whose value is `value`. */
function tick(name, value) {
  const radio = [...$("mc-form").elements[name]].find((input) => input.value === value);
  if (radio) radio.checked = true;
}

/** The language a subtitle code names ("English"), else the code itself. */
function languageName(code) {
  try {
    return new Intl.DisplayNames(undefined, { type: "language" }).of(code) || code;
  } catch {
    return code;
  }
}

function renderMediaCard(info) {
  const chip = { live: ["LIVE", "red"], upcoming: ["UPCOMING", "amber"] }[info.live];
  $("mc-live").hidden = !chip;
  if (chip) Object.assign($("mc-live"), { textContent: chip[0], className: `chip tone-${chip[1]}` });
  $("mc-title").textContent = info.title || tab.title || "";
  $("mc-title").title = info.title || "";
  $("mc-channel").textContent = info.uploader || hostOf(tab.url);
  // A thumbnail that fails to load leaves the card's gradient, not a broken-image box.
  if (/^https?:\/\//i.test(info.thumbnail || "")) {
    Object.assign($("mc-thumb"), { src: info.thumbnail, hidden: false, onerror: () => ($("mc-thumb").hidden = true) });
  }
  const length = Number(info.duration) > 0 ? formatDuration(info.duration) : "";
  Object.assign($("mc-length"), { textContent: length, hidden: !length });

  const video = videoChoices(info);
  $("mc-quality").replaceChildren(...video.map((choice) => segment("quality", choice.value, choice.label, choice.marks)));
  $("mc-audio-row").hidden = !info.audio;
  tick("quality", pickQuality(last.quality, [...video.map((choice) => choice.value), ...(info.audio ? ["audio-m4a", "audio-mp3"] : [])]));
  tick("container", last.container);

  const subs = subtitleChoices(info);
  $("mc-subs").replaceChildren(
    el("option", { value: "", textContent: "None" }),
    ...(subs.length > 1 ? [el("option", { value: "all", textContent: "All languages" })] : []),
    ...subs.map(({ code, auto }) => el("option", { value: code, textContent: `${languageName(code)}${auto ? " (auto)" : ""}` })),
  );
  $("mc-subs").value = [...$("mc-subs").options].some((option) => option.value === last.subtitles) ? last.subtitles : "";
  $("mc-end").placeholder = length || "end";

  $("mc-sponsor-row").hidden = !info.sponsorblock;
  $("mc-scope-row").hidden = !info.playlist;
  $("mc-scope-all").textContent = `Whole playlist${Number(info.playlist?.count) > 0 ? ` (${info.playlist.count})` : ""}`;
  // A list or channel page (no one video's length) downloads the list.
  if (info.playlist && !(Number(info.duration) > 0)) tick("scope", "playlist");
  $("mc-live-row").hidden = info.live !== "live";
  $("mc-form").hidden = false;
  updateMediaForm();
}

/** Shows what the choice so far allows: container and subtitles for video, a clip for one video that is not live. */
function updateMediaForm() {
  const form = $("mc-form").elements;
  const audio = form.quality.value.startsWith("audio-");
  $("mc-format-row").hidden = audio;
  $("mc-subs-row").hidden = audio || $("mc-subs").options.length < 2;
  $("mc-clip-row").hidden = info.live === "live" || info.live === "upcoming" || (!!info.playlist && form.scope.value === "playlist");
}

/**
 * Runs in the tab's top frame (isolated world); must not reference anything outside itself. The time the playing
 * video is at, else the largest one's; null with no video.
 */
function pageVideoTime() {
  const videos = [...document.querySelectorAll("video")].filter((v) => Number.isFinite(v.currentTime));
  const area = (v) => v.clientWidth * v.clientHeight;
  const video = videos.find((v) => !v.paused) || videos.sort((a, b) => area(b) - area(a))[0];
  return video ? video.currentTime : null;
}

async function useCurrentTime(input) {
  let time = null;
  try {
    [{ result: time } = {}] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: pageVideoTime });
  } catch {
    // Not readable: said below.
  }
  if (!Number.isFinite(time)) return say($("mc-msg"), "No video on the page to take the time from.", "err");
  input.value = formatDuration(Math.floor(time));
  say($("mc-msg"), "");
}

/** Sends the page with `media` (see lib/media.js mediaPayload); `button` waits meanwhile. */
async function sendMedia(media, button) {
  button.disabled = true;
  say($("mc-msg"), "Sending…");
  const reply = await ask({ cmd: "SEND_MEDIA", tabId: tab.id, url: tab.url, media });
  button.disabled = false;
  report($("mc-msg"), reply, "Sent to the downloader.");
}

function downloadMedia(event) {
  event.preventDefault();
  const form = $("mc-form").elements;
  const choice = {
    quality: form.quality.value,
    container: form.container.value,
    subtitles: $("mc-subs").value,
    start: $("mc-start").value,
    end: $("mc-end").value,
    sponsorblock: form.sponsorblock.value,
    playlist: form.scope.value === "playlist",
    fromStart: form.live.value === "start",
  };
  const { media, error } = mediaPayload(choice, info);
  if (error) return say($("mc-msg"), error, "err");
  // The next card opens with this quality, and the container and subtitles when they were on show.
  last = {
    quality: choice.quality,
    container: $("mc-format-row").hidden ? last.container : choice.container,
    subtitles: $("mc-subs-row").hidden ? last.subtitles : choice.subtitles,
  };
  chrome.storage.local.set({ mediaLast: last }).catch(() => {});
  sendMedia(media, $("mc-download"));
}

/**
 * What the app makes of the tab's page (see PAGE_KIND): a page of a site its video engine takes gets the media card;
 * a file host's, share's or folder's page puts "Download this page" first; any other offers to ask the app's video
 * engine about it, which takes a few seconds, only when clicked.
 */
async function showPageKind() {
  const kind = await pageKind(tab.url);
  // The sites of MEDIA_SITE_HOSTS went by isMediaPage already, which leaves out their home and search pages.
  if (kind === "media" && !mediaSite) {
    mediaPage = true;
    $("page-card").hidden = true;
    return loadMediaCard();
  }
  const files = kind === "files";
  $("page-card").classList.toggle("featured", files || mediaSite);
  $("send-page").classList.toggle("primary", files || mediaSite);
  if (files) {
    $("page-note").textContent = "The downloader finds the files of this page itself: send the page.";
    $("page-note").hidden = false;
  }
  $("find-video").hidden = files || mediaSite;
}

/** The timer of a first click on "every open tab" (it then asks to be clicked again), 0 when none is waiting. */
let allTabsArmed = 0;
const ALL_TABS_LABEL = "Send the videos of every open tab";

function disarmAllTabs() {
  clearTimeout(allTabsArmed);
  allTabsArmed = 0;
  $("all-tabs").querySelector("span").textContent = ALL_TABS_LABEL;
}

/** "Every open tab": the first click counts what would go and asks again, a second within 6 s sends it. */
async function sendAllTabs() {
  const node = $("all-tabs-msg");
  if (!allTabsArmed) {
    const { count = 0 } = await ask({ cmd: "ALL_TABS" });
    if (count === 0) return say(node, "No videos found in the open tabs.");
    $("all-tabs").querySelector("span").textContent = `Send ${count} ${count === 1 ? "video" : "videos"}`;
    allTabsArmed = setTimeout(disarmAllTabs, 6000);
    return say(node, `${count} open ${count === 1 ? "tab has" : "tabs have"} a video to send. Click again to send ${count === 1 ? "it" : "them"}.`);
  }
  disarmAllTabs();
  $("all-tabs").disabled = true;
  say(node, "Sending…");
  const reply = await ask({ cmd: "SEND_ALL_TABS" });
  $("all-tabs").disabled = false;
  report(node, reply, `Sent ${reply.sent} to the downloader.`);
}

/**
 * Runs in every frame of the tab (isolated world); must not reference anything outside itself. The media the page
 * names without having played it: og:video and twitter:player:stream, a JSON-LD VideoObject's or AudioObject's
 * contentUrl, the sources of <video>, <audio> and <source> (lazy data-src ones too), and its <track> subtitles.
 */
function collectPageMedia() {
  const urls = [];
  const add = (value) => {
    try {
      if (typeof value === "string" && value) urls.push(new URL(value, document.baseURI).href);
    } catch {
      // Not a URL: skipped.
    }
  };
  const metas = ["og:video", "og:video:url", "og:video:secure_url", "twitter:player:stream"].map((name) => `meta[property="${name}"], meta[name="${name}"]`);
  for (const meta of document.querySelectorAll(metas.join(", "))) add(meta.content);
  const visit = (node, depth) => {
    if (!node || typeof node !== "object" || depth > 6) return;
    if (Array.isArray(node)) return node.slice(0, 50).forEach((child) => visit(child, depth + 1));
    const types = [].concat(node["@type"] || []);
    if (types.includes("VideoObject") || types.includes("AudioObject")) add(node.contentUrl);
    for (const key of ["@graph", "video", "audio", "associatedMedia", "hasPart", "mainEntity"]) visit(node[key], depth + 1);
  };
  for (const script of document.querySelectorAll('script[type="application/ld+json"]')) {
    try {
      visit(JSON.parse(script.textContent), 0);
    } catch {
      // Not JSON: skipped.
    }
  }
  for (const node of document.querySelectorAll("video, audio, source")) {
    add(node.getAttribute("src"));
    add(node.getAttribute("data-src"));
  }
  const tracks = Array.from(document.querySelectorAll("track[src]"))
    .filter((track) => track.kind === "subtitles" || track.kind === "captions")
    .map((track) => ({ url: track.src, lang: track.srclang, label: track.label }));
  return { urls: [...new Set(urls)].filter((url) => /^https?:/i.test(url)).slice(0, 200), tracks: tracks.slice(0, 20) };
}

/** Hands what collectPageMedia finds in each frame to the background, which lists its videos with the others. */
async function scanPageMedia() {
  const results = await chrome.scripting.executeScript({ target: { tabId: tab.id, allFrames: true }, func: collectPageMedia }).catch(() => []);
  const frames = results
    .filter((r) => r?.result && (r.result.urls?.length || r.result.tracks?.length))
    .map((r) => ({ frameId: r.frameId, urls: r.result.urls, tracks: r.result.tracks }));
  if (frames.length) await ask({ cmd: "PAGE_VIDEOS", tabId: tab.id, frames });
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
  const bits = [];
  if (v.width > 0 && v.height > 0) bits.push(`${v.width}×${v.height}`);
  bits.push(v.live ? "LIVE" : v.duration > 0 ? formatDuration(v.duration) : "not loaded");
  if (v.blob) bits.push("blob");
  if (v.muted) bits.push("muted");
  bits.push(v.paused ? "paused" : "playing");
  if (v.frameId) bits.push("in a frame");
  const details = bits.join(" · ");
  const record = button("Record", "btn small", "record");
  record.setAttribute("aria-label", `Record playback of video ${n + 1}`);
  record.addEventListener("click", async () => {
    record.disabled = true;
    const reply = await ask({ cmd: "MSR_START", tabId: tab.id, frameId: v.frameId, index: v.index });
    record.disabled = false;
    const muted = reply.silent
      ? " Without sound: the browser won't unmute it before the page is clicked. Unmute it on the page to record its sound from then on."
      : v.muted
        ? " It was muted: it plays at near-zero volume so its sound is recorded, and is muted again after."
        : "";
    report($("rec-msg"), reply, `Recording video ${n + 1}. Let it play to the end or press Stop.${muted}`);
    if (reply.ok) refresh();
  });
  const text = el("p", { className: "video-text" }, el("span", { textContent: `Video ${n + 1}` }), el("span", { className: "note", textContent: details, title: details }));
  return el("li", { className: "video" }, text, record);
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
  const now = Date.now();
  recs.forEach((r, i) => {
    let row = recRows.get(r.key);
    if (!row) {
      row = makeRecRow(r.key);
      recRows.set(r.key, row);
    }
    updateRecRow(row, r, now);
    if (list.children[i] !== row.li) list.insertBefore(row.li, list.children[i] || null);
  });
  $("stop").hidden = !armed && !recs.some((r) => r.mode !== "browser");
  $("speed-wrap").hidden = !recs.some((r) => r.mode === "mse");
  setCount("rec-count", recs.length);
  // Byte counts only move while something records (a failed browser download stays only to show why), so poll only then.
  const moving = recs.some((r) => !r.error);
  $("tab-record").classList.toggle("live", moving);
  // Only failed browser downloads left (kept to say why): nothing is in progress any more.
  $("recs-title").textContent = moving || armed ? "In progress" : "Failed";
  $("live-dot").hidden = !moving && !armed;
  if (moving && !ticker) ticker = setInterval(refresh, 1000);
  else if (!moving && ticker) {
    clearInterval(ticker);
    ticker = null;
  }
}

function makeRecRow(key) {
  const row = {};
  row.kind = el("span", { className: "chip" });
  row.label = el("span", { className: "label" });
  row.stop = button("Stop", "btn small danger");
  row.bar = el("div", { className: "bar", hidden: true }, el("i"));
  row.bar.setAttribute("role", "progressbar");
  row.bar.setAttribute("aria-valuemin", "0");
  row.bar.setAttribute("aria-valuemax", "100");
  row.info = el("span", { className: "info" });
  row.stop.addEventListener("click", async () => {
    row.stop.disabled = true;
    // A failed download's button dismisses it.
    const failed = row.failed;
    const reply = await ask({ cmd: "BROWSER_DL_CANCEL", tabId: tab.id, key });
    row.stop.disabled = false;
    report($("rec-msg"), reply, failed ? "" : "Browser download stopped.");
    refresh();
  });
  row.li = el("li", { className: "rec" }, el("div", { className: "rec-top" }, row.kind, row.label, row.stop), row.bar, row.info);
  return row;
}

/** Browser downloads show how far they got; recordings their elapsed time, bytes and speed. */
function updateRecRow(row, r, now) {
  const view = recordingView(r, now);
  const title = `${view.kind} · ${view.title}`;
  row.kind.textContent = view.kind;
  row.kind.className = `chip ${view.browser ? "tone-cyan" : "tone-red"}`;
  row.label.textContent = view.title;
  row.label.title = title;
  row.info.textContent = view.info;
  // A browser download that failed was thrown away; it stays to say why until dismissed.
  row.failed = view.failed;
  row.li.classList.toggle("failed", view.failed);
  row.bar.hidden = view.percent === null || view.failed;
  row.bar.style.setProperty("--p", String((view.percent ?? 0) / 100));
  row.bar.setAttribute("aria-valuenow", String(view.percent ?? 0));
  row.bar.setAttribute("aria-label", `${title}: ${view.percent ?? 0}%`);
  row.stop.hidden = !view.browser;
  row.stop.className = `btn small ${view.failed ? "" : "danger"}`;
  row.stop.label.textContent = view.failed ? "Dismiss" : "Stop";
  row.stop.setAttribute("aria-label", `${view.failed ? "Dismiss" : "Stop"} ${title}`);
}

// ---- Links ----

/** The page's links (see normalizeLinks), the ones ticked, and the chips chosen; null until the page is read. */
let links = null;
/** The page's images as collectPageLinks found them, for "Full-size images". */
let pageImages = [];
const picked = new Set();
const kinds = new Set();

/**
 * Runs in the tab's top frame (isolated world); must not reference anything outside itself. The links and media of
 * the page and its same-origin frames, as the URLs the browser resolved, and srcsets with the base they resolve against;
 * and each image with its sizes, its size in pixels and the link around it, for "Full-size images".
 */
function collectPageLinks() {
  const urls = [];
  const srcsets = [];
  const images = [];
  const rels = new Set(["alternate", "enclosure", "preload", "prefetch", "image_src"]);
  const visit = (doc) => {
    if (!doc) return;
    for (const node of doc.querySelectorAll("a[href], area[href]")) if (typeof node.href === "string") urls.push(node.href);
    for (const node of doc.querySelectorAll("img[src], video[src], audio[src], source[src]")) urls.push(node.src);
    for (const node of doc.querySelectorAll("img[srcset], source[srcset]")) srcsets.push([node.getAttribute("srcset"), doc.baseURI]);
    for (const img of Array.from(doc.images).slice(0, 5000)) {
      const linked = img.closest("a[href]")?.href;
      images.push({ src: img.src, srcset: img.getAttribute("srcset") || "", base: doc.baseURI, width: img.naturalWidth, height: img.naturalHeight, linked: typeof linked === "string" ? linked : "" });
    }
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
  return { urls, srcsets, images };
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
  const { urls = [], srcsets = [], images = [] } = found?.result || {};
  links = normalizeLinks([...urls, ...srcsets.map(([srcset, base]) => largestSrcset(srcset, base))]);
  // Each image at its largest source; the pixel size is the one the page shows, which the largest is at least.
  pageImages = images.map(({ src, srcset, base, width, height, linked }) => ({ url: largestSrcset(srcset, base) || src, width, height, linked }));
  picked.clear();
  say($("links-msg"), "");
  renderLinks();
}

/**
 * "Full-size images": selects the page's images at their largest (see fullSizeImages), no smaller than the size
 * chosen beside it, instead of what was selected; a linked original not yet listed joins the list. The type chips
 * are cleared so every one of them shows (and is sent).
 */
function pickImages() {
  const wanted = normalizeLinks(fullSizeImages(pageImages, Number($("image-min").value) || 0));
  const listed = new Set((links || []).map((link) => link.url));
  links = [...(links || []), ...wanted.filter((link) => !listed.has(link.url))];
  picked.clear();
  for (const link of wanted) picked.add(link.url);
  kinds.clear();
  for (const chip of $("kinds").children) chip.setAttribute("aria-pressed", "false");
  renderLinks();
  say($("links-msg"), wanted.length ? `Selected ${wanted.length} full-size ${wanted.length === 1 ? "image" : "images"}.` : "No images of that size on this page.", wanted.length ? "ok" : "");
}

/** The links the filters let through. */
function shownLinks() {
  const filter = linkFilter({ kinds: [...kinds], query: $("links-query").value, sameSite: $("same-site").checked, pageHost: hostOf(tab.url) });
  return (links || []).filter(filter);
}

function renderLinks() {
  const shown = shownLinks();
  const chosen = shown.filter((link) => picked.has(link.url)).length;
  $("links").replaceChildren(
    ...shown.map((link) => {
      const box = el("input", { type: "checkbox", checked: picked.has(link.url) });
      box.addEventListener("change", () => {
        if (box.checked) picked.add(link.url);
        else picked.delete(link.url);
        renderCount();
      });
      const text = el("span", { className: "link-text" }, el("span", { textContent: linkName(link.url) }), el("span", { className: "link-url", textContent: link.url }));
      const label = el("label", { title: link.url }, box, text);
      return el("li", {}, label, ...(link.kind ? [el("span", { className: "chip", textContent: link.kind })] : []));
    }),
  );
  $("no-links").hidden = links === null || shown.length > 0;
  $("no-links-text").textContent =
    textMatcher($("links-query").value) === null ? "That /regex/ is not valid." : links?.length ? "No links match." : "No links on this page.";
  renderCount(shown, chosen);
}

/** The count and the Send button, for the ticked links the filters show (those are what is sent). */
function renderCount(shown = shownLinks(), chosen = shown.filter((link) => picked.has(link.url)).length) {
  $("selected-count").textContent = `${chosen} of ${shown.length} selected`;
  $("send-links-label").textContent = `Send ${chosen} to app`;
  $("send-links").disabled = chosen === 0;
  setCount("links-count", chosen);
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

// ---- Downloads: the app's ----

/** Download id → its row of the app's list, kept across polls so a focused button is not replaced. */
const queueRows = new Map();
/** Which polling of the app's list runs (see pollQueue); an older one stops at its next turn. */
let queuePoll = 0;

/** Asks the app for its downloads now and each second after while `on` (the Downloads view shows); stops otherwise. */
function pollQueue(on) {
  const poll = ++queuePoll;
  const tick = async () => {
    const reply = await ask({ cmd: "APP_QUEUE" });
    if (poll !== queuePoll) return;
    renderQueue(reply);
    setTimeout(tick, 1000);
  };
  if (on) tick();
}

function renderQueue(reply) {
  const listed = reply.ok && Array.isArray(reply.downloads) ? reply.downloads.filter((d) => d && Number.isSafeInteger(d.id)) : [];
  // Running and waiting ones first, then the stopped and done ones as the app lists them (newest first).
  const rank = { downloading: 0, pausing: 0, queued: 1, auth: 1, paused: 2, failed: 3, done: 4 };
  const shown = listed.sort((a, b) => (rank[a.status] ?? 5) - (rank[b.status] ?? 5)).slice(0, 30);
  const list = $("queue");
  const keep = new Set(shown.map((d) => d.id));
  for (const [id, row] of queueRows) {
    if (!keep.has(id)) {
      row.li.remove();
      queueRows.delete(id);
    }
  }
  shown.forEach((d, i) => {
    let row = queueRows.get(d.id);
    if (!row) queueRows.set(d.id, (row = makeQueueRow(d.id)));
    updateQueueRow(row, d);
    if (list.children[i] !== row.li) list.insertBefore(row.li, list.children[i] || null);
  });
  const note = !reply.ok ? reply.error || "The app didn't answer." : shown.length ? "" : "No downloads in the app.";
  $("queue-note").textContent = note;
  $("queue-note").hidden = !note;
}

const QUEUE_ACTIONS = { pause: "Pause", resume: "Resume", remove: "Remove" };

function makeQueueRow(id) {
  const row = { buttons: {} };
  row.chip = el("span", { className: "chip" });
  row.label = el("span", { className: "label" });
  row.bar = el("div", { className: "bar", hidden: true }, el("i"));
  row.bar.setAttribute("role", "progressbar");
  row.bar.setAttribute("aria-valuemin", "0");
  row.bar.setAttribute("aria-valuemax", "100");
  row.info = el("span", { className: "info" });
  for (const [action, text] of Object.entries(QUEUE_ACTIONS)) {
    const node = button(text, action === "remove" ? "btn small ghost" : "btn small");
    node.addEventListener("click", async () => {
      node.disabled = true;
      const reply = await ask({ cmd: "APP_QUEUE_ACTION", id, action });
      node.disabled = false;
      if (!reply.ok) report($("queue-msg"), reply, "");
    });
    row.buttons[action] = node;
  }
  row.li = el("li", { className: "rec" }, el("div", { className: "rec-top" }, row.chip, row.label, ...Object.values(row.buttons)), row.bar, row.info);
  return row;
}

function updateQueueRow(row, d) {
  const view = queueView(d);
  row.chip.textContent = view.chip[0];
  row.chip.className = view.chip[1] ? `chip tone-${view.chip[1]}` : "chip";
  row.label.textContent = view.title;
  row.label.title = view.title;
  row.info.textContent = view.info;
  row.li.classList.toggle("failed", d.status === "failed");
  row.bar.hidden = view.percent === null;
  row.bar.style.setProperty("--p", String((view.percent ?? 0) / 100));
  row.bar.setAttribute("aria-valuenow", String(view.percent ?? 0));
  row.bar.setAttribute("aria-label", `${view.title}: ${view.percent ?? 0}%`);
  for (const [action, node] of Object.entries(row.buttons)) {
    node.hidden = !view.actions.includes(action);
    const text = action === "resume" && d.status === "failed" ? "Retry" : QUEUE_ACTIONS[action];
    node.label.textContent = text;
    node.setAttribute("aria-label", `${text} ${view.title}`);
  }
}

// ---- Downloads: the browser's ----

/** Download id → its row of the log, kept across refreshes so a focused button is not replaced. */
const logRows = new Map();
/** What the app makes of the pages asked about (see PAGE_KIND): link → "media", "files" or "none", once known. */
const knownKinds = new Map();
const askedKinds = new Map();

/** What the app makes of the page at `url`, asked once per popup; known in knownKinds once it answers. */
function pageKind(url) {
  if (!askedKinds.has(url)) {
    askedKinds.set(url, ask({ cmd: "PAGE_KIND", url }).then((reply) => knownKinds.set(url, reply.kind || "none").get(url)));
  }
  return askedKinds.get(url);
}

function renderLog(log) {
  const list = $("log");
  const now = Date.now();
  const keep = new Set(log.map((entry) => entry.id));
  for (const [id, row] of logRows) {
    if (!keep.has(id)) {
      row.li.remove();
      logRows.delete(id);
    }
  }
  log.forEach((entry, i) => {
    let row = logRows.get(entry.id);
    if (!row) logRows.set(entry.id, (row = makeLogRow(entry.id)));
    updateLogRow(row, entry, now);
    if (list.children[i] !== row.li) list.insertBefore(row.li, list.children[i] || null);
  });
  $("log-empty").hidden = log.length > 0;
  renderSiteCapture();
}

function makeLogRow(id) {
  const row = {};
  row.chip = el("span", { className: "chip" });
  row.label = el("span", { className: "label" });
  row.action = button("", "btn small", "send");
  row.info = el("span", { className: "info" });
  row.reason = el("span", { className: "reason" });
  row.action.addEventListener("click", async () => {
    const page = row.mode === "page";
    row.action.disabled = true;
    say($("log-msg"), "Sending…");
    const reply = page
      ? await ask({ cmd: "SEND_URL", tabId: tab?.id ?? -1, url: row.entry.page, referer: row.entry.page })
      : await ask({ cmd: "SEND_LOGGED", id });
    row.action.disabled = false;
    report($("log-msg"), reply, page ? "Page sent to the downloader." : "Sent to the downloader.");
    refresh();
  });
  row.li = el("li", { className: "rec" }, el("div", { className: "rec-top" }, row.chip, row.label, row.action), row.info, row.reason);
  return row;
}

function updateLogRow(row, entry, now) {
  const view = logView(entry, now);
  row.entry = entry;
  row.mode = view.action;
  row.chip.textContent = view.chip[0];
  row.chip.className = view.chip[1] ? `chip tone-${view.chip[1]}` : "chip";
  row.label.textContent = view.title;
  row.label.title = view.title;
  row.info.textContent = view.info;
  row.reason.textContent = view.reason;
  row.action.label.textContent = view.action === "page" ? "Send page" : "Send to app";
  row.action.setAttribute("aria-label", `${row.action.label.textContent}: ${view.title}`);
  // The page a download the page made came from is offered once the app says it can take something from it.
  const kind = view.action === "page" ? knownKinds.get(entry.page) : null;
  if (kind === undefined) pageKind(entry.page).then(() => renderLog(state.log || []));
  row.action.hidden = !view.action || (view.action === "page" && kind !== "media" && kind !== "files");
}

/** "Always send" and "Never" for the tab's site (see setSiteCapture), pressed as the settings say. */
function renderSiteCapture() {
  const s = state.settings;
  const site = httpTab ? siteOf(hostOf(tab.url)) : "";
  $("site-capture").hidden = !site || !s;
  if (!site || !s) return;
  $("site-name").textContent = site;
  $("site-always").setAttribute("aria-pressed", String(s.captureAlwaysSites.includes(site)));
  $("site-never").setAttribute("aria-pressed", String(s.captureExcludedSites.includes(site)));
}

/** Puts the tab's site on `list` ("captureAlwaysSites" or "captureExcludedSites") and off the other, or takes it off again. */
function setSiteCapture(list) {
  const s = state.settings;
  const site = siteOf(hostOf(tab.url));
  const other = list === "captureAlwaysSites" ? "captureExcludedSites" : "captureAlwaysSites";
  const on = s[list].includes(site);
  const patch = { [list]: on ? s[list].filter((host) => host !== site) : [...s[list], site], [other]: s[other].filter((host) => host !== site) };
  const done = on ? `${site} follows the usual rules again.` : list === "captureAlwaysSites" ? `Downloads from ${site} go to the app.` : `Downloads from ${site} stay in the browser.`;
  saveSettings(patch, s.captureEnabled ? done : `${done} Turn on "Send browser downloads to the app" in Settings for it to apply.`, $("log-msg"));
}

// ---- Settings ----

function renderSettings(s) {
  for (const [id, key] of [["min-size", "minSizeKB"], ["max-size", "maxSizeKB"]]) {
    const input = $(id);
    if (document.activeElement !== input) input.value = String(s[key] ?? 0);
  }
  $("convert-mp4").checked = s.convertToMp4 !== false;
  $("media-cookies").checked = s.mediaCookies === true;
  $("youtube-button").checked = s.youtubeButton !== false;
  $("video-button").checked = s.videoButton !== false;
  $("youtube-button").closest(".setting").hidden = NO_YOUTUBE;
  $("capture").checked = s.captureEnabled === true;
  $("capture-all").checked = s.captureAll === true;
  $("capture-start").checked = s.captureStartsApp !== false;
  const typed = {
    "capture-types": s.captureTypes?.join(" "),
    "capture-min": s.captureMinMB,
    "capture-over": s.captureAnyOverMB,
    "capture-always": s.captureAlwaysSites?.join(" "),
    "capture-sites": s.captureExcludedSites?.join(" "),
  };
  for (const [id, value] of Object.entries(typed)) {
    if (document.activeElement !== $(id)) $(id).value = String(value ?? "");
  }
  const hosts = Array.isArray(s.blockedHosts) ? s.blockedHosts : [];
  const key = hosts.join("\n");
  if (key === blockedKey) return;
  blockedKey = key;
  $("blocked").replaceChildren(
    ...hosts.map((host) => {
      const remove = iconButton("x", `Unblock ${host}`);
      remove.addEventListener("click", () =>
        saveSettings({ blockedHosts: (state.settings.blockedHosts || []).filter((h) => h !== host) }, `Unblocked ${host}.`),
      );
      return el("li", { className: "host" }, el("span", { title: host, textContent: host }), remove);
    }),
  );
  $("no-blocked").hidden = hosts.length > 0;
}

/**
 * Saves run one after another, each on top of the last: typing a size and then clicking the MP4 switch fires both
 * at once, and two saves built from the same settings would undo each other.
 */
let saving = Promise.resolve();
function saveSettings(patch, success, node = $("settings-msg")) {
  saving = saving.then(async () => {
    if (!state.settings) return say(node, "Settings haven't loaded; reopen the popup.", "err");
    const settings = { ...state.settings, ...patch };
    const reply = await ask({ cmd: "SET_SETTINGS", settings });
    if (reply.ok) state.settings = settings;
    report(node, reply, success);
    renderSettings(state.settings);
    renderSiteCapture();
  });
  return saving;
}

/** Export: the settings as a JSON file in the browser's downloads (one the extension starts stays in the browser). */
async function exportSettings() {
  if (!state.settings) return say($("backup-msg"), "Settings haven't loaded; reopen the popup.", "err");
  const blob = new Blob([JSON.stringify(state.settings, null, 2) + "\n"], { type: "application/json" });
  try {
    await chrome.downloads.download({ url: URL.createObjectURL(blob), filename: "endo-extension-settings.json", saveAs: false });
    say($("backup-msg"), "Saved as endo-extension-settings.json in your downloads.", "ok");
  } catch (e) {
    say($("backup-msg"), `Couldn't save the file: ${e?.message || e}`, "err");
  }
}

/** Import…: picks a settings file. Firefox closes the popup when a file picker opens, so there it opens in a tab. */
function chooseSettingsFile() {
  if (chrome.runtime.getURL("").startsWith("moz-extension:") && !new URLSearchParams(location.search).has("tab")) {
    chrome.tabs.create({ url: chrome.runtime.getURL(`popup.html?tab=${tab?.id ?? 0}&view=settings`) });
    return window.close();
  }
  $("import-file").click();
}

/** The picked settings file replaces the settings it names; the background checks each value. */
async function importSettings(event) {
  const file = event.target.files?.[0];
  event.target.value = "";
  if (!file) return;
  if (file.size > 1 << 20) return say($("backup-msg"), "That file is too big to be a settings file.", "err");
  const reply = await ask({ cmd: "IMPORT_SETTINGS", text: await file.text() });
  report($("backup-msg"), reply, "Settings imported.");
  if (reply.ok) refresh();
}

/** A whole-number setting typed in `id` (`what` names its unit for the error), saved under `key`. */
function wholeInput(id, key, what) {
  $(id).addEventListener("change", (event) => {
    const n = Number(event.target.value);
    if (event.target.value.trim() !== "" && Number.isInteger(n) && n >= 0) return saveSettings({ [key]: n }, "Saved.");
    say($("settings-msg"), `Enter a whole number of ${what}.`, "err");
    event.target.value = String(state.settings?.[key] ?? 0);
  });
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

/**
 * "Start the app": the background starts it and sends the last request that failed for want of it again. The popup
 * may close meanwhile (the browser asks before opening the app); the tab's badge then tells how it went.
 */
async function startApp() {
  const retry = lastFailed;
  lastFailed = null;
  $("start-app").disabled = true;
  say($("start-msg"), "Starting the app… Allow it if the browser asks.");
  const reply = await ask({ cmd: "START_APP", tabId: tab?.id ?? -1, retry });
  $("start-app").disabled = false;
  report($("start-msg"), reply, retry ? "The app started and has the download." : "The app started.");
  if (reply.ok && mediaPage && !$("mc-error").hidden) loadMediaCard();
  refresh();
}

// ---- Wiring ----

async function init() {
  // Test hook: popup.html?tab=<id> acts on that tab instead of the active one.
  const forced = Number(new URLSearchParams(location.search).get("tab"));
  [tab] = forced > 0 ? [await chrome.tabs.get(forced).catch(() => null)] : await chrome.tabs.query({ active: true, currentWindow: true });
  // The Chrome Web Store build leaves YouTube alone (lib/detect.js NO_YOUTUBE): there it is like a browser page.
  const offLimits = isOffLimits(hostOf(tab?.url || ""));
  httpTab = /^https?:\/\//i.test(tab?.url || "") && !offLimits;
  $("unsupported").hidden = httpTab;
  if (offLimits) $("unsupported").querySelector("p").textContent = "This version of the extension doesn't work on YouTube.";
  // Off a web page only Settings has anything to show.
  for (const view of ["media", "links", "record"]) $(`tab-${view}`).disabled = !httpTab;
  $("about-ext").textContent = `v${chrome.runtime.getManifest().version}`;

  if (httpTab) {
    mediaSite = isMediaSiteHost(hostOf(tab.url));
    $("page-card").classList.toggle("featured", mediaSite);
    $("send-page").classList.toggle("primary", mediaSite);
    $("page-note").hidden = !mediaSite;
    // What loaded before the extension was installed or updated was never seen; a reload replays it. Media sites
    // are handed to the downloader whole, so neither that nor recording is suggested there.
    for (const id of ["reload-hint", "reload", "go-record"]) $(id).hidden = mediaSite;
    if (mediaSite) {
      $("empty-title").textContent = "Streams aren't collected on this site";
      $("empty-text").textContent = "Use Download this page above: the downloader takes the page itself.";
    }
    mediaPage = isMediaPage(tab.url);
    $("page-card").hidden = mediaPage;
    if (mediaPage) loadMediaCard();
    else showPageKind();
    if (!mediaSite && !mediaPage) scanPageMedia();
    scanVideos();
  }

  viewKey = `endo.view.${tab?.windowId ?? 0}`;
  let saved = null;
  try {
    saved = localStorage.getItem(viewKey);
  } catch {
    // No storage: the popup opens on Media.
  }
  for (const view of VIEWS) $(`tab-${view}`).addEventListener("click", () => showView(view, { remember: true }));
  $("tabs").addEventListener("keydown", onTabKey);
  $("go-record").addEventListener("click", () => showView("record", { focus: true, remember: true }));

  $("send-page").addEventListener("click", async () => {
    const button = $("send-page");
    button.disabled = true;
    say($("page-msg"), "Sending…");
    const reply = await ask({ cmd: "SEND_URL", tabId: tab.id, url: tab.url, referer: tab.url });
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
  $("pick-images").addEventListener("click", pickImages);
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
  $("media-cookies").addEventListener("change", (event) => saveSettings({ mediaCookies: event.target.checked }, "Saved."));
  $("youtube-button").addEventListener("change", (event) => saveSettings({ youtubeButton: event.target.checked }, "Saved."));
  $("video-button").addEventListener("change", (event) => saveSettings({ videoButton: event.target.checked }, "Saved."));
  $("all-tabs").addEventListener("click", sendAllTabs);
  $("export-settings").addEventListener("click", exportSettings);
  $("import-settings").addEventListener("click", chooseSettingsFile);
  $("import-file").addEventListener("change", importSettings);
  $("capture").addEventListener("change", (event) => saveSettings({ captureEnabled: event.target.checked }, "Saved."));
  $("capture-all").addEventListener("change", (event) => saveSettings({ captureAll: event.target.checked }, "Saved."));
  $("capture-start").addEventListener("change", (event) => saveSettings({ captureStartsApp: event.target.checked }, "Saved."));
  $("capture-types").addEventListener("change", (event) => saveSettings({ captureTypes: parseTypes(event.target.value) }, "Saved."));
  $("capture-always").addEventListener("change", (event) => saveSettings({ captureAlwaysSites: parseSites(event.target.value) }, "Saved."));
  $("capture-sites").addEventListener("change", (event) => saveSettings({ captureExcludedSites: parseSites(event.target.value) }, "Saved."));
  wholeInput("capture-min", "captureMinMB", "MB (0 = any size)");
  wholeInput("capture-over", "captureAnyOverMB", "MB (0 = off)");
  $("site-always").addEventListener("click", () => setSiteCapture("captureAlwaysSites"));
  $("site-never").addEventListener("click", () => setSiteCapture("captureExcludedSites"));
  $("start-app").addEventListener("click", startApp);
  $("mc-form").addEventListener("change", updateMediaForm);
  $("mc-form").addEventListener("submit", downloadMedia);
  $("mc-start-now").addEventListener("click", () => useCurrentTime($("mc-start")));
  $("mc-end-now").addEventListener("click", () => useCurrentTime($("mc-end")));
  $("mc-anyway").addEventListener("click", () => sendMedia({ quality: "best" }, $("mc-anyway")));
  $("find-video").addEventListener("click", () => {
    $("find-video").hidden = true;
    $("media-card").classList.add("loading");
    loadMediaCard();
  });

  // popup.html?view=<name> opens on that view (Firefox's Import opens Settings in a tab).
  const asked = new URLSearchParams(location.search).get("view");
  showView(VIEWS.includes(asked) && !$(`tab-${asked}`).disabled ? asked : httpTab ? (VIEWS.includes(saved) ? saved : "media") : "settings");
  chrome.runtime.onMessage.addListener((msg) => {
    if ((msg?.cmd === "STATE_CHANGED" && msg.tabId === tab?.id) || msg?.cmd === "LOG_CHANGED") refresh();
  });
  await refresh();
}

init();
