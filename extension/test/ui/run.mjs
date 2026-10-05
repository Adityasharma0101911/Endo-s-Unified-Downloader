// Headless popup harness: fixture pages + mock app + headless Edge with the extension loaded, driven over CDP.
// Opens each fixture, then popup.html?tab=<id> as a tab, captures every view in light and dark, clicks through the
// main actions and asserts what reached the mock app. See README.md.
//   node extension/test/ui/run.mjs [--out <dir>] [--only name,name] [--ext <dir>] [--edge <msedge.exe>] [--media-host <host>]
// Never touches the real screen, mouse or keyboard: the browser is headless and uses a throwaway profile.
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { normalizeLinks } from "../../lib/links.js";
import { startFixtures } from "./fixtures/serve.mjs";
import { INFO, INFO_ERROR, startMockApp } from "./mock-app.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const CDP_PORT = 9447;
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : fallback;
};
const OUT = path.resolve(arg("out", path.join(os.tmpdir(), `endo-popup-ui-${Date.now()}`)));
const EDGE = arg("edge", "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe");
const ONLY = arg("only", "").split(",").filter(Boolean);
// The extension to load: this one by default, or another copy (say, the last release) to compare with.
const EXT = path.resolve(arg("ext", path.join(HERE, "../..")));
const MEDIA_HOST = arg("media-host", "www.twitch.tv");
const THEMES = ["light", "dark"];
const VIEWS = ["Media", "Record", "Links", "Settings"];
const FX = "http://127.0.0.1:8765";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const report = { started: new Date().toISOString(), out: OUT, extension: EXT, captures: [], checks: [], errors: [], pageErrors: [] };
/** sessionId → what it shows, to label the exceptions and console errors of popups and fixtures. */
const labels = new Map();
const check = (name, pass, detail = "") => {
  report.checks.push({ name, pass: !!pass, detail });
  console.log(`${pass ? "PASS" : "FAIL"} ${name}${detail ? ` — ${typeof detail === "string" ? detail : JSON.stringify(detail)}` : ""}`);
};

/** Polls `fn` until it returns something truthy or `ms` pass; returns the last value. */
async function until(fn, ms = 5000, step = 150) {
  const end = Date.now() + ms;
  let value;
  while (!(value = await fn().catch(() => null)) && Date.now() < end) await sleep(step);
  return value;
}

// ---- CDP over one browser WebSocket, with flat sessions ----

let ws;
let nextId = 0;
const pending = new Map();
function send(method, params = {}, sessionId) {
  const id = ++nextId;
  ws.send(JSON.stringify({ id, method, params, sessionId }));
  return new Promise((resolve, reject) => {
    // A call to a page that crashed or closed never gets an answer: fail it instead of hanging the run.
    const timer = setTimeout(() => (pending.delete(id), reject(new Error(`${method}: no answer in 20 s`))), 20000);
    pending.set(id, (m) => (clearTimeout(timer), m.error ? reject(new Error(`${method}: ${m.error.message}`)) : resolve(m.result)));
  });
}
async function evaluate(sessionId, expression) {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true, userGesture: true }, sessionId);
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
  return r.result.value;
}
async function open(url, { newWindow = false } = {}) {
  const { targetId } = await send("Target.createTarget", { url, newWindow });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  labels.set(sessionId, url);
  await send("Runtime.enable", {}, sessionId);
  // A new target starts on about:blank, already "complete": wait for the real page.
  await until(() => evaluate(sessionId, `document.readyState === "complete" && (location.href !== "about:blank" || ${JSON.stringify(url)} === "about:blank")`), 10000);
  return { targetId, sessionId };
}

// ---- popup helpers: found by role and visible text, so they work on today's markup and the redesign ----

const PAGE_HELPERS = `
  window.__vis = (e) => !!(e && (e.offsetParent || e.getClientRects().length));
  window.__find = (sel, re) => [...document.querySelectorAll(sel)].find((e) => __vis(e) && (re.test(e.getAttribute("aria-label") || "") || re.test(e.textContent.trim())));
  window.__view = (name) => {
    const want = new RegExp("^\\\\s*" + name, "i");
    const tab = [...document.querySelectorAll('[role=tab], nav button, .views button, button[data-view], summary')].find((e) => want.test(e.textContent));
    scrollTo(0, 0);
    if (tab?.tagName === "SUMMARY") { tab.parentElement.open = true; tab.scrollIntoView(); return "details"; }
    if (tab) { tab.click(); return "tab"; }
    const head = [...document.querySelectorAll("h1, h2, h3")].find((h) => __vis(h) && want.test(h.textContent));
    if (head) { head.scrollIntoView(); return "heading"; }
    return "missing";
  };
  window.__click = (re) => { const b = __find("button, [role=button], summary", re); if (!b) return "missing"; if (b.disabled) return "disabled"; b.click(); return "clicked"; };
  // What sticks out sideways: boxes whose content is wider than they are.
  window.__overflow = () => [document.documentElement, ...document.querySelectorAll("body, .views, .panel, .card, .banner, .links-top, .sticky-foot, li")]
    .filter((e) => __vis(e) && e.scrollWidth > e.clientWidth + 1 && getComputedStyle(e).overflowX !== "hidden")
    .map((e) => e.id || e.className || e.tagName);
  // Visible text (and form fields) under 4.5:1 against the colours actually behind it; disabled and faded controls
  // are exempt (WCAG 1.4.3 "inactive components").
  window.__contrast = () => {
    const parse = (s) => {
      let m = /rgba?\\(([^)]+)\\)/.exec(s);
      if (m) { const p = m[1].split(/[\\s,\\/]+/).filter(Boolean).map(Number); return [p[0], p[1], p[2], p[3] ?? 1]; }
      m = /color\\(srgb ([^)]+)\\)/.exec(s);
      if (m) { const p = m[1].split(/[\\s\\/]+/).filter(Boolean).map(Number); return [p[0] * 255, p[1] * 255, p[2] * 255, p[3] ?? 1]; }
      return null;
    };
    const over = (top, under) => [0, 1, 2].map((i) => top[i] * top[3] + under[i] * (1 - top[3])).concat(1);
    const lum = (c) => { const f = (v) => ((v /= 255) <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4); return 0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2]); };
    const out = [];
    let n = 0;
    for (const e of document.querySelectorAll("body *")) {
      const field = e.matches("input:not([type=checkbox]), select");
      if (!field && ![...e.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim())) continue;
      if (!__vis(e) || e.closest(":disabled, [aria-disabled=true], option, svg")) continue;
      let faded = false;
      const layers = [];
      for (let n = e; n; n = n.parentElement) {
        const s = getComputedStyle(n);
        if (Number(s.opacity) < 1 || s.visibility === "hidden") faded = true;
        const c = parse(s.backgroundColor);
        if (c && c[3] > 0) { layers.push(c); if (c[3] >= 1) break; }
      }
      if (faded) continue;
      n++;
      let bg = [255, 255, 255, 1];
      for (const c of layers.reverse()) bg = over(c, bg);
      const fg = over(parse(getComputedStyle(e).color), bg);
      const [a, b] = [lum(fg), lum(bg)].sort((x, y) => y - x);
      const ratio = (a + 0.05) / (b + 0.05);
      if (ratio < 4.5) out.push({ text: (field ? e.value || e.placeholder : e.textContent).trim().slice(0, 40), where: e.id || e.className || e.tagName, ratio: Math.round(ratio * 100) / 100 });
    }
    return { n, out };
  };
  // The longest transition and animation (finite ones) on any element or ::before/::after, in seconds.
  window.__motion = () => {
    const most = (list) => Math.max(0, ...list.split(",").map((t) => parseFloat(t) * (t.trim().endsWith("ms") ? 0.001 : 1)));
    let transition = 0, animation = 0;
    for (const e of document.querySelectorAll("*")) for (const pseudo of [null, "::before", "::after"]) {
      const s = getComputedStyle(e, pseudo);
      transition = Math.max(transition, most(s.transitionDuration));
      if (s.animationName !== "none" && s.animationIterationCount !== "infinite") animation = Math.max(animation, most(s.animationDuration));
    }
    return { transition, animation };
  };
  true;`;

