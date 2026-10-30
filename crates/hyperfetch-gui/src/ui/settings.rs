use std::hash::Hash;
use std::path::PathBuf;

use hyperfetch_core::engine::IpFamily;
use hyperfetch_core::AudioSpec;

use super::*;
use crate::settings::{VIDEO_CODECS, VIDEO_HEIGHTS, VIDEO_RANGES};

/// Seconds since the Settings page was opened (`render` keys the page change the same way).
fn opened(ctx: &egui::Context) -> f32 {
    anim::since(ctx, egui::Id::new("page"), (Tab::Settings as u8, None::<usize>), false)
}

/// The `n`th card of the page: as the page opens the cards rise in one after another, each icon
/// popping.
fn section(ui: &mut Ui, n: usize, glyph: &str, title: &str, add: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    let t = anim::progress(ui.ctx(), opened(ui.ctx()), anim::stagger(n), anim::APPEAR);
    anim::shifted(ui, anim::ease(t), anim::RISE, |ui| {
        card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                anim::icon(ui, glyph, 18.0, p.accent, 0.6 + 0.4 * anim::overshoot(t), 0.0);
                ui.label(bold(title).size(16.0).color(p.strong));
            });
            add(ui);
        });
    });
    ui.add_space(12.0);
}

/// How lit the row of setting `title` is: 1.0 as `value` changes on the open page, fading out
/// over 0.8 s, typing included; a change made elsewhere (the sidebar, the Add page) does not
/// light it up when the page opens.
fn flash(ctx: &egui::Context, title: &str, value: impl Hash) -> f32 {
    let since = anim::since(ctx, egui::Id::new(("setting", title)), value, false);
    if since < opened(ctx) {
        1.0 - anim::ease(anim::progress(ctx, since, 0.0, 0.8))
    } else {
        0.0
    }
}

