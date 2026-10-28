//! The track chooser of an HLS or DASH download (see `crate::tracks`), on the Add page: its
//! tracks, parts and time range, or for a live stream where its recording starts and stops.

use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::{StreamInfo, TrackKind};

use super::*;
use crate::tracks::{self, Picks, Start};

/// The chooser open in `app`, where the caller shows it: a spinner while the stream is read, then
/// what it holds to tick (Save tracks for a queued download, Download for the form's link), or
/// why it could not be read.
pub(super) fn chooser(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let Some(mut chooser) = app.tracks.take() else { return };
    ui.add_space(6.0);
    // Some(true): use the picks; Some(false): close.
    let mut choice = None;
    Frame::none().stroke(Stroke::new(1.0, p.border)).rounding(8.0).inner_margin(12.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        match &chooser.info {
            None => {
                ui.horizontal(|ui| {
                    anim::spinner(ui, 14.0, p.muted);
                    ui.label(RichText::new("Reading the stream's tracks…").color(p.muted));
                });
            }
            Some(Err(error)) => {
                error_alert(ui, error);
                if ui.add(button("Close")).clicked() {
                    choice = Some(false);
                }
            }
            Some(Ok(info)) => choice = picks(ui, info, &mut chooser.picks, chooser.item.is_some()),
        }
    });
    let saves = chooser.item.is_some();
    app.tracks = Some(chooser).filter(|_| choice != Some(false));
    match choice {
        Some(true) if saves => app.save_tracks(),
        Some(true) => super::add::start_from_form(app),
        _ => {}
    }
}

