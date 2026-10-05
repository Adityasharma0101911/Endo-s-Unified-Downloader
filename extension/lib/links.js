// Pure helpers of the popup's link grabber (no browser APIs here, so the tests can run them).

/** Most links one batch sends to the app, which refuses more. */
export const MAX_BATCH = 1000;
/** Most links taken from one page, so a huge page can't stall the popup. */
export const MAX_LINKS = 5000;

/** The type chips, each with the file extensions it stands for. */
export const LINK_KINDS = {
  Video: ["mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "3gp", "ogv", "ts", "m3u8", "mpd"],
  Audio: ["mp3", "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "wma", "aiff"],
  Images: ["jpg", "jpeg", "png", "gif", "webp", "avif", "bmp", "svg", "tif", "tiff", "heic"],
  Archives: ["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "cab"],
  Documents: ["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "csv", "epub", "mobi", "djvu"],
  Programs: ["exe", "msi", "msix", "dmg", "pkg", "deb", "rpm", "apk", "appimage", "jar"],
};

const KIND_OF = new Map(Object.entries(LINK_KINDS).flatMap(([kind, exts]) => exts.map((ext) => [ext, kind])));

/** The chip a link belongs to, by the extension of its path; "" for none. */
export function linkKind(url) {
  try {
    const name = new URL(url).pathname.split("/").pop();
    const dot = name.lastIndexOf(".");
    return dot < 0 ? "" : KIND_OF.get(name.slice(dot + 1).toLowerCase()) || "";
  } catch {
    return "";
  }
}

/**
 * The largest image of a srcset (by its `w` or `x` descriptor; none counts as 1x), resolved against `base`; "" when
 * there is none.
 */
export function largestSrcset(srcset, base) {
  let best = "";
  let bestSize = -1;
  // ponytail: candidates split at a comma followed by a space or by a descriptor, not the full HTML parser; a URL
  // holding ", " splits wrongly.
  for (const candidate of String(srcset || "").split(/,\s+|(?<=\d[wx]),/)) {
    const [url, descriptor = "1x"] = candidate.trim().replace(/,+$/, "").split(/\s+/);
    const size = parseFloat(descriptor);
    if (!url || !(size > bestSize)) continue;
    try {
      best = new URL(url, base).href;
      bestSize = size;
    } catch {
      // Not a URL: skipped.
    }
  }
  return best;
}

/**
 * The links of a page as the popup lists them: absolute http(s) only, without a #fragment, each once in the order
 * found, at most MAX_LINKS, with their host and chip.
 */
export function normalizeLinks(urls) {
  const seen = new Map();
  for (const raw of urls) {
    if (seen.size >= MAX_LINKS) break;
    let url;
    try {
      url = new URL(raw);
    } catch {
      continue;
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") continue;
    url.hash = "";
    if (!seen.has(url.href)) seen.set(url.href, { url: url.href, host: url.hostname.toLowerCase(), kind: linkKind(url.href) });
  }
  return [...seen.values()];
}

/**
 * The site a host belongs to, for the same-site filter: its last two labels, or three under a short second-level
 * label of a country (bbc.co.uk); an IP address is its own site.
 * ponytail: a guess, not the Public Suffix List; a short name under a ccTLD (t.co) counts its subdomain.
 */
export function siteOf(host) {
  const labels = String(host || "").toLowerCase().replace(/\.$/, "").split(".");
  if (labels.length <= 2 || /^\d+$/.test(labels.at(-1))) return labels.join(".");
  const count = labels.at(-1).length === 2 && labels.at(-2).length <= 3 ? 3 : 2;
  return labels.slice(-count).join(".");
}

/**
 * What the text box matches: `/regex/` (case-insensitive) or else plain text anywhere in the URL, any case. null
 * when the regex is not one.
 */
export function textMatcher(query) {
  const text = String(query || "").trim();
  const regex = /^\/(.+)\/[a-z]*$/i.exec(text);
  if (!regex) return (url) => url.toLowerCase().includes(text.toLowerCase());
  try {
    const pattern = new RegExp(regex[1], "i");
    return (url) => pattern.test(url);
  } catch {
    return null;
  }
}

/** Whether a link passes the filters: one of the chosen chips (none = all), the text box, the same-site toggle. */
export function linkFilter({ kinds = [], query = "", sameSite = false, pageHost = "" } = {}) {
  const wanted = new Set(kinds);
  const match = textMatcher(query) || (() => false);
  const site = siteOf(pageHost);
  return (link) => (wanted.size === 0 || wanted.has(link.kind)) && (!sameSite || siteOf(link.host) === site) && match(link.url);
}

/** A Cookie header of chrome.cookies cookies, leaving out any a header can't carry. */
export function cookieHeader(cookies) {
  return (cookies || [])
    .filter((cookie) => cookie?.name && !/[;\r\n\t]/.test(cookie.name + cookie.value))
    .map((cookie) => `${cookie.name}=${cookie.value}`)
    .join("; ");
}
