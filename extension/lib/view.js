// Pure text helpers of the popup's views (no browser APIs here, so the tests can run them).
import { formatBytes, formatDuration } from "./detect.js";

/** "1080p · 5.2 Mbps" for an HLS variant. */
export function variantLabel(v) {
  const parts = [v.height ? `${v.height}p` : v.resolution ? v.resolution.replace("x", "×") : "Variant"];
  if (v.bandwidth > 0) parts.push(`${(v.bandwidth / 1e6).toFixed(1)} Mbps`);
  return parts.join(" · ");
}

const hlsOf = (item) => (item.hls && typeof item.hls === "object" ? item.hls : null);
const masterOf = (item) => (item.hls?.kind === "master" && Array.isArray(item.hls.variants) ? item.hls : null);

/** A media item's chips as [text, tone]: its kind, then what sets it apart; tone is a palette colour or "". */
export function itemChips(item) {
  const hls = hlsOf(item);
  const stream = item.kind === "hls" || item.kind === "dash" || item.kind === "smooth";
  const chips = [[stream ? item.kind.toUpperCase() : String(item.format || "file").toUpperCase(), stream ? "accent" : "cyan"]];
  if (item.inline) chips.push(["Inline", ""]);
  if (hls?.live || item.live) chips.push(["LIVE", "red"]);
  if (hls?.encrypted) chips.push(["AES-128", "amber"]);
  if (isProtected(item)) chips.push(["DRM", "red"]);
  return chips;
}

/** Whether an item is DRM-protected: an HLS key system, or a DASH manifest's ContentProtection. */
export const isProtected = (item) => Boolean(hlsOf(item)?.drm || item.drm);

/**
 * The item a one-click download sends (Alt+Shift+D, the button over a video, every open tab): of those shown and not
 * protected, a stream before a file (an HLS master, then DASH, an HLS media playlist, Smooth Streaming), then the
 * largest, then the newest; null when there is none.
 */
export function bestItem(items) {
  const rank = (item) => (hlsOf(item)?.kind === "master" ? 4 : { dash: 3, hls: 2, smooth: 1 }[item.kind] || 0);
  const shown = items.filter((item) => item && !item.hidden && !isProtected(item));
  return shown.sort((a, b) => rank(b) - rank(a) || (b.size || 0) - (a.size || 0) || (b.time || 0) - (a.time || 0))[0] || null;
}

/** "12.3 MB · 4:05 · 1920×1080": what is known of an item's size, length and best resolution. */
export function itemMeta(item) {
  const hls = hlsOf(item);
  const best = masterOf(item)?.variants[0];
  const bits = [];
  if (item.size > 0) bits.push(formatBytes(item.size));
  if (hls?.duration > 0 && !hls.live) bits.push(formatDuration(hls.duration));
  if (best && (best.resolution || best.height)) bits.push(best.resolution ? best.resolution.replace("x", "×") : `${best.height}p`);
  return bits.join(" · ");
}

/** Why an item downloads the way it does, or "". */
export function itemNote(item) {
  const hls = hlsOf(item);
  if (isProtected(item)) return "Protected (DRM): it can't be downloaded.";
  if (masterOf(item)?.separateAudio) return "Separate audio: the downloader merges it with the chosen quality.";
  if (item.kind === "dash" && item.live) return "Live DASH stream: the downloader records it as it plays.";
  if (item.kind === "dash") return "DASH stream: the downloader fetches it and merges the audio.";
  if (item.kind === "smooth") return "Smooth Streaming: the downloader hands it to its media engine.";
  if (hls?.live) return "Live stream: the downloader records it as it plays.";
  return "";
}

/**
 * What a row of the Record tab shows for a recording or browser download `r` at `now` (ms): its kind, how far a
 * browser download got (done of total, as the page reports it), and elapsed time · size · average speed · tracks,
 * or why it failed.
 */
export function recordingView(r, now) {
  const browser = r.mode === "browser";
  const bytes = Math.max(0, Number(r.bytes) || 0);
  const tracks = Array.isArray(r.tracks) ? r.tracks.length : Number(r.tracks) || 0;
  const done = Math.max(0, Number(r.done) || 0);
  const total = Math.max(0, Number(r.total) || 0);
  const percent = browser && total > 0 ? Math.min(100, Math.floor((done / total) * 100)) : null;
  const failed = browser && typeof r.error === "string" && r.error !== "";
  const seconds = Number(r.started) > 0 ? Math.max(0, (now - r.started) / 1000) : 0;
  const bits = [];
  if (seconds > 0) bits.push(formatDuration(seconds));
  if (percent !== null) bits.push(`${percent}%`);
  bits.push(formatBytes(bytes));
  if (seconds >= 1 && bytes > 0) bits.push(`${formatBytes(bytes / seconds)}/s`);
  if (!browser) bits.push(`${tracks} ${tracks === 1 ? "track" : "tracks"}`);
  return {
    browser,
    kind: browser ? "Browser download" : r.mode === "msr" ? "Playback" : "Buffers",
    title: r.title || "Untitled",
    percent,
    failed,
    info: failed ? `Failed: ${r.error}` : bits.join(" · "),
  };
}

