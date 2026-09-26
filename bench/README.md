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
(`--timeout`) is killed and counted as hung. Every run gets a fresh server, a fresh output
directory and a temporary `LOCALAPPDATA`/`HOME`/`XDG_DATA_HOME`/`ENDO_HISTORY_PATH`, so your
real download history is never touched. Every output file is checked by SHA-256. The table
shows the median wall time of the passing runs; progress goes to stderr, the table to stdout.

| Scenario | Command lines |
|---|---|
| `large`: 256 MiB file | `-s 16` |
| `small`: 40 x 256 KiB + 10 x 1 MiB via `-i` | v1.0 (always sequential); new `-j 1`; new `-j 4` |
| `no-accept-ranges`: 256 MiB, HEAD omits `Accept-Ranges` | `-s 16` |
| `stall`: 256 MiB, one connection stalls forever mid-body | `-s 16` (new: default 30 s stall timeout) |

All runs also pass `-q -d <tempdir>`. Pick scenarios with `--only large,stall`; change the
server with `--rate-mib`, `--latency-ms`, `--large-mib`, `--small-count`, `--small-kib`,
`--medium-count`, `--medium-mib`.

## Server alone

```sh
python bench/throttled_server.py --port 8000             # prints READY 8000
curl -r 0-99 http://127.0.0.1:8000/large.bin | xxd | head
python bench/throttled_server.py --help                  # fault modes and sizes
python bench/test_throttled_server.py                    # self-check, prints ok
```