/// `picks` of `info` to tick and type; whether a button was clicked: Some(true) to use them
/// (`saves`: Save tracks, else Download), Some(false) to cancel.
fn picks(ui: &mut Ui, info: &StreamInfo, picks: &mut Picks, saves: bool) -> Option<bool> {
    let p = palette(ui);
    let heading = |ui: &mut Ui, text: &str| {
        ui.add_space(4.0);
        ui.label(bold(text).color(p.strong));
    };
    let of = |kind: TrackKind| info.tracks.iter().enumerate().filter(move |(_, track)| track.kind == kind);
    if of(TrackKind::Video).next().is_some() {
        heading(ui, "Video");
        for (at, track) in of(TrackKind::Video) {
            ui.radio_value(&mut picks.video, Some(at), tracks::label(track));
        }
        ui.radio_value(&mut picks.video, None, "No video");
    }
    for (kind, title, ticked) in [(TrackKind::Audio, "Audio", &mut picks.audio), (TrackKind::Subtitles, "Subtitles", &mut picks.subtitles)] {
        // A live recording takes no subtitles (see `Picks::of`).
        if of(kind).next().is_none() || (info.live && kind == TrackKind::Subtitles) {
            continue;
        }
        heading(ui, title);
        for (at, track) in of(kind) {
            let mut on = ticked.contains(&at);
            if ui.checkbox(&mut on, tracks::label(track)).changed() {
                ticked.retain(|&other| other != at);
                if on {
                    ticked.push(at);
                }
            }
        }
    }
    if info.live {
        heading(ui, "Live stream");
        ui.horizontal_wrapped(|ui| {
            ui.label("Start from");
            ui.selectable_value(&mut picks.start, Start::Now, "Now");
            ui.selectable_value(&mut picks.start, Start::Oldest, "The start");
            ui.selectable_value(&mut picks.start, Start::MinutesAgo, "Minutes ago");
            ui.add_enabled(picks.start == Start::MinutesAgo, egui::DragValue::new(&mut picks.minutes_ago).range(1..=10_080).suffix(" min"));
            ui.selectable_value(&mut picks.start, Start::At, "At");
            let at = egui::TextEdit::singleline(&mut picks.at).hint_text("2026-10-28 21:00").desired_width(130.0);
            ui.add_enabled(picks.start == Start::At, at);
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Stop after");
            ui.add(egui::DragValue::new(&mut picks.stop_after).range(0..=100_000).suffix(" min"));
            ui.label("or when idle for");
            ui.add(egui::DragValue::new(&mut picks.idle).range(0..=86_400).suffix(" s"));
            ui.label(RichText::new("0: no limit").small().color(p.muted));
        });
    } else {
        if info.parts.len() > 1 {
            heading(ui, "Parts");
            let row = ui.spacing().interact_size.y;
            let most = 8.0 * (row + ui.spacing().item_spacing.y);
            let scroll = egui::ScrollArea::vertical().id_salt("stream parts").max_height(most).auto_shrink([false, true]);
            scroll.show_rows(ui, row, info.parts.len(), |ui, rows| {
                for part in &info.parts[rows] {
                    let mut text = format!("Part {} · {}", part.id + 1, format_duration(part.duration as u64));
                    for said in [part.summary.as_str(), if part.ad { "looks like an ad" } else { "" }].into_iter().filter(|s| !s.is_empty()) {
                        text.push_str(" · ");
                        text.push_str(said);
                    }
                    let mut kept = !picks.dropped.contains(&part.id);
                    if ui.checkbox(&mut kept, text).changed() {
                        picks.dropped.retain(|&id| id != part.id);
                        if !kept {
                            picks.dropped.push(part.id);
                        }
                    }
                }
            });
        }
        heading(ui, "Time range");
        ui.horizontal_wrapped(|ui| {
            ui.label("From");
            ui.add(egui::TextEdit::singleline(&mut picks.from).hint_text("the start").desired_width(80.0));
            ui.label("to");
            ui.add(egui::TextEdit::singleline(&mut picks.to).hint_text("the end").desired_width(80.0));
            ui.label(RichText::new("e.g. 1:30 or 90; the whole segments in it are kept").small().color(p.muted));
        });
    }
    let checked = picks.apply(info, &mut DownloadOptions::default());
    if let Err(e) = &checked {
        error_alert(ui, e);
    }
    ui.add_space(6.0);
    let mut choice = None;
    ui.horizontal(|ui| {
        let label = if saves { "Save tracks" } else { "Download with these tracks" };
        if ui.add_enabled(checked.is_ok(), primary_button(label, p.accent)).clicked() {
            choice = Some(true);
        }
        if ui.add(button("Cancel")).clicked() {
            choice = Some(false);
        }
    });
    choice
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::super::harness::Harness;
    use super::*;

    /// A server of `files` (path, body), each answer of fixed length, 404 for any other path.
    async fn serve_files(files: Vec<(&'static str, String)>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let files = Arc::new(files);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let files = Arc::clone(&files);
                tokio::spawn(async move {
                    let (mut request, mut buf) = (Vec::new(), [0u8; 1024]);
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let path = request.split(' ').nth(1).unwrap_or_default();
                    let (status, body) = files.iter().find(|(at, _)| *at == path).map_or(("404 Not Found", ""), |(_, body)| ("200 OK", body.as_str()));
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(head.as_bytes()).await;
                    if !request.starts_with("HEAD") {
                        let _ = socket.write_all(body.as_bytes()).await;
                    }
                });
            }
        });
        addr
    }

    /// An HLS stream of two videos, English and Japanese audio and Brazilian subtitles, in two
    /// parts of 10 and 4 seconds.
    fn hls_stream() -> Vec<(&'static str, String)> {
        let master = [
            "#EXTM3U",
            r#"#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",LANGUAGE="en",NAME="English",DEFAULT=YES,URI="en.m3u8""#,
            r#"#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",LANGUAGE="ja",NAME="Japanese",URI="ja.m3u8""#,
            r#"#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",LANGUAGE="pt-BR",NAME="Brazilian",URI="subs.m3u8""#,
            r#"#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2",AUDIO="aud",SUBTITLES="subs""#,
            "hi.m3u8",
            r#"#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="aud",SUBTITLES="subs""#,
            "lo.m3u8",
        ]
        .join("\n");
        let media = |ext: &str| format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\na.{ext}\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\nb.{ext}\n#EXT-X-ENDLIST\n");
        vec![
            ("/v/master.m3u8", master),
            ("/v/hi.m3u8", media("ts")),
            ("/v/lo.m3u8", media("ts")),
            ("/v/en.m3u8", media("aac")),
            ("/v/ja.m3u8", media("aac")),
            ("/v/subs.m3u8", media("vtt")),
        ]
    }

    /// Where the last frame shows a text starting with `start`.
    fn starting(h: &Harness, start: &str) -> Rect {
        h.texts().into_iter().find(|(text, _)| text.starts_with(start)).map(|(_, rect)| rect).unwrap_or_else(|| panic!("{start} in {:?}", h.texts()))
    }

    /// An HLS link typed in the form offers Choose tracks…, which reads the stream (from a local
    /// server) and lists its videos, audio and subtitles (languages in words) and parts, ticked as
    /// the Settings take them; what is ticked and typed goes with the download, once right. Its
    /// queue row offers the chooser again, which opens on its page as it was saved; Save keeps
    /// the new choice with the download, also in the saved queue.
    #[test]
    fn tracks_are_chosen_in_the_form_and_from_a_queue_row() {
        let server = tokio::runtime::Runtime::new().unwrap();
        let addr = server.block_on(serve_files(hls_stream()));
        let link = format!("http://{addr}/v/master.m3u8");
        let mut h = Harness::new();
        h.app.engines = Arc::new(|_, _| Err("not downloaded here".to_string()));
        assert!(h.settle());
        assert!(h.find("Choose tracks…").is_none());
        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.type_text(&link);
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle());
        h.click("Choose tracks…");
        h.wait_for(|app| app.tracks.as_ref().is_some_and(|chooser| chooser.info.is_some()));
        assert!(h.settle());
        for shown in ["1920x1080", "640x360", "No video", "English", "Japanese", "Portuguese (BR)", "Part 1 · 00:10", "Part 2 · 00:04", "Time range"] {
            starting(&h, shown);
        }
        let picks = h.app.tracks.as_ref().map(|chooser| chooser.picks.clone()).unwrap();
        let ticked = (picks.video, picks.audio.len(), picks.subtitles.len(), picks.dropped.len());
        assert_eq!(ticked, (Some(0), 1, 1, 0), "the best video, the default audio, every subtitles track");

        h.click_at(starting(&h, "Japanese").center());
        h.click_at(starting(&h, "Part 2").center());
        h.click("the start");
        h.type_text("x");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle());
        starting(&h, "\"x\" is not a time in the stream");
        h.click("Download with these tracks");
        assert!(h.app.queue.items().is_empty(), "nothing is added while the range is wrong");
        h.click_at(h.find("x").unwrap().center());
        h.key(egui::Key::A, egui::Modifiers::COMMAND);
        h.type_text("0:05");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle());
        h.click("Download with these tracks");
        h.frames(2);
        let item = h.app.queue.items()[0].clone();
        assert_eq!(item.options.tracks.ids, ["v0", "a0", "a1", "s0"]);
        assert_eq!((item.options.drop_parts.as_slice(), item.options.sections.first().map(|range| range.0)), (&[1][..], Some(5.0)));
        assert!(h.app.tracks.is_none() && h.app.url_input.is_empty(), "the form clears");

        // It fails here (no engine runs), and offers its chooser again.
        h.wait_for(|app| matches!(app.queue.get_item(item.id).map(|item| &item.status), Some(QueueItemStatus::Failed(_))));
        h.app.tab = Tab::Queue;
        assert!(h.settle());
        h.click_at(h.find_near(&item.filename, icon::SLIDERS_HORIZONTAL).unwrap().center());
        assert!(h.app.tab == Tab::Downloader && h.app.focused == Some(item.id));
        h.wait_for(|app| app.tracks.as_ref().is_some_and(|chooser| chooser.info.is_some()));
        assert!(h.settle());
        let reopened = h.app.tracks.as_ref().map(|chooser| chooser.picks.clone()).unwrap();
        assert_eq!((reopened.audio.len(), reopened.dropped.as_slice(), reopened.from.as_str()), (2, &[1][..], "00:05"), "as it was saved");
        h.click_at(starting(&h, "Japanese").center());
        // Its row's icon again only shows the chooser open, with what is ticked.
        h.app.tab = Tab::Queue;
        assert!(h.settle());
        h.click_at(h.find_near(&item.filename, icon::SLIDERS_HORIZONTAL).unwrap().center());
        assert!(h.settle());
        let kept = h.app.tracks.as_ref().filter(|chooser| chooser.info.is_some()).map(|chooser| chooser.picks.audio.len());
        assert_eq!((h.app.tab == Tab::Downloader, kept), (true, Some(1)));
        h.click("Save tracks");
        assert!(h.app.tracks.is_none());
        let saved = h.app.queue.get_item(item.id).unwrap().options.tracks.ids.clone();
        assert_eq!(saved, ["v0", "a0", "s0"]);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gui-queue.json");
        crate::queue_store::save(&file, h.app.queue.clone()).unwrap();
        let (restored, problem) = crate::queue_store::load(&file);
        assert_eq!((restored.get_item(item.id).map(|item| item.options.tracks.ids.clone()), problem), (Some(saved), None), "kept across a restart");
    }
}
