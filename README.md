<p align="center">
  <img src="assets/logo.svg" alt="Endo's Unified Downloader" width="128" height="128" />
</p>

<h1 align="center">Endo's Unified Downloader</h1>

<p align="center">
  High-performance, multi-source file ingestion engine and download accelerator written in Rust.
</p>

---

## Overview

Endo's Unified Downloader is a systems-level download utility engineered to maximize throughput across constrained or throttled connections. It replaces traditional static byte-range downloaders with an adaptive work-stealing engine, automated multi-cluster mirror discovery, zero-copy memory-mapped disk writes, and zero-scan crash resumption.

---

## Core Capabilities

### Adaptive Dynamic Chunking (Work Stealing)
Instead of dividing files into fixed static ranges, the engine partitions files into dynamic chunks (default: 4 MB). Workers that complete their assigned ranges early inspect active streams and split the remaining byte-range of the slowest in-flight connection at its midpoint. This prevents single slow streams from gating overall download completion.

### Universal Host Resolvers & Landing Page Bypass
Endo features an extensible `HostResolver` pipeline that detects host patterns and resolves them to direct multi-stream endpoints:
- **Google Drive:** Automatically extracts file IDs and bypasses the "Google Drive can't scan this file for viruses" confirmation gate on files >100MB, streaming directly from `docs.googleusercontent.com`.
- **MediaFire:** Automatically scrapes landing pages to extract direct high-speed CDN streaming links (`downloadXXXX.mediafire.com`), bypassing web countdowns and advertisements.
- **Archive.org:** Queries the Metadata API (`/metadata/:id`) to extract all replica hosts in `workable_servers`, racing across 3–5 physical data-center clusters simultaneously.
- **SourceForge:** Harvests 5+ global CDN mirrors (`fastly`, `heanet`, `jaist`, `liquidtelecom`) and feeds them into `MirrorRacer` concurrently.
- **Dropbox:** Automatically normalizes preview links (`dl=0`) to raw binary streaming links (`dl=1`).
- **Anti-QoS Browser Headers:** Emulates modern browser headers (`Sec-Ch-Ua`, Chrome 124 user-agent) to prevent hosters from routing connections to low-priority bandwidth queues.

### Mirror Racing
When provided with multiple mirror URLs for a resource, the engine maintains an Exponential Weighted Moving Average (EWMA) of latency (TTFB) and throughput for each host. Requests are dynamically routed to whichever host currently yields the highest sustained transfer rate, with automated backoff for throttled endpoints.

### Zero-Overhead Disk Subsystem
- **OS Preallocation:** Uses `FILE_ALLOCATION_INFO` on Windows and `posix_fallocate` on Unix to preallocate file boundaries immediately, avoiding file fragmentation and runtime allocation stalls.
- **Memory-Mapped I/O:** Uses `memmap2` to write incoming network buffers directly into mapped virtual memory, avoiding OS buffer-cache thrashing at high throughput.
- **Streaming Verification:** Validates completed chunks via BLAKE3 hashes. Corrupted chunks are invalidated and re-requested individually without discarding the rest of the file.

### Crash Resilience (.hfstate)
State is maintained in an atomic binary write-ahead log (`.hfstate`). If a transfer is interrupted by network failure or process termination, the engine reloads completed ranges instantly with zero disk re-reading.

---

## Building from Source

Prerequisites: Rust 1.80 or later.

```bash
git clone https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader.git
cd Endo-s-Unified-Downloader
cargo build --release
```

The compiled binary will be placed at `target/release/Endos-Unified-Downloader.exe`.

---

## Usage

### Interactive Mode
Running the executable without arguments launches the interactive console interface:

```powershell
.\Endos-Unified-Downloader.exe
```

Prompts will guide you through entering URLs, configuring connection limits, and choosing the output directory (defaults to the user's Downloads folder).

### Command-Line Mode

```powershell
# Basic download with 16 parallel connections
.\Endos-Unified-Downloader.exe https://example.com/file.zip -s 16

# Racing multiple mirrors simultaneously
.\Endos-Unified-Downloader.exe https://mirror1.com/file.iso https://mirror2.com/file.iso -s 32

# Custom destination and chunk size
.\Endos-Unified-Downloader.exe https://example.com/file.zip -s 16 -c 8 -o "C:\Downloads\file.zip"
```

### Options

| Flag | Long Flag | Description | Default |
| :--- | :--- | :--- | :--- |
| `-s` | `--split` | Number of concurrent worker connections | `16` |
| `-c` | `--chunk-size` | Base chunk size in megabytes | `4` |
| `-o` | `--output` | Destination file or directory path | Inferred / Downloads |
| `-h` | `--help` | Print help information | |
| `-V` | `--version` | Print version information | |

---

## Architecture

```
                    +----------------------------------------------+
                    |               CLI / Interactive              |
                    +----------------------+-----------------------+
                                           |
                                           v
                    +----------------------------------------------+
                    |                DownloadEngine                |
                    |   - Orchestrates workers & mirrors           |
                    |   - Coordinates lifecycle & resume           |
                    +----------------------+-----------------------+
                                           |
                                           v
               +---------------------------+--------------------------+
               |                                                      |
               v                                                      v
+------------------------------+                       +------------------------------+
|         ChunkManager         |                       |         MirrorRacer          |
|  - Sparse interval tracking  |                       |  - EWMA latency/throughput   |
|  - Work-stealing scheduler   |                       |  - Dynamic mirror routing    |
|  - Bitfield & WAL state      |                       |  - Failover & backoff        |
+--------------+---------------+                       +--------------+---------------+
               |                                                      |
               +---------------------------+--------------------------+
                                           |
                                           v
                    +----------------------------------------------+
                    |                 HttpWorker                   |
                    |   - Async byte-range streaming (reqwest)     |
                    |   - Cooperative range cancellation           |
                    +----------------------+-----------------------+
                                           |
                                           v
                    +----------------------------------------------+
                    |                 DiskWriter                   |
                    |   - OS-native preallocation                  |
                    |   - Memory-mapped writes (memmap2)           |
                    |   - Streaming BLAKE3 verification            |
                    +----------------------------------------------+
```

---

## Verification & Tests

Run the full test suite:
```bash
cargo test --all
```

Run Criterion benchmarks:
```bash
cargo bench --bench range_bench
```

---

## License

Licensed under either of:
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
