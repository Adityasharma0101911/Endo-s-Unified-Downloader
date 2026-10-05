# Popup UI harness

Loads the extension into **headless** Edge, opens fixture pages that produce each popup state, opens
`popup.html?tab=<id>` as a tab for each, screenshots every view in light and dark, clicks through the main actions
and checks what reached a mock of the desktop app. Nothing touches the real screen, mouse, keyboard or browser
profile. Not shipped (the release steps leave `extension/test/` out of both zips) nor run by
`node --test "extension/test/*.test.mjs"`.

```
node extension/test/ui/run.mjs [--out <dir>] [--only hls-master,links] [--ext <extension dir>] [--edge <msedge.exe>] [--media-host <host>]
```

- `--out`: where the PNGs and `report.json` go (default: a new `endo-popup-ui-*` folder in the temp dir).
- `--only`: fixture names (below) to run; the app-status captures always run.
- `--ext`: another copy of the extension to load, e.g. the last release, for before/after shots.
- Needs Node 22+ (global `WebSocket`) and Edge. Port 9447 (CDP), 8765 (fixtures) and one of 49152–49155 (mock app)
  must be free; it refuses to run while the real app answers on 49152–49155.
- Exit code 0 when every check passed, 1 when one failed, 2 when the run itself broke.

## What it does

| Fixture | Page | Checks |
|---|---|---|
| `hls-master` | `stream.html?src=/hls/master.m3u8` (3 variants) | 1 item; 3 qualities; Download sends the master URL with `hls`, the page's `X-Token` header and referer; with 720p chosen it sends that variant; ArrowRight/End/Home/ArrowLeft in the `[role=tablist]` move the selection and focus; reduced motion leaves no transition or animation, and without it none lasts over 250 ms |
| `hls-live` | live media playlist (no `ENDLIST`) | 1 item, shows LIVE |
| `hls-aes` | `METHOD=AES-128` | 1 item, shows AES-128; Clear empties the list (empty state) |
| `hls-drm` | `METHOD=SAMPLE-AES` | 1 item, shows DRM, Download disabled, nothing sent, no MP4 switch |
| `dash` | `dash/manifest.mpd` | Download sends it with `dash` |
| `file` | 600 KB `video/mp4` | Download sends the URL, headers, referer, a `.mp4` name |
| `via-browser` | `/media/slow.mp4` (20 MB; the extension's Range request trickles) | the ⋯ menu's Download via browser reaches `/record/start`; the Record tab shows its progress; Stop ends it |
| `via-browser-fails` | `/media/gone.mp4` (404 to the extension's Range request) | the failed download stays with its reason under "Failed"; Dismiss removes it |
| `media-site` | `media-site.html` served as `http://www.twitch.tv/…` (`--host-resolver-rules`) | Download this page sends the page URL |
| `links` | `links.html` (archives, docs, images, a duplicate, `#`/`mailto:`) | Select all + Send sends every link `normalizeLinks` keeps, in one batch, with the page as referer; the Archives chip and a typed filter narrow the list and the Send count; a bad `/regex/` says so |
| `video` | `video.html` (`<video>` fed by a canvas stream) | Record playback reaches `/record/start`; Stop ends with chunks and `/record/<id>/finish` |
| `unsupported` | `about:blank` | screenshots only |

Then, on the `hls-master` tab: the mock app is switched to `outdated`, `absent` and back to `running` and every view
is shot (`app-<mode>-<view>-<theme>.png`); with the app absent a Download error must stay until its ✕ is clicked. The
"site access needed" notice is shot with GET_STATE's `hostAccess` patched to `false` in the popup (an extension loaded
with `--load-extension` always has it). Min size and Convert to MP4 are changed, the popup reopened (values and the
last tab must survive) and put back. Last, on the `dash` tab, Block host (two clicks in the ⋯ menu) must drop the item
and list the host in Settings, whose ✕ unblocks it.

Every capture is also checked for sideways overflow, and every visible text and form field in it for WCAG AA contrast
(4.5:1 against the colours actually behind it; disabled or faded controls exempt); the popups must log no exceptions or
console errors.

Captures: `<fixture>-<view>-<theme>.png` for views Media, Record, Links, Settings (a tab with that name is clicked; a
`<summary>` is opened; else the heading is scrolled to) plus `<fixture>-<step>-<theme>.png` after actions and
`app-<mode>-<view>-<theme>.png`, `host-access-<view>-<theme>.png`, `block-<step>-<theme>.png`. The viewport is 420×600 (the popup's size) with `prefers-reduced-motion: reduce`.
`report.json` holds every capture, check, page exception/console error, and the non-`/ping` requests the mock app got.

Controls are found by role and visible text or `aria-label` ("Download", "Download this page", "Select all"/"All",
"Send N…", "Record playback", "Stop…", "More actions", "Download via browser", "Block <host>", "Click again to
block", "Unblock <host>", "Dismiss", "Clear list"), so the harness survives markup changes as long as those words stay. A
few state checks read the redesign's ids (`#items`, `#empty`, `#rec-list`, `#recs-title`, `#links`, `#links-query`,
`#blocked`, `#min-size`, `#convert-mp4`).

## Pieces

- `mock-app.mjs`: `startMockApp({ mode })` → `{ port, mode, requests, since() }`; modes `running` / `outdated` (the
  pre-`/ping` app: 404 and the old `/add` preflight) / `absent` (drops connections). `node mock-app.mjs outdated`
  runs it alone and prints each request.
- `fixtures/serve.mjs`: static server for `fixtures/` on 8765; `node fixtures/serve.mjs` runs it alone.
- Edge runs with `--headless=new --remote-debugging-port=9447`, a throwaway `--user-data-dir` in the temp dir (a
  deep path breaks `chrome.storage.local` on Windows' 260-character limit), `--load-extension` and
  `--disable-features=DisableLoadExtensionCommandLineSwitch` (branded builds ignore `--load-extension` otherwise).
