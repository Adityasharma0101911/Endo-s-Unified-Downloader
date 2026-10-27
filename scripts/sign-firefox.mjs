// Signs the browser extension for Firefox through Mozilla's add-on site (AMO), as an unlisted add-on that installs
// from this repo's releases and updates itself through the manifest's update_url.
//
//   node scripts/sign-firefox.mjs <folder> --version X.Y.Z [--dry-run]
//     Copies extension/ without test/ and package.json (as the release zip), has `npx web-ext sign
//     --channel=unlisted` upload it and fetch the signed file, and writes into <folder>:
//       Endos-Unified-Downloader-Firefox-vX.Y.Z.xpi  the signed add-on
//       firefox-updates.json                         what update_url serves (releases/latest/download/…)
//     The AMO API key comes from AMO_JWT_ISSUER and AMO_JWT_SECRET in the environment; it is handed to web-ext through
//     its own environment variables, never on a command line, and never printed or stored.
//     --dry-run does everything up to the upload, prints the command it would run, and writes nothing to <folder>.
//
// Run it before sign-release.mjs: SHA256SUMS must cover both files.

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const EXTENSION = fileURLToPath(new URL("../extension", import.meta.url));
const REPO = "https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader";
// Pinned: what signs the release doesn't change under it.
const WEB_EXT = "web-ext@10.7.0";
export const xpiName = (version) => `Endos-Unified-Downloader-Firefox-v${version}.xpi`;

/** firefox-updates.json: the one version this release offers, its download link and the .xpi's hash. */
export function firefoxUpdates(id, version, xpi) {
  const update = {
    version,
    update_link: `${REPO}/releases/download/v${version}/${xpiName(version)}`,
    update_hash: `sha256:${createHash("sha256").update(xpi).digest("hex")}`,
  };
  return `${JSON.stringify({ addons: { [id]: { updates: [update] } } }, null, 2)}\n`;
}

/** The files of `dir`, relative, with "/" separators. */
const filesOf = (dir) => readdirSync(dir, { recursive: true, withFileTypes: true }).filter((e) => e.isFile()).map((e) => relative(dir, join(e.parentPath, e.name)).split(sep).join("/"));

/** Stops the run with `message` (the temporary copy is still removed). */
function fail(message) {
  throw Object.assign(new Error(message), { expected: true });
}

function main() {
  const { values, positionals } = parseArgs({ allowPositionals: true, options: { version: { type: "string" }, "dry-run": { type: "boolean" } } });
  const [folder] = positionals;
  const version = values.version ?? "";
  if (!folder || positionals.length > 1 || !/^\d+\.\d+\.\d+$/.test(version)) fail("Usage: node scripts/sign-firefox.mjs <folder> --version X.Y.Z [--dry-run]");
  const manifest = JSON.parse(readFileSync(join(EXTENSION, "manifest.json"), "utf8"));
  const gecko = manifest.browser_specific_settings?.gecko ?? {};
  if (!statSync(folder, { throwIfNoEntry: false })?.isDirectory()) fail(`${folder} is not a folder.`);
  if (manifest.version !== version) fail(`extension/manifest.json says ${manifest.version}, not ${version}.`);
  if (!gecko.id || !gecko.update_url) fail("extension/manifest.json needs browser_specific_settings.gecko.id and update_url.");
  const missing = ["AMO_JWT_ISSUER", "AMO_JWT_SECRET"].filter((name) => !process.env[name]);
  if (missing.length) fail(`Set ${missing.join(" and ")} (the API key from https://addons.mozilla.org/developers/addon/api/key/).`);

  const work = mkdtempSync(join(tmpdir(), "endo-firefox-"));
  try {
    const source = join(work, "extension");
    const artifacts = join(work, "signed");
    cpSync(EXTENSION, source, { recursive: true, filter: (path) => !["test", "package.json"].includes(relative(EXTENSION, path).split(sep)[0]) });
    const command = `npx --yes ${WEB_EXT} sign --channel=unlisted --no-input --source-dir "${source}" --artifacts-dir "${artifacts}"`;
    if (values["dry-run"]) {
      console.log(`Dry run: ${filesOf(source).length} files packed for ${gecko.id} ${version}:\n  ${filesOf(source).sort().join("\n  ")}`);
      console.log(`Would run: ${command}\nand write ${xpiName(version)} and firefox-updates.json into ${folder}.`);
      return;
    }
    const env = { ...process.env, WEB_EXT_API_KEY: process.env.AMO_JWT_ISSUER, WEB_EXT_API_SECRET: process.env.AMO_JWT_SECRET };
    const run = spawnSync(command, { shell: true, stdio: "inherit", env });
    if (run.status !== 0) fail(`web-ext sign failed (exit ${run.status ?? run.error?.message}).`);
    const signed = readdirSync(artifacts).filter((name) => name.endsWith(".xpi"));
    if (signed.length !== 1) fail(`Expected one signed .xpi from web-ext, found ${signed.length}.`);
    const xpi = readFileSync(join(artifacts, signed[0]));
    writeFileSync(join(folder, xpiName(version)), xpi);
    writeFileSync(join(folder, "firefox-updates.json"), firefoxUpdates(gecko.id, version, xpi));
    console.log(`Wrote ${xpiName(version)} and firefox-updates.json into ${folder}.`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    main();
  } catch (error) {
    console.error(error.expected ? error.message : error);
    process.exitCode = 1;
  }
}
