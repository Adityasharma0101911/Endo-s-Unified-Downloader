import { test } from "node:test";
import assert from "node:assert/strict";
import {
  classifyResponse,
  formatBytes,
  formatDuration,
  isAdHost,
  isMediaSiteHost,
  parseM3u8,
  passesSizeFilter,
  sanitizeFilename,
  splitRequestHeaders,
  suggestFilename,
} from "../lib/detect.js";

test("an m3u8 is found by its content type, whatever the URL says", () => {
  const found = classifyResponse({ url: "https://cdn.example/play?id=1", type: "xmlhttprequest", contentType: "application/vnd.apple.mpegURL; charset=utf-8" });
  assert.deepEqual(found, { format: "m3u8", kind: "hls", name: "play", size: 0 });
  assert.equal(classifyResponse({ url: "https://a.example/x", contentType: "audio/x-mpegurl" }).kind, "hls");
});

test("an m3u8 with no content type is found by extension on an XHR only", () => {
  assert.equal(classifyResponse({ url: "https://a.example/v/index.m3u8?t=1", type: "xmlhttprequest" }).format, "m3u8");
  assert.equal(classifyResponse({ url: "https://a.example/v/index.m3u8", type: "other" }), null);
  assert.equal(classifyResponse({ url: "https://a.example/v.mp4", type: "media" }).format, "mp4");
  assert.equal(classifyResponse({ url: "https://a.example/v.webm", type: "media" }).format, "webm");
  assert.equal(classifyResponse({ url: "https://a.example/v.mp4", type: "xmlhttprequest" }), null);
});

test("octet-stream, text/plain and other types count only by a media extension", () => {
  const url = "https://a.example/files/clip.mkv";
  assert.equal(classifyResponse({ url, contentType: "application/octet-stream", contentLength: "9000" }).format, "mkv");
  assert.equal(classifyResponse({ url, contentType: "binary/octet-stream" }).format, "mkv");
  assert.equal(classifyResponse({ url: "https://a.example/list.m3u8", contentType: "text/plain" }).kind, "hls");
  assert.equal(classifyResponse({ url: "https://a.example/blob.bin", contentType: "application/octet-stream" }), null);
  assert.equal(classifyResponse({ url: "https://a.example/page.html", contentType: "text/html" }), null);
  assert.equal(classifyResponse({ url: "https://a.example/v/clip.mp4", contentType: "application/mp4" }).format, "mp4");
  assert.equal(classifyResponse({ url: "https://a.example/v/clip.json", contentType: "application/json" }), null);
  const disposed = classifyResponse({ url: "https://a.example/dl?id=4", contentType: "application/octet-stream", contentDisposition: 'attachment; filename="Holiday Film.mp4"' });
  assert.equal(disposed.format, "mp4");
  assert.equal(disposed.name, "Holiday Film.mp4");
});

test("segments are never items", () => {
  assert.equal(classifyResponse({ url: "https://a.example/seg-1.m4s", contentType: "video/mp4", contentLength: "999999" }), null);
  assert.equal(classifyResponse({ url: "https://a.example/seg-1.ts", contentType: "video/mp2t" }), null);
  assert.equal(classifyResponse({ url: "https://a.example/seg-1.ts", contentType: "application/octet-stream" }), null);
});

test("size is the Content-Range total, else Content-Length", () => {
  const ranged = classifyResponse({ url: "https://a.example/v.mp4", contentType: "video/mp4", contentLength: "100", contentRange: "bytes 0-99/52428800" });
  assert.equal(ranged.size, 52428800);
  assert.equal(classifyResponse({ url: "https://a.example/v.mp4", contentType: "video/mp4", contentLength: "2048" }).size, 2048);
  assert.equal(classifyResponse({ url: "https://a.example/v.mp4", contentType: "video/mp4" }).size, 0);
});

test("master.txt served as text/plain is a playlist; a nameless URL gets a default name", () => {
  assert.equal(classifyResponse({ url: "https://a.example/hls/master.txt", contentType: "text/plain" }).format, "m3u8");
  assert.equal(classifyResponse({ url: "https://a.example/", contentType: "video/webm" }).name, "video.webm");
});

test("passesSizeFilter: HLS always, files need a size inside the bounds", () => {
  const limits = { minSizeKB: 500, maxSizeKB: 0 };
  assert.ok(passesSizeFilter({ kind: "hls", size: 0 }, limits));
  assert.ok(!passesSizeFilter({ kind: "file", size: 0 }, { minSizeKB: 0, maxSizeKB: 0 }));
  assert.ok(!passesSizeFilter({ kind: "file", size: 499 * 1024 }, limits));
  assert.ok(passesSizeFilter({ kind: "file", size: 500 * 1024 }, limits));
  assert.ok(!passesSizeFilter({ kind: "file", size: 2048 * 1024 }, { minSizeKB: 0, maxSizeKB: 1024 }));
});

test("isAdHost matches the second-level label; isMediaSiteHost matches subdomains", () => {
  assert.ok(isAdHost("edge-3.doppiocdn.com"));
  assert.ok(isAdHost("adtng.net"));
  assert.ok(!isAdHost("doppiocdn.example.com"));
  assert.ok(!isAdHost("localhost"));
  assert.ok(isMediaSiteHost("www.youtube.com"));
  assert.ok(isMediaSiteHost("x.com"));
  assert.ok(!isMediaSiteHost("notyoutube.com"));
});

