// Endo's Unified Downloader: the content script in the extension's isolated world, in every
// frame. It passes what hook.js (the page's world) finds on to the background service worker,
// arms "Capture from start" for the reload, changes the playback speed of a buffer capture, and
// records one video with MediaRecorder ("Record playback").
(() => {
  "use strict";

  /** sessionStorage key hook.js reads at the next document start. */
  const ARM_KEY = "__endo_rec_armed";
  const MAX_PLAYLIST = 2 * 1024 * 1024;
  const MAX_URL = 32 * 1024;
  /** Raw bytes per REC_CHUNK: base64 adds a third, and Chrome refuses messages over 64 MiB. */
  const SLICE = 8 * 1024 * 1024;
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

  function send(msg) {
    if (orphaned) return;
    try {
      // The callback reads lastError so a sleeping or missing listener is not reported as an error.
      chrome.runtime.sendMessage(msg, () => void chrome.runtime.lastError);
    } catch {
      // "Extension context invalidated": stop recording for a background that is gone.
      orphaned = true;
      abandonMsr();
      toHook({ t: "rec-stop" });
    }
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
  function relay(ms, track, mime, blob, session) {
    for (let at = 0; at < blob.size; at += SLICE) {
      const part = blob.slice(at, at + SLICE);
      later(() =>
        toBase64(part).then((data) => {
          if (session ? !session.closed : openMs.has(ms)) send({ cmd: "REC_CHUNK", ms, track, mime, data });
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

  /** Passes on what hook.js found. It comes from the page's world, so each message is checked. */
  function fromHook(e) {
    const m = e.data;
    if (orphaned || !m || typeof m !== "object") return;
    if (m.t === "m3u8") {
      if (
        typeof m.url === "string" &&
        m.url.length <= MAX_URL &&
        /^https?:\/\//i.test(m.url) &&
        typeof m.text === "string" &&
        m.text.length <= MAX_PLAYLIST
      ) {
        send({ cmd: "M3U8_SEEN", url: m.url, text: m.text });
      }
    } else if (m.t === "rec-active") {
      send({ cmd: "REC_ACTIVE" });
    } else if (m.t === "rec-chunk") {
      if (
        !hidden &&
        isMs(m.ms) &&
        Number.isInteger(m.track) &&
        m.track >= 0 &&
        m.track <= 15 &&
        typeof m.mime === "string" &&
        m.mime.length <= 255 &&
        m.buf instanceof ArrayBuffer
      ) {
        openMs.add(m.ms);
        relay(m.ms, m.track, m.mime, new Blob([m.buf]), null);
      }
    } else if (m.t === "rec-end") {
      if (isMs(m.ms)) later(() => openMs.delete(m.ms) && send({ cmd: "REC_END", ms: m.ms }));
    }
  }

  function offerPort() {
    try {
      const channel = new MessageChannel();
      channel.port1.onmessage = fromHook;
      hookPorts.push(channel.port1);
      window.postMessage({ __endoBridge: 1 }, "*", [channel.port2]);
    } catch {}
  }

  // Whichever of hook.js and this script starts first, the hook gets a port: one is offered now
  // and another whenever the hook says hello. The hello is kept from the page's listeners.
  window.addEventListener(
    "message",
    (e) => {
      if (e.source !== window || !e.data || e.data.__endoHello !== 1) return;
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
    if (video.muted) {
      return {
        ok: false,
        error: "The video is muted, so the recording would have no sound. Unmute it, then try again.",
      };
    }
    let recorder;
    try {
      const type = RECORDER_TYPES.find((t) => MediaRecorder.isTypeSupported(t));
      recorder = new MediaRecorder(video.captureStream(), type ? { mimeType: type } : undefined);
      recorder.start(2000);
    } catch (err) {
      return {
        ok: false,
        error: `The browser will not record this video (it may be DRM-protected or from another site): ${err && err.message}`,
      };
    }
    const session = { recorder, closed: false };
    const onEnded = () => {
      if (msr === session) stopMsr();
    };
    recorder.ondataavailable = (e) => {
      if (e.data.size) relay("msr", 0, recorder.mimeType || "video/webm", e.data, session);
    };
    // The last dataavailable comes before stop, so REC_END is queued after the last chunk.
    recorder.onstop = () => {
      video.removeEventListener("ended", onEnded);
      endMsr(session, false);
    };
    video.addEventListener("ended", onEnded);
    msr = session;
    return { ok: true };
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
  });

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
    }
  });
})();
