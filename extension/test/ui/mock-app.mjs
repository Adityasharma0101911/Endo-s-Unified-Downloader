// A stand-in for the desktop app's local API, for the popup harness: /ping, /add, /record/*. It records every
// request so a test can assert what the extension sent. Standalone: `node mock-app.mjs [running|outdated|absent]`.
import http from "node:http";
import { pathToFileURL } from "node:url";

export const PORTS = [49152, 49153, 49154, 49155];

/**
 * Starts on the first free port of PORTS; `app.mode` can be changed at any time:
 *  running  – answers like the current app ({app, version} on /ping);
 *  outdated – the app from before /ping: 404 there, and the old preflight on /add (see background.js isOldApp);
 *  absent   – drops every connection, as if nothing listened.
 * Refuses to start when something already answers /ping on PORTS (the real app would get the test downloads).
 */
export async function startMockApp({ mode = "running", version = "9.9.9" } = {}) {
  for (const port of PORTS) {
    const ping = await fetch(`http://127.0.0.1:${port}/ping`, { signal: AbortSignal.timeout(500) }).catch(() => null);
    if (ping?.ok) throw new Error(`Something answers /ping on 127.0.0.1:${port} (the real app?). Quit it first.`);
  }
  const requests = [];
  let nextRec = 1;
  const app = {
    mode,
    version,
    requests,
    /** Requests after index `since` (take requests.length before acting), optionally only paths matching `path`. */
    since: (since, path) => requests.slice(since).filter((r) => !path || path.test(r.path)),
  };
  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const url = new URL(req.url, "http://x");
      const raw = Buffer.concat(chunks);
      let body = null;
      if ((req.headers["content-type"] || "").includes("json")) {
        try {
          body = JSON.parse(raw.toString("utf8"));
        } catch {
          body = raw.toString("utf8");
        }
      }
      requests.push({ time: Date.now(), mode: app.mode, method: req.method, path: url.pathname, query: Object.fromEntries(url.searchParams), body, bytes: raw.length });
      if (app.mode === "absent") return req.socket.destroy();
      const send = (status, json) => {
        res.writeHead(status, { "Content-Type": "application/json" });
        res.end(JSON.stringify(json));
      };
      if (app.mode === "outdated") {
        if (req.method !== "OPTIONS" || url.pathname !== "/add") return send(404, { error: "not found" });
        res.writeHead(204, { "Access-Control-Allow-Methods": "POST, OPTIONS" });
        return res.end();
      }
      // No `extension` field: background.js would reload itself for a newer one.
      if (url.pathname === "/ping") return send(200, { app: "endos-unified-downloader", version: app.version });
      if (req.method !== "POST") return send(405, { error: "POST only" });
      if (url.pathname === "/add") return send(200, { ok: true });
      if (url.pathname === "/record/start") return send(200, { id: String(nextRec++) });
      if (/^\/record\/[^/]+\/(chunk|finish|abort)$/.test(url.pathname)) return send(200, { ok: true });
      send(404, { error: "not found" });
    });
  });
  for (const port of PORTS) {
    const listening = await new Promise((resolve) => {
      server.once("error", () => resolve(false));
      server.listen(port, "127.0.0.1", () => resolve(true));
    });
    if (listening) return Object.assign(app, { port, close: () => new Promise((resolve) => server.close(resolve)) });
  }
  throw new Error(`No free port in ${PORTS.join(", ")}.`);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const app = await startMockApp({ mode: process.argv[2] || "running" });
  console.log(`mock app (${app.mode}) on 127.0.0.1:${app.port}; Ctrl+C stops it`);
  let shown = 0;
  setInterval(() => {
    for (; shown < app.requests.length; shown++) console.log(JSON.stringify(app.requests[shown]));
  }, 300);
}
