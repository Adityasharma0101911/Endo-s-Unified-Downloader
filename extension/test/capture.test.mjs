import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ALT_WINDOW_MS,
  CAPTURE_TYPES,
  altClicked,
  captureDecision,
  extensionOf,
  originOf,
  parseSites,
  parseTypes,
  shouldCapture,
  upgradedTypes,
} from "../lib/capture.js";

test("file types as typed keep their last extension, lower case, once each", () => {
  assert.deepEqual(parseTypes(".ZIP, iso;tar.gz  *.exe exe bad! "), ["zip", "iso", "gz", "exe"]);
  assert.deepEqual(parseTypes(["zip", "ZIP", "", 7]), ["zip", "7"]);
  assert.deepEqual(parseTypes(""), []);
  assert.deepEqual(parseTypes(undefined), []);
  assert.deepEqual(parseTypes(CAPTURE_TYPES), CAPTURE_TYPES);
});

test("sites as typed are hosts, from links and wildcards too", () => {
  assert.deepEqual(parseSites("https://Example.com/path, *.cdn.test sub.example.org:8080 not_a_host! example.com."), [
    "example.com",
    "cdn.test",
    "sub.example.org",
  ]);
  assert.deepEqual(parseSites(["a.test", "A.TEST", "x_y"]), ["a.test"]);
  assert.deepEqual(parseSites(undefined), []);
});

test("the extension of a name, a path or a link's path", () => {
  assert.equal(extensionOf("C:\\Users\\a\\Downloads\\Setup.EXE"), "exe");
  assert.equal(extensionOf("/home/a/file.tar.gz"), "gz");
  assert.equal(extensionOf("movie.mkv"), "mkv");
  assert.equal(extensionOf("https://x.test/a/b.ISO?name=c.zip#d.rar"), "iso");
  for (const none of ["https://x.test/download", "https://x.test/", "README", "", undefined, "a.tar.", "bad.ex e"]) {
    assert.equal(extensionOf(none), "", String(none));
  }
});

const SETTINGS = { captureEnabled: true, captureTypes: CAPTURE_TYPES, captureMinMB: 1, captureExcludedSites: ["skip.test"] };
const ITEM = {
  url: "https://dl.example.com/get?id=1",
  finalUrl: "https://cdn.example.com/files/setup.exe",
  filename: "setup.exe",
  referrer: "https://www.example.com/downloads",
  totalBytes: 5 * 1024 * 1024,
  fileSize: -1,
  incognito: false,
  state: "in_progress",
};
const takes = (item, settings = {}, flags) => shouldCapture({ ...ITEM, ...item }, { ...SETTINGS, ...settings }, flags);

test("a matching download goes to the app", () => {
  assert.equal(takes({}), true);
  // The size unknown: it is not held back for it.
  assert.equal(takes({ totalBytes: 0, fileSize: -1 }), true);
  // Firefox: an absolute path, and only fileSize known.
  assert.equal(takes({ filename: "/home/a/Downloads/setup.exe", totalBytes: -1, fileSize: 2 * 1024 * 1024 }), true);
  // No name yet: the link's own extension.
  assert.equal(takes({ filename: "" }), true);
  assert.equal(takes({ filename: "", finalUrl: undefined, url: "https://x.test/a.zip" }), true);
  assert.equal(takes({ captureMinMB: 0, totalBytes: 10 }, { captureMinMB: 0 }), true);
});

test("capture off, incognito, Alt, POST, blob: and data: stay in the browser", () => {
  assert.equal(takes({}, { captureEnabled: false }), false);
  assert.equal(takes({ incognito: true }), false);
  assert.equal(takes({ byExtensionId: "abcdefghijklmnopabcdefghijklmnop" }), false);
  assert.equal(takes({}, {}, { altClicked: true }), false);
  assert.equal(takes({}, {}, { posted: true }), false);
  assert.equal(takes({ url: "blob:https://www.example.com/5e1f", finalUrl: "blob:https://www.example.com/5e1f" }), false);
  assert.equal(takes({ url: "data:application/zip;base64,UEsDBA==", finalUrl: undefined, filename: "a.zip" }), false);
  assert.equal(takes({ finalUrl: "ftp://ftp.example.com/setup.exe" }), false);
  assert.equal(takes({ state: "complete" }), false);
});

test("types, the minimum size and excluded sites filter", () => {
  assert.equal(takes({ filename: "page.html", finalUrl: "https://cdn.example.com/page.html" }), false);
  assert.equal(takes({ filename: "setup.exe" }, { captureTypes: ["zip"] }), false);
  assert.equal(takes({}, { captureTypes: [] }), false);
  // The file name decides over the link.
  assert.equal(takes({ filename: "notes.txt" }), false);
  assert.equal(takes({ totalBytes: 1024 * 1024 - 1 }), false);
  assert.equal(takes({ totalBytes: 1024 * 1024 }), true);
  assert.equal(takes({ totalBytes: 3 * 1024 * 1024 }, { captureMinMB: 4 }), false);
  // The download's host, or the page's, on the list or under it.
  assert.equal(takes({ finalUrl: "https://files.skip.test/setup.exe" }), false);
  assert.equal(takes({ referrer: "https://skip.test/" }), false);
  assert.equal(takes({ referrer: "https://notskip.test/" }), true);
  assert.equal(takes({ referrer: "" }), true);
});

test("a .torrent is not sent by default: a page could start a swarm download with it", () => {
  assert.equal(CAPTURE_TYPES.includes("torrent"), false);
  assert.equal(takes({ filename: "x.torrent", finalUrl: "https://cdn.example.com/x.torrent", totalBytes: 0 }), false);
  assert.equal(takes({ filename: "x.torrent", totalBytes: 0 }, { captureTypes: ["torrent"] }), true);
});

