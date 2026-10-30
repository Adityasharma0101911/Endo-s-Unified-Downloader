import { test } from "node:test";
import assert from "node:assert/strict";
import {
  MAX_TRIES,
  WAITING_TTL_MS,
  WaitingList,
  baseName,
  firstTry,
  matchDownload,
  matchMedia,
  rescueWithMedia,
  samePage,
  sendCaptured,
} from "../lib/rescue.js";

const WAITING = [
  { id: 4, name: "Setup.exe", size: 640 * 1024 * 1024, host: "cdn.example.com", page: "https://www.example.com/downloads?tab=win" },
  { id: 7, name: "talk.mp4", size: 52_000_000, host: "media.example.net", page: "https://video.example.org/watch/42?t=10" },
  { id: 9, name: "no-page.zip", size: 1000, host: "files.example.com", page: null },
];
const DOWNLOAD = { url: "https://cdn.example.com/get?id=1", finalUrl: "https://cdn.example.com/files/setup.exe?sig=new", filename: "Setup.exe" };

test("a file name is compared without its folder, the browser's (1), or its case", () => {
  assert.equal(baseName("Setup (1).exe"), "setup.exe");
  assert.equal(baseName("C:\\Users\\a\\Downloads\\Setup (12).EXE"), "setup.exe");
  assert.equal(baseName("/home/a/Downloads/report (2)"), "report");
  assert.equal(baseName("Episode (1080p).mp4"), "episode (1080p).mp4", "only a number in brackets is the browser's");
  assert.equal(baseName(undefined), "");
});

test("a browser download of a waiting file's name, but for its (1), is a new link for it", () => {
  assert.equal(matchDownload(WAITING, DOWNLOAD)?.id, 4);
  assert.equal(matchDownload(WAITING, { ...DOWNLOAD, filename: "C:\\Downloads\\Setup (1).exe" })?.id, 4);
  // No name yet: the link's own.
  assert.equal(matchDownload(WAITING, { ...DOWNLOAD, filename: "" })?.id, 4);
  assert.equal(matchDownload(WAITING, { ...DOWNLOAD, filename: "Setup-2.exe" }), null);
  assert.equal(matchDownload([], DOWNLOAD), null);
});

test("a browser download's size, when known, must be the waiting file's", () => {
  assert.equal(matchDownload(WAITING, { ...DOWNLOAD, totalBytes: 640 * 1024 * 1024 })?.id, 4);
  assert.equal(matchDownload(WAITING, { ...DOWNLOAD, totalBytes: 0, fileSize: 640 * 1024 * 1024 })?.id, 4, "Firefox: fileSize");
  assert.equal(matchDownload(WAITING, { ...DOWNLOAD, totalBytes: 812 * 1024 * 1024 }), null, "another size is another file");
  // Unknown (0 or -1): the name decides, and the app checks the size itself.
  assert.equal(matchDownload(WAITING, { ...DOWNLOAD, totalBytes: -1, fileSize: -1 })?.id, 4);
});

test("pages are the same by origin and path alone", () => {
  assert.equal(samePage("https://video.example.org/watch/42?t=10", "https://video.example.org/watch/42#comments"), true);
  assert.equal(samePage("https://video.example.org/watch/42", "https://video.example.org/watch/43"), false);
  assert.equal(samePage("https://video.example.org/watch/42", "http://video.example.org/watch/42"), false);
  assert.equal(samePage("https://video.example.org/", null), false);
});

test("a file the waiting download's page plays, of its name or type, is a candidate; streams and other pages are not", () => {
  const played = { kind: "file", name: "master_720.mp4", format: "mp4", pageUrl: "https://video.example.org/watch/42", url: "https://media.example.net/v/master_720.mp4?exp=2" };
  assert.deepEqual(matchMedia(WAITING, played).map((d) => d.id), [7], "same page, same type");
  assert.deepEqual(matchMedia(WAITING, { ...played, name: "talk.mp4", format: "" }).map((d) => d.id), [7], "same name");
  assert.deepEqual(matchMedia(WAITING, { ...played, name: "clip", format: "mp4" }).map((d) => d.id), [7], "the type the page served");
  assert.deepEqual(matchMedia(WAITING, { ...played, name: "audio.webm", format: "webm" }), [], "another type");
  assert.deepEqual(matchMedia(WAITING, { ...played, pageUrl: "https://video.example.org/watch/43" }), [], "another page");
  assert.deepEqual(matchMedia(WAITING, { ...played, kind: "hls", format: "m3u8" }), [], "a stream");
  assert.deepEqual(matchMedia(WAITING, { ...played, name: "no-page.zip", pageUrl: "https://files.example.com/" }), [], "a download from no page");
});

