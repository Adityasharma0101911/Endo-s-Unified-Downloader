// Pure helpers of "Send browser downloads to the app": which of the browser's downloads go to the app, and the
// settings' file types and sites as typed. No chrome.* in here, so node can test it.
import { cleanHost, isBlocked } from "./detect.js";

/** The file types sent to the app until 1.9 (see upgradedTypes). */
const FIRST_TYPES = [
  "zip", "7z", "rar", "tar", "gz", "xz", "bz2", "zst", "iso", "img", "dmg", "pkg", "exe", "msi", "msix", "appimage",
  "deb", "rpm", "apk", "mp4", "mkv", "webm", "avi", "mov", "mp3", "flac", "wav", "m4a", "pdf", "epub",
];
/** The file types sent to the app until the user changes them. Not `torrent`: any page could then start a swarm download. */
export const CAPTURE_TYPES = [
  ...FIRST_TYPES,
  "tgz", "lz", "lzma", "cab", "wim", "esd", "dat", "bin", "jar", "ipa", "appx", "appxbundle", "msixbundle", "xapk",
  "vhd", "vhdx", "vmdk", "vdi", "qcow2", "ova", "safetensors", "gguf", "ckpt", "pth", "onnx", "m4v", "ts", "m2ts",
  "wmv", "flv", "ogg", "opus", "aac", "wma", "mobi", "azw3", "djvu", "cbz", "cbr", "psd", "ttf", "otf",
];
const MB = 1024 * 1024;

/**
 * A stored list of types, with the new defaults when it is still exactly the list of before 1.9: a list the user
 * edited is kept as it is.
 */
export function upgradedTypes(types) {
  const same = types.length === FIRST_TYPES.length && FIRST_TYPES.every((type) => types.includes(type));
  return same ? CAPTURE_TYPES : types;
}
/** How long after an Alt-click a download stays in the browser. */
export const ALT_WINDOW_MS = 5000;

/**
 * Whether a download whose page is `referrer` came within ALT_WINDOW_MS of an Alt-click (`clicks`: when the last one
 * on each page origin happened), so it stays in the browser. A download has no tab, so its page's origin stands for
 * it; one whose page is not known (no referrer) goes by an Alt-click on any page.
 */
export function altClicked(clicks, referrer, now = Date.now()) {
  const origin = originOf(referrer);
  const times = origin ? [clicks.get(origin)] : [...clicks.values()];
  return times.some((at) => at !== undefined && now - at < ALT_WINDOW_MS);
}

/** The origin of a page's link ("https://example.com"); "" when it has none. */
export function originOf(url) {
  try {
    const { origin } = new URL(url);
    return origin === "null" ? "" : origin;
  } catch {
    return "";
  }
}

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

/**
 * The settings in the text of an exported settings file (the popup's Export), or null when it is not one: a JSON
 * object with at least one of `keys`, whose other fields are left out. The background then checks each value
 * (normalizeSettings), so a wrong one falls back to its default.
 */
export function settingsOfFile(text, keys) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  const known = Object.fromEntries(Object.entries(parsed).filter(([key]) => keys.includes(key)));
  return Object.keys(known).length > 0 ? known : null;
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

/** Whether `host` is one of the sites of `sites` or under one. */
const onList = (host, sites = []) => Boolean(host) && sites.some((site) => host === site || host.endsWith("." + site));

/**
 * Whether a browser download (a downloads.DownloadItem) goes to the app, and why, as {capture, reason}. It stays in
 * the browser when capture is off; in a private window; when an extension started it; after an Alt-click
 * (`altClicked`); once it is no longer running; when the app can't fetch it again (not http(s), such as blob: and
 * data:, or the answer to a form sent with POST, `posted`); or when it or the page it came from is on an excluded
 * site. Past those, a download from an "always" site goes; one under the minimum size stays (an unknown size
 * passes); and the rest goes with "Everything" on, else when it is at least `captureAnyOverMB` (0: off), else when
 * its file name, else its link, has one of the types.
 */
export function captureDecision(item, settings, { altClicked = false, posted = false } = {}) {
  const url = item.finalUrl || item.url;
  const stay = (reason) => ({ capture: false, reason });
  if (!settings.captureEnabled) return stay("Sending browser downloads to the app is off");
  if (item.incognito) return stay("Private window");
  if (item.byExtensionId) return stay("Started by an extension");
  if (altClicked) return stay("Alt was held on the click");
  if (item.state && item.state !== "in_progress") return stay("Finished before it could be handed over");
  if (![item.url, url].every((link) => /^https?:\/\//i.test(link || ""))) {
    return stay(/^(blob|data):/i.test(url || "") ? "Made by the page (blob: or data:): only the browser has it" : "Not an http(s) link");
  }
  if (posted) return stay("The answer to a form sent with POST: the app can only ask for a link again");
  const hosts = [url, item.referrer].map(hostOf).filter(Boolean);
  const excluded = hosts.find((host) => isBlocked(host, settings.captureExcludedSites));
  if (excluded) return stay(`Never from ${excluded}`);
  const always = hosts.find((host) => onList(host, settings.captureAlwaysSites));
  if (always) return { capture: true, reason: `Always from ${always}` };
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : 0;
  if (size && size < settings.captureMinMB * MB) return stay(`Under the minimum size (${settings.captureMinMB} MB)`);
  if (settings.captureAll) return { capture: true, reason: "Everything goes to the app" };
  const over = settings.captureAnyOverMB > 0 ? settings.captureAnyOverMB : 0;
  if (size && over && size >= over * MB) return { capture: true, reason: `${over} MB or more` };
  const type = extensionOf(item.filename) || extensionOf(url);
  if (type && settings.captureTypes.includes(type)) return { capture: true, reason: `.${type} is one of the file types` };
  const why = type ? `.${type} isn't one of the file types` : "No file type in its name or link";
  return stay(!over ? why : size ? `${why}, and it's under ${over} MB` : `${why}, and its size isn't known`);
}

/** Whether a browser download goes to the app (see captureDecision). */
export function shouldCapture(item, settings, flags) {
  return captureDecision(item, settings, flags).capture;
}
