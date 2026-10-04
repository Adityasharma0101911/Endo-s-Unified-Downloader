// Service worker: finds media requests in every tab, keeps them per tab in session storage, and is the hub between
// the popup, the content-script bridge and the desktop app's local HTTP API. Chrome stops the worker after ~30 s
// idle, so everything that must outlive it is in chrome.storage.session; the in-memory maps are only caches and
// queues that may start empty. Listeners are registered at the top level so a restarted worker gets the event that
// woke it.

import {
  classifyResponse,
  isAdHost,
  isMediaSiteHost,
  parseM3u8,
  passesSizeFilter,
  sanitizeFilename,
  splitRequestHeaders,
} from "./lib/detect.js";

const APP_NAME = "endos-unified-downloader";
const APP_PORTS = [49152, 49153, 49154, 49155];
const NOT_RUNNING = "Endo's Unified Downloader is not running.";
const MAX_ITEMS_PER_TAB = 100;
const MAX_PLAYLIST_CHARS = 2 * 1024 * 1024;
// A 64 MiB chunk is about 87.4 M base64 characters.
const MAX_CHUNK_BASE64 = Math.ceil((64 * 1024 * 1024) / 3) * 4;
const HEADER_CACHE_SIZE = 300;
const REQUEST_FILTER = { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "object", "other"] };
const DEFAULT_SETTINGS = { minSizeKB: 500, maxSizeKB: 0, blockedHosts: [], convertToMp4: true };
const BADGE_COLOR = "#ef4444";
const MENU_TITLE = "Download with Endo's Unified Downloader";
const PLAYBACK_RATES = [1, 2, 4, 8, 16];
// How long after REC_ARM a frame may say its buffer capture is live: the hook's 60 s, plus the time messages take.
const ARM_WINDOW_MS = 70000;
/** sessionStorage key that arms the hook for the reload (set by the bridge, or from here when there is none yet). */
const ARM_KEY = "__endo_rec_armed";

chrome.action.setBadgeBackgroundColor({ color: BADGE_COLOR }).catch(() => {});
chrome.action.setBadgeTextColor({ color: "#ffffff" }).catch(() => {});

function isHttpUrl(value) {
  if (typeof value !== "string" || !/^https?:\/\//i.test(value)) return false;
  try {
    return Boolean(new URL(value));
  } catch {
    return false;
  }
}

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

/** True when `host` is a blocked host or a subdomain of one. */
function isBlocked(host, blockedHosts) {
  return blockedHosts.some((blocked) => host === blocked || host.endsWith("." + blocked));
}

/** A lower-cased host name, or "" when `value` is not one. */
function cleanHost(value) {
  const host = typeof value === "string" ? value.trim().toLowerCase().replace(/\.$/, "") : "";
  return /^[a-z0-9-]+(\.[a-z0-9-]+)*$/.test(host) && host.length <= 253 ? host : "";
}

/** Tells an open popup that a tab's items or recordings changed; with no popup open there is nobody to tell. */
function notifyPopup(tabId) {
  chrome.runtime.sendMessage({ cmd: "STATE_CHANGED", tabId }).catch(() => {});
}

/** Sends to a tab's content scripts (all frames unless `options.frameId`); a frame without our script is fine. */
function sendToFrames(tabId, message, options) {
  return chrome.tabs.sendMessage(tabId, message, options).catch(() => undefined);
}

/**
 * Runs `task` after every earlier task queued under the same key, so read-modify-write of one tab's items, or the
 * chunks of one recording, never interleave.
 */
function serialize(queues, key, task) {
  const run = (queues.get(key) || Promise.resolve()).then(task);
  const settled = run.catch((error) => console.warn("Endo:", error));
  queues.set(key, settled);
  settled.then(() => {
    if (queues.get(key) === settled) queues.delete(key);
  });
  return run;
}

// ---------------------------------------------------------------------------------------------------------------
// Settings

let settingsPromise = null;

function normalizeSettings(raw) {
  const settings = raw && typeof raw === "object" ? raw : {};
  const kb = (value, fallback) => {
    const number = Number(value);
    return value !== null && value !== "" && Number.isFinite(number) && number >= 0 ? Math.floor(number) : fallback;
  };
  const hosts = Array.isArray(settings.blockedHosts) ? settings.blockedHosts.map(cleanHost).filter(Boolean) : [];
  return {
    minSizeKB: kb(settings.minSizeKB, DEFAULT_SETTINGS.minSizeKB),
    maxSizeKB: kb(settings.maxSizeKB, DEFAULT_SETTINGS.maxSizeKB),
    blockedHosts: [...new Set(hosts)].slice(0, 500),
    convertToMp4: typeof settings.convertToMp4 === "boolean" ? settings.convertToMp4 : DEFAULT_SETTINGS.convertToMp4,
  };
}

function getSettings() {
  return (settingsPromise ??= chrome.storage.local.get("settings").then((stored) => normalizeSettings(stored.settings)));
}

async function saveSettings(settings) {
  const clean = normalizeSettings(settings);
  await chrome.storage.local.set({ settings: clean });
  settingsPromise = Promise.resolve(clean);
  return clean;
}

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === "local" && changes.settings) settingsPromise = null;
});

