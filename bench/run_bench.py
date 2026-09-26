#!/usr/bin/env python3
"""Compare two Endo's Unified Downloader CLI binaries against bench/throttled_server.py.

Every run gets a fresh server, a fresh output directory and a throwaway history/home, so the
user's real history is never touched. Output files are verified by SHA-256. Prints a markdown
table of the median wall time over passing runs.
"""

import argparse
import hashlib
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

# (id, title, server flags, files: "large" or "small")
SCENARIOS = [
    ("large", "Large file, -s 16", [], "large"),
    ("small", "Small-file batch via -i", [], "small"),
    ("no-accept-ranges", "Large file, HEAD without Accept-Ranges", ["--head-without-accept-ranges"], "large"),
    ("stall", "Large file, one connection stalls forever", ["--stall"], "large"),
]


def variants(scenario, old, new):
    """(label, argv prefix) of every binary configuration run in a scenario."""
    if scenario == "small":
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


def run_once(args, argv, server_flags, files, expected):
    """Returns (status, seconds). Status is "ok", "hung", "exit N" or "bad: <file>"."""
    server, port = start_server(args, server_flags)
    work = Path(tempfile.mkdtemp(prefix="endo-bench-"))
    try:
        out, home = work / "out", work / "home"
        out.mkdir()
        home.mkdir()
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--old", required=True, type=Path, help="v1.0 CLI binary (commit 976365a)")
    parser.add_argument("--new", required=True, type=Path, help="current CLI binary")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=180, help="seconds before a run counts as hung")
    parser.add_argument("--rate-mib", type=float, default=2.0)
    parser.add_argument("--latency-ms", type=float, default=40.0)
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
    for sid, title, server_flags, group in SCENARIOS:
        if only and sid not in only:
            continue
        files = groups[group]
        for name in files:
            expected.setdefault(name, throttled_server.sha256_of(name, sizes[name]))
        total = sum(sizes[n] for n in files)
        for label, argv in variants(sid, old, new):
            results = []
            for i in range(args.runs):
                status, seconds = run_once(args, argv, server_flags, files, expected)
                shown = f"{seconds:.2f}s" if seconds is not None else f">{args.timeout:.0f}s"
                print(f"[{sid}] {label} run {i + 1}/{args.runs}: {status} {shown}", file=sys.stderr, flush=True)
                results.append((status, seconds))
            passed = [s for status, s in results if status == "ok"]
            median = statistics.median(passed) if passed else None
            rows.append((title, label, median, total / median / 1e6 if median else None,
                         f"{len(passed)}/{args.runs}", ", ".join(sorted({st for st, _ in results}))))

    print(f"\nServer: {args.rate_mib} MiB/s per connection, {args.latency_ms:g} ms latency per request; "
          f"{args.runs} run(s) each, timeout {args.timeout:g}s.\n")
    print("| Scenario | Binary | Median wall | MB/s | Passed | Outcomes |")
    print("|---|---|---:|---:|---:|---|")
    for title, label, median, mbps, passed, outcomes in rows:
        wall = f"{median:.2f} s" if median else "-"
        rate = f"{mbps:.1f}" if mbps else "-"
        print(f"| {title} | {label} | {wall} | {rate} | {passed} | {outcomes} |")


if __name__ == "__main__":
    main()
