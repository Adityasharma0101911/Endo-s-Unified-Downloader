# Privacy Policy

Endo's Unified Downloader is a desktop download manager with a browser extension (for Chrome, Edge, Firefox and other browsers). This policy covers both. Effective October 28, 2026.

**The short version:** everything stays on your computer. Neither the extension nor the app collects, sells or shares your data, and neither sends anything to the developer. There are no accounts, ads, analytics or tracking.

## The browser extension

The extension finds videos, streams and downloadable links on the pages you visit and hands the ones you choose to the Endo's Unified Downloader app on the same computer.

**What it handles, and why:**

- **Page addresses, and the links, video and stream addresses found on a page or in what the page loads.** They are used to show you what can be downloaded, and to send what you pick to the app. Only video and stream addresses are picked out of what a page loads; nothing else from it is kept.
- **Request headers of those links (such as the referring page and the browser's user agent).** Many sites only serve a file to a request that looks like the browser's own, so the app repeats them.
- **Cookies of the site a link belongs to, and of no other site.** They go to the app only with that link, when you send it, so the app can download files that need you to be signed in, the same way your browser does. For media sites they are sent only when you turn on "Use my browser sign-in on media sites".
- **Video recorded from a page,** only when you start a recording yourself. It is passed to the app in pieces as it is recorded.
- **Your settings and your last download choices.** They are stored in the browser's local extension storage. What the extension found on each tab is kept in the browser's session storage and is cleared when the browser closes.

**Where it goes:** only to the Endo's Unified Downloader app on your computer, at `127.0.0.1` (your own computer's local network address, which no other computer can reach). The extension makes no requests to any other server. When it fetches a file for you from inside a page, the request goes only to the site that serves that file.

**What it never does:** send your data to the developer or to anyone else, sell it, use it for ads or credit decisions, or let people read it. It does not read anything you type into pages.

## The desktop app

The app stores its settings, queue and download history in its own folder on your computer, along with the names of the sites it downloaded from and how many connections each was fastest with (forgotten after 30 days). It connects only to:

- the sites you download from;
- GitHub, to check for and download its own updates, and the tools it uses (yt-dlp and ffmpeg). It also contacts Codeberg for gallery-dl;
- services you turn on and give your own key for: VirusTotal (it sends only a downloaded file's SHA-256 hash, never the file), Real-Debrid or AllDebrid, and the Google Drive API.

Your keys and passwords are saved only on your computer. They are never stored with the queue or the history.

## Deleting your data

- Removing the extension deletes everything it stored.
- In the app, you can clear the history at any time.
- Uninstalling the app and deleting its data folder removes everything it stored.

## Chrome Web Store

The use of information received by the extension adheres to the Chrome Web Store User Data Policy, including the Limited Use requirements.

## Changes and contact

Changes to this policy are published in this file, with a new effective date. For questions, open an issue at <https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/issues>.