// ---------------------------------------------------------------------------------------------------------------
// Request headers seen by the browser

/** Headers of requests still in flight, by requestId. */
const headersByRequest = new Map();
/** Headers of the last HEADER_CACHE_SIZE requests by URL, for playlists the page reports itself. */
const headersByUrl = new Map();

chrome.webRequest.onSendHeaders.addListener(
  (details) => {
    // Our own calls to the app come from the extension origin and are not the page's.
    if (details.initiator && !/^https?:/i.test(details.initiator)) return;
    // A CORS preflight shares the request's URL but not its headers: it must not replace them.
    if (details.method === "OPTIONS") return;
    const headers = details.requestHeaders || [];
    headersByRequest.set(details.requestId, headers);
    headersByUrl.delete(details.url);
    headersByUrl.set(details.url, { tabId: details.tabId, frameId: details.frameId, headers, time: Date.now() });
    if (headersByUrl.size > HEADER_CACHE_SIZE) headersByUrl.delete(headersByUrl.keys().next().value);
  },
  REQUEST_FILTER,
  ["requestHeaders", "extraHeaders"],
);

const forgetRequest = (details) => headersByRequest.delete(details.requestId);
chrome.webRequest.onCompleted.addListener(forgetRequest, REQUEST_FILTER);
chrome.webRequest.onErrorOccurred.addListener(forgetRequest, REQUEST_FILTER);

// ---------------------------------------------------------------------------------------------------------------
// Per-tab items in chrome.storage.session

const tabQueues = new Map();
const tabKey = (tabId) => `tab:${tabId}`;

async function readTab(tabId) {
  const key = tabKey(tabId);
  const state = (await chrome.storage.session.get(key))[key];
  return state && typeof state.items === "object" ? state : { items: {} };
}

function showCount(tabId, items) {
  const count = Object.values(items).filter((item) => !item.hidden).length;
  return chrome.action.setBadgeText({ tabId, text: count ? String(count) : "" }).catch(() => {});
}

/**
 * Changes a tab's stored state under its queue. When `mutate` returns true the state is saved, the badge recounted
 * and the popup told.
 */
function updateTab(tabId, mutate) {
  return serialize(tabQueues, tabId, async () => {
    const state = await readTab(tabId);
    if (!mutate(state)) return false;
    await chrome.storage.session.set({ [tabKey(tabId)]: state });
    await showCount(tabId, state.items);
    notifyPopup(tabId);
    return true;
  });
}

function clearTab(tabId) {
  return updateTab(tabId, (state) => {
    state.items = {};
    state.armed = false;
    return true;
  });
}

