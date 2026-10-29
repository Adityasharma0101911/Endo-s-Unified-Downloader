// Turns a copy of extension/ into the Chrome Web Store build, whose rules ban downloading from YouTube: no YouTube
// button, no content scripts in YouTube's pages and frames (lib/detect.js NO_YOUTUBE does the rest), and no
// Firefox-only manifest keys. Usage: node scripts/chrome-store.mjs <a copy of extension/>
import { readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const YOUTUBE_PAGES = ["*://*.youtube.com/*", "*://*.youtube-nocookie.com/*", "*://youtu.be/*"];

export function toChromeStore(dir) {
  const manifestPath = join(dir, "manifest.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  manifest.content_scripts = manifest.content_scripts
    .filter((script) => !script.js.includes("content/youtube.js"))
    .map((script) => ({ ...script, exclude_matches: [...(script.exclude_matches ?? []), ...YOUTUBE_PAGES] }));
  delete manifest.browser_specific_settings;
  delete manifest.background.scripts;
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + "\n");
  rmSync(join(dir, "content", "youtube.js"));
  const detect = join(dir, "lib", "detect.js");
  const text = readFileSync(detect, "utf8");
  const store = text.replace("export const NO_YOUTUBE = false;", "export const NO_YOUTUBE = true;");
  if (store === text) throw new Error(`${detect} has no "export const NO_YOUTUBE = false;" to turn on`);
  writeFileSync(detect, store);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (!process.argv[2]) {
    console.error("Usage: node scripts/chrome-store.mjs <a copy of extension/>");
    process.exit(2);
  }
  toChromeStore(process.argv[2]);
}
