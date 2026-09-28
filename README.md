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
- Splits a file into chunks and downloads them over several HTTP/1.1 connections at once (one TCP connection per worker, up to 64). The probe's answer goes on as the first chunk, and the other connections start as soon as the size is known.
- Work stealing is decided by time: a worker that finishes early takes over the part of the slowest chunk that it can finish sooner, split where both finish together, and takes over at once a chunk whose connection has gone silent for 2 s (or 4 answer times).
- Several mirror URLs of the same file are used together. The download starts with the first mirror that answers with range support, and the others join as their probes come back. A mirror that reports a different size or a different strong ETag is dropped, so bytes from different files are never mixed.
- Chunk requests go straight to where a mirror redirects (GitHub releases, SourceForge, Dropbox), and back to the mirror's own URL if that target expires.
- An optional speed limit (`--max-speed`) is shared by all connections of a download, HLS segments and the streams of a media download included, and by all downloads running at once (a batch, the GUI queue).
- Batches run 4 downloads at once by default (`-j`). All running downloads together open at most 64 connections to one host (`--max-connections-per-host`), probes and HLS playlist, key and segment requests included, and the downloads of a batch share their HTTP connections and TLS sessions, so later files skip the handshakes.
- What a host was seen to do is remembered for 10 minutes: whether it takes ranges, whether it caps each connection's speed (and at what rate), how long a new connection takes, and a connection cap learned from 429 answers. Later downloads from it start at the right width without measuring again.

