// Endo's Unified Downloader: runs in the page's own JavaScript world (MAIN), in every frame,
// before any of the page's scripts. It finds HLS playlists among the responses the page itself
// reads and, only after the user chose "Capture from start" for the tab, copies the media bytes
// the page's player hands to Media Source Extensions. Everything goes to bridge.js over a
// private MessageChannel. It must stay cheap and invisible: patched methods return exactly what
// the originals return, and nothing here may throw into the page.
(() => {
  "use strict";

  /** Largest playlist reported (the background's limit for M3U8_SEEN). */
  const MAX_PLAYLIST = 2 * 1024 * 1024;
  /** Bytes held for bridge.js until its port arrives. */
  const HOLD_LIMIT = 64 * 1024 * 1024;
  /** Tracks per MediaSource the desktop app accepts (track 0..=15). */
  const MAX_TRACKS = 16;
  /** sessionStorage key bridge.js sets so "Capture from start" survives the reload. */
  const ARM_KEY = "__endo_rec_armed";
  const PLAYLIST = /^\s*#EXTM3U/;

  // The page may replace these later; the copies taken now are the browser's own.
  const listen = EventTarget.prototype.addEventListener;
  const sourceOf = Function.prototype.toString;
  const hasOwn = Object.prototype.hasOwnProperty;
  const decoder = new TextDecoder();
  const ignore = () => {};

  /**
   * Replaces the method `name` of `Class.prototype` with the one `wrap` builds around it, carrying
   * the original's name, length and source text so a page that inspects it sees a native method.
   */
  function patch(Class, name, wrap) {
    try {
      const proto = Class.prototype;
      const original = proto[name];
      if (typeof original === "function") proto[name] = disguise(wrap(original), original);
    } catch {}
  }

  function disguise(fn, original) {
    Object.defineProperty(fn, "name", { value: original.name });
    Object.defineProperty(fn, "length", { value: original.length });
    Object.defineProperty(fn, "toString", {
      value: sourceOf.bind(original),
      writable: true,
      configurable: true,
    });
    return fn;
  }

  // ---- The port to bridge.js ----

  let port = null;
  /** Messages waiting for the port: {msg, transfer, size}. */
  let held = [];
  let heldBytes = 0;

  /**
   * Sends a message to bridge.js, holding it until the bridge's port arrives. Returns false when
   * it had to be dropped because the hold is full.
   */
  function post(msg, transfer = []) {
    try {
      if (port) {
        port.postMessage(msg, transfer);
        return true;
      }
      const size = msg.buf ? msg.buf.byteLength : msg.text ? msg.text.length : 0;
      if (heldBytes + size > HOLD_LIMIT && msg.t === "rec-chunk") {
        // Out of room: playlist reports are given up before any recorded bytes.
        held = held.filter((h) => h.msg.t !== "m3u8" || ((heldBytes -= h.size), false));
      }
      if (heldBytes + size > HOLD_LIMIT) return false;
      held.push({ msg, transfer, size });
      heldBytes += size;
      return true;
    } catch {
      return false;
    }
  }

  // bridge.js offers a port at start and again on every hello; only the first one is kept. The
  // offers are kept from the page's own listeners (this one is registered before any of them);
  // bridge.js does the same with the hello.
  listen.call(
    window,
    "message",
    (e) => {
      try {
        if (e.source !== window || !e.data || e.data.__endoBridge !== 1) return;
        e.stopImmediatePropagation();
        if (port || !e.ports[0]) return;
        port = e.ports[0];
        port.onmessage = (m) => {
          if (m.data && m.data.t === "rec-stop") stopCapture();
        };
        for (const h of held) port.postMessage(h.msg, h.transfer);
        held = [];
        heldBytes = 0;
      } catch {}
    },
    true,
  );
  try {
    window.postMessage({ __endoHello: 1 }, "*");
  } catch {}

  // ---- HLS playlists the page reads ----

  const reported = new Set();

  /**
   * Reports `body` (text, or an ArrayBuffer) when it is an HLS playlist, once per URL. Never
   * throws: it runs inside the page's own event listeners and promise chains.
   */
  function inspect(url, body) {
    try {
      // blob:, data: and Responses the page built itself have nothing the downloader can fetch.
      if (!/^https?:/i.test(url)) return;
      let text = body;
      if (typeof body !== "string") {
        if (!body || body.byteLength < 7 || body.byteLength > MAX_PLAYLIST) return;
        // Only the first bytes are decoded unless they start a playlist (after a BOM or blanks).
        const head = new Uint8Array(body, 0, Math.min(body.byteLength, 64));
        if (!PLAYLIST.test(decoder.decode(head))) return;
        text = decoder.decode(body);
      }
      if (text.length > MAX_PLAYLIST || !PLAYLIST.test(text)) return;
      // Low-latency HLS reloads a playlist with _HLS_msn/_HLS_part/_HLS_skip values that change
      // every second; those are all the same playlist.
      const key = url.replace(/[?&]_HLS_[a-z]+=[^&#]*/gi, "");
      if (reported.has(key)) return;
      reported.add(key);
      post({ t: "m3u8", url, text });
    } catch {}
  }

  function onXhrLoad() {
    try {
      const type = this.responseType;
      if (type === "" || type === "text") inspect(this.responseURL, this.responseText);
      else if (type === "arraybuffer") inspect(this.responseURL, this.response);
    } catch {}
  }

  const watchedXhrs = new WeakSet();
  patch(window.XMLHttpRequest, "send", (nativeSend) => ({
    send() {
      try {
        if (!watchedXhrs.has(this)) {
          watchedXhrs.add(this);
          listen.call(this, "load", onXhrLoad);
        }
      } catch {}
      return nativeSend.apply(this, arguments);
    },
  }).send);

  // fetch: only bodies the page reads itself are looked at; nothing is cloned or fetched again.
  for (const name of ["text", "arrayBuffer"]) {
    patch(window.Response, name, (read) => ({
      [name]() {
        const result = read.apply(this, arguments);
        try {
          const url = this.url;
          result.then((body) => inspect(url, body), ignore);
        } catch {}
        return result;
      },
    })[name]);
  }

  // ---- "Capture from start": Media Source Extensions buffer capture ----

  let armed = false;
  try {
    const store = window.sessionStorage;
    const raw = store.getItem(ARM_KEY);
    if (raw !== null) {
      const age = Date.now() - Number(raw);
      armed = age >= 0 && age < 60000;
      // Removed a little later rather than at once, so the page's same-origin frames that load
      // with it are armed too (as FetchV does).
      setTimeout(() => {
        try {
          if (store.getItem(ARM_KEY) === raw) store.removeItem(ARM_KEY);
        } catch {}
      }, 5000);
    }
  } catch {}

  let capturing = false;
  /** MediaSource → its index, the `ms` of its recording. */
  const sourceIndexes = new WeakMap();
  /** SourceBuffer → {ms, track, mime}. */
  const sourceBuffers = new WeakMap();
  /** Recording state by `ms`: {tracks, bytes, ended}. */
  const streams = [];

  /** Tells bridge.js a MediaSource's recording is complete; once, and only if it has bytes. */
  function end(ms) {
    const stream = streams[ms];
    if (stream.ended) return;
    stream.ended = true;
    if (stream.bytes) post({ t: "rec-end", ms });
  }

  function stopCapture() {
    if (!capturing) return;
    capturing = false;
    streams.forEach((_, ms) => end(ms));
  }

  function adopt(mediaSource, sourceBuffer, type) {
    let ms = sourceIndexes.get(mediaSource);
    if (ms === undefined) {
      ms = streams.push({ tracks: 0, bytes: 0, ended: false }) - 1;
      sourceIndexes.set(mediaSource, ms);
      // A player ends the stream after appending its last segment. The wait lets a seek back
      // reopen it, which keeps the recording going.
      listen.call(mediaSource, "sourceended", () =>
        setTimeout(() => {
          try {
            if (mediaSource.readyState !== "open") end(ms);
          } catch {}
        }, 5000),
      );
    }
    const stream = streams[ms];
    if (stream.tracks < MAX_TRACKS) {
      sourceBuffers.set(sourceBuffer, { ms, track: stream.tracks++, mime: String(type) });
    }
  }

  function record(sourceBuffer, data) {
    const info = sourceBuffers.get(sourceBuffer);
    if (!capturing || !info || streams[info.ms].ended) return;
    // Players reuse their buffers, so the bytes are copied before they are handed over.
    const view = ArrayBuffer.isView(data)
      ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
      : new Uint8Array(data);
    if (!view.byteLength) return;
    const buf = view.slice().buffer;
    const msg = { t: "rec-chunk", ms: info.ms, track: info.track, mime: info.mime, buf };
    if (post(msg, [buf])) streams[info.ms].bytes += buf.byteLength;
    // A hole would spoil everything after it, so the recording ends with what it has.
    else end(info.ms);
  }

  function startCapture() {
    capturing = true;
    for (const MS of [window.MediaSource, window.ManagedMediaSource]) {
      if (!MS || !hasOwn.call(MS.prototype, "addSourceBuffer")) continue;
      patch(MS, "addSourceBuffer", (add) => ({
        addSourceBuffer(type) {
          const sourceBuffer = add.apply(this, arguments);
          try {
            adopt(this, sourceBuffer, type);
          } catch {}
          return sourceBuffer;
        },
      }).addSourceBuffer);
    }
    for (const SB of [window.SourceBuffer, window.ManagedSourceBuffer]) {
      if (!SB || !hasOwn.call(SB.prototype, "appendBuffer")) continue;
      patch(SB, "appendBuffer", (append) => ({
        appendBuffer(data) {
          // An append the browser refuses (buffer full, still updating) throws here and is not
          // recorded; the player appends it again later.
          const result = append.apply(this, arguments);
          try {
            record(this, data);
          } catch {}
          return result;
        },
      }).appendBuffer);
    }

    // Players often resume where the viewer left off. For the first 10 s the first seek, and any
    // within 1 s of it, goes to 0 instead, so the capture starts at the beginning (as FetchV).
    try {
      const proto = HTMLMediaElement.prototype;
      const native = Object.getOwnPropertyDescriptor(proto, "currentTime");
      const startedAt = Date.now();
      let firstSeekAt = 0;
      const setter = Object.getOwnPropertyDescriptor(
        {
          set currentTime(value) {
            try {
              const now = Date.now();
              if (now - startedAt < 10000) {
                if (!firstSeekAt) firstSeekAt = now;
                if (now - firstSeekAt < 1000) value = 0;
              }
            } catch {}
            native.set.call(this, value);
          },
        },
        "currentTime",
      ).set;
      Object.defineProperty(proto, "currentTime", { ...native, set: disguise(setter, native.set) });
    } catch {}

    post({ t: "rec-active" });
  }

  if (armed) startCapture();
})();
