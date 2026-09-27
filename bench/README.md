# Benchmarks

Compares the released v1.0 CLI (commit `976365a`) with the current CLI on a local server that caps
every connection at 2 MiB/s and adds 40 ms before every response. Python 3.9+, stdlib only.

## Build the two binaries

v1.0, in a throwaway worktree outside the checkout (Windows paths; use any stable location):

```sh
git worktree add --detach C:/tmp/endo-bench/endo-v1 976365a
cd C:/tmp/endo-bench/endo-v1 && cargo build --release -p hyperfetch-cli
mkdir -p C:/tmp/endo-bench/bin
cp target/release/Endos-Unified-Downloader-CLI.exe C:/tmp/endo-bench/bin/endo-v1.0-cli.exe
cd - && git worktree remove --force C:/tmp/endo-bench/endo-v1
```

Current build, from the repository root:

```sh
cargo build --release -p hyperfetch-cli
```

## Run

```sh
python bench/run_bench.py --old C:/tmp/endo-bench/bin/endo-v1.0-cli.exe \
                          --new target/release/Endos-Unified-Downloader-CLI.exe
```

Each scenario runs 3 times per binary (`--runs`); a run that takes longer than 180 s
(`--timeout`) is killed and counted as hung. The binaries take turns (v1.0, new, v1.0, new, ...),
so a machine that gets busier or quieter during the benchmark affects both alike. Every run gets
a fresh server, a fresh output directory and a temporary
`LOCALAPPDATA`/`HOME`/`XDG_DATA_HOME`/`ENDO_HISTORY_PATH`, so your real download history is never
touched. Every output file is checked by SHA-256. The table shows the median, fastest (Min) and
slowest (Max) wall time of the passing runs, so an occasional slow run (a disk flush that
stalls, say) shows next to a median it does not move; progress goes to stderr, the table to
stdout.

| Scenario | Command lines |
|---|---|
| `large`: 256 MiB file | `-s 16` |
| `small`: 40 x 256 KiB + 10 x 1 MiB via `-i` | v1.0 (always sequential); new `-j 1`; new `-j 4` |
| `handshake`: `small`, every new connection waits 80 ms first (`--connect-ms`) | as `small` |
| `history`: `small`, with 1000 finished downloads already in the history | as `small` |
| `no-accept-ranges`: 256 MiB, HEAD omits `Accept-Ranges` | `-s 16` |
| `stall`: 256 MiB, one connection stalls forever mid-body | `-s 16` (new: default 30 s stall timeout) |

`handshake` stands in for the TCP and TLS setup of a real HTTPS server, which loopback does not
have: it shows what reusing connections across the downloads of a batch saves. `history` seeds
the history of both binaries, which otherwise start every run empty.

All runs also pass `-q -d <tempdir>`. Pick scenarios with `--only large,stall`; change the
server with `--rate-mib`, `--latency-ms`, `--connect-ms` (the `handshake` scenario's setup
time), `--large-mib`, `--small-count`, `--small-kib`, `--medium-count`, `--medium-mib`.

## Results (2026-09-26, Windows 11, 32 threads, commit `bb0bc43`)

Default server (2 MiB/s per connection, 40 ms per request; 80 ms connection setup in the
slow-setup scenario):

| Scenario | Binary | Median wall | Min | Max | MB/s | Passed | Outcomes |
|---|---|---:|---:|---:|---:|---:|---|
| Large file, -s 16 | v1.0 | 9.00 s | 8.97 s | 9.43 s | 29.8 | 3/3 | ok |
| Large file, -s 16 | new | 8.74 s | 8.64 s | 8.84 s | 30.7 | 3/3 | ok |
| Small-file batch via -i | v1.0 (sequential) | 6.18 s | 6.04 s | 6.44 s | 3.4 | 3/3 | ok |
| Small-file batch via -i | new -j 1 | 7.02 s | 6.83 s | 7.05 s | 3.0 | 3/3 | ok |
| Small-file batch via -i | new -j 4 | 1.72 s | 1.67 s | 1.89 s | 12.2 | 3/3 | ok |
| Small-file batch via -i, slow connection setup | v1.0 (sequential) | 14.32 s | 14.12 s | 14.41 s | 1.5 | 3/3 | ok |
| Small-file batch via -i, slow connection setup | new -j 1 | 10.84 s | 10.60 s | 11.31 s | 1.9 | 3/3 | ok |
| Small-file batch via -i, slow connection setup | new -j 4 | 3.10 s | 2.94 s | 3.31 s | 6.8 | 3/3 | ok |
| Small-file batch via -i, 1000-entry history | v1.0 (sequential) | 6.48 s | 6.39 s | 6.57 s | 3.2 | 3/3 | ok |
| Small-file batch via -i, 1000-entry history | new -j 1 | 7.38 s | 6.49 s | 7.41 s | 2.8 | 3/3 | ok |
| Small-file batch via -i, 1000-entry history | new -j 4 | 1.94 s | 1.91 s | 2.08 s | 10.8 | 3/3 | ok |
| Large file, HEAD without Accept-Ranges | v1.0 | 128.66 s | 128.62 s | 130.16 s | 2.1 | 3/3 | ok |
| Large file, HEAD without Accept-Ranges | new | 8.52 s | 8.45 s | 8.53 s | 31.5 | 3/3 | ok |
| Large file, one connection stalls forever | v1.0 | - | - | - | - | 0/3 | hung |
| Large file, one connection stalls forever | new | 8.73 s | 8.70 s | 8.82 s | 30.8 | 3/3 | ok |

