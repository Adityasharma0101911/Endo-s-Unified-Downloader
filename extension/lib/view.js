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
  const stream = item.kind === "hls" || item.kind === "dash";
  const chips = [[stream ? item.kind.toUpperCase() : String(item.format || "file").toUpperCase(), stream ? "accent" : "cyan"]];
  if (item.inline) chips.push(["Inline", ""]);
  if (hls?.live) chips.push(["LIVE", "red"]);
  if (hls?.encrypted) chips.push(["AES-128", "amber"]);
  if (hls?.drm) chips.push(["DRM", "red"]);
  return chips;
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
  if (hls?.drm) return "DRM-protected (SAMPLE-AES or a key system): it can't be downloaded.";
  if (masterOf(item)?.separateAudio) return "Separate audio: the downloader merges it with the chosen quality.";
  if (item.kind === "dash") return "DASH stream: the downloader fetches it and merges the audio.";
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

/** The name a link list row leads with: the last part of the path, decoded, else the host. */
export function linkName(url) {
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return String(url);
  }
  const last = parsed.pathname.split("/").filter(Boolean).pop();
  if (!last) return parsed.hostname;
  try {
    return decodeURIComponent(last);
  } catch {
    return last;
  }
}
