// Service worker: finds media requests in every tab, keeps them per tab in session storage, and is the hub between
// the popup, the content-script bridge and the desktop app's local HTTP API. Chrome stops the worker after ~30 s
// idle, so everything that must outlive it is in chrome.storage.session; the in-memory maps are only caches and
// queues that may start empty. Listeners are registered at the top level so a restarted worker gets the event that
// woke it.

import {
  classifyResponse,
  fetchableHeaders,
  isAdHost,
  isMediaSiteHost,
  mediaUrlFormat,
  newerVersion,
  parseM3u8,
  passesSizeFilter,
  rangedVideoCandidate,
  sanitizeFilename,
  splitRequestHeaders,
} from "./lib/detect.js";

const APP_NAME = "endos-unified-downloader";
const APP_PORTS = [49152, 49153, 49154, 49155];
const NOT_RUNNING = "Endo's Unified Downloader is not running.";
const OUTDATED = "An older Endo's Unified Downloader is running — restart it after updating.";
const MAX_ITEMS_PER_TAB = 200;
const MAX_PLAYLIST_CHARS = 2 * 1024 * 1024;
/** The app's limit for a playlist the page built itself (sent inline as `playlist`). */
const MAX_INLINE_PLAYLIST_BYTES = 1024 * 1024;
/** Characters of the texts of playlists pages built kept at once, every tab together (see keepInlineText). */
const INLINE_BUDGET = 4 * 1024 * 1024;
const INLINE_KEY = "inlineTexts";
/** The app's limit for `cookie_jar`. */
const MAX_COOKIES = 500;
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
/** Hosts per tab whose cookies are remembered (see noteCookiesSent). */
const MAX_COOKIE_HOSTS = 100;
const IS_FIREFOX = chrome.runtime.getURL("").startsWith("moz-extension:");

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

/**
 * The origin that made a request: Chrome's `initiator`, else (Firefox) the origin of the document or the URL that
 * made it. Undefined when none is known.
 */
function initiatorOf(details) {
  if (details.initiator) return details.initiator;
  try {
    return new URL(details.documentUrl || details.originUrl).origin;
  } catch {
    return undefined;
  }
}

/** Which document sent a message: Chrome's documentId, else (Firefox has none) the frame URL. */
const documentOf = (sender) => sender.documentId ?? `url:${sender.url}`;

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
/**
 * By tab, by host: the last request the tab made there with cookies, and the names of the cookies the browser sent
 * with it. Only those cookies go into a download's cookie jar (see cookieJarFor).
 */
const cookiesSent = new Map();

function noteCookiesSent(details, headers) {
  const cookie = details.tabId >= 0 && headers.find((header) => header.name.toLowerCase() === "cookie")?.value;
  if (!cookie) return;
  let hosts = cookiesSent.get(details.tabId);
  if (!hosts) cookiesSent.set(details.tabId, (hosts = new Map()));
  const host = hostOf(details.url);
  hosts.delete(host);
  hosts.set(host, { url: details.url, names: new Set(cookie.split(";").map((pair) => pair.split("=")[0].trim())) });
  if (hosts.size > MAX_COOKIE_HOSTS) hosts.delete(hosts.keys().next().value);
}

