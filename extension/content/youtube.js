// Endo's Unified Downloader on YouTube: the content script (isolated world, top frame of www/m.youtube.com) that puts
// a Download button beside Share on watch pages and among the Shorts buttons. Its menu queues the page in the app in
// a quality, through the background and the same POST /add as the popup's media card. The button keeps its place
// across YouTube's in-page navigation and follows its light/dark theme (YouTube's own colour variables). Nothing of
// the page is read but where the button goes, and only the user's own clicks (isTrusted) act: the page can't start
// a download by itself.
(() => {
  "use strict";

  // One per document: the background also puts this into the YouTube tabs open when the extension is installed or
  // updated. One cut off by an update (its extension context gone) gives way to the next.
  try {
    if (typeof globalThis.__endoYoutubeLive === "function" && globalThis.__endoYoutubeLive()) return;
  } catch {}
  const alive = () => {
    try {
      return Boolean(chrome.runtime?.id);
    } catch {
      return false;
    }
  };
  globalThis.__endoYoutubeLive = alive;

  /** Where the button goes, the first one on screen: the watch page's actions (Share among them), the Short playing, the mobile site's action bar. */
  const PLACES = [
    ["ytd-watch-metadata #top-level-buttons-computed", "pill"],
    ["ytd-reel-video-renderer[is-active] #actions, ytd-reel-video-renderer[is-active] reel-action-bar-view-model", "round"],
    ["ytm-slim-video-action-bar-renderer .slim-video-action-bar-actions", "pill"],
  ];
  /** The menu: /add's media.quality and its label. */
  const CHOICES = [
    ["best", "Best quality"],
    ["1080", "1080p"],
    ["720", "720p"],
    ["audio-mp3", "Audio (MP3)"],
    ["audio-m4a", "Audio (M4A)"],
  ];
  /** Phosphor's download icon (regular, MIT licence), as in the popup. */
  const ICON =
    "M224,144v64a8,8,0,0,1-8,8H40a8,8,0,0,1-8-8V144a8,8,0,0,1,16,0v56H208V144a8,8,0,0,1,16,0Zm-101.66,5.66a8,8,0,0,0,11.32,0l40-40a8,8,0,0,0-11.32-11.32L136,124.69V32a8,8,0,0,0-16,0v92.69L93.66,98.34a8,8,0,0,0-11.32,11.32Z";
  // YouTube's own colour variables (they flip with its theme), each falling back to its light or dark value.
  const STYLE = `
    :host { --fg: #0f0f0f; --chip: rgba(0, 0, 0, 0.05); --chip-hover: rgba(0, 0, 0, 0.1); --menu: #fff; --layer: rgba(0, 0, 0, 0.1);
      --toast: #0f0f0f; --toast-fg: #fff; display: inline-flex; margin-left: 8px; font-family: Roboto, Arial, sans-serif; }
    :host(.endo-dark) { --fg: #f1f1f1; --chip: rgba(255, 255, 255, 0.1); --chip-hover: rgba(255, 255, 255, 0.2); --menu: #282828;
      --layer: rgba(255, 255, 255, 0.2); --toast: #f1f1f1; --toast-fg: #0f0f0f; }
    :host(.endo-round) { display: flex; justify-content: center; margin: 0 0 16px; }
    button { border: 0; color: inherit; font: inherit; cursor: pointer; }
    button:focus-visible { outline: 2px solid currentColor; outline-offset: 2px; }
    svg { width: 24px; height: 24px; fill: currentColor; }
    .main { display: inline-flex; align-items: center; gap: 6px; height: 36px; padding: 0 16px 0 12px; border-radius: 18px;
      background: var(--yt-spec-badge-chip-background, var(--chip)); color: var(--yt-spec-text-primary, var(--fg));
      font-size: 14px; font-weight: 500; white-space: nowrap; }
    .main:hover { background: var(--yt-spec-button-chip-background-hover, var(--chip-hover)); }
    .icon { display: grid; place-items: center; }
    :host(.endo-round) .main { flex-direction: column; gap: 4px; height: auto; padding: 0; background: none; font-size: 12px; font-weight: 400; }
    :host(.endo-round) .icon { width: 48px; height: 48px; border-radius: 50%; background: var(--yt-spec-badge-chip-background, var(--chip)); }
    :host(.endo-round) .main:hover .icon { background: var(--yt-spec-button-chip-background-hover, var(--chip-hover)); }
    .menu, .toast { position: fixed; inset: auto; box-sizing: border-box; margin: 0; border: 0; font-size: 14px; }
    .menu { width: 220px; padding: 8px 0; border-radius: 12px; background: var(--yt-spec-menu-background, var(--menu));
      color: var(--yt-spec-text-primary, var(--fg)); box-shadow: 0 4px 32px rgba(0, 0, 0, 0.1); }
    .menu:popover-open { display: grid; }
    .item { height: 36px; padding: 0 16px; background: none; text-align: left; }
    .item:hover, .item:focus-visible { background: var(--yt-spec-10-percent-layer, var(--layer)); outline: 0; }
    hr { width: 100%; margin: 8px 0; border: 0; border-top: 1px solid var(--yt-spec-10-percent-layer, var(--layer)); }
    .toast { max-width: 320px; padding: 10px 14px; border-radius: 8px; background: var(--yt-spec-inverted-background, var(--toast));
      color: var(--yt-spec-text-primary-inverse, var(--toast-fg)); }
  `;

  /** Creates an element with the given properties and children (never HTML: YouTube enforces Trusted Types). */
  function el(tag, props = {}, ...children) {
    const node = Object.assign(document.createElement(tag), props);
    node.append(...children);
    return node;
  }

  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 256 256");
  svg.setAttribute("aria-hidden", "true");
  svg.append(document.createElementNS("http://www.w3.org/2000/svg", "path"));
  svg.firstChild.setAttribute("d", ICON);
  const main = el("button", { type: "button", className: "main", title: "Download with Endo's Unified Downloader" }, el("span", { className: "icon" }, svg), el("span", { textContent: "Download" }));
  main.setAttribute("aria-haspopup", "menu");
  const menu = el("div", { className: "menu", popover: "auto" });
  menu.setAttribute("role", "menu");
  menu.setAttribute("aria-label", "Download with Endo's Unified Downloader");
  main.popoverTargetElement = menu;
  const note = el("p", { className: "toast", popover: "manual" });
  note.setAttribute("role", "status");
  // Closed: the page's scripts can't reach into the button or its menu.
  const host = document.createElement("endo-download");
  host.attachShadow({ mode: "closed" }).append(el("style", { textContent: STYLE }), main, menu, note);

  /** A menu entry that closes the menu and runs `act`, on the user's own click only. */
  function item(text, act) {
    const node = el("button", { type: "button", className: "item", textContent: text });
    node.setAttribute("role", "menuitem");
    node.addEventListener("click", (event) => {
      if (!event.isTrusted) return;
      menu.hidePopover();
      act();
    });
    return node;
  }
  menu.append(...CHOICES.map(([quality, label]) => item(label, () => send(quality))), el("hr"), item("More options…", openPopup));

  /**
   * Puts the menu or the note under the button, or over it where `height` px don't fit below, inside the window.
   * ponytail: the menu's height is a constant (~250 px); measure it if the menu grows.
   */
  function place(box, height) {
    const at = main.getBoundingClientRect();
    const up = at.bottom + height > innerHeight;
    Object.assign(box.style, {
      left: `${Math.max(8, Math.min(at.left, innerWidth - 228))}px`,
      top: up ? "auto" : `${at.bottom + 4}px`,
      bottom: up ? `${innerHeight - at.top + 4}px` : "auto",
    });
  }
  menu.addEventListener("beforetoggle", (event) => event.newState === "open" && place(menu, 260));

  let noteTimer = 0;
  /** Says how a click went, for 4 s. */
  function say(text) {
    note.textContent = text;
    if (!host.isConnected) return;
    place(note, 60);
    if (!note.matches(":popover-open")) note.showPopover();
    clearTimeout(noteTimer);
    noteTimer = setTimeout(() => note.matches(":popover-open") && note.hidePopover(), 4000);
  }

  /** Asks the background; always an object, with `error` when it failed. */
  async function ask(msg) {
    try {
      return (await chrome.runtime.sendMessage(msg)) || { ok: false, error: "No answer from the extension." };
    } catch (error) {
      return { ok: false, error: alive() ? String(error?.message || error) : "The extension was updated: reload this page." };
    }
  }

  async function send(quality) {
    say("Sending…");
    const reply = await ask({ cmd: "SEND_MEDIA", url: location.href, quality });
    say(reply.ok ? "Sent to Endo's Unified Downloader." : reply.error || "Failed.");
  }

  async function openPopup() {
    if (!(await ask({ cmd: "OPEN_POPUP" })).ok) say("For more options, click Endo's Unified Downloader in the browser's toolbar.");
  }

  // ---- Keeping the button in place ----

  /** "Show a Download button on YouTube" (Settings); off until the setting is read. */
  let enabled = false;
  let queued = false;

  /** Puts the button where it belongs now, at most 4 times a second (YouTube's page changes all the time). */
  function schedule() {
    if (queued) return;
    queued = true;
    setTimeout(settle, 250);
  }

  function settle() {
    queued = false;
    if (!alive()) return stop();
    host.classList.toggle("endo-dark", document.documentElement.hasAttribute("dark"));
    const found = enabled && PLACES.map(([selector, look]) => [document.querySelector(selector), look]).find(([node]) => node?.getClientRects().length);
    if (!found) return host.remove();
    const [parent, look] = found;
    host.classList.toggle("endo-round", look === "round");
    // In a Short's column, above the sound's button at its foot.
    if (host.parentNode !== parent) parent.insertBefore(host, parent.querySelector(":scope > pivot-button-view-model, :scope > #pivot-button"));
  }

  const observer = new MutationObserver(schedule);

  function stop() {
    observer.disconnect();
    removeEventListener("yt-navigate-finish", schedule, true);
    host.remove();
  }

  observer.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["is-active", "dark"] });
  addEventListener("yt-navigate-finish", schedule, true);
  chrome.storage.local.get("settings").then(({ settings }) => {
    enabled = settings?.youtubeButton !== false;
    schedule();
  }, () => {});
  chrome.storage.onChanged.addListener((changes, area) => {
    if (area !== "local" || !changes.settings) return;
    enabled = changes.settings.newValue?.youtubeButton !== false;
    schedule();
  });
})();
