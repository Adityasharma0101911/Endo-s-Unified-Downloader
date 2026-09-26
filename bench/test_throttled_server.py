#!/usr/bin/env python3
"""Self-check for throttled_server.py: python bench/test_throttled_server.py"""

import http.client
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from run_bench import history_paths, seed_history, summarize
from throttled_server import PERIOD, content, parse_range

assert parse_range(None, 100) is None
assert parse_range("bytes=0-0", 100) == (0, 0)
assert parse_range("bytes=10-", 100) == (10, 99)
assert parse_range("bytes=90-500", 100) == (90, 99)
assert parse_range("bytes=-30", 100) == (70, 99)
assert parse_range("bytes=100-", 100) == "unsatisfiable"
assert parse_range("bytes=0-1,5-6", 100) is None
whole = content("large.bin", 0, 3 * PERIOD)
assert content("large.bin", PERIOD - 5, 20) == whole[PERIOD - 5:PERIOD + 15]
assert content("large.bin", 0, 64) != content("small-000.bin", 0, 64)

assert summarize([("ok", 3.0), ("hung", None), ("ok", 1.0), ("exit 1", 0.5), ("ok", 2.0)]) == (2.0, 1.0, 3.0)
assert summarize([("hung", None)]) == (None, None, None)
with tempfile.TemporaryDirectory() as home:
    seed_history(Path(home), 1000)
    for path in history_paths(Path(home)):
        entries = json.loads(path.read_text())
        assert len(entries) == 1000 and len({e["id"] for e in entries}) == 1000
        # v1.0 requires every field.
        assert all(set(e) == {"id", "file_name", "file_path", "file_size", "downloaded_bytes", "urls", "status",
                              "blake3_hash", "sha256_hash", "started_at", "completed_at"} for e in entries)


def start(*flags):
    proc = subprocess.Popen([sys.executable, str(Path(__file__).with_name("throttled_server.py")),
                             "--large-mib", "1", "--small-count", "0", "--medium-count", "0",
                             "--latency-ms", "0", "--rate-mib", "0", *flags],
                            stdout=subprocess.PIPE, text=True)
    return proc, int(proc.stdout.readline().split()[1])


def timed_get(conn):
    began = time.perf_counter()
    conn.request("GET", "/large.bin", headers={"Range": "bytes=0-0"})
    conn.getresponse().read()
    return time.perf_counter() - began


# --connect-ms delays a connection's first request only, not the ones kept alive after it.
server, port = start("--connect-ms", "1000")
try:
    conn = http.client.HTTPConnection("127.0.0.1", port)
    assert timed_get(conn) >= 1.0
    assert timed_get(conn) < 0.5
finally:
    server.kill()
    server.wait()

server, port = start("--head-without-accept-ranges")
try:
    conn = http.client.HTTPConnection("127.0.0.1", port)  # one keep-alive connection for all
    conn.request("HEAD", "/large.bin")
    head = conn.getresponse()
    head.read()
    assert head.status == 200 and head.getheader("Content-Length") == str(1 << 20)
    assert head.getheader("Accept-Ranges") is None
    etag = head.getheader("ETag")
    conn.request("GET", "/large.bin", headers={"Range": "bytes=1000-1999", "If-Range": etag})
    part = conn.getresponse()
    assert part.status == 206 and part.getheader("Content-Range") == f"bytes 1000-1999/{1 << 20}"
    assert part.read() == content("large.bin", 1000, 1000)
    conn.request("GET", "/large.bin", headers={"Range": "bytes=0-9", "If-Range": '"other"'})
    full = conn.getresponse()
    assert full.status == 200 and len(full.read()) == 1 << 20
    conn.request("GET", "/missing.bin")
    missing = conn.getresponse()
    missing.read()
    assert missing.status == 404
finally:
    server.kill()
    server.wait()
print("ok")
