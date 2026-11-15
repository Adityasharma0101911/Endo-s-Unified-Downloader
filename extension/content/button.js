// Endo's Unified Downloader's Download button over a page's videos (isolated world, every frame but YouTube's, which
// has its own: youtube.js). It shows while the pointer is over a <video> drawn larger than 200×150 and sends that
// video to the app through the background (VIDEO_SEND), as the popup's Download would. It lives in a closed shadow
// root, so the page can neither style nor reach it, takes only the user's own clicks (isTrusted), and keeps them from
// the page's player. "Show a Download button on videos" (Settings, on by default) turns it off.
(() => {
  "use strict";

  if (/(^|\.)(youtube\.com|youtube-nocookie\.com)$/.test(location.hostname)) return;
  // One per document: the background also puts this into the tabs open when the extension is installed or updated.
  // One cut off by an update (its extension context gone) gives way to the next.
  try {
    if (typeof globalThis.__endoVideoButtonLive === "function" && globalThis.__endoVideoButtonLive()) return;
  } catch {}
  const alive = () => {
    try {
      return Boolean(chrome.runtime?.id);
    } catch {
      return false;
    }
  };
  globalThis.__endoVideoButtonLive = alive;

  const MIN_WIDTH = 200;
  const MIN_HEIGHT = 150;
  const videos = document.getElementsByTagName("video");
  let enabled = false;
  let blocked = false;
  let pointer = null;
  let pending = false;
  /** The video the button is over, and whether a send of it is under way (the button then stays). */
  let current = null;
  let busy = false;
  let host = null;
  let button = null;
  let label = null;

  function build() {
    host = document.createElement("endo-video-button");
    // Inline !important: a page's own CSS can't move, hide or show it.
    host.style.cssText = "all: initial !important; position: fixed !important; top: 0 !important; left: 0 !important; width: 0 !important; height: 0 !important; z-index: 2147483647 !important; pointer-events: none !important;";
    const root = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      button { position: fixed; display: inline-flex; align-items: center; gap: 6px; height: 30px; padding: 0 12px 0 10px;
        border: 1px solid rgba(255, 255, 255, 0.25); border-radius: 15px; background: rgba(17, 17, 23, 0.82); color: #fff;
        font: 600 13px/1 system-ui, -apple-system, "Segoe UI", sans-serif; letter-spacing: 0; cursor: pointer;
        pointer-events: auto; box-shadow: 0 2px 10px rgba(0, 0, 0, 0.35); transform: translateX(-100%); }
      button:hover { background: rgba(37, 37, 48, 0.92); }
      button:focus-visible { outline: 2px solid #8ab4ff; outline-offset: 2px; }
      button[disabled] { cursor: default; opacity: 0.85; }
      button[hidden] { display: none; }
      svg { width: 16px; height: 16px; flex: none; }
      @media (prefers-reduced-motion: no-preference) { button { transition: background 0.12s; } }
    `;
    button = document.createElement("button");
    button.type = "button";
    button.hidden = true;
    button.title = "Send this video to Endo's Unified Downloader";
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("viewBox", "0 0 256 256");
    svg.setAttribute("aria-hidden", "true");
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("fill", "currentColor");
    path.setAttribute(
      "d",
      "M224 144v64a8 8 0 0 1-8 8H40a8 8 0 0 1-8-8v-64a8 8 0 0 1 16 0v56h160v-56a8 8 0 0 1 16 0Zm-101.66 5.66a8 8 0 0 0 11.32 0l40-40a8 8 0 0 0-11.32-11.32L136 124.69V32a8 8 0 0 0-16 0v92.69l-26.34-26.35a8 8 0 0 0-11.32 11.32Z",
    );
    svg.append(path);
    label = document.createElement("span");
    label.textContent = "Download";
    button.append(svg, label);
    // The page's player must not take these as its own (a click there pauses the video or opens its overlay).
    for (const type of ["pointerdown", "pointerup", "mousedown", "mouseup", "click", "dblclick", "contextmenu"]) {
      button.addEventListener(type, (event) => event.stopPropagation());
    }
    button.addEventListener("click", onClick);
    root.append(style, button);
  }

  function hide() {
    if (busy || !button) return;
    button.hidden = true;
    current = null;
  }

  /** Puts the button at the top right of `rect`, `video`'s place on screen. */
  function show(video, rect) {
    if (!host) build();
    if (!host.isConnected) document.documentElement.append(host);
    current = video;
    button.style.top = `${Math.max(0, rect.top) + 10}px`;
    button.style.left = `${Math.min(innerWidth, rect.right) - 10}px`;
    button.hidden = false;
  }

  /**
   * The video drawn under (x, y), large enough for the button, with its place; null for none.
   * shortcut: only videos in the document itself, not in a page's own shadow roots; walk those if a site needs it.
   */
  function videoAt(x, y) {
    for (const video of videos) {
      const rect = video.getBoundingClientRect();
      if (rect.width >= MIN_WIDTH && rect.height >= MIN_HEIGHT && x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom) {
        return [video, rect];
      }
    }
    return null;
  }

  function update() {
    pending = false;
    if (!alive()) return stop();
    if (busy) return;
    const found = enabled && !blocked && pointer && videos.length > 0 && !document.fullscreenElement ? videoAt(...pointer) : null;
    if (found) show(...found);
    else hide();
  }

  function onMove(event) {
    pointer = [event.clientX, event.clientY];
    if (!pending) {
      pending = true;
      requestAnimationFrame(update);
    }
  }

  async function onClick(event) {
    if (!event.isTrusted || busy || !current) return;
    event.preventDefault();
    busy = true;
    button.disabled = true;
    label.textContent = "Sending…";
    const src = current.currentSrc || current.src || "";
    let reply;
    try {
      reply = await chrome.runtime.sendMessage({ cmd: "VIDEO_SEND", src });
    } catch (error) {
      reply = { ok: false, error: String(error?.message || error) };
    }
    const ok = reply?.ok === true;
    label.textContent = ok ? "Sent to the app" : "Not sent";
    button.title = ok ? "Send this video to Endo's Unified Downloader" : reply?.error || "Not sent.";
    setTimeout(() => {
      busy = false;
      button.disabled = false;
      label.textContent = "Download";
      update();
    }, ok ? 2000 : 4000);
  }

  /** The pointer left the window (or this frame): the button goes. */
  function onOut(event) {
    if (event.relatedTarget) return;
    pointer = null;
    update();
  }

  function stop() {
    removeEventListener("mousemove", onMove, true);
    removeEventListener("mouseout", onOut, true);
    removeEventListener("scroll", hide, true);
    host?.remove();
  }

  /** The settings that say whether the button shows here: on, and this site not blocked (subdomains included). */
  function apply(settings) {
    enabled = settings?.videoButton !== false;
    const here = location.hostname.toLowerCase();
    const hosts = Array.isArray(settings?.blockedHosts) ? settings.blockedHosts : [];
    blocked = hosts.some((name) => typeof name === "string" && (here === name || here.endsWith(`.${name}`)));
    update();
  }

  addEventListener("mousemove", onMove, { capture: true, passive: true });
  addEventListener("mouseout", onOut, { capture: true, passive: true });
  addEventListener("scroll", hide, { capture: true, passive: true });
  chrome.storage.local.get("settings").then(({ settings }) => apply(settings), () => {});
  chrome.storage.onChanged.addListener((changes, area) => {
    if (area === "local" && changes.settings) apply(changes.settings.newValue);
  });
})();