/**
 * Hides media playlists that are a variant or audio rendition of a master in the same tab and gives the master their
 * duration, and whether they are live, encrypted or DRM-protected: the master is what the popup shows.
 */
function linkPlaylists(items) {
  const masterOf = new Map();
  for (const item of Object.values(items)) {
    if (item.hls?.kind !== "master") continue;
    for (const rendition of [...item.hls.variants, ...item.hls.audio]) masterOf.set(rendition.url, item);
  }
  for (const item of Object.values(items)) {
    const master = masterOf.get(item.url);
    if (!master || master === item) continue;
    item.hidden = true;
    if (item.hls?.kind !== "media") continue;
    if (!master.hls.duration) master.hls.duration = item.hls.duration;
    for (const flag of ["live", "encrypted", "drm"]) master.hls[flag] ||= item.hls[flag];
  }
}

/**
 * `url` without the _HLS_msn/_HLS_part/_HLS_skip values a low-latency HLS player puts on every reload of a playlist,
 * so the reloads are one item. The rest of the query is kept exactly as it was (it may be signed).
 */
function withoutLowLatency(url) {
  const at = url.indexOf("?");
  if (at < 0) return url;
  const kept = url.slice(at + 1).split("&").filter((pair) => !/^_HLS_[a-z]+=/i.test(pair));
  return url.slice(0, at) + (kept.length ? "?" + kept.join("&") : "");
}

/** Stores an item, merging it into the tab's item for the same URL if there is one. */
function addItem(tabId, item) {
  if (item.kind === "hls") item.url = withoutLowLatency(item.url);
  return updateTab(tabId, (state) => {
    const existing = Object.values(state.items).find((stored) => stored.url === item.url);
    if (existing) {
      existing.size = Math.max(existing.size || 0, item.size || 0);
      if (item.hls) existing.hls = item.hls;
      for (const field of ["referer", "userAgent", "cookies"]) existing.request[field] ||= item.request[field];
      existing.request.headers = { ...item.request.headers, ...existing.request.headers };
    } else if (Object.keys(state.items).length < MAX_ITEMS_PER_TAB) {
      state.items[item.id] = item;
    } else {
      return false;
    }
    linkPlaylists(state.items);
    return true;
  });
}

/** The tab a request belongs to. Requests with no tab (a page's service worker) go to the focused tab of their site. */
async function tabFor(tabId, initiator) {
  if (tabId >= 0) return chrome.tabs.get(tabId).catch(() => null);
  const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  return tab?.url?.startsWith(initiator) ? tab : null;
}

chrome.webRequest.onResponseStarted.addListener(
  (details) => {
    // Read now: the request may complete, and its entry go, before the handler's first await returns.
    const requestHeaders = headersByRequest.get(details.requestId) || headersByUrl.get(details.url)?.headers || [];
    onMediaResponse(details, requestHeaders).catch((error) => console.warn("Endo:", error));
  },
  REQUEST_FILTER,
  ["responseHeaders"],
);

async function onMediaResponse(details, requestHeaders) {
  if (!isHttpUrl(details.url) || details.statusCode < 200 || details.statusCode > 299) return;
  if (!details.initiator || !/^https?:\/\//i.test(details.initiator)) return;
  const header = (name) => details.responseHeaders?.find((h) => h.name.toLowerCase() === name)?.value;
  const found = classifyResponse({
    url: details.url,
    type: details.type,
    contentType: header("content-type"),
    contentDisposition: header("content-disposition"),
    contentLength: header("content-length"),
    contentRange: header("content-range"),
  });
  if (!found) return;
  const host = hostOf(details.url);
  const settings = await getSettings();
  if (isAdHost(host) || isBlocked(host, settings.blockedHosts) || !passesSizeFilter(found, settings)) return;
  const tab = await tabFor(details.tabId, details.initiator);
  if (!tab || isMediaSiteHost(hostOf(tab.url))) return;
  await addItem(tab.id, {
    id: details.requestId,
    url: details.url,
    kind: found.kind,
    format: found.format,
    name: found.name,
    size: found.size,
    contentType: header("content-type") || "",
    tabId: tab.id,
    frameId: details.frameId,
    pageUrl: tab.url || "",
    title: tab.title || "",
    time: Date.now(),
    request: splitRequestHeaders(requestHeaders),
    hls: null,
    hidden: false,
  });
}

chrome.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status !== "loading" || !info.url) return;
  clearTab(tabId);
  finishTabRecordings(tabId);
});

