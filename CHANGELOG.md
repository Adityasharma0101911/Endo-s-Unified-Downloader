# Changelog

What changed in each release, newest first. Each release's page on [GitHub](https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases) has its files and the full notes.

## Unreleased

- Subtitles in WebVTT, TTML/IMSC and inside MP4 segments (`stpp`, `wvtt`) are read into one timeline, joined across segments and written as SRT or WebVTT, for stream downloads to keep.
- **DASH and stream tracks:** on-demand DASH manifests (`.mpd`) download natively, with resume, expired links renewed and several periods joined; an HLS master playlist's separate audio downloads natively too. A video spec (`1080p`, `hevc`, `hdr`, `30fps`, ...) and an audio spec (languages, `all`, `eac3`, `6ch`, ...) or track ids choose the tracks; several are joined by ffmpeg without re-encoding, each audio track tagged with its language.
- **Live HLS and DASH recorded natively:** a live playlist or dynamic DASH manifest is recorded by the engine itself, every chosen track side by side and written as it comes, from now, its oldest segment (`--live-from-start`) or a time ago or clock time (`--live-from`), until it ends, `--record-seconds`, no new segment for `--live-idle-timeout` seconds, or a stop, then joined and remuxed as a download is. Expired segments are skipped as gaps; the free-space guard and "keep what was recorded" rules apply as for other recordings. Media sites are still recorded by yt-dlp.
- **More for streams:** an HLS or DASH download takes its subtitles tracks in the subtitle languages (WebVTT, TTML, `wvtt`, `stpp`), joined into the file with their languages or saved as `<name>.<language>.srt`, or alone with no video and audio; `--sections` keeps the whole segments that overlap its ranges, in every track; parts and segments whose links match ad patterns (`*` for any run) can be left out; `--name-pattern` names a stream download from its height, codec, audio languages, channel, date and more. Subtitle files beside a stream are now `.srt`.
- **Channel lists and TV streams:** an extended M3U of channel links (a `.m3u` link or a local file) becomes one download per channel chosen by name or group patterns, named after it; an MPEG-TS link with no length is recorded like a live stream, until it ends, a time or size limit, idle time or a stop, keeping what it recorded.
- **Smooth Streaming:** `.ism/Manifest` links and `application/vnd.ms-sstr+xml` answers go to yt-dlp, a protected one refused first; the browser extension lists them like DASH manifests.

## 1.8.0 (2026-10-27)

