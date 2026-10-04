// Signs a release for the app's updater (see "Releasing" in CLAUDE.md).
//
//   node scripts/sign-release.mjs --keygen [--key <path>]
//     Makes the Ed25519 signing key (PKCS#8 PEM, default ~/.endo-release/update-key.pem; an existing file is never
//     overwritten) and prints its public key, the base64 that goes in PUBLIC_KEY in crates/hyperfetch-core/src/updater.rs.
//
//   node scripts/sign-release.mjs <folder> --version X.Y.Z [--key <path>]
//     Writes <folder>/SHA256SUMS over every file in the folder and <folder>/SHA256SUMS.sig, its signature. Refuses a
//     key that is not the one the app trusts, and a version Cargo.toml and extension/manifest.json do not both say,
//     unless --key names another key (for tests), which only warns.

import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";
import { mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { parseArgs } from "node:util";

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: { keygen: { type: "boolean" }, key: { type: "string" }, version: { type: "string" } },
});
const keyPath = values.key ?? join(homedir(), ".endo-release", "update-key.pem");

/** The raw 32-byte public key of an Ed25519 key, as base64: the last 32 bytes of its SPKI encoding. */
const rawPublic = (key) => createPublicKey(key).export({ type: "spki", format: "der" }).subarray(-32).toString("base64");

function fail(message) {
  console.error(message);
  process.exit(1);
}

if (values.keygen) {
  const { privateKey } = generateKeyPairSync("ed25519");
  mkdirSync(dirname(keyPath), { recursive: true });
  try {
    writeFileSync(keyPath, privateKey.export({ type: "pkcs8", format: "pem" }), { flag: "wx", mode: 0o600 });
  } catch (error) {
    fail(error.code === "EEXIST" ? `${keyPath} already exists; it is not overwritten.` : error.message);
  }
  console.log(`Key written to ${keyPath}. Back it up and never commit it. Public key for updater.rs:`);
  console.log(rawPublic(privateKey));
  process.exit(0);
}

const [folder] = positionals;
if (!folder || positionals.length > 1 || !/^\d+\.\d+\.\d+$/.test(values.version ?? "")) {
  fail("Usage: node scripts/sign-release.mjs <folder> --version X.Y.Z [--key <path>]\n       node scripts/sign-release.mjs --keygen [--key <path>]");
}

/** A file of the source tree this script is in, or "" when it is not there. */
function source(path) {
  try {
    return readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
  } catch {
    return "";
  }
}

// An app that says it is older than its release offers that release again forever, and the browser reloads only an
// extension that says it is newer: both must say this version. As with the key, only --key (tests) lets it pass.
const built = {
  "Cargo.toml": /^\[workspace\.package\][^[]*?^version\s*=\s*"([^"]*)"/m.exec(source("Cargo.toml"))?.[1],
  "extension/manifest.json": JSON.parse(source("extension/manifest.json") || "{}").version,
};
const stale = Object.entries(built).filter(([, version]) => version !== values.version);
if (stale.length) {
  const says = stale.map(([file, version]) => `${file} says ${version ?? "no version"}`).join(" and ");
  if (values.key === undefined) fail(`${says}, not ${values.version}: bump and build again; nothing written.`);
  console.warn(`Warning: ${says}, not ${values.version}; the app will offer this release again forever.`);
}

const key = createPrivateKey(readFileSync(keyPath));
// No updater.rs: no key matches, so only --key goes on.
const trusted = /const PUBLIC_KEY: &str = "([^"]+)"/.exec(source("crates/hyperfetch-core/src/updater.rs"))?.[1];
if (rawPublic(key) !== trusted) {
  if (values.key === undefined) fail(`${keyPath} is not the key whose public key is PUBLIC_KEY in updater.rs; nothing written.`);
  console.warn(`Warning: ${keyPath} is not the key the app trusts; the app will refuse this release.`);
}

// The app's parser is strict: names of [A-Za-z0-9._-]+, sorted, after a "# version" line.
const names = readdirSync(folder, { withFileTypes: true })
  .filter((entry) => entry.isFile() && !entry.name.startsWith("SHA256SUMS"))
  .map((entry) => entry.name)
  .sort();
if (!names.length) fail(`${folder} has no files to sign.`);
const bad = names.find((name) => !/^[A-Za-z0-9._-]+$/.test(name));
if (bad) fail(`"${bad}" is not a name the updater accepts (letters, digits, ".", "_" and "-" only).`);

const lines = names.map((name) => `${createHash("sha256").update(readFileSync(join(folder, name))).digest("hex")}  ${name}`);
const sums = Buffer.from(`# version ${values.version}\n${lines.join("\n")}\n`, "utf8");
const signature = sign(null, sums, key);
if (!verify(null, sums, createPublicKey(key), signature)) fail("The signature does not verify; nothing written.");
writeFileSync(join(folder, "SHA256SUMS"), sums);
writeFileSync(join(folder, "SHA256SUMS.sig"), `${signature.toString("base64")}\n`);
console.log(`Signed ${names.length} files for ${values.version}:\n${lines.join("\n")}`);