chrome.tabs.onRemoved.addListener((tabId) => {
  serialize(tabQueues, tabId, () => chrome.storage.session.remove(tabKey(tabId)));
  finishTabRecordings(tabId);
});

// ---------------------------------------------------------------------------------------------------------------
// The desktop app

async function ping(port) {
  try {
    const response = await fetch(`http://127.0.0.1:${port}/ping`, { signal: AbortSignal.timeout(800) });
    const body = await response.json();
    return response.ok && body?.app === APP_NAME ? { ok: true, port, version: String(body.version ?? "") } : null;
  } catch {
    return null;
  }
}

/** Finds the running app: the port that answered last, else the first of APP_PORTS that answers. */
async function findApp() {
  const { appPort } = await chrome.storage.session.get("appPort");
  const cached = APP_PORTS.includes(appPort) ? await ping(appPort) : null;
  if (cached) return cached;
  const found = (await Promise.all(APP_PORTS.map(ping))).find(Boolean);
  if (!found) {
    await chrome.storage.session.remove("appPort");
    return { ok: false, port: null, version: null };
  }
  await chrome.storage.session.set({ appPort: found.port });
  return found;
}

/**
 * POSTs to the app and returns its JSON reply, throwing a readable error on failure. `verify` pings first (used
 * before sending cookies); otherwise the cached port is trusted, which keeps recording chunks to one request each.
 */
async function callApp(path, { json, body, timeout = 10000, verify = false } = {}) {
  let port = verify ? null : (await chrome.storage.session.get("appPort")).appPort;
  if (!APP_PORTS.includes(port)) port = (await findApp()).port;
  if (!port) throw new Error(NOT_RUNNING);
  const init = { method: "POST", signal: AbortSignal.timeout(timeout) };
  if (json !== undefined) {
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(json);
  } else if (body !== undefined) {
    init.headers = { "Content-Type": "application/octet-stream" };
    init.body = body;
  }
  let response;
  try {
    response = await fetch(`http://127.0.0.1:${port}${path}`, init);
  } catch (error) {
    await chrome.storage.session.remove("appPort");
    throw new Error(error?.name === "TimeoutError" ? "The downloader did not answer in time." : NOT_RUNNING);
  }
  const reply = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(typeof reply.error === "string" ? reply.error : `The downloader answered HTTP ${response.status}.`);
  return reply;
}

/** Queues a download in the app via POST /add; empty fields are left out. */
async function sendToApp({ url, cookies, userAgent, referer, headers, filename, hls, mp4 }) {
  if (!isHttpUrl(url)) return { ok: false, error: "Only http(s) links can be downloaded." };
  const body = { url };
  // The app only takes a link for a playlist by its URL; one served from /api/… must be named one.
  if (hls) body.hls = true;
  // Whether the app remuxes the saved stream into an MP4; only a playlist has anything to convert.
  if (hls && typeof mp4 === "boolean") body.mp4 = mp4;
  if (cookies) body.cookies = cookies;
  if (userAgent) body.user_agent = userAgent;
  if (isHttpUrl(referer)) body.referer = referer;
  if (headers && Object.keys(headers).length) body.headers = headers;
  if (typeof filename === "string" && filename.trim()) body.filename = sanitizeFilename(filename);
  try {
    await callApp("/add", { json: body, verify: true });
    return { ok: true };
  } catch (error) {
    return { ok: false, error: error.message };
  }
}

