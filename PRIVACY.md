# Privacy Policy

Endo's Unified Downloader is a desktop download manager with a browser extension (for Chrome, Edge, Firefox and other browsers). This policy covers both. Effective November 15, 2026.

**The short version:** everything stays on your computer. Neither the extension nor the app collects, sells or shares your data, and neither sends anything to the developer. There are no accounts, ads, analytics or tracking.

## The browser extension

The extension finds videos, streams and downloadable links on the pages you visit and hands the ones you choose to the Endo's Unified Downloader app on the same computer.

**What it handles, and why:**

- **Page addresses, and the links, video, subtitle and stream addresses found on a page or in what the page loads.** They are used to show you what can be downloaded, and to send what you pick to the app. Only video, subtitle and stream addresses are picked out of what a page loads; nothing else from it is kept. When you open the popup, the address of the page goes to the app, which answers from its own lists whether it can download from that page (it fetches nothing); "Ask the app for a video on this page" has the app read the page itself.
- **Request headers of those links (such as the referring page, the browser's user agent and a site's authorization header), and of the browser downloads it hands to the app when you turn that on.** Many sites only serve a file to a request that looks like the browser's own, so the app repeats them; an authorization header is sent only to the link's own hosts.
- **Cookies of the site a link belongs to, and of no other site.** They go to the app only with that link, when you send it, so the app can download files that need you to be signed in, the same way your browser does. For media sites they are sent only when you turn on "Use my browser sign-in on media sites".
- **New links for the app's downloads whose link expired.** The app tells the extension which of its downloads need a new link (their file names, sizes, sites and the pages they came from). When you start one of those files again in the browser, or its page plays its video again, the extension sends that new link to the app with the same request headers and cookies as above, in place of a new download. Nothing else is sent for this.
- **The app's own downloads (their names and progress),** shown in the popup while its Downloads view is open. They are not stored.
- **Video recorded from a page,** only when you start a recording yourself. It is passed to the app in pieces as it is recorded.
- **Your settings and your last download choices.** They are stored in the browser's local extension storage. What the extension found on each tab, and the last 20 browser downloads with why each went to the app or stayed (not those of private windows), are kept in the browser's session storage and are cleared when the browser closes.

**Where it goes:** only to the Endo's Unified Downloader app on your computer, at `127.0.0.1` (your own computer's local network address, which no other computer can reach). The extension makes no requests to any other server. When it fetches a file for you from inside a page, the request goes only to the site that serves that file.

**What it never does:** send your data to the developer or to anyone else, sell it, use it for ads or credit decisions, or let people read it. It does not read anything you type into pages. It reads the text you select on a page only when you choose "Download the links in the selection", to find the links in it.

## The desktop app

The app stores its settings, queue and download history in its own folder on your computer, along with the names of the sites it downloaded from, how many connections each was fastest with, how many it served at once, the speed it held a download to and whether it speaks HTTP/2 or HTTP/3, and the top speed your internet line reached (all forgotten after 30 days). It connects only to:

