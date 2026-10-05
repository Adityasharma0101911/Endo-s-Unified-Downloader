// Pure helpers of the media card (popup), the YouTube button and the context menu: which pages are media pages,
// clip times, the qualities /info's answer allows and the `media` choice POST /add takes. No chrome.* in here, so
// node can test it.
import { formatDuration, isMediaSiteHost } from "./detect.js";

/** The qualities /add's `media` takes (crates/hyperfetch-gui/src/ipc.rs QUALITIES). */
export const QUALITIES = ["best", "2160", "1440", "1080", "720", "480", "360", "audio-m4a", "audio-mp3"];
export const CONTAINERS = ["mp4", "mkv", "webm"];
/** The latest second a clip may end at: the app takes 24 hours. */
const MAX_END = 86400;

/**
 * Whether `url` is a page the media card is for: a media site (see isMediaSiteHost) past its home page; on YouTube a
 * video, Short, live, list or channel page, not the home page, search or a feed.
 * ponytail: paths, not yt-dlp's extractor list; a site page with no video in it gets the card's error state.
 */
export function isMediaPage(url) {
  let u;
  try {
    u = new URL(url);
  } catch {
    return false;
  }
  const host = u.hostname.toLowerCase();
  if (!/^https?:$/.test(u.protocol) || !isMediaSiteHost(host)) return false;
  if (host === "youtu.be") return u.pathname.length > 1;
  if (/(^|\.)youtube\.com$/.test(host)) {
    const path = u.pathname;
    return (
      (path === "/watch" && u.searchParams.has("v")) ||
      (path === "/playlist" && u.searchParams.has("list")) ||
      /^\/((shorts|live|channel|c|user)\/|@)[^/]/.test(path)
    );
  }
  return u.pathname.replace(/\/+$/, "") !== "";
}

/** Seconds in "90", "1:30" or "1:02:03" (the last part may have decimals; the later ones are under 60); else NaN. */
export function parseTime(text) {
  const parts = String(text ?? "").trim().split(":");
  const valid = parts.length <= 3 && parts.every((part, i) => (i === parts.length - 1 ? /^\d+(\.\d+)?$/ : /^\d+$/).test(part));
  if (!valid || parts.slice(1).some((part) => Number(part) >= 60)) return NaN;
  return parts.reduce((total, part) => total * 60 + Number(part), 0);
}

/**
 * The video qualities to offer for /info's answer: Best, then each height /add takes that the video has, tallest
 * first, with its marks (60 fps, HDR). Heights /add can't name (4320, 240…) are left to Best.
 */
export function videoChoices(info) {
  const has = (list, h) => Array.isArray(list) && list.includes(h);
  const heights = QUALITIES.filter((q) => /^\d+$/.test(q) && has(info?.heights, Number(q))).map(Number);
  return [
    { value: "best", label: "Best", marks: [] },
    ...heights.map((h) => ({
      value: String(h),
      label: h === 2160 ? "4K" : `${h}p`,
      marks: [has(info.fps60, h) && "60", has(info.hdr, h) && "HDR"].filter(Boolean),
    })),
  ];
}

/** The remembered quality when it is offered, else the tallest offered under it, else "best". */
export function pickQuality(remembered, offered) {
  if (offered.includes(remembered)) return remembered;
  return offered.find((q) => /^\d+$/.test(q) && Number(q) < Number(remembered)) || "best";
}

/**
 * The subtitle languages /info offers, each once: the uploaded ones, then the automatic ones they lack (`auto`).
 * yt-dlp's live_chat and other names that are no language code are left out.
 */
export function subtitleChoices(info) {
  const codes = (list) => (Array.isArray(list) ? list : []).filter((c) => typeof c === "string" && /^[a-z]{2,3}(-[A-Za-z0-9]{1,8})*$/.test(c));
  const own = new Set(codes(info?.subtitles));
  const auto = new Set(codes(info?.auto_subtitles).filter((code) => !own.has(code)));
  return [...[...own].map((code) => ({ code, auto: false })), ...[...auto].map((code) => ({ code, auto: true }))];
}

/** A remembered choice (storage holds anything) as {quality, container, subtitles}, with the defaults for what isn't one. */
export function rememberedChoice(raw) {
  const r = raw && typeof raw === "object" ? raw : {};
  return {
    quality: QUALITIES.includes(r.quality) ? r.quality : "best",
    container: CONTAINERS.includes(r.container) ? r.container : "mp4",
    subtitles: typeof r.subtitles === "string" && /^[A-Za-z0-9,_-]{1,200}$/.test(r.subtitles) ? r.subtitles : "",
  };
}

/**
 * The `media` POST /add takes for a choice ({quality, container, subtitles, start, end, sponsorblock, playlist,
 * fromStart}) on what /info said, or {error} when the clip is not one: the quality; for video the container and
 * subtitles ("" for None); a clip from start to end (m:ss; left empty, the video's start or end; past the end, cut to
 * it); SponsorBlock where /info offers it ("off" too); the whole list; a live stream from its start. None and Off are
 * sent as such: left out, the app's Settings would apply.
 */
export function mediaPayload(choice, info = {}) {
  const media = { quality: QUALITIES.includes(choice.quality) ? choice.quality : "best" };
  if (!media.quality.startsWith("audio-")) {
    if (CONTAINERS.includes(choice.container)) media.container = choice.container;
    if (typeof choice.subtitles === "string") media.subtitles = choice.subtitles;
  }
  const [from, to] = [String(choice.start ?? "").trim(), String(choice.end ?? "").trim()];
  const live = info.live === "live" || info.live === "upcoming";
  if (!live && !choice.playlist && (from || to)) {
    const length = Math.min(Number(info.duration) > 0 ? info.duration : MAX_END, MAX_END);
    const start = from ? parseTime(from) : 0;
    const end = to ? Math.min(parseTime(to), length) : length;
    if (Number.isNaN(start) || Number.isNaN(end)) return { error: "Enter the clip's times as 1:30 or 1:02:03." };
    if (!(start < end)) return { error: `The clip must start before it ends (the video is ${formatDuration(length)} long).` };
    media.sections = [[start, end]];
  }
  if (info.sponsorblock && ["off", "remove", "mark"].includes(choice.sponsorblock)) media.sponsorblock = choice.sponsorblock;
  if (info.playlist && choice.playlist) media.playlist = true;
  if (info.live === "live" && choice.fromStart) media.live_from_start = true;
  return { media };
}