/** Makes the popup believe the background reported `patch` in GET_STATE (states a test browser can't reach). */
const fakeState = (patch) => `{
  const send = chrome.runtime.sendMessage.bind(chrome.runtime);
  const patched = (m) => send(m).then((r) => (m?.cmd === "GET_STATE" && r ? { ...r, ...${JSON.stringify(patch)} } : r));
  Object.defineProperty(chrome.runtime, "sendMessage", { value: patched, configurable: true, writable: true });
  window.__patched = chrome.runtime.sendMessage === patched;
}`;

async function openPopup(tabId, { patch } = {}) {
  const page = await open(`chrome-extension://${report.extensionId}/popup.html?tab=${tabId}`, { newWindow: true });
  await send("Emulation.setDeviceMetricsOverride", { width: 420, height: 600, deviceScaleFactor: 1, mobile: false }, page.sessionId);
  if (patch) {
    await send("Page.enable", {}, page.sessionId);
    await send("Page.addScriptToEvaluateOnNewDocument", { source: fakeState(patch) }, page.sessionId);
    await send("Page.reload", {}, page.sessionId);
    await until(() => evaluate(page.sessionId, "document.readyState === 'complete' && window.__patched !== undefined"), 5000);
  }
  await evaluate(page.sessionId, PAGE_HELPERS);
  // Settled once the app status is known (GET_STATE answered).
  await until(() => evaluate(page.sessionId, "!/Checking/i.test(document.body.innerText)"), 4000);
  await sleep(400);
  return page;
}

async function theme(page, scheme) {
  await send(
    "Emulation.setEmulatedMedia",
    { features: [{ name: "prefers-color-scheme", value: scheme }, { name: "prefers-reduced-motion", value: "reduce" }] },
    page.sessionId,
  );
}

async function capture(page, name, extra = {}) {
  await sleep(150);
  const { data } = await send("Page.captureScreenshot", { format: "png" }, page.sessionId);
  const file = path.join(OUT, `${name}.png`);
  fs.writeFileSync(file, Buffer.from(data, "base64"));
  const overflow = await evaluate(page.sessionId, "__overflow()");
  report.captures.push({ name, file, ...extra, ...(overflow.length ? { overflow } : {}) });
}

/** Low-contrast text of the view on screen, once per distinct text, colour scheme and ratio. */
const lowContrast = new Map();
async function auditContrast(page, where) {
  const { n, out } = await evaluate(page.sessionId, "__contrast()");
  report.contrastChecked = (report.contrastChecked || 0) + n;
  for (const f of out) {
    const key = `${where.theme}|${f.where}|${f.text}|${f.ratio}`;
    if (!lowContrast.has(key)) lowContrast.set(key, { ...f, ...where });
  }
}

const view = (page, name) => evaluate(page.sessionId, `__view(${JSON.stringify(name)})`);
const click = (page, re) => evaluate(page.sessionId, `__click(${re})`);

/** Captures every view in both themes as <fixture>-<view>-<theme>.png. */
async function captureViews(page, fixture) {
  for (const scheme of THEMES) {
    await theme(page, scheme);
    for (const name of VIEWS) {
      const how = await view(page, name);
      await capture(page, `${fixture}-${name.toLowerCase()}-${scheme}`, { fixture, view: name, theme: scheme, how });
      await auditContrast(page, { fixture, view: name, theme: scheme });
    }
  }
  await view(page, "Media");
}

/** Arrow keys, Home and End in the [role=tablist] move the selection and the focus (contract). */
async function keyboardTabs(page) {
  const selected = "(() => { const t = document.querySelector('[role=tablist] [role=tab][aria-selected=true]'); return t ? t.id + (document.activeElement === t ? '' : ' (not focused)') : null; })()";
  const start = await evaluate(page.sessionId, `(document.querySelector('[role=tablist] [role=tab][aria-selected=true]')?.focus(), ${selected})`);
  if (start === null) {
    console.log("SKIP keyboard: arrow keys, Home and End move between tabs — no [role=tablist]");
    return report.checks.push({ name: "keyboard: arrow keys, Home and End move between tabs", pass: true, skipped: true });
  }
  const codes = { ArrowRight: 39, ArrowLeft: 37, Home: 36, End: 35 };
  const steps = [];
  for (const key of ["ArrowRight", "End", "Home", "ArrowLeft"]) {
    for (const type of ["rawKeyDown", "keyUp"]) await send("Input.dispatchKeyEvent", { type, key, code: key, windowsVirtualKeyCode: codes[key] }, page.sessionId);
    await sleep(150);
    steps.push(`${key}→${await evaluate(page.sessionId, selected)}`);
  }
  const want = ["ArrowRight→tab-links", "End→tab-settings", "Home→tab-media", "ArrowLeft→tab-settings"];
  check("keyboard: arrow keys, Home and End move between tabs (and the focus)", want.join() === steps.join(), { from: start, steps });
  await view(page, "Media");
}

/** With reduced motion nothing moves; otherwise every finite transition and animation is 120–250 ms. */
async function motion(page) {
  const reduced = await evaluate(page.sessionId, "__motion()");
  await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "no-preference" }] }, page.sessionId);
  const full = await evaluate(page.sessionId, "__motion()");
  await theme(page, "light");
  check("motion: reduced motion turns off every transition and animation", reduced.transition === 0 && reduced.animation === 0, reduced);
  check("motion: otherwise nothing lasts longer than 250 ms", full.transition > 0 && full.transition <= 0.25 && full.animation <= 0.25, full);
}

async function captureAfter(page, fixture, name) {
  for (const scheme of THEMES) {
    await theme(page, scheme);
    await capture(page, `${fixture}-${name}-${scheme}`, { fixture, view: name, theme: scheme });
    await auditContrast(page, { fixture, view: name, theme: scheme });
  }
}

// ---- the media card ----

