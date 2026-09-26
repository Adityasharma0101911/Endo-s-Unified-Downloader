<p align="center">
  <img src="assets/logo.svg" alt="Endo's Unified Downloader" width="128" height="128" />
</p>

<h1 align="center">Endo's Unified Downloader</h1>

<p align="center">
  A multi-connection download accelerator written in Rust, with a command-line tool and a desktop GUI.
</p>

---

## Features

**Fast downloads**
- Splits a file into chunks and downloads them over several HTTP/1.1 connections at once (one TCP connection per worker, up to 64). Workers that finish early take over part of the slowest remaining range (work stealing).
- Several mirror URLs of the same file are used together. Every mirror is probed first, and a mirror that reports a different size or a different strong ETag is dropped, so bytes from different files are never mixed.
- An optional speed limit (`--max-speed`) is shared by all connections of a download.

**Safe, resumable files**
- A download is written to `<name>.part`. Its resume state is saved to `<name>.part.hfstate` before the first byte arrives and again every 2 seconds. When the download finishes, the data is flushed to disk and the file is renamed to its final name, so a file at its final name is always complete.
- Stopping (Ctrl+C, `systemctl stop`) saves the resume state. Running the same command again continues where it stopped, but only if the server still reports the same size, validators (ETag/Last-Modified) and range support. Otherwise the stale `.part` is discarded and the download starts over.
- Existing files are never overwritten. A different file with the same name is saved as `name (1).ext`. A file that is already complete (the checksum matches, or history recorded it as completed at that exact path) is not downloaded again.
- Checksums: `sha256:`, `md5:`, `blake3:` or bare hex. The BLAKE3 hash of every finished download is recorded in the history, so `--verify` can check the file later.

**Resilient transfers**
- Stall detection: a connection that receives no data for `--stall-timeout` seconds (default 30) is dropped and retried.
- Retries with exponential backoff and jitter. HTTP 429/503 responses respect `Retry-After` and reduce the number of connections. Mirrors that keep failing are disabled. An attempt that made progress does not count against `--max-retries`.
- Servers without range support or without a known length are downloaded as a single stream.