test("each link is tried once for a waiting download, and a few at most", () => {
  const tried = new Map();
  assert.equal(firstTry(tried, 7, "https://media.example.net/a.mp4"), true);
  assert.equal(firstTry(tried, 7, "https://media.example.net/a.mp4"), false, "a player asks for the same file again and again");
  for (let n = 1; n < MAX_TRIES; n++) assert.equal(firstTry(tried, 7, `https://media.example.net/${n}.mp4`), true);
  assert.equal(firstTry(tried, 7, "https://media.example.net/last.mp4"), false);
  assert.equal(firstTry(tried, 4, "https://media.example.net/last.mp4"), true, "another download's tries are its own");
});

test("a captured download goes to /relink for the waiting download it matches, else to /add as before", async () => {
  const calls = [];
  const relink = (answer) => async (download) => (calls.push(`relink ${download.id}`), answer);
  const add = async () => (calls.push("add"), { ok: true, added: true });
  assert.deepEqual(await sendCaptured(WAITING, DOWNLOAD, relink(true), add), { ok: true });
  assert.deepEqual(calls.splice(0), ["relink 4"], "rescued: no new download");
  assert.deepEqual(await sendCaptured(WAITING, { ...DOWNLOAD, filename: "other.exe" }, relink(true), add), { ok: true, added: true });
  assert.deepEqual(calls.splice(0), ["add"], "no match: added as before");
  assert.deepEqual(await sendCaptured([], DOWNLOAD, relink(true), add), { ok: true, added: true });
  assert.deepEqual(calls.splice(0), ["add"], "nothing waits");
  await sendCaptured(WAITING, DOWNLOAD, relink(false), add);
  assert.deepEqual(calls.splice(0), ["relink 4", "add"], "the app refused it (another file): added as before");
});

test("a file a waiting download's page plays is tried as its new link, each link once, until one is taken", async () => {
  const page = "https://video.example.org/watch/42";
  const waiting = [WAITING[1], { id: 8, name: "talk-part2.mp4", size: 9, page }];
  const tried = new Map();
  const asked = [];
  const relink = (takes) => async (download) => (asked.push(download.id), takes(download));
  const played = (url) => ({ kind: "file", name: "v.mp4", format: "mp4", pageUrl: page, url });
  assert.equal(await rescueWithMedia(waiting, played("https://cdn.example.net/a.mp4"), tried, relink(() => false)), null);
  assert.deepEqual(asked.splice(0), [7, 8], "both downloads of the page, refused (another size)");
  assert.equal(await rescueWithMedia(waiting, played("https://cdn.example.net/a.mp4"), tried, relink(() => false)), null);
  assert.deepEqual(asked.splice(0), [], "the same link again is not sent again");
  const taken = await rescueWithMedia(waiting, played("https://cdn.example.net/b.mp4"), tried, relink((d) => d.id === 7));
  assert.equal(taken?.id, 7);
  assert.deepEqual(asked.splice(0), [7], "taken: the others are not tried");
  assert.equal(await rescueWithMedia(waiting, { ...played("https://cdn.example.net/c.m3u8"), kind: "hls" }, tried, relink(() => true)), null);
  assert.deepEqual(asked, [], "a stream is never sent");
});

test("the waiting list is asked for when a candidate comes and it is stale, once for callers at once, empty when the app can't say", async () => {
  let now = 1_000_000;
  let asked = 0;
  let answer = [WAITING[0]];
  const list = new WaitingList(async () => {
    asked++;
    if (answer instanceof Error) throw answer;
    return answer;
  }, () => now);
  assert.equal(asked, 0, "never asked before a candidate comes");
  const [a, b] = await Promise.all([list.get(), list.get()]);
  assert.deepEqual([a, b, asked], [[WAITING[0]], [WAITING[0]], 1]);
  now += WAITING_TTL_MS - 1;
  answer = [];
  assert.deepEqual(await list.get(), [WAITING[0]], "cached");
  assert.equal(asked, 1);
  assert.deepEqual(await list.get({ fresh: true }), [], "a browser download asks again at once");
  assert.equal(asked, 2);
  now += WAITING_TTL_MS;
  answer = new Error("Endo's Unified Downloader is not running.");
  assert.deepEqual(await list.get(), []);
  answer = { error: "not found" };
  assert.deepEqual(await list.get({ fresh: true }), [], "an app too old to list them");
  assert.equal(asked, 4);

  answer = WAITING;
  await list.get({ fresh: true });
  list.drop(4);
  assert.deepEqual((await list.get()).map((d) => d.id), [7, 9], "one that took its new link is gone");
});
