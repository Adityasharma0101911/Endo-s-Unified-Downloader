// Rescuing the app's downloads whose link expired: the app lists them (POST /expired), and a new link the browser comes
// upon for one (the user starts the file again, or the page plays its video again) goes to the app's POST /relink in
// place of a new download. The app checks the file is the same (its size), so a wrong guess is refused harmlessly.
// No chrome.* in here, so node can test it.
import { extensionOf } from "./capture.js";

/** How long the app's list is used before it is asked for again, when a candidate comes. */
export const WAITING_TTL_MS = 15000;
/** Most links tried for one waiting download from the media its page plays. */
export const MAX_TRIES = 3;

/** A file name without its folder or the " (1)" a browser adds to a name already taken, lower case. */
export function baseName(name) {
  const file = String(name || "").split(/[/\\]/).pop();
  return file.replace(/ \(\d+\)(?=\.[^.]*$|$)/, "").toLowerCase();
}

/** The last path segment of a link, decoded; "" when it has none. */
function linkName(url) {
  try {
    return decodeURIComponent(new URL(url).pathname.split("/").pop());
  } catch {
    return "";
  }
}

/**
 * The waiting download (of `waiting`, the app's list) a browser download (a downloads.DownloadItem) is a new link for:
 * the same file name (else its link's), but for a browser's " (1)", and the same size when both are known. Null when
 * none is.
 */
export function matchDownload(waiting, item) {
  const name = baseName(item.filename) || baseName(linkName(item.finalUrl || item.url));
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : 0;
  const same = (download) => baseName(download.name) === name && (!size || !(download.size > 0) || download.size === size);
  return (name && waiting.find(same)) || null;
}

/** Whether two links are of the same page: the same origin and path, whatever their query and fragment. */
export function samePage(a, b) {
  try {
    const [x, y] = [new URL(a), new URL(b)];
    return x.origin === y.origin && x.pathname === y.pathname;
  } catch {
    return false;
  }
}

/**
 * The waiting downloads a file a page plays (an item the background found: `kind`, `name`, `format`, `pageUrl`) may be
 * a new link for: those that came from that page, with the same file name or the same type. Streams are none.
 */
export function matchMedia(waiting, item) {
  if (item.kind !== "file") return [];
  const name = baseName(item.name);
  const type = extensionOf(item.name) || String(item.format || "").toLowerCase();
  const alike = (download) => baseName(download.name) === name || extensionOf(download.name) === type;
  return waiting.filter((download) => download.page && samePage(download.page, item.pageUrl) && alike(download));
}

/** Notes `url` tried for waiting download `id` in `tried`; false when it was tried before, or MAX_TRIES links were. */
export function firstTry(tried, id, url) {
  const urls = tried.get(id) ?? new Set();
  if (urls.has(url) || urls.size >= MAX_TRIES) return false;
  tried.set(id, urls.add(url));
  return true;
}

/**
 * Gives the app a browser download it captured (`item`, fetched from `url`): as the new link of the waiting download it
 * matches (see matchDownload), through `relink(download)`; else, when none matches or the app refused it (another
 * file), through `add()` as before. Resolves to what `add` gives, or {ok: true} for a download rescued.
 */
export async function sendCaptured(waiting, item, relink, add) {
  const expired = matchDownload(waiting, item);
  return expired && (await relink(expired)) ? { ok: true } : add();
}

/**
 * Tries a file a page plays (`item`) as the new link of each waiting download it may be (see matchMedia), through
 * `relink(download)`, each link once and MAX_TRIES at most for one download (`tried`), until one takes it. Resolves to
 * the download rescued, else null.
 */
export async function rescueWithMedia(waiting, item, tried, relink) {
  for (const download of matchMedia(waiting, item)) {
    if (firstTry(tried, download.id, item.url) && (await relink(download))) return download;
  }
  return null;
}

/**
 * The app's list of downloads waiting for a new link, asked for (`fetchList`) only when a candidate comes and the list
 * is older than WAITING_TTL_MS, or `fresh` is asked; never on a timer. Asked at once by several, it is asked for once.
 * When the app can't be asked (not running, or too old to list them), the list is empty.
 */
export class WaitingList {
  constructor(fetchList, now = () => Date.now()) {
    this.fetchList = fetchList;
    this.now = now;
    this.list = [];
    this.at = -Infinity;
    this.pending = null;
  }

  get({ fresh = false } = {}) {
    if (!fresh && this.now() - this.at < WAITING_TTL_MS) return Promise.resolve(this.list);
    this.pending ??= Promise.resolve()
      .then(this.fetchList)
      .then((list) => (Array.isArray(list) ? list : []), () => [])
      .then((list) => {
        [this.list, this.at, this.pending] = [list, this.now(), null];
        return list;
      });
    return this.pending;
  }

  /** Forgets download `id`, which took its new link. */
  drop(id) {
    this.list = this.list.filter((download) => download.id !== id);
  }
}