// ---------------------------------------------------------------------------------------------------------------
// Context menus

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: "endo-link", title: MENU_TITLE, contexts: ["link"] });
    chrome.contextMenus.create({ id: "endo-media", title: MENU_TITLE, contexts: ["video", "audio"] });
    chrome.contextMenus.create({ id: "endo-page", title: MENU_TITLE, contexts: ["page"] });
  });
});

/** Shows "✓" or "!" on the tab's badge for 2 s, then the item count again. */
async function flashBadge(tabId, ok) {
  if (!(tabId >= 0)) return;
  await chrome.action.setBadgeBackgroundColor({ tabId, color: ok ? "#22c55e" : BADGE_COLOR }).catch(() => {});
  await chrome.action.setBadgeText({ tabId, text: ok ? "✓" : "!" }).catch(() => {});
  setTimeout(async () => {
    await chrome.action.setBadgeBackgroundColor({ tabId, color: BADGE_COLOR }).catch(() => {});
    await showCount(tabId, (await readTab(tabId)).items);
  }, 2000);
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const url = { "endo-link": info.linkUrl, "endo-media": info.srcUrl, "endo-page": info.pageUrl }[info.menuItemId];
  if (url === undefined) return;
  // A media element's request headers (cookies, Origin) are usually still in the cache by URL.
  const seen = headersByUrl.get(url);
  const request = seen ? splitRequestHeaders(seen.headers) : {};
  const result = await sendToApp({ ...request, url, referer: info.pageUrl || tab?.url });
  flashBadge(tab?.id, result.ok);
});

// ---------------------------------------------------------------------------------------------------------------
// Recording relay: chunks from the bridge go to the app in arrival order, one queue per recording key.

const recQueues = new Map();
let recsPromise = null;

function loadRecs() {
  return (recsPromise ??= chrome.storage.session.get("recs").then((stored) => stored.recs || {}));
}

async function saveRecs() {
  await chrome.storage.session.set({ recs: await loadRecs() });
}

/** Removes a recording and tells the app to join what it has. Runs inside the recording's queue. */
async function closeRecording(key, action = "finish") {
  const recs = await loadRecs();
  const rec = recs[key];
  if (!rec) return;
  delete recs[key];
  await saveRecs();
  if (rec.appId) await callApp(`/record/${rec.appId}/${action}`).catch((error) => console.warn("Endo:", error));
  notifyPopup(rec.tabId);
}

function finishRecording(key) {
  return serialize(recQueues, key, () => closeRecording(key));
}

async function finishTabRecordings(tabId, keep = () => false) {
  const recs = await loadRecs();
  const keys = Object.keys(recs).filter((key) => recs[key].tabId === tabId && !keep(recs[key]));
  await Promise.all(keys.map(finishRecording));
}

const isRecordingIndex = (ms) => ms === "msr" || (Number.isInteger(ms) && ms >= 0);

/**
 * Whether a new recording may start: a buffer capture in a document whose REC_ACTIVE came while the tab was armed,
 * or the one "Record playback" MSR_START granted to the frame (used up here).
 */
async function mayRecord(tabId, frameId, ms, documentId) {
  // Read in the tab's queue, after the REC_ACTIVE that came before this chunk is saved.
  if (ms !== "msr") return (await serialize(tabQueues, tabId, () => readTab(tabId))).capture?.[frameId] === documentId;
  return updateTab(tabId, (state) => {
    if (!state.msr?.[frameId]) return false;
    delete state.msr[frameId];
    return true;
  });
}