/** Waits for the media card's choices (or its error), then reads what it shows. */
async function mediaCard(page) {
  await until(() => evaluate(page.sessionId, `!document.getElementById("mc-form").hidden || !document.getElementById("mc-error").hidden`), 8000);
  return evaluate(page.sessionId, `(() => {
    const $ = (id) => document.getElementById(id);
    return {
      title: $("mc-title").textContent,
      channel: $("mc-channel").textContent,
      length: __vis($("mc-length")) ? $("mc-length").textContent : "",
      chip: __vis($("mc-live")) ? $("mc-live").textContent : "",
      thumb: __vis($("mc-thumb")) && $("mc-thumb").complete && $("mc-thumb").naturalWidth > 0,
      qualities: [...document.querySelectorAll("#mc-quality .seg")].map((l) => l.textContent),
      audio: __vis($("mc-audio-row")),
      quality: document.querySelector("#mc-form input[name=quality]:checked")?.value,
      container: document.querySelector("#mc-form input[name=container]:checked")?.value,
      subs: [...$("mc-subs").options].map((o) => o.value + (/\\(auto\\)$/.test(o.textContent) ? "*" : "")),
      sub: $("mc-subs").value,
      rows: ["format", "subs", "clip", "sponsor", "scope", "live"].filter((row) => __vis($("mc-" + row + "-row"))),
      scope: $("mc-scope-all").textContent,
      error: __vis($("mc-error")) ? $("mc-error-text").textContent : "",
      pageCard: __vis($("page-card")),
    };
  })()`);
}

/** Ticks the card's radio `name` = `value` (as a click on it does). */
const pick = (page, name, value) => evaluate(page.sessionId, `document.querySelector('#mc-form input[name=${name}][value="${value}"]').click()`);

/** Clicks the card's Download and returns the /add body that reached the app, and what the card said. */
async function downloadMedia(page, app) {
  await until(() => evaluate(page.sessionId, `__vis(document.getElementById("mc-download"))`), 3000);
  const before = app.requests.length;
  const clicked = await click(page, "/^\\s*download\\s*$/i");
  const body = (await until(async () => app.since(before, /^\/add$/).at(-1), 8000))?.body;
  return { clicked, body, said: await evaluate(page.sessionId, `document.getElementById("mc-msg").textContent`) };
}

// ---- the YouTube button (content/youtube.js), on fixture pages that mimic YouTube's markup ----

/** Puts content/youtube.js into a fixture tab as the manifest does on YouTube (the fixtures are http://127.0.0.1). */
const injectYoutube = (ctx) => evaluate(ctx.sw, `chrome.scripting.executeScript({ target: { tabId: ${ctx.tabId} }, files: ["content/youtube.js"] }).then(() => true)`);

/**
 * Reads the button in the tab from the extension's isolated world, where chrome.dom opens its closed shadow root:
 * where it sits, its theme and colour, whether its menu and note are open, and the centre of its control `label`.
 */
const ytProbe = (ctx, label = "Download") =>
  evaluate(ctx.sw, `chrome.scripting.executeScript({ target: { tabId: ${ctx.tabId} }, args: [${JSON.stringify(label)}], func: (label) => {
    const host = document.querySelector("endo-download");
    const root = host && chrome.dom.openOrClosedShadowRoot(host);
    if (!root) return { present: false, count: document.querySelectorAll("endo-download").length };
    const target = [...root.querySelectorAll("button")].find((b) => b.textContent.trim() === label);
    const r = target?.getBoundingClientRect();
    const note = root.querySelector(".toast");
    return {
      present: true,
      count: document.querySelectorAll("endo-download").length,
      parent: host.parentElement?.id || host.parentElement?.tagName.toLowerCase(),
      reel: host.closest("ytd-reel-video-renderer")?.id || null,
      after: host.previousElementSibling?.textContent.trim() || null,
      before: host.nextElementSibling?.tagName.toLowerCase() || null,
      round: host.classList.contains("endo-round"),
      dark: host.classList.contains("endo-dark"),
      color: target ? getComputedStyle(target).color : null,
      menu: root.querySelector(".menu").matches(":popover-open"),
      note: note.matches(":popover-open") ? note.textContent : "",
      at: r && r.width ? { x: r.x + r.width / 2, y: r.y + r.height / 2 } : null,
    };
  } }).then(([r]) => r.result)`);

/** A real (trusted) click at a point of the headless page: nothing on the screen moves. */
async function clickAt(ctx, label) {
  const { at } = await ytProbe(ctx, label);
  if (!at) return "missing";
  for (const type of ["mousePressed", "mouseReleased"]) await send("Input.dispatchMouseEvent", { type, x: at.x, y: at.y, button: "left", clickCount: 1 }, ctx.sessionId);
  return "clicked";
}

/** Opens the button's menu and clicks `label` in it; returns the /add body that reached the app, if any. */
async function ytChoose(ctx, app, label) {
  const before = app.requests.length;
  const opened = await clickAt(ctx, "Download");
  const menu = await until(async () => (await ytProbe(ctx)).menu, 3000);
  const chose = await clickAt(ctx, label);
  return { opened, menu, chose, body: (await until(async () => app.since(before, /^\/add$/).at(-1), 4000))?.body };
}

/** Captures the fixture page in YouTube's light and dark theme, once the button has followed it. */
async function captureYoutube(ctx, name) {
  const colors = {};
  for (const scheme of THEMES) {
    await evaluate(ctx.sessionId, `document.documentElement.toggleAttribute("dark", ${scheme === "dark"})`);
    colors[scheme] = (await until(async () => { const p = await ytProbe(ctx); return p.dark === (scheme === "dark") && p; }, 3000))?.color;
    await capture(ctx, `${name}-${scheme}`, { fixture: ctx.name, view: name, theme: scheme });
  }
  await evaluate(ctx.sessionId, `document.documentElement.removeAttribute("dark")`);
  return colors;
}

/** Sets "Show a Download button on YouTube" straight in storage, as the popup's switch does. */
const youtubeSetting = (ctx, on) =>
  evaluate(ctx.sw, `chrome.storage.local.get("settings").then(({ settings }) => chrome.storage.local.set({ settings: { ...settings, youtubeButton: ${on} } })).then(() => true)`);

/** Opens a fixture page in its own window at 900×600, with the page helpers, YouTube's button put in. */
async function youtubePage(ctx) {
  await send("Emulation.setDeviceMetricsOverride", { width: 900, height: 600, deviceScaleFactor: 1, mobile: false }, ctx.sessionId);
  await evaluate(ctx.sessionId, PAGE_HELPERS);
  await injectYoutube(ctx);
  return until(async () => { const p = await ytProbe(ctx); return p.present && p.at && p; }, 5000);
}

// ---- the fixtures: what each page is, what the background must detect, and what the popup must send ----

/** Clicks the first item's Download and returns the /add body that reached the app. */
async function downloadFirst(page, app) {
  const before = app.requests.length;
  await view(page, "Media");
  const clicked = await click(page, "/^\\s*download\\s*$/i");
  const add = await until(async () => app.since(before, /^\/add$/).at(-1), 5000);
  return { clicked, body: add?.body };
}

