// Pure helpers shared by the service worker and the popup: what counts as a media response, how request headers
// split into the fields the desktop app takes, what an m3u8 playlist holds, and how names and sizes are shown.
// No chrome.* in here, so node can test it.

/** Sites yt-dlp handles by page URL. Sniffing is skipped on tabs showing them: the page link is the better download. */
export const MEDIA_SITE_HOSTS = [
  "youtube.com",
  "youtu.be",
  "twitch.tv",
  "tiktok.com",
  "twitter.com",
  "x.com",
  "vimeo.com",
  "reddit.com",
  "instagram.com",
  "facebook.com",
  "dailymotion.com",
];

/** True when `hostname` is one of MEDIA_SITE_HOSTS or a subdomain of one. */
export function isMediaSiteHost(hostname) {
  const host = String(hostname || "").toLowerCase().replace(/\.$/, "");
  return MEDIA_SITE_HOSTS.some((site) => host === site || host.endsWith("." + site));
}

/** Content types that name their format outright. */
const TYPE_FORMATS = {
  "application/vnd.apple.mpegurl": "m3u8",
  "application/x-mpegurl": "m3u8",
  "audio/mpegurl": "m3u8",
  "audio/x-mpegurl": "m3u8",
  "video/mp4": "mp4",
  "video/webm": "webm",
  "video/ogg": "ogg",
  "video/x-flv": "flv",
  "video/quicktime": "mov",
  "video/x-msvideo": "avi",
  "video/x-ms-wmv": "wmv",
  "video/x-matroska": "mkv",
  "video/3gpp": "3gp",
  "video/x-f4v": "f4v",
  "audio/mpeg": "mp3",
  "audio/wav": "wav",
  "audio/ogg": "ogg",
};

/** Extensions that make a file a media item whatever its content type (octet-stream, text/plain, application/mp4…). */
const EXTENSION_FORMATS = ["m3u8", "m3u", "mp4", "webm", "avi", "ogg", "flv", "mkv", "3gp", "mp3"];

/** Formats that become items. Segments (.ts, .m4s) are left out: the playlist that lists them is the download. */
const ACCEPTED_FORMATS = new Set([
  "m3u8", "m3u", "mp4", "3gp", "flv", "mov", "avi", "wmv", "webm", "f4v", "mkv", "mp3", "wav", "ogg",
]);