- **Installers and packages:** a Windows setup.exe (also for winget), Linux x64 and arm64 `.tar.gz`, `.deb` and `.rpm`, and a static Linux CLI; the Windows programs are code-signed when the project's signing is set up. `install.sh` installs the release for the machine's architecture.
- **Browser downloads to the app** (off by default): files of chosen types and size go to the app instead of the browser, except from blocked sites, private windows or an Alt-click; the popup can start the app.
- **FTP** downloads with several connections and resume; `--max-filesize`; keep the server's file date (`--remote-time`); download only when the server has a newer copy (`-N`).
- **Checksums the site publishes** (in its headers or a `.sha256`/`SHA256SUMS` file beside the download) are checked by themselves. A finished file that fails its check later is repaired by fetching only its damaged pieces; a metalink's piece hashes re-fetch only the bad pieces. A file with no proper extension gets one from its contents, and sorting goes by it.
- **Request options:** custom headers, user agent, cookies, IPv4 or IPv6 only, connect timeout, no proxy, and (CLI only) skipping certificate checks; live recordings for a set time (`--record-seconds`); a connection count that adapts (off by default).
- **CLI:** `--json` results, `--spider`, `-S`, `--preview` (a ZIP's contents), `--dry-run`, `--stdout`, `-u`, `--cookie`, `--no-clobber`, `--logfile`, `--completions`.
- **App:** a tray icon and close to tray, start hidden and at login, a start/stop schedule, quit, sleep or shut down when the queue is done (30 s to cancel), one app at a time, taskbar progress on Windows, a log file and portable mode; sorting categories you can edit, each with its own folder; "See what's inside" for ZIP links; settings for site checksums, file dates, IP version, connect timeout and user agent.
- A fast mirror with the wrong file no longer decides the download when the first link is slow to answer.

## 1.7.0 (2026-10-05)

- **macOS:** the app, the CLI, the browser extension and self-updates run natively on Apple Silicon and Intel Macs (macOS 11 or later), each build tested on a real Mac of both kinds. A DMG per chip; the app is ad-hoc signed, not notarized. Data in `~/Library/Application Support/EndosUnifiedDownloader`; downloads are marked as coming from the internet for Gatekeeper; Reduce Motion and ⌘ shortcuts are followed; an update replaces the whole app in one step and rolls back on failure.
- Settings shows the browser extension's folder on every platform.

## 1.6.0 (2026-10-05)

- **Media sites in the extension:** a media card in the popup on YouTube, Twitch, TikTok, X, Vimeo, Reddit, Instagram, SoundCloud and more (quality up to 4K with HDR and 60 fps marked, audio only, MP4/MKV/WebM, subtitles, clips, SponsorBlock, whole playlists, live streams from the start or from now), a Download button on YouTube's own page, and an optional "Use my browser sign-in on media sites".
- CLI: `--sections`, `--sponsorblock remove|mark`, `--merge-format`; 1080p and 720p pick the right size on portrait videos.
- **More direct links:** WeTransfer, pCloud, Yandex Disk and Box shares; GitHub folders, repositories and release assets; Hugging Face folders; Civitai models.
- **Links with a password, key or token:** pasted with the link (`user:pass@`, `Password: …`, `Token: …`, `Cookie: …`, tokens next to their own sites' links, a MEGA key, a request copied from DevTools as cURL, fetch or PowerShell). Secrets go only to their own link's host and are never shown, logged or saved. CLI: `--password`.

## 1.5.0 (2026-10-05)

- **More places to download from:** BitTorrent from the swarm (seeding to a ratio or a time limit, through a SOCKS5 proxy when one is set), TorBox and Premiumize debrid, MEGA files and folders, Pixeldrain, Gofile, OneDrive/SharePoint, Terabox, and image galleries through gallery-dl.
- **Several networks at once:** a download's connections spread over Wi-Fi, Ethernet and a hotspot.
- **Mark of the Web** on downloaded files, so SmartScreen checks them.
- **Post-processing** (off by default): unpack archives, sort files into folders by type, look up a file's hash on VirusTotal, run a command after each download.
- **A new app and extension:** sidebar layout, a Settings page covering every option, richer queue rows, dark and light themes, animations that stop when idle, and Reduce motion; the extension gains Media, Links, Record and Settings tabs and better accessibility.

## 1.4.0 (2026-10-04)

- **Self-updater:** the app checks GitHub for a newer release at startup and offers Update and restart; `--update` on the command line. An update installs only when its `SHA256SUMS` carries a valid signature from the project's release key and names the version installed; the programs are swapped by renaming, put back on failure, and an update cut off midway is finished at the next start. The extension next to the app is updated and reloads itself.

## 1.3.0 (2026-10-04)

- **Browser extension:** finds HLS playlists, DASH manifests and media files on a page and sends them to the app with the page's headers and cookies; Via browser for sites that block outside downloaders; Capture from start and Record playback for blob or MSE video. Only the extension or a local program can use the app's local API.
- **HLS:** converted to MP4 without re-encoding, ad breaks skipped, WebVTT subtitles kept, expiring segment links refreshed, disguised segments unwrapped.
- **Lists, feeds and folders:** playlists and channels with a download archive, podcasts and RSS/Atom feeds, Google Drive and MediaFire folders.
- **Media:** subtitles, tags and chapters embedded; live streams recorded from their start; ffmpeg installed only once you agree; Anna's Archive links resolved.

## 1.2.0 (2026-09-27)

- **Links that just work:** short and wrapped links are followed to where they land; code-site "view file" links download the file; Google Docs, Sheets and Slides download as Office files; any site yt-dlp supports; a link that names a file but answers with a web page is an error, not a saved page.
- **Batches:** `.torrent`, `.metalink` and `.meta4` files in the GUI; the CLI and the GUI accept the same input; metalink folders kept.
- **Checksums:** SHA-512 and SHA-1, computed while downloading.
- **Privacy:** history and the saved queue keep links without their passwords, signatures and tokens; Authorization is never sent to listed mirrors.

## 1.1.0 (2026-09-27)

- **A rebuilt engine:** batches run 4 at a time over shared, reused connections with one per-host limit; faster starts; stalled connections replaced within seconds; BLAKE3, SHA-256 and MD5 computed while downloading; `--fsync`; parallel HLS segments and keys; parallel video and audio for yt-dlp links; `--repair` in parallel.
- **Reliability:** strict resume checks (ranges, ETag, Last-Modified, If-Range), partial files never deleted on a busy server, credentials sent only to their own hosts, a GUI queue that survives restarts.

## 1.0.0 (2026-09-24)

- First release: a multi-connection engine with work stealing, a native GUI with a live chunk view, a queue and history, safe resume and file-name collision handling, `--verify` and `--repair`, and a headless CLI with batch input and a systemd service for Linux.
