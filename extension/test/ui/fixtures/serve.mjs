// Serves the fixture pages on 127.0.0.1:8765. Any request whose Host is not 127.0.0.1/localhost (a media-site
// name mapped here by the browser's --host-resolver-rules) gets media-site.html. /media/clip.mp4 is 600 KB of
// zeros typed video/mp4: above the extension's default 500 KB minimum, and nothing needs to play it.
import http from "node:http";
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html; charset=utf-8", ".m3u8": "application/vnd.apple.mpegurl", ".mpd": "application/dash+xml" };
export const FIXTURE_PORT = 8765;

export function startFixtures(port = FIXTURE_PORT) {
  const server = http.createServer(async (req, res) => {
    const { pathname } = new URL(req.url, "http://x");
    const host = String(req.headers.host || "").replace(/:\d+$/, "");
    if (pathname === "/media/clip.mp4") {
      res.writeHead(200, { "Content-Type": "video/mp4", "Content-Length": 600 * 1024 });
      return res.end(Buffer.alloc(600 * 1024));
    }
    // "Via browser" fixtures. The page's own fetch has no Range header; the extension's download asks for one.
    // gone.mp4: the download gets a 404 (a failed browser download). slow.mp4: 20 MB, the first 9 MB at once (the
    // bridge reports progress every 8 MB), then 640 KB/s, so the download stays in progress for ~17 s.
    if (pathname === "/media/gone.mp4") {
      if (req.headers.range) return res.writeHead(404).end("gone");
      res.writeHead(200, { "Content-Type": "video/mp4", "Content-Length": 600 * 1024 });
      return res.end(Buffer.alloc(600 * 1024));
    }
    if (pathname === "/media/slow.mp4") {
      const size = 20 * 1024 * 1024;
      let sent = 9 * 1024 * 1024;
      res.writeHead(200, { "Content-Type": "video/mp4", "Content-Length": size });
      res.write(Buffer.alloc(sent));
      const timer = setInterval(() => {
        const n = Math.min(64 * 1024, size - sent);
        res.write(Buffer.alloc(n));
        if ((sent += n) >= size) clearInterval(timer), res.end();
      }, 100);
      return res.on("close", () => clearInterval(timer));
    }
    const file = host !== "127.0.0.1" && host !== "localhost" ? "media-site.html" : path.normalize(pathname).replace(/^[\/]+/, "");
    try {
      if (file.startsWith("..") || file.endsWith(".mjs")) throw new Error("outside");
      const body = await fs.readFile(path.join(DIR, file));
      res.writeHead(200, { "Content-Type": TYPES[path.extname(file)] || "application/octet-stream", "Cache-Control": "no-store" });
      res.end(body);
    } catch {
      res.writeHead(404, { "Content-Type": "text/plain" });
      res.end("not found");
    }
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => resolve({ port, close: () => new Promise((done) => server.close(done)) }));
  });
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  await startFixtures();
  console.log(`fixtures on http://127.0.0.1:${FIXTURE_PORT}/ (stream.html?src=/hls/master.m3u8, links.html, video.html…)`);
}