function onRecChunk({ ms, track, mime, data }, sender) {
  if (
    !isRecordingIndex(ms) ||
    !Number.isInteger(track) || track < 0 || track > 15 ||
    typeof mime !== "string" || mime.length > 255 ||
    typeof data !== "string" || data.length > MAX_CHUNK_BASE64
  ) {
    return { ok: false, error: "bad chunk" };
  }
  const tab = sender.tab;
  const frameId = sender.frameId ?? 0;
  const key = `${tab.id}:${frameId}:${ms}`;
  // Queued synchronously, so chunks keep the order they arrived in.
  return serialize(recQueues, key, async () => {
    const recs = await loadRecs();
    // A reload starts index 0 again under the same key: the earlier document's recording is complete.
    if (recs[key] && recs[key].documentId !== sender.documentId) await closeRecording(key);
    let rec = recs[key];
    if (!rec) {
      // The page can fake the hook's messages, so only a recording the user asked for in this tab reaches the app.
      if (!(await mayRecord(tab.id, frameId, ms, sender.documentId))) return { ok: false, error: "not armed" };
      rec = recs[key] = {
        appId: null,
        tabId: tab.id,
        frameId,
        title: tab.title || "Recording",
        mode: ms === "msr" ? "msr" : "mse",
        bytes: 0,
        tracks: 0,
        started: Date.now(),
        documentId: sender.documentId,
      };
      try {
        const reply = await callApp("/record/start", { json: { title: rec.title, page_url: tab.url || "" }, verify: true });
        if (/^[A-Za-z0-9]{1,32}$/.test(reply.id)) rec.appId = reply.id;
      } catch (error) {
        console.warn("Endo:", error);
      }
      await saveRecs();
      notifyPopup(tab.id);
    }
    // Without the first chunks (the init segment) the file cannot be played, so a recording that lost one stops.
    if (!rec.appId) return { ok: false, error: NOT_RUNNING };
    const bytes = await (await fetch("data:application/octet-stream;base64," + data)).arrayBuffer();
    let failed = "";
    try {
      await callApp(`/record/${rec.appId}/chunk?track=${track}&mime=${encodeURIComponent(mime)}`, { body: bytes, timeout: 60000 });
      rec.bytes += bytes.byteLength;
      rec.tracks = Math.max(rec.tracks, track + 1);
    } catch (error) {
      // Nothing after a lost chunk can be joined, so the recording ends: with what reached the app saved (a full disk
      // or the size limit late in a long capture), or deleted when that is not even the start.
      callApp(`/record/${rec.appId}/${rec.bytes ? "finish" : "abort"}`).catch(() => {});
      rec.appId = null;
      failed = error.message;
      console.warn("Endo:", error);
    }
    await saveRecs();
    return rec.appId ? { ok: true } : { ok: false, error: failed || NOT_RUNNING };
  });
}

// ---------------------------------------------------------------------------------------------------------------
// Messages

const validTabId = (tabId) => Number.isInteger(tabId) && tabId >= 0;

