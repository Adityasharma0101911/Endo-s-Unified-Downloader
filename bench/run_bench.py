#!/usr/bin/env python3
"""Compare two Endo's Unified Downloader CLI binaries against bench/throttled_server.py.

Every run gets a fresh server, a fresh output directory and a throwaway history/home, so the
user's real history is never touched. Output files are verified by SHA-256. The binaries take
turns run by run, so a machine that slows down or speeds up mid-benchmark affects both alike.
Prints a markdown table of the median, fastest and slowest wall time over passing runs.
"""

import argparse
import hashlib
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import throttled_server  # noqa: E402

# (id, title, server flags for the parsed args, files: "large" or "small", entries to seed the history with)
SCENARIOS = [
    ("large", "Large file, -s 16", lambda a: [], "large", 0),
    ("small", "Small-file batch via -i", lambda a: [], "small", 0),
    ("handshake", "Small-file batch via -i, slow connection setup",
     lambda a: ["--connect-ms", str(a.connect_ms)], "small", 0),
    ("history", "Small-file batch via -i, 1000-entry history", lambda a: [], "small", 1000),
    ("no-accept-ranges", "Large file, HEAD without Accept-Ranges", lambda a: ["--head-without-accept-ranges"], "large", 0),
    ("stall", "Large file, one connection stalls forever", lambda a: ["--stall"], "large", 0),
]


def variants(group, old, new):
    """(label, argv prefix) of every binary configuration run for a group of files."""
    if group == "small":
        return [("v1.0 (sequential)", [old]), ("new -j 1", [new, "-j", "1"]), ("new -j 4", [new, "-j", "4"])]
    return [("v1.0", [old]), ("new", [new])]


def start_server(args, flags):
    cmd = [sys.executable, str(HERE / "throttled_server.py"),
           "--rate-mib", str(args.rate_mib), "--latency-ms", str(args.latency_ms),
           "--large-mib", str(args.large_mib), "--small-count", str(args.small_count),
           "--small-kib", str(args.small_kib), "--medium-count", str(args.medium_count),
           "--medium-mib", str(args.medium_mib), *flags]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True)
    line = proc.stdout.readline().split()
    if len(line) != 2 or line[0] != "READY":
        proc.kill()
        sys.exit(f"server failed to start: {cmd}")
    return proc, int(line[1])


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(1 << 20):
            digest.update(block)
    return digest.hexdigest()


def history_entries(count, folder):
    """`count` completed downloads of other files, in the history format of v1.0 and the new build."""
    now = int(time.time())
    return [{
        "id": f"seed-{i}",
        "file_name": f"seed-{i:04}.bin",
        "file_path": str(folder / f"seed-{i:04}.bin"),
        "file_size": 1 << 20,
        "downloaded_bytes": 1 << 20,
        "urls": [f"http://127.0.0.1:9/seed-{i:04}.bin"],
        "status": "Completed",
        "blake3_hash": "0" * 64,
        "sha256_hash": None,
        "started_at": now - 60 - i,
        "completed_at": now - i,
    } for i in range(count)]


def history_paths(home):
    """Where each binary reads its history under the throwaway home: ENDO_HISTORY_PATH (new),
    and v1.0's default for LOCALAPPDATA (Windows) or XDG_DATA_HOME (elsewhere)."""
    return [home / "history.json", home / "EndosUnifiedDownloader" / "history.json",
            home / "endos-downloader" / "history.json"]


