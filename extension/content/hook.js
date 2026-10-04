// Endo's Unified Downloader: runs in the page's own JavaScript world (MAIN), in every frame,
// before any of the page's scripts. It finds HLS playlists, DASH manifests and JSON naming either
// among the responses the page itself reads and, only after the user chose "Capture from start"
// for the tab, copies the media bytes the page's player hands to Media Source Extensions.
// Everything goes to bridge.js over a private MessageChannel. It must stay cheap and invisible:
// patched methods return exactly what the originals return, and nothing here may throw into the
// page.
(() => {
  "use strict";

  /** Largest playlist reported (the background's limit for M3U8_SEEN). */
  const MAX_PLAYLIST = 2 * 1024 * 1024;
  /** Largest JSON text body looked through. */
  const MAX_JSON = 2 * 1024 * 1024;
  /** How much of a parsed JSON value is looked through: depth, strings, and values of any kind. */
  const MAX_DEPTH = 6;
  const MAX_STRINGS = 2000;
  const MAX_VALUES = 20000;
  /** Longest link reported (the bridge's limit). */
  const MAX_URL = 32 * 1024;
  /** Characters at the start of a body that tell a DASH manifest. */
  const HEAD = 4096;
  /** Bytes held for bridge.js until its port arrives. */
  const HOLD_LIMIT = 64 * 1024 * 1024;
  /** Tracks per MediaSource the desktop app accepts (track 0..=15), and parts per track (0..=255). */
  const MAX_TRACKS = 16;
  const MAX_PART = 255;
  /** sessionStorage key bridge.js sets so "Capture from start" survives the reload. */
  const ARM_KEY = "__endo_rec_armed";
  const PLAYLIST = /^\s*#EXTM3U/;
  const MANIFEST = /^\s*<(?:\?xml|MPD)/;
  const JSON_TEXT = /^\s*[[{]/;
  /** A JSON body is parsed only when it holds one of these. */
  const JSON_HINT = /#EXTM3U|\.m3u8|\.mpd/i;
  const STREAM_LINK = /^https?:\/\/\S*\.(?:m3u8|mpd)/i;
  /** A master playlist, or a media playlist that is not live. */
  const FINISHED = /#EXT-X-STREAM-INF|#EXT-X-ENDLIST|#EXT-X-PLAYLIST-TYPE:VOD/;

  // The page may replace these later; the copies taken now are the browser's own.
  const listen = EventTarget.prototype.addEventListener;
  const sourceOf = Function.prototype.toString;
  const hasOwn = Object.prototype.hasOwnProperty;
  const parseJson = JSON.parse;
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
  /** Which bridge.js instance `port` came from. */
  let bridgeId = null;
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
  // bridge.js does the same with the hello. An offer from another bridge than the one whose port
  // was taken passes on: after an extension update, the bridge and hook injected into an open tab
  // pair up past this (now orphaned) hook. The handshake's version (2) keeps the scripts of
  // earlier versions left in open tabs from swallowing this one's.
  listen.call(
    window,
    "message",
    (e) => {
      try {
        if (e.source !== window || !e.data || e.data.__endoBridge !== 2) return;
        if (port && e.data.id !== bridgeId) return;
        e.stopImmediatePropagation();
        if (port || !e.ports[0]) return;
        port = e.ports[0];
        bridgeId = e.data.id;
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
    window.postMessage({ __endoHello: 2 }, "*");
  } catch {}

  // ---- Streams the page reads ----

  /** Reported already: playlist URLs (the text, for inline playlists), "mpd:" and "link:" URLs. */
  const reported = new Set();

  function once(key, msg) {
    if (reported.has(key)) return;
    reported.add(key);
    post(msg);
  }

  /**
   * Reports playlist `text`. `inline` when `url` does not serve it: the page built it (blob:,
   * data:), or it came inside JSON. The background then sends the text itself to the app, so
   * only a master or a finished media playlist is: a live one cannot be downloaded from its text
   * (there is nothing to reload), and the page builds it again, different, every few seconds.
   */
  function reportPlaylist(url, text, inline) {
    if (text.length > MAX_PLAYLIST) return;
    if (inline) {
      if (!FINISHED.test(text)) return;
      // A data: URL is the playlist itself; the background goes by the frame's URL anyway.
      once(text, { t: "m3u8", url: /^data:/i.test(url) ? "data:" : url, text, inline: true });
    } else {
      // Low-latency HLS reloads a playlist with _HLS_msn/_HLS_part/_HLS_skip values that change
      // every second; those are all the same playlist.
      once(url.replace(/[?&]_HLS_[a-z]+=[^&#]*/gi, ""), { t: "m3u8", url, text });
    }
  }

  /**
   * Looks through a JSON value the page parsed (only so deep and so much of it) for playlist
   * text and links to HLS playlists or DASH manifests.
   */
  function scanJson(url, value) {
    try {
      if (!/^https?:/i.test(url)) return;
      let strings = MAX_STRINGS;
      let values = MAX_VALUES;
      const walk = (v, depth) => {
        if (typeof v === "string") {
          strings--;
          if (PLAYLIST.test(v)) reportPlaylist(url, v, true);
          else if (v.length <= MAX_URL && STREAM_LINK.test(v)) once("link:" + v, { t: "media-url", url: v });
        } else if (v && typeof v === "object" && depth < MAX_DEPTH) {
          const keys = Array.isArray(v) ? null : Object.keys(v);
          const count = keys ? keys.length : v.length;
          for (let i = 0; i < count && strings > 0 && values-- > 0; i++) walk(v[keys ? keys[i] : i], depth + 1);
        }
      };
      walk(value, 0);
    } catch {}
  }

  /**
   * Reports what a body the page read (text, or an ArrayBuffer) holds: an HLS playlist, a DASH
   * manifest, or JSON naming either. Never throws: it runs inside the page's own event listeners
   * and promise chains.
   */
  function inspect(url, body) {
    try {
      if (typeof url !== "string") return;
      // Responses the page built itself have no URL; a blob: or data: playlist is reported inline.
      const inline = /^(?:blob|data):/i.test(url);
      if (!inline && !/^https?:/i.test(url)) return;
      let text = body;
      if (typeof body !== "string") {
        if (!body || body.byteLength < 7) return;
        // Only the first bytes are decoded unless they start a playlist or a manifest (after a
        // BOM or blanks); binary JSON bodies are not looked at.
        const head = decoder.decode(new Uint8Array(body, 0, Math.min(body.byteLength, 64)));
        if (PLAYLIST.test(head) && body.byteLength <= MAX_PLAYLIST) text = decoder.decode(body);
        else if (MANIFEST.test(head) && !inline) text = decoder.decode(new Uint8Array(body, 0, Math.min(body.byteLength, HEAD)));
        else return;
      }
      if (PLAYLIST.test(text)) reportPlaylist(url, text, inline);
      else if (inline) return;
      else if (MANIFEST.test(text) && text.slice(0, HEAD).includes("<MPD")) once("mpd:" + url, { t: "mpd", url });
      // JSON is parsed only when it names a stream, so most API responses cost one regex test.
      else if (text.length <= MAX_JSON && JSON_TEXT.test(text) && JSON_HINT.test(text)) scanJson(url, parseJson(text));
    } catch {}
  }

  function onXhrLoad() {
    try {
      const type = this.responseType;
      if (type === "" || type === "text") inspect(this.responseURL, this.responseText);
      else if (type === "arraybuffer") inspect(this.responseURL, this.response);
      else if (type === "json") scanJson(this.responseURL, this.response);
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
  for (const [name, look] of [["text", inspect], ["arrayBuffer", inspect], ["json", scanJson]]) {
    patch(window.Response, name, (read) => ({
      [name]() {
        const result = read.apply(this, arguments);
        try {
          const url = this.url;
          result.then((body) => look(url, body), ignore);
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
  /**
   * SourceBuffer → {ms, track, mime, part, media}: `part` counts the init segments that came
   * after media (a quality switch, or a seek that loads a new init), `media` whether the
   * current part has media yet.
   */
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
      sourceBuffers.set(sourceBuffer, { ms, track: stream.tracks++, mime: String(type), part: 0, media: false });
    }
  }

  const boxType = (b, at) => String.fromCharCode(b[at + 4], b[at + 5], b[at + 6], b[at + 7]);

  /** Whether an append starts with an init segment: an fMP4 ftyp/moov box, or a WebM EBML header. */
  function startsWithInit(b) {
    if (b.length < 8) return false;
    const type = boxType(b, 0);
    return type === "ftyp" || type === "moov" || (b[0] === 0x1a && b[1] === 0x45 && b[2] === 0xdf && b[3] === 0xa3);
  }

  /**
   * Whether bytes that start with an init segment carry media after it: an fMP4 moof or mdat box,
   * or a WebM Cluster (its ID turns up in a header only by a one in four billion chance).
   */
  function holdsMedia(b) {
    if (b[0] === 0x1a && b[1] === 0x45 && b[2] === 0xdf && b[3] === 0xa3) {
      for (let at = 4; at + 4 <= b.length; at++) {
        if (b[at] === 0x1f && b[at + 1] === 0x43 && b[at + 2] === 0xb6 && b[at + 3] === 0x75) return true;
      }
      return false;
    }
    for (let at = 0; at + 8 <= b.length; ) {
      const type = boxType(b, at);
      if (type === "moof" || type === "mdat") return true;
      const size = ((b[at] << 24) | (b[at + 1] << 16) | (b[at + 2] << 8) | b[at + 3]) >>> 0;
      if (size < 8) return false;
      at += size;
    }
    return false;
  }

  function record(sourceBuffer, data) {
    const info = sourceBuffers.get(sourceBuffer);
    if (!capturing || !info || streams[info.ms].ended) return;
    // Players reuse their buffers, so the bytes are copied before they are handed over.
    const view = ArrayBuffer.isView(data)
      ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
      : new Uint8Array(data);
    if (!view.byteLength) return;
    // An init segment after media starts a new part: the app joins parts with the same init and
    // re-encodes the others, as a file cannot change its init halfway.
    if (!startsWithInit(view)) info.media = true;
    else {
      if (info.media && info.part < MAX_PART) info.part++;
      info.media = holdsMedia(view);
    }
    const buf = view.slice().buffer;
    const msg = { t: "rec-chunk", ms: info.ms, track: info.track, part: info.part, mime: info.mime, buf };
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