/** Commands from the popup (an extension page). */
const POPUP_COMMANDS = {
  async GET_STATE({ tabId }) {
    if (!validTabId(tabId)) return { ok: false, error: "bad tabId" };
    const [state, app, recs, settings] = await Promise.all([readTab(tabId), findApp(), loadRecs(), getSettings()]);
    const recordings = Object.entries(recs)
      .filter(([, rec]) => rec.tabId === tabId)
      .map(([key, rec]) => ({ key, mode: rec.mode, bytes: rec.bytes, tracks: rec.tracks, title: rec.title }));
    const items = Object.values(state.items).sort((a, b) => b.time - a.time);
    return { items, app, recordings, settings, armed: Boolean(state.armed) };
  },

  async SEND({ tabId, id, url, filename, mp4 }) {
    if (!validTabId(tabId) || typeof id !== "string") return { ok: false, error: "bad request" };
    const item = (await readTab(tabId)).items[id];
    if (!item) return { ok: false, error: "That item is gone; reopen the popup." };
    const target = url || item.url;
    const { referer, userAgent, cookies, headers } = item.request;
    return sendToApp({
      url: target,
      // The cookies were sent to the item's host; a variant on another host must not receive them.
      cookies: hostOf(target) === hostOf(item.url) ? cookies : "",
      userAgent,
      referer: referer || item.pageUrl,
      headers,
      filename,
      hls: item.kind === "hls",
      mp4,
    });
  },

  SEND_URL({ url, filename, referer }) {
    return sendToApp({ url, filename, referer });
  },

  async BLOCK_HOST({ host }) {
    const blocked = cleanHost(host);
    if (!blocked) return { ok: false, error: "Not a host name." };
    const settings = await getSettings();
    if (!settings.blockedHosts.includes(blocked)) {
      await saveSettings({ ...settings, blockedHosts: [...settings.blockedHosts, blocked] });
    }
    const stored = await chrome.storage.session.get(null);
    const tabIds = Object.keys(stored).filter((key) => key.startsWith("tab:")).map((key) => Number(key.slice(4)));
    await Promise.all(
      tabIds.map((tabId) =>
        updateTab(tabId, (state) => {
          const before = Object.keys(state.items).length;
          for (const [id, item] of Object.entries(state.items)) {
            if (isBlocked(hostOf(item.url), [blocked])) delete state.items[id];
          }
          return Object.keys(state.items).length !== before;
        }),
      ),
    );
    return { ok: true };
  },

  async CLEAR({ tabId }) {
    if (!validTabId(tabId)) return { ok: false, error: "bad tabId" };
    await updateTab(tabId, (state) => {
      state.items = {};
      return true;
    });
    return { ok: true };
  },

  async SET_SETTINGS({ settings }) {
    if (!settings || typeof settings !== "object") return { ok: false, error: "bad settings" };
    await saveSettings({ ...(await getSettings()), ...settings });
    return { ok: true };
  },

  async REC_ARM({ tabId }) {
    if (!validTabId(tabId)) return { ok: false, error: "bad tabId" };
    await updateTab(tabId, (state) => {
      state.armedAt = Date.now();
      return true;
    });
    // The bridge marks sessionStorage on receipt; the reload lets the hook start capturing at document_start. A tab
    // opened before the extension was installed or updated has no bridge yet: the mark is set from here instead.
    if (!(await sendToFrames(tabId, { cmd: "REC_ARM" }))) {
      try {
        await chrome.scripting.executeScript({
          target: { tabId, allFrames: true },
          func: (key) => {
            try {
              sessionStorage.setItem(key, String(Date.now()));
            } catch {}
          },
          args: [ARM_KEY],
        });
      } catch (error) {
        return { ok: false, error: `This page can't be recorded: ${error?.message || error}` };
      }
    }
    setTimeout(() => chrome.tabs.reload(tabId).catch(() => {}), 300);
    return { ok: true };
  },

  async MSR_START({ tabId, frameId, index }) {
    if (!validTabId(tabId) || !Number.isInteger(frameId) || frameId < 0 || !Number.isInteger(index) || index < 0) {
      return { ok: false, error: "bad request" };
    }
    const reply = await sendToFrames(tabId, { cmd: "MSR_START", index }, { frameId });
    if (!reply) return { ok: false, error: "The page did not answer; reload it and try again." };
    if (!reply.ok) return { ok: false, error: typeof reply.error === "string" ? reply.error : "Recording failed." };
    // Granted before the recorder's first chunk, which comes 2 s after it starts.
    await updateTab(tabId, (state) => {
      state.msr = { ...state.msr, [frameId]: true };
      return true;
    });
    return { ok: true };
  },

  async REC_STOP({ tabId }) {
    if (!validTabId(tabId)) return { ok: false, error: "bad tabId" };
    sendToFrames(tabId, { cmd: "REC_STOP" });
    await updateTab(tabId, (state) => {
      if (!state.armed && !state.capture) return false;
      state.armed = false;
      // No buffer recording starts again in this tab until it is armed again.
      delete state.capture;
      return true;
    });
    // Frames answer with REC_END; the recordings open now that are still open after 5 s are finished here, not those
    // started since (the user may have armed the tab or started "Record playback" again meanwhile).
    const open = Object.entries(await loadRecs()).filter(([, rec]) => rec.tabId === tabId);
    setTimeout(() => {
      for (const [key, rec] of open) {
        serialize(recQueues, key, async () => (await loadRecs())[key] === rec && closeRecording(key));
      }
    }, 5000);
    return { ok: true };
  },

  REC_SPEED({ tabId, rate }) {
    if (!validTabId(tabId) || !PLAYBACK_RATES.includes(rate)) return { ok: false, error: "bad request" };
    sendToFrames(tabId, { cmd: "REC_SPEED", rate });
    return { ok: true };
  },
};