const GB = 1024 * 1024 * 1024;
const why = (item, settings = {}, flags) => captureDecision({ ...ITEM, ...item }, { ...SETTINGS, ...settings }, flags);

test("a download of any type goes when it is at least captureAnyOverMB; types decide below it", () => {
  const dat = { filename: "dump.dat", finalUrl: "https://cdn.example.com/dump.dat" };
  const big = { ...dat, filename: "dump.unknownext", totalBytes: 5 * GB };
  assert.deepEqual(why(big, { captureAnyOverMB: 50 }), { capture: true, reason: "50 MB or more" });
  assert.equal(takes(big, { captureAnyOverMB: 0 }), false, "0 turns it off");
  assert.deepEqual(why({ ...big, totalBytes: 10 * 1024 * 1024 }, { captureAnyOverMB: 50 }), {
    capture: false,
    reason: ".unknownext isn't one of the file types, and it's under 50 MB",
  });
  // The size unknown: the type decides, as before.
  assert.deepEqual(why({ ...big, totalBytes: 0 }, { captureAnyOverMB: 50 }), {
    capture: false,
    reason: ".unknownext isn't one of the file types, and its size isn't known",
  });
  assert.equal(takes({ ...dat, totalBytes: 0 }, { captureTypes: CAPTURE_TYPES }), true, ".dat is a default type now");
  // Every other rule still applies.
  assert.equal(takes(big, { captureAnyOverMB: 50 }, { posted: true }), false);
  assert.equal(takes({ ...big, referrer: "https://skip.test/" }, { captureAnyOverMB: 50 }), false);
});

test("Everything sends any http(s) download but those under the minimum size or kept by the other rules", () => {
  const page = { filename: "page.html", finalUrl: "https://cdn.example.com/page.html" };
  assert.deepEqual(why(page, { captureAll: true }), { capture: true, reason: "Everything goes to the app" });
  assert.equal(takes({ ...page, totalBytes: 0 }, { captureAll: true }), true);
  assert.equal(takes({ ...page, totalBytes: 1000 }, { captureAll: true }), false);
  assert.equal(takes({ ...page, finalUrl: "https://a.skip.test/x" }, { captureAll: true }), false);
  assert.equal(takes({ ...page, incognito: true }, { captureAll: true }), false);
  assert.equal(takes(page, { captureAll: true }, { altClicked: true }), false);
});

test("an always site's downloads go whatever their type or size; an excluded site wins", () => {
  const tiny = { filename: "notes.txt", finalUrl: "https://files.always.test/notes.txt", totalBytes: 10 };
  assert.deepEqual(why(tiny, { captureAlwaysSites: ["always.test"] }), { capture: true, reason: "Always from files.always.test" });
  assert.equal(takes({ ...tiny, finalUrl: "https://cdn.other.test/notes.txt", referrer: "https://always.test/page" }, { captureAlwaysSites: ["always.test"] }), true);
  assert.equal(takes(tiny, { captureAlwaysSites: ["always.test"], captureExcludedSites: ["always.test"] }), false);
  assert.equal(takes(tiny, { captureAlwaysSites: ["notalways.test"] }), false);
});

test("why a download stays in the browser", () => {
  assert.equal(why({}, { captureEnabled: false }).reason, "Sending browser downloads to the app is off");
  assert.equal(why({ url: "blob:https://www.example.com/5e1f", finalUrl: "" }).reason, "Made by the page (blob: or data:): only the browser has it");
  assert.equal(why({}, {}, { posted: true }).reason, "The answer to a form sent with POST: the app can only ask for a link again");
  assert.equal(why({ state: "complete" }).reason, "Finished before it could be handed over");
  assert.equal(why({ totalBytes: 10 }).reason, "Under the minimum size (1 MB)");
  assert.equal(why({ finalUrl: "https://files.skip.test/setup.exe" }).reason, "Never from files.skip.test");
  assert.equal(why({ filename: "", finalUrl: "https://x.test/get" }).reason, "No file type in its name or link");
  assert.deepEqual(why({}), { capture: true, reason: ".exe is one of the file types" });
});

test("a stored type list still the old default gets the new defaults; an edited one is kept", () => {
  const old = CAPTURE_TYPES.slice(0, 30);
  assert.equal(upgradedTypes([...old].reverse()), CAPTURE_TYPES);
  assert.deepEqual(upgradedTypes(["zip"]), ["zip"]);
  assert.deepEqual(upgradedTypes([...old, "torrent"]), [...old, "torrent"]);
  for (const type of ["dat", "bin", "vhdx", "safetensors", "gguf", "opus", "cbz", "ttf", "esd"]) assert.ok(CAPTURE_TYPES.includes(type), type);
});

test("an Alt-click keeps the downloads of its own page in the browser, not those of other pages", () => {
  const now = 1_000_000;
  const clicks = new Map([["https://www.example.com", now - 1000]]);
  assert.equal(altClicked(clicks, "https://www.example.com/downloads", now), true);
  assert.equal(altClicked(clicks, "https://other.test/page", now), false);
  assert.equal(altClicked(clicks, "https://www.example.com/", now + ALT_WINDOW_MS), false, "too long ago");
  // No page known: any page's Alt-click counts.
  assert.equal(altClicked(clicks, "", now), true);
  assert.equal(altClicked(new Map(), undefined, now), false);
  assert.equal(originOf("https://a.test:8443/x?y"), "https://a.test:8443");
  assert.equal(originOf("about:blank"), "");
  assert.equal(originOf(undefined), "");
});
