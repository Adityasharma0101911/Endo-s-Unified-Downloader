import { test } from "node:test";
import assert from "node:assert/strict";
import { itemChips, itemMeta, itemNote, linkName, recordingView, variantLabel } from "../lib/view.js";

const master = (extra = {}) => ({
  kind: "master",
  variants: [
    { url: "https://a.example/1080.m3u8", height: 1080, resolution: "1920x1080", bandwidth: 5_200_000 },
    { url: "https://a.example/720.m3u8", height: 720, bandwidth: 0 },
  ],
  ...extra,
});

test("a variant is named by its height, else its resolution, with its bitrate", () => {
  assert.equal(variantLabel({ height: 1080, bandwidth: 5_200_000 }), "1080p · 5.2 Mbps");
  assert.equal(variantLabel({ resolution: "640x360" }), "640×360");
  assert.equal(variantLabel({}), "Variant");
});

test("chips name the kind first, then live, encryption and DRM", () => {
  assert.deepEqual(itemChips({ kind: "hls", hls: master() }), [["HLS", "accent"]]);
  assert.deepEqual(itemChips({ kind: "file", format: "webm" }), [["WEBM", "cyan"]]);
  assert.deepEqual(itemChips({ kind: "file" }), [["FILE", "cyan"]]);
  assert.deepEqual(itemChips({ kind: "dash" }), [["DASH", "accent"]]);
  assert.deepEqual(itemChips({ kind: "hls", inline: true, hls: { kind: "media", live: true, encrypted: true, drm: true } }), [
    ["HLS", "accent"],
    ["Inline", ""],
    ["LIVE", "red"],
    ["AES-128", "amber"],
    ["DRM", "red"],
  ]);
});

test("meta lists size, length (not of a live stream) and the best resolution", () => {
  assert.equal(itemMeta({ kind: "hls", size: 2048, hls: master({ duration: 125 }) }), "2.0 KB · 2:05 · 1920×1080");
  assert.equal(itemMeta({ kind: "hls", hls: { kind: "media", duration: 30, live: true } }), "");
  assert.equal(itemMeta({ kind: "hls", hls: { kind: "master", variants: [{ height: 480 }] } }), "480p");
  assert.equal(itemMeta({ kind: "file" }), "");
});

test("the note says why an item downloads as it does, DRM first", () => {
  assert.match(itemNote({ kind: "hls", hls: { drm: true, live: true } }), /DRM/);
  assert.match(itemNote({ kind: "hls", hls: master({ separateAudio: true }) }), /Separate audio/);
  assert.match(itemNote({ kind: "dash" }), /DASH/);
  assert.match(itemNote({ kind: "hls", hls: { kind: "media", live: true } }), /Live/);
  assert.equal(itemNote({ kind: "file" }), "");
});

test("a recording shows elapsed time, size, average speed and tracks", () => {
  const view = recordingView({ mode: "mse", bytes: 10 * 1024 * 1024, tracks: 2, title: "Clip", started: 1_000 }, 11_000);
  assert.deepEqual(view, {
    browser: false,
    kind: "Buffers",
    title: "Clip",
    percent: null,
    failed: false,
    info: "0:10 · 10.0 MB · 1.0 MB/s · 2 tracks",
  });
  assert.equal(recordingView({ mode: "msr", tracks: ["a"] }, 0).info, "0 B · 1 track");
  assert.equal(recordingView({ mode: "msr" }, 0).kind, "Playback");
  assert.equal(recordingView({ mode: "msr" }, 0).title, "Untitled");
});

test("a browser download shows its percentage, or why it failed", () => {
  const view = recordingView({ mode: "browser", bytes: 512, done: 3, total: 4, started: 0 }, 5_000);
  assert.equal(view.percent, 75);
  assert.equal(view.info, "75% · 512 B");
  assert.equal(recordingView({ mode: "browser", done: 9, total: 4 }, 0).percent, 100);
  assert.equal(recordingView({ mode: "browser", total: 0 }, 0).percent, null);
  const failed = recordingView({ mode: "browser", error: "HTTP 403", done: 1, total: 4 }, 0);
  assert.equal(failed.failed, true);
  assert.equal(failed.info, "Failed: HTTP 403");
  assert.equal(recordingView({ mode: "mse", error: "x" }, 0).failed, false);
});

test("a link row leads with the decoded file name, else the host", () => {
  assert.equal(linkName("https://a.example/files/My%20Clip.mp4?x=1"), "My Clip.mp4");
  assert.equal(linkName("https://a.example/dir/"), "dir");
  assert.equal(linkName("https://a.example/"), "a.example");
  assert.equal(linkName("https://a.example/bad%E0%A4%A"), "bad%E0%A4%A");
  assert.equal(linkName("not a url"), "not a url");
});