/** The file name a Content-Disposition header gives, or "". */
function dispositionFilename(header) {
  const value = String(header || "");
  const encoded = /filename\*\s*=\s*[\w-]*'[^']*'([^;]+)/i.exec(value);
  if (encoded) {
    try {
      const name = decodeURIComponent(encoded[1].trim());
      if (name) return name;
    } catch {
      // A malformed escape falls back to the plain filename parameter.
    }
  }
  const plain = /filename\s*=\s*("[^"]*"|'[^']*'|[^;]*)/i.exec(value);
  return plain ? plain[1].replace(/["']/g, "").trim() : "";
}

/** The last segment of a URL path, percent-decoded where possible. */
function lastPathSegment(pathname) {
  const segment = pathname.split("/").pop() || "";
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

/** The total size a Content-Range header states ("bytes 0-99/12345" → 12345), or 0. */
function rangeTotal(header) {
  const match = /\/\s*(\d+)\s*$/.exec(String(header || ""));
  return match ? Number(match[1]) : 0;
}

/**
 * Decides whether a response is a video/audio file or an HLS playlist worth listing, following FetchV's rules:
 * the content type (lower-cased, parameters stripped) names the format; any other type (octet-stream, text/plain,
 * application/mp4…) counts only by the file name's extension; with no content type, a "media" request for
 * .mp4/.webm or an XHR for .m3u8 still counts. The file name is the Content-Disposition one, else the URL's last path segment.
 */
export function classifyResponse({ url, type, contentType, contentDisposition, contentLength, contentRange } = {}) {
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  const fileName = dispositionFilename(contentDisposition) || lastPathSegment(parsed.pathname);
  const dot = fileName.lastIndexOf(".");
  const ext = dot < 0 ? "" : fileName.slice(dot + 1).toLowerCase();

  let mime = String(contentType || "").toLowerCase().split(";").map((part) => part.trim()).find(Boolean) || "";
  if (!mime) {
    const path = parsed.pathname.toLowerCase();
    if (type === "media" && path.endsWith(".mp4")) mime = "video/mp4";
    else if (type === "media" && path.endsWith(".webm")) mime = "video/webm";
    else if (type === "xmlhttprequest" && path.endsWith(".m3u8")) mime = "application/vnd.apple.mpegurl";
    else return null;
  }

  let format = null;
  if (mime === "text/plain" && fileName.includes("master.txt")) format = "m3u8";
  else if (TYPE_FORMATS[mime]) format = mime === "video/mp4" && ext === "m4s" ? "m4s" : TYPE_FORMATS[mime];
  else if (EXTENSION_FORMATS.includes(ext)) format = ext;
  if (!ACCEPTED_FORMATS.has(format)) return null;

  const length = Number.parseInt(String(contentLength ?? ""), 10);
  return {
    format,
    kind: format === "m3u8" || format === "m3u" ? "hls" : "file",
    name: fileName || `video.${format}`,
    size: rangeTotal(contentRange) || (length > 0 ? length : 0),
  };
}

/** HLS always passes (its size is unknown); a file needs a known size inside the min/max KB bounds (0 = none). */
export function passesSizeFilter(item, { minSizeKB = 0, maxSizeKB = 0 } = {}) {
  if (item.kind === "hls") return true;
  const size = Number(item.size) || 0;
  const min = (Number(minSizeKB) || 0) * 1024;
  const max = (Number(maxSizeKB) || 0) * 1024;
  return size > 0 && (!min || size >= min) && (!max || size <= max);
}

/** Ad/tracker CDNs FetchV ignores, matched on the label before the top-level domain. */
const AD_LABELS = ["doppiocdn", "adtng", "afcdn", "sacdnssedge"];

/** True for hosts like "x.doppiocdn.com" whose second-level label is a known ad CDN. */
export function isAdHost(hostname) {
  const labels = String(hostname || "").toLowerCase().split(".");
  labels.pop();
  return labels.length > 0 && AD_LABELS.includes(labels[labels.length - 1]);
}

/** Request headers that describe this one request or the connection, not the client: the app sets its own. */
const DROPPED_HEADERS = new Set([
  "host", "connection", "content-length", "content-type", "range", "if-range", "if-match", "if-none-match",
  "if-modified-since", "if-unmodified-since", "accept", "accept-encoding", "upgrade-insecure-requests", "priority",
  "x-original-request-id",
]);

/**
 * Splits the headers the browser sent ([{name, value}]) into the fields the app's /add takes. Referer, User-Agent
 * and Cookie get their own fields; per-request and connection headers are dropped; the rest (Origin,
 * Authorization, sec-ch-ua*, X-*, …) is kept with its original casing.
 */
export function splitRequestHeaders(list) {
  const split = { referer: "", userAgent: "", cookies: "", headers: {} };
  for (const header of Array.isArray(list) ? list : []) {
    // webRequest gives binary values as binaryValue with no value: they cannot travel as text.
    if (!header || typeof header.name !== "string" || typeof header.value !== "string") continue;
    const lower = header.name.toLowerCase();
    if (lower === "referer") split.referer = header.value;
    else if (lower === "user-agent") split.userAgent = header.value;
    else if (lower === "cookie") split.cookies = header.value;
    else if (
      !DROPPED_HEADERS.has(lower) &&
      !lower.startsWith("proxy-") &&
      !lower.startsWith("sec-fetch-") &&
      // A CORS preflight's own headers, when its OPTIONS request was the one seen.
      !lower.startsWith("access-control-request-")
    ) {
      split.headers[header.name] = header.value;
    }
  }
  return split;
}

/** Attributes of an HLS tag ("BANDWIDTH=1,CODECS=\"a,b\"") as an object; quoted values may hold commas. */
function parseAttributes(text) {
  const attributes = {};
  for (const match of text.matchAll(/([A-Z0-9-]+)=(?:"([^"]*)"|([^,]*))/gi)) {
    attributes[match[1].toUpperCase()] = match[2] ?? match[3].trim();
  }
  return attributes;
}

/** `ref` resolved against `base`, or null when neither makes a URL. */
function resolveUrl(ref, base) {
  try {
    return new URL(ref, base).href;
  } catch {
    try {
      return new URL(ref).href;
    } catch {
      return null;
    }
  }
}

/**
 * Reads an m3u8 playlist. A master playlist gives its variants (best first) and audio renditions; a media playlist
 * gives its duration and segment count and whether it is live, AES-128 encrypted or DRM-protected. Null when the
 * text is not a playlist.
 */
export function parseM3u8(text, baseUrl) {
  if (typeof text !== "string") return null;
  // trim() also removes a byte order mark.
  const body = text.trim();
  if (!body.startsWith("#EXTM3U")) return null;
  const lines = body.split(/\r\n|\r|\n/).map((line) => line.trim()).filter(Boolean);

  if (lines.some((line) => line.startsWith("#EXT-X-STREAM-INF:"))) {
    const variants = [];
    const audio = [];
    lines.forEach((line, index) => {
      if (line.startsWith("#EXT-X-STREAM-INF:")) {
        const attributes = parseAttributes(line.slice("#EXT-X-STREAM-INF:".length));
        const uri = lines.slice(index + 1).find((next) => !next.startsWith("#") || next.startsWith("#EXT-X-STREAM-INF:"));
        const url = uri && !uri.startsWith("#") ? resolveUrl(uri, baseUrl) : null;
        if (!url) return;
        const resolution = /^\d+x\d+$/i.test(attributes.RESOLUTION || "") ? attributes.RESOLUTION : null;
        variants.push({
          url,
          bandwidth: Number.parseInt(attributes.BANDWIDTH, 10) || 0,
          resolution,
          height: resolution ? Number(resolution.split(/x/i)[1]) : null,
          codecs: attributes.CODECS || null,
          audioGroup: attributes.AUDIO || null,
        });
      } else if (line.startsWith("#EXT-X-MEDIA:")) {
        const attributes = parseAttributes(line.slice("#EXT-X-MEDIA:".length));
        const url = attributes.TYPE === "AUDIO" && attributes.URI ? resolveUrl(attributes.URI, baseUrl) : null;
        if (url) {
          audio.push({
            url,
            name: attributes.NAME || null,
            language: attributes.LANGUAGE || null,
            groupId: attributes["GROUP-ID"] || null,
          });
        }
      }
    });
    variants.sort((a, b) => b.bandwidth - a.bandwidth);
    const separateAudio = variants.some((variant) => variant.audioGroup && audio.some((rendition) => rendition.groupId === variant.audioGroup));
    return { kind: "master", variants, audio, separateAudio };
  }

  let duration = 0;
  let segments = 0;
  let ended = false;
  let vod = false;
  let encrypted = false;
  let drm = false;
  for (const line of lines) {
    if (line.startsWith("#EXTINF:")) {
      duration += Number.parseFloat(line.slice("#EXTINF:".length)) || 0;
    } else if (line === "#EXT-X-ENDLIST") {
      ended = true;
    } else if (line.startsWith("#EXT-X-PLAYLIST-TYPE:")) {
      vod = line.slice("#EXT-X-PLAYLIST-TYPE:".length).trim().toUpperCase() === "VOD";
    } else if (line.startsWith("#EXT-X-KEY:")) {
      const attributes = parseAttributes(line.slice("#EXT-X-KEY:".length));
      const method = (attributes.METHOD || "").toUpperCase();
      if (method === "AES-128") encrypted = true;
      if (method.startsWith("SAMPLE-AES") || (attributes.KEYFORMAT && attributes.KEYFORMAT !== "identity")) drm = true;
    } else if (!line.startsWith("#")) {
      segments += 1;
    }
  }
  return { kind: "media", duration: Math.round(duration * 1000) / 1000, segments, live: !ended && !vod, encrypted, drm };
}

/** A name safe on every OS: no path or reserved characters, no control characters, at most 150 characters. */
export function sanitizeFilename(name) {
  // Whitespace collapses first so tabs and newlines become spaces rather than vanishing as control characters.
  const cleaned = String(name ?? "")
    .replace(/\s+/g, " ")
    .replace(/[<>:"/\\|?*\u0000-\u001f\u007f]/g, "")
    .replace(/^[\s.]+|[\s.]+$/g, "");
  // Cut by code point so a surrogate pair is never split, then re-trim what the cut exposed.
  const short = Array.from(cleaned).slice(0, 150).join("").replace(/[\s.]+$/, "");
  return short || "video";
}

/** "<clean title>.<ext>"; HLS downloads are saved as mp4. */
export function suggestFilename(title, ext) {
  let extension = String(ext ?? "").replace(/^\./, "").toLowerCase();
  if (!extension || extension === "hls" || extension === "m3u8" || extension === "m3u") extension = "mp4";
  return `${sanitizeFilename(title)}.${extension}`;
}

/** "12.3 MB" style size; bytes below 1 KB are shown whole. */
export function formatBytes(n) {
  let value = Number(n);
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${Math.round(value)} B` : `${value.toFixed(1)} ${units[unit]}`;
}

/** "1:02:03" or "2:03"; "" when the duration is unknown. */
export function formatDuration(seconds) {
  const total = Math.round(Number(seconds));
  if (!Number.isFinite(total) || total < 0) return "";
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = String(total % 60).padStart(2, "0");
  return hours ? `${hours}:${String(minutes).padStart(2, "0")}:${secs}` : `${minutes}:${secs}`;
}