test("splitRequestHeaders routes, drops and keeps headers", () => {
  const split = splitRequestHeaders([
    { name: "Referer", value: "https://page.example/" },
    { name: "User-Agent", value: "UA" },
    { name: "Cookie", value: "a=1; b=2" },
    { name: "Origin", value: "https://page.example" },
    { name: "Authorization", value: "Bearer x" },
    { name: "sec-ch-ua", value: '"Chromium"' },
    { name: "X-Custom", value: "y" },
    { name: "Accept-Language", value: "en" },
    { name: "Range", value: "bytes=0-" },
    { name: "Accept", value: "*/*" },
    { name: "Sec-Fetch-Mode", value: "cors" },
    { name: "Proxy-Authorization", value: "z" },
    { name: "Host", value: "cdn" },
    { name: "Access-Control-Request-Method", value: "GET" },
    { name: "Binary", binaryValue: [1] },
  ]);
  assert.equal(split.referer, "https://page.example/");
  assert.equal(split.userAgent, "UA");
  assert.equal(split.cookies, "a=1; b=2");
  assert.deepEqual(split.headers, {
    Origin: "https://page.example",
    Authorization: "Bearer x",
    "sec-ch-ua": '"Chromium"',
    "X-Custom": "y",
    "Accept-Language": "en",
  });
  assert.deepEqual(splitRequestHeaders(undefined), { referer: "", userAgent: "", cookies: "", headers: {} });
});

test("parseM3u8 reads a master playlist: quoted commas, relative URLs, best variant first", () => {
  const text = [
    "#EXTM3U",
    '#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2"',
    "low/index.m3u8",
    '#EXT-X-STREAM-INF:BANDWIDTH=5200000,RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2"',
    "/abs/high.m3u8?token=a,b",
  ].join("\n");
  const master = parseM3u8(text, "https://cdn.example/v/master.m3u8");
  assert.equal(master.kind, "master");
  assert.equal(master.separateAudio, false);
  assert.deepEqual(master.variants.map((v) => [v.url, v.height, v.codecs]), [
    ["https://cdn.example/abs/high.m3u8?token=a,b", 1080, "avc1.640028,mp4a.40.2"],
    ["https://cdn.example/v/low/index.m3u8", 360, "avc1.4d401e,mp4a.40.2"],
  ]);
  assert.equal(master.variants[0].resolution, "1920x1080");
  assert.equal(master.variants[0].bandwidth, 5200000);
});

test("parseM3u8 flags audio delivered as a separate rendition", () => {
  const text = [
    "#EXTM3U",
    '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English, main",LANGUAGE="en",URI="audio/en.m3u8"',
    '#EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO="aud"',
    "video.m3u8",
  ].join("\r\n");
  const master = parseM3u8(text, "https://cdn.example/v/master.m3u8");
  assert.equal(master.separateAudio, true);
  assert.deepEqual(master.audio, [{ url: "https://cdn.example/v/audio/en.m3u8", name: "English, main", language: "en", groupId: "aud" }]);
  assert.equal(master.variants[0].audioGroup, "aud");
  assert.equal(master.variants[0].resolution, null);
});

test("parseM3u8 reads media playlists: VOD, live, AES-128, DRM", () => {
  const vod = parseM3u8("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\na.ts\n#EXTINF:5.5,\nb.ts\n#EXT-X-ENDLIST\n", "https://a.example/");
  assert.deepEqual(vod, { kind: "media", duration: 15.5, segments: 2, live: false, encrypted: false, drm: false });
  assert.equal(parseM3u8("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4,\na.ts", "").live, false);
  assert.equal(parseM3u8("#EXTM3U\n#EXTINF:4,\na.ts", "").live, true);
  const aes = parseM3u8('#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI="key.bin"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST', "");
  assert.equal(aes.encrypted, true);
  assert.equal(aes.drm, false);
  assert.equal(parseM3u8('#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI="skd://x"\n#EXTINF:4,\na.ts', "").drm, true);
  assert.equal(parseM3u8('#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI="k",KEYFORMAT="com.apple.streamingkeydelivery"\na.ts', "").drm, true);
  assert.equal(parseM3u8('#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI="k",KEYFORMAT="identity"\na.ts', "").drm, false);
});

test("parseM3u8 skips a BOM and leading whitespace, and rejects non-playlists", () => {
  assert.equal(parseM3u8("﻿  \n#EXTM3U\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST", "https://a.example/").kind, "media");
  assert.equal(parseM3u8("<html>#EXTM3U", "https://a.example/"), null);
  assert.equal(parseM3u8(null, "https://a.example/"), null);
});

test("sanitizeFilename and suggestFilename make safe names", () => {
  assert.equal(sanitizeFilename('  a<b>:c"/d\\e|f?g*h\u0001  '), "abcdefgh");
  assert.equal(sanitizeFilename("..hidden.."), "hidden");
  assert.equal(sanitizeFilename("a   b\tc"), "a b c");
  assert.equal(sanitizeFilename(""), "video");
  assert.equal(sanitizeFilename("x".repeat(200)).length, 150);
  assert.equal(suggestFilename("My: Show", "hls"), "My Show.mp4");
  assert.equal(suggestFilename("Clip", ".webm"), "Clip.webm");
  assert.equal(suggestFilename("", "mp3"), "video.mp3");
});

test("formatBytes and formatDuration", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(512), "512 B");
  assert.equal(formatBytes(1536), "1.5 KB");
  assert.equal(formatBytes(12.3 * 1024 * 1024), "12.3 MB");
  assert.equal(formatDuration(3723), "1:02:03");
  assert.equal(formatDuration(65), "1:05");
  assert.equal(formatDuration(NaN), "");
});
