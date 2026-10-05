import { test } from "node:test";
import assert from "node:assert/strict";
import { isMediaPage, mediaPayload, parseTime, pickQuality, rememberedChoice, subtitleChoices, videoChoices } from "../lib/media.js";

const video = {
  title: "A video",
  duration: 213.4,
  live: "none",
  heights: [4320, 2160, 1080, 720, 360, 240],
  hdr: [2160],
  fps60: [1080, 720],
  audio: true,
  subtitles: ["en", "es", "live_chat"],
  auto_subtitles: ["en", "de"],
  playlist: null,
  sponsorblock: true,
};

test("media pages: a YouTube video, Short, live, list or channel, and any other media site past its home page", () => {
  for (const url of [
    "https://www.youtube.com/watch?v=abc",
    "https://m.youtube.com/watch?v=abc&list=PL1",
    "https://music.youtube.com/watch?v=abc",
    "https://www.youtube.com/shorts/abc",
    "https://www.youtube.com/live/abc",
    "https://www.youtube.com/playlist?list=PL1",
    "https://www.youtube.com/@someone/videos",
    "https://www.youtube.com/channel/UC1",
    "https://youtu.be/abc",
    "https://www.twitch.tv/videos/1",
    "http://vimeo.com/1",
    "https://soundcloud.com/a/b",
    "https://www.bilibili.com/video/BV1",
  ]) {
    assert.ok(isMediaPage(url), url);
  }
  for (const url of [
    "https://www.youtube.com/",
    "https://www.youtube.com/watch",
    "https://www.youtube.com/results?search_query=x",
    "https://www.youtube.com/feed/subscriptions",
    "https://youtu.be/",
    "https://www.twitch.tv/",
    "https://example.com/watch?v=abc",
    "ftp://youtube.com/watch?v=abc",
    "not a url",
  ]) {
    assert.ok(!isMediaPage(url), url);
  }
});

test("clip times read as seconds, h:mm:ss or m:ss; anything else is NaN", () => {
  assert.equal(parseTime("90"), 90);
  assert.equal(parseTime(" 1:30 "), 90);
  assert.equal(parseTime("1:02:03"), 3723);
  assert.equal(parseTime("0:01.5"), 1.5);
  for (const bad of ["", "a", "1:60", "1:5:60", "1:2:3:4", "-1", "1.5:00", "1:"]) assert.ok(Number.isNaN(parseTime(bad)), bad);
});

test("video qualities: Best, then the heights /add takes, tallest first, with 60 and HDR marks", () => {
  assert.deepEqual(videoChoices(video), [
    { value: "best", label: "Best", marks: [] },
    { value: "2160", label: "4K", marks: ["HDR"] },
    { value: "1080", label: "1080p", marks: ["60"] },
    { value: "720", label: "720p", marks: ["60"] },
    { value: "360", label: "360p", marks: [] },
  ]);
  assert.deepEqual(videoChoices({}), [{ value: "best", label: "Best", marks: [] }]);
});

test("a remembered quality is kept when offered, else the tallest under it, else Best", () => {
  const offered = ["best", "1080", "720", "audio-m4a", "audio-mp3"];
  assert.equal(pickQuality("720", offered), "720");
  assert.equal(pickQuality("audio-mp3", offered), "audio-mp3");
  assert.equal(pickQuality("1440", offered), "1080");
  assert.equal(pickQuality("360", offered), "best");
  assert.equal(pickQuality("audio-mp3", ["best"]), "best");
});

test("subtitles: uploaded languages, then automatic ones they lack; no live chat", () => {
  assert.deepEqual(subtitleChoices(video), [
    { code: "en", auto: false },
    { code: "es", auto: false },
    { code: "de", auto: true },
  ]);
  assert.deepEqual(subtitleChoices({}), []);
});

test("a remembered choice keeps only what /add takes", () => {
  assert.deepEqual(rememberedChoice({ quality: "720", container: "mkv", subtitles: "en,es" }), { quality: "720", container: "mkv", subtitles: "en,es" });
  assert.deepEqual(rememberedChoice({ quality: "4k", container: "avi", subtitles: "en;--exec" }), { quality: "best", container: "mp4", subtitles: "" });
  assert.deepEqual(rememberedChoice("junk"), { quality: "best", container: "mp4", subtitles: "" });
});

test("the card's choice becomes /add's media", () => {
  const choice = { quality: "1080", container: "mkv", subtitles: "en", start: "0:30", end: "1:00", sponsorblock: "remove", playlist: false, fromStart: false };
  assert.deepEqual(mediaPayload(choice, video).media, { quality: "1080", container: "mkv", subtitles: "en", sections: [[30, 60]], sponsorblock: "remove" });
  // Audio drops the container and subtitles; an empty clip sends nothing.
  assert.deepEqual(mediaPayload({ ...choice, quality: "audio-mp3", start: "", end: "" }, video).media, { quality: "audio-mp3", sponsorblock: "remove" });
  // None and Off are sent, over the app's Settings.
  const none = mediaPayload({ ...choice, subtitles: "", start: "", end: "", sponsorblock: "off" }, video).media;
  assert.deepEqual(none, { quality: "1080", container: "mkv", subtitles: "", sponsorblock: "off" });
  // An empty side is the video's start or end; past the end is cut to it.
  assert.deepEqual(mediaPayload({ ...choice, start: "", end: "1:00" }, video).media.sections, [[0, 60]]);
  assert.deepEqual(mediaPayload({ ...choice, start: "3:00", end: "" }, video).media.sections, [[180, 213.4]]);
  assert.deepEqual(mediaPayload({ ...choice, start: "3:00", end: "9:00" }, video).media.sections, [[180, 213.4]]);
  assert.deepEqual(mediaPayload({ ...choice, start: "1:00", end: "" }, { duration: null }).media.sections, [[60, 86400]]);
  assert.match(mediaPayload({ ...choice, start: "1:00", end: "0:30" }, video).error, /start before it ends/);
  assert.match(mediaPayload({ ...choice, start: "4:00", end: "" }, video).error, /start before it ends/);
  assert.match(mediaPayload({ ...choice, start: "1:2:3:4" }, video).error, /1:30/);
  // SponsorBlock only where /info offers it; the list only when there is one.
  assert.deepEqual(mediaPayload({ ...choice, start: "", end: "", playlist: true }, { ...video, sponsorblock: false }).media, { quality: "1080", container: "mkv", subtitles: "en" });
  const list = mediaPayload({ ...choice, playlist: true }, { ...video, playlist: { title: "L", count: 3 } }).media;
  assert.equal(list.playlist, true);
  assert.equal(list.sections, undefined, "a list takes no clip");
  // Live: from its start when asked, never a clip.
  const live = mediaPayload({ ...choice, fromStart: true }, { ...video, live: "live", duration: null }).media;
  assert.equal(live.live_from_start, true);
  assert.equal(live.sections, undefined);
  assert.equal(mediaPayload({ ...choice, fromStart: true }, { ...video, live: "upcoming" }).media.live_from_start, undefined);
  assert.deepEqual(mediaPayload({ quality: "nonsense" }).media, { quality: "best" });
});