chrome.webRequest.onSendHeaders.addListener(
  (details) => {
    // Our own calls to the app come from the extension origin and are not the page's.
    const initiator = initiatorOf(details);
    if (initiator && !/^https?:/i.test(initiator)) return;
    // A CORS preflight shares the request's URL but not its headers: it must not replace them.
    if (details.method === "OPTIONS") return;
    const headers = details.requestHeaders || [];
    headersByRequest.set(details.requestId, headers);
    headersByUrl.delete(details.url);
    headersByUrl.set(details.url, { tabId: details.tabId, frameId: details.frameId, headers, time: Date.now() });
    if (headersByUrl.size > HEADER_CACHE_SIZE) headersByUrl.delete(headersByUrl.keys().next().value);
    noteCookiesSent(details, headers);
  },
  REQUEST_FILTER,
  // Chrome hides Cookie/Referer/Origin without "extraHeaders"; Firefox shows them and rejects the option.
  chrome.webRequest.OnSendHeadersOptions?.EXTRA_HEADERS ? ["requestHeaders", "extraHeaders"] : ["requestHeaders"],
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

/**
 * Session storage holds 10 MB for everything, so the texts of the playlists pages built (inline items, up to 1 MiB
 * each) are kept apart from the tabs' items, under one key, `${tabId}:${itemId}` → {text, time}: the newest
 * INLINE_BUDGET characters of them. An older one is dropped; its item asks for a reload when sent.
 */
const inlineQueue = new Map();

function changeInlineTexts(change) {
  return serialize(inlineQueue, INLINE_KEY, async () => {
    const texts = (await chrome.storage.session.get(INLINE_KEY))[INLINE_KEY] || {};
    change(texts);
    let total = 0;
    for (const [key, entry] of Object.entries(texts).sort(([, a], [, b]) => b.time - a.time)) {
      total += entry.text.length;
      if (total > INLINE_BUDGET) delete texts[key];
    }
    await chrome.storage.session.set({ [INLINE_KEY]: texts });
  });
}

const keepInlineText = (tabId, id, text) => changeInlineTexts((texts) => (texts[`${tabId}:${id}`] = { text, time: Date.now() }));

async function inlineText(tabId, id) {
  return (await chrome.storage.session.get(INLINE_KEY))[INLINE_KEY]?.[`${tabId}:${id}`]?.text;
}

const forgetInlineTexts = (tabId) =>
  changeInlineTexts((texts) => {
    for (const key of Object.keys(texts)) if (key.startsWith(`${tabId}:`)) delete texts[key];
  });

/** Forgets what is known of a tab's requests: the texts of its inline items and the cookies it sent. */
function forgetTab(tabId) {
  cookiesSent.delete(tabId);
  forgetInlineTexts(tabId);
}

function clearTab(tabId) {
  forgetTab(tabId);
  return updateTab(tabId, (state) => {
    state.items = {};
    state.armed = false;
    return true;
  });
}

/**
 * Hides media playlists that are a variant, audio or subtitle rendition of a master in the same tab and gives the
 * master their duration, hosts, and whether they are live, encrypted or DRM-protected: the master is what the popup
 * shows. A playlist whose master is gone shows again.
 */
function linkPlaylists(items) {
  const masterOf = new Map();
  for (const item of Object.values(items)) {
    if (item.hls?.kind !== "master") continue;
    for (const rendition of [...item.hls.variants, ...item.hls.audio, ...(item.hls.subtitles || [])]) {
      masterOf.set(rendition.url, item);
    }
  }
  for (const item of Object.values(items)) {
    const master = item.inline ? null : masterOf.get(item.url);
    item.hidden = Boolean(master && master !== item);
    if (!item.hidden || item.hls?.kind !== "media") continue;
    if (!master.hls.duration) master.hls.duration = item.hls.duration;
    for (const flag of ["live", "encrypted", "drm"]) master.hls[flag] ||= item.hls[flag];
    master.hls.hosts = [...new Set([...(master.hls.hosts || []), ...(item.hls.hosts || [])])];
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

/**
 * Stores an item, merging it into the tab's item for the same URL (a playlist the page built: the same text, so the
 * same id) if there is one. A full tab makes room by dropping the shown item seen least lately, a master only when
 * nothing else is left (its renditions would show in its place).
 */
function addItem(tabId, item) {
  if (item.kind === "hls" && !item.inline) item.url = withoutLowLatency(item.url);
  return updateTab(tabId, (state) => {
    const existing = Object.values(state.items).find((stored) =>
      item.inline ? stored.id === item.id : !stored.inline && stored.url === item.url,
    );
    if (existing) {
      existing.lastSeen = Date.now();
      existing.size = Math.max(existing.size || 0, item.size || 0);
      if (item.hls) existing.hls = item.hls;
      for (const field of ["referer", "userAgent", "cookies"]) existing.request[field] ||= item.request[field];
      existing.request.headers = { ...item.request.headers, ...existing.request.headers };
    } else {
      const items = Object.values(state.items);
      if (items.length >= MAX_ITEMS_PER_TAB) {
        const isMaster = (stored) => stored.hls?.kind === "master";
        const seen = (stored) => stored.lastSeen || stored.time;
        const oldest = items.filter((stored) => !stored.hidden).sort((a, b) => isMaster(a) - isMaster(b) || seen(a) - seen(b))[0];
        if (!oldest) return false;
        delete state.items[oldest.id];
      }
      state.items[item.id] = item;
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

/** URLs already asked about with CHECK_VIDEO_SRC (a ranged player asks for the same file many times), and the answer. */
const videoSrcAnswers = new Map();

/** Whether the frame plays `url` in a <video>/<source>, asked once per URL. */
async function isVideoSrc(tabId, frameId, url) {
  if (!videoSrcAnswers.has(url)) {
    const answer = (await sendToFrames(tabId, { cmd: "CHECK_VIDEO_SRC", url }, { frameId: Math.max(0, frameId) })) === true;
    videoSrcAnswers.set(url, answer);
    if (videoSrcAnswers.size > HEADER_CACHE_SIZE) videoSrcAnswers.delete(videoSrcAnswers.keys().next().value);
  }
  return videoSrcAnswers.get(url);
}

async function onMediaResponse(details, requestHeaders) {
  if (!isHttpUrl(details.url) || details.statusCode < 200 || details.statusCode > 299) return;
  const initiator = initiatorOf(details);
  if (!initiator || !/^https?:\/\//i.test(initiator)) return;
  const header = (name) => details.responseHeaders?.find((h) => h.name.toLowerCase() === name)?.value;
  const response = {
    url: details.url,
    type: details.type,
    contentType: header("content-type"),
    contentDisposition: header("content-disposition"),
    contentLength: header("content-length"),
    contentRange: header("content-range"),
  };
  const classified = classifyResponse(response);
  const found = classified || rangedVideoCandidate(response);
  if (!found) return;
  const host = hostOf(details.url);
  const settings = await getSettings();
  if (isAdHost(host) || isBlocked(host, settings.blockedHosts) || !passesSizeFilter(found, settings)) return;
  const tab = await tabFor(details.tabId, initiator);
  if (!tab || isMediaSiteHost(hostOf(tab.url))) return;
  // An untyped ranged XHR is only a video when the page plays that URL.
  if (!classified && !(await isVideoSrc(tab.id, details.frameId, details.url))) return;
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
    // Whether the page may read it itself ("Via browser"): a <video>'s own request needs no CORS, the page's fetch does.
    pageCanFetch: details.type !== "media" || new URL(details.url).origin === initiator || Boolean(header("access-control-allow-origin")),
  });
}

chrome.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status !== "loading" || !info.url) return;
  clearTab(tabId);
  finishTabRecordings(tabId);
});

chrome.tabs.onRemoved.addListener((tabId) => {
  forgetTab(tabId);
  serialize(tabQueues, tabId, () => chrome.storage.session.remove(tabKey(tabId)));
  finishTabRecordings(tabId);
});

// ---------------------------------------------------------------------------------------------------------------
// The desktop app

/**
 * Asks a port whether the app listens there. An app from before /ping existed answers it with 404: that is an
 * outdated app ({ok: false, outdated: true}) when it is that app (see isOldApp), not another program on the port.
 */
async function ping(port) {
  try {
    const response = await fetch(`http://127.0.0.1:${port}/ping`, { signal: AbortSignal.timeout(800) });
    if (response.status === 404) return (await isOldApp(port)) ? { ok: false, outdated: true, port, version: null } : null;
    const body = await response.json();
    if (!response.ok || body?.app !== APP_NAME) return null;
    reloadIfNewer(body.extension).catch(() => {});
    return { ok: true, port, version: String(body.version ?? "") };
  } catch {
    return null;
  }
}

/**
 * The app's updater replaces the extension folder next to it and says which version is there; Chrome only runs it
 * after a reload. Reloads once per offered version, so an extension loaded from another folder never loops.
 */
async function reloadIfNewer(offered) {
  if (!newerVersion(offered, chrome.runtime.getManifest().version)) return;
  const { reloadedFor } = await chrome.storage.local.get("reloadedFor");
  if (reloadedFor === offered) return;
  // A reload cuts off a request to the app and the recordings and browser downloads it relays: a later ping, once
  // none is under way, reloads.
  if (Object.values(await loadRecs()).some((rec) => rec.appId) || appCalls) return;
  await chrome.storage.local.set({ reloadedFor: offered });
  chrome.runtime.reload();
}

/** Whether the app from before /ping listens on `port`: it answers every preflight with exactly these methods. */
async function isOldApp(port) {
  try {
    const preflight = await fetch(`http://127.0.0.1:${port}/add`, { method: "OPTIONS", signal: AbortSignal.timeout(800) });
    return preflight.status === 204 && preflight.headers.get("access-control-allow-methods") === "POST, OPTIONS";
  } catch {
    return false;
  }
}

/** Finds the running app: the port that answered last, else the first of APP_PORTS that answers. */
async function findApp() {
  const { appPort } = await chrome.storage.session.get("appPort");
  const cached = APP_PORTS.includes(appPort) ? await ping(appPort) : null;
  if (cached?.ok) return cached;
  const answers = await Promise.all(APP_PORTS.map(ping));
  const found = answers.find((answer) => answer?.ok);
  if (!found) {
    await chrome.storage.session.remove("appPort");
    return answers.find((answer) => answer?.outdated) || { ok: false, port: null, version: null };
  }
  await chrome.storage.session.set({ appPort: found.port });
  return found;
}

/** Requests to the app under way (see callApp); the extension does not reload under one (see reloadIfNewer). */
let appCalls = 0;

/**
 * POSTs to the app and returns its JSON reply, throwing a readable error on failure. `verify` pings first (used
 * before sending cookies); otherwise the cached port is trusted, which keeps recording chunks to one request each.
 */
async function callApp(path, options) {
  appCalls++;
  try {
    return await requestApp(path, options);
  } finally {
    appCalls--;
  }
}

/** callApp's request. */
async function requestApp(path, { json, body, timeout = 10000, verify = false } = {}) {
  let port = verify ? null : (await chrome.storage.session.get("appPort")).appPort;
  if (!APP_PORTS.includes(port)) {
    const app = await findApp();
    if (!app.ok) throw new Error(app.outdated ? OUTDATED : NOT_RUNNING);
    port = app.port;
  }
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

/**
 * Queues a download in the app via POST /add; empty fields are left out. `playlist` is the text of a playlist the
 * page built itself (then `url` is the base for its relative links); `height` caps the quality the app picks.
 */
async function sendToApp({ url, cookies, cookieJar, userAgent, referer, headers, filename, hls, dash, mp4, height, playlist }) {
  if (!isHttpUrl(url)) return { ok: false, error: "Only http(s) links can be downloaded." };
  const body = { url };
  // The app only takes a link for a playlist by its URL; one served from /api/… must be named one.
  if (hls) body.hls = true;
  if (dash) body.dash = true;
  // Whether the app remuxes the saved stream into an MP4; only a stream has anything to convert.
  if ((hls || dash) && typeof mp4 === "boolean") body.mp4 = mp4;
  if (Number.isInteger(height) && height > 0) body.height = height;
  if (typeof playlist === "string" && playlist) body.playlist = playlist;
  if (cookies) body.cookies = cookies;
  if (cookieJar?.length) body.cookie_jar = cookieJar;
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

/**
 * The cookies the browser sent from the item's tab to the places a download of `item` (as `target`) fetches from: the
 * item and target URLs, each variant, audio and subtitle playlist, and the hosts its segments and keys come from. Those
 * come from playlist text the page controls, so only a cookie the tab's own requests carried there goes in (see
 * noteCookiesSent): no cookie the browser would hold back (SameSite), none for a host the tab never asked. In the
 * app's `cookie_jar` shape, with what chrome.cookies knows of each (domain, path, expiry), one per name+domain+path, at
 * most MAX_COOKIES.
 */
async function cookieJarFor(item, target) {
  const sent = cookiesSent.get(item.tabId);
  if (!chrome.cookies || !sent) return [];
  const hls = item.hls;
  const urls = [item.url, target];
  if (hls?.kind === "master") urls.push(...[...hls.variants, ...hls.audio, ...(hls.subtitles || [])].map((rendition) => rendition.url));
  const hosts = new Set([...urls.map(hostOf), ...(hls?.hosts || [])]);
  const found = await Promise.all(
    [...hosts]
      .map((host) => sent.get(host))
      .filter(Boolean)
      .map(({ url, names }) => chrome.cookies.getAll({ url }).then((cookies) => cookies.filter((cookie) => names.has(cookie.name)), () => [])),
  );
  const jar = new Map();
  for (const cookie of found.flat()) {
    // The app's cookies file is tab-separated: a field holding a tab or newline cannot be written.
    if (/[\t\r\n]/.test(cookie.name + cookie.value + cookie.domain + cookie.path)) continue;
    jar.set(`${cookie.name}\t${cookie.domain}\t${cookie.path}`, {
      domain: cookie.domain,
      path: cookie.path,
      name: cookie.name,
      value: cookie.value,
      secure: Boolean(cookie.secure),
      http_only: Boolean(cookie.httpOnly),
      host_only: Boolean(cookie.hostOnly),
      expires: cookie.session ? 0 : Math.max(0, Math.floor(cookie.expirationDate || 0)),
    });
  }
  return [...jar.values()].slice(0, MAX_COOKIES);
}

// ---------------------------------------------------------------------------------------------------------------
// Install / update

/**
 * Content scripts only reach pages loaded after install; this puts them into the tabs already open (and replaces the
 * ones an update cut off), so their media is found once it is requested again. Tabs that refuse are skipped.
 */
async function injectIntoOpenTabs() {
  const tabs = await chrome.tabs.query({ url: ["http://*/*", "https://*/*"] }).catch(() => []);
  for (const tab of tabs) {
    const target = { tabId: tab.id, allFrames: true };
    chrome.scripting.executeScript({ target, files: ["content/hook.js"], world: "MAIN", injectImmediately: true }).catch(() => {});
    chrome.scripting.executeScript({ target, files: ["content/bridge.js"], injectImmediately: true }).catch(() => {});
  }
}

// ---------------------------------------------------------------------------------------------------------------
// Context menus

chrome.runtime.onInstalled.addListener((details) => {
  // Firefox puts the manifest's content scripts into the open tabs itself.
  if ((details.reason === "install" || details.reason === "update") && !IS_FIREFOX) injectIntoOpenTabs();
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

/**
 * Removes a recording and tells the app to join what it has. Runs inside the recording's queue. A browser download
 * that failed (its `error`) is thrown away instead, not saved as if it were whole, and stays listed with its error
 * (`failed`) until it is dismissed (closed again) or its tab moves on.
 */
async function closeRecording(key, action = "finish") {
  const recs = await loadRecs();
  const rec = recs[key];
  if (!rec) return;
  delete recs[key];
  if (rec.error && !rec.failed) {
    action = "abort";
    recs[key] = { ...rec, appId: null, failed: true };
  }
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

/** A browser download's recording index: "dl:" + the key BROWSER_DL made. */
const isBrowserDownload = (ms) => typeof ms === "string" && /^dl:[A-Za-z0-9]{1,64}$/.test(ms);
const isRecordingIndex = (ms) => ms === "msr" || (Number.isInteger(ms) && ms >= 0) || isBrowserDownload(ms);

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

function onRecChunk({ ms, track, part = 0, mime, data }, sender) {
  if (
    !isRecordingIndex(ms) ||
    !Number.isInteger(track) || track < 0 || track > 15 ||
    // A track's parts are the stretches between two init segments (a quality switch); the app joins them in order.
    !Number.isInteger(part) || part < 0 || part > 255 ||
    typeof mime !== "string" || mime.length > 255 ||
    typeof data !== "string" || data.length > MAX_CHUNK_BASE64
  ) {
    return { ok: false, error: "bad chunk" };
  }
  const tab = sender.tab;
  const frameId = sender.frameId ?? 0;
  const key = `${tab.id}:${frameId}:${ms}`;
  const browser = isBrowserDownload(ms);
  const documentId = documentOf(sender);
  // Queued synchronously, so chunks keep the order they arrived in.
  return serialize(recQueues, key, async () => {
    const recs = await loadRecs();
    // A reload starts index 0 again under the same key: the earlier document's recording is complete.
    if (!browser && recs[key] && recs[key].documentId !== documentId) await closeRecording(key);
    let rec = recs[key];
    if (!rec) {
      // The page can fake the hook's messages, so only a recording the user asked for in this tab reaches the app;
      // a browser download exists only once BROWSER_DL opened it.
      if (browser || !(await mayRecord(tab.id, frameId, ms, documentId))) return { ok: false, error: "not armed" };
      rec = recs[key] = {
        appId: null,
        tabId: tab.id,
        frameId,
        title: tab.title || "Recording",
        mode: ms === "msr" ? "msr" : "mse",
        bytes: 0,
        tracks: 0,
        started: Date.now(),
        documentId,
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
      await callApp(`/record/${rec.appId}/chunk?track=${track}&part=${part}&mime=${encodeURIComponent(mime)}`, {
        body: bytes,
        timeout: 60000,
      });
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
    const [state, app, recs, settings, hostAccess] = await Promise.all([
      readTab(tabId),
      findApp(),
      loadRecs(),
      getSettings(),
      // Firefox (and Chrome's "on click" site access) can withhold the host access detection needs.
      chrome.permissions.contains({ origins: ["<all_urls>"] }).catch(() => false),
    ]);
    const recordings = Object.entries(recs)
      .filter(([, rec]) => rec.tabId === tabId)
      .map(([key, rec]) => ({
        key,
        mode: rec.mode,
        bytes: rec.bytes,
        tracks: rec.tracks,
        title: rec.title,
        // A browser download's progress in segments (HLS) or bytes (file), and why it failed.
        done: rec.done ?? null,
        total: rec.total ?? null,
        error: rec.failed ? rec.error : null,
      }));
    const items = Object.values(state.items).sort((a, b) => b.time - a.time);
    return { items, app, recordings, settings, armed: Boolean(state.armed), hostAccess };
  },

  async SEND({ tabId, id, url, filename, mp4, height }) {
    if (!validTabId(tabId) || typeof id !== "string") return { ok: false, error: "bad request" };
    const item = (await readTab(tabId)).items[id];
    if (!item) return { ok: false, error: "That item is gone; reopen the popup." };
    const chosen = url || item.url;
    const master = item.hls?.kind === "master" ? item.hls : null;
    const variant = chosen !== item.url ? master?.variants.find((v) => v.url === chosen) : null;
    // A variant alone has no sound when the audio is a separate rendition: the app gets the master (yt-dlp joins the
    // two) with the variant's height as the quality cap.
    const viaMaster = Boolean(variant && master.separateAudio);
    const target = viaMaster ? item.url : chosen;
    const { referer, userAgent, cookies, headers } = item.request;
    // A playlist the page built is sent as text, with the frame URL (item.url) as its base.
    const playlist = item.inline && target === item.url ? await inlineText(tabId, item.id) : undefined;
    if (item.inline && target === item.url && !playlist) {
      return { ok: false, error: "This playlist was dropped to save memory; reload the page to find it again." };
    }
    return sendToApp({
      url: target,
      // The cookies were sent to the item's host; a variant on another host must not receive them.
      cookies: hostOf(target) === hostOf(item.url) ? cookies : "",
      cookieJar: await cookieJarFor(item, target),
      userAgent,
      referer: referer || item.pageUrl,
      headers,
      filename,
      hls: item.kind === "hls",
      dash: item.kind === "dash",
      mp4,
      height: (viaMaster && variant.height) || (Number.isInteger(height) && height <= 100000 ? height : undefined),
      playlist,
    });
  },

  /**
   * Downloads an item through the page itself (its TLS fingerprint, cookies and bot-check clearance): the bridge in
   * the item's frame fetches it and sends the bytes back as a recording, which the app saves like a capture.
   */
  async BROWSER_DL({ tabId, id, url, filename }) {
    if (!validTabId(tabId) || typeof id !== "string") return { ok: false, error: "bad request" };
    const item = (await readTab(tabId)).items[id];
    if (!item) return { ok: false, error: "That item is gone; reopen the popup." };
    if (item.kind !== "hls" && item.kind !== "file") {
      return { ok: false, error: "Only HLS streams and files can be downloaded through the browser." };
    }
    if (item.inline) return { ok: false, error: "The page built this playlist itself; use Download instead." };
    if (item.hls?.drm) return { ok: false, error: "This stream is DRM-protected and can't be downloaded." };
    if (item.pageCanFetch === false) {
      return { ok: false, error: "The page can't read this file itself (its server doesn't let other sites read it); use Download instead." };
    }
    let target = url || item.url;
    let audioUrl = null;
    const master = item.hls?.kind === "master" ? item.hls : null;
    // The page fetches one variant (the chosen one, else the best) and the audio rendition it plays with.
    const variant = master?.variants.find((v) => v.url === target) || master?.variants[0];
    if (variant) {
      target = variant.url;
      // The rendition its player starts with (DEFAULT=YES), else the group's first.
      const group = master.audio.filter((rendition) => rendition.groupId === variant.audioGroup);
      audioUrl = (group.find((rendition) => rendition.default) || group[0])?.url || null;
    }
    if (!isHttpUrl(target)) return { ok: false, error: "Only http(s) links can be downloaded." };
    const named = typeof filename === "string" && filename.trim() ? sanitizeFilename(filename).replace(/\.[a-z0-9]{1,5}$/i, "") : "";
    const title = named || sanitizeFilename(item.title);
    let appId;
    try {
      appId = (await callApp("/record/start", { json: { title, page_url: item.pageUrl || "" }, verify: true })).id;
    } catch (error) {
      return { ok: false, error: error.message };
    }
    if (!/^[A-Za-z0-9]{1,32}$/.test(appId)) return { ok: false, error: "The downloader gave an unexpected answer." };
    // Unguessable, so the page cannot feed chunks into it; the bridge alone learns it.
    const key = crypto.randomUUID().replaceAll("-", "");
    const frameId = Math.max(0, item.frameId ?? 0);
    const recKey = `${tabId}:${frameId}:dl:${key}`;
    (await loadRecs())[recKey] = {
      appId,
      tabId,
      frameId,
      title,
      mode: "browser",
      bytes: 0,
      tracks: 0,
      started: Date.now(),
      key,
      done: 0,
      total: 0,
    };
    await saveRecs();
    notifyPopup(tabId);
    const start = { cmd: "BROWSER_DL_START", key, url: target, kind: item.kind, audioUrl, headers: fetchableHeaders(item.request.headers) };
    let failure = "";
    try {
      const reply = await chrome.tabs.sendMessage(tabId, start, { frameId });
      if (reply?.ok === false) failure = typeof reply.error === "string" ? reply.error : "The page could not start the download.";
    } catch (error) {
      // A bridge that took the message but sent no reply is fine; no bridge at all is not.
      if (/receiving end does not exist/i.test(error?.message || "")) failure = "The page did not answer; reload it and try again.";
    }
    if (failure) {
      await serialize(recQueues, recKey, () => closeRecording(recKey, "abort"));
      return { ok: false, error: failure };
    }
    return { ok: true };
  },

  /** Stops a browser download (by its recordings key or its own key); what reached the app is kept. */
  async BROWSER_DL_CANCEL({ tabId, key }) {
    if (!validTabId(tabId) || typeof key !== "string") return { ok: false, error: "bad request" };
    const recs = await loadRecs();
    const recKey = Object.keys(recs).find(
      (candidate) => recs[candidate].tabId === tabId && recs[candidate].mode === "browser" && (candidate === key || recs[candidate].key === key),
    );
    if (!recKey) return { ok: true };
    sendToFrames(tabId, { cmd: "BROWSER_DL_CANCEL", key: recs[recKey].key }, { frameId: recs[recKey].frameId });
    await finishRecording(recKey);
    return { ok: true };
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
    forgetInlineTexts(tabId);
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
    // `silent`: the browser would not unmute the video, so it is recorded without sound.
    return { ok: true, silent: reply.silent === true };
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
    // Browser downloads are not recordings of the page and go on.
    const open = Object.entries(await loadRecs()).filter(([, rec]) => rec.tabId === tabId && rec.mode !== "browser");
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

const HLS_FIELDS = { kind: "hls", format: "m3u8", contentType: "application/vnd.apple.mpegurl" };
const DASH_FIELDS = { kind: "dash", format: "mpd", contentType: "application/dash+xml" };

/** The request headers to repeat for a URL a frame reported: those the browser sent for it, else the frame's own. */
function requestFor(url, sender) {
  const seen = headersByUrl.get(url);
  if (seen) return splitRequestHeaders(seen.headers);
  const frameUrl = isHttpUrl(sender.url) ? sender.url : "";
  return { referer: frameUrl, userAgent: "", cookies: "", headers: frameUrl ? { Origin: new URL(frameUrl).origin } : {} };
}

/**
 * Adds an item for a URL a frame reported, unless the tab shows a media site or the URL's host is an ad CDN or
 * blocked. `fields` sets kind, format and contentType, and overrides any other default.
 */
async function addPageItem(sender, url, fields) {
  const tab = sender.tab;
  if (url.length > 16384 || isMediaSiteHost(hostOf(tab.url))) return { ok: false };
  const host = hostOf(url);
  const settings = await getSettings();
  if (isAdHost(host) || isBlocked(host, settings.blockedHosts)) return { ok: false };
  await addItem(tab.id, {
    id: "m:" + url,
    url,
    name: classifyResponse({ url, contentType: fields.contentType })?.name || `video.${fields.format}`,
    size: 0,
    tabId: tab.id,
    frameId: sender.frameId ?? 0,
    pageUrl: tab.url || "",
    title: tab.title || "",
    time: Date.now(),
    request: requestFor(url, sender),
    hls: null,
    hidden: false,
    ...fields,
  });
  return { ok: true };
}

/**
 * A playlist the page built itself (a blob:/data: URL): the app gets its text, with the frame URL as the base its
 * relative links resolve against. One that links to no http(s) segment or variant is of no use to the app, nor is a
 * live one (the app cannot reload it). Its id comes from its text, so the same text is one item; the text itself is
 * kept apart (see keepInlineText).
 */
async function onInlinePlaylist(text, sender) {
  const base = isHttpUrl(sender.url) ? sender.url : sender.tab.url;
  const playlist = text.trim();
  const bytes = new TextEncoder().encode(playlist);
  if (!isHttpUrl(base) || bytes.length > MAX_INLINE_PLAYLIST_BYTES) return { ok: false };
  const hls = parseM3u8(playlist, base);
  if (!hls || !(hls.kind === "media" ? !hls.live && hls.hosts.length : hls.variants.some((variant) => isHttpUrl(variant.url)))) {
    return { ok: false };
  }
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  const id = "i:" + Array.from(digest.subarray(0, 16), (byte) => byte.toString(16).padStart(2, "0")).join("");
  await keepInlineText(sender.tab.id, id, playlist);
  return addPageItem(sender, base, { ...HLS_FIELDS, id, name: "playlist.m3u8", hls, inline: true });
}

/** Messages from our content scripts. Their data passes through the page, so all of it is checked. */
const PAGE_COMMANDS = {
  async M3U8_SEEN({ url, text, inline }, sender) {
    if (typeof text !== "string" || text.length > MAX_PLAYLIST_CHARS) return { ok: false };
    if (inline === true) return onInlinePlaylist(text, sender);
    if (!isHttpUrl(url)) return { ok: false };
    const hls = parseM3u8(text, url);
    return hls ? addPageItem(sender, url, { ...HLS_FIELDS, hls }) : { ok: false };
  },

  /** A playlist or manifest link the page had in its data (JSON), not necessarily requested yet. */
  MEDIA_URL_SEEN({ url }, sender) {
    const format = mediaUrlFormat(url);
    if (!format) return { ok: false };
    return addPageItem(sender, url, format === "mpd" ? DASH_FIELDS : HLS_FIELDS);
  },

  /** A response the page read whose text is a DASH manifest. */
  MPD_SEEN({ url }, sender) {
    return isHttpUrl(url) ? addPageItem(sender, url, DASH_FIELDS) : { ok: false };
  },

  /** How far the bridge's download for BROWSER_DL got, and why it failed (its last progress has `error`). */
  async BROWSER_DL_PROGRESS({ key, done, total, error }, sender) {
    const count = (n) => Number.isSafeInteger(n) && n >= 0;
    if (typeof key !== "string" || !isBrowserDownload("dl:" + key) || !count(done) || !count(total)) return { ok: false };
    const rec = (await loadRecs())[`${sender.tab.id}:${sender.frameId ?? 0}:dl:${key}`];
    if (!rec) return { ok: false };
    rec.done = done;
    rec.total = total;
    // The REC_END that follows then throws the download away (see closeRecording).
    if (typeof error === "string" && error) rec.error = error.slice(0, 500);
    await saveRecs();
    notifyPopup(rec.tabId);
    return { ok: true };
  },

  async REC_ACTIVE(_message, sender) {
    const tabId = sender.tab.id;
    const frameId = sender.frameId ?? 0;
    // Only a document loaded while the user's REC_ARM was fresh may record; the page could send this itself.
    const armed = await updateTab(tabId, (state) => {
      if (!(Date.now() - (state.armedAt || 0) < ARM_WINDOW_MS)) return false;
      state.capture = { ...state.capture, [frameId]: documentOf(sender) };
      state.armed = true;
      return true;
    });
    if (!armed) return { ok: false };
    // Buffer recordings this frame's previous document left open are complete.
    await finishTabRecordings(tabId, (rec) => rec.frameId !== frameId || rec.mode !== "mse" || rec.documentId === documentOf(sender));
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
