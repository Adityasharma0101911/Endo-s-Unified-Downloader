// Pure helpers of "Send browser downloads to the app": which of the browser's downloads go to the app, and the
// settings' file types and sites as typed. No chrome.* in here, so node can test it.
import { cleanHost, isBlocked } from "./detect.js";

/** The file types sent to the app until the user changes them. */
export const CAPTURE_TYPES = [
  "zip", "7z", "rar", "tar", "gz", "xz", "bz2", "zst", "iso", "img", "dmg", "pkg", "exe", "msi", "msix", "appimage",
  "deb", "rpm", "apk", "mp4", "mkv", "webm", "avi", "mov", "mp3", "flac", "wav", "m4a", "pdf", "epub", "torrent",
];
/** How long after an Alt-click a download stays in the browser. */
export const ALT_WINDOW_MS = 5000;

/** The words of a list as typed ("zip, .iso tar") or as stored (an array). */
const words = (value) => (Array.isArray(value) ? value.join(" ") : String(value ?? "")).split(/[\s,;]+/).filter(Boolean);

/** File types as typed (".zip", "tar.gz", "*.iso"): the last extension, lower case, each once. */
export function parseTypes(value) {
  const types = words(value).map((word) => word.toLowerCase().split(".").pop());
  return [...new Set(types.filter((type) => /^[a-z0-9]{1,16}$/.test(type)))].slice(0, 200);
}

/** Sites as typed: a host or a link ("https://example.com/x", "*.example.com"), as its host, each once. */
export function parseSites(value) {
  const hosts = words(value).map((word) => cleanHost(word.replace(/^[a-z][a-z0-9+.-]*:\/\//i, "").split(/[/?#:]/)[0].replace(/^\*\./, "")));
  return [...new Set(hosts.filter(Boolean))].slice(0, 500);
}

/** The extension of a file name, a path or a link's path, lower case; "" when it has none. */
export function extensionOf(name) {
  let path = String(name || "");
  try {
    path = new URL(path).pathname;
  } catch {
    // A name or a path.
  }
  const base = path.split(/[/\\]/).pop();
  const dot = base.lastIndexOf(".");
  const ext = dot < 0 ? "" : base.slice(dot + 1).toLowerCase();
  return /^[a-z0-9]{1,16}$/.test(ext) ? ext : "";
}

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

/**
 * Whether a browser download (a downloads.DownloadItem) goes to the app: capture is on; it is an http(s) download
 * the app can fetch again (not incognito, not blob: or data:, not the answer to a form sent with POST, `posted`)
 * that no extension started;
 * Alt was not held on the click that started it (`altClicked`); its file name, else its link, has one of the types;
 * it is at least the minimum size (an unknown size passes); and neither it nor the page it came from is on an
 * excluded site.
 */
export function shouldCapture(item, settings, { altClicked = false, posted = false } = {}) {
  const url = item.finalUrl || item.url;
  if (!settings.captureEnabled || item.incognito || item.byExtensionId || altClicked || posted) return false;
  if ((item.state && item.state !== "in_progress") || ![item.url, url].every((link) => /^https?:\/\//i.test(link || ""))) return false;
  if (!settings.captureTypes.includes(extensionOf(item.filename) || extensionOf(url))) return false;
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : 0;
  if (size && size < settings.captureMinMB * 1024 * 1024) return false;
  return ![url, item.referrer].some((link) => isBlocked(hostOf(link), settings.captureExcludedSites));
}