const FIXTURES = [
  {
    name: "hls-master",
    url: `${FX}/stream.html?src=/hls/master.m3u8&title=Fixture%20HLS%20master`,
    items: 1,
    async act(page, app, ctx) {
      const quality = await evaluate(page.sessionId, "[...document.querySelectorAll('select option')].map((o) => o.textContent)");
      check("hls-master: quality choices list the 3 variants", quality.filter((t) => /\d+p/.test(t)).length === 3, quality);
      const { clicked, body } = await downloadFirst(page, app);
      check("hls-master: Download sends the master URL as HLS", body?.url === `${FX}/hls/master.m3u8` && body?.hls === true, { clicked, url: body?.url, hls: body?.hls });
      check("hls-master: Download sends the page's headers and referer", body?.headers?.["X-Token"] === "abc" && body?.referer === ctx.url, { headers: body?.headers, referer: body?.referer });
      await captureAfter(page, "hls-master", "after-download");
      const picked = await evaluate(page.sessionId, `(() => {
        const s = [...document.querySelectorAll("select")].find((s) => [...s.options].some((o) => /^720p/.test(o.textContent)));
        if (!s) return null;
        s.value = [...s.options].find((o) => /^720p/.test(o.textContent)).value;
        s.dispatchEvent(new Event("change", { bubbles: true }));
        return s.value;
      })()`);
      const chosen = await downloadFirst(page, app);
      check("hls-master: Download with 720p chosen sends that variant", chosen.body?.url === `${FX}/hls/v720.m3u8` && chosen.body?.hls === true, { picked, url: chosen.body?.url });
      await motion(page);
    },
  },
  {
    name: "hls-live",
    url: `${FX}/stream.html?src=/hls/live.m3u8&title=Fixture%20live%20stream`,
    items: 1,
    async act(page) {
      check("hls-live: the item says LIVE", await evaluate(page.sessionId, "/\\bLIVE\\b/.test(document.body.innerText)"));
    },
  },
  {
    name: "hls-aes",
    url: `${FX}/stream.html?src=/hls/aes.m3u8&title=Fixture%20AES%20stream`,
    items: 1,
    async act(page) {
      check("hls-aes: the item says AES-128", await evaluate(page.sessionId, "/AES-128/.test(document.body.innerText)"));
      const clicked = await click(page, "/^\\s*clear( list)?\\s*$/i");
      const emptied = await until(() => evaluate(page.sessionId, "document.querySelectorAll('#items > li').length === 0 && __vis(document.getElementById('empty'))"), 4000);
      check("hls-aes: Clear empties the list and shows the empty state", !!emptied, { clicked });
      await captureAfter(page, "hls-aes", "cleared");
    },
  },
  {
    name: "hls-drm",
    url: `${FX}/stream.html?src=/hls/drm.m3u8&title=Fixture%20DRM%20stream`,
    items: 1,
    async act(page, app) {
      const before = app.requests.length;
      const clicked = await click(page, "/^\\s*download\\s*$/i");
      await sleep(800);
      check("hls-drm: Download is disabled and nothing is sent", clicked === "disabled" && app.since(before, /^\/add$/).length === 0, { clicked });
      check("hls-drm: the item says DRM", await evaluate(page.sessionId, "/\\bDRM\\b/.test(document.body.innerText)"));
      check("hls-drm: no Convert to MP4 switch for what can't be downloaded", await evaluate(page.sessionId, "!__find('label', /^\\s*MP4\\s*$/)"));
    },
  },
  {
    name: "dash",
    url: `${FX}/stream.html?src=/dash/manifest.mpd&title=Fixture%20DASH%20manifest`,
    items: 1,
    async act(page, app) {
      const { clicked, body } = await downloadFirst(page, app);
      check("dash: Download sends the manifest as DASH", body?.url === `${FX}/dash/manifest.mpd` && body?.dash === true, { clicked, url: body?.url, dash: body?.dash });
    },
  },
  {
    name: "file",
    url: `${FX}/stream.html?src=/media/clip.mp4&title=Fixture%20MP4%20file`,
    items: 1,
    async act(page, app, ctx) {
      const { clicked, body } = await downloadFirst(page, app);
      check("file: Download sends the mp4 URL", body?.url === `${FX}/media/clip.mp4` && !body?.hls, { clicked, url: body?.url });
      check("file: Download sends the page's headers, referer and a .mp4 name", body?.headers?.["X-Token"] === "abc" && body?.referer === ctx.url && /\.mp4$/.test(body?.filename || ""), {
        headers: body?.headers,
        referer: body?.referer,
        filename: body?.filename,
      });
    },
  },
  {
    name: "via-browser",
    url: `${FX}/stream.html?src=/media/slow.mp4&title=Fixture%20slow%20file`,
    items: 1,
    async act(page, app) {
      await view(page, "Media");
      const menu = await click(page, "/more actions/i");
      await captureAfter(page, "via-browser", "menu");
      const before = app.requests.length;
      const via = await click(page, "/download via browser/i");
      const start = await until(async () => app.since(before, /^\/record\/start$/).at(-1), 5000);
      check("via-browser: Download via browser starts a download in the app", !!start, { menu, via, title: start?.body?.title });
      const percent = await until(() => evaluate(page.sessionId, "Number(document.querySelector('#rec-list [role=progressbar]:not([hidden])')?.getAttribute('aria-valuenow')) || 0"), 8000);
      check("via-browser: the Record tab shows its progress", percent > 0 && percent < 100, { percent });
      await view(page, "Record");
      await captureAfter(page, "via-browser", "downloading");
      const stop = await click(page, "/^stop browser download/i");
      const end = await until(async () => app.since(before, /^\/record\/[^/]+\/(finish|abort)$/).at(-1), 8000);
      check("via-browser: Stop ends the browser download", !!end, { stop, end: end?.path });
    },
  },
  {
    name: "via-browser-fails",
    url: `${FX}/stream.html?src=/media/gone.mp4&title=Fixture%20gone%20file`,
    items: 1,
    async act(page) {
      await click(page, "/more actions/i");
      const via = await click(page, "/download via browser/i");
      const failed = await until(() => evaluate(page.sessionId, "document.querySelector('#rec-list li.failed')?.innerText"), 8000);
      const title = await evaluate(page.sessionId, "document.getElementById('recs-title')?.textContent");
      check("via-browser-fails: a failed browser download says why and stays, not as in progress", /failed/i.test(failed || "") && title === "Failed", { via, failed, title });
      await view(page, "Record");
      await captureAfter(page, "via-browser-fails", "failed");
      const dismiss = await click(page, "/^dismiss browser download/i");
      const gone = await until(() => evaluate(page.sessionId, "document.querySelectorAll('#rec-list li').length === 0"), 5000);
      check("via-browser-fails: Dismiss removes it", !!gone, { dismiss });
    },
  },
  {
    name: "media-home",
    url: `http://${MEDIA_HOST}/`,
    async act(page, app, ctx) {
      check("media-home: a media site's home page has no media card", await evaluate(page.sessionId, `!__vis(document.getElementById("media-card"))`));
      const before = app.requests.length;
      const clicked = await click(page, "/download this page/i");
      const body = (await until(async () => app.since(before, /^\/add$/).at(-1), 5000))?.body;
      check("media-home: Download this page sends the page URL", body?.url === ctx.url, { clicked, url: body?.url });
      await captureAfter(page, "media-home", "after-send");
    },
  },
  {
    name: "media-site",
    url: `http://${MEDIA_HOST}/video/x8fixture`,
    async act(page, app, ctx) {
      const asked = app.requests.filter((r) => r.path === "/info" && r.body?.url === ctx.url);
      check("media-site: the card asks /info about the page, with no cookies by default", asked.length > 0 && asked.every((r) => !r.body.cookie_jar), { asked: asked.length });
      const card = await mediaCard(page);
      const v = INFO.video;
      check("media-site: the card shows the thumbnail, title, channel and length; no page card", card.thumb && card.title === v.title && card.channel === v.uploader && card.length === "3:33" && !card.pageCard, card);
      check("media-site: qualities are Best and the heights, 4K marked HDR, 1080p and 720p 60 fps; audio M4A/MP3", card.qualities.join() === "Best,4KHDR,1440p,1080p60,720p60,480p,360p" && card.audio, card.qualities);
      check("media-site: subtitles list each language once, automatic ones marked", card.subs.join() === ",all,en,es,de*", card.subs);
      check("media-site: a fresh card picks Best and MP4, with format, subtitles, clip and SponsorBlock", card.quality === "best" && card.container === "mp4" && card.rows.join() === "format,subs,clip,sponsor", card);
      // Use the current time: the fixture page's video plays.
      await evaluate(page.sessionId, `document.getElementById("mc-start-now").click()`);
      const now = await until(() => evaluate(page.sessionId, `document.getElementById("mc-start").value`), 3000);
      check("media-site: Use the current time reads the page's video", /^\d+:\d\d$/.test(now || ""), { start: now });
      // Audio hides the container and subtitles.
      await pick(page, "quality", "audio-mp3");
      const audioRows = (await mediaCard(page)).rows.join();
      check("media-site: audio hides the format and subtitles", audioRows === "clip,sponsor", audioRows);
      await pick(page, "quality", "1080");
      await pick(page, "container", "mkv");
      await pick(page, "sponsorblock", "remove");
      await evaluate(page.sessionId, `(() => {
        const subs = document.getElementById("mc-subs");
        subs.value = "en";
        subs.dispatchEvent(new Event("change", { bubbles: true }));
        document.getElementById("mc-start").value = "2:00";
        document.getElementById("mc-end").value = "1:00";
      })()`);
      // A clip that ends before it starts is refused in the card.
      const refused = await downloadMedia(page, app);
      const why = await until(() => evaluate(page.sessionId, `document.querySelector("#mc-msg.err")?.textContent`), 2000);
      check("media-site: a clip that ends before it starts says so and sends nothing", !refused.body && /start before it ends/.test(why || ""), { why });
      await evaluate(page.sessionId, `document.getElementById("mc-start").value = "0:30"; document.getElementById("mc-end").value = "1:00";`);
      await captureAfter(page, "media-site", "chosen");
      const { clicked, body } = await downloadMedia(page, app);
      const want = { quality: "1080", container: "mkv", subtitles: "en", sections: [[30, 60]], sponsorblock: "remove" };
      check("media-site: Download sends the page with the choice as media, and no cookies", body?.url === ctx.url && body?.referer === ctx.url && JSON.stringify(body?.media) === JSON.stringify(want) && !body?.cookie_jar, { clicked, body });
      await captureAfter(page, "media-site", "sent");

      // The next card opens with that quality, format and subtitles.
      let again = await openPopup(ctx.tabId);
      labels.set(again.sessionId, "popup on media-site, again");
      const kept = await mediaCard(again);
      check("media-site: the next card opens with the last quality, format and subtitles", kept.quality === "1080" && kept.container === "mkv" && kept.sub === "en", kept);

      // "Use my browser sign-in on media sites": the site's cookies (and no other site's) go with /info and /add.
      await evaluate(ctx.sessionId, `document.cookie = "endo_fixture=1; path=/"`);
      await send("Network.setCookie", { name: "elsewhere", value: "1", url: "http://127.0.0.1:8765/" }, ctx.sessionId);
      await view(again, "Settings");
      const off = await evaluate(again.sessionId, `document.getElementById("media-cookies").checked`);
      await evaluate(again.sessionId, `document.getElementById("media-cookies").click()`);
      await sleep(800);
      await captureAfter(again, "media-site", "sign-in-on");
      await send("Target.closeTarget", { targetId: again.targetId });
      const before = app.requests.length;
      again = await openPopup(ctx.tabId);
      labels.set(again.sessionId, "popup on media-site, signed in");
      await mediaCard(again);
      const jar = app.since(before, /^\/info$/).at(-1)?.body?.cookie_jar || [];
      const names = jar.map((c) => `${c.name}@${c.domain}`);
      const ok = (list) => list.some((c) => c.name === "endo_fixture" && c.domain === "www.twitch.tv") && list.every((c) => /(^|\.)twitch\.tv$/.test(c.domain));
      check("media-site: sign-in is off by default; on, /info carries the site's cookies and no other site's", off === false && ok(jar), { off, cookies: names });
      // It reopens on Settings, the last tab shown.
      await view(again, "Media");
      const signed = await downloadMedia(again, app);
      check("media-site: with sign-in on, /add carries them too", ok(signed.body?.cookie_jar || []), {
        clicked: signed.clicked,
        said: signed.said,
        cookies: (signed.body?.cookie_jar || []).map((c) => `${c.name}@${c.domain}`),
      });
      await view(again, "Settings");
      await evaluate(again.sessionId, `document.getElementById("media-cookies").click()`);
      await sleep(800);
      await send("Target.closeTarget", { targetId: again.targetId });
    },
  },
  {
    name: "media-loading",
    url: `http://${MEDIA_HOST}/slow/x8fixture`,
    // /info answers only once released: the captures show the card loading.
    before: (app) => (app.holdInfo = true),
    async act(page, app) {
      const loading = await evaluate(page.sessionId, `document.getElementById("media-card").getAttribute("aria-busy") === "true" && __vis(document.querySelector("#media-card .bones")) && /looking/i.test(document.getElementById("mc-msg").textContent)`);
      app.holdInfo = false;
      app.releaseInfo();
      const card = await mediaCard(page);
      check("media-loading: a skeleton shows until /info answers, then the card", loading && card.title === INFO.video.title, { loading, title: card.title });
      await captureAfter(page, "media-loading", "loaded");
    },
  },
  {
    name: "media-live",
    url: `http://${MEDIA_HOST}/live/x8fixture`,
    async act(page, app, ctx) {
      const card = await mediaCard(page);
      check("media-live: LIVE, From now/From start; no clip, subtitles or SponsorBlock", card.chip === "LIVE" && card.rows.join() === "format,live", card);
      await pick(page, "live", "start");
      await captureAfter(page, "media-live", "from-start");
      const { body } = await downloadMedia(page, app);
      check("media-live: From start sends live_from_start and no clip", body?.url === ctx.url && body?.media?.live_from_start === true && !body?.media?.sections, body?.media);
    },
  },
  {
    name: "media-playlist",
    url: `http://${MEDIA_HOST}/playlist/x8fixture`,
    async act(page, app, ctx) {
      const card = await mediaCard(page);
      check("media-playlist: offers This video / Whole playlist (42), this video first", card.scope === "Whole playlist (42)" && card.rows.includes("scope") && card.rows.includes("clip"), card);
      await pick(page, "scope", "playlist");
      const rows = (await mediaCard(page)).rows;
      check("media-playlist: the whole list takes no clip", !rows.includes("clip"), rows);
      await captureAfter(page, "media-playlist", "whole");
      const { body } = await downloadMedia(page, app);
      check("media-playlist: Whole playlist sends playlist: true", body?.url === ctx.url && body?.media?.playlist === true, body?.media);
    },
  },
  {
    name: "media-error",
    url: `http://${MEDIA_HOST}/error/x8fixture`,
    async act(page, app, ctx) {
      const card = await mediaCard(page);
      check("media-error: the card says why /info failed", card.error === INFO_ERROR && card.qualities.length === 0, card);
      const before = app.requests.length;
      const clicked = await click(page, "/download anyway/i");
      const body = (await until(async () => app.since(before, /^\/add$/).at(-1), 5000))?.body;
      check("media-error: Download anyway sends the page in the best quality", body?.url === ctx.url && JSON.stringify(body?.media) === JSON.stringify({ quality: "best" }), { clicked, body });
      await captureAfter(page, "media-error", "sent");
    },
  },
  {
    name: "youtube",
    url: `${FX}/youtube.html?v=fixture1`,
    popup: false,
    async act(_page, app, ctx) {
      let at = await youtubePage(ctx);
      check("youtube: the button sits right after Share on a watch page", at?.parent === "top-level-buttons-computed" && at.after === "Share" && !at.round && at.count === 1, at);
      await injectYoutube(ctx);
      await sleep(700);
      check("youtube: put in twice, it is still one button", (await ytProbe(ctx)).count === 1);
      const colors = await captureYoutube(ctx, "youtube-watch");
      check("youtube: it follows YouTube's light and dark theme", colors.light === "rgb(15, 15, 15)" && colors.dark === "rgb(241, 241, 241)", colors);
      // The menu, on the user's (trusted) click; captured open in both themes.
      await clickAt(ctx, "Download");
      const menu = await until(async () => (await ytProbe(ctx)).menu, 3000);
      await captureYoutube(ctx, "youtube-menu");
      await clickAt(ctx, "Download");
      check("youtube: a click opens its menu", menu);
      const chosen = await ytChoose(ctx, app, "720p");
      check("youtube: 720p sends the page in 720p", chosen.body?.url === ctx.url && chosen.body?.referer === ctx.url && JSON.stringify(chosen.body?.media) === JSON.stringify({ quality: "720" }), chosen);
      const note = await until(async () => (await ytProbe(ctx)).note, 3000);
      check("youtube: it says the video was sent", /^sent/i.test(note || ""), { note });
      await captureYoutube(ctx, "youtube-sent");
      // A click the page makes up (not the user's) sends nothing.
      const before = app.requests.length;
      await evaluate(ctx.sw, `chrome.scripting.executeScript({ target: { tabId: ${ctx.tabId} }, func: () => {
        const root = chrome.dom.openOrClosedShadowRoot(document.querySelector("endo-download"));
        [...root.querySelectorAll("button")].find((b) => b.textContent.trim() === "Best quality").click();
      } }).then(() => true)`);
      await sleep(1000);
      check("youtube: a scripted click sends nothing", app.since(before, /^\/add$/).length === 0);
      // YouTube's in-page navigation re-renders the actions: the button comes back, for the new video.
      await evaluate(ctx.sessionId, `navigateTo("fixture2")`);
      at = await until(async () => { const p = await ytProbe(ctx); return p.present && p.after === "Share" && p.at && p; }, 5000);
      const next = at && (await ytChoose(ctx, app, "Best quality"));
      check("youtube: after in-page navigation it is back beside Share, for the new video", next?.body?.url === `${FX}/youtube.html?v=fixture2` && next.body.media?.quality === "best", { at, body: next?.body });
      // The Settings switch hides it and shows it again.
      await youtubeSetting(ctx, false);
      const gone = await until(async () => !(await ytProbe(ctx)).present, 3000);
      await youtubeSetting(ctx, true);
      const back = await until(async () => (await ytProbe(ctx)).present, 3000);
      check("youtube: \"Show a Download button on YouTube\" off hides it, on shows it", gone && back, { gone, back });
      // More options… opens the popup where the browser allows it, else says where it is.
      const known = new Set((await send("Target.getTargets")).targetInfos.map((t) => t.targetId));
      await until(async () => (await ytProbe(ctx)).at, 3000);
      await ytChoose(ctx, app, "More options…");
      const outcome = await until(async () => {
        const popup = (await send("Target.getTargets")).targetInfos.find((t) => !known.has(t.targetId) && /popup\.html/.test(t.url));
        if (popup) return (await send("Target.closeTarget", { targetId: popup.targetId }), "the popup opened");
        const said = (await ytProbe(ctx)).note;
        return /toolbar/i.test(said) ? `hint: ${said}` : null;
      }, 5000);
      check("youtube: More options… opens the popup, or says where to find it", !!outcome, { outcome });
      const errors = report.pageErrors.filter((e) => (e.page || "").includes("/youtube.html"));
      check("youtube: no exceptions or console errors on the page", errors.length === 0, errors.slice(0, 5));
    },
  },
  {
    name: "youtube-shorts",
    url: `${FX}/youtube.html?shorts`,
    popup: false,
    async act(_page, app, ctx) {
      const at = await youtubePage(ctx);
      check("youtube-shorts: a round button in the playing Short's column, above its sound", at?.parent === "actions" && at.reel === "reel-1" && at.before === "pivot-button-view-model" && at.round, at);
      await captureYoutube(ctx, "youtube-shorts");
      await evaluate(ctx.sessionId, "nextShort()");
      const moved = await until(async () => { const p = await ytProbe(ctx); return p.reel === "reel-2" && p.at && p; }, 3000);
      check("youtube-shorts: it moves to the next Short", !!moved, moved);
      const { body } = await ytChoose(ctx, app, "Audio (MP3)");
      check("youtube-shorts: Audio (MP3) sends the page as MP3", body?.url === ctx.url && body?.media?.quality === "audio-mp3", body);
    },
  },
  {
    name: "links",
    url: `${FX}/links.html`,
    async act(page, app, ctx) {
      const found = await evaluate(ctx.sessionId, "[...document.querySelectorAll('a[href], img[src]')].map((e) => e.href || e.src)");
      const expected = normalizeLinks(found).map((l) => l.url);
      await view(page, "Links");
      await until(() => evaluate(page.sessionId, "!!__find('button', /^\\s*(select )?all\\s*$/i)"), 3000);
      const all = await click(page, "/^\\s*(select )?all\\s*$/i");
      await captureAfter(page, "links", "selected");
      const before = app.requests.length;
      const sent = await click(page, "/^\\s*send \\d+/i");
      const body = (await until(async () => app.since(before, /^\/add$/).at(-1), 5000))?.body;
      const urls = Array.isArray(body?.urls) ? body.urls : [];
      check(`links: Send sends all ${expected.length} page links in one batch`, urls.length === expected.length && expected.every((u) => urls.includes(u)), {
        all,
        sent,
        got: urls.length,
        missing: expected.filter((u) => !urls.includes(u)),
      });
      check("links: the batch carries the page as referer", body?.referer === ctx.url, body?.referer);
      // Filters: the Archives chip shows only archives, and Send counts only the ticked links still shown.
      const archives = normalizeLinks(found).filter((l) => l.kind === "Archives").length;
      await evaluate(page.sessionId, "[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === 'Archives')?.click()");
      await sleep(200);
      const filtered = await evaluate(page.sessionId, "({ rows: document.querySelectorAll('#links > li').length, send: __find('button', /^\\s*send \\d+/i)?.textContent.trim() })");
      check(`links: the Archives chip leaves the ${archives} archives and Send counts them`, filtered.rows === archives && new RegExp(`^Send ${archives}\\b`).test(filtered.send || ""), filtered);
      await captureAfter(page, "links", "filtered");
      const typed = (q) => evaluate(page.sessionId, `(() => { const i = document.getElementById("links-query"); i.value = ${JSON.stringify(q)}; i.dispatchEvent(new Event("input")); return document.querySelectorAll('#links > li').length; })()`);
      const zips = await typed("zip");
      check("links: typing a filter narrows the list", zips === 1, { rows: zips });
      await typed("/[/");
      await captureAfter(page, "links", "bad-regex");
      check("links: an invalid /regex/ says so", await evaluate(page.sessionId, "/not valid/i.test(document.body.innerText)"));
    },
  },
  {
    name: "video",
    url: `${FX}/video.html`,
    async act(page, app) {
      await view(page, "Record");
      await until(() => evaluate(page.sessionId, "!!__find('button', /record playback/i)"), 4000);
      const before = app.requests.length;
      const rec = await click(page, "/record playback/i");
      const start = await until(async () => app.since(before, /^\/record\/start$/).at(-1), 5000);
      check("video: Record playback starts a recording in the app", !!start, { rec, title: start?.body?.title });
      await sleep(2500);
      await captureAfter(page, "video", "recording");
      const stop = await click(page, "/^\\s*stop/i");
      const done = await until(async () => app.since(before, /^\/record\/[^/]+\/(finish|abort)$/).at(-1), 8000);
      const chunks = app.since(before, /\/chunk$/);
      check("video: Stop finishes the recording with its chunks", done?.path.endsWith("/finish") && chunks.length > 0, { stop, end: done?.path, chunks: chunks.length });
    },
  },
  { name: "unsupported", url: "about:blank" },
];