/// One row of a Settings section: `title` and what it does on the left, `control` on the right
/// (laid out right to left). The row lights up for a moment as `value`, what it sets, changes.
fn setting(ui: &mut Ui, title: &str, about: &str, value: impl Hash, control: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    let lit = flash(ui.ctx(), title, value);
    ui.separator();
    let background = ui.painter().add(egui::Shape::Noop);
    let row = ui.horizontal(|ui| {
        let width = (ui.available_width() * 0.5).clamp(160.0, 380.0);
        let size = Vec2::new(width, ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(size, Layout::top_down(Align::LEFT), |ui| {
            ui.set_width(width);
            ui.label(RichText::new(title).color(p.text));
            if !about.is_empty() {
                ui.label(RichText::new(about).small().color(p.muted));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
    if lit > 0.0 {
        let glow = egui::epaint::RectShape::filled(row.response.rect.expand2(Vec2::new(8.0, 3.0)), 6.0, p.accent.gamma_multiply(0.14 * lit));
        ui.painter().set(background, glow);
    }
}

/// A setting with an on/off switch; true when it was switched.
fn switch(ui: &mut Ui, title: &str, about: &str, on: &mut bool) -> bool {
    let mut changed = false;
    setting(ui, title, about, *on, |ui| changed = toggle(ui, on).changed());
    changed
}

fn text_setting(ui: &mut Ui, title: &str, about: &str, value: &mut String, hint: &str, secret: bool) {
    setting(ui, title, about, egui::Id::new(&*value), |ui| {
        ui.add(egui::TextEdit::singleline(value).hint_text(hint).password(secret).desired_width(ui.available_width()));
    });
}

pub(super) fn settings_page(app: &mut App, ui: &mut Ui) {
    section(ui, 0, icon::SLIDERS, "General", |ui| general_settings(app, ui));
    section(ui, 1, icon::DESKTOP, "Desktop", |ui| desktop_settings(app, ui));
    section(ui, 2, icon::NETWORK, "Connections & networks", |ui| connection_settings(&mut app.settings, ui));
    section(ui, 3, icon::MAGNET, "BitTorrent", |ui| torrent_settings(app, ui));
    section(ui, 4, icon::CLOUD_ARROW_DOWN, "Debrid & hosts", |ui| host_settings(app, ui));
    section(ui, 5, icon::FILM_STRIP, "Media", |ui| media_settings(app, ui));
    section(ui, 6, icon::BROADCAST, "Streams", |ui| stream_settings(&mut app.settings, ui));
    section(ui, 7, icon::PACKAGE, "Post-processing", |ui| post_settings(app, ui));
    section(ui, 8, icon::PUZZLE_PIECE, "Browser extension", |ui| {
        let p = palette(ui);
        ui.separator();
        let text = "The extension hands this app the links, videos and cookies of the pages you visit, on this computer \
                    only; there is nothing to set up here. What it sends cannot change these settings: post-processing, \
                    the command run after downloads, networks and seeding stay as set here.";
        ui.add(egui::Label::new(RichText::new(text).color(p.muted)).wrap());
        if let Some(dir) = hyperfetch_core::updater::extension_dir() {
            let about = "Chrome, Edge and Brave load the extension from this folder: Extensions, Developer mode, Load unpacked.";
            setting(ui, "Extension folder", about, (), |ui| {
                if ui.add_enabled(dir.is_dir(), button("Open")).clicked() {
                    if let Err(e) = util::open_path(&dir) {
                        app.notice = Some(Err(format!("Could not open {}: {}", dir.display(), e)));
                    }
                }
                ui.add(egui::Label::new(RichText::new(dir.display().to_string()).small().color(p.muted)).truncate());
            });
        }
    });
    section(ui, 9, icon::ARROW_CIRCLE_UP, "Updates", |ui| {
        setting(ui, "Version", "", (), |ui| {
            if ui.add(button("Check now")).clicked() {
                app.check_for_update(true);
            }
            ui.label(env!("CARGO_PKG_VERSION"));
        });
        let about = "Asks GitHub whether a newer release is out and offers it; nothing is installed unless you choose to, \
                     and only releases signed by the maintainer are.";
        switch(ui, "Check for updates at startup", about, &mut app.settings.check_updates);
    });
    section(ui, 10, icon::WRENCH, "Advanced", |ui| {
        let s = &mut app.settings;
        setting(ui, "Retries", "Failed attempts per chunk before the download fails; attempts that made progress don't count.", s.max_retries, |ui| {
            ui.add(egui::DragValue::new(&mut s.max_retries).range(0..=100));
        });
        setting(ui, "Stall timeout", "A connection that receives nothing for this long is retried.", s.stall_timeout_secs, |ui| {
            ui.add(egui::DragValue::new(&mut s.stall_timeout_secs).range(5..=600).suffix(" s"));
        });
        let about = "Wait until each finished file is on the disk before showing it as done. Slower; without it a power \
                     loss right after a download finishes can damage the file (Verify finds that).";
        switch(ui, "Flush finished files to disk", about, &mut s.fsync_on_complete);
    });
}

fn general_settings(app: &mut App, ui: &mut Ui) {
    setting(ui, "Save folder", "Where new downloads go.", egui::Id::new(&app.settings.save_dir), |ui| {
        if ui.add_enabled(!app.dialog_open, button("Browse…")).clicked() {
            app.pending_dialog = Some(Dialog::SaveDir);
        }
        ui.add(egui::TextEdit::singleline(&mut app.settings.save_dir).desired_width(ui.available_width()));
    });
    setting(ui, "Theme", "", app.settings.theme, |ui| {
        for (value, name) in [(2, "Light"), (1, "Dark"), (0, "System")] {
            ui.selectable_value(&mut app.settings.theme, value, name);
        }
    });
    let about = if cfg!(target_os = "macos") {
        "Instant page changes and no moving icons. System follows Reduce motion in the Accessibility settings of macOS."
    } else {
        "Instant page changes and no moving icons. System follows the Animation effects setting of Windows."
    };
    setting(ui, "Reduce motion", about, app.settings.reduce_motion, |ui| {
        for (value, name) in [(Some(false), "Off"), (Some(true), "On"), (None, "System")] {
            ui.selectable_value(&mut app.settings.reduce_motion, value, name);
        }
    });
    let mut watch = app.settings.clipboard_watch;
    if switch(ui, "Watch the clipboard", "Offer links you copy anywhere as downloads.", &mut watch) {
        set_clipboard_watch(app, watch);
    }
    let s = &mut app.settings;
    switch(ui, "Run the queue automatically", "Start queued downloads by themselves.", &mut s.auto_run_queue);
    setting(ui, "Downloads at once", "How many queued downloads run together.", s.max_concurrent, |ui| {
        ui.add(egui::DragValue::new(&mut s.max_concurrent).range(1..=8));
    });
}

/// The days of the week, as `Schedule::days` keeps them.
const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The tray, launch at startup, the queue's schedule and where the data and logs are.
fn desktop_settings(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let s = &mut app.settings;
    let about = "Closing the window hides it in the tray (the menu bar on macOS) and downloads go on; Quit in the tray's \
                 menu closes the app. Where the desktop has no tray, closing closes.";
    switch(ui, "Close to the tray", about, &mut s.close_to_tray);
    let about = "Start hidden in the tray, as --minimized does (minimized where there is no tray).";
    switch(ui, "Start minimized", about, &mut s.start_minimized);
    let about = if cfg!(windows) {
        "Start with Windows, hidden in the tray."
    } else if cfg!(target_os = "macos") {
        "Start when you log in, hidden in the menu bar."
    } else {
        "Start with your desktop session, hidden in the tray."
    };
    let mut startup = s.launch_at_startup;
    if switch(ui, "Launch at startup", about, &mut startup) {
        app.set_launch_at_startup(startup);
    }
    let s = &mut app.settings.schedule;
    let about = "At the start time, on the days ticked, the queue starts; at the stop time the running downloads pause \
                 until the next start. Local time.";
    switch(ui, "Schedule the queue", about, &mut s.enabled);
    ui.add_enabled_ui(s.enabled, |ui| {
        setting(ui, "Start and stop", "HH:MM; leave one empty to only start or only stop.", (egui::Id::new(&s.start), egui::Id::new(&s.stop)), |ui| {
            time_edit(ui, &mut s.stop, "Stop");
            ui.label("to");
            time_edit(ui, &mut s.start, "Start");
        });
        setting(ui, "Days", "The days the queue starts on.", s.days, |ui| {
            for (on, day) in s.days.iter_mut().zip(DAYS).rev() {
                ui.toggle_value(on, day);
            }
        });
        for time in [&s.start, &s.stop].into_iter().flatten() {
            if crate::desktop::minute_of(time).is_none() {
                error_alert(ui, &format!("{} is not a time like 08:30; it is left out", time));
            }
        }
    });
    let data = crate::settings::data_dir();
    let about = "Settings, queue, history and logs. A file named portable next to the program keeps them in a data folder \
                 beside it; --data-dir DIR keeps them in DIR.";
    setting(ui, "Data folder", about, (), |ui| {
        if ui.add(button("Open logs folder")).clicked() {
            let logs = data.join("logs");
            if let Err(e) = std::fs::create_dir_all(&logs).and_then(|()| util::open_path(&logs)) {
                app.notice = Some(Err(format!("Could not open {}: {}", logs.display(), e)));
            }
        }
        ui.add(egui::Label::new(RichText::new(data.display().to_string()).small().color(p.muted)).truncate());
    });
}

/// A text box for a time of day, kept as written, None when empty.
fn time_edit(ui: &mut Ui, time: &mut Option<String>, hint: &str) {
    let mut text = time.clone().unwrap_or_default();
    if ui.add(egui::TextEdit::singleline(&mut text).hint_text(hint).desired_width(56.0)).changed() {
        *time = Some(text.trim().to_string()).filter(|t| !t.is_empty());
    }
}

fn connection_settings(s: &mut Settings, ui: &mut Ui) {
    setting(ui, "Connections per download", "More can be faster; some servers allow only a few.", s.connections, |ui| {
        ui.add(egui::Slider::new(&mut s.connections, 1..=64));
    });
    setting(ui, "Connections per server", "All running downloads together, to one server (0 = no limit).", s.max_connections_per_host, |ui| {
        ui.add(egui::DragValue::new(&mut s.max_connections_per_host).range(0..=256));
    });
    let about = "Start with the connections the server suits and add one while that makes the download faster, up to the \
                 number above; drop one that slows it. Off, a download keeps the number it starts with.";
    switch(ui, "Smarter connection count", about, &mut s.adaptive);
    setting(ui, "Speed limit", "For all downloads together (0 = unlimited).", (s.max_speed_in_mb, s.max_speed.to_bits()), |ui| {
        let in_mb = &mut s.max_speed_in_mb;
        egui::ComboBox::from_id_salt("speed_unit_combo").width(70.0).selected_text(if *in_mb { "MB/s" } else { "KB/s" }).show_ui(
            ui,
            |ui| {
                ui.selectable_value(in_mb, false, "KB/s");
                ui.selectable_value(in_mb, true, "MB/s");
            },
        );
        ui.add(egui::DragValue::new(&mut s.max_speed).range(0.0..=1_000_000.0).speed(1.0).max_decimals(1));
    });
    text_setting(ui, "Proxy", "Every connection goes through it.", &mut s.proxy, "http://127.0.0.1:8080 or socks5://127.0.0.1:1080", false);
    let about = "Several proxies, comma-separated; connections take turns over them.";
    text_setting(ui, "Proxy pool", about, &mut s.proxy_pool, "http://p1:8080, http://p2:8080", false);
    let about = "Connect over IPv4 or IPv6 only, or whichever works (Any). Videos yt-dlp fetches take either.";
    setting(ui, "IP version", about, s.ip_family, |ui| {
        for (value, name) in [(IpFamily::V6, "IPv6"), (IpFamily::V4, "IPv4"), (IpFamily::Any, "Any")] {
            ui.selectable_value(&mut s.ip_family, value, name);
        }
    });
    let about = "How long opening a connection may take: DNS, TCP and TLS. Not for videos yt-dlp fetches.";
    setting(ui, "Connect timeout", about, s.connect_timeout_secs, |ui| {
        ui.add(egui::DragValue::new(&mut s.connect_timeout_secs).range(1..=600).suffix(" s"));
    });
    let about = "Sent in place of the browser one the app sends (and the browser's own, from the extension), video sites included.";
    text_setting(ui, "User agent", about, &mut s.user_agent, "Empty: the app's", false);
    let about = "Spread a download's connections over the addresses its server has (a CDN often has several, one of them busy),                  dropping one that fails or stays far slower. Not through a proxy.";
    switch(ui, "Use the server's addresses", about, &mut s.spread_addresses);
    let about = "Spread connections over every network this computer is on (Wi-Fi and Ethernet, say), or the ones chosen below.";
    switch(ui, "Use several networks", about, &mut s.multi_network);
    ui.add_enabled_ui(s.multi_network, |ui| network_choice(ui, &mut s.bind_addresses));
}

/// The local addresses to connect from: the usable networks (looked up once, and again on
/// request) to tick, and the addresses as text.
fn network_choice(ui: &mut Ui, chosen: &mut Vec<String>) {
    let p = palette(ui);
    let id = egui::Id::new("usable_networks");
    let found = ui.data(|d| d.get_temp::<Vec<(String, String)>>(id)).unwrap_or_else(|| {
        let found: Vec<(String, String)> =
            hyperfetch_core::netif::usable().into_iter().map(|n| (n.name, n.address.to_string())).collect();
        ui.data_mut(|d| d.insert_temp(id, found.clone()));
        found
    });
    setting(ui, "Networks to use", "None ticked: every usable network.", egui::Id::new(&*chosen), |ui| {
        if icon_button(ui, icon::ARROWS_CLOCKWISE, "Look for networks again", true).clicked() {
            ui.data_mut(|d| d.remove::<Vec<(String, String)>>(id));
        }
        ui.vertical(|ui| {
            if found.is_empty() {
                ui.label(RichText::new("No usable network found").color(p.muted));
            }
            for (name, address) in &found {
                let mut on = chosen.iter().any(|c| c.trim() == address);
                if ui.checkbox(&mut on, format!("{} ({})", name, address)).changed() {
                    if on {
                        chosen.push(address.clone());
                    } else {
                        chosen.retain(|c| c.trim() != address);
                    }
                }
            }
        });
    });
    setting(ui, "Addresses", "The local IP addresses to connect from, comma-separated.", egui::Id::new(&*chosen), |ui| {
        // Split and joined on the same comma, so typing is never undone.
        let mut text = chosen.join(",");
        let edit = egui::TextEdit::singleline(&mut text).hint_text("192.168.1.20, 10.0.0.5").desired_width(ui.available_width());
        if ui.add(edit).changed() {
            *chosen = if text.trim().is_empty() { Vec::new() } else { text.split(',').map(str::to_string).collect() };
        }
    });
    if let Some(bad) = chosen.iter().map(|a| a.trim()).find(|a| !a.is_empty() && a.parse::<std::net::IpAddr>().is_err()) {
        error_alert(ui, &format!("{} is not an IP address; it is left out", bad));
    }
}

fn torrent_settings(app: &mut App, ui: &mut Ui) {
    let s = &mut app.settings;
    let about = "Magnet links and .torrent files without web seeds download from the BitTorrent swarm.";
    // Turned off: no torrent goes on seeding, listening or forwarding a port.
    if switch(ui, "Download from peers", about, &mut s.p2p) && !s.p2p {
        app.rt.spawn(hyperfetch_core::p2p::shutdown());
    }
    ui.add_enabled_ui(s.p2p, |ui| {
        let about = "Keep sharing a finished torrent until it has uploaded this many times its size (0 = don't seed).";
        setting(ui, "Seed ratio", about, s.seed_ratio.to_bits(), |ui| {
            ui.add(egui::DragValue::new(&mut s.seed_ratio).range(0.0..=100.0).speed(0.05).max_decimals(2));
        });
        setting(ui, "Seed time limit", "Stop sharing after this long, whichever comes first (0 = no limit).", s.seed_minutes, |ui| {
            ui.add(egui::DragValue::new(&mut s.seed_minutes).range(0..=100_000).suffix(" min"));
        });
        let about = "The port peers connect to (0 = the default range). Port and forwarding changes apply once no torrent is running.";
        setting(ui, "Listen port", about, s.bt_port, |ui| {
            ui.add(egui::DragValue::new(&mut s.bt_port).range(0..=65535));
        });
        let about = "Ask the router to forward that port, so more peers can reach this computer.";
        switch(ui, "Port forwarding (UPnP)", about, &mut s.bt_upnp);
    });
}

/// Debrid services by the name the settings keep ("" = auto-detect).
const DEBRID_SERVICES: [(&str, &str); 5] = [
    ("", "Auto-detect"),
    ("realdebrid", "Real-Debrid"),
    ("alldebrid", "AllDebrid"),
    ("torbox", "TorBox"),
    ("premiumize", "Premiumize"),
];

fn debrid_combo(ui: &mut Ui, provider: &mut String) {
    // Older versions saved "real-debrid".
    let current = provider.trim().to_lowercase().replace('-', "");
    let shown = DEBRID_SERVICES.iter().find(|(key, _)| *key == current).map_or_else(|| provider.clone(), |(_, name)| name.to_string());
    egui::ComboBox::from_id_salt("debrid_provider_combo").selected_text(shown).show_ui(ui, |ui| {
        for (key, name) in DEBRID_SERVICES {
            if ui.selectable_label(current == key, name).clicked() {
                *provider = key.to_string();
            }
        }
    });
}

fn host_settings(app: &mut App, ui: &mut Ui) {
    let about = "Unlocks hoster links, and magnets, at full speed through your account.";
    setting(ui, "Debrid service", about, egui::Id::new(&app.settings.debrid_provider), |ui| debrid_combo(ui, &mut app.settings.debrid_provider));
    let about = "Saved in the settings only, never with the queue or in the history.";
    text_setting(ui, "Debrid API key", about, &mut app.settings.debrid_api_key, "Real-Debrid, AllDebrid, TorBox or Premiumize key", true);
    let about = "With a key set, magnet links go to the debrid service first; a torrent it has cached arrives at once.";
    switch(ui, "Magnets through debrid", about, &mut app.settings.debrid_magnets);
    let about = "Lists whole Google Drive folders with sizes and checksums. Sent only to Google's Drive API and kept until \
                 the app closes; set ENDO_GOOGLE_API_KEY to have it at every launch. Without one a folder is read from its \
                 public page, which has no sizes or checksums.";
    text_setting(ui, "Google API key", about, &mut app.settings.google_api_key, "Optional", true);
    setting(ui, "Browser cookies", "Sign in to sites as in this browser, for videos and folders that need it.", app.settings.browser_cookies, |ui| {
        combo(ui, "browser_cookies_combo", &mut app.settings.browser_cookies, &BROWSERS);
    });
    setting(ui, "Cookies file", "A Netscape cookies.txt, for sites that need you signed in.", egui::Id::new(&app.settings.cookies_path), |ui| {
        if ui.add_enabled(!app.dialog_open, button("Browse…")).clicked() {
            app.pending_dialog = Some(Dialog::CookiesFile);
        }
        let edit = egui::TextEdit::singleline(&mut app.settings.cookies_path).hint_text("Optional").desired_width(ui.available_width());
        ui.add(edit);
    });
    let about = "Sent as the Referer header, for hosts that check where a download comes from.";
    text_setting(ui, "Referer", about, &mut app.settings.referer, "https://example.com/", false);
}

fn media_settings(app: &mut App, ui: &mut Ui) {
    setting(ui, "Quality", "For videos and audio from sites like YouTube.", app.settings.media_preset, |ui| {
        combo(ui, "media_preset_combo", &mut app.settings.media_preset, &MEDIA_PRESETS);
    });
    let s = &mut app.settings;
    setting(ui, "Video format", "The container a video's picture and sound are merged into.", s.merge_format, |ui| {
        combo(ui, "merge_format_combo", &mut s.merge_format, &crate::settings::MERGE_FORMATS);
    });
    let about = "Sponsor, self-promotion and subscribe reminder segments of YouTube videos, as SponsorBlock's users mark them.";
    setting(ui, "SponsorBlock", about, s.sponsorblock, |ui| {
        combo(ui, "sponsorblock_combo", &mut s.sponsorblock, &crate::settings::SPONSORBLOCK_MODES);
    });
    let about = "Languages to save a video's subtitles in, as .srt or .vtt files next to it: the site's own, else its automatic captions. \
                 An HLS or DASH stream takes its subtitles tracks in them (empty: all of them; none: none).";
    text_setting(ui, "Subtitles", about, &mut s.subtitles, "en,es or all", false);
    let about = "Title, artist, date, description, link and chapters inside the file (needs ffmpeg).";
    switch(ui, "Embed tags and chapters", about, &mut s.embed_metadata);
    let about = "Where the site keeps it, from its start instead of from now. Stop finishes the recording and keeps it.";
    switch(ui, "Record live streams from the start", about, &mut s.live_from_start);
    let about = "A stream or premiere that has not begun is waited for, checking every 1 to 10 minutes. Stop ends the wait.";
    switch(ui, "Wait for scheduled streams", about, &mut s.wait_for_video);
    let about = "An HLS (m3u8) stream is remuxed into an MP4 without re-encoding; needs ffmpeg, without it the .ts is kept.";
    switch(ui, "Convert HLS streams to MP4", about, &mut s.hls_to_mp4);
    let about = if cfg!(target_os = "macos") {
        "Imgur, Pixiv, DeviantArt, ArtStation, Flickr, Tumblr, Pinterest, boorus, Bluesky and Reddit galleries; \
         needs gallery-dl (brew install gallery-dl)."
    } else {
        "Imgur, Pixiv, DeviantArt, ArtStation, Flickr, Tumblr, Pinterest, boorus, Bluesky and Reddit galleries; \
         gallery-dl is installed when first needed."
    };
    switch(ui, "Image galleries with gallery-dl", about, &mut s.gallery_dl);
    let about = "Leave out the videos, tracks, episodes and files of a playlist, channel, feed or folder downloaded before.";
    switch(ui, "Only new playlist items", about, &mut s.only_new);
    setting(ui, "Newest items only", "Only this many of a channel's, playlist's or feed's newest items (0 = all).", s.latest, |ui| {
        ui.add(egui::DragValue::new(&mut s.latest).range(0..=100_000));
    });
    let mut install = app.settings.install_ffmpeg == Some(true);
    if switch(ui, "Install ffmpeg when a video needs it", FFMPEG_ABOUT, &mut install) {
        app.answer_ffmpeg(install);
    }
}

/// What HLS and DASH downloads take when their tracks are not chosen (see `Choose tracks…`), what
/// they leave out, how live recordings end and how stream files are named. A value the engine
/// would refuse says so and is left out.
fn stream_settings(s: &mut Settings, ui: &mut Ui) {
    let about = "The video an HLS or DASH stream's download takes: the best at most this tall, in this codec and range.";
    setting(ui, "Video quality", about, (s.stream_height, s.stream_codec, s.stream_range), |ui| {
        combo(ui, "stream_range_combo", &mut s.stream_range, &VIDEO_RANGES.map(|(name, _)| name));
        combo(ui, "stream_codec_combo", &mut s.stream_codec, &VIDEO_CODECS.map(|(name, _)| name));
        combo(ui, "stream_height_combo", &mut s.stream_height, &VIDEO_HEIGHTS.map(|(name, _)| name));
    });
    let about = "The audio tracks it takes, the best of each language in this order (all: every language). Empty: the default track.";
    text_setting(ui, "Audio languages", about, &mut s.audio_languages, "en, ja", false);
    if let Err(e) = AudioSpec::parse(&s.audio_languages) {
        error_alert(ui, &format!("{}; it is left out", e));
    }
    let about = "Segments whose links hold one of these, comma-separated (* for any run of characters), are left out of every track, \
                 as the ad breaks a playlist marks are.";
    text_setting(ui, "Ad link patterns", about, &mut s.ad_patterns, "ad*.ts, /ads/", false);
    let about = "A live recording ends after this long and is kept, a channel's too (0 = until the stream ends or you stop it).";
    setting(ui, "Stop live recordings after", about, s.record_minutes, |ui| {
        ui.add(egui::DragValue::new(&mut s.record_minutes).range(0..=100_000).suffix(" min"));
    });
    let about = "A live HLS or DASH recording, or a channel's, ends and is kept once nothing new came for this long (0 = never).";
    setting(ui, "Stop when idle for", about, s.live_idle_timeout, |ui| {
        ui.add(egui::DragValue::new(&mut s.live_idle_timeout).range(0..=86_400).suffix(" s"));
    });
    let variables: Vec<String> = hyperfetch_core::NAME_VARIABLES.iter().map(|name| format!("{{{}}}", name)).collect();
    let about = format!("How a stream's file is named, its extension added (empty: as it would be). Variables: {}.", variables.join(" "));
    text_setting(ui, "File name pattern", &about, &mut s.name_pattern, "{name} {height}p {audio}", false);
    if let Err(e) = Some(s.name_pattern.as_str()).filter(|pattern| !pattern.trim().is_empty()).map_or(Ok(()), hyperfetch_core::check_name_pattern) {
        error_alert(ui, &format!("{}; it is left out", e));
    }
}

fn post_settings(app: &mut App, ui: &mut Ui) {
    let s = &mut app.settings;
    ui.add_enabled_ui(cfg!(any(windows, target_os = "macos")), |ui| {
        let about = if cfg!(target_os = "macos") {
            "macOS then asks before opening a downloaded program, as it does for browser downloads."
        } else {
            "Windows then asks before running a downloaded program, as it does for browser downloads."
        };
        switch(ui, "Mark as downloaded from the internet", about, &mut s.mark_of_the_web);
    });
    let about = "When the site publishes the file's checksum (a .sha256 file next to it, say) and you gave none, a \
                 download that does not match it fails; one that does says so.";
    switch(ui, "Check against the site's checksum", about, &mut s.remote_checksum);
    let about = "A finished file is dated as on the server, not when it was downloaded.";
    switch(ui, "Keep the server's file date", about, &mut s.remote_time);
    switch(ui, "Unpack archives", "Zip, 7z, rar and tar files unpack into a folder next to them.", &mut s.auto_extract);
    ui.add_enabled_ui(s.auto_extract, |ui| {
        switch(ui, "Delete archives after unpacking", "Only once everything is unpacked.", &mut s.delete_archives);
    });
    let about = "Single files move into the folder of the first category below that takes their extension.";
    switch(ui, "Sort into folders", about, &mut s.sort_downloads);
    ui.add_enabled_ui(s.sort_downloads, |ui| category_settings(app, ui));
    let s = &mut app.settings;
    let about = "Sends the file's SHA-256 only, never the file, and notes how many engines flag it.";
    switch(ui, "Look files up on VirusTotal", about, &mut s.virustotal_check);
    ui.add_enabled_ui(s.virustotal_check, |ui| {
        let about = "Saved in the settings only, never with the queue or in the history.";
        text_setting(ui, "VirusTotal API key", about, &mut s.virustotal_api_key, "Your VirusTotal key", true);
    });
    let about = "A program and its arguments, run without a shell after each download. {path}, {dir}, {name} and {url} are filled in.";
    let hint = if cfg!(windows) { "e.g. C:\\Tools\\scan.exe \"{path}\"" } else { "e.g. /usr/local/bin/scan \"{path}\"" };
    text_setting(ui, "Run after each download", about, &mut s.run_after, hint, false);
}

/// The categories sorting uses, a line each: its name, its extensions (comma-separated) and its
/// folder (empty: the folder of its name in the save folder; a picked or typed one replaces it),
/// then Add category (a new one goes first) and Reset to the built-in ones. The first that takes
/// an extension wins.
fn category_settings(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let (mut delete, dialog_open) = (None, app.dialog_open);
    let categories = &mut app.settings.categories;
    for (n, category) in categories.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut category.name).hint_text("Name").desired_width(110.0));
            // Split and joined on the same ", ", so typing is never undone.
            let mut extensions = category.extensions.join(", ");
            if ui.add(egui::TextEdit::singleline(&mut extensions).hint_text("mp4, mkv").desired_width(230.0)).changed() {
                category.extensions = extensions.split(", ").map(str::to_string).collect();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, icon::TRASH, "Delete this category", true).clicked() {
                    delete = Some(n);
                }
                if icon_button(ui, icon::FOLDER_OPEN, "Choose its folder", !dialog_open).clicked() {
                    app.pending_dialog = Some(Dialog::CategoryFolder(n));
                }
                let mut folder = category.folder.as_ref().map(|f| f.display().to_string()).unwrap_or_default();
                let hint = format!("{} in the save folder", category.name.trim());
                if ui.add(egui::TextEdit::singleline(&mut folder).hint_text(hint).desired_width(ui.available_width())).changed() {
                    category.folder = (!folder.trim().is_empty()).then(|| PathBuf::from(folder));
                }
            });
        });
    }
    if let Some(n) = delete {
        categories.remove(n);
    }
    ui.horizontal(|ui| {
        // At the top, so it takes its extensions from the ones below.
        if ui.add(button(format!("{} Add category", icon::PLUS))).clicked() {
            categories.insert(0, Default::default());
        }
        if ui.add(button(format!("{} Reset", icon::ARROW_COUNTER_CLOCKWISE))).on_hover_text("Back to the built-in categories").clicked() {
            *categories = hyperfetch_core::postprocess::default_categories();
        }
        if categories.is_empty() {
            ui.label(RichText::new("No categories: nothing is sorted").small().color(p.muted));
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::super::harness::{Harness, TOGGLE};
    use super::*;

    /// The Desktop section's switches set their settings; launch at startup goes through the
    /// app's writer and is on once written; the schedule takes its times as typed and its days
    /// as ticked, and says when a time is none.
    #[test]
    fn desktop_settings_work_through_input() {
        let mut h = Harness::new();
        let written = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&written);
        h.app.writer = Box::new(move |place, value| {
            lock(&log).push((place.clone(), value.map(str::to_string)));
            Ok(())
        });
        h.app.tab = Tab::Settings;
        assert!(h.settle());
        // The section's first row brought up to the top of the page.
        let top = h.find("Close to the tray").unwrap().top();
        h.hover(h.find("Desktop").unwrap().center());
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, 100.0 - top), modifiers: egui::Modifiers::NONE };
        h.frame(vec![wheel]);
        h.frames(60);
        h.leave();
        assert!(h.settle());
        assert!(h.find("Data folder").is_some(), "{:?}", h.texts());
        // The pointer goes there first: a press that comes with a move drags the page.
        let click = |h: &mut Harness, at: Pos2| {
            h.hover(at);
            h.click_at(at);
            h.frames(30);
        };
        let switch = |h: &mut Harness, title: &str| {
            let knob = h.find_near(title, TOGGLE).unwrap_or_else(|| panic!("{title}")).center();
            click(h, knob);
        };
        let press = |h: &mut Harness, label: &str| {
            let at = h.find(label).unwrap_or_else(|| panic!("{label} in {:?}", h.texts())).center();
            click(h, at);
        };
        switch(&mut h, "Close to the tray");
        switch(&mut h, "Start minimized");
        switch(&mut h, "Launch at startup");
        let s = &h.app.settings;
        assert!(s.close_to_tray && s.start_minimized && s.launch_at_startup);
        assert_eq!(lock(&written).iter().map(|(_, value)| value.is_some()).collect::<Vec<_>>(), [true]);

        switch(&mut h, "Schedule the queue");
        assert!(h.app.settings.schedule.enabled);
        press(&mut h, "Sun");
        assert_eq!(h.app.settings.schedule.days, [true, true, true, true, true, true, false]);
        press(&mut h, "Start");
        h.type_text("7:30");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        press(&mut h, "Stop");
        h.type_text("25:00");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert_eq!((h.app.settings.schedule.start.as_deref(), h.app.settings.schedule.stop.as_deref()), (Some("7:30"), Some("25:00")));
        assert!(h.settle());
        assert!(h.find("25:00 is not a time like 08:30; it is left out").is_some());
    }

    /// Scrolls the Settings page with the wheel until `title` shows near its top.
    fn scroll_to(h: &mut Harness, title: &str) {
        h.hover(Pos2::new(600.0, 400.0));
        for _ in 0..40 {
            let top = h.find(title).map(|rect| rect.top());
            if top.is_some_and(|top| (80.0..=200.0).contains(&top)) {
                break;
            }
            let delta = Vec2::new(0.0, top.map_or(-400.0, |top| 120.0 - top));
            h.frame(vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, modifiers: egui::Modifiers::NONE }]);
            h.frames(30);
        }
        h.leave();
        assert!(h.settle() && h.find(title).is_some(), "{title}");
    }

    /// A click on what `find` finds, the pointer there first: a press that comes with a move drags
    /// the page.
    fn click(h: &mut Harness, find: impl Fn(&Harness) -> Option<Rect>) {
        let at = find(h).unwrap_or_else(|| panic!("not shown: {:?}", h.texts())).center();
        h.hover(at);
        h.click_at(at);
        h.frames(30);
    }

    /// The connection rows and the checks before sorting set their settings through input.
    #[test]
    fn new_rows_work_through_input() {
        let mut h = Harness::new();
        h.app.tab = Tab::Settings;
        assert!(h.settle());
        scroll_to(&mut h, "Smarter connection count");
        click(&mut h, |h| h.find_near("Smarter connection count", TOGGLE));
        click(&mut h, |h| h.find_near("IP version", "IPv6"));
        click(&mut h, |h| h.find("Empty: the app's"));
        h.type_text("Agent/2.0");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        let s = &h.app.settings;
        assert!(!s.adaptive && s.ip_family == IpFamily::V6 && s.user_agent == "Agent/2.0", "on by default, so off: {s:?}");
        assert!(h.find("Connect timeout").is_some());
        scroll_to(&mut h, "Use the server's addresses");
        click(&mut h, |h| h.find_near("Use the server's addresses", TOGGLE));
        assert!(!h.app.settings.tuning().spread_addresses, "on by default, so off");

        scroll_to(&mut h, "Check against the site's checksum");
        click(&mut h, |h| h.find_near("Check against the site's checksum", TOGGLE));
        click(&mut h, |h| h.find_near("Keep the server's file date", TOGGLE));
        assert!(!h.app.settings.remote_checksum && h.app.settings.remote_time);
    }

    /// The Streams rows set their settings through input: the video quality from its three
    /// choices, the audio languages, ad patterns and file-name pattern as typed, the variables
    /// listed; a value the engine would refuse says so until it is put right.
    #[test]
    fn stream_rows_work_through_input() {
        let mut h = Harness::new();
        h.app.tab = Tab::Settings;
        assert!(h.settle());
        scroll_to(&mut h, "Video quality");
        click(&mut h, |h| h.find_near("Video quality", "Best"));
        click(&mut h, |h| h.find("1080p"));
        click(&mut h, |h| h.find_near("Video quality", "Any codec"));
        click(&mut h, |h| h.find("HEVC (H.265)"));
        assert_eq!(h.app.settings.video_spec(), "1080p,hevc");
        assert!(h.texts().iter().any(|(text, _)| text.contains("{name} {height} {width}") && text.contains("{channel} {date} {time}")), "{:?}", h.texts());

        let refused = |h: &Harness, start: &str| h.texts().iter().any(|(text, _)| text.starts_with(start) && text.ends_with("it is left out"));
        click(&mut h, |h| h.find("en, ja"));
        h.type_text("en, ja, english!");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle() && refused(&h, "Unknown audio term \"english!\""), "{:?}", h.texts());
        click(&mut h, |h| h.find("{name} {height}p {audio}"));
        h.type_text("{name} [{heigth}p]");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle() && refused(&h, "Unknown name variable {heigth}"), "{:?}", h.texts());
        click(&mut h, |h| h.find("ad*.ts, /ads/"));
        h.type_text("/ads/, *promo*.ts");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        let s = &mut h.app.settings;
        assert_eq!((s.audio_languages.as_str(), s.ad_patterns.as_str()), ("en, ja, english!", "/ads/, *promo*.ts"));
        (s.audio_languages, s.name_pattern) = ("en, ja".into(), "{name} [{height}p]".into());
        assert!(h.settle() && !h.texts().iter().any(|(text, _)| text.ends_with("it is left out")), "put right");
        let file = [url::Url::parse("https://example.com/v/master.m3u8").unwrap()];
        let opts = h.app.settings.download_options(&file, "", "").unwrap();
        assert_eq!((opts.tracks.video.as_str(), opts.tracks.audio.as_str(), opts.name_pattern.as_deref()), ("1080p,hevc", "en, ja", Some("{name} [{height}p]")));
        assert_eq!(opts.ad_patterns, ["/ads/", "*promo*.ts"]);
    }

    /// The categories under Sort into folders are edited in place: one deleted, a new one (first,
    /// so it takes .pdf from Documents) named, given extensions and a folder of its own; a download
    /// then sorts into it; Reset brings back the built-in ones.
    #[test]
    fn categories_are_edited_and_sort_downloads() {
        let server = tokio::runtime::Runtime::new().unwrap();
        let addr = server.block_on(crate::tests::serve(Arc::new(vec![b'%'; 4096])));
        let elsewhere = tempfile::tempdir().unwrap();
        let mut h = Harness::new();
        h.app.tab = Tab::Settings;
        assert!(h.settle());
        scroll_to(&mut h, "Sort into folders");
        click(&mut h, |h| h.find_near("Sort into folders", TOGGLE));
        assert!(h.app.settings.sort_downloads && h.settle());
        click(&mut h, |h| h.find_near("Music", icon::TRASH));
        assert!(h.app.settings.categories.iter().all(|c| c.name != "Music") && h.app.settings.categories.len() == 5);
        click(&mut h, |h| h.find("Add category"));
        assert!(h.settle());
        click(&mut h, |h| h.find("Name"));
        h.type_text("Papers");
        click(&mut h, |h| h.find("mp4, mkv"));
        h.type_text("PDF, .Epub");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle() && h.find("Papers in the save folder").is_some(), "{:?}", h.texts());
        click(&mut h, |h| h.find_near("Papers", icon::FOLDER_OPEN));
        assert!(matches!(h.app.pending_dialog, Some(Dialog::CategoryFolder(0))), "{:?}", h.app.pending_dialog);
        h.app.pending_dialog = None;
        h.app.handle_event(crate::AppEvent::Picked(Dialog::CategoryFolder(0), Some(elsewhere.path().to_path_buf())));
        assert!(h.settle() && h.find(&elsewhere.path().display().to_string()).is_some(), "{:?}", h.texts());
        let papers = &h.app.settings.sort_categories()[0];
        assert_eq!((papers.name.as_str(), papers.extensions.as_slice()), ("Papers", &["pdf".to_string(), "epub".to_string()][..]));

        h.app.settings.remote_checksum = false;
        let id = h.app.add_download(&format!("http://{addr}/report.pdf"), "", "").unwrap();
        assert!(h.app.start_job(id, false));
        h.wait_for(|app| app.queue.get_item(id).is_some_and(|item| !item.status.is_active()));
        let item = h.app.queue.get_item(id).unwrap();
        assert_eq!(item.target_path.as_deref(), Some(elsewhere.path().join("report.pdf").as_path()), "{:?}", item.status);
        assert_eq!(std::fs::read(elsewhere.path().join("report.pdf")).unwrap().len(), 4096);

        click(&mut h, |h| h.find("Reset"));
        assert_eq!(h.app.settings.categories, hyperfetch_core::postprocess::default_categories());
    }

    /// A setting changed on the open page lights its row up for a moment, after which frames
    /// stop; one changed elsewhere does not light up when the page opens.
    #[test]
    fn a_changed_setting_flashes_its_row() {
        let mut h = Harness::new();
        h.app.tab = Tab::Settings;
        assert_eq!(h.frames(40), Duration::MAX, "the cards are in");
        let lit = |h: &Harness| flash(&h.app.ctx, "Downloads at once", h.app.settings.max_concurrent);
        assert_eq!(lit(&h), 0.0);

        h.app.settings.max_concurrent = 7;
        h.frames(2);
        assert!(lit(&h) > 0.9, "lit up");
        assert_eq!(h.frames(60), Duration::MAX, "and then idle");
        assert_eq!(lit(&h), 0.0);

        h.app.tab = Tab::Queue;
        h.frames(2);
        h.app.settings.max_concurrent = 2;
        h.app.tab = Tab::Settings;
        h.frames(2);
        assert_eq!(lit(&h), 0.0, "changed while away");
    }
}
