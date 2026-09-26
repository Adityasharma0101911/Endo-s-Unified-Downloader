#!/usr/bin/env python3
"""Local HTTP/1.1 file server with a per-connection bandwidth cap, for benchmarking downloaders.

Serves deterministic generated files (nothing is written to disk):
  /large.bin           --large-mib MiB
  /small-NNN.bin       --small-count files of --small-kib KiB
  /medium-NN.bin       --medium-count files of --medium-mib MiB

Keep-alive, HEAD, single Range requests (206 + Content-Range, 416 when unsatisfiable),
If-Range, ETag and Last-Modified. Every response waits --latency-ms before its headers and
every body is paced to --rate-mib MiB/s on its connection.

Fault modes:
  --head-without-accept-ranges  HEAD responses omit Accept-Ranges (ranged GETs still work)
  --stall                       the first ranged GET starting in the second half of a file sends
                                half its body, then holds the connection open forever

Prints "READY <port>" on stdout once it accepts connections.
"""

import argparse
import hashlib
import random
import threading
import time
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Content repeats with a period that no power-of-two chunk size divides, so a chunk written at
# the wrong offset changes the file's hash.
PERIOD = 1_000_003
BLOCK = random.Random(20240601).randbytes(PERIOD)
PIECE = 16 * 1024
ETAG_DATE = "Mon, 01 Jan 2024 00:00:00 GMT"


def build_files(large_mib, small_count, small_kib, medium_count, medium_mib):
    """Name -> size in bytes of every file the server serves."""
    files = {"large.bin": int(large_mib * 1024 * 1024)}
    files.update({f"small-{i:03}.bin": small_kib * 1024 for i in range(small_count)})
    files.update({f"medium-{i:02}.bin": int(medium_mib * 1024 * 1024) for i in range(medium_count)})
    return files


def content(name, start, length):
    """Bytes [start, start + length) of the generated file `name`."""
    pos = (start + zlib.crc32(name.encode())) % PERIOD
    out = bytearray()
    while len(out) < length:
        take = min(length - len(out), PERIOD - pos)
        out += BLOCK[pos:pos + take]
        pos = 0
    return bytes(out)


def sha256_of(name, size):
    digest = hashlib.sha256()
    for start in range(0, size, 1 << 20):
        digest.update(content(name, start, min(1 << 20, size - start)))
    return digest.hexdigest()


def parse_range(header, size):
    """(start, end) inclusive for a single "bytes=" range, None to serve the whole file, or
    "unsatisfiable"."""
    if not header or not header.startswith("bytes=") or "," in header:
        return None
    first, _, last = header[len("bytes="):].strip().partition("-")
    try:
        if first == "":
            suffix = int(last)
            if suffix <= 0:
                return "unsatisfiable"
            return max(0, size - suffix), size - 1
        start = int(first)
        end = int(last) if last else size - 1
    except ValueError:
        return None
    if start >= size or end < start:
        return "unsatisfiable"
    return start, min(end, size - 1)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = 120  # closes idle keep-alive connections of clients that went away

    def log_message(self, *args):
        pass

    def do_HEAD(self):
        self.respond(head=True)

    def do_GET(self):
        self.respond(head=False)

    def respond(self, head):
        cfg = self.server.cfg
        time.sleep(cfg.latency_ms / 1000)
        name = self.path.split("?", 1)[0].lstrip("/")
        size = cfg.files.get(name)
        if size is None:
            self.send_error(404)
            return
        etag = f'"{zlib.crc32(name.encode()):08x}-{size:x}"'
        if_range = self.headers.get("If-Range")
        rng = None
        if not head and (if_range is None or if_range in (etag, ETAG_DATE)):
            rng = parse_range(self.headers.get("Range"), size)
        if rng == "unsatisfiable":
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        start, end = rng if rng else (0, size - 1)
        self.send_response(206 if rng else 200)
        if rng:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        if not (head and cfg.head_without_accept_ranges):
            self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(end - start + 1))
        self.send_header("ETag", etag)
        self.send_header("Last-Modified", ETAG_DATE)
        self.end_headers()
        if head:
            return
        stall_after = None
        if rng and cfg.stall and start >= size // 2 and self.server.claim_stall():
            stall_after = (end - start + 1) // 2
        self.send_body(name, start, end + 1, stall_after)

    def send_body(self, name, start, stop, stall_after):
        rate = self.server.cfg.rate_mib * 1024 * 1024
        began = time.monotonic()
        sent = 0
        for offset in range(start, stop, PIECE):
            if stall_after is not None and sent >= stall_after:
                self.wfile.flush()
                threading.Event().wait()  # hold the connection open without sending
            piece = content(name, offset, min(PIECE, stop - offset))
            self.wfile.write(piece)
            sent += len(piece)
            if rate:
                ahead = sent / rate - (time.monotonic() - began)
                if ahead > 0:
                    time.sleep(ahead)


class Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 128

    def __init__(self, address, cfg):
        super().__init__(address, Handler)
        self.cfg = cfg
        self._stalled = False
        self._lock = threading.Lock()

    def claim_stall(self):
        with self._lock:
            first, self._stalled = not self._stalled, True
            return first

    def handle_error(self, request, client_address):
        pass  # clients drop connections on purpose (cancelled chunks, stall recovery)


def add_file_args(parser):
    parser.add_argument("--large-mib", type=float, default=256)
    parser.add_argument("--small-count", type=int, default=40)
    parser.add_argument("--small-kib", type=int, default=256)
    parser.add_argument("--medium-count", type=int, default=10)
    parser.add_argument("--medium-mib", type=float, default=1)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0, help="0 picks a free port")
    parser.add_argument("--rate-mib", type=float, default=2.0, help="per-connection cap in MiB/s (0 = unlimited)")
    parser.add_argument("--latency-ms", type=float, default=40.0, help="delay before every response")
    parser.add_argument("--head-without-accept-ranges", action="store_true")
    parser.add_argument("--stall", action="store_true")
    add_file_args(parser)
    cfg = parser.parse_args()
    cfg.files = build_files(cfg.large_mib, cfg.small_count, cfg.small_kib, cfg.medium_count, cfg.medium_mib)
    server = Server((cfg.host, cfg.port), cfg)
    print(f"READY {server.server_address[1]}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