- the sites you download from;
- for a container image, its registry and the token service the registry names (for Docker Hub, `auth.docker.io`), asked for an anonymous pull token that names only the image; for an Ollama model, Ollama's registry (`registry.ollama.ai`) and the storage it sends the files from, which learn the model asked for;
- for an IPFS link (`ipfs://...`, or a gateway's `/ipfs/...` link), public IPFS gateways (ipfs.io, dweb.link, Pinata, Filebase and 4EVERLAND), which then learn the content address it names;
- for a link to an archive site, the services that list its files and the mirrors that hold them: archive.org's metadata, scraping and search APIs (for an Internet Archive collection, search or account); Kiwix's download server and library catalog, and the mirrors its Metalinks name; the Wikimedia dumps' mirrors (your.org, Umeå University, PDApps); Project Gutenberg's mirror list and the mirrors on it; `export.arxiv.org` (for an arXiv paper); `api.figshare.com` (for a Figshare article); Software Heritage's vault; and the Wayback Machine's CDX API (for a listing of captures). Each learns only the record, file or link it is asked about;
- GitHub, to check for and download its own updates, and the tools it uses (yt-dlp and ffmpeg). It also contacts Codeberg for gallery-dl;
- for an account you sign in to under Accounts & tools (`accounts` in the CLI), and only then: the helper program it runs for it (rclone, legendary, DepotDownloader or streamlink), downloaded from its project's GitHub releases when first needed; your cloud drives, through rclone; Epic Games, through legendary (its sign-in link `legendary.gl/epiclogin` leads to Epic's page, and legendary checks `api.legendary.gl` for its own updates); GOG's sign-in and account services (`auth.gog.com`, `embed.gog.com`) and its download servers; Steam, through DepotDownloader; and for a live stream sent to streamlink, the stream's site. Each learns only what you sign in to or download;
- for an NZB, the Usenet servers you set, signed in to with the user name and password you give them (saved in the app's settings only, never in the queue, history or logs); they learn which articles are asked for;
- for a mod link, Modrinth's API (`api.modrinth.com`) and Thunderstore's (`thunderstore.io`), and, with the key you give each, CurseForge's API (`api.curseforge.com`) and Nexus Mods' (`api.nexusmods.com`), each sent only to its own service (saved in the app's settings only, never in the queue, history or logs); they learn which mod, pack or file is asked for, and the files come from the hosts they name (Modrinth's CDN or GitHub for a modpack, CurseForge's and Nexus Mods' download servers);
- for a paper's DOI, Unpaywall (`api.unpaywall.org`), sent the DOI with the email address you set for it, and nothing when none is set; doi.org, asked for the DOI's record; for a PubMed Central article, its open-access set on Amazon S3 (`pmc-oa-opendata.s3.amazonaws.com`); for a preprint, bioRxiv's API (`api.biorxiv.org`). Each learns which paper is asked for;
- for a package's page, its registry: Maven Central (`repo1.maven.org`), NuGet (`api.nuget.org`), Go's module proxy and checksum database (`proxy.golang.org`, `sum.golang.org`), Homebrew's API (`formulae.brew.sh`) and GitHub's registry (`ghcr.io`, with an anonymous token, and the storage it sends the bottle from), Flathub (`flathub.org`, `dl.flathub.org`), anaconda.org (`api.anaconda.org`, `conda.anaconda.org`), RubyGems (`rubygems.org`), Packagist (`repo.packagist.org`, and where its dist links lead, often GitHub), F-Droid (`f-droid.org` and the mirrors its index names). Each learns which package is asked for;
- services you turn on and give your own key for: VirusTotal (it sends only a downloaded file's SHA-256 hash, never the file), Real-Debrid or AllDebrid, and the Google Drive API. An S3 access key you give signs requests only to the S3 storage you download from (Amazon S3, Wasabi, Backblaze B2, DigitalOcean Spaces, R2, or the server you name as its endpoint); its secret is never sent, only signatures made with it.

When you turn on "Send links from your phone" (off by default), the app also listens on your local network for links sent from your own phone, from the page its QR code opens. It answers only devices on your local network, and adds a link only when the request carries the code from that QR code, which is saved in the app's settings and never logged. The links go from your phone straight to the app: nothing leaves your network for this.

For SFTP downloads the app keeps, in a `known_hosts` file in its folder, the name or address and port of each SFTP server it signed in to and that server's public host key, so that it can tell when a server's key changes. It holds no passwords and nothing about the files.

The sign-ins of those accounts stay in an `accounts` folder in the app's folder, never your passwords, which only the services' own pages and apps see: the drives' tokens rclone keeps (`accounts/rclone/rclone.conf`), the sign-in legendary keeps (`accounts/epic/user.json`), the app's GOG token (`accounts/gog/token.json`), and for Steam your account's name (`accounts/steam/account`) and the sign-in DepotDownloader remembers (elsewhere than on Windows in `accounts/steam`; on Windows in .NET's storage for your user, `%LOCALAPPDATA%\IsolatedStorage`, where the app cannot put it). Signing out deletes them (a cloud drive with `accounts sign-out cloud NAME`).

Your keys, passwords and that code are saved only on your computer. They are never stored with the queue or the history.

## Deleting your data

- Removing the extension deletes everything it stored.
- In the app, you can clear the history at any time.
- Uninstalling the app and deleting its data folder removes everything it stored.

## Chrome Web Store

The use of information received by the extension adheres to the Chrome Web Store User Data Policy, including the Limited Use requirements.

## Changes and contact

Changes to this policy are published in this file, with a new effective date. For questions, open an issue at <https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/issues>.