/** Messages from our content scripts. Their data passes through the page, so all of it is checked. */
const PAGE_COMMANDS = {
  async M3U8_SEEN({ url, text }, sender) {
    if (!isHttpUrl(url) || typeof text !== "string" || text.length > MAX_PLAYLIST_CHARS) return { ok: false };
    const tab = sender.tab;
    if (isMediaSiteHost(hostOf(tab.url))) return { ok: false };
    const host = hostOf(url);
    const settings = await getSettings();
    if (isAdHost(host) || isBlocked(host, settings.blockedHosts)) return { ok: false };
    const hls = parseM3u8(text, url);
    if (!hls) return { ok: false };
    const seen = headersByUrl.get(url);
    let request;
    if (seen) {
      request = splitRequestHeaders(seen.headers);
    } else {
      const frameUrl = isHttpUrl(sender.url) ? sender.url : "";
      request = { referer: frameUrl, userAgent: "", cookies: "", headers: frameUrl ? { Origin: new URL(frameUrl).origin } : {} };
    }
    await addItem(tab.id, {
      id: "m:" + url,
      url,
      kind: "hls",
      format: "m3u8",
      name: classifyResponse({ url, contentType: "application/vnd.apple.mpegurl" }).name,
      size: 0,
      contentType: "application/vnd.apple.mpegurl",
      tabId: tab.id,
      frameId: sender.frameId ?? 0,
      pageUrl: tab.url || "",
      title: tab.title || "",
      time: Date.now(),
      request,
      hls,
      hidden: false,
    });
    return { ok: true };
  },

  async REC_ACTIVE(_message, sender) {
    const tabId = sender.tab.id;
    const frameId = sender.frameId ?? 0;
    // Only a document loaded while the user's REC_ARM was fresh may record; the page could send this itself.
    const armed = await updateTab(tabId, (state) => {
      if (!(Date.now() - (state.armedAt || 0) < ARM_WINDOW_MS)) return false;
      state.capture = { ...state.capture, [frameId]: sender.documentId };
      state.armed = true;
      return true;
    });
    if (!armed) return { ok: false };
    // Buffer recordings this frame's previous document left open are complete.
    await finishTabRecordings(tabId, (rec) => rec.frameId !== frameId || rec.mode !== "mse" || rec.documentId === sender.documentId);
    return { ok: true };
  },

  REC_CHUNK: onRecChunk,

  REC_END({ ms }, sender) {
    if (!isRecordingIndex(ms)) return { ok: false };
    finishRecording(`${sender.tab.id}:${sender.frameId ?? 0}:${ms}`);
    return { ok: true };
  },
};

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (sender.id !== chrome.runtime.id || !message || typeof message.cmd !== "string") return false;
  const fromExtensionPage = typeof sender.url === "string" && sender.url.startsWith(chrome.runtime.getURL(""));
  const commands = fromExtensionPage ? POPUP_COMMANDS : sender.tab?.id >= 0 ? PAGE_COMMANDS : null;
  const handler = commands && Object.hasOwn(commands, message.cmd) ? commands[message.cmd] : null;
  if (!handler) return false;
  // Called synchronously so REC_CHUNK joins its queue in arrival order.
  let result;
  try {
    result = Promise.resolve(handler(message, sender));
  } catch (error) {
    result = Promise.reject(error);
  }
  result.then(sendResponse, (error) => sendResponse({ ok: false, error: error?.message || String(error) }));
  return true;
});