**Safe, resumable files**
- A download is written to `<name>.part`. Its resume state is saved to `<name>.part.hfstate` before the first byte arrives and again every 2 seconds, each time after the data it records has been flushed to disk. When the download finishes, the file is renamed to its final name, so a file at its final name is always complete.
- A finished file is not flushed to disk before it is reported done; the operating system writes it out on its own schedule, as with curl, wget and browsers. A power loss right after a download finishes can therefore leave a damaged file, which `--verify` detects. `--fsync` (GUI: **Flush finished files to disk**) waits for each finished file to reach the disk first.
- Stopping (Ctrl+C, `systemctl stop`) saves the resume state. Running the same command again continues where it stopped, but only if the server still reports the same size, validators (ETag/Last-Modified) and range support. Otherwise the stale `.part` is discarded and the download starts over.
- Existing files are never overwritten. A different file with the same name is saved as `name (1).ext`. A file that is already complete (the checksum matches, or history recorded it as completed at that exact path) is not downloaded again.
- Checksums: `sha256:`, `sha512:`, `sha1:`, `md5:`, `blake3:` or bare hex. The BLAKE3 hash of every finished download is recorded in the history, so `--verify` can check the file later. The hashes are taken while the file is written (SHA-256, SHA-512, SHA-1 and MD5 follow the file's start as it grows, so with such a checksum chunks are at most 8 MiB unless `-c` sets their size), and finishing a download reads back only what was not hashed on the way. A mismatch is confirmed by reading the whole file before it is discarded.
- A server that ignores `If-Range` and sends part of a changed file is caught by the ETag or Last-Modified date of its answer, and the download stops instead of mixing two versions.

**Resilient transfers**
- Stall detection: a connection that receives no data for `--stall-timeout` seconds (default 30) is dropped and retried. A chunk's connection that goes quiet mid-body is replaced after 5 s (or the stall timeout, if shorter), keeping what it received.
- Retries with exponential backoff and jitter. HTTP 429/503 responses respect `Retry-After` and reduce the number of connections. Mirrors that keep failing are disabled. An attempt that made progress does not count against `--max-retries`.
- Servers without range support or without a known length are downloaded as a single stream, which goes on with the probe's answer instead of asking again.

**Many kinds of input**
- **Link resolvers:** Google Drive (large-file confirmation), MediaFire, Dropbox share links, SourceForge (several mirrors), Archive.org (all replica servers), and web pages with an embedded video (`<video>`, `og:video`). Whether a Google Drive link, or a URL that may be a web page (no extension, or a page one), answers with a page is read from the probe's own answer, not from a request of its own, and an Archive.org item is asked only for the three metadata fields that name its servers, once for all its files.
- **HLS (`.m3u8`):** master and media playlists, AES-128 encrypted segments, `#EXT-X-MAP` init sections, output as `.ts` or `.mp4`, and resumable. Segments come over several connections in a sliding window, so a slow segment does not hold up the others (it is requested a second time once it lags), and are written by a writer of their own, which saves the resume state every 2 s. AES keys are fetched in parallel, adjoining byte ranges of one file are fetched together, and `--stall-timeout` and `--max-retries` apply. Live streams and DRM are not supported.
- **Media sites:** YouTube, Twitch, TikTok, Twitter/X, Vimeo, Reddit, Instagram, Facebook and Dailymotion are handed to [yt-dlp](https://github.com/yt-dlp/yt-dlp) automatically. yt-dlp finds the formats, and when they are plain files or HLS playlists the engine downloads the video and audio together over its own connections, then ffmpeg joins them without re-encoding; anything else, or a failure, falls back to yt-dlp's own download, which reuses that extraction. If yt-dlp is not installed, a managed copy is downloaded and checked against its published SHA-256 sums (on Windows the unpacked build, which starts faster). Its version is cached on disk, and browser cookies are read once and reused for 15 minutes. ffmpeg is needed to merge separate video and audio streams. Use `--media-preset` to choose the quality (`m4a` picks an AAC source, so the audio is copied, not re-encoded) and `--cookies-from-browser` for sites that require a login.
- **Magnet links** with HTTP web seeds (`ws=`), and **`.torrent`** files with web seeds (`url-list`). Every file of a multi-file torrent becomes a separate download. BitTorrent peer-to-peer transfer is not supported: a `.torrent` URL without web seeds downloads the `.torrent` file itself, for a torrent client. Padding files (BEP 47) are skipped, and a BitTorrent v2-only torrent is an error. A `.torrent`, `.metalink` or `.meta4` link is read whatever type its server labels it (PHP labels everything a web page), and one to a GitHub or Hugging Face file page or a Dropbox share from the file itself; one whose host refuses it (a login, or GitHub's "not found" for a private repository) or answers with a web page is downloaded as the file it is, from the link you gave, with your cookies and Authorization header, and a web page in its place is an error. A host that is busy (a timeout, a rate limit) or fails is an error, to try again. A checksum you give is for the file a document lists, so a document downloaded itself is not checked against it.
- **Metalink** (`.metalink`, `.meta4`): mirrors are ordered by priority, and the file name and checksum (the strongest well-formed one of SHA-512, SHA-256, SHA-1 and MD5 it gives) are taken from the metalink. A name with folders (`dir/file.iso`) is saved in those folders; `..` and absolute names are refused. Versions up to 1.1.0 dropped the folders and cleaned names a little differently, so a metalink or torrent download paused by one of them may start again from zero under its new path.

**Links that just work**

A pasted link gives the file or the media it stands for, judged by where it lands, not by the page it may answer with. The same rules apply in the command line and the GUI.
- **Short and wrapped links:** a link from a shortener (bit.ly, TinyURL, git.io, ...) or a mail link scanner (Outlook Safe Links, Proofpoint, Mimecast) is downloaded from where it lands, through that host's resolver or yt-dlp. "You are leaving this site" links (`youtube.com/redirect`, `google.com/url` and Google's country sites, `l.facebook.com`, `l.instagram.com`, Steam's link filter, `out.reddit.com`, LinkedIn, VK, DuckDuckGo) are replaced by the target they hold, without a request, so the queue and history keep the target. A shortener or scanner that shows a page of its own (a preview, a warning) is an error that asks you to open the link in your browser; nothing on it is clicked.
- **t.co:** X's links, and other pages that send the browser on at once (a 0-second refresh), are followed to their target. At most three links are followed past the one given, and never back to one already tried.
- **Code-site file pages:** a "view file" link on GitHub (`/blob/`, a renamed branch included), GitLab (`/-/blob/`), Bitbucket (`/src/`) or Hugging Face (`/blob/`, and `/raw/`, which on its own gives only the pointer of a large file) downloads the file itself. A folder link is an error.
- **Google Docs, Sheets and Slides:** an editor or share link downloads the document as `.docx`, `.xlsx` (every sheet, whichever one the link opens on) or `.pptx`. An export link keeps the format it asks for, such as one sheet's `.csv` (`export?format=csv&gid=...`). A private document is an error that says how to share it, or to pass your browser's cookies with `--load-cookies`.
- **Any site yt-dlp supports:** a web page that plays no video of its own is offered to yt-dlp's site extractors (never its generic one), which fail at once for a site they do not know. When one takes the page, its media is downloaded instead of the page, in the GUI's media quality. So are Box, SharePoint video and Yandex Disk share pages; one that yt-dlp cannot download is an error. Any other page whose site finds no media there (a news article without a video) is downloaded as the page, and one whose site fails at it (an HTTP error, a private, removed or geo-blocked video, a login) is an error. A DRM-protected video is an error, never a download.
- **Documents in the GUI:** `.metalink`, `.meta4` and `.torrent` files and links add one download per file they list (see the GUI section).
- **Pages are refused, not saved:** a link that names a file (`.zip`, `.exe`, `.iso`, `.pdf`, ...) and answers with a web page (a login, an expired link, a download page) is an error, as is a Google Drive page (private, deleted or over quota) and the share page of a service not supported yet (MEGA, OneDrive, SharePoint, WeTransfer, Terabox, Gofile, Pixeldrain, iCloud, pCloud). A file whose server labels it a web page still downloads when its first bytes show it is a file.
- **Secrets stay out of history:** history, and the GUI's saved queue for downloads that finished or failed, keep links without the user name, password, signatures and tokens they carry (`X-Amz-Signature`, `sig`, `key`, `Authorization`, any parameter named `...token`, `..._key`, `...secret` or `...password`, Discord's `hm`, Safe Links' `data`, a Telegram bot's token, ...), including links inside other links and error messages. A link saved without its secret no longer tells one file from another (`object_key=` may name the file), so giving it again finds its finished file only by a link without a secret that it led to (where a short link landed, a resolver's link); otherwise the file is downloaded again, as `name (1).ext`. A Redownload, Retry or repair that would need the secret asks for the link instead. A download that has not finished keeps its links whole so that it can resume: in the saved queue while it waits or is paused, and in its resume file (`<name>.part.hfstate`) until it completes.

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
| `-j, --max-concurrent-downloads N` | Number of batch downloads that run at the same time (1-32). Above 1, each result line names its input (see below). | `4` |
| `--max-connections-per-host N` | Connections all running downloads may open to one host together. `0` means no limit. | `64` |
| `--fsync` | Wait until each finished file is on the disk before reporting it done. | off |
| `-d, --dir DIR` | Directory to save into. It is created if it is missing. | current directory |
| `-o, --output FILE` | Output file name for a single download. It is relative to `-d` when both are given. | name from the server |
| `-s, --split N` | Connections per download (1-64). | `16` |
| `-c, --chunk-size MIB` | Base chunk size in MiB (1-1024). | `4` |
| `--max-speed RATE` | Speed limit for all running downloads together (`-j`), e.g. `500K`, `2M`, `1.5MiB`. K/M/G are powers of 1024, and `0` means unlimited. | unlimited |
| `--max-retries N` | Failed attempts allowed per chunk before the download gives up. | `8` |
| `--stall-timeout SECS` | Seconds without data before a connection is retried (1-3600). | `30` |
| `--checksum SUM` | Expected checksum: `sha256:HEX`, `sha512:HEX`, `sha1:HEX`, `md5:HEX`, `blake3:HEX` or bare hex. Only for a single download or `--verify`. | |
| `--header HEADER` | Authorization header, as `"Authorization: Bearer TOKEN"` or `"Bearer TOKEN"`. Other header names are rejected. It is never sent to the mirrors a `.metalink` or `.torrent` lists. | |
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

**Results.** Each finished download prints `[OK] <path>`, and a failed one `[FAILED] <name>: <reason>` on stderr. With `-j` above 1 results arrive in the order the downloads finish, so each line names its input: the line of the `-i` file that listed it, and its first URL without the user name, password and query string, which may hold credentials: `[OK] line 3 https://example.com/file.iso -> /srv/downloads/file.iso`. The files of one metalink or torrent share its line.

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

`--repair` downloads only the missing ranges. It takes the URLs from the command line if you give any. Otherwise it uses the mirrors in the file's resume state, then the history entry for the same path. File names alone are never matched. Each mirror must first show that it still serves the version the file was downloaded as (same ETag, else Last-Modified) and its size; the others are left out. The missing ranges then come from one of them through the download engine, over up to `-s` connections with work stealing and retries, and the file is verified again afterwards. The engine holds the server to that version with `If-Range`, so this needs a mirror that sends a strong ETag or a Last-Modified date and honors `If-Range`. Otherwise, and for a `.part` whose final name is already taken by another file (which then keeps its `.part` name), the ranges are repaired in place over one connection, checking the version of every answer. Either way a repair never mixes two versions of a file. If the file changes on the server just as the engine starts, the engine downloads the new version whole instead, and the repair reports that. A checksum mismatch cannot be repaired, because nothing shows which bytes are wrong. Download the file again instead.

### History

`--history` lists downloads newest first: status (completed, failed, or stopped with its percentage), size, file name and host. Full URLs are not shown because they may contain tokens, and the history file keeps links without their secrets (see [Links that just work](#features)). The history is stored in:
- Windows: `%LOCALAPPDATA%\EndosUnifiedDownloader\history.json`
- Linux: `$XDG_DATA_HOME/endos-downloader/history.json`, otherwise `~/.hyperfetch/history.json`

Set `ENDO_HISTORY_PATH` to use a different file.

### Subtitles and tags

`--subs LANGS` (GUI: Advanced Options > Subtitles) saves a video's subtitles next to it, as `name.LANG.srt` where the site offers SRT, else `.vtt`, else the site's own format. Give languages as `en,es`, or `all`.
- A site's own subtitles come first. Its automatic captions are used for a language it has no subtitles of its own in. `all` gets every language the site has its own subtitles in, without the automatic translations and YouTube's live chat.
- A language the video has no subtitles in is skipped, not an error. A subtitle that YouTube sends empty without a proof-of-origin (PO) token is skipped with a warning in the log and not retried. No PO-token plugins are used.
- When the engine downloads the streams itself, yt-dlp then writes only the subtitles, from the same extraction.

Tags are embedded by default, when ffmpeg is present. The file gets the title, artist, date, description, link, genre, album and show or episode details, and the video's chapters. Audio presets (`mp3`, `m4a`) also get the thumbnail as cover art. Without ffmpeg the download goes on without tags. `--no-embed-metadata` (GUI: clear "Embed tags and chapters") leaves tags out.

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
- **Batch queue:** add one download per line (mirrors of one file go on one line, separated by spaces). Each item keeps the folder and options it was added with. A `.metalink`, `.meta4` or `.torrent` (a URL, a local path, or a file dropped on the window) adds one download per file it lists, each saved under its own path in the folder; one that lists more than 50 files asks first. The Authorization header is never sent to the mirrors it lists. Auto-run handles 1–8 downloads at once (4 by default; a number you saved before is kept), with per-item Start, Pause, Resume, Retry, Remove, Open and Folder. Downloads share their HTTP connections and TLS sessions.
- **Verify & Repair:** checks a file against the BLAKE3 hash recorded when it was downloaded. Results are VERIFIED, INCOMPLETE (with a cancellable repair of just the missing ranges), CHECKSUM MISMATCH or UNVERIFIED.
- **Clipboard watcher:** offers "Download Now" / "Add to Queue" for copied links. It ignores links the app copied itself and magnets without web seeds.
- **Advanced options:** speed limit (for all running downloads together), retries per chunk, stall timeout, connections per host (64 by default, 0 = no limit), flushing finished files to disk (off by default, see above), proxy, cookies (file or browser), Authorization header, checksum and media quality.
- **Remembers settings:** folder, connections and advanced options are stored in `gui-settings.json` next to the history. The Authorization header and checksum are never saved.
- **Safe to close:** closing the window pauses running downloads and saves their state (waiting at most 3 s). It also stops yt-dlp. The app uses no CPU while idle.

---

## Architecture

```
  CLI / GUI
      |
      v
  DownloadEngine ---- media hosts ----> yt-dlp (finds the formats; process tree, cancellable)
      |  resolve (Drive, MediaFire, SourceForge, Archive.org, pages)
      |  probe mirrors concurrently, keep identical ones; the first answer is the first chunk
      |---- .m3u8 ---------------------> HlsEngine (sliding window, writer thread, AES-128)
      v
  ChunkManager (dynamic chunks, time-based work stealing, per-chunk retries)
      |                      \
      v                       v
  HTTP workers x N        MirrorRacer (per-mirror speed, throttling, failover)
      |  hosts (per-host connection budget and profile, shared by every download)
      v
  DiskWriter (positional writes into <name>.part, preallocation, hashed as written)
  DownloadState (<name>.part.hfstate, saved every 2 s and on stop)
```

The engine is the `hyperfetch-core` library crate. `hyperfetch-cli` and `hyperfetch-gui` are the front ends.

---

## Benchmarks

v1.0 against this version on a local server (`bench/`, the two binaries taking turns, median of
3 runs, every file checked by SHA-256, Windows 11, 32 threads). Batches use `-j 4`, the default,
unless marked `-j 1`; v1.0 always downloads one file at a time.

| Server caps each connection at 2 MiB/s, 40 ms per request | v1.0 | now |
|---|---:|---:|
| 256 MiB file, `-s 16` | 9.00 s | 8.74 s |
| 256 MiB file, HEAD without `Accept-Ranges` | 128.66 s (one connection) | 8.52 s |
| 256 MiB file, one connection stalls forever | hangs | 8.73 s |
| 50 small files (20 MiB), `-i` | 6.18 s | 1.72 s (`-j 1`: 7.02 s) |
| Same, every new connection takes 80 ms to set up | 14.32 s | 3.10 s (`-j 1`: 10.84 s) |
| Same as the plain batch, with 1000 downloads in the history | 6.48 s | 1.94 s (`-j 1`: 7.38 s) |

| No per-connection cap, 40 ms per request | v1.0 | now |
|---|---:|---:|
| 256 MiB file, `-s 16` | 0.62 s | 0.32 s |
| 50 small files (20 MiB), `-i` | 4.80 s | 0.66 s (`-j 1`: 2.31 s) |
| Same, every new connection takes 80 ms to set up | 13.32 s | 0.88 s (`-j 1`: 2.52 s) |

One at a time (`-j 1`), tiny files from a server that caps every connection are still slightly
slower than v1.0's fixed 16-way split: each file starts on the connection that probed it. BLAKE3
is computed while the file downloads, so large files finish without reading the whole file back.
Reproduce with `bench/README.md`.

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