**Many kinds of input**
- **Link resolvers:** Google Drive (large-file confirmation), MediaFire, Dropbox share links, SourceForge (several mirrors), Archive.org (all replica servers), and web pages with an embedded video (`<video>`, `og:video`).
- **HLS (`.m3u8`):** master and media playlists, AES-128 encrypted segments, `#EXT-X-MAP` init sections, output as `.ts` or `.mp4`, and resumable. Live streams and DRM are not supported.
- **Media sites:** YouTube, Twitch, TikTok, Twitter/X, Vimeo, Reddit, Instagram, Facebook and Dailymotion are handed to [yt-dlp](https://github.com/yt-dlp/yt-dlp) automatically. If yt-dlp is not installed, a managed copy is downloaded and checked against its published SHA-256 sums. ffmpeg is needed to merge separate video and audio streams. Use `--media-preset` to choose the quality and `--cookies-from-browser` for sites that require a login.
- **Magnet links** with HTTP web seeds (`ws=`), and **`.torrent`** files with web seeds (`url-list`). Every file of a multi-file torrent becomes a separate download. BitTorrent peer-to-peer transfer is not supported.
- **Metalink** (`.metalink`, `.meta4`): mirrors are ordered by priority, and the file name and SHA-256/MD5 checksum are taken from the metalink.

---

## Installation

### Windows

Install Rust 1.89 or newer from <https://rustup.rs>, then build the project:

```powershell
git clone https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader.git
cd Endo-s-Unified-Downloader
cargo build --release --locked
```

The build produces two programs:
- `target\release\Endos-Unified-Downloader.exe`, the GUI
- `target\release\Endos-Unified-Downloader-CLI.exe`, the command-line tool

### Linux

```bash
git clone https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader.git
cd Endo-s-Unified-Downloader
./install.sh            # add --gui, --no-gui, --service or --uninstall as needed
```

Run `install.sh` as your normal user. It asks for `sudo` only for the system-wide steps. It does the following:
- installs a C toolchain with apt, dnf, pacman, zypper or apk (Debian/Ubuntu, Fedora/RHEL, Arch, openSUSE, Alpine). No OpenSSL is needed because TLS is provided by rustls.
- installs Rust with rustup (for your user only) if it is missing or older than 1.89.
- runs `cargo build --release --locked`.
- installs `/usr/local/bin/endos-downloader-cli` with the aliases `endos-downloader` and `hyperfetch`. An alias is not created if another program already uses that name.
- installs the GUI as `endos-downloader-gui`, with a menu entry, when a display is present or `--gui` is given.

The installer is safe to run again to upgrade, even while the CLI or the service is running, because the binaries are replaced atomically. To build without installing, use `./build-linux.sh [--gui]`.

---

## Command line

```
Endos-Unified-Downloader-CLI [OPTIONS] [URLS]...
```

All `URLS` given on the command line are **mirrors of one file**. To download several files, use `-i`. Without URLs and without `-i`, an interactive prompt starts. The prompt uses the same options as the command line.

| Option | Description | Default |
| :--- | :--- | :--- |
| `-i, --input-file FILE` | Batch file with one download per line (see below). `-` reads the list from stdin. | |
| `-j, --max-concurrent-downloads N` | Number of batch downloads that run at the same time (1-32). | `1` |
| `-d, --dir DIR` | Directory to save into. It is created if it is missing. | current directory |
| `-o, --output FILE` | Output file name for a single download. It is relative to `-d` when both are given. | name from the server |
| `-s, --split N` | Connections per download (1-64). | `16` |
| `-c, --chunk-size MIB` | Base chunk size in MiB (1-1024). | `4` |
| `--max-speed RATE` | Speed limit per download, e.g. `500K`, `2M`, `1.5MiB`. K/M/G are powers of 1024, and `0` means unlimited. | unlimited |
| `--max-retries N` | Failed attempts allowed per chunk before the download gives up. | `8` |
| `--stall-timeout SECS` | Seconds without data before a connection is retried (1-3600). | `30` |
| `--checksum SUM` | Expected checksum: `sha256:HEX`, `md5:HEX`, `blake3:HEX` or bare hex. Only for a single download or `--verify`. | |
| `--header HEADER` | Authorization header, as `"Authorization: Bearer TOKEN"` or `"Bearer TOKEN"`. Other header names are rejected. | |
| `--load-cookies FILE` | Netscape `cookies.txt` file. | |
| `--proxy URL` | `http://`, `https://`, `socks5://` or `socks5h://` proxy. | |
| `--media-preset PRESET` | `best`, `1080p`, `720p`, `mp3`, `m4a`, or any yt-dlp format selector. With a preset, page URLs that are not direct files are also sent to yt-dlp. | `best` |
| `--cookies-from-browser B` | `chrome`, `edge`, `firefox`, `brave`, `opera` or `vivaldi` (media downloads). | |
| `--concurrent-fragments N` | Connections for yt-dlp media downloads (1-32). | `8` |
| `--history` | List past downloads, including failed and stopped ones, then exit. | |
| `--verify FILE` | Check whether a downloaded or partial file is complete and intact. | |
| `--repair` | Used with `--verify`: download the missing ranges again. | |
| `-q, --quiet` | Hide progress output. Errors and warnings are still printed. | |
| `-v, --verbose` | More log output: `-v` info, `-vv` debug, `-vvv` trace. | warnings |
| `-h, --help` / `-V, --version` | Show help or the version. | |

**Batch files.** Each line is one download. Mirrors of the same file go on one line, separated by spaces. Blank lines and lines starting with `#` are ignored. A `.metalink`, `.meta4` or `.torrent` entry (a local path or a URL) must be on a line of its own and can expand into several downloads. The file may be UTF-8, with or without a BOM, or UTF-16 with a BOM (the default for Windows PowerShell 5 `>` redirects).

```text
# queue.txt
https://example.com/ubuntu.iso https://mirror.example.org/ubuntu.iso
magnet:?xt=urn:btih:...&dn=file.bin&ws=https://seed.example.com/files/
https://example.com/release.meta4
/home/me/debian.torrent
```

**Progress.** Each running download shows a bar with the engine's measured speed, the number of open connections and the ETA. If no data arrives for 5 seconds, the bar shows `STALLED`. Batches also show a total bar. When stderr is not a terminal (journald, cron, pipes), a plain progress line is printed every 10 seconds instead of the bars.

**Stopping.** Press Ctrl+C once, or send SIGTERM, to stop all downloads, save their resume state (within at most 10 seconds) and skip the rest of the batch. Press Ctrl+C a second time to quit immediately.

**Exit status.**

| Code | Meaning |
| :--- | :--- |
| `0` | All downloads finished (or the file verified). |
| `1` | At least one download or repair failed. |
| `2` | Usage error, unreadable input, or the file failed `--verify`. |
| `130` / `143` | Stopped by Ctrl+C (SIGINT) / SIGTERM. |

**Logging.** Warnings from the engine (a dropped mirror, a retried chunk, an ignored option) are printed to stderr. `-v` adds more detail. `RUST_LOG` (for example `RUST_LOG=hyperfetch_core=debug`) overrides the level.

### Examples

Linux:

```bash
# One file over 16 connections into ~/Downloads
endos-downloader https://example.com/file.iso -d ~/Downloads

# Two mirrors of the same file, checksum verified, limited to 5 MiB/s
endos-downloader https://a.example.com/f.iso https://b.example.com/f.iso \
    --checksum sha256:9f86d0... --max-speed 5M

# A batch, three downloads at a time, list read from stdin
curl -s https://example.com/links.txt | endos-downloader -i - -j 3 -d /srv/downloads

# Media: 720p video, cookies from Firefox
endos-downloader "https://www.youtube.com/watch?v=..." --media-preset 720p --cookies-from-browser firefox

# Behind a proxy, with a token
endos-downloader https://api.example.com/export.zip --proxy socks5h://127.0.0.1:1080 \
    --header "Authorization: Bearer $TOKEN"

# Check a file, and repair missing ranges using the URLs from its resume state or history
endos-downloader --verify ~/Downloads/file.iso
endos-downloader --verify ~/Downloads/file.iso --repair
endos-downloader --verify ./file.iso --repair https://example.com/file.iso   # explicit URL
```

Windows (PowerShell):

```powershell
.\Endos-Unified-Downloader-CLI.exe https://example.com/file.zip -s 16 -d "$env:USERPROFILE\Downloads"
.\Endos-Unified-Downloader-CLI.exe https://example.com/file.zip -o "D:\Downloads\renamed.zip"
.\Endos-Unified-Downloader-CLI.exe -i .\links.txt -j 2 -d D:\Downloads --max-speed 2M
Get-Content .\links.txt | .\Endos-Unified-Downloader-CLI.exe -i - -d D:\Downloads
.\Endos-Unified-Downloader-CLI.exe --history
.\Endos-Unified-Downloader-CLI.exe --verify D:\Downloads\file.zip --repair
```

### Verify and repair

`--verify FILE` checks the file, or its `FILE.part` while the download is unfinished. It uses the strongest evidence available: the `--checksum` you give, else the BLAKE3 hash in the history for that exact path, then the `.hfstate` range records and the expected size. A file with no evidence at all is reported as unverifiable, never as complete.

`--repair` downloads only the missing ranges. It takes the URLs from the command line if you give any. Otherwise it uses the mirrors in the file's resume state, then the history entry for the same path. File names alone are never matched. Repair accepts only `206 Partial Content` responses whose range and total size match the file, and it verifies the file again afterwards. A checksum mismatch cannot be repaired, because nothing shows which bytes are wrong. Download the file again instead.

### History

`--history` lists downloads newest first: status (completed, failed, or stopped with its percentage), size, file name and host. Full URLs are not shown because they may contain tokens. The history is stored in:
- Windows: `%LOCALAPPDATA%\EndosUnifiedDownloader\history.json`
- Linux: `$XDG_DATA_HOME/endos-downloader/history.json`, otherwise `~/.hyperfetch/history.json`

Set `ENDO_HISTORY_PATH` to use a different file.

---

## Linux service (download queue)

`./install.sh --service` installs `endos-downloader.service`, creates the `endos-downloader` system user, and creates `/var/downloads` with an empty `queue.txt`. The directory is group-writable: add yourself to the `endos-downloader` group to edit the queue.

```bash
sudo usermod -aG endos-downloader "$USER"                           # then log in again
echo "https://example.com/file.iso" >> /var/downloads/queue.txt
sudo systemctl start endos-downloader        # process the queue now
sudo systemctl enable endos-downloader       # also at every boot
journalctl -u endos-downloader -f            # progress and results
```

The service downloads every line of the queue, two at a time, and then exits. Files that are already complete are skipped on the next run, and interrupted files resume. `systemctl stop` sends SIGTERM, which saves the resume state. The service restarts only after a crash. It does not restart after a failed download, so a dead link cannot cause a restart loop. The unit runs as an unprivileged user with a read-only system, no home-directory access and a restricted set of address families. Its history and the managed yt-dlp are stored in `/var/lib/endos-downloader`.

---

## GUI

`Endos-Unified-Downloader` (`endos-downloader-gui` on Linux) is a native desktop app built on the same engine. On Linux it needs a desktop session with OpenGL, and file dialogs use the desktop's xdg-desktop-portal.

- **Live view:** a chunk map, per-connection progress, a throughput graph, real open-connection count, smoothed speed and ETA. A **STALLED** warning appears after 5 s without data, and downloads with several mirrors get a per-mirror speed table.
- **Pause, Resume, Start Over:** Pause waits until resume state is saved ("Pausing…"). Resume continues the same file with the same settings. Start Over and Delete Leftovers remove only the `.part` and its resume state, never a finished file.
- **Batch queue:** add one download per line (mirrors of one file go on one line, separated by spaces). Each item keeps the folder and options it was added with. Auto-run handles 1–8 downloads at once, with per-item Start, Pause, Resume, Retry, Remove, Open and Folder.
- **Verify & Repair:** checks a file against the BLAKE3 hash recorded when it was downloaded. Results are VERIFIED, INCOMPLETE (with a cancellable repair of just the missing ranges), CHECKSUM MISMATCH or UNVERIFIED.
- **Clipboard watcher:** offers "Download Now" / "Add to Queue" for copied links. It ignores links the app copied itself and magnets without web seeds.
- **Advanced options:** speed limit, retries per chunk, stall timeout, proxy, cookies (file or browser), Authorization header, checksum and media quality.
- **Remembers settings:** folder, connections and advanced options are stored in `gui-settings.json` next to the history. The Authorization header and checksum are never saved.
- **Safe to close:** closing the window pauses running downloads and saves their state (waiting at most 3 s). It also stops yt-dlp. The app uses no CPU while idle.

---

## Architecture

```
  CLI / GUI
      |
      v
  DownloadEngine ---- media hosts ----> yt-dlp (process tree, cancellable)
      |  resolve (Drive, MediaFire, SourceForge, Archive.org, pages)
      |  probe mirrors concurrently, keep identical ones
      |---- .m3u8 ---------------------> HlsEngine (ordered segment pipeline, AES-128)
      v
  ChunkManager (dynamic chunks, work stealing, per-chunk retries)
      |                      \
      v                       v
  HTTP workers x N        MirrorRacer (per-mirror speed, throttling, failover)
      |
      v
  DiskWriter (positional writes into <name>.part, preallocation, fsync)
  DownloadState (<name>.part.hfstate, saved every 2 s and on stop)
```

The engine is the `hyperfetch-core` library crate. `hyperfetch-cli` and `hyperfetch-gui` are the front ends.

---

## Development

```bash
cargo test --workspace          # tests (set ENDO_HISTORY_PATH to keep your real history untouched)
cargo clippy --workspace --all-targets
cargo bench --bench range_bench -p hyperfetch-core
```

---

## License

Licensed under either of the Apache License, Version 2.0 or the MIT license, at your option.
