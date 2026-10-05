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

**Several networks**
- With "Use several networks" (GUI: Settings > Connections & networks) the connections of a download are spread over every network with internet access (wired, Wi-Fi, a phone), or over the ones you tick or the local addresses you type. The CLI takes `--all-networks`, or `--bind-address IP` once per address. With a proxy pool too, each proxy is combined with each address (at most 16 routes). A route that cannot connect is dropped for the rest of that download (keeping at least one), and each route has its own per-host connection budget. Only the connections that download ranges use them; the first request, HLS and yt-dlp go out the usual way.
- `--proxy-pool` (comma-separated) and `--proxies-file` (one per line) spread connections over several proxies the same way (GUI: Proxy pool).

**After the download**

In this order, for every download that finishes (not for a file found already downloaded):
- **Mark of the Web** (Windows, on by default): each file gets the `Zone.Identifier` stream browsers write (zone 3, internet, and the link without its secrets or its `#` part), so Windows asks before running a downloaded program. Extracted files get it too, and a torrent's or gallery's files as they arrive, so those of one stopped halfway have it. `--no-mark-of-the-web` (GUI: clear "Mark as downloaded from the internet") turns it off.
- **Unpack archives** (`--extract`): `.zip`, `.7z`, `.rar`, `.tar`, `.tar.gz`, `.tar.xz` and `.tar.bz2` are unpacked with the system's tar (bsdtar, built into Windows 10 and later) into a new folder next to the archive, named after it (numbered if taken). Every entry is checked first: one that would land outside that folder refuses the whole archive. A password-protected `.zip`, `.7z` or `.rar` opens with the password given with its link (see [Links with a password, key or token](#features)): a `.zip` with tar, a `.7z` or `.rar` with [7-Zip](https://7-zip.org) when it is installed; a wrong password leaves it packed, with a note. A part of a split archive (`.part2.rar`, `.7z.002`), a password-protected one without its password and one holding a symbolic link are left packed, with a note. On Linux `.zip`, `.7z` and `.rar` need bsdtar (package `libarchive-tools`); without it they are left packed, with a note. `--delete-archive` deletes the archive once it is unpacked.
- **Sort into folders** (`--sort`): a single file straight in the save folder moves into `Video`, `Music`, `Pictures`, `Documents`, `Archives` or `Programs` there by its extension (numbered if the name is taken), and the history follows it. Folders (torrents, galleries), the files of a playlist, cloud folder or torrent in a folder of their own, and a torrent's single file (it seeds from there) stay where they are.
- **VirusTotal** (`--virustotal-key KEY` / `VIRUSTOTAL_API_KEY`; GUI: "Look files up on VirusTotal" and a key): the file's SHA-256 is looked up, never the file itself, and a note says how many engines flag it, or that VirusTotal does not know it. The key is saved in the GUI settings only, never in the queue or history, and is never in a note or log.
- **Run a command** (`--exec CMD`; GUI: "Run after each download"): the command is split once into the program and its arguments (quotes group words) and run without a shell, so nothing in a file name can run anything else. `{path}`, `{dir}`, `{name}` and `{url}` in its arguments are replaced, and the same values are in `ENDO_PATH`, `ENDO_DIR`, `ENDO_NAME` and `ENDO_URL`. A bare program name is looked up on PATH alone (never in the app's folder). It is stopped after 10 minutes; a failure is a note. Only the settings and the command line set it, never the browser extension.

Notes (a refused archive, the VirusTotal verdict, a failed command) show under the download in the GUI and indented under its `[OK]` line in the CLI. GUI: Settings > Post-processing.

**Limits of these**
- BitTorrent: seeding stops when the app closes and does not start again with it; the chosen networks do not apply to it; behind a proxy it needs a SOCKS5 one (any other refuses swarm downloads) and then runs without DHT, local discovery, incoming connections and UDP trackers (peers and HTTP trackers go through the proxy); a changed listen port or UPnP setting takes effect once no torrent is running (clearing Download from peers stops BitTorrent at once); a magnet nobody shares waits until it is stopped; a `.torrent` link behind a login is not fetched with your cookies.
- MEGA: a finished MEGA file cannot be repaired with `--verify --repair` (verifying it works), and a MEGA folder added again brings all its files, not only the new ones.
- File shares: OneDrive and SharePoint folders, and Terabox shares of several files, are not supported; a Gofile folder lists its first 1000 items. Password-protected pCloud, Yandex Disk and Box shares and Box's `/v/` custom links are not supported, a cloud folder lists at most 200 subfolders, and a WeTransfer transfer comes as one zip of all its files.
- Media: a clip starts at the keyframe at or before the time asked (often a few seconds early), since cutting exactly would mean re-encoding; its progress may show no percentage while ffmpeg cuts it. "Whole playlist" from the browser lists the playlist with the cookies set in the app, not the browser's (those still go with each video).
- Repositories: private or gated repositories and models cannot be listed (the listing is read without your Authorization header).
- After the download: VirusTotal looks up single files, not the files of a torrent or gallery folder; extracting leaves the history pointing at the archive.

**Resilient transfers**
- Stall detection: a connection that receives no data for `--stall-timeout` seconds (default 30) is dropped and retried. A chunk's connection that goes quiet mid-body is replaced after 5 s (or the stall timeout, if shorter), keeping what it received.
- Retries with exponential backoff and jitter. HTTP 429/503 responses respect `Retry-After` and reduce the number of connections. Mirrors that keep failing are disabled. An attempt that made progress does not count against `--max-retries`.
- Servers without range support or without a known length are downloaded as a single stream, which goes on with the probe's answer instead of asking again.

**Many kinds of input**
- **Link resolvers:** Google Drive (large-file confirmation), MediaFire, Dropbox share links, SourceForge (several mirrors), Archive.org (all replica servers), and web pages with an embedded video (`<video>`, `og:video`). Whether a Google Drive link, or a URL that may be a web page (no extension, or a page one), answers with a page is read from the probe's own answer, not from a request of its own, and an Archive.org item is asked only for the three metadata fields that name its servers, once for all its files.
- **HLS (`.m3u8`):** master and media playlists, AES-128 encrypted segments, `#EXT-X-MAP` init sections, output as `.ts` or `.mp4`, and resumable. With `--hls-mp4` (GUI: "Convert HLS streams to MP4", on by default) an MPEG-TS stream is remuxed into an `.mp4` of the same name once downloaded (numbered if that name is taken; no re-encoding), and the `.ts` is deleted once that succeeds; without ffmpeg (installed only as for media), or if the remux fails or is stopped, the `.ts` is kept and the log says why. Segments come over several connections in a sliding window, so a slow segment does not hold up the others (it is requested a second time once it lags), and are written by a writer of their own, which saves the resume state every 2 s. AES keys are fetched in parallel, adjoining byte ranges of one file are fetched together, and `--stall-timeout` and `--max-retries` apply. A live playlist, and a master playlist whose audio is only in a separate `#EXT-X-MEDIA` rendition (the segments alone would be silent), is handed to yt-dlp instead (see [Live streams](#live-streams)), and so is a DASH manifest (`.mpd`, or one served as `application/dash+xml`). Ad breaks are skipped (`#EXT-X-CUE-OUT` up to `CUE-IN`, or until the duration the break gives has passed, and `#EXT-X-DATERANGE`s whose class is an ad), the MP4 keeps timestamps running across `#EXT-X-DISCONTINUITY`s, its subtitles moved along with their parts (parts ffmpeg cannot join that way, such as an audio-only bumper, are remuxed in one piece instead), WebVTT subtitle renditions become tracks of the MP4 (else `<name>.<lang>.vtt` files), and a stream without video is converted to `.m4a` instead. When segment links expire (401, 403, 404 or 410) the playlist is read again for fresh ones, up to 3 times, and a server that refuses HTTP/1.1 is tried again over HTTP/2. DRM is not supported.
- **Media sites:** YouTube, Twitch, TikTok, Twitter/X, Vimeo, Reddit, Instagram, Facebook and Dailymotion are handed to [yt-dlp](https://github.com/yt-dlp/yt-dlp) automatically. yt-dlp finds the formats, and when they are plain files or HLS playlists the engine downloads the video and audio together over its own connections, then ffmpeg joins them without re-encoding; anything else, or a failure, falls back to yt-dlp's own download, which reuses that extraction. If yt-dlp is not installed, a managed copy is downloaded and checked against its published SHA-256 sums (on Windows the unpacked build, which starts faster). Its version is cached on disk, and browser cookies are read once and reused for 15 minutes. ffmpeg is needed to merge separate video and audio streams (see below). Use `--media-preset` to choose the quality (`m4a` picks an AAC source, so the audio is copied, not re-encoded) and `--cookies-from-browser` for sites that require a login. Subtitles, tags and chapters, clips, SponsorBlock and the container, and live recordings are described under [Subtitles and tags](#subtitles-and-tags), [Clips, SponsorBlock and formats](#clips-sponsorblock-and-formats) and [Live streams](#live-streams).
- **ffmpeg:** merging separate video and audio (the best quality), the MP3/M4A presets, remuxing and embedding need ffmpeg. It is looked for next to the program, in the managed folder (`%LOCALAPPDATA%\EndosUnifiedDownloader\bin\ffmpeg-build` on Windows, `~/.local/share/EndosUnifiedDownloader/bin/ffmpeg-build` on Linux, `~/Library/Application Support/EndosUnifiedDownloader/bin/ffmpeg-build` on macOS), then on PATH; on Linux the system's ffmpeg comes before the managed one. When a media download finds none, it is installed only once you agree: the GUI asks before the first video that needs it starts (the answer is kept as "Install ffmpeg when a video needs it" in Settings > Media), and the command line asks when it runs in the foreground at a terminal, while the downloads that do not need ffmpeg go ahead: only those that do wait for the answer (`--install-ffmpeg` agrees and `--no-install-ffmpeg` declines without asking; a run nobody can answer, such as one reading a pipe, a background job or the service, does not install it, and its log says to pass `--install-ffmpeg`). What is installed is the GPL build the yt-dlp project publishes ([yt-dlp/FFmpeg-Builds](https://github.com/yt-dlp/FFmpeg-Builds), win64 or linux64, and the arm64 builds) is installed there: about 200 MB is downloaded from the release tagged `latest`, checked against its `checksums.sha256` before it is unpacked, and only `ffmpeg` and `ffprobe` are kept (about 330 MB on disk). It is downloaded only once tar (and on Linux xz) is found to run, and a download that receives nothing for 30 s fails; a slow one goes on. The install appears complete in one rename, so a failed one leaves no half-written ffmpeg, and the download then goes on without it (a lower-quality format that needs no merging; an audio preset fails and says why). A failed install is not tried again for 15 minutes, except while FFmpeg-Builds replaces the release (daily, for some minutes), when the next video tries again. An install missing `ffmpeg` or `ffprobe` is installed again, and what an interrupted install left in the folder is deleted by the next video an hour later. On macOS it is instead the static release build of [Martin Riedl](https://ffmpeg.martin-riedl.de) for the Mac's chip: `ffmpeg.zip` and `ffprobe.zip` (about 70 MB together), each checked against its `.sha256` before it is unpacked; Homebrew's ffmpeg on PATH works too. These ffmpeg builds are licensed under the GPL (version 3); the app runs ffmpeg as a separate program and does not link to it.
- **BitTorrent:** a magnet link, or a `.torrent` (a link or a local file) without web seeds, downloads from the swarm (with [librqbit](https://github.com/ikatson/rqbit), built in; rustls, no OpenSSL). DHT is on (its state is kept in the app's data folder), peers named in the magnet (`x.pe`) are used, and the speed limit applies. A multi-file torrent is saved in a folder named after it, a single-file one as its file; when that name is taken by something the torrent did not write, it goes into a numbered folder (`name (1)`) instead, never over it (`-o` cannot rename a torrent's files). Every name the torrent gives is checked before anything is written: one that would land outside that folder (`..`, a drive, on Windows any `:`) refuses the whole torrent. Progress shows the peers, what was uploaded and the upload speed. Stopping keeps the pieces; running it again checks them and goes on. A finished torrent seeds until it has uploaded the seed ratio times its size (GUI 1.0, CLI `--seed-ratio`, 0 = no seeding) or for the seed time (GUI 60 minutes, CLI `--seed-time`), whichever comes first; the files are kept. GUI: Settings > BitTorrent (Download from peers, Seed ratio, Seed time limit, Listen port, Port forwarding (UPnP)); `--no-p2p` turns the swarm off.
- **Magnet links** with HTTP web seeds (`ws=`), and **`.torrent`** files with web seeds (`url-list`), are downloaded from the web seeds instead: every file of a multi-file torrent becomes a separate download. With the swarm off, a `.torrent` URL without web seeds downloads the `.torrent` file itself, for a torrent client. Padding files (BEP 47) are skipped, and a BitTorrent v2-only torrent is an error. A `.torrent`, `.metalink` or `.meta4` link is read whatever type its server labels it (PHP labels everything a web page), and one to a GitHub or Hugging Face file page or a Dropbox share from the file itself; one whose host refuses it (a login, or GitHub's "not found" for a private repository) or answers with a web page is downloaded as the file it is, from the link you gave, with your cookies and Authorization header, and a web page in its place is an error. A host that is busy (a timeout, a rate limit) or fails is an error, to try again. A checksum you give is for the file a document lists, so a document downloaded itself is not checked against it.
- **Debrid services:** with an API key for Real-Debrid, AllDebrid, TorBox or Premiumize (`--debrid-key` / `ENDO_DEBRID_KEY` and `--debrid-provider`; GUI: Settings > Debrid & hosts), a link to a file host the service supports (its own list, fetched once per run, else a built-in one) is unlocked into a direct link first, and a magnet link goes through the service (GUI: Magnets through debrid, on by default): it is added to your account, every file is selected, and each file becomes a download with its name and size. A cached torrent starts at once; one that is not finished within 60 seconds is an error that says how far the service got (it goes on in your account; try again later). The key is sent in an `Authorization` header wherever the service allows (Premiumize and one TorBox call take it only as a parameter) and never appears in errors, logs, the history or the saved queue. When debrid fails on a link only it can download, the error is the service's own message; a magnet it fails on goes on from its web seeds or the swarm when those can take it, with that message as a note.
- **MEGA:** `mega.nz/file/...#key`, `mega.nz/folder/...#key` (a file or folder inside it too) and the older `#!id!key` and `#F!id!key` links. A file downloads over several connections, each part decrypted (AES-128-CTR) as it is written, so resume and checksums work on the real bytes, and the file's MAC is checked at the end: a mismatch deletes it. A folder link adds one download per file, in a folder named after it, subfolders kept. When MEGA's free transfer quota is used up, the error says so; a debrid service can take MEGA links instead.
- **File shares:** Pixeldrain (`/u/` files, and `/l/` lists, one download per file with its SHA-256), Gofile folders (`gofile.io/d/...`, through a guest account, subfolders kept, with MD5 checksums; a password-protected folder opens with the password given with its link, and is an error without it or with a wrong one), OneDrive and SharePoint shared file links (`1drv.ms`, `onedrive.live.com`, `*.sharepoint.com/:x:/...`; a shared folder is an error, since it cannot be listed without signing in) and Terabox single-file shares (all its domains; these need your Terabox cookies: export a `cookies.txt` and pass `--load-cookies`, GUI: Cookies file). Pixeldrain gets at most 4 connections, the most it allows.
- **Cloud storage:** WeTransfer transfers (`we.tl/...`, `wetransfer.com/downloads/...`; an expired or deleted one is an error that says so), pCloud public links (`u.pcloud.link`, `e.pcloud.link` and `pcloud.com` `publink/show?code=...`, and the web app's `#page=publink&code=...`; a file over every server pCloud names for it), Yandex Disk public links (`disk.yandex.*/d/...` and `/i/...`, `yadi.sk`, a file or folder inside a shared folder too; files come with their SHA-256) and Box shared links (`*.box.com/s/...`, a file or folder inside it too). A shared folder adds one download per file, subfolders kept; Box files the owner does not let you download are skipped. A Yandex Disk video its API does not hand over, and a Box page without share data, go to yt-dlp as before.
- **Image galleries:** imgur, Pixiv, DeviantArt, ArtStation, Flickr, Tumblr, Pinterest, Danbooru, Gelbooru, e621, Bluesky and Reddit `/gallery/` links go to [gallery-dl](https://github.com/mikf/gallery-dl), into a folder named after the gallery; an X/Twitter or Reddit post where yt-dlp finds no video goes there too. gallery-dl is taken from PATH (never from next to the app, which may be the Downloads folder), else installed into the managed folder from its latest Codeberg release, checked against that release's `SHA256SUMS` (Windows, and Linux x86_64; elsewhere install it with `pip install gallery-dl`, or on macOS `brew install gallery-dl` or `pipx install gallery-dl`). It is not updated afterwards; where none can run, the link is downloaded as it would be without it. Progress shows the bytes of the files so far; a gallery-dl that stops partway fails the download, keeping the files it got for the next run. `--no-gallery-dl` (GUI: clear "Image galleries with gallery-dl") leaves these links to the rest of the app.
- **Metalink** (`.metalink`, `.meta4`): mirrors are ordered by priority, and the file name and checksum (the strongest well-formed one of SHA-512, SHA-256, SHA-1 and MD5 it gives) are taken from the metalink. A name with folders (`dir/file.iso`) is saved in those folders; `..` and absolute names are refused. Versions up to 1.1.0 dropped the folders and cleaned names a little differently, so a metalink or torrent download paused by one of them may start again from zero under its new path.

**Links that just work**

A pasted link gives the file or the media it stands for, judged by where it lands, not by the page it may answer with. The same rules apply in the command line and the GUI.
- **Short and wrapped links:** a link from a shortener (bit.ly, TinyURL, git.io, ...) or a mail link scanner (Outlook Safe Links, Proofpoint, Mimecast) is downloaded from where it lands, through that host's resolver or yt-dlp. "You are leaving this site" links (`youtube.com/redirect`, `google.com/url` and Google's country sites, `l.facebook.com`, `l.instagram.com`, Steam's link filter, `out.reddit.com`, LinkedIn, VK, DuckDuckGo) are replaced by the target they hold, without a request, so the queue and history keep the target. A shortener or scanner that shows a page of its own (a preview, a warning) is an error that asks you to open the link in your browser; nothing on it is clicked.
- **t.co:** X's links, and other pages that send the browser on at once (a 0-second refresh), are followed to their target. At most three links are followed past the one given, and never back to one already tried.
- **Code-site file pages:** a "view file" link on GitHub (`/blob/`, a renamed branch included), GitLab (`/-/blob/`), Bitbucket (`/src/`) or Hugging Face (`/blob/`, and `/raw/`, which on its own gives only the pointer of a large file) downloads the file itself. A GitHub or Hugging Face folder is listed (see the next point); a GitLab or Bitbucket folder link is an error.
- **Code and model repositories:** a GitHub repository link (`github.com/owner/repo`) downloads its default branch as a zip; a folder (`/tree/<branch>/<path>`, a branch with slashes too) adds one download per file, subfolders kept; a release (`/releases/latest`, `/releases/tag/<tag>`) one per asset (with GitHub's SHA-256 where it gives one), in a folder named after the repository and tag. A Hugging Face model, dataset or Space (`huggingface.co/owner/name`, `/datasets/...`, `/spaces/...`, and `/tree/<revision>/<path>` in any of them) adds one download per file, large files with their SHA-256. A Civitai model (`civitai.com/models/<id>`) adds the files of its newest version, or of the one `?modelVersionId=` names. The Authorization header (`--header`, GUI: Advanced options) goes only to the service's own host (github.com, huggingface.co, civitai.com), never to a file elsewhere. A token pasted with the link (`ghp_...`, `hf_...`, or `Token: ...`, see Links with a password, key or token) is sent with the listing to that service's API too, so private and gated repositories can be listed. GitHub allows 60 listings an hour without signing in; past that the error says when to try again.
- **Cloud folders:** a Google Drive folder link (`drive.google.com/drive/folders/...`, `/drive/u/0/folders/...`, the mobile app's `/drive/mobile/folders/...`, and older `folderview?id=` and `open?id=` links that name a folder) or a MediaFire one (`mediafire.com/folder/...`, and an old `mediafire.com/?key` link that names a folder) adds one download per file, in a folder named after the one shared, subfolders kept. With a Google API key (`--google-api-key`, `ENDO_GOOGLE_API_KEY`, or the GUI's Google API Key setting, which is not saved) a Drive folder is listed through the Drive API, with each file's size and MD5 checksum, and the key is sent to the Drive API alone; without one, from the folder's public page, which gives no sizes or checksums and may not show every file of a very large folder. Shortcuts are followed, a file reached twice is downloaded once, and Docs, Sheets and Slides download as their export (see below). MediaFire's folder API gives each file's size and SHA-256, which is checked; a file MediaFire flags as malware is an error that says so. Forms, drawings, files behind a MediaFire password and subfolders that are private or deleted are left out, and the CLI warns of them (the GUI shows them under the header); a private or deleted folder is an error. A request Drive or MediaFire answers busy (a rate limit, a server error) or that cannot reach it is made again, 4 times in all, waiting 1, 2 and 4 seconds or as long as the host asks (at most 30); a subfolder still unread then is missing, and the listing has failed in part: what was read is added, the CLI reports the folder as failed and exits non-zero, the GUI shows it as an error. Add the folder again later for the rest. Drive's download quota is reported, never worked around. Every file that finishes is noted in the download archive by its id and, where the listing tells it, its content (with an API key, a Drive file's MD5; MediaFire's SHA-256), and a folder added again brings only the files not downloaded before and those whose content changed since (GUI: Only new items; `--all-items` brings them all); one with nothing new is nothing to do. The public page of a Drive folder tells no MD5, so a Drive file noted by any listing, with a key or without, counts as downloaded; it is new again only when the listing tells its MD5 and the archive notes it with another one alone. A Google Docs, Sheets or Slides document has no MD5 and is known by its id alone: an edited document is not downloaded again by a folder added again, only with `--all-items` (GUI: clear Only new items), which saves it next to the old one (`Plan (1).docx`).
- **Google Docs, Sheets and Slides:** an editor or share link downloads the document as `.docx`, `.xlsx` (every sheet, whichever one the link opens on) or `.pptx`. An export link keeps the format it asks for, such as one sheet's `.csv` (`export?format=csv&gid=...`). A private document is an error that says how to share it, or to pass your browser's cookies with `--load-cookies`.
- **Any site yt-dlp supports:** a web page that plays no video of its own is offered to yt-dlp's site extractors (never its generic one), which fail at once for a site they do not know. When one takes the page, its media is downloaded instead of the page, in the GUI's media quality. So are SharePoint video pages, and the Box and Yandex Disk shares the app cannot download itself (see Cloud storage under [Features](#features)); one that yt-dlp cannot download is an error. Any other page whose site finds no media there (a news article without a video) is downloaded as the page, and one whose site fails at it (an HTTP error, a private, removed or geo-blocked video, a login) is an error. A DRM-protected video is an error, never a download.
- **Playlists and channels:** a YouTube playlist or channel (`/@name`, `/channel/`, `/c/`, `/user/`, or its videos, shorts or live tab), a SoundCloud user (their own tracks, not their reposts) or set, a Bandcamp artist or album, a Vimeo showcase, album, channel, group or user's videos, and a Twitch channel's videos, clips or collection become one download per video, in the list's order, in a folder named after the list. Each file is named `Title [id].ext`, as yt-dlp names files by default, so entries of one title (an album's "Intro", a streamer's daily VODs) do not overwrite or stand in for each other. They run as many at a time as the queue allows, each with its own progress, retry and history, and listings run one at a time, so no site is paged by several yt-dlp runs at once. A channel's tabs and the albums on an artist's page are listed in their place, 50 albums to a yt-dlp run; streams that are live or still to come are left out until they are videos. A video link that names its playlist too (`watch?v=...&list=...`) is the one video: the GUI asks at once whether to add it or the whole playlist (and says how many once it has read the playlist), the command line takes `--yes-playlist`. Every media download that finishes is noted in a download archive (`download-archive.txt` in the `EndosUnifiedDownloader` data folder, with the managed yt-dlp, in yt-dlp's `--download-archive` format), and a playlist or channel added again brings only what is new (GUI: Only new items; `--all-items` brings everything). A file that was there already is noted only when its name holds the video's id: one named by title alone may be another video's. A list with nothing new in it is nothing to do: it is said so, and the CLI (or the service) does not count it as failed. The newest N items (GUI, or `--latest N`) are a channel's first and a playlist's last (a channel's "Play all" list, `list=UU...`, is newest first); Twitch clips and lists sorted by views have no newest, so they are listed whole. yt-dlp is never given the archive, so a video you ask for again is downloaded again.
- **Podcasts and feeds:** an RSS or Atom feed link (Simplecast, Megaphone, Libsyn, Buzzsprout, Art19, Acast, Omny, Transistor, Podbean, Spreaker, SoundCloud and Anchor feeds, a private Patreon or Supercast feed, or any link ending in `.rss`, `.xml`, `/rss` or `/feed`) downloads every episode, newest first, as `YYYY-MM-DD Title.mp3` in a folder named after the show (always with an audio or video extension: `.mp3` or `.mp4` when the feed names none). UTF-16 feeds are read too, and ISO-8859-1 (windows-1252) ones when their XML declaration or Content-Type says so; in any other feed a byte that is not UTF-8 becomes a replacement character, and the rest of its letters stay. The newest N episodes (GUI, or `--latest N`) are its latest, and giving the feed again downloads only the episodes not downloaded before, from the same link or into the same file in the show's folder, so a host that changes its tracking links does not bring the back catalogue again (GUI: Only new items; `--all-items` downloads them all). An episode that finishes is noted in the download archive by its link (unless the link holds a secret) and by its file together with a digest of the feed's link (without the secrets of its query, so a new token there is the same feed; the link itself, which may hold a private feed's token in its path, is not written), so it is known however long ago it was downloaded, and another show of the same title is not taken for it; the history, which keeps its newest 1000 downloads, is asked too. A feed with nothing new in it is nothing to do, as a playlist's is. An Apple Podcasts show link downloads its public feed, found through Apple's lookup API, and an episode link (`?i=`) that episode. When Apple does not publish a show's feed (a subscription show, or one its publisher hid), a show link downloads the newest episodes the lookup API gives a public file for, or is an error that says it is not available, and an episode link downloads the file the lookup API gives it; an episode it does not list goes to yt-dlp. A private feed's token stays with the feed: it is never sent to the hosts of the episodes, nor is your Authorization header. A link that looks like a feed but is a page, another file or a feed without audio or video is downloaded as it is, and so is a link other files share the shape of (an `.xml` file, an `/rss` path on a host that is no known podcast host) whose host is busy or unreachable, with the usual retries; a busy feed host (`feeds.`, `rss.`, a `.rss` link) is an error, to try again.
- **Documents in the GUI:** `.metalink`, `.meta4` and `.torrent` files and links add one download per file they list (see the GUI section).
- **Pages are refused, not saved:** a link that names a file (`.zip`, `.exe`, `.iso`, `.pdf`, ...) and answers with a web page (a login, an expired link, a download page) is an error, as is a Google Drive page (private, deleted or over quota) and the share page of a service not supported yet (iCloud, pages of WeTransfer, pCloud, Yandex Disk and Box other than the links under Cloud storage, and SharePoint pages other than shared files). A file whose server labels it a web page still downloads when its first bytes show it is a file.
- **Links with a password, key or token:** paste a link with what it needs (in the Add form, the queue input, a dropped `.txt`, the clipboard banner, Ctrl+V or the command line) and that link gets it. `https://user:pass@host/...` becomes a Basic sign-in. A line or tail `Password: x` (also `pass`, `pw`, `pwd`, `passwd`, `parola`, `contraseña`, `mot de passe`), `Key: x`, `Token: x`, `API key: x`, `Bearer x`, `Authorization: ...` or `Cookie: ...` belongs to the link on its line or above it, and to every link when it comes before the first one or after the last (the password of a multi-part archive). A MEGA key after its link (`#key` on the next line) is joined to it. A GitHub (`ghp_`, `gho_`, `github_pat_`), Hugging Face (`hf_`) or GitLab (`glpat-`) token is taken only next to a link of its own service; any other unlabelled text is no secret. A browser's Copy as cURL (bash or cmd), Copy as fetch or Copy as PowerShell, and a wget command, are read, never run: their link, headers, cookies, user name and Referer make one download each; one that sends data (a POST) is refused. The password opens a Gofile folder, a video yt-dlp asks one for (`--video-password`) and a `.zip`, `.7z` or `.rar` that `--extract` unpacks; a `user:pass@` sign-in on a media site is yt-dlp's user name and password, which it gets in a temporary file, never on its command line. What a paste gives goes only to its link's host: never to a mirror on another host, a redirect to another host, a debrid service or VirusTotal. What you type wins over it (GUI: the Authorization header under Advanced options; CLI: `--header` and `--password`). It is never shown, logged or saved: a note names what a download uses ("Using the password from your paste for host"), the link is shown without it, and the Add form clears once the downloads are added. A download still unfinished when the app closes comes back without it: add the link again with it (one with a pasted sign-in waits for its Authorization header).
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

### macOS

macOS 11 (Big Sur) or later, on Apple Silicon (M1 and later) or Intel.

- **Download:** from the [releases page](https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases), `Endos-Unified-Downloader-vX.Y.Z-macos-apple-silicon.dmg` for an Apple Silicon Mac or `...-macos-intel.dmg` for an Intel one (Apple menu > About This Mac names the chip). Open it and drag Endo's Unified Downloader onto Applications. Run it from Applications, not from the disk image, so that it can update itself.
- **First launch:** the app is ad-hoc signed, not notarized by Apple (there is no paid developer account behind it), so the first time macOS says it cannot check it. Choose Done, open System Settings > Privacy & Security, click Open Anyway next to the message about the app, and confirm. Or run `xattr -dr com.apple.quarantine "/Applications/Endo's Unified Downloader.app"` once in Terminal. Later updates need neither: the app checks their signature itself (see [Updates](#updates)).
- **Data:** settings, history, the queue, the managed tools (yt-dlp, ffmpeg) and the extension are in `~/Library/Application Support/EndosUnifiedDownloader`.
- **Command line:** the CLI is inside the app, at `"/Applications/Endo's Unified Downloader.app/Contents/MacOS/Endos-Unified-Downloader-CLI"`. To call it `endos-downloader`: `sudo mkdir -p /usr/local/bin && sudo ln -sf "/Applications/Endo's Unified Downloader.app/Contents/MacOS/Endos-Unified-Downloader-CLI" /usr/local/bin/endos-downloader`.
- **Browser extension:** the app copies it to the `extension` folder of its data folder, which Settings > Browser extension shows with an Open button; load that folder as described under [Browser extension](#browser-extension).
- **Tools:** the app installs its own ffmpeg and yt-dlp when a video needs them (its ffmpeg needs macOS 12 or later: on macOS 11, `brew install ffmpeg`). Homebrew's are found too, also when the app is opened from Finder (it looks in `/opt/homebrew/bin`, `/usr/local/bin` and `~/.local/bin`), and gallery-dl comes only from there (`brew install gallery-dl` or `pipx install gallery-dl`; a plain `pip install` puts it where the app does not look); Homebrew's ffmpeg is optional. Shortcuts use ⌘ where Windows uses Ctrl.
- **Build it yourself:** with Rust 1.89 or newer and the Xcode command line tools (`xcode-select --install`), `cargo build --release --locked`, then `bash packaging/macos/bundle.sh X.Y.Z` makes the app, the DMG and the updater's archive in `target/macos`.

---

## Command line

```
Endos-Unified-Downloader-CLI [OPTIONS] [URLS]...
```

All `URLS` given on the command line are **mirrors of one file**, except a link that lists many given on its own: a folder, playlist, channel or feed link (see [Links that just work](#features)) becomes one download per file, video or episode, as a `.metalink` does. To download several files, use `-i`. A paste is the exception (see [Links with a password, key or token](#features)): a link with its password, key or token, a `user:pass@` link or a copied curl, wget, fetch or PowerShell command (quote it) makes one download per link, with what the paste gave it; `-` as the only URL reads the paste from stdin, and `-i` takes a paste as well as a list. Without URLs and without `-i`, an interactive prompt starts. The prompt uses the same options as the command line.

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
| `--header HEADER` | Authorization header, as `"Authorization: Bearer TOKEN"` or `"Bearer TOKEN"`. Other header names are rejected. It is never sent to the mirrors a `.metalink` or `.torrent` lists, nor to the files, episodes and videos a folder, feed, playlist or channel lists, except a repository's files on that service's own host (see Code and model repositories under [Features](#features)). | |
| `--password PASS` | Password of a share (a Gofile folder), a video (yt-dlp's `--video-password`) or an archive `--extract` unpacks, for every download of the run. It wins over a password in a paste, and is never shown or saved. | |
| `--load-cookies FILE` | Netscape `cookies.txt` file. | |
| `--proxy URL` | `http://`, `https://`, `socks5://` or `socks5h://` proxy. | |
| `--media-preset PRESET` | `best`, `1080p`, `720p`, `mp3`, `m4a`, or any yt-dlp format selector. With a preset, page URLs that are not direct files are also sent to yt-dlp. | `best` |
| `--cookies-from-browser B` | `chrome`, `edge`, `firefox`, `brave`, `opera` or `vivaldi` (media downloads). | |
| `--concurrent-fragments N` | Connections for yt-dlp media downloads (1-32). | `8` |
| `--subs LANGS` | Subtitle languages to save next to a video, e.g. `en,es` or `all`, as `.srt` when ffmpeg is present (see [Subtitles and tags](#subtitles-and-tags)). | none |
| `--sections START-END` | Download only this part of a video, e.g. `90-150`, `1:30-2:30` or `1:02:03-1:05:00`; repeat it for several parts (see [Clips, SponsorBlock and formats](#clips-sponsorblock-and-formats)). | the whole video |
| `--sponsorblock MODE` | `remove` cuts YouTube's sponsor, self-promotion and interaction segments out; `mark` marks every SponsorBlock segment as a chapter. | off |
| `--merge-format F` | Container a video's streams are joined in: `mp4`, `mkv` or `webm`. | `mp4` |
| `--no-embed-metadata` | Leave out the tags, chapters and cover art written into media files. | embedded |
| `--live-from-start` | Record a live stream from its start, where the site keeps it, instead of from now (see [Live streams](#live-streams)). | from now |
| `--wait-for-video` | Wait for a scheduled stream or premiere to begin instead of failing. | off |
| `--hls-mp4` | Remux an HLS stream saved as MPEG-TS into an `.mp4` once downloaded, without re-encoding (see HLS under [Features](#features)). | keeps the `.ts` |
| `--install-ffmpeg` | Install ffmpeg (about 200 MB) when a media download needs it and finds none (see ffmpeg under [Features](#features)). | asks at a terminal |
| `--no-install-ffmpeg` | Never install ffmpeg, and do not ask. | |
| `--latest N` | Only the newest N items of a channel, playlist or feed: a channel's first, a playlist's last, a feed's latest episodes. | all |
| `--all-items` | Download every item of a channel, playlist, feed or cloud folder, also those downloaded before (in the download archive, or for a feed's episodes also in the history). | only new items |
| `--yes-playlist` | For a video link that also names its playlist (`watch?v=...&list=...`), download the whole playlist. | the one video |
| `--google-api-key KEY` | Google API key (or `ENDO_GOOGLE_API_KEY`) that lists a whole Google Drive folder through the Drive API, with sizes and checksums, and is sent nowhere else. Without it the folder's public page is read. | |
| `--debrid-key KEY` | Debrid API key (or `ENDO_DEBRID_KEY`): file host links are unlocked and magnets downloaded through the service (see Debrid under [Features](#features)). | |
| `--debrid-provider P` | `realdebrid`, `alldebrid`, `torbox` or `premiumize`. | Real-Debrid, then AllDebrid |
| `--proxy-pool LIST` / `--proxies-file FILE` | Several proxies (comma-separated, or one per line), connections taking turns over them. | |
| `--bind-address IP` | Local address to connect from; repeat it to spread connections over several networks. | the system's choice |
| `--all-networks` | Spread connections over every network with internet access. | off |
| `--no-p2p` | Never download magnets and `.torrent` files from the BitTorrent swarm. | swarm on |
| `--seed-ratio R` | Seed a finished torrent until it has uploaded R times its size; the program waits for that before it exits (Ctrl+C stops it). | `0`, no seeding |
| `--seed-time MINUTES` | Seed for at most this long; alone, seed that long whatever the ratio. | |
| `--bt-port PORT` / `--upnp` | BitTorrent listen port, and asking the router to forward it. | librqbit's range, no UPnP |
| `--no-gallery-dl` | Do not send image galleries to gallery-dl. | gallery-dl on |
| `--extract` / `--delete-archive` | Unpack downloaded archives; delete each once unpacked (see After the download under [Features](#features)). | off |
| `--sort` | Move each downloaded file into a category folder. | off |
| `--exec CMD` | Run a command after each download, without a shell; `{path}`, `{dir}`, `{name}`, `{url}` are replaced. | |
| `--no-mark-of-the-web` | Do not mark downloaded files as from the internet (Windows). | marked |
| `--virustotal-key KEY` | Look each downloaded file's SHA-256 up on VirusTotal (or `VIRUSTOTAL_API_KEY`). | |
| `--history` | List past downloads, including failed and stopped ones, then exit. | |
| `--verify FILE` | Check whether a downloaded or partial file is complete and intact. | |
| `--repair` | Used with `--verify`: download the missing ranges again. | |
| `-q, --quiet` | Hide progress output. Errors and warnings are still printed. | |
| `-v, --verbose` | More log output: `-v` info, `-vv` debug, `-vvv` trace. | warnings |
| `-h, --help` / `-V, --version` | Show help or the version. | |

**Batch files.** Each line is one download. Mirrors of the same file go on one line, separated by spaces. Blank lines and lines starting with `#` are ignored. A `.metalink`, `.meta4` or `.torrent` entry (a local path or a URL), or a folder, playlist, channel or feed link, must be on a line of its own and can expand into several downloads. The file may be UTF-8, with or without a BOM, or UTF-16 with a BOM (the default for Windows PowerShell 5 `>` redirects).

```text
# queue.txt
https://example.com/ubuntu.iso https://mirror.example.org/ubuntu.iso
magnet:?xt=urn:btih:...&dn=file.bin&ws=https://seed.example.com/files/
https://example.com/release.meta4
/home/me/debian.torrent
```

**Results.** Each finished download prints `[OK] <path>`, and a failed one `[FAILED] <name>: <reason>` on stderr. With `-j` above 1 results arrive in the order the downloads finish, so each line names its input: the line of the `-i` file that listed it, and its first URL without the user name, password and query string, which may hold credentials: `[OK] line 3 https://example.com/file.iso -> /srv/downloads/file.iso`. The files of one metalink or torrent share its line.

**Progress.** Each running download shows a bar with the engine's measured speed, the number of open connections and the ETA. If no data arrives for 5 seconds, the bar shows `STALLED`. Batches also show a total bar. When stderr is not a terminal (journald, cron, pipes), a plain progress line is printed every 10 seconds instead of the bars.

**Stopping.** Press Ctrl+C once, or send SIGTERM, to stop all downloads, save their resume state (within at most 10 seconds) and skip the rest of the batch. A live recording is finished and kept instead, however long that takes (see [Live streams](#live-streams)). Press Ctrl+C a second time to quit immediately.

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

# A link and its password, as copied from a forum post; a request copied from the browser's DevTools
endos-downloader "https://h.example/pack.7z Password: s3cret" --extract
xclip -o | endos-downloader - -d ~/Downloads

# Media: 720p video, cookies from Firefox
endos-downloader "https://www.youtube.com/watch?v=..." --media-preset 720p --cookies-from-browser firefox

# A video with its English subtitles; a live stream recorded from its start (Ctrl+C ends it)
endos-downloader "https://www.youtube.com/watch?v=..." --subs en
endos-downloader "https://www.youtube.com/watch?v=..." --live-from-start

# Every file of a shared Google Drive folder, subfolders kept
endos-downloader "https://drive.google.com/drive/folders/..." -d ~/Downloads

# The newest 5 videos of a channel, and a podcast's latest episode; run again, only new ones come
endos-downloader "https://www.youtube.com/@NASA" --latest 5 -d ~/Videos
endos-downloader "https://feeds.simplecast.com/..." --latest 1 -d ~/Podcasts

# A magnet from the swarm, seeding to ratio 1 or for 30 minutes before exiting
endos-downloader "magnet:?xt=urn:btih:..." --seed-ratio 1 --seed-time 30 -d ~/Downloads

# A MEGA folder; a Pixeldrain list; an imgur album through gallery-dl
endos-downloader "https://mega.nz/folder/...#..." -d ~/Downloads
endos-downloader "https://pixeldrain.com/l/..." -d ~/Downloads
endos-downloader "https://imgur.com/a/..." -d ~/Pictures

# Unpack, sort and scan what was downloaded, then run a script on it
endos-downloader https://example.com/pack.zip --extract --delete-archive --virustotal-key "$VT_KEY" \
    --exec "/home/me/bin/after.sh {path}"

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

Set `ENDO_HISTORY_PATH` to use a different file. The download archive that tells which videos of a playlist or channel are new (see [Links that just work](#features)) is `download-archive.txt` in the `EndosUnifiedDownloader` data folder (`%LOCALAPPDATA%` on Windows, `$XDG_DATA_HOME` or `~/.local/share` on Linux); set `ENDO_ARCHIVE_PATH` to use a different file.

### Subtitles and tags

`--subs LANGS` (GUI: Settings > Media > Subtitles) saves a video's subtitles next to it, as `name.LANG.srt`. Give languages as `en,es`, or `all`.
- The site's SRT is taken where it has one, else its WebVTT, else its best format, converted to SRT when ffmpeg is present. Without ffmpeg they stay as the site has them (`.srt`, `.vtt` or the site's own format), and so does one that fails to convert, with a warning in the log. A failed conversion never fails the video.
- A site's own subtitles come first. Its automatic captions are used for a language it has no subtitles of its own in. `all` gets every language the site has its own subtitles in, without the automatic translations and YouTube's live chat.
- A language code also stands for the site's own subtitles of its regions: `en` gets YouTube's `en-US` where the video has no plain `en` of its own. This needs what yt-dlp found first, which audio presets skip: for those, name the region (`en-US`) or give a pattern (`en.*`, which also matches automatic captions).
- A language the video has no subtitles in is skipped, not an error, with a warning in the log; so are subtitles that came empty. YouTube subtitles that need a proof-of-origin (PO) token are left out, with yt-dlp's warning in the log, and not retried. No PO-token plugins are used.
- When the engine downloads the streams itself, yt-dlp then writes only the subtitles, from the same extraction.

Tags are embedded by default, when ffmpeg is present. The file gets the title, artist, date, description, link, genre, album and show or episode details, and the video's chapters. Audio presets (`mp3`, `m4a`) also get the thumbnail as cover art. Without ffmpeg the download goes on without tags. `--no-embed-metadata` (GUI: clear "Embed tags and chapters") leaves tags out.

### Clips, SponsorBlock and formats

- **Clips:** `--sections START-END` (the browser extension's Clip) downloads only that part of a video, with yt-dlp's `--download-sections`. Times are seconds, `M:SS` or `H:MM:SS`. A clip is cut without re-encoding, so it starts at the keyframe at or before the start asked (`--force-keyframes-at-cuts` is not used, since it re-encodes the whole clip). Each clip's file name ends in its span, `Title [90-150].mp4`, so clips of one video and the whole video never take each other's place.
- **SponsorBlock:** `--sponsorblock remove` (GUI: Settings > Media > SponsorBlock, "Remove them") cuts YouTube's sponsor, self-promotion and interaction reminder segments out, from the [SponsorBlock](https://sponsor.ajay.app) database; `mark` ("Mark as chapters") keeps the video whole and marks every segment as a chapter. Other sites have no SponsorBlock data, and nothing changes for them.
- **Container:** `--merge-format` (GUI: Settings > Media > Video format) joins the video and audio in MP4 (the default), MKV or WebM. WebM takes only VP9/AV1 video with Opus or Vorbis audio, so a video without those is saved as MKV instead.
- A clip, SponsorBlock, MKV or WebM is yt-dlp's own download instead of the engine's (the engine cannot cut, mark or rejoin), over `--concurrent-fragments` connections.

### Live streams

A live stream is recorded by yt-dlp. This covers a site's live video and an HLS playlist that has no end yet.
- By default the recording starts now. With `--live-from-start` (GUI: "Record live streams from the start") it starts from the beginning, where the site keeps it (YouTube does).
- `--wait-for-video` waits for a scheduled stream or premiere to begin, checking every 1 to 10 minutes (GUI: "Wait for scheduled streams").
- Progress shows the size recorded and how long the recording has run, not a percentage.
- Stop ends the recording and keeps it (Ctrl+C in the CLI, Pause in the GUI). yt-dlp is asked to stop the way Ctrl+C at a terminal asks it: SIGINT on Linux, a Ctrl+C sent to its console on Windows (the CLI, which has a console of its own, starts itself again without one to send it). It finishes the file, which is reported done with its size and saved in history as completed. A recording that has ended by itself and is being finished is left to finish.
- Closing the GUI window stops the recordings the same way, and the window stays open until their files are finished. Close it a second time to quit at once, which cuts them off.
- Sometimes yt-dlp is stopped outright instead: when it does not stop recording within 30 seconds, or cannot be asked. What it recorded is kept either way:
  - An MPEG-TS recording is named `.ts`, then remuxed to `.mp4` when ffmpeg is present and its disk has room for the copy that writes; without that room the `.ts` is kept, which plays as it is, and the log says so.
  - Video and audio recorded apart are joined with ffmpeg. When that fails (no ffmpeg, a full disk) they stay as their `.part` files, which the error names, and the stream is not recorded again over them.
  - A recording that cannot be renamed is kept as its `.part` file and reported done under that name.
- A recording that breaks off (the connection drops, the stream fails) keeps what it recorded the same way, and the download fails naming that file. So does one ended because the space left on its disk came down to the size of what it recorded plus 512 MiB, which is checked every 10 seconds while it records: finishing the file (yt-dlp's fixup and tags, the remux to MP4, joining streams recorded apart) writes a copy of it, and a full disk would cut it off mid-write.
- DRM-protected live TV (a FairPlay, Widevine or other non-`identity` key format) is refused, not recorded.

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

The service downloads every line of the queue, two at a time, and then exits. Files that are already complete are skipped on the next run, and interrupted files resume. `systemctl stop` sends SIGTERM, which saves the resume state, and finishes the file of a live recording (the unit waits up to 15 minutes for that before it kills the service). The service restarts only after a crash. It does not restart after a failed download, so a dead link cannot cause a restart loop. The unit runs as an unprivileged user with a read-only system, no home-directory access and a restricted set of address families. A folder, playlist, channel or feed line is listed on every run, and only what is new is downloaded, so the queue can hold subscriptions. Its history, the download archive and the managed yt-dlp (and ffmpeg, once `--install-ffmpeg` is added to `ExecStart`) are stored in `/var/lib/endos-downloader`.

---

## GUI

`Endos-Unified-Downloader` (`endos-downloader-gui` on Linux) is a native desktop app built on the same engine. On Linux it needs a desktop session with OpenGL, and file dialogs use the desktop's xdg-desktop-portal.

- **Layout:** a sidebar with Add, Queue (with its count), History and Settings, the clipboard watch switch and a System/Dark/Light theme picker; Inter type and Phosphor icons. Ctrl+V outside a text box adds each copied link to the queue, Delete removes the selected queue row (not a running one), and Ctrl+1 to Ctrl+4 switch pages (on macOS ⌘ instead of Ctrl, and Backspace removes the row too).
- **Queue rows:** status, name, progress bar, `#id · % · done of total · speed · time left`, for a torrent its peers and what it uploaded (and Seeding), the post-processing notes, and icon buttons for the row's actions; a failed row shows its error. Click selects a row, double click shows its download.
- **Live view:** a chunk map, per-connection progress, a throughput graph, real open-connection count, smoothed speed and ETA. A **STALLED** warning appears after 5 s without data, and downloads with several mirrors get a per-mirror speed table.
- **Pause, Resume, Start Over:** Pause waits until resume state is saved ("Pausing…"). Resume continues the same file with the same settings. Start Over and Delete Leftovers remove only the `.part` and its resume state, never a finished file.
- **Batch queue:** add one download per line (mirrors of one file go on one line, separated by spaces). Each item keeps the folder and options it was added with. A `.metalink`, `.meta4` or `.torrent` (a URL, a local path, or a file dropped on the window) adds one download per file it lists, each saved under its own path in the folder, and a Drive or MediaFire folder, playlist, channel or podcast feed one download per file, video or episode, read without holding up the window; one that lists more than 50 files asks first, a video link that names its playlist asks at once for the video or the whole playlist, and what a listing left out (an unreadable subfolder, a file behind a password) is shown under the header. The Authorization header is never sent to the mirrors it lists. Auto-run handles 1–8 downloads at once (4 by default; a number you saved before is kept), with per-item Start, Pause, Resume, Retry, Remove, Open and Folder. Downloads share their HTTP connections and TLS sessions.
- **Verify & Repair:** checks a file against the BLAKE3 hash recorded when it was downloaded. Results are VERIFIED, INCOMPLETE (with a cancellable repair of just the missing ranges), CHECKSUM MISMATCH or UNVERIFIED.
- **Clipboard watcher:** offers "Download now" / "Add to queue" for copied links and magnets. It ignores links the app copied itself.
- **Add page:** the link, save folder, connections and media quality; under Advanced options the per-download checksum and Authorization header (never saved).
- **Settings page:** every setting, in sections: General (save folder, theme, clipboard, queue), Connections & networks (connections per download and per host (64 by default, 0 = no limit), speed limit for all running downloads together, proxy and proxy pool, several networks), BitTorrent, Debrid & hosts (service, key, magnets through debrid, Google API key for Drive folders, browser cookies or a cookies file, Referer), Media, Post-processing, Browser extension, Updates (version, Check now, checking at startup) and Advanced (retries per chunk, stall timeout, flushing finished files to disk, off by default, see above). Media holds media quality, video format (MP4, MKV or WebM), SponsorBlock (off, remove the segments, or mark them as chapters), subtitles, embedded tags and chapters (on by default), recording live streams from the start, waiting for scheduled streams and installing ffmpeg when a video needs it (asked before the first video that needs it; a notice shows while it installs); for playlists, channels, feeds and cloud folders only new items (on by default) and the newest N (0 = all); and image galleries with gallery-dl.
- **Remembers settings:** folder, connections and every setting are stored in `gui-settings.json` next to the history, the debrid and VirusTotal keys included (they are never put in the saved queue or the history, but read from the settings when a download starts). The Authorization header, the checksum and the Google API key are never saved: the key is kept until the app closes, and taken from `ENDO_GOOGLE_API_KEY` at launch when that is set.
- **Safe to close:** closing the window pauses running downloads and saves their state (waiting at most 3 s). It also stops yt-dlp, but a live recording is finished first (see [Live streams](#live-streams)). The app uses no CPU while idle.

---

## Updates

- **Checked at startup:** the GUI asks this repository's GitHub releases for the newest version (through the proxy in Settings, if set) and, when it is newer, shows "Version X.Y.Z is available" with Update and restart and What's new. Turn it off with "Check for updates at startup" in Settings > Updates; "Check now" next to it checks at once. Nothing is sent but that request.
- **Command line:** `Endos-Unified-Downloader-CLI --update` checks and installs (through `--proxy` if given). The CLI never checks on its own.
- **Signed releases only:** a release is installed only when its `SHA256SUMS` carries a valid signature from the maintainer's key, whose public half is built into the app, and every file matches its SHA-256 there. The list also names its version, so an older release cannot be offered as a new one. Whoever takes over the GitHub account or the download servers cannot make the app install anything else; a release that fails a check changes nothing.
- **What is replaced:** the GUI and the CLI in the app's folder (whichever of them are there) and the `extension/` folder next to them. The GUI saves its queue, finishes live recordings and restarts as the new version. The previous programs are kept as `.old.exe` until the next start deletes them, and a swap that fails puts them back. On macOS the whole `Endo's Unified Downloader.app` is replaced by the release's `.app.tar.gz` for the Mac's chip (the GUI and the CLI in it update it alike), the old bundle is deleted at the next start, and the extension in the data folder is refreshed from the new one.
- **Windows and macOS only:** on Linux What's new opens the release page; pull and run `./install.sh` again. On macOS the app must be in a folder it can write to (Applications), not run from the disk image or from where macOS translocated it: move it to Applications, open it once, then update.
- **Browser extension:** when it was loaded from the `extension/` folder next to the app, it reloads itself once the app reports a newer version there.
- **From 1.3.0:** 1.3.0 and earlier have no updater, so download the first release that has one from the [releases page](https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases) by hand; it updates itself from then on.

---

## Browser extension

`extension/` is a Manifest V3 extension for Chrome and Edge (121 or later) and Firefox (128 or later) that sends the videos of a page to the running GUI. It needs no build step.

- **Detection:** it watches each tab's requests and lists HLS playlists, DASH manifests (`.mpd` or `application/dash+xml`) and video or audio files: any `video/*` or `audio/*` type (`.mp4`, `.webm`, `.mkv`, `.mp3` and others, not `.ts` or `.m4s` segments), a media request answered as `application/octet-stream`, and a ranged request with no usable type when a `<video>` on the page plays that URL. A playlist is recognised by its content, so one served as `text/plain` or `application/octet-stream` is found too, and so is one the page fetches and reads itself, a playlist the page builds itself (a `blob:` or `data:` URL, sent to the app as text), and playlist or manifest links inside the JSON the page reads. Small files (500 KB by default) are skipped, as are YouTube, Twitch and the other media sites yt-dlp handles, which get the media card instead (see Media sites below). A tab keeps its newest 200 items. Per video the popup shows the quality choices of a master playlist (by height when its audio is a separate rendition), LIVE, DASH, AES-128 and DRM marks, a file name you can edit, and Block host. When the extension is installed or updated it is added to the tabs already open; the popup asks you to reload a page that shows nothing, since what it loaded before is not seen again.
- **Sending:** Download sends the link with the page's Referer, Origin, Cookie, User-Agent and other request headers, plus the cookies the tab's own requests carried to each host the stream uses (its variants, audio, subtitles, segments and keys), for that download only; Settings in the app are not changed. The context menu does the same for links, media and pages. In the app a DASH manifest, and an HLS stream whose audio is a separate rendition, go to yt-dlp, which keeps to the height chosen in the popup (see HLS under [Features](#features)).
- **Media sites:** on a video, Short, live stream, playlist or channel page of YouTube, or any page but the home page of Twitch, TikTok, X, Vimeo, Reddit, Instagram, Facebook, SoundCloud, Dailymotion or Bilibili, the popup shows a media card. The app asks yt-dlp about the link (at most two at once, each answer kept for 10 minutes, nothing downloaded) and the card shows its thumbnail, title, channel and length, LIVE or UPCOMING, and lets you choose: the quality (Best or a height the video has, marked 4K, HDR or 60 fps) or Audio (M4A or MP3); the format (MP4, MKV, WebM); subtitles; a clip, whose start and end "Use the current time" reads from the page's video; SponsorBlock (YouTube videos only); "This video" or "Whole playlist (N)" for a video in a playlist; and From now or From start for a live stream. Download sends the choice with the link; the last quality, format and subtitles are remembered. When the app cannot read the link, the card says why and offers "Download anyway (best quality)". The context menu sends a media page with the card's last choice, else the app's settings.
- **YouTube button:** a Download button sits beside Share under a YouTube video, and among a Short's buttons, with Best quality, 1080p, 720p, Audio (MP3), Audio (M4A), and More options…, which opens the popup (where the browser does not allow that, it says to click the toolbar icon). It follows YouTube's theme and its in-page navigation, acts on real clicks only, and touches nothing on the page but its own button. Turn it off in Settings: "Show a Download button on YouTube".
- **Sign-in on media sites:** "Use my browser sign-in on media sites" (Settings, off by default) sends the site's own cookies (for YouTube, those of `youtube.com`, no other site's) with the card's look-up and with the downloads of media pages, so the app can download age-restricted or members-only videos you can already watch. The app saves them like any cookies sent with a download, in a file per site that yt-dlp renews, never in a log, error or the queue. yt-dlp warns that using your account can get it rate-limited or flagged by the site. Off, the look-up and media downloads carry no cookies, as before.
- **Links:** the popup's Links view lists every link of the page (and of its same-origin frames): `a` and `area` links, image, video and audio sources (the largest image of a `srcset`), and `link` tags for media. Filter them by type (Video, Audio, Images, Archives, Documents, Programs), by text or a `/regex/`, and to the same site; tick them (Select all / None apply to the links shown) and "Send N to app" queues up to 1000 at once. Each gets the page as its Referer, the browser's User-Agent and only the cookies of its own origin (an `http://` link never gets an `https://` one's). Nothing the extension sends can set the post-processing, the command run after a download, the networks or seeding.
- **Via browser:** for an HLS stream or file that the app is refused (a TLS fingerprint check, a bot check), "Via browser" downloads it in the page itself, with the page's cookies and headers: HLS segments in order (AES-128 decrypted in the page, image disguises cut off, a separate audio rendition as a second track), files in 8 MiB ranges. Progress and a Stop button show under Record; what arrived before Stop is kept, while a download that fails is thrown away and its error stays under Record until dismissed. The tab must stay open.
- **Capture:** for video the app cannot fetch (blob or MSE sources), "Capture from start" reloads the page and copies the data the player appends to its buffers from the beginning. "Record playback" records a playing `<video>` in real time instead; a muted video plays at near-zero volume while it is recorded, so its sound is kept, and is muted again afterwards. The pieces go to the app, and ffmpeg joins them in the save folder without re-encoding: fragmented MP4 pieces in decode order with repeats dropped (seeking during a capture is fine), MP4 and MPEG-TS into `.mp4`, WebM into `.webm`, anything else into `.mkv`. A quality switch mid-capture is joined by re-encoding that track (H.264/AAC, at the largest size, padded). Without ffmpeg the raw tracks are saved next to each other. A capture the app was taking when it closed is joined the next time it starts, with a notice.
- **Install:** in Chrome or Edge open `chrome://extensions`, turn on Developer mode, choose Load unpacked and select the `extension/` folder. In Firefox open `about:debugging#/runtime/this-firefox`, choose Load Temporary Add-on and select `extension/manifest.json`; then use "Allow access to all sites" in the popup if it shows, since Firefox does not grant that on install.
- **Connection:** the app must be running. The extension finds it on `127.0.0.1`, ports 49152 to 49155; an older app there (one without `/ping`) is reported as needing a restart after updating. Only an extension (or a local program, which sends no `Origin`) may use its routes; a web page gets `403`, so no site can queue downloads or send cookies to the app. Cookies sent with a download reach the Anna's Archive resolver only when the link is on Anna's Archive.
- **Limits:** DRM streams (SAMPLE-AES, FairPlay, Widevine, PlayReady) cannot be downloaded, by the app or through the browser, and a recording of one is blank or fails. Scrambled segments that are not AES-128 need Capture. "Via browser" takes no live streams (use Capture), DASH manifests, playlists the page built itself or files whose server lets only the page's video player read them (no CORS), and decrypts AES-128 only on `https` pages. A playlist the page built itself is not re-read when its links expire, and a live one is not listed (the app could not reload it). Tests: `node --test "extension/test/*.test.mjs"`.
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
cargo test --workspace          # tests (set ENDO_HISTORY_PATH, and LOCALAPPDATA or XDG_DATA_HOME, to a
                                # temporary place to keep your history, archive and managed tools untouched)
cargo clippy --workspace --all-targets
cargo bench --bench range_bench -p hyperfetch-core
```

---

## License

Licensed under either of the Apache License, Version 2.0 or the MIT license, at your option.

The ffmpeg the app installs when a video needs it (see ffmpeg under Features) is a separate program under the GPL, downloaded from [yt-dlp/FFmpeg-Builds](https://github.com/yt-dlp/FFmpeg-Builds); its licence and sources are published there.
