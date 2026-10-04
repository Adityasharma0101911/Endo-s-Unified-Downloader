// Endo's Unified Downloader: the content script in the extension's isolated world, in every
// frame. It passes what hook.js (the page's world) finds on to the background service worker,
// arms "Capture from start" for the reload, changes the playback speed of a buffer capture,
// records one video with MediaRecorder ("Record playback"), and downloads a stream or file with
// the page's own requests ("Via browser") for sites that refuse the desktop app.
(() => {
  "use strict";

  // One bridge per document: Firefox puts the manifest's scripts into the tabs open when the
  // extension is installed or updated, and a tab that loads meanwhile gets them twice in Chrome;
  // two bridges would both start every download and recording. One cut off by an update (its
  // extension context gone) gives way to the next.
  try {
    if (typeof globalThis.__endoBridgeLive === "function" && globalThis.__endoBridgeLive()) return;
  } catch {}
  const runtime = chrome.runtime;
  globalThis.__endoBridgeLive = () => !orphaned && Boolean(runtime?.id);

  /** sessionStorage key hook.js reads at the next document start. */
  const ARM_KEY = "__endo_rec_armed";
  const MAX_PLAYLIST = 2 * 1024 * 1024;
  const MAX_URL = 32 * 1024;
  /** Raw bytes per REC_CHUNK: base64 adds a third, and Chrome refuses messages over 64 MiB. */
  const SLICE = 8 * 1024 * 1024;
  /** Bytes per ranged request of a browser file download. */
  const RANGE = 8 * 1024 * 1024;
  /** Segments a browser download fetches at once, and tries per request. */
  const IN_FLIGHT = 2;
  const ATTEMPTS = 3;
  /** Volume a muted video plays at while it is recorded: Chrome records a muted video's sound as silence. */
  const QUIET = 0.0001;
  const SPEEDS = [1, 2, 4, 8, 16];
  const RECORDER_TYPES = [
    "video/mp4;codecs=avc1,mp4a.40.2",
    "video/webm;codecs=vp9,opus",
    "video/webm;codecs=vp8,opus",
    "video/webm",
  ];

  /** Ports offered to hook.js. It keeps the first that reaches it, so `rec-stop` goes to all. */
  const hookPorts = [];
  /**
   * Recording messages wait their turn here: FileReader is asynchronous, and the background must
   * get every chunk, and the REC_END after them, in the order they were recorded.
   */
  let queue = Promise.resolve();
  /** Set once the extension was reloaded or removed; nothing reaches it from here any more. */
  let orphaned = false;
  /** The MediaRecorder recording in this frame: {recorder, closed}, or null. */
  let msr = null;
  /** The hook's buffer recordings (its `ms`) this document sent bytes of and has not ended. */
  const openMs = new Set();
  /** Set on pagehide: the buffer recordings ended with the page; bytes still on their way are dropped. */
  let hidden = false;

  /**
   * Sends to the background; false when the extension is gone. The default callback reads
   * lastError so a sleeping or missing listener is not reported as an error.
   */
  function send(msg, done = () => void chrome.runtime.lastError) {
    if (orphaned) return false;
    try {
      chrome.runtime.sendMessage(msg, done);
      return true;
    } catch {
      // "Extension context invalidated": stop recording for a background that is gone.
      orphaned = true;
      abandonMsr();
      toHook({ t: "rec-stop" });
      return false;
    }
  }

  /** Sends to the background and resolves with its reply. */
  function ask(msg) {
    return new Promise((resolve, reject) => {
      const sent = send(msg, (reply) => {
        const error = chrome.runtime.lastError;
        if (error) reject(new Error(error.message));
        else resolve(reply);
      });
      if (!sent) reject(new Error("The extension was reloaded or removed; reload the page."));
    });
  }

  function later(step) {
    queue = queue.then(step).catch(() => {});
  }

  function toBase64(blob) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result.slice(reader.result.indexOf(",") + 1));
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(blob);
    });
  }

  /**
   * Queues `blob` as REC_CHUNK messages; those of a MediaRecorder `session`, or of a buffer
   * recording, that ended meanwhile are dropped.
   */
  function relay(ms, track, part, mime, blob, session) {
    for (let at = 0; at < blob.size; at += SLICE) {
      const slice = blob.slice(at, at + SLICE);
      later(() =>
        toBase64(slice).then((data) => {
          if (session ? !session.closed : openMs.has(ms)) send({ cmd: "REC_CHUNK", ms, track, part, mime, data });
        }),
      );
    }
  }

  // ---- hook.js ----

  function toHook(msg) {
    for (const port of hookPorts) {
      try {
        port.postMessage(msg);
      } catch {}
    }
  }

  const isMs = (ms) => Number.isSafeInteger(ms) && ms >= 0;
  const isUrl = (url, scheme) => typeof url === "string" && url.length <= MAX_URL && scheme.test(url);
  const HTTP = /^https?:\/\//i;
  /** Where a playlist the page built itself came from (the background goes by the frame's URL). */
  const INLINE = /^(?:https?:\/\/|blob:|data:)/i;

  /** Passes on what hook.js found. It comes from the page's world, so each message is checked. */
  function fromHook(e) {
    const m = e.data;
    if (orphaned || !m || typeof m !== "object") return;
    if (m.t === "m3u8") {
      if (typeof m.text !== "string" || m.text.length > MAX_PLAYLIST) return;
      if (m.inline === true) {
        if (isUrl(m.url, INLINE)) send({ cmd: "M3U8_SEEN", url: m.url, text: m.text, inline: true });
      } else if (isUrl(m.url, HTTP)) {
        send({ cmd: "M3U8_SEEN", url: m.url, text: m.text });
      }
    } else if (m.t === "media-url" || m.t === "mpd") {
      if (isUrl(m.url, HTTP)) send({ cmd: m.t === "mpd" ? "MPD_SEEN" : "MEDIA_URL_SEEN", url: m.url });
    } else if (m.t === "rec-active") {
      send({ cmd: "REC_ACTIVE" });
    } else if (m.t === "rec-chunk") {
      const part = m.part === undefined ? 0 : m.part;
      if (
        !hidden &&
        isMs(m.ms) &&
        Number.isInteger(m.track) &&
        m.track >= 0 &&
        m.track <= 15 &&
        Number.isInteger(part) &&
        part >= 0 &&
        part <= 255 &&
        typeof m.mime === "string" &&
        m.mime.length <= 255 &&
        m.buf instanceof ArrayBuffer
      ) {
        openMs.add(m.ms);
        relay(m.ms, m.track, part, m.mime, new Blob([m.buf]), null);
      }
    } else if (m.t === "rec-end") {
      if (isMs(m.ms)) later(() => openMs.delete(m.ms) && send({ cmd: "REC_END", ms: m.ms }));
    }
  }

  /** Tells this bridge's port offers from those of a bridge injected after an extension update. */
  const BRIDGE_ID = Math.random().toString(36).slice(2);

  function offerPort() {
    try {
      const channel = new MessageChannel();
      channel.port1.onmessage = fromHook;
      hookPorts.push(channel.port1);
      window.postMessage({ __endoBridge: 2, id: BRIDGE_ID }, "*", [channel.port2]);
    } catch {}
  }

  // Whichever of hook.js and this script starts first, the hook gets a port: one is offered now
  // and another whenever the hook says hello. The hello is kept from the page's listeners. Once
  // the extension is updated this bridge is cut off: the hello is left to the bridge injected then.
  window.addEventListener(
    "message",
    (e) => {
      if (e.source !== window || !e.data || e.data.__endoHello !== 2 || !chrome.runtime?.id) return;
      e.stopImmediatePropagation();
      offerPort();
    },
    true,
  );
  offerPort();

  // ---- "Record playback" (MediaRecorder) ----

  function startMsr(index) {
    if (msr) return { ok: false, error: "A video in this frame is already being recorded." };
    const video = Number.isInteger(index) && index >= 0 ? document.querySelectorAll("video")[index] : null;
    if (!video) return { ok: false, error: "That video is no longer on the page." };
    if (!video.currentSrc && !video.srcObject) {
      return { ok: false, error: "The video has no source yet. Start playing it, then try again." };
    }
    // A muted (or silent) video is recorded without its sound, so it plays all but silently
    // instead while it is recorded; its own setting comes back afterwards.
    const playing = !video.paused;
    const sound = video.muted || video.volume === 0 ? { muted: video.muted, volume: video.volume } : null;
    if (sound) {
      video.muted = false;
      video.volume = QUIET;
    }
    // Chrome pauses a video that played muted, instead of unmuting it, when a script asks before
    // the viewer used the page: it plays on muted then, and is recorded without sound.
    const silent = Boolean(sound && playing && video.paused);
    if (silent) {
      restoreSound({ video, sound });
      video.play().catch(() => {});
    }
    let recorder;
    try {
      const type = RECORDER_TYPES.find((t) => MediaRecorder.isTypeSupported(t));
      recorder = new MediaRecorder(video.captureStream(), type ? { mimeType: type } : undefined);
      recorder.start(2000);
    } catch (err) {
      restoreSound({ video, sound });
      return {
        ok: false,
        error: `The browser will not record this video (it may be DRM-protected or from another site): ${err && err.message}`,
      };
    }
    const session = { recorder, closed: false, video, sound };
    const onEnded = () => {
      if (msr === session) stopMsr();
    };
    recorder.ondataavailable = (e) => {
      if (e.data.size) relay("msr", 0, 0, recorder.mimeType || "video/webm", e.data, session);
    };
    // The last dataavailable comes before stop, so REC_END is queued after the last chunk.
    recorder.onstop = () => {
      video.removeEventListener("ended", onEnded);
      restoreSound(session);
      endMsr(session, false);
    };
    video.addEventListener("ended", onEnded);
    msr = session;
    return { ok: true, silent };
  }

  /** Gives a recorded video back the sound setting it had, unless the viewer changed it meanwhile. */
  function restoreSound({ video, sound }) {
    if (!sound || video.muted || video.volume !== QUIET) return;
    video.volume = sound.volume;
    video.muted = sound.muted;
  }

  /** Sends REC_END for a MediaRecorder recording once: after its queued chunks, or `now`. */
  function endMsr(session, now) {
    if (msr === session) msr = null;
    const finish = () => {
      if (session.closed) return;
      session.closed = true;
      send({ cmd: "REC_END", ms: "msr" });
    };
    if (now) finish();
    else later(finish);
  }

  function stopMsr() {
    if (!msr) return;
    try {
      msr.recorder.stop();
    } catch {
      endMsr(msr, false);
    }
  }

  /** Stops the MediaRecorder recording at once, giving up what it has not delivered yet. */
  function abandonMsr() {
    const session = msr;
    if (!session) return null;
    msr = null;
    session.recorder.ondataavailable = session.recorder.onstop = null;
    try {
      session.recorder.stop();
    } catch {}
    restoreSound(session);
    return session;
  }

  // Work queued now would not run once the page is gone, so the recordings end at once: a reload,
  // or a frame moving on to the next video, finishes them. Chunks still being encoded are given
  // up, and a page kept for Back records no more when it comes back.
  window.addEventListener("pagehide", () => {
    const session = abandonMsr();
    if (session) endMsr(session, true);
    hidden = true;
    for (const ms of openMs) send({ cmd: "REC_END", ms });
    openMs.clear();
    toHook({ t: "rec-stop" });
    for (const job of downloads.values()) endDownload(job, "The page was closed or reloaded.");
  });

  // ---- "Via browser": a download made with this page's own requests ----

  /**
   * Header names fetch() refuses or that this download sets itself; the browser sends its own
   * Origin, Referer, User-Agent, cookies and sec-* headers anyway.
   */
  const UNSENDABLE =
    /^(?:accept-charset|accept-encoding|access-control-request-.*|connection|content-length|cookie2?|date|dnt|expect|host|keep-alive|origin|range|referer|set-cookie|te|trailer|transfer-encoding|upgrade|user-agent|via|proxy-.*|sec-.*)$/i;
  const TOKEN = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;

  /** Browser downloads running in this frame, by key. */
  const downloads = new Map();

  /** An error that trying again would not fix. */
  function fail(message, status) {
    return Object.assign(new Error(message), { final: true, status });
  }

  /** The background's captured request headers that fetch() can send. */
  function sendableHeaders(headers) {
    const out = {};
    if (headers && typeof headers === "object") {
      for (const [name, value] of Object.entries(headers)) {
        if (TOKEN.test(name) && !UNSENDABLE.test(name) && typeof value === "string" && !/[\0\r\n]/.test(value)) {
          out[name] = value;
        }
      }
    }
    return out;
  }

  function startDownload({ key, url, kind, audioUrl, headers }) {
    if (
      typeof key !== "string" ||
      !key ||
      key.length > 256 ||
      !isUrl(url, HTTP) ||
      (kind !== "hls" && kind !== "file") ||
      (audioUrl && !isUrl(audioUrl, HTTP))
    ) {
      return { ok: false, error: "bad request" };
    }
    if (downloads.has(key)) return { ok: false, error: "That download is already running." };
    const controller = new AbortController();
    const job = {
      key,
      ms: "dl:" + key,
      url,
      kind,
      audioUrl: audioUrl || "",
      headers: sendableHeaders(headers),
      controller,
      signal: controller.signal,
      /** Origins that refuse requests with credentials (they allow any origin, "*"). */
      plain: new Set(),
      /** AES-128 key URL → Promise<CryptoKey>. */
      keys: new Map(),
      done: 0,
      total: 0,
      shownAt: 0,
      ended: false,
    };
    downloads.set(key, job);
    (kind === "hls" ? downloadHls(job) : downloadFile(job)).then(
      () => endDownload(job, ""),
      (err) => endDownload(job, (err && err.message) || String(err)),
    );
    return { ok: true };
  }

  /**
   * Ends a download once: the last progress (with `error` when it failed), then REC_END, which
   * the background sends after every chunk. A cancelled download ends without an error and
   * keeps what it delivered.
   */
  function endDownload(job, error) {
    if (job.ended) return;
    job.ended = true;
    const cancelled = job.signal.aborted;
    job.controller.abort(); // requests still running after a failure
    downloads.delete(job.key);
    const last = { cmd: "BROWSER_DL_PROGRESS", key: job.key, done: job.done, total: job.total };
    send(error && !cancelled ? { ...last, error } : last);
    send({ cmd: "REC_END", ms: job.ms });
  }

  function progress(job) {
    const now = Date.now();
    if (now - job.shownAt < 500) return;
    job.shownAt = now;
    send({ cmd: "BROWSER_DL_PROGRESS", key: job.key, done: job.done, total: job.total });
  }

  /**
   * Hands `bytes` of a track to the background, waiting until each slice reached the app, so a
   * download holds no more than a few segments in memory however slow the app is.
   */
  async function deliver(job, track, part, mime, bytes) {
    for (let at = 0; at < bytes.length; at += SLICE) {
      const data = await toBase64(new Blob([bytes.subarray(at, at + SLICE)]));
      job.signal.throwIfAborted();
      const reply = await ask({ cmd: "REC_CHUNK", ms: job.ms, track, part, mime, data });
      if (reply && reply.ok === false) throw fail(reply.error || "The downloader did not take the data.");
    }
  }

  /** Runs `attempt` until it works, up to ATTEMPTS times, waiting longer after each failure. */
  async function retry(job, attempt) {
    for (let n = 1; ; n++) {
      try {
        return await attempt();
      } catch (err) {
        if (job.signal.aborted || (err && err.final) || n >= ATTEMPTS) throw err;
        await new Promise((resolve) => setTimeout(resolve, n * 1000));
      }
    }
  }

  /**
   * One request with the page's cookies and the captured headers. A server that allows every
   * origin ("*") refuses requests with credentials, and players mostly send none: those are
   * asked again without, and the origin remembered once that works. A network error looks the
   * same, so a request without credentials that fails leaves the next try to ask with them.
   */
  async function open(job, url, range) {
    const headers = new Headers(job.headers);
    if (range) headers.set("Range", `bytes=${range.start}-${range.end}`);
    const origin = new URL(url).origin;
    const request = (credentials) => fetch(url, { headers, credentials, signal: job.signal });
    let response;
    if (job.plain.has(origin)) {
      response = await request("same-origin");
    } else {
      try {
        response = await request("include");
      } catch (err) {
        if (job.signal.aborted) throw err;
        const plain = await request("same-origin").catch(() => null);
        if (job.signal.aborted) throw err;
        if (!plain) {
          throw new Error(
            `The page can't read ${url}: its server doesn't let other sites read it (CORS), or the network failed. Use Download instead.`,
          );
        }
        if (!plain.ok) throw new Error(`${url} answered HTTP ${plain.status} without cookies`);
        response = plain;
        job.plain.add(origin);
      }
    }
    if (response.ok) return response;
    const message = `${url} answered HTTP ${response.status}`;
    // Server trouble and rate limits may pass; anything else the server meant.
    if (response.status >= 500 || response.status === 408 || response.status === 429) throw new Error(message);
    throw fail(message, response.status);
  }

  /** The bytes `range` (inclusive) of `url` covers, or all of it. */
  function fetchBytes(job, url, range) {
    return retry(job, async () => {
      const response = await open(job, url, range);
      const bytes = new Uint8Array(await response.arrayBuffer());
      // A server that ignores the range sends the whole resource.
      return range && response.status === 200 ? bytes.slice(range.start, range.end + 1) : bytes;
    });
  }

  // HLS

  /** Attributes of an HLS tag: `NAME=value,NAME="quoted, value"`. */
  function attributes(text) {
    const out = {};
    for (const [, name, value] of text.matchAll(/([A-Z0-9-]+)=("[^"]*"|[^,]*)/g)) out[name] = value.replace(/^"(.*)"$/, "$1");
    return out;
  }

  function byteRange(text, next) {
    const m = /^(\d+)(?:@(\d+))?$/.exec(text.trim());
    if (!m) throw fail(`The playlist has a bad byte range: ${text}`);
    const start = m[2] === undefined ? next : Number(m[2]);
    return { start, end: start + Number(m[1]) - 1 };
  }

  function hexIv(text) {
    const hex = text.replace(/^0x/i, "");
    if (!/^[0-9a-f]{1,32}$/i.test(hex)) throw fail(`The playlist has a bad AES-128 IV: ${text}`);
    return Uint8Array.from(hex.padStart(32, "0").match(/../g), (byte) => parseInt(byte, 16));
  }

  /** The IV of a segment whose key names none: its media sequence number, big-endian. */
  function sequenceIv(sequence) {
    const iv = new Uint8Array(16);
    const view = new DataView(iv.buffer);
    view.setUint32(8, Math.floor(sequence / 2 ** 32));
    view.setUint32(12, sequence >>> 0);
    return iv;
  }

  function keyOf(a, resolve) {
    if (a.METHOD === "NONE") return null;
    if (a.METHOD !== "AES-128" || (a.KEYFORMAT && a.KEYFORMAT !== "identity")) {
      throw fail("This stream is DRM-protected, so it cannot be downloaded.");
    }
    if (!a.URI) throw fail("The playlist names no AES-128 key.");
    return { url: resolve(a.URI), iv: a.IV ? hexIv(a.IV) : null };
  }

  /**
   * Reads a playlist: a master gives `{variants: [{url, bandwidth, audioUrl}]}`, best first; a
   * media playlist `{segments: [{url, sequence, key, map, range}]}`, where `map` (the init
   * section, fMP4) is `{url, range, key, sequence}`, `key` `{url, iv}` and ranges are inclusive.
   */
  function parsePlaylist(text, base) {
    const lines = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    if (!lines.length || !lines[0].replace(/^﻿/, "").startsWith("#EXTM3U")) {
      throw fail("The address did not answer with an HLS playlist.");
    }
    const resolve = (uri) => new URL(uri, base).href;
    const variants = [];
    const audio = new Map(); // AUDIO group → the URL of its default rendition
    const segments = [];
    let sequence = 0;
    let key = null;
    let map = null;
    let range = null;
    let next = 0;
    let stream = null;
    let complete = false;
    for (const line of lines) {
      if (!line.startsWith("#")) {
        if (stream) variants.push({ url: resolve(line), bandwidth: Number(stream.BANDWIDTH) || 0, audio: stream.AUDIO });
        else segments.push({ url: resolve(line), sequence: sequence + segments.length, key, map, range });
        stream = range = null;
        continue;
      }
      const colon = line.indexOf(":");
      const tag = colon < 0 ? line : line.slice(0, colon);
      const value = colon < 0 ? "" : line.slice(colon + 1);
      if (tag === "#EXT-X-STREAM-INF") stream = attributes(value);
      else if (tag === "#EXT-X-MEDIA") {
        const a = attributes(value);
        const group = a["GROUP-ID"];
        if (a.TYPE === "AUDIO" && a.URI && (!audio.has(group) || a.DEFAULT === "YES")) audio.set(group, resolve(a.URI));
      } else if (tag === "#EXT-X-MEDIA-SEQUENCE") sequence = Number(value) || 0;
      else if (tag === "#EXT-X-ENDLIST") complete = true;
      else if (tag === "#EXT-X-PLAYLIST-TYPE") complete ||= value === "VOD";
      else if (tag === "#EXT-X-KEY") key = keyOf(attributes(value), resolve);
      else if (tag === "#EXT-X-MAP") {
        const a = attributes(value);
        if (!a.URI) throw fail("The playlist has an EXT-X-MAP without a URI.");
        const mapRange = a.BYTERANGE ? byteRange(a.BYTERANGE, 0) : null;
        map = { url: resolve(a.URI), range: mapRange, key, sequence: sequence + segments.length };
      } else if (tag === "#EXT-X-BYTERANGE") {
        range = byteRange(value, next);
        next = range.end + 1;
      }
    }
    if (variants.length) {
      variants.sort((a, b) => b.bandwidth - a.bandwidth);
      for (const variant of variants) variant.audioUrl = audio.get(variant.audio) || "";
      return { variants };
    }
    if (!complete) throw fail("This is a live stream: record it with “Capture from start” instead.");
    if (!segments.length) throw fail("The playlist lists no segments.");
    return { segments };
  }

  async function fetchPlaylist(job, url) {
    const { text, base } = await retry(job, async () => {
      const response = await open(job, url);
      return { text: await response.text(), base: response.url || url };
    });
    return parsePlaylist(text, base);
  }

  async function decrypt(job, key, sequence, data) {
    if (!job.keys.has(key.url)) {
      if (!crypto.subtle) throw fail("AES-128 streams can only be downloaded through the browser on https pages.");
      const loaded = fetchBytes(job, key.url).then((raw) => {
        if (raw.length !== 16) throw fail("The AES-128 key is not 16 bytes long.");
        return crypto.subtle.importKey("raw", raw, "AES-CBC", false, ["decrypt"]);
      });
      loaded.catch(() => {});
      job.keys.set(key.url, loaded);
    }
    const cryptoKey = await job.keys.get(key.url);
    try {
      const iv = key.iv || sequenceIv(sequence);
      return new Uint8Array(await crypto.subtle.decrypt({ name: "AES-CBC", iv }, cryptoKey, data));
    } catch {
      throw fail("AES-128 decryption failed (wrong key or corrupt data).");
    }
  }

  /**
   * Cuts what some hosts put around an MPEG-TS segment so it passes for an image, as the app
   * does: the segment starts where five packets in a row begin (0x47 every 188 bytes; a GIF
   * starts with 0x47 too), and ends with the last whole packet that starts with 0x47. A segment
   * with no such run is kept whole.
   */
  function stripDisguise(data) {
    const run = (i) =>
      data[i] === 0x47 && data[i + 188] === 0x47 && data[i + 376] === 0x47 && data[i + 564] === 0x47 && data[i + 752] === 0x47;
    let start = 0;
    while (start + 4 * 188 < data.length && !run(start)) start++;
    if (start + 4 * 188 >= data.length) return data;
    let end = start + Math.floor((data.length - start) / 188) * 188;
    while (data[end - 188] !== 0x47) end -= 188;
    return data.subarray(start, end);
  }

  /** The media type of a track's first bytes, else `fallback`. */
  function sniffType(b, fallback) {
    const box = String.fromCharCode(b[4], b[5], b[6], b[7]);
    if (b[0] === 0x47) return "video/mp2t";
    if (box === "ftyp" || box === "styp" || box === "moof" || box === "moov") return "video/mp4";
    if (b[0] === 0x1a && b[1] === 0x45 && b[2] === 0xdf && b[3] === 0xa3) return "video/webm";
    if ((b[0] === 0x49 && b[1] === 0x44 && b[2] === 0x33) || (b[0] === 0xff && (b[1] & 0xf6) === 0xf0)) return "audio/aac";
    return fallback;
  }

  async function fetchSegment(job, segment) {
    let data = await fetchBytes(job, segment.url, segment.range);
    if (segment.key) data = await decrypt(job, segment.key, segment.sequence, data);
    // Only MPEG-TS is disguised so; fMP4 segments (with an init section) are left as they are.
    return segment.map ? data : stripDisguise(data);
  }

  /**
   * Downloads a media playlist's segments in order, IN_FLIGHT at a time, as `track` (0 video or
   * muxed, 1 audio). An fMP4 init section goes first, and a new one later starts a new part.
   */
  async function downloadTrack(job, track, segments) {
    const load = (segment) => {
      const loading = fetchSegment(job, segment);
      loading.catch(() => {}); // awaited in turn below
      return loading;
    };
    const loading = segments.slice(0, IN_FLIGHT).map(load);
    let part = 0;
    let mime = "";
    let map = null;
    for (let i = 0; i < segments.length; i++) {
      const segment = segments[i];
      const data = await loading.shift();
      if (i + IN_FLIGHT < segments.length) loading.push(load(segments[i + IN_FLIGHT]));
      const changed = segment.map && !(map && map.url === segment.map.url && map.range?.start === segment.map.range?.start);
      if (changed) {
        let init = await fetchBytes(job, segment.map.url, segment.map.range);
        if (segment.map.key) init = await decrypt(job, segment.map.key, segment.map.sequence, init);
        if (map) part = Math.min(part + 1, 255);
        map = segment.map;
        mime ||= track ? "audio/mp4" : "video/mp4";
        await deliver(job, track, part, mime, init);
      }
      mime ||= sniffType(data, "video/mp2t");
      await deliver(job, track, part, mime, data);
      job.done++;
      progress(job);
    }
  }

  async function downloadHls(job) {
    let media = await fetchPlaylist(job, job.url);
    let audioUrl = job.audioUrl;
    if (media.variants) {
      const best = media.variants[0];
      audioUrl ||= best.audioUrl;
      media = await fetchPlaylist(job, best.url);
      if (media.variants) throw fail("The master playlist names another master playlist.");
    }
    const tracks = [media.segments];
    if (audioUrl) {
      const audio = await fetchPlaylist(job, audioUrl);
      if (audio.variants) throw fail("The audio rendition is a master playlist.");
      tracks.push(audio.segments);
    }
    job.total = tracks.reduce((sum, segments) => sum + segments.length, 0);
    progress(job);
    for (const [track, segments] of tracks.entries()) await downloadTrack(job, track, segments);
  }

  // Files

  function mediaType(response, bytes) {
    const type = (response.headers.get("Content-Type") || "").split(";")[0].trim().toLowerCase();
    return /^(?:video|audio)\/[\w.+-]+$/.test(type) ? type : sniffType(bytes, "video/mp4");
  }

  /** A file in RANGE-sized requests; a server that ignores ranges sends it whole, passed on as it arrives. */
  async function downloadFile(job) {
    let mime = "";
    for (let at = 0; ; ) {
      const range = { start: at, end: at + RANGE - 1 };
      let got;
      try {
        got = await retry(job, async () => {
          const response = await open(job, job.url, range);
          return { response, bytes: response.status === 206 ? new Uint8Array(await response.arrayBuffer()) : null };
        });
      } catch (err) {
        // Asked past the end of a file whose size was not known.
        if (at && !job.total && err.status === 416) return;
        throw err;
      }
      const { response, bytes } = got;
      if (!bytes) {
        if (!at) return streamFile(job, response);
        // Some servers answer a range past the end with the whole file (cut off as the download ends).
        if (!job.total) return;
        throw fail("The server stopped answering byte ranges.");
      }
      // Content-Range is hidden from a cross-origin page unless the server exposes it.
      const sent = /^bytes (\d+)-\d+\/(\d+|\*)$/i.exec(response.headers.get("Content-Range") || "");
      if (sent && Number(sent[1]) !== at) throw fail("The server sent another part of the file than the one asked for.");
      if (sent) job.total = Number(sent[2]) || 0;
      mime ||= mediaType(response, bytes);
      await deliver(job, 0, 0, mime, bytes);
      at += bytes.length;
      job.done = at;
      progress(job);
      // Without a size (Content-Range is hidden from a cross-origin page unless exposed), a short
      // answer may only be the server's own limit, so the next range is asked for anyway.
      if (!bytes.length || (job.total && at >= job.total)) return;
    }
  }

  async function streamFile(job, response) {
    job.total = Number(response.headers.get("Content-Length")) || 0;
    const reader = response.body.getReader();
    let mime = "";
    let held = [];
    let size = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (value) {
        held.push(value);
        size += value.length;
      }
      if (size >= RANGE || (done && size)) {
        const bytes = new Uint8Array(await new Blob(held).arrayBuffer());
        held = [];
        size = 0;
        mime ||= mediaType(response, bytes);
        await deliver(job, 0, 0, mime, bytes);
        job.done += bytes.length;
        progress(job);
      }
      if (done) return;
    }
  }

  // ---- Commands from the background ----

  function setSpeed(rate) {
    for (const video of document.querySelectorAll("video")) {
      try {
        const source = video.srcObject;
        if (video.currentSrc.startsWith("blob:") || (source && !(source instanceof MediaStream))) {
          video.playbackRate = rate;
        }
      } catch {}
    }
  }

  chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
    if (!msg || typeof msg !== "object") return;
    switch (msg.cmd) {
      case "REC_ARM":
        try {
          sessionStorage.setItem(ARM_KEY, String(Date.now()));
        } catch {}
        reply({ ok: true });
        break;
      case "REC_STOP":
        stopMsr();
        toHook({ t: "rec-stop" });
        reply({ ok: true });
        break;
      case "REC_SPEED":
        if (SPEEDS.includes(msg.rate)) setSpeed(msg.rate);
        reply({ ok: SPEEDS.includes(msg.rate) });
        break;
      case "MSR_START":
        reply(startMsr(msg.index));
        break;
      case "CHECK_VIDEO_SRC":
        // Whether a ranged response with no usable type is what a video here plays.
        reply(
          typeof msg.url === "string" &&
            [...document.querySelectorAll("video, source")].some((el) => el.src === msg.url || el.currentSrc === msg.url),
        );
        break;
      case "BROWSER_DL_START":
        reply(startDownload(msg));
        break;
      case "BROWSER_DL_CANCEL": {
        const job = downloads.get(msg.key);
        if (job) job.controller.abort();
        reply({ ok: Boolean(job) });
        break;
      }
    }
  });
})();