// ---- run ----

async function main() {
  fs.mkdirSync(OUT, { recursive: true });
  const busy = await fetch(`http://127.0.0.1:${CDP_PORT}/json/version`).then(() => true, () => false);
  if (busy) throw new Error(`Port ${CDP_PORT} is in use: close that browser first.`);
  const fixtures = await startFixtures();
  const app = await startMockApp({ mode: "running" });
  report.mockPort = app.port;
  // Throwaway profile, deleted at the end. A short path: under a deep --out, Edge's LevelDB files pass Windows'
  // 260-character limit and chrome.storage.local fails ("IO error: .../LOCK").
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "eu-"));
  const edge = spawn(
    EDGE,
    [
      "--headless=new",
      `--remote-debugging-port=${CDP_PORT}`,
      `--user-data-dir=${profile}`,
      `--load-extension=${EXT}`,
      `--disable-extensions-except=${EXT}`,
      // Branded builds ignore --load-extension unless this feature is off.
      "--disable-features=DisableLoadExtensionCommandLineSwitch",
      `--host-resolver-rules=MAP ${MEDIA_HOST} 127.0.0.1:8765`,
      "--no-first-run",
      "--no-default-browser-check",
      "--mute-audio",
      "--autoplay-policy=no-user-gesture-required",
      "about:blank",
    ],
    { stdio: "ignore" },
  );
  try {
    const version = await until(() => fetch(`http://127.0.0.1:${CDP_PORT}/json/version`).then((r) => r.json()), 15000);
    if (!version) throw new Error("Edge did not open its debugging port.");
    report.browser = version.Browser;
    ws = new WebSocket(version.webSocketDebuggerUrl);
    ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      if (m.method === "Runtime.exceptionThrown") {
        report.pageErrors.push({ page: labels.get(m.sessionId), text: m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text });
      } else if (m.method === "Runtime.consoleAPICalled" && m.params.type === "error") {
        report.pageErrors.push({ page: labels.get(m.sessionId), text: m.params.args.map((a) => a.value ?? a.description).join(" ") });
      }
      pending.get(m.id)?.(m);
      pending.delete(m.id);
    };
    await new Promise((resolve, reject) => ((ws.onopen = resolve), (ws.onerror = reject)));

    // The extension's service worker, found by its manifest name (Edge runs built-in extensions too).
    const sw = await until(async () => {
      const { targetInfos } = await send("Target.getTargets");
      for (const t of targetInfos.filter((t) => t.type === "service_worker" && t.url.startsWith("chrome-extension://"))) {
        const { sessionId } = await send("Target.attachToTarget", { targetId: t.targetId, flatten: true });
        if ((await evaluate(sessionId, "chrome.runtime.getManifest().name")) === "Endo's Unified Downloader") {
          report.extensionId = new URL(t.url).host;
          return sessionId;
        }
        await send("Target.detachFromTarget", { sessionId });
      }
      return null;
    }, 15000, 500);
    if (!sw) throw new Error("The extension did not load (no service worker).");
    // Without the "tabs" permission a tab shows its URL only where the extension has host access (not about:blank).
    const tabIdOf = (url) => evaluate(sw, `chrome.tabs.query({}).then((tabs) => tabs.find((t) => (t.url || "about:blank") === ${JSON.stringify(url)})?.id ?? null)`);

    const opened = {};
    for (const fx of FIXTURES.filter((f) => !ONLY.length || ONLY.includes(f.name))) {
      try {
        // A page the harness clicks in gets its own window: the one shown, so it takes input.
        const ctx = { ...fx, sw, ...(await open(fx.url, { newWindow: fx.popup === false })) };
        const loaded = await evaluate(ctx.sessionId, "location.href");
        if (loaded !== fx.url) throw new Error(`${fx.url} loaded as ${loaded}`);
        const tabId = await until(() => tabIdOf(fx.url), 3000);
        if (tabId === null) throw new Error(`no tab id for ${fx.url}`);
        opened[fx.name] = ctx.tabId = tabId;
        if (fx.items) {
          const n = await until(
            () => evaluate(sw, `chrome.storage.session.get("tab:${tabId}").then((s) => Object.values(s["tab:${tabId}"]?.items || {}).filter((i) => !i.hidden).length)`),
            6000,
          );
          check(`${fx.name}: the background detects ${fx.items} item(s)`, n === fx.items, `found ${n}`);
        }
        if (fx.popup === false) {
          await fx.act(null, app, ctx);
          continue;
        }
        await fx.before?.(app);
        const page = await openPopup(tabId);
        labels.set(page.sessionId, `popup on ${fx.name}`);
        await captureViews(page, fx.name);
        if (fx.name === "hls-master") await keyboardTabs(page);
        await fx.act?.(page, app, ctx);
        await send("Target.closeTarget", { targetId: page.targetId });
      } catch (error) {
        report.errors.push(`${fx.name}: ${error.stack || error}`);
        check(`${fx.name}: ran without error`, false, String(error.message || error));
      }
    }

    const step = async (name, fn) => {
      try {
        await fn();
      } catch (error) {
        report.errors.push(`${name}: ${error.stack || error}`);
        check(`${name}: ran without error`, false, String(error.message || error));
      }
    };
    const closePage = (page) => send("Target.closeTarget", { targetId: page.targetId });
    const tabId = opened["hls-master"] ?? Object.values(opened)[0];

    // The app status notices, on one detected stream, in every view; with the app gone a Download error stays.
    await step("app status", async () => {
      for (const mode of ["outdated", "absent", "running"]) {
        app.mode = mode;
        const page = await openPopup(tabId);
        labels.set(page.sessionId, `popup, app ${mode}`);
        await captureViews(page, `app-${mode}`);
        const text = await evaluate(page.sessionId, "document.body.innerText");
        const want = { running: /connected/i, outdated: /update|older/i, absent: /not running/i }[mode];
        check(`app status: ${mode} shows in the popup`, want.test(text));
        if (mode === "absent") {
          const clicked = await click(page, "/^\\s*download\\s*$/i");
          const error = await until(() => evaluate(page.sessionId, "document.querySelector('.msg.err')?.innerText"), 10000);
          await sleep(3000);
          const kept = await evaluate(page.sessionId, "!!document.querySelector('.msg.err')");
          check("errors: a failed Download says why and stays", !!error && kept, { clicked, error, kept });
          await captureAfter(page, "app-absent", "download-error");
          const dismissed = await click(page, "/^dismiss$/i");
          check("errors: ✕ dismisses it", await until(() => evaluate(page.sessionId, "!document.querySelector('.msg.err')"), 2000), { dismissed });
        }
        await closePage(page);
      }
    });

    // No fixture can withhold host access from an extension loaded this way: GET_STATE's answer is patched instead.
    await step("host access", async () => {
      const page = await openPopup(tabId, { patch: { hostAccess: false } });
      labels.set(page.sessionId, "popup, no host access");
      await captureViews(page, "host-access");
      check("host access: the notice and its grant button show", await evaluate(page.sessionId, "/site access needed/i.test(document.body.innerText) && !!__find('button', /allow access/i)"), { patched: await evaluate(page.sessionId, "window.__patched") });
      await closePage(page);
    });

    // Settings and the last tab survive closing the popup; then they are put back.
    await step("settings", async () => {
      const set = (min, toggle) =>
        evaluate(page.sessionId, `(() => {
          const i = document.getElementById("min-size");
          i.value = "${min}";
          i.dispatchEvent(new Event("change"));
          ${toggle ? 'document.getElementById("convert-mp4").click();' : ""}
        })()`);
      const read = () => evaluate(page.sessionId, `({ min: document.getElementById("min-size").value, mp4: document.getElementById("convert-mp4").checked, tab: document.querySelector("[role=tab][aria-selected=true]")?.id })`);
      let page = await openPopup(tabId);
      await view(page, "Settings");
      const before = await read();
      await set(400, true);
      await sleep(800);
      await closePage(page);
      page = await openPopup(tabId);
      const after = await read();
      check("settings: min size and Convert to MP4 persist across reopening", after.min === "400" && after.mp4 === !before.mp4, { before, after });
      check("tabs: the popup reopens on the last tab", after.tab === "tab-settings", after);
      await set(before.min, true);
      await sleep(800);
      await closePage(page);
    });

    // Block host from a card's menu (two clicks), then unblock it in Settings. Last: it drops 127.0.0.1's items.
    if (opened.dash) {
      await step("block host", async () => {
        const page = await openPopup(opened.dash);
        await view(page, "Media");
        await click(page, "/more actions/i");
        const first = await click(page, "/^block 127\\.0\\.0\\.1/i");
        await captureAfter(page, "block", "confirm");
        const second = await click(page, "/click again to block/i");
        const dropped = await until(() => evaluate(page.sessionId, "document.querySelectorAll('#items > li').length === 0"), 5000);
        await view(page, "Settings");
        const listed = await until(() => evaluate(page.sessionId, "[...document.querySelectorAll('#blocked li')].map((li) => li.textContent.trim()).join()"), 3000);
        check("block host: two clicks block it, drop its media and list it in Settings", !!dropped && listed === "127.0.0.1", { first, second, listed });
        await captureAfter(page, "block", "settings");
        const unblock = await click(page, "/^unblock 127\\.0\\.0\\.1/i");
        check("block host: ✕ in Settings unblocks it", await until(() => evaluate(page.sessionId, "document.querySelectorAll('#blocked li').length === 0"), 3000), { unblock });
        await closePage(page);
      });
    }

    const overflowing = report.captures.filter((c) => c.overflow).map((c) => `${c.name}: ${c.overflow.join(", ")}`);
    check("layout: nothing overflows sideways in any capture", overflowing.length === 0, overflowing);
    report.lowContrast = [...lowContrast.values()];
    check("contrast: all visible text reaches 4.5:1 in both themes", report.lowContrast.length === 0, report.lowContrast.slice(0, 15));
    const popupErrors = report.pageErrors.filter((e) => /^popup|popup.html/.test(e.page || ""));
    check("popup: no exceptions or console errors", popupErrors.length === 0, popupErrors.slice(0, 5));
    await send("Browser.close").catch(() => {});
  } finally {
    ws?.close();
    // Browser.close normally ended it; otherwise end Edge's whole process tree (it was started by us).
    if (edge.exitCode === null) {
      if (process.platform === "win32") spawnSync("taskkill", ["/pid", String(edge.pid), "/T", "/F"], { stdio: "ignore" });
      else edge.kill();
    }
    await app.close();
    await fixtures.close();
    report.pings = app.requests.filter((r) => r.path === "/ping").length;
    report.requests = app.requests.filter((r) => r.path !== "/ping");
    report.passed = report.checks.length > 0 && report.checks.every((c) => c.pass) && report.errors.length === 0;
    fs.writeFileSync(path.join(OUT, "report.json"), JSON.stringify(report, null, 2));
    // The profile is locked until Edge has exited.
    await sleep(1000);
    fs.rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 });
  }
}

main().then(
  () => {
    const failed = report.checks.filter((c) => !c.pass).length;
    console.log(`\n${report.captures.length} captures, ${report.checks.length - failed}/${report.checks.length} checks passed → ${path.join(OUT, "report.json")}`);
    process.exit(report.passed ? 0 : 1);
  },
  (error) => {
    console.error(error);
    process.exit(2);
  },
);
