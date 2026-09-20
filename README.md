<p align="center">
  <img src="assets/logo.svg" alt="Endo's Unified Downloader Logo" width="160" height="160" />
</p>

<h1 align="center">⚡ Endo's Unified Downloader</h1>

<p align="center">
  <strong>Next-Generation High-Speed Download Accelerator & Multi-Source File Ingestion Engine</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Language-Rust-orange.svg" alt="Rust" />
  <img src="https://img.shields.io/badge/License-MIT%20%2F%20Apache--2.0-blue.svg" alt="License" />
  <img src="https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS-purple.svg" alt="Platform" />
  <img src="https://img.shields.io/badge/Status-Active%20Development-success.svg" alt="Status" />
</p>

---

## 🌟 Overview

**Endo's Unified Downloader** is an ultra-fast, cross-platform download accelerator engineered from the ground up in Rust to surpass legacy tools like `aria2c`, `axel`, and `IDM`. 

It combines **adaptive dynamic chunking with work-stealing**, **multi-source & mirror racing**, **Archive.org multi-cluster auto-discovery**, **zero-overhead memory-mapped I/O**, and **crash-resilient WAL state tracking** into a unified, high-throughput pipeline.

---

## 🚀 Key Features

- **⚡ Adaptive Dynamic Chunking (Work Stealing):**
  - Files are dynamically partitioned into adaptive sub-ranges.
  - Faster connections automatically steal and split remaining byte-ranges from slow or stalled connections, eliminating the classic "stuck at 99%" bottleneck.
- **🌐 Archive.org Smart Multi-Cluster Resolver:**
  - Automatically queries the Internet Archive's Metadata API to discover all underlying physical server nodes (`workable_servers`).
  - Distributes connections across multiple physical data-center clusters simultaneously to bypass single-server throttling (yielding **80–120 MB/s** on Archive.org).
- **🏎️ Multi-Source & Mirror Racing:**
  - Accepts multiple mirror URLs for the same resource.
  - Dynamically routes chunk requests to the fastest mirror based on real-time EWMA (Exponential Weighted Moving Average) bandwidth and TTFB tracking.
- **💾 Zero-Overhead Disk Subsystem:**
  - **OS-Native Preallocation:** Utilizes `FILE_ALLOCATION_INFO` (Windows) and `posix_fallocate` (Unix) to eliminate disk fragmentation without allocation stalls.
  - **Memory-Mapped Direct Writes:** Incoming network buffers write directly into mapped memory (`memmap2`), avoiding OS page-cache thrashing.
  - **Streaming BLAKE3 Verification:** Real-time chunk validation detects and re-requests corrupt chunks immediately without discarding the file.
- **🛡️ Crash Resilience & Zero-Scan Resume:**
  - Maintains a lightweight atomic `.hfstate` WAL file.
  - Resumes interrupted downloads instantaneously from the exact last downloaded byte with zero disk scanning.
- **🖥️ Dual Mode (Interactive UI + Power CLI):**
  - **Interactive Mode:** Double-click the `.exe` to launch an interactive terminal UI with paste support, custom connection limits, and download directory selection.
  - **CLI Mode:** Full command-line support for scripts, automation, and power users.

---

## 📦 Quick Start

### 1. Download Precompiled Executable
Run the standalone Windows executable directly from the repository root:
- `Endo's Unified Downloader.exe` (Interactive double-click mode)
- `Endos-Unified-Downloader.exe` (CLI mode)

### 2. Building from Source
Prerequisites: [Rust & Cargo](https://rustup.rs/) (1.80+)

```bash
# Clone the repository
git clone https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader.git
cd Endo-s-Unified-Downloader

# Build release binary
cargo build --release
```

The optimized binary will be available at `target/release/Endos-Unified-Downloader.exe`.

---

## 💻 Usage

### Interactive Mode (Default)
Simply run the executable with no arguments:
```powershell
.\Endos-Unified-Downloader.exe
```
Follow the on-screen prompts to paste URLs, adjust connections (default: 16), and select your destination folder.

### Command-Line Mode
```powershell
# High-speed multi-connection download (16 parallel streams)
.\Endos-Unified-Downloader.exe https://example.com/largefile.zip -s 16

# Racing multiple mirrors simultaneously
.\Endos-Unified-Downloader.exe https://mirror1.com/file.iso https://mirror2.com/file.iso -s 32

# Custom output destination and chunk size
.\Endos-Unified-Downloader.exe https://example.com/file.zip -s 16 -c 8 -o "C:\Downloads\file.zip"
```

---

## 🏛️ Architecture

```
                    ┌──────────────────────────────────────────────┐
                    │               CLI / TUI / RPC                │
                    └──────────────────────┬───────────────────────┘
                                           │
                                           ▼
                    ┌──────────────────────────────────────────────┐
                    │                DownloadEngine                │
                    │   - Orchestrates workers & mirrors           │
                    │   - Coordinates lifecycle & resume           │
                    └──────────┬───────────────────────┬───────────┘
                               │                       │
               ┌───────────────▼─────────┐   ┌─────────▼──────────────┐
               │      ChunkManager       │   │      MirrorRacer       │
               │  - Sparse interval tree │   │  - EWMA latency/speed  │
               │  - Work-stealing logic  │   │  - Dynamic routing     │
               │  - Bitfield & WAL state │   │  - Failover & pacing   │
               └───────────────┬─────────┘   └─────────┬──────────────┘
                               │                       │
                               ▼                       ▼
                    ┌──────────────────────────────────────────────┐
                    │                 HttpWorker                   │
                    │   - reqwest/hyper HTTP/1.1 & HTTP/2 streaming│
                    │   - Cooperative range cancellation           │
                    └──────────────────────┬───────────────────────┘
                                           │
                                           ▼
                    ┌──────────────────────────────────────────────┐
                    │                 DiskWriter                   │
                    │   - OS-native preallocation (sparse/valid)   │
                    │   - Memory-mapped writes (memmap2)           │
                    │   - Streaming BLAKE3 chunk verification      │
                    └──────────────────────────────────────────────┘
```

---

## 🧪 Testing & Benchmarks

Run the test suite:
```bash
cargo test --all
```

Run Criterion benchmarks for interval arithmetic and work-stealing scheduling:
```bash
cargo bench --bench range_bench
```

---

## 📜 License

Licensed under either of:
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