`--rate-mib 0 --only large,small,handshake` (no per-connection cap):

| Scenario | Binary | Median wall | Min | Max | MB/s | Passed | Outcomes |
|---|---|---:|---:|---:|---:|---:|---|
| Large file, -s 16 | v1.0 | 0.62 s | 0.53 s | 0.68 s | 430.5 | 3/3 | ok |
| Large file, -s 16 | new | 0.32 s | 0.32 s | 0.38 s | 827.5 | 3/3 | ok |
| Small-file batch via -i | v1.0 (sequential) | 4.80 s | 4.78 s | 4.88 s | 4.4 | 3/3 | ok |
| Small-file batch via -i | new -j 1 | 2.31 s | 2.29 s | 2.31 s | 9.1 | 3/3 | ok |
| Small-file batch via -i | new -j 4 | 0.66 s | 0.65 s | 0.67 s | 31.8 | 3/3 | ok |
| Small-file batch via -i, slow connection setup | v1.0 (sequential) | 13.32 s | 13.15 s | 13.46 s | 1.6 | 3/3 | ok |
| Small-file batch via -i, slow connection setup | new -j 1 | 2.52 s | 2.48 s | 2.70 s | 8.3 | 3/3 | ok |
| Small-file batch via -i, slow connection setup | new -j 4 | 0.88 s | 0.78 s | 0.90 s | 23.9 | 3/3 | ok |

## Earlier results (2026-09-26, commit `da1613a`)

Taken before the runs alternated and before the `handshake` and `history` scenarios and the
Min/Max columns existed, with the new build at commit `da1613a`.

Default server (2 MiB/s per connection, 40 ms per request):

| Scenario | Binary | Median wall | MB/s | Passed |
|---|---|---:|---:|---:|
| Large file, -s 16 | v1.0 | 8.50 s | 31.6 | 3/3 |
| Large file, -s 16 | new | 8.48 s | 31.7 | 3/3 |
| Small-file batch via -i | v1.0 (sequential) | 5.98 s | 3.5 | 3/3 |
| Small-file batch via -i | new -j 1 | 9.93 s | 2.1 | 3/3 |
| Small-file batch via -i | new -j 4 | 2.71 s | 7.7 | 3/3 |
| Large file, HEAD without Accept-Ranges | v1.0 | 128.56 s | 2.1 | 3/3 |
| Large file, HEAD without Accept-Ranges | new | 8.41 s | 31.9 | 3/3 |
| Large file, one connection stalls forever | v1.0 | - | - | 0/3 (hung) |
| Large file, one connection stalls forever | new | 35.65 s | 7.5 | 3/3 |

`--rate-mib 0 --only small,large` (no per-connection cap, 40 ms per request):

| Scenario | Binary | Median wall | MB/s | Passed |
|---|---|---:|---:|---:|
| Large file, -s 16 | v1.0 | 0.52 s | 516.7 | 3/3 |
| Large file, -s 16 | new | 0.90 s | 296.7 | 3/3 |
| Small-file batch via -i | v1.0 (sequential) | 4.65 s | 4.5 | 3/3 |
| Small-file batch via -i | new -j 1 | 2.76 s | 7.6 | 3/3 |
| Small-file batch via -i | new -j 4 | 0.77 s | 27.1 | 3/3 |

The uncapped large-file runs move 300–500 MB/s over loopback and are noisy (v1.0 0.51–1.68 s,
new 0.57–2.64 s); at that speed the new build is slower, which is not investigated yet. A small
file comes over the connection
that probed it unless that connection is capped, so v1.0's fixed 16-way split still wins for
tiny files from a server that caps every connection when they are fetched one at a time.

## Server alone

```sh
python bench/throttled_server.py --port 8000             # prints READY 8000
python bench/throttled_server.py --connect-ms 80         # every new connection waits 80 ms first
curl -r 0-99 http://127.0.0.1:8000/large.bin | xxd | head
python bench/throttled_server.py --help                  # fault modes and sizes
python bench/test_throttled_server.py                    # self-check, prints ok
```
