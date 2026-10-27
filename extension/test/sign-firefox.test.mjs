// scripts/sign-firefox.mjs up to the upload (--dry-run), and the firefox-updates.json it writes. Never signs.
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { firefoxUpdates, xpiName } from "../../scripts/sign-firefox.mjs";

const SCRIPT = fileURLToPath(new URL("../../scripts/sign-firefox.mjs", import.meta.url));
const MANIFEST = JSON.parse(readFileSync(new URL("../manifest.json", import.meta.url), "utf8"));
const VERSION = MANIFEST.version;
const KEYS = { AMO_JWT_ISSUER: "user:12345:678", AMO_JWT_SECRET: "s3cr3t-never-printed-0123456789abcdef" };

function run(args, env = KEYS) {
  const clean = { ...process.env };
  delete clean.AMO_JWT_ISSUER;
  delete clean.AMO_JWT_SECRET;
  const r = spawnSync(process.execPath, [SCRIPT, ...args], { env: { ...clean, ...env }, encoding: "utf8" });
  return { status: r.status, out: r.stdout + r.stderr };
}

test("the manifest has a stable Firefox id and the releases' update_url", () => {
  const gecko = MANIFEST.browser_specific_settings.gecko;
  assert.equal(gecko.id, "endo-downloader@local");
  assert.equal(gecko.update_url, "https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases/latest/download/firefox-updates.json");
});

test("firefox-updates.json offers the version, its release link and the .xpi's sha256", () => {
  const xpi = Buffer.from("PK fake xpi");
  const json = JSON.parse(firefoxUpdates("endo-downloader@local", "1.2.3", xpi));
  assert.deepEqual(json, {
    addons: {
      "endo-downloader@local": {
        updates: [
          {
            version: "1.2.3",
            update_link: "https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases/download/v1.2.3/Endos-Unified-Downloader-Firefox-v1.2.3.xpi",
            update_hash: `sha256:${createHash("sha256").update(xpi).digest("hex")}`,
          },
        ],
      },
    },
  });
  assert.equal(xpiName("1.2.3"), "Endos-Unified-Downloader-Firefox-v1.2.3.xpi");
});

test("--dry-run packs the extension as the release zip does, stops before the upload and never prints the key", () => {
  const folder = mkdtempSync(join(tmpdir(), "endo-ff-test-"));
  try {
    const { status, out } = run([folder, "--version", VERSION, "--dry-run"]);
    assert.equal(status, 0, out);
    assert.match(out, /Would run: npx --yes web-ext@\d+\.\d+\.\d+ sign --channel=unlisted /);
    assert.match(out, /^  manifest\.json$/m);
    assert.match(out, /^  background\.js$/m);
    assert.doesNotMatch(out, /^  (test\/|package\.json)/m);
    assert.ok(!out.includes(KEYS.AMO_JWT_SECRET) && !out.includes(KEYS.AMO_JWT_ISSUER));
    assert.deepEqual(readdirSync(folder), []);
  } finally {
    rmSync(folder, { recursive: true, force: true });
  }
});

test("it refuses a wrong version, a missing key or folder, before anything is uploaded", () => {
  const folder = mkdtempSync(join(tmpdir(), "endo-ff-test-"));
  try {
    assert.match(run([folder, "--version", "0.0.1", "--dry-run"]).out, /says .*not 0\.0\.1/);
    const noKey = run([folder, "--version", VERSION, "--dry-run"], { AMO_JWT_ISSUER: "x" });
    assert.equal(noKey.status, 1);
    assert.match(noKey.out, /Set AMO_JWT_SECRET/);
    assert.match(run([join(folder, "missing"), "--version", VERSION]).out, /is not a folder/);
    assert.match(run([folder]).out, /^Usage:/);
    assert.deepEqual(readdirSync(folder), []);
  } finally {
    rmSync(folder, { recursive: true, force: true });
  }
});
