// scripts/chrome-store.mjs on a copy of the extension: the Chrome Web Store build leaves YouTube alone.
import { test } from "node:test";
import assert from "node:assert/strict";
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { toChromeStore } from "../../scripts/chrome-store.mjs";
import { isBlocked } from "../lib/detect.js";
import { isMediaPage } from "../lib/media.js";

const EXTENSION = fileURLToPath(new URL("..", import.meta.url));

test("the Chrome Web Store build has no YouTube button and leaves YouTube's hosts and pages alone", async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "endo-store-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  cpSync(EXTENSION, dir, { recursive: true, filter: (src) => !/^(test|node_modules)([\\/]|$)/.test(relative(EXTENSION, src)) });
  toChromeStore(dir);

  const manifest = JSON.parse(readFileSync(join(dir, "manifest.json"), "utf8"));
  assert.equal(existsSync(join(dir, "content", "youtube.js")), false);
  assert.equal(manifest.content_scripts.length, 2);
  for (const script of manifest.content_scripts) assert.ok(script.exclude_matches.includes("*://*.youtube.com/*"));
  assert.equal(manifest.browser_specific_settings, undefined);
  assert.deepEqual(manifest.background, { service_worker: "background.js", type: "module" });

  const detect = await import(pathToFileURL(join(dir, "lib", "detect.js")).href);
  const media = await import(pathToFileURL(join(dir, "lib", "media.js")).href);
  assert.equal(detect.isBlocked("rr1---sn-abc.googlevideo.com", []), true);
  assert.equal(detect.isBlocked("www.youtube.com", []), true);
  assert.equal(detect.isBlocked("vimeo.com", []), false);
  assert.equal(media.isMediaPage("https://www.youtube.com/watch?v=abc"), false);
  assert.equal(media.isMediaPage("https://vimeo.com/123"), true);
});

test("the extension itself keeps YouTube", () => {
  assert.equal(isBlocked("www.youtube.com", []), false);
  assert.equal(isMediaPage("https://www.youtube.com/watch?v=abc"), true);
});
