import { test } from "node:test";
import assert from "node:assert/strict";
import { isMediaPage, mediaPayload, parseTime, pickerRows, pickQuality, rememberedChoice, sizeText, subtitleChoices, videoChoices } from "../lib/media.js";

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

test("the picker words /info's qualities as the app's card does, each with its pick and formats", () => {
  // The app's answer for the 4K sample (crates/hyperfetch-core/tests/data/media-4k60.json), trimmed.
  const opus = { id: "251", codec: "Opus", kbps: 129, size: { bytes: 10202210, exact: true }, language: null, original: false };
  const aac = { id: "140", codec: "AAC", kbps: 129, size: { bytes: 10271496, exact: true }, language: null, original: false };
  const av1 = { id: "399", codec: "AV1", fps: 60, hdr: false, size: { bytes: 124386876, exact: true }, has_audio: false };
  const vp9 = { id: "303", codec: "VP9", fps: 60, hdr: false, size: { bytes: 168736189, exact: true }, has_audio: false };
  const qualities = { heights: [{ p: 1080, options: [av1, vp9] }], audio: [opus, aac], languages: [] };
  assert.deepEqual(pickerRows(qualities), {
    heights: [
      {
        label: "1080p",
        options: [
          { value: "1080p:av1", label: "AV1 · 60 fps · 128.4 MiB", formats: "399+251" },
          { value: "1080p:vp9", label: "VP9 · 60 fps · 170.6 MiB", formats: "303+251" },
        ],
      },
    ],
    audio: [
      { value: "original", label: "Original · Opus 129 kbps · 9.7 MiB", formats: "251" },
      { value: "m4a", label: "M4A · AAC 129 kbps, not converted", formats: "140" },
      { value: "mp3", label: "MP3 · converted from Opus", formats: "251" },
    ],
  });
  // An estimate is "about"; an unknown size is left out; HDR and 30 fps or less as the app says them; dubbed audio.
  const de = { ...opus, id: "a-de", language: "de", original: true, size: { bytes: 1000000, exact: true } };
  const en = { ...opus, id: "a-en", language: "en", size: { bytes: 1100000, exact: false } };
  const sdr = { id: "v1", codec: "VP9", fps: 30, hdr: false, size: { bytes: 50000000, exact: false }, has_audio: false };
  const hdr = { id: "v2", codec: "VP9", fps: 30, hdr: true, size: null, has_audio: false };
  const dubbed = { heights: [{ p: 1080, options: [sdr, hdr] }], audio: [de, en], languages: ["de", "en"] };
  assert.deepEqual(pickerRows(dubbed).heights[0].options.map((o) => [o.value, o.label, o.formats]), [
    ["1080p:vp9", "VP9 · about 48.6 MiB", "v1+a-de"],
    ["1080p:vp9:hdr", "VP9 · HDR", "v2+a-de"],
  ]);
  assert.equal(pickerRows(dubbed, "en").heights[0].options[0].label, "VP9 · about 48.7 MiB");
  assert.equal(pickerRows(dubbed, "en").audio[1].label, "M4A · converted from Opus");
  assert.deepEqual(pickerRows({}), { heights: [], audio: [] });
  assert.equal(sizeText({ bytes: 900, exact: true }), "900 bytes");
  assert.equal(sizeText(null), "");
});

test("the picker's choice becomes /add's pick, with its formats for the one video", () => {
  const choice = { pick: "1080p:vp9@de", formats: "303+251", remember: true, container: "mkv", subtitles: "en", start: "", end: "", sponsorblock: "remove" };
  assert.deepEqual(mediaPayload(choice, video).media, { pick: "1080p:vp9@de", formats: "303+251", remember: true, container: "mkv", subtitles: "en", sponsorblock: "remove" });
  assert.deepEqual(mediaPayload({ ...choice, pick: "mp3", formats: "251", remember: false }, video).media, { pick: "mp3", formats: "251", sponsorblock: "remove" });
  const list = mediaPayload({ ...choice, playlist: true }, { ...video, playlist: { title: "L", count: 3 } }).media;
  assert.equal(list.formats, undefined, "a list's videos have formats of their own");
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