def seed_history(home, count):
    text = json.dumps(history_entries(count, home / "seeded"))
    for path in history_paths(home):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def run_once(args, argv, server_flags, files, expected, history):
    """Returns (status, seconds). Status is "ok", "hung", "exit N" or "bad: <file>"."""
    server, port = start_server(args, server_flags)
    work = Path(tempfile.mkdtemp(prefix="endo-bench-"))
    try:
        out, home = work / "out", work / "home"
        out.mkdir()
        home.mkdir()
        if history:
            seed_history(home, history)
        env = dict(os.environ, LOCALAPPDATA=str(home), APPDATA=str(home), USERPROFILE=str(home),
                   HOME=str(home), XDG_DATA_HOME=str(home), ENDO_HISTORY_PATH=str(home / "history.json"))
        urls = [f"http://127.0.0.1:{port}/{name}" for name in files]
        cmd = [*argv, "-q", "-s", "16", "-d", str(out)]
        if len(urls) == 1:
            cmd += urls
        else:
            (work / "urls.txt").write_text("\n".join(urls) + "\n")
            cmd += ["-i", str(work / "urls.txt")]
        began = time.perf_counter()
        proc = subprocess.Popen(cmd, env=env, cwd=work, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            _, stderr = proc.communicate(timeout=args.timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate()
            return "hung", None
        seconds = time.perf_counter() - began
        if proc.returncode != 0:
            print(f"    stderr: {stderr.strip()[-400:]}", file=sys.stderr)
            return f"exit {proc.returncode}", seconds
        for name in files:
            path = out / name
            if not path.is_file() or sha256_file(path) != expected[name]:
                return f"bad: {name}", seconds
        return "ok", seconds
    finally:
        server.kill()
        server.wait()
        shutil.rmtree(work, ignore_errors=True)


def summarize(results):
    """(median, fastest, slowest) seconds of the passing runs, all None without one."""
    passed = [s for status, s in results if status == "ok"]
    if not passed:
        return None, None, None
    return statistics.median(passed), min(passed), max(passed)


def wall(seconds):
    return f"{seconds:.2f} s" if seconds is not None else "-"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--old", required=True, type=Path, help="v1.0 CLI binary (commit 976365a)")
    parser.add_argument("--new", required=True, type=Path, help="current CLI binary")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=180, help="seconds before a run counts as hung")
    parser.add_argument("--rate-mib", type=float, default=2.0)
    parser.add_argument("--latency-ms", type=float, default=40.0)
    parser.add_argument("--connect-ms", type=float, default=80.0, help="connection setup time in the handshake scenario")
    parser.add_argument("--only", help="comma-separated scenario ids: " + ",".join(s[0] for s in SCENARIOS))
    throttled_server.add_file_args(parser)
    args = parser.parse_args()
    for binary in (args.old, args.new):
        if not binary.is_file():
            sys.exit(f"not a file: {binary}")
    old, new = str(args.old.resolve()), str(args.new.resolve())
    only = set(args.only.split(",")) if args.only else None

    sizes = throttled_server.build_files(args.large_mib, args.small_count, args.small_kib,
                                         args.medium_count, args.medium_mib)
    groups = {"large": ["large.bin"], "small": [n for n in sizes if n != "large.bin"]}
    expected = {}

    rows = []
    for sid, title, flags, group, history in SCENARIOS:
        if only and sid not in only:
            continue
        files = groups[group]
        for name in files:
            expected.setdefault(name, throttled_server.sha256_of(name, sizes[name]))
        total = sum(sizes[n] for n in files)
        configs = variants(group, old, new)
        results = {label: [] for label, _ in configs}
        for i in range(args.runs):
            for label, argv in configs:
                status, seconds = run_once(args, argv, flags(args), files, expected, history)
                shown = f"{seconds:.2f}s" if seconds is not None else f">{args.timeout:.0f}s"
                print(f"[{sid}] {label} run {i + 1}/{args.runs}: {status} {shown}", file=sys.stderr, flush=True)
                results[label].append((status, seconds))
        for label, _ in configs:
            median, fastest, slowest = summarize(results[label])
            passed = sum(status == "ok" for status, _ in results[label])
            rows.append((title, label, median, fastest, slowest, total / median / 1e6 if median else None,
                         f"{passed}/{args.runs}", ", ".join(sorted({st for st, _ in results[label]}))))

    print(f"\nServer: {args.rate_mib} MiB/s per connection, {args.latency_ms:g} ms latency per request, "
          f"{args.connect_ms:g} ms connection setup in the handshake scenario; "
          f"{args.runs} run(s) each, timeout {args.timeout:g}s.\n")
    print("| Scenario | Binary | Median wall | Min | Max | MB/s | Passed | Outcomes |")
    print("|---|---|---:|---:|---:|---:|---:|---|")
    for title, label, median, fastest, slowest, mbps, passed, outcomes in rows:
        rate = f"{mbps:.1f}" if mbps else "-"
        print(f"| {title} | {label} | {wall(median)} | {wall(fastest)} | {wall(slowest)} | {rate} "
              f"| {passed} | {outcomes} |")


if __name__ == "__main__":
    main()
