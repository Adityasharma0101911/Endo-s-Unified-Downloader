## Endo's Unified Downloader v1.0.0

High-speed unified download accelerator, ingestion engine, and native desktop GUI written in Rust.

---

### Downloads & Assets

| Asset | Description | Size |
| :--- | :--- | :--- |
| **`Endos-Unified-Downloader.exe`** | Standalone Hardware-Accelerated Native GUI (Windows x64) | 7.2 MB |
| **`Endos-Unified-Downloader-CLI.exe`** | High-Performance Command-Line Tool (Windows x64) | 4.3 MB |
| **`Endos-Unified-Downloader-v1.0.0-windows-x64.zip`** | Complete Windows Bundle (GUI + CLI + Documentation) | 5.6 MB |

---

### Core Highlights & Features

1. **Ultra-Fast Parallel Engine & Dynamic Work-Stealing**
   - Splits files across up to 64 concurrent chunk connections with HTTP Range requests.
   - Dynamic midpoint work-stealing ensures idle workers automatically assist slower streams so no worker idles while bytes remain.
   - Preallocates disk blocks to prevent file fragmentation.

2. **Hardware-Accelerated Native GUI**
   - Real-time chunk allocation canvas with 60 FPS animation.
   - Per-stream progress tracking, interactive queue manager, and instant clipboard URL ingestion.
   - Live download history browser with filtering and direct folder revelation.

3. **Collision Resolution & Overwrite Protection**
   - Automatically detects identical or already-downloaded files to prevent accidental re-downloads.
   - Safely resumes partial downloads even if `.hfstate` is missing by preserving existing disk bytes.
   - Auto-disambiguates conflicting filenames (`filename (1).ext`) to guarantee zero accidental data loss.

4. **Build Chunk Verification & Selective Repair**
   - Inspects completed or partial game builds, ISOs, and large archives (`--verify`).
   - Identifies missing byte gaps and downloads only missing ranges (`--repair`) without redownloading existing valid chunks.

5. **100% Linux Server & Headless Support**
   - Includes one-command installer (`install.sh`) supporting Ubuntu, Debian, Fedora, Arch Linux.
   - Headless CLI with batch file input (`-i`), quiet mode (`-q`), and systemd unit integration (`endos-downloader.service`).
   - Robust POSIX signal handling (`SIGINT` and `SIGTERM`) flushes memory-mapped writes and writes state for resume.

---

### CLI Quickstart

```powershell
# High-speed download with 16 connections
.\Endos-Unified-Downloader-CLI.exe https://example.com/archive.iso -s 16

# Multi-mirror racing download
.\Endos-Unified-Downloader-CLI.exe https://mirror1.org/data.zip https://mirror2.org/data.zip -s 32

# Verify and repair build chunks
.\Endos-Unified-Downloader-CLI.exe --verify "C:\Downloads\archive.iso" --repair
```

---

### Linux Installation (One Command)

```bash
git clone https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader.git
cd Endo-s-Unified-Downloader
chmod +x install.sh
./install.sh
```

---

### SHA-256 Checksums

```text
E0EACD04F8EDB0B51335847C7FA8D908FF9AD42A3C501C2D946A862B4DDE890A  Endos-Unified-Downloader.exe
3A841051B81AE4348574F687A008E742E50E35641833FFCAC6D6693E1E9336CB  Endos-Unified-Downloader-CLI.exe
9BE1A135832F44984CE15316D1A3B528C2212CDD79BAA60D712F4687D174F727  Endos-Unified-Downloader-v1.0.0-windows-x64.zip
```