/** What the popup says of a tab whose video was given DRM keys by `system` (a key system's name, or ""). */
export function drmNote(system) {
  const names = [[/widevine/i, "Widevine"], [/playready/i, "PlayReady"], [/^com\.apple\.fps/i, "FairPlay"], [/clearkey/i, "ClearKey"]];
  const name = names.find(([pattern]) => pattern.test(system))?.[1];
  return `Protected (DRM${name ? `, ${name}` : ""}): this page's video can't be downloaded or recorded.`;
}

/** "just now", "5 min ago", "2 h ago" for a time `then` (ms) seen at `now`. */
export function timeAgo(then, now) {
  const minutes = Math.floor(Math.max(0, now - then) / 60000);
  return minutes < 1 ? "just now" : minutes < 60 ? `${minutes} min ago` : `${Math.floor(minutes / 60)} h ago`;
}

/**
 * What a row of the browser downloads log shows for entry `e` at `now`: where it went (chip), its name, host · size ·
 * when, why, and what it offers: "send" (to the app again: a link the app can fetch that it doesn't have) or "page"
 * (the page a download the page made came from), else null.
 */
export function logView(e, now) {
  const bits = [e.host || "made by the page"];
  if (e.size > 0) bits.push(formatBytes(e.size));
  bits.push(timeAgo(e.time, now));
  return {
    chip: e.sent ? ["App", "green"] : ["Browser", ""],
    title: e.name || "Unnamed download",
    info: bits.join(" · "),
    reason: e.reason || "",
    action: e.sent ? null : e.url && !e.posted ? "send" : !e.url && e.page ? "page" : null,
  };
}

/** A queue status of the app's (POST /queue) as the chip the popup shows: [text, tone]. */
const QUEUE_CHIPS = {
  downloading: ["Downloading", "accent"],
  pausing: ["Pausing", "amber"],
  queued: ["Waiting", ""],
  paused: ["Paused", "amber"],
  failed: ["Failed", "red"],
  done: ["Done", "green"],
  auth: ["Needs its sign-in", "amber"],
};

/**
 * What a row of the app's downloads shows for `d` (as POST /queue lists it): its name, status chip, how far it got
 * (percent, or null when the size is not known or it is done), done of total · speed (or why it failed), and the
 * actions the app takes for its status: "pause" a running one, "resume" a stopped one, "remove" one not running.
 */
export function queueView(d) {
  const done = Math.max(0, Number(d.done) || 0);
  const total = Math.max(0, Number(d.total) || 0);
  const percent = total > 0 && d.status !== "done" ? Math.min(100, Math.floor((done / total) * 100)) : null;
  const bits = [];
  if (percent !== null) bits.push(`${percent}%`);
  bits.push(total > 0 && d.status !== "done" ? `${formatBytes(done)} of ${formatBytes(total)}` : formatBytes(total || done));
  if (d.status === "downloading" && d.speed > 0) bits.push(`${formatBytes(d.speed)}/s`);
  const actions = {
    downloading: ["pause"],
    paused: ["resume", "remove"],
    failed: ["resume", "remove"],
    queued: ["remove"],
    done: ["remove"],
    auth: ["remove"],
  };
  return {
    title: d.name || `Download #${d.id}`,
    chip: QUEUE_CHIPS[d.status] || ["", ""],
    percent,
    info: d.status === "failed" && d.error ? d.error : bits.join(" · "),
    actions: actions[d.status] || [],
  };
}

/** The name a link list row leads with: the last part of the path, decoded, else the host; a magnet link's own name. */
export function linkName(url) {
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return String(url);
  }
  if (parsed.protocol === "magnet:") return parsed.searchParams.get("dn") || "Magnet link";
  const last = parsed.pathname.split("/").filter(Boolean).pop();
  if (!last) return parsed.hostname;
  try {
    return decodeURIComponent(last);
  } catch {
    return last;
  }
}
