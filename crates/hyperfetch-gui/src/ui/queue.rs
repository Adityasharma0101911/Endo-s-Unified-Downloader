use std::collections::{HashMap, HashSet};

use hyperfetch_core::queue::{Priority, Remaining};

use super::*;
use crate::slow::{self, Fix};

const AUTH_REQUIRED: &str = "Its Authorization header was not saved: enter it again below, then Resume to continue from the partial file.";
/// What the card of a download whose link expired says (see `QueueItem::link_expired`).
const LINK_EXPIRED: &str = "Link expired: get a new link";

/// The page `item` came from, to open: the referer it was sent with, when that is a link.
fn page_of(item: &QueueItem) -> Option<&str> {
    item.options.referer.as_deref().filter(|page| page.starts_with("https://") || page.starts_with("http://"))
}

/// Why failed download `item` failed, as its card says it (a desktop notification too); None for
/// one that did not fail.
pub(crate) fn failure_text(item: &QueueItem) -> Option<String> {
    match &item.status {
        QueueItemStatus::Failed(_) if item.link_expired() => Some(LINK_EXPIRED.to_string()),
        QueueItemStatus::Failed(error) => Some(util::error_hint(error).unwrap_or(error).to_string()),
        _ => None,
    }
}

/// Whether `item` is stopped and over plain links, so that it can take a new one.
fn takes_new_link(item: &QueueItem) -> bool {
    matches!(item.status, QueueItemStatus::Paused | QueueItemStatus::Failed(_)) && item.relinkable()
}

// ---- Details --------------------------------------------------------------------------------

/// Everything about one download, calm and in reading order: its name, status and actions; its
/// progress with the pieces filling in; what holds it back; its speed over the last minute; its
/// connections and mirrors; what it was checked against; where it goes and where it came from;
/// and what happened to it. `in_place` (a narrow window): drawn on a card of its own with a way
/// back to the list; beside the list, the panel is its card.
pub(super) fn detail_page(app: &mut App, ui: &mut Ui, item: &QueueItem, in_place: bool) {
    // Done once its bytes are in (see `util::checking`).
    let shown = util::shown(item);
    let item = &*shown;
    if in_place {
        card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            detail_contents(app, ui, item, true);
        });
    } else {
        detail_contents(app, ui, item, false);
    }
}

fn detail_contents(app: &mut App, ui: &mut Ui, item: &QueueItem, in_place: bool) {
    let p = palette(ui);
    detail_header(app, ui, item, in_place);
    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| detail_actions(app, ui, item));
    // The shown download failing shakes its alert, which opens and closes its room smoothly.
    let error = match &item.status {
        QueueItemStatus::Failed(error) => Some(match util::error_hint(error) {
            Some(hint) => format!("Download failed: {}. {}", error.trim_end_matches('.'), hint),
            None => format!("Download failed: {}", error),
        }),
        _ => None,
    };
    let dx = anim::shake(ui.ctx(), egui::Id::new(("failed", item.id)), error.is_some());
    let dx = if error.is_some() { dx } else { 0.0 };
    super::add::fading(ui, &format!("failed {}", item.id), error, |ui, error| super::add::nudged(ui, dx, |ui| error_alert(ui, &error)));
    if item.status == QueueItemStatus::AuthRequired {
        auth_box(app, ui, item);
    }
    ui.add_space(14.0);
    progress_block(app, ui, item);
    ui.add_space(8.0);
    let (status, color) = status_line(app, item, &p);
    // A failure says itself in its alert above; an expired link says more here.
    if !matches!(item.status, QueueItemStatus::Failed(_)) || item.link_expired() {
        ui.add(egui::Label::new(RichText::new(status).color(color)).wrap());
    }
    if takes_new_link(item) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let open = app.new_link.as_ref().is_some_and(|new_link| new_link.id == item.id);
            let about = "Its link expired? Paste a fresh one to the same file: the download resumes where it stopped";
            if !open && ui.add(button(format!("{} New link…", icon::LINK))).on_hover_text(about).clicked() {
                app.new_link = Some(crate::NewLink::new(item.id));
            }
            if let Some(page) = page_of(item) {
                if ui.add(button(format!("{} Open the page", icon::ARROW_SQUARE_OUT))).on_hover_text(page).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(page));
                }
            }
        });
        new_link_box(app, ui, item);
    }
    // While its bytes come in, it can tell what holds it back.
    if item.status == QueueItemStatus::Downloading && !app.is_resolving(item.id) && !item.is_finishing() {
        let open = egui::Id::new(("why slow", item.id));
        ui.add_space(4.0);
        if ui.add(button(format!("{} Why is it slow?", icon::GAUGE))).clicked() {
            ui.data_mut(|d| {
                let shown = d.get_temp_mut_or_default::<bool>(open);
                *shown = !*shown;
            });
        }
        if ui.data(|d| d.get_temp::<bool>(open)).unwrap_or(false) {
            why_slow(app, ui, item);
        }
    }
    super::add::stream_tracks(app, ui, Some(item));

    let view = app.jobs.get(&item.id);
    // Once the engine has stopped, chunks it last reported as in flight are no longer being fetched.
    let live = view.is_some_and(|v| v.running.is_some());
    let chunks: &[ChunkSnapshot] = view.map_or(&[], |v| &v.chunks);
    if !chunks.is_empty() {
        let about = format!("{} pieces, each in the colour of the connection that holds it", chunks.len());
        section_title(ui, icon::PLUGS_CONNECTED, "Connections", &about);
        chunk_map(ui, item.id, chunks, item.total_bytes, live);
        ui.add_space(8.0);
        chunk_table(ui, item.id, chunks, live, item.urls.first().and_then(|url| url.host_str()).unwrap_or_default());
    }
    section_title(ui, icon::CHART_LINE, "Speed", "the last 60 seconds");
    throughput_graph(ui, item.id, view.map(|view| &view.speed_history), eased_speed(ui, item), live);
    if let Some(view) = app.jobs.get(&item.id).filter(|view| view.mirror_speeds.len() > 1) {
        section_title(ui, icon::TREE_STRUCTURE, "Mirrors", "the servers it takes pieces from, the fastest first");
        mirrors(ui, &view.mirror_speeds);
    }
    checks(ui, item);
    where_from(app, ui, item);
    timeline_view(ui, item);
    own_limits(app, ui, item);
}

/// A section of the details: its icon and title, and what it shows in a few words.
fn section_title(ui: &mut Ui, glyph: &str, title: &str, about: &str) {
    let p = palette(ui);
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(glyph).size(16.0).color(p.link));
        ui.label(bold(title).size(15.0).color(p.strong));
        if !about.is_empty() {
            ui.add(egui::Label::new(RichText::new(about).small().color(p.muted)).truncate());
        }
    });
    ui.add_space(6.0);
}

/// The way back to the list (in place) or the close button (beside it), the download's status,
/// its name (two lines at most, in full on hover), and where it comes from.
fn detail_header(app: &mut App, ui: &mut Ui, item: &QueueItem, in_place: bool) {
    let p = palette(ui);
    ui.horizontal(|ui| {
        if in_place {
            if ui.add(button(format!("{} Downloads", icon::ARROW_LEFT))).on_hover_text("Back to the list (Escape)").clicked() {
                app.focused = None;
            }
        }
        let (status, color) = status_badge(app, item, &p);
        let done = item.status == QueueItemStatus::Completed;
        status_chip(ui, egui::Id::new(("item", item.id)), status, color, item.status == QueueItemStatus::Downloading);
        if done {
            // Kept current while shown, so that the check draws itself as the download completes.
            let drawn = anim::ease(anim::changed(ui.ctx(), egui::Id::new(("completed check", item.id)), done, 0.5));
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 9.0 * anim::overshoot(drawn), p.green.gamma_multiply(0.2));
            anim::paint_check(ui.painter(), rect.shrink(3.0), drawn, Stroke::new(2.0, p.green));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if !in_place && icon_button(ui, icon::X, "Close the details (Escape)", true).clicked() {
                app.focused = None;
            }
            ui.label(RichText::new(format!("#{}", item.id)).small().color(p.dim));
        });
    });
    ui.add_space(6.0);
    let name = egui::Label::new(bold(&item.filename).size(19.0).color(p.strong)).wrap();
    ui.add(name).on_hover_text(&item.filename);
    let host = item.urls.first().and_then(|url| url.host_str()).unwrap_or_default().trim_start_matches("www.").to_string();
    let mut from = Vec::new();
    if !host.is_empty() {
        from.push(format!("from {host}"));
    }
    if item.urls.len() > 1 {
        from.push(format!("{} mirrors", item.urls.len()));
    }
    if item.priority != Priority::Normal {
        from.push(format!("{:?} priority", item.priority).to_lowercase());
    }
    if !from.is_empty() {
        ui.label(RichText::new(from.join("  ·  ")).color(p.muted));
    }
}

/// The buttons for the shown download, by its state, and New download on the right.
fn detail_actions(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let id = item.id;
    match &item.status {
        QueueItemStatus::Queued => {
            let start = if app.metered_holds(id) { "Download anyway" } else { "Start now" };
            if ui.add(primary_button(&format!("{} {start}", icon::PLAY), p.accent)).clicked() {
                app.start_job(id, false);
            }
        }
        QueueItemStatus::Downloading => {
            if ui.add(button(format!("{} Pause", icon::PAUSE))).on_hover_text("Pause it; Resume goes on from where it is (Space)").clicked() {
                app.pause_job(id);
            }
        }
        QueueItemStatus::Pausing => {
            anim::spinner(ui, 16.0, p.muted);
            let text = if app.is_recording(id) { "Stopping: finishing the recording..." } else { "Pausing: saving the resume state..." };
            ui.label(RichText::new(text).color(p.muted));
        }
        QueueItemStatus::Paused | QueueItemStatus::Failed(_) => {
            let resume = if item.status == QueueItemStatus::Paused { "Resume" } else { "Retry" };
            let go_on = primary_button(&format!("{} {}", icon::PLAY, resume), p.accent);
            if ui.add(go_on).on_hover_text("Continue from the saved state").clicked() {
                app.start_job(id, false);
            }
            let start_over = ui.add(button(format!("{} Start over", icon::ARROW_COUNTER_CLOCKWISE)));
            if start_over.on_hover_text("Delete the partial file and download from the beginning").clicked() {
                app.start_job(id, true);
            }
            let delete = button(RichText::new(format!("{} Delete leftovers", icon::TRASH)).color(p.red));
            if ui.add(delete).on_hover_text("Remove this download and delete its partial file and resume state").clicked() {
                app.discard_job(id);
            }
        }
        QueueItemStatus::AuthRequired => {}
        QueueItemStatus::Completed => {
            if let Some(path) = &item.target_path {
                if ui.add(primary_button(&format!("{} Open file", icon::FILE), p.accent)).clicked() {
                    report(app, util::open_path(path));
                }
                let idle = !app.verifying && app.repair.is_none();
                let check = ui.add_enabled(idle, button(format!("{} Check again", icon::SHIELD_CHECK)));
                if check.on_hover_text("Read the file again and check it against its size and checksum").clicked() {
                    app.verify(VerifyRequest {
                        path: path.clone(),
                        expected_size: (item.total_bytes > 0).then_some(item.total_bytes),
                        checksum: item.options.expected_checksum.clone(),
                    });
                }
            }
        }
    }
    let choosing = app.tracks.as_ref().is_some_and(|chooser| chooser.item == Some(id));
    if let Some(url) = item.urls.first().filter(|_| crate::tracks::offered(item) && !choosing) {
        if ui.add(button(format!("{} Choose tracks…", icon::SLIDERS_HORIZONTAL))).clicked() {
            app.list_tracks(Some(id), url.clone());
        }
    }
    if let Some(path) = &item.target_path {
        if ui.add(button(format!("{} Show in folder", icon::FOLDER_OPEN))).clicked() {
            report(app, util::reveal_in_folder(path));
        }
    }
    if let Some(url) = item.urls.first() {
        if ui.add(button(format!("{} Copy link", icon::COPY))).clicked() {
            app.copy_text(url.to_string());
        }
    }
    // Not while its engine runs, also for one shown done while its checksum is checked.
    let running = app.queue.get_item(id).is_some_and(|real| real.status.is_active());
    if !running && ui.add(button(format!("{} Remove", icon::X))).on_hover_text("Off the list; its files are kept (Undo brings it back)").clicked() {
        app.remove_job(id);
        app.focused = None;
        app.selected = None;
    }
    let new = ui.add(button(format!("{} New download", icon::PLUS)));
    if new.on_hover_text("Keep this download going and start another").clicked() {
        app.new_download();
    }
}

/// For a download restored without its Authorization header: the header to enter again, never
/// saved, and Resume.
fn auth_box(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    ui.add_space(8.0);
    ui.label(RichText::new(AUTH_REQUIRED).color(p.amber));
    ui.horizontal(|ui| {
        let field = egui::TextEdit::singleline(&mut app.auth_input).hint_text("Bearer <token>").password(true).desired_width((ui.available_width() - 120.0).max(120.0));
        ui.add(field);
        let ready = !app.auth_input.trim().is_empty();
        let resume = ui.add_enabled(ready, primary_button(&format!("{} Resume", icon::PLAY), p.accent)).on_disabled_hover_text("Enter the Authorization header first");
        if resume.clicked() {
            app.resume_with_auth(item.id);
        }
    });
}

/// The New link box of stopped download `item`, when open: a fresh link to paste, checked to be
/// the same file before it is taken (see `App::check_new_link`), and why one was refused.
fn new_link_box(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let Some(new_link) = app.new_link.as_mut().filter(|new_link| new_link.id == item.id) else { return };
    let (mut check, mut cancel) = (false, false);
    ui.add_space(8.0);
    Frame::none().fill(p.field).rounding(12.0).inner_margin(14.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(bold("New link").color(p.strong));
        let about = "A fresh link to the same file, from the page it came from. It is checked to be the same size first; the \
                     download then resumes where it stopped.";
        ui.add(egui::Label::new(RichText::new(about).small().color(p.muted)).wrap());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 190.0).max(120.0);
            let field = egui::TextEdit::singleline(&mut new_link.text).hint_text("https://…").desired_width(width);
            let field = ui.add_enabled(!new_link.checking, field);
            if std::mem::take(&mut new_link.focus) {
                field.request_focus();
                field.scroll_to_me(Some(Align::Center));
            }
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let label = if new_link.checking { "Checking…" } else { "Use this link" };
            let ready = !new_link.checking && !new_link.text.trim().is_empty();
            check = ui.add_enabled(ready, primary_button(label, p.accent)).clicked() || entered && ready;
            cancel = ui.add(button("Cancel")).clicked();
        });
        if let Some(why) = &new_link.refused {
            error_alert(ui, why);
        }
    });
    if cancel {
        app.new_link = None;
    } else if check {
        app.check_new_link();
    }
}

/// What holds running download `item` back (see `slow::findings`), coming in one after another,
/// each with the button of its fix when the app has one; or that nothing in the app does.
fn why_slow(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let Some(view) = app.jobs.get(&item.id) else { return };
    let running = || app.queue.items().iter().filter(|other| other.status == QueueItemStatus::Downloading);
    // shortcut: downloads share a server when their links' do, not where those redirect to; the
    // engine's own host slots would be exact.
    let server = |other: &QueueItem| other.urls.first().map(hyperfetch_core::hosts::HostKey::of);
    let on_host: usize = running().filter(|other| server(*other) == server(item)).map(|other| app.jobs.get(&other.id).map_or(0, |v| v.active_workers)).sum();
    let limited: f64 = running().filter(|other| other.options.max_speed == item.options.max_speed).map(|other| other.speed_bytes_per_sec).sum();
    let found = slow::findings(item, view, on_host, limited);
    let mut fix = None;
    ui.add_space(8.0);
    Frame::none().fill(p.field).rounding(12.0).inner_margin(14.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(bold("What holds it back").color(p.strong));
        ui.add_space(4.0);
        if found.is_empty() {
            ui.label(RichText::new("Nothing in the app: the server or your internet line sets its speed.").color(p.muted));
        }
        for (n, finding) in found.iter().enumerate() {
            anim::fade_in(ui, egui::Id::new(("why slow", item.id, n)), anim::stagger(n), anim::RISE, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(change) = finding.fix {
                            let name = match change {
                                Fix::NoLimit => "Lift the limit for it".to_string(),
                                Fix::Connections(n) => format!("Use {n} connections"),
                            };
                            if ui.add(button(name)).on_hover_text("Starts it again from where it is, with this change").clicked() {
                                fix = Some(change);
                            }
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new(icon::INFO).color(p.amber));
                            ui.add(egui::Label::new(finding.text.as_str()).wrap());
                        });
                    });
                });
            });
        }
    });
    if let Some(change) = fix {
        app.restart_with(item.id, |options| match change {
            Fix::NoLimit => options.max_speed = None,
            Fix::Connections(n) => options.num_connections = n,
        });
    }
}

fn metric(ui: &mut Ui, label: &str, value: String, color: Option<Color32>) {
    let p = palette(ui);
    ui.label(RichText::new(label).small().color(p.muted));
    // Truncated: a wrapped line in a column is stretched to its width.
    let text = bold(&value).size(15.0).color(color.unwrap_or(p.strong));
    ui.add(egui::Label::new(text).truncate()).on_hover_text(value);
}

/// The big percentage rolling to each reading, the pieces filling in, and the numbers that
/// matter: how much, how fast, how long.
fn progress_block(app: &App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let view = app.jobs.get(&item.id);
    let torrent = view.and_then(|view| view.torrent.as_ref());
    let downloading = item.status == QueueItemStatus::Downloading;
    let (downloaded, total) = (item.downloaded_bytes, item.total_bytes);
    let elapsed = view.map_or(0, |view| view.elapsed().as_secs());
    // Shared with the download's card in the list, so the bar carries on from where it was.
    let id = egui::Id::new(("item", item.id));
    let fill = if item.status == QueueItemStatus::Completed { p.green } else { p.accent };
    let fill = anim::color(ui.ctx(), id.with("bar"), fill, 0.35);
    let ratio = item.progress_ratio as f32;
    let shown = anim::roll(ui.ctx(), id.with("percent"), ratio.clamp(0.0, 1.0) * 100.0);
    ui.horizontal(|ui| {
        let percent = if total > 0 || item.status == QueueItemStatus::Completed { format!("{:.0}%", shown.floor()) } else { "…".to_string() };
        ui.label(RichText::new(percent).font(FontId::new(34.0, semibold())).color(p.strong));
        // Of a size not known, what came so far; a known one's share is the number already.
        if total == 0 {
            ui.vertical(|ui| {
                ui.add_space(6.0);
                let text = util::progress_text(total, downloaded, eased_progress(ui, id, ratio) as f64, 1, view.map(|_| elapsed));
                ui.label(RichText::new(text).color(p.muted));
            });
        }
    });
    let live = downloading && app.stalled_for(item.id).is_none() && !app.is_resolving(item.id);
    progress_bar(ui, id, Vec2::new(ui.available_width(), 10.0), ratio, fill, live, None);
    ui.add_space(10.0);

    let eta = match item.status {
        QueueItemStatus::Completed => "Done".to_string(),
        _ if downloading => util::eta_secs(total, downloaded, item.speed_bytes_per_sec).map_or_else(|| "--:--".to_string(), format_duration),
        _ => "--:--".to_string(),
    };
    let speed = eased_speed(ui, item);
    let transferred = if total > 0 { format!("{} / {}", format_bytes(downloaded), format_bytes(total)) } else { format_bytes(downloaded) };
    let (label, connections) = match (torrent, view) {
        (Some(torrent), _) if downloading => ("Peers", torrent.peers.to_string()),
        (_, Some(view)) if downloading => ("Connections", format!("{} active", view.active_workers)),
        _ => ("Connections", "none".to_string()),
    };
    let cells = [
        ("Transferred", transferred, None),
        ("Speed", format!("{}/s", format_bytes(speed as u64)), Some(p.cyan)),
        ("Elapsed / left", format!("{} / {}", format_duration(elapsed), eta), None),
        (label, connections, None),
    ];
    // Four across, or two by two where four would cut their values short (the details beside).
    let across = if ui.available_width() < 560.0 { 2 } else { 4 };
    for row in cells.chunks(across) {
        ui.columns(across, |cols| {
            for (col, (label, value, color)) in cols.iter_mut().zip(row) {
                metric(col, label, value.clone(), *color);
            }
        });
    }

    if let Some(torrent) = torrent {
        ui.add_space(6.0);
        let mut text = format!("{} Uploaded {} at {}/s", icon::UPLOAD_SIMPLE, format_bytes(torrent.uploaded_bytes), format_bytes(torrent.upload_speed as u64));
        if torrent.seeding {
            text.push_str(", seeding");
        }
        ui.label(RichText::new(text).color(p.muted));
    }
    for note in &item.notes {
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::INFO).color(p.link));
            ui.add(egui::Label::new(note.as_str()).wrap());
        });
    }

    if let Some(why) = view.and_then(|view| view.autopilot.as_deref()).filter(|_| downloading) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Autopilot").small().color(p.muted));
            ui.add(egui::Label::new(RichText::new(why).color(p.strong)).truncate()).on_hover_text(why);
        });
    }

    if let Some(stalled) = app.stalled_for(item.id).filter(|_| downloading) {
        ui.add_space(6.0);
        let text = match app.waits_for_network(item.id) {
            true => "Waiting for the network: the system says it is gone. The download goes on by itself once it is back, \
                     without using up its retries."
                .to_string(),
            false => format!(
                "Stalled: no data received for {}s. Stalled connections are retried after {}s without data.",
                stalled.as_secs(),
                item.options.stall_timeout_secs
            ),
        };
        ui.add(egui::Label::new(bold(text).color(p.amber)).wrap());
    }
}

/// The progress `progress_bar(ui, id, ..)` shows on its way to `ratio`, for numbers that keep up
/// with the bar.
fn eased_progress(ui: &Ui, id: egui::Id, ratio: f32) -> f32 {
    // The same target as the bar's, clamped alike: a different one would restart its easing.
    anim::value(ui.ctx(), id.with("progress"), ratio.clamp(0.0, 1.0), 0.35)
}

/// The download's speed, rolling to each new reading.
fn eased_speed(ui: &Ui, item: &QueueItem) -> f64 {
    anim::roll(ui.ctx(), egui::Id::new(("item", item.id, "speed")), item.speed_bytes_per_sec.max(0.0) as f32).max(0.0) as f64
}

fn status_line(app: &App, item: &QueueItem, p: &Palette) -> (String, Color32) {
    match &item.status {
        QueueItemStatus::Queued => ("Waiting: starts when a download slot is free.".to_string(), p.muted),
        QueueItemStatus::Downloading if app.is_resolving(item.id) => ("Connecting to the server\u{2026}".to_string(), p.muted),
        QueueItemStatus::Downloading if item.is_finishing() => {
            (format!("Finishing {}: verifying the file and moving it into place...", item.filename), p.muted)
        }
        QueueItemStatus::Downloading => (format!("Downloading {}", item.filename), p.muted),
        QueueItemStatus::Pausing if app.is_recording(item.id) => ("Stopping: yt-dlp is finishing the recording...".to_string(), p.muted),
        QueueItemStatus::Pausing => ("Pausing: saving resume state...".to_string(), p.muted),
        QueueItemStatus::Paused => ("Paused. Resume continues where it stopped; Start over deletes the partial file first.".to_string(), p.muted),
        QueueItemStatus::Completed => {
            let path = item.target_path.as_ref().map_or_else(|| item.filename.clone(), |p| p.display().to_string());
            (format!("Completed: {}", path), p.green)
        }
        QueueItemStatus::Failed(error) if item.link_expired() => {
            (format!("{}. Its server refused the old one ({}), as it does once a link's time is up.", LINK_EXPIRED, error), p.amber)
        }
        QueueItemStatus::Failed(error) => match util::error_hint(error) {
            Some(hint) => (format!("Error: {}. {}", error.trim_end_matches('.'), hint), p.red),
            None => (format!("Error: {}", error), p.red),
        },
        QueueItemStatus::AuthRequired => (AUTH_REQUIRED.to_string(), p.amber),
    }
}

/// A worker owns the chunk ("Worker n" from the range engine, "Downloading" from the HLS engine).
fn is_active_chunk(chunk: &ChunkSnapshot) -> bool {
    chunk.status.starts_with("Worker") || chunk.status == "Downloading"
}

/// The colour of connection `worker`: the theme's `worker`th, around again past the last.
fn lane(p: &Palette, worker: usize) -> Color32 {
    p.lanes[worker % p.lanes.len()]
}

/// How long a piece split off another (see [`chunk_map`]) takes to turn from its colour to its own.
const SPLIT: f64 = 0.4;

/// What a download's chunk map showed, kept between frames to tell pieces split off others.
#[derive(Clone, Default)]
struct Pieces {
    /// The pass that last drew the map; a gap means it was away and starts over.
    pass: Option<u64>,
    /// The id the next piece split off gets.
    next: usize,
    /// Pieces split off while the map showed: id, when, and the colour of the piece cut.
    split: Vec<(usize, f64, Color32)>,
}

/// The ranges of download `item` side by side, each in the colour of the connection that holds
/// it: each fills smoothly as its bytes come in and turns green when done; one handed out but not
/// yet receiving glows while the download runs. A piece a connection takes over from the end of
/// another's (work stealing) splits off: a light flashes on the cut, and the piece turns from the
/// other's colour to its own.
fn chunk_map(ui: &mut Ui, item: usize, chunks: &[ChunkSnapshot], total_bytes: u64, live: bool) {
    let p = palette(ui);
    let ctx = ui.ctx().clone();
    let height = 26.0;
    let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), height), Sense::hover());
    let rect = response.rect;
    let key = egui::Id::new(("chunk map", item));
    // Where tests find it.
    ui.interact(rect, key, Sense::hover());
    painter.rect_filled(rect, 8.0, p.field);
    if chunks.is_empty() || total_bytes == 0 {
        return;
    }
    let base = |chunk: &ChunkSnapshot| match chunk.worker_id {
        _ if chunk.total_bytes > 0 && chunk.downloaded_bytes >= chunk.total_bytes => p.green,
        Some(worker) => lane(&p, worker),
        None if chunk.status.starts_with("Failed") => p.red,
        None => p.dim,
    };
    // Pieces get the next free id as they are split off; ones that come with the map are no news.
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    let mut pieces: Pieces = ctx.data_mut(|d| d.remove_temp(key)).unwrap_or_default();
    if pieces.pass.is_some_and(|last| last + 1 == pass) {
        for chunk in chunks.iter().filter(|chunk| chunk.id >= pieces.next) {
            if let Some(cut) = chunks.iter().find(|other| other.range_end + 1 == chunk.range_start) {
                pieces.split.push((chunk.id, now, base(cut)));
            }
        }
    }
    pieces.split.retain(|&(_, at, _)| !anim::reduced(&ctx) && now - at < SPLIT);
    pieces.pass = Some(pass);
    pieces.next = pieces.next.max(chunks.iter().map(|chunk| chunk.id + 1).max().unwrap_or(0));

    let painter = painter.with_clip_rect(rect.shrink(1.0));
    let x = |bytes: u64| rect.min.x + (bytes as f32 / total_bytes as f32).clamp(0.0, 1.0) * rect.width();
    let glow = if live { anim::pulse(&ctx, 1.8) } else { 0.0 };
    let mut cuts = Vec::new();
    for chunk in chunks {
        let (left, right) = (x(chunk.range_start), x(chunk.range_end + 1));
        let seg_rect = Rect::from_x_y_ranges(left..=right.max(left + 1.0), rect.min.y + 1.0..=rect.max.y - 1.0);
        // Keyed like the chunk table's bars.
        let id = egui::Id::new(("chunk", item, chunk.id));
        let mut color = anim::color(&ctx, id.with("bar"), base(chunk), 0.3);
        if let Some(&(_, at, from)) = pieces.split.iter().find(|split| split.0 == chunk.id) {
            let t = anim::ease(anim::progress(&ctx, (now - at) as f32, 0.0, SPLIT as f32));
            color = from.lerp_to_gamma(color, t);
            cuts.push((left, t));
        }

        if chunk.total_bytes > 0 && chunk.downloaded_bytes > 0 {
            painter.rect_filled(seg_rect, 0.0, color.gamma_multiply(0.3));
            // Where its bytes reach in the file, so that a piece cut shorter keeps its fill still.
            let reach = (chunk.range_start + chunk.downloaded_bytes.min(chunk.total_bytes)) as f32 / total_bytes as f32;
            let reach = rect.min.x + anim::value(&ctx, id.with("reach"), reach.clamp(0.0, 1.0), 0.35) * rect.width();
            painter.rect_filled(Rect::from_x_y_ranges(left..=reach.clamp(left, seg_rect.right()), seg_rect.y_range()), 0.0, color);
        } else if live && is_active_chunk(chunk) {
            // Handed out but no bytes yet.
            painter.rect_filled(seg_rect, 0.0, color.gamma_multiply(0.35 + glow * 0.5));
        } else if chunk.status.starts_with("Failed") {
            painter.rect_filled(seg_rect, 0.0, color.gamma_multiply(0.6));
        } else {
            painter.rect_filled(seg_rect, 0.0, p.border);
        }
        painter.line_segment([seg_rect.right_top(), seg_rect.right_bottom()], Stroke::new(1.0, p.card));
    }
    for (left, t) in cuts {
        let light = Stroke::new(2.0, Color32::WHITE.gamma_multiply(1.0 - t));
        painter.line_segment([Pos2::new(left, rect.min.y + 1.0), Pos2::new(left, rect.max.y - 1.0)], light);
    }
    painter.rect_stroke(rect, 8.0, Stroke::new(1.0, p.border));
    ctx.data_mut(|d| d.insert_temp(key, pieces));
}

/// Each piece of the file: the connection that holds it (in its colour) and its server, its
/// part of the file, how much of it came, and its state. Only the rows in view are laid out: an
/// HLS stream can have thousands of segments.
fn chunk_table(ui: &mut Ui, item: usize, chunks: &[ChunkSnapshot], live: bool, host: &str) {
    let p = palette(ui);
    let row_height = 20.0;
    let scroll = egui::ScrollArea::vertical().id_salt(("chunk_table", item)).max_height(220.0).auto_shrink([false, true]);
    scroll.show_rows(ui, row_height, chunks.len(), |ui, rows| {
        for chunk in &chunks[rows] {
            ui.horizontal(|ui| {
                ui.set_height(row_height);
                let busy = live && is_active_chunk(chunk);
                let color = if chunk.status == "Completed" {
                    p.green
                } else if busy {
                    chunk.worker_id.map_or(p.cyan, |worker| lane(&p, worker))
                } else if chunk.status.starts_with("Failed") {
                    p.red
                } else {
                    p.muted
                };
                // The engine's "Worker 3" is the download's third connection.
                let status = match chunk.status.strip_prefix("Worker ") {
                    _ if !live && is_active_chunk(chunk) => "Stopped".to_string(),
                    Some(n) => format!("Connection {}", n),
                    None => chunk.status.clone(),
                };
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, color);
                ui.add_sized([108.0, row_height], egui::Label::new(RichText::new(status).color(color)).truncate()).on_hover_text(format!("{} on {host}", chunk.status));
                let ratio = if chunk.total_bytes > 0 { (chunk.downloaded_bytes as f32 / chunk.total_bytes as f32).clamp(0.0, 1.0) } else { 0.0 };
                let id = egui::Id::new(("chunk", item, chunk.id));
                let percent = format!("{:.0}%", eased_progress(ui, id, ratio) * 100.0);
                let width = (ui.available_width() - 150.0).clamp(60.0, 200.0);
                progress_bar(ui, id, Vec2::new(width, 6.0), ratio, color, busy, None);
                ui.add_sized([40.0, row_height], egui::Label::new(RichText::new(percent).small().color(p.text)));
                let range = format!("{} – {}", format_bytes(chunk.range_start), format_bytes(chunk.range_end));
                ui.add(egui::Label::new(RichText::new(range).small().color(p.dim)).truncate());
            });
        }
    });
}

/// The mirrors a download takes pieces from, fastest first, each with a bar of its speed; the
/// fastest is marked as winning.
fn mirrors(ui: &mut Ui, speeds: &[(usize, String, f64)]) {
    let p = palette(ui);
    let mut sorted: Vec<&(usize, String, f64)> = speeds.iter().collect();
    sorted.sort_by(|a, b| b.2.total_cmp(&a.2));
    let top = sorted.first().map_or(1.0, |m| m.2.max(1.0));
    for (n, (id, host, speed)) in sorted.into_iter().enumerate() {
        ui.horizontal(|ui| {
            let mark = if n == 0 { RichText::new(icon::CROWN_SIMPLE).color(p.amber) } else { RichText::new(format!("#{id}")).small().color(p.dim) };
            ui.add_sized([28.0, 20.0], egui::Label::new(mark)).on_hover_text(if n == 0 { "The fastest: it gets the most pieces" } else { "" });
            ui.add_sized([160.0, 20.0], egui::Label::new(RichText::new(host).color(p.text)).truncate()).on_hover_text(host);
            let width = (ui.available_width() - 90.0).max(40.0);
            progress_bar(ui, egui::Id::new(("mirror", id)), Vec2::new(width, 6.0), (*speed / top) as f32, if n == 0 { p.link } else { p.dim }, false, None);
            ui.label(RichText::new(format!("{}/s", format_bytes(*speed as u64))).small().color(p.cyan));
        });
    }
}

/// What the download was or will be checked against: the checksum given, the site's own, or
/// only its size; after it finished, what post-processing said of it.
fn checks(ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    section_title(ui, icon::SHIELD_CHECK, "Checks", "");
    let verified = item.notes.iter().find(|note| note.starts_with("Checked against"));
    let (glyph, color, text) = match (&item.options.expected_checksum, verified) {
        (_, Some(note)) => (icon::CHECK_CIRCLE, p.green, note.clone()),
        (Some(sum), None) => {
            let kind = sum.split_once(':').map_or("checksum", |(kind, _)| kind).to_uppercase();
            let when = if item.status == QueueItemStatus::Completed { "Matched" } else { "To be checked against" };
            (icon::SHIELD_CHECK, p.link, format!("{when} the {kind} you gave: {}", truncate_chars(sum, 40)))
        }
        (None, None) if item.options.remote_checksum => (icon::SHIELD, p.muted, "Checked against the site's checksum when it publishes one; its size always".to_string()),
        (None, None) => (icon::SHIELD, p.muted, "Checked against its size; give a checksum under More options to check its bytes too".to_string()),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(glyph).color(color));
        ui.add(egui::Label::new(RichText::new(text).color(p.text)).wrap());
    });
}

/// Where the file goes (and Show in folder once there), and where it came from: the page, and
/// its links to copy.
fn where_from(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    section_title(ui, icon::FOLDER_OPEN, "Where", "");
    let target = item.target_path.clone().or_else(|| item.options.output_path.clone());
    if let Some(path) = target {
        let shown = path.display().to_string();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Saved to").small().color(p.muted));
            ui.add(egui::Label::new(RichText::new(&shown).monospace().color(p.text)).truncate()).on_hover_text(&shown);
        });
    }
    if let Some(page) = page_of(item) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("From the page").small().color(p.muted));
            if ui.link(truncate_chars(page, 70)).on_hover_text(page).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(page));
            }
        });
    }
    for url in item.urls.iter().take(4) {
        ui.horizontal(|ui| {
            if icon_button(ui, icon::COPY, "Copy the link", true).clicked() {
                app.copy_text(url.to_string());
            }
            let shown = hyperfetch_core::history::redact_url(url.as_str());
            ui.add(egui::Label::new(RichText::new(&shown).small().monospace().color(p.dim)).truncate()).on_hover_text(shown.clone());
        });
    }
}

/// This download's own speed limit, under the one for all downloads, which still holds, and its
/// connections, the Autopilot's ceiling: kept with it, and taken at once while it runs, without
/// starting it again (see `App::set_own_limits`).
fn own_limits(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    const MIB: f64 = 1024.0 * 1024.0;
    if item.status == QueueItemStatus::Completed {
        return;
    }
    let p = palette(ui);
    section_title(ui, icon::GAUGE, "For this download", "under the limit for all downloads");
    let id = egui::Id::new(("own limits", item.id));
    let current = (item.options.own_speed.map_or(0.0, |bytes| bytes as f64 / MIB), item.options.num_connections);
    let (mut speed, mut connections) = ui.data(|d| d.get_temp::<(f64, usize)>(id)).unwrap_or(current);
    let mut apply = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Speed limit").color(p.text));
        ui.add(egui::DragValue::new(&mut speed).range(0.0..=10_000.0).speed(0.1).max_decimals(1).suffix(" MB/s"));
        ui.label(RichText::new("0 = none").small().color(p.muted));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Connections").color(p.text));
        ui.add(egui::DragValue::new(&mut connections).range(1..=64));
        let changed = (speed - current.0).abs() > 0.05 || connections != current.1;
        apply = ui.add_enabled(changed, button("Apply")).on_hover_text("A running download takes it at once").clicked();
    });
    ui.data_mut(|d| d.insert_temp(id, (speed, connections)));
    if apply {
        app.set_own_limits(item.id, (speed > 0.0).then(|| ((speed * MIB) as u64).max(1)), connections);
    }
}

/// The speed of download `item` over the last minute. A new sample rises from the one before
/// and the scale eases to a new peak; the newest point glows while the download runs.
fn throughput_graph(ui: &mut Ui, item: usize, history: Option<&VecDeque<(Instant, f64)>>, current: f64, live: bool) {
    let p = palette(ui);
    let id = egui::Id::new(("graph", item));
    let now = Instant::now();
    let window = GRAPH_WINDOW.as_secs_f32();
    // (age in seconds, speed) of the samples inside the window.
    let mut samples: Vec<(f32, f64)> = history
        .into_iter()
        .flatten()
        .map(|(t, speed)| (now.duration_since(*t).as_secs_f32(), *speed))
        .filter(|(age, _)| *age <= window)
        .collect();
    let peak = samples.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
    let avg = if samples.is_empty() { 0.0 } else { samples.iter().map(|(_, s)| s).sum::<f64>() / samples.len() as f64 };
    let stats = format!("Peak {}/s  ·  Average {}/s  ·  Now {}/s", format_bytes(peak as u64), format_bytes(avg as u64), format_bytes(current as u64));
    ui.label(RichText::new(stats).small().color(p.muted));
    ui.add_space(4.0);

    let graph_height = 96.0;
    let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), graph_height), Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 10.0, p.field);
    let y_mid = rect.center().y;
    painter.line_segment([Pos2::new(rect.min.x + 8.0, y_mid), Pos2::new(rect.max.x - 8.0, y_mid)], Stroke::new(1.0, p.border));

    let max_y = anim::value(ui.ctx(), id.with("scale"), (peak * 1.15).max(1024.0 * 1024.0) as f32, 0.4) as f64;
    if let Some((_, speed)) = samples.last_mut() {
        *speed = anim::value(ui.ctx(), id.with("tip"), *speed as f32, 0.3) as f64;
    }
    for (y, value) in [(rect.min.y + 4.0, max_y), (y_mid + 2.0, max_y / 2.0)] {
        let label = format!("{}/s", format_bytes(value as u64));
        painter.text(Pos2::new(rect.min.x + 8.0, y), Align2::LEFT_TOP, label, FontId::proportional(10.5), p.dim);
    }

    let points: Vec<Pos2> = samples
        .iter()
        .map(|(age, speed)| {
            let x = rect.min.x + (1.0 - age / window) * rect.width();
            let y = (rect.max.y - 2.0) - (*speed as f32 / max_y as f32).clamp(0.0, 1.0) * (rect.height() - 6.0);
            Pos2::new(x, y)
        })
        .collect();
    if points.len() >= 2 {
        // Area fill as one mesh with a vertical gradient.
        let mut mesh = egui::Mesh::default();
        for point in &points {
            mesh.colored_vertex(*point, p.link.gamma_multiply(0.28));
            mesh.colored_vertex(Pos2::new(point.x, rect.max.y - 1.0), p.link.gamma_multiply(0.02));
        }
        for i in 0..(points.len() as u32 - 1) {
            let (top_left, bot_left, top_right, bot_right) = (i * 2, i * 2 + 1, i * 2 + 2, i * 2 + 3);
            mesh.add_triangle(top_left, bot_left, bot_right);
            mesh.add_triangle(top_left, bot_right, top_right);
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::line(points.clone(), Stroke::new(2.0, p.link)));

        if let Some(&last) = points.last() {
            let pulse = if live { anim::pulse(ui.ctx(), 1.8) } else { 0.0 };
            painter.circle_filled(last, 4.0 + pulse * 3.0, p.link.gamma_multiply(0.15 + pulse * 0.2));
            painter.circle(last, 3.5, p.field, Stroke::new(2.0, p.link));
        }
    }

    if let Some(hover) = response.hover_pos().filter(|pos| rect.contains(*pos)) {
        let target_age = (1.0 - (hover.x - rect.min.x) / rect.width()) * window;
        let nearest = samples.iter().min_by(|(a, _), (b, _)| (a - target_age).abs().total_cmp(&(b - target_age).abs()));
        if let Some((_, speed)) = nearest {
            painter.line_segment([Pos2::new(hover.x, rect.min.y), Pos2::new(hover.x, rect.max.y)], Stroke::new(1.0, p.muted));
            response.show_tooltip_text(format!("{}s ago: {}/s", target_age as u64, format_bytes(*speed as u64)));
        }
    }
}

// ---- Timeline -------------------------------------------------------------------------------

/// What happened to each download while the app watched: noted by [`observe`] as its status
/// changes (added, started, resumed, stalled, paused, failed, finished) and as Autopilot changes
/// its mind. Kept for this run of the app.
#[derive(Clone, Default)]
struct Timeline {
    /// Each download's status kind, whether it was stalled, and Autopilot's last word.
    seen: HashMap<usize, (u8, bool, Option<String>)>,
    /// Each download's events: the local time (HH:MM) and what happened, oldest first.
    events: HashMap<usize, Vec<(String, String)>>,
}

fn timeline_id() -> egui::Id {
    egui::Id::new("download timeline")
}

/// A number for the kind of `status`, its error text aside.
fn status_kind(status: &QueueItemStatus) -> u8 {
    match status {
        QueueItemStatus::Queued => 0,
        QueueItemStatus::Downloading => 1,
        QueueItemStatus::Pausing => 2,
        QueueItemStatus::Paused => 3,
        QueueItemStatus::Completed => 4,
        QueueItemStatus::Failed(_) => 5,
        QueueItemStatus::AuthRequired => 6,
    }
}

/// Notes every download's news in its timeline (see [`Timeline`]); called once a frame.
pub(super) fn observe(app: &App, ctx: &egui::Context) {
    ctx.data_mut(|d| {
        let timeline = d.get_temp_mut_or_default::<Timeline>(timeline_id());
        let mut time = None;
        let mut now = || {
            time.get_or_insert_with(|| {
                let (_, minute) = crate::desktop::local_now();
                format!("{:02}:{:02}", minute / 60, minute % 60)
            })
            .clone()
        };
        for item in app.queue.items() {
            let kind = status_kind(&item.status);
            let stalled = app.stalled_for(item.id).is_some();
            let autopilot = app.jobs.get(&item.id).and_then(|view| view.autopilot.clone());
            let before = timeline.seen.get(&item.id).cloned();
            if before.as_ref() == Some(&(kind, stalled, autopilot.clone())) {
                continue;
            }
            let mut news = Vec::new();
            match &before {
                None => {
                    news.push(match item.status {
                        QueueItemStatus::Completed => "Finished".to_string(),
                        _ => "Added to the list".to_string(),
                    });
                    news.extend(autopilot.as_ref().map(|why| format!("Autopilot: {why}")));
                }
                Some((was, was_stalled, was_autopilot)) => {
                    if *was != kind {
                        news.push(match &item.status {
                            QueueItemStatus::Queued => "Waiting for a free slot".to_string(),
                            QueueItemStatus::Downloading if item.downloaded_bytes > 0 => format!("Resumed at {}", format_bytes(item.downloaded_bytes)),
                            QueueItemStatus::Downloading => "Started".to_string(),
                            QueueItemStatus::Pausing => "Pausing".to_string(),
                            QueueItemStatus::Paused => "Paused".to_string(),
                            QueueItemStatus::Completed => format!("Finished: {}", format_bytes(item.total_bytes.max(item.downloaded_bytes))),
                            QueueItemStatus::Failed(error) => format!("Failed: {}", truncate_chars(error, 120)),
                            QueueItemStatus::AuthRequired => "Waiting for its Authorization header".to_string(),
                        });
                    }
                    if stalled != *was_stalled {
                        news.push(if stalled { "Stalled: no data for a while" } else { "Data coming in again" }.to_string());
                    }
                    if autopilot != *was_autopilot {
                        if let Some(why) = &autopilot {
                            news.push(format!("Autopilot: {why}"));
                        }
                    }
                }
            }
            timeline.seen.insert(item.id, (kind, stalled, autopilot));
            let events = timeline.events.entry(item.id).or_default();
            for what in news {
                events.push((now(), what));
            }
            // The newest 40 are plenty to read.
            if events.len() > 40 {
                events.drain(..events.len() - 40);
            }
        }
    });
}

/// The timeline of `item` (see [`Timeline`]), newest first, a line each: the time on a dot, and
/// what happened.
fn timeline_view(ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let events = ui.data(|d| d.get_temp::<Timeline>(timeline_id())).and_then(|t| t.events.get(&item.id).cloned()).unwrap_or_default();
    if events.is_empty() {
        return;
    }
    section_title(ui, icon::CLOCK_COUNTER_CLOCKWISE, "What happened", "while the app was open");
    for (n, (time, what)) in events.iter().rev().enumerate() {
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
            let color = if n == 0 { p.link } else { p.dim };
            ui.painter().circle_filled(dot.center(), 3.5, color);
            ui.add_sized([42.0, 18.0], egui::Label::new(RichText::new(time).small().color(p.muted)));
            ui.add(egui::Label::new(RichText::new(what).color(if n == 0 { p.text } else { p.muted })).wrap());
        });
    }
}

// ---- Cards ----------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum RowAction {
    Select,
    Start,
    Pause,
    Show,
    Remove,
    Open,
    Reveal,
    /// Shows the download with its track chooser open (see `App::list_tracks`).
    Tracks,
    /// Puts it first among the waiting downloads (see `DownloadQueue::download_next`).
    Next,
    /// Moves it to this place of the list.
    MoveTo(usize),
    Priority(Priority),
    /// Shows it with its New link box open (see `App::check_new_link`).
    NewLink,
    /// Opens the page it came from in the browser.
    OpenPage,
    /// Joins it to the download of its file from another host (see `App::join_twin`).
    Join,
    /// Keeps it apart from that download (see `App::keep_apart`).
    KeepApart,
}

/// The order and priority of `item`, the `index`th of `len` downloads: Download next, Move up and
/// Move down, and for a waiting one its priority; for a stopped one, New link… and the page it
/// came from. Its card's ⋯ menu and right-click menu.
fn order_menu(ui: &mut Ui, item: &QueueItem, index: usize, len: usize, action: &mut Option<(usize, RowAction)>) {
    let waiting = item.status == QueueItemStatus::Queued;
    let moves = [
        (waiting, icon::ARROW_LINE_UP, "Download next", RowAction::Next),
        (index > 0, icon::ARROW_UP, "Move up", RowAction::MoveTo(index.saturating_sub(1))),
        (index + 1 < len, icon::ARROW_DOWN, "Move down", RowAction::MoveTo(index + 1)),
    ];
    // Download next only where it applies; the others greyed out at the ends.
    for (enabled, glyph, name, what) in moves.into_iter().filter(|&(enabled, _, name, _)| enabled || name != "Download next") {
        if ui.add_enabled(enabled, egui::Button::new(format!("{glyph} {name}"))).clicked() {
            *action = Some((item.id, what));
            ui.close_menu();
        }
    }
    if waiting {
        ui.separator();
        ui.label(RichText::new("Priority: which waiting download starts first").small().color(palette(ui).muted));
        for (priority, name) in [(Priority::High, "High"), (Priority::Normal, "Normal"), (Priority::Low, "Low")] {
            if ui.selectable_label(item.priority == priority, name).clicked() {
                *action = Some((item.id, RowAction::Priority(priority)));
                ui.close_menu();
            }
        }
    }
    if takes_new_link(item) {
        ui.separator();
        let page = page_of(item).map(|_| (icon::ARROW_SQUARE_OUT, "Open the page", RowAction::OpenPage));
        for (glyph, name, what) in std::iter::once((icon::LINK, "New link…", RowAction::NewLink)).chain(page) {
            if ui.button(format!("{glyph} {name}")).clicked() {
                *action = Some((item.id, what));
                ui.close_menu();
            }
        }
    }
}

/// Height of one card: every card has the same, so only the cards in view are laid out.
const ROW_HEIGHT: f32 = 82.0;
/// The space between two cards.
const GAP: f32 = 8.0;

/// The buttons of a card for a download in `status`: icon, tooltip and action.
fn row_actions(status: &QueueItemStatus) -> &'static [(&'static str, &'static str, RowAction)] {
    use RowAction::*;
    match status {
        QueueItemStatus::Queued => &[(icon::PLAY, "Start", Start), (icon::INFO, "Details", Show), (icon::X, "Remove (files are kept)", Remove)],
        QueueItemStatus::Downloading => &[(icon::PAUSE, "Pause", Pause), (icon::INFO, "Details", Show)],
        QueueItemStatus::Pausing => &[(icon::INFO, "Details", Show)],
        QueueItemStatus::Paused => &[(icon::PLAY, "Resume", Start), (icon::INFO, "Details", Show), (icon::X, "Remove (files are kept)", Remove)],
        QueueItemStatus::Failed(_) => {
            &[(icon::ARROW_CLOCKWISE, "Retry", Start), (icon::INFO, "Details", Show), (icon::X, "Remove (files are kept)", Remove)]
        }
        QueueItemStatus::AuthRequired => {
            &[(icon::INFO, "Details: enter the Authorization header there to resume", Show), (icon::X, "Remove (files are kept)", Remove)]
        }
        QueueItemStatus::Completed => &[
            (icon::FILE, "Open the file", Open),
            (icon::FOLDER_OPEN, "Show in its folder", Reveal),
            (icon::INFO, "Details", Show),
            (icon::X, "Remove from the list (the file is kept)", Remove),
        ],
    }
}

/// The plain one-line status of a card: where it is (`ratio` as shown), how fast, how long still,
/// over how many connections or peers, and what post-processing said.
fn row_summary(app: &App, item: &QueueItem, ratio: f64) -> String {
    let view = app.jobs.get(&item.id);
    let elapsed = view.map(|view| view.elapsed().as_secs());
    let mut parts = Vec::new();
    if item.status == QueueItemStatus::Completed {
        parts.push(format_bytes(item.total_bytes.max(item.downloaded_bytes)));
    } else {
        parts.push(util::progress_text(item.total_bytes, item.downloaded_bytes, ratio, 0, elapsed));
        if item.total_bytes > 0 {
            parts.push(format!("{} of {}", format_bytes(item.downloaded_bytes), format_bytes(item.total_bytes)));
        }
    }
    if item.status == QueueItemStatus::Downloading {
        parts.push(format!("{}/s", format_bytes(item.speed_bytes_per_sec as u64)));
        if let Some(eta) = util::eta_secs(item.total_bytes, item.downloaded_bytes, item.speed_bytes_per_sec) {
            parts.push(format!("{} left", format_duration(eta)));
        }
        if app.waits_for_network(item.id) {
            parts.push("waiting for the network".to_string());
        } else if let Some(stalled) = app.stalled_for(item.id) {
            parts.push(format!("no data for {} s, retrying", stalled.as_secs()));
        } else if let Some(view) = view.filter(|view| view.active_workers > 0 && view.torrent.is_none()) {
            parts.push(match view.active_workers {
                1 => "1 connection".to_string(),
                n => format!("{n} connections"),
            });
        }
    }
    if let Some(twin) = app.twins.get(&item.id).filter(|twin| twin.offered) {
        parts.push(format!("waits: #{} has this file from {}; download it from both?", twin.of, twin.host));
    }
    if let Some(torrent) = view.and_then(|view| view.torrent.as_ref()) {
        parts.push(format!("{} peers", torrent.peers));
        parts.push(format!("{} {} up", icon::UPLOAD_SIMPLE, format_bytes(torrent.uploaded_bytes)));
        if torrent.seeding {
            parts.push("seeding".to_string());
        }
    }
    if item.status == QueueItemStatus::Queued && app.metered_holds(item.id) {
        parts.push("waits: the connection is metered (Start downloads it anyway)".to_string());
    }
    parts.extend(item.notes.iter().cloned());
    parts.extend(app.rerun_text(item.id));
    parts.join("  ·  ")
}

/// How long a removed card lingers: fading out for the first half, then closing its gap.
const GHOST: f64 = 0.4;

/// A removed download's card, still fading out.
#[derive(Clone)]
struct Ghost {
    item: QueueItem,
    /// Where it was listed.
    index: usize,
    /// When it was removed.
    since: f64,
}

/// What the list showed, kept between frames to tell cards that come and go.
#[derive(Clone, Default)]
struct Rows {
    /// The pass that last drew the list; a gap means the page was away and starts over.
    pass: Option<u64>,
    /// The queue's revision then, and a copy of its downloads (only taken again when it moves).
    revision: u64,
    items: Vec<QueueItem>,
    ghosts: Vec<Ghost>,
    /// Cards that changed places: id, how many rows below its new place it was (above it when
    /// negative), and when.
    moves: Vec<(usize, f32, f64)>,
}

/// How long a card that changed places takes to glide to its new one.
const GLIDE: f64 = 0.3;

/// How many rows below its place (above it when negative) card `id` still shows while it glides
/// there (see [`Rows::moves`]).
fn glide(moves: &[(usize, f32, f64)], id: usize, now: f64) -> f32 {
    moves.iter().find(|m| m.0 == id).map_or(0.0, |&(_, from, since)| from * (1.0 - anim::ease(((now - since) / GLIDE) as f32)))
}

/// Notes what changed since the list was drawn on the frame before (`pass - 1`): removed
/// downloads become ghosts, cards that changed places glide there from where they showed, and the
/// ids returned are new. A list that was not drawn then (the page just opened) starts over:
/// nothing is new, removed or moved. No ghosts or glides under Reduce motion.
fn track(rows: &mut Rows, items: &[QueueItem], revision: u64, pass: u64, now: f64, reduced: bool) -> Vec<usize> {
    let continuous = rows.pass.is_some_and(|last| last + 1 == pass);
    let mut new = Vec::new();
    if !continuous || rows.revision != revision {
        if continuous {
            let ids: HashSet<usize> = items.iter().map(|item| item.id).collect();
            let old: HashSet<usize> = rows.items.iter().map(|item| item.id).collect();
            for (index, item) in rows.items.iter().enumerate().filter(|(_, item)| !ids.contains(&item.id)) {
                rows.ghosts.push(Ghost { item: item.clone(), index, since: now });
            }
            new = items.iter().map(|item| item.id).filter(|id| !old.contains(id)).collect();
            // Places among the cards there before and still: cards that come or go move none.
            let was: HashMap<usize, usize> =
                rows.items.iter().filter(|item| ids.contains(&item.id)).enumerate().map(|(place, item)| (item.id, place)).collect();
            for (place, item) in items.iter().filter(|item| old.contains(&item.id)).enumerate() {
                if was[&item.id] != place {
                    let from = was[&item.id] as f32 - place as f32 + glide(&rows.moves, item.id, now);
                    rows.moves.retain(|m| m.0 != item.id);
                    rows.moves.push((item.id, from, now));
                }
            }
        }
        rows.items = items.to_vec();
        rows.revision = revision;
    }
    rows.pass = Some(pass);
    rows.ghosts.retain(|ghost| !reduced && now - ghost.since < GHOST);
    rows.moves.retain(|&(_, _, since)| !reduced && now - since < GLIDE);
    new
}

/// One line of the list.
struct Line<'a> {
    item: &'a QueueItem,
    /// How far (0.0 to 1.0) the ghost of a removed download has faded out.
    ghost: Option<f32>,
    /// Added since the frame before: it fades in.
    new: bool,
    /// How many rows down it still is while ghosts above it close their gap (see [`GHOST`]), or
    /// it glides to a new place (see [`GLIDE`]).
    drop: f32,
}

/// The lines of the list: `items`, ghosts fading out where they were, the cards under a ghost
/// closing its gap pushed down by what is left of it, and cards that changed places on their
/// way; and how many rows of gap are still closing under the last line.
// ponytail: ghosts of separate removals less than GHOST apart may land a row off; placing them
// by their neighbours would be exact.
fn lines<'a>(items: &'a [QueueItem], rows: &'a Rows, new: &[usize], now: f64) -> (Vec<Line<'a>>, f32) {
    let mut lines: Vec<Line> = items.iter().map(|item| Line { item, ghost: None, new: new.contains(&item.id), drop: 0.0 }).collect();
    let mut ghosts: Vec<&Ghost> = rows.ghosts.iter().collect();
    ghosts.sort_by_key(|ghost| ghost.index);
    for ghost in ghosts {
        let t = ((now - ghost.since) / GHOST) as f32;
        lines.insert(ghost.index.min(lines.len()), Line { item: &ghost.item, ghost: Some(t), new: false, drop: 0.0 });
    }
    let mut drop = 0.0;
    lines.retain_mut(|line| match line.ghost {
        // Faded out: its card is gone, and what is under it slides up.
        Some(t) if t >= 0.5 => {
            drop += 1.0 - anim::ease(2.0 * t - 1.0);
            false
        }
        Some(t) => {
            line.ghost = Some(2.0 * t);
            true
        }
        None => {
            line.drop = drop + glide(&rows.moves, line.item.id, now);
            true
        }
    });
    (lines, drop)
}

/// How far (0.0 to 1.0) `item`'s card is into its status, which changed when its kind did (not
/// its progress or error text) or when it started or stopped `waiting`; and, just after it
/// failed, how far its card shakes sideways.
fn row_motion(ctx: &egui::Context, item: &QueueItem, waiting: bool) -> (f32, f32) {
    let id = egui::Id::new(("queue row", item.id));
    let kind = (std::mem::discriminant(&item.status), waiting);
    let t = anim::changed(ctx, id.with("status"), kind, 0.45);
    // Keyed by every change, so that failing again after a retry shakes again.
    let shake = anim::shake(ctx, id.with("shake"), kind);
    (t, if matches!(item.status, QueueItemStatus::Failed(_)) { shake } else { 0.0 })
}

/// The icon that leads a card, `t` (0.0 to 1.0) into its status: a spinner while it waits, an
/// arrow bobbing while data comes in (still when `stalled`), a check that draws itself on
/// completion; the others pop and fade in.
fn status_icon(ui: &mut Ui, item: &QueueItem, waiting: bool, stalled: bool, color: Color32, t: f32) {
    const SIZE: f32 = 18.0;
    if waiting {
        anim::spinner(ui, SIZE, color);
        return;
    }
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(SIZE), Sense::hover());
    let glyph = match item.status {
        QueueItemStatus::Completed => {
            ui.painter().circle_stroke(rect.center(), SIZE / 2.0 - 1.0, Stroke::new(1.5, color));
            anim::paint_check(ui.painter(), rect.shrink(4.0), anim::ease(t), Stroke::new(2.0, color));
            return;
        }
        QueueItemStatus::Downloading => icon::ARROW_DOWN,
        QueueItemStatus::Failed(_) => icon::WARNING_CIRCLE,
        QueueItemStatus::AuthRequired => icon::LOCK_SIMPLE,
        QueueItemStatus::Queued => icon::CLOCK,
        QueueItemStatus::Paused | QueueItemStatus::Pausing => icon::PAUSE,
    };
    let bob = if item.status == QueueItemStatus::Downloading && !stalled && ui.is_rect_visible(rect) { 2.0 - 4.0 * anim::pulse(ui.ctx(), 1.3) } else { 0.0 };
    let size = 16.0 * (0.6 + 0.4 * anim::overshoot(t));
    anim::paint_icon(ui.painter(), rect.center() + Vec2::new(0.0, bob), glyph, size, color.gamma_multiply(anim::ease(t)), 0.0);
}

/// A card's progress: with the pieces known, each connection's piece filling in its own colour
/// (a little towards the accent) on a thin track; else one bar. A light sweeps over what came
/// while bytes arrive.
fn segment_bar(ui: &mut Ui, id: egui::Id, item: &QueueItem, chunks: &[ChunkSnapshot], color: Color32, live: bool) {
    let p = palette(ui);
    let size = Vec2::new(ui.available_width(), 6.0);
    let ratio = item.progress_ratio as f32;
    if chunks.is_empty() || item.total_bytes == 0 || item.status == QueueItemStatus::Completed {
        progress_bar(ui, id, size, ratio, color, live, None);
        return;
    }
    // Keeps the shared bar's easing going for the details' bar.
    let shown = anim::value(ui.ctx(), id.with("progress"), ratio.clamp(0.0, 1.0), 0.35);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.rect_filled(rect, 3.0, p.border);
    let total = item.total_bytes as f32;
    let x = |bytes: u64| rect.left() + (bytes as f32 / total).clamp(0.0, 1.0) * rect.width();
    let mut filled = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let reach = chunk.range_start + chunk.downloaded_bytes.min(chunk.total_bytes);
        let (left, right) = (x(chunk.range_start), x(reach));
        if right <= left {
            continue;
        }
        let tone = match chunk.worker_id {
            _ if chunk.total_bytes > 0 && chunk.downloaded_bytes >= chunk.total_bytes => color,
            Some(worker) => lane(&p, worker).lerp_to_gamma(color, 0.35),
            None => color,
        };
        painter.rect_filled(Rect::from_x_y_ranges(left..=right, rect.y_range()), 0.0, tone);
        filled.push(left..=right);
    }
    // The light crosses the pieces that came, not the gaps between them.
    if live && shown > 0.0 {
        anim::sweep(ui, rect, |x| if filled.iter().any(|piece| piece.contains(&x)) { rect.height() / 2.0 } else { 0.0 });
    }
}

/// One card of the list: its status icon and name, its buttons on the right, the pieces filling
/// in under them, and its plain status line; or why it failed (in full on hover), or why one
/// restored without its Authorization header waits. A click selects it, a double click (or
/// Enter) shows it, and it can be dragged (see `queue_page`); its ⋯ button and a right click
/// open its [`order_menu`]. A High or Low priority shows by its name. Under the pointer it lifts
/// a little on a softer, deeper shadow. It fades in when new and out as a ghost, glows green on
/// completion and shakes on failure. Returns the card's response.
fn queue_row(app: &App, ui: &mut Ui, line: &Line, action: &mut Option<(usize, RowAction)>) -> egui::Response {
    let p = palette(ui);
    // Done once its bytes are in (see `util::checking`).
    let shown = util::shown(line.item);
    let item = &*shown;
    let ctx = ui.ctx().clone();
    let (slot, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let id = egui::Id::new(("queue row", item.id));
    let downloading = item.status == QueueItemStatus::Downloading;
    let waiting = item.status == QueueItemStatus::Pausing || downloading && (app.is_resolving(item.id) || item.is_finishing());
    let (t, shake) = row_motion(&ctx, item, waiting);
    let (opacity, shift) = match line.ghost {
        Some(gone) => (1.0 - anim::ease(gone), Vec2::new(24.0 * anim::ease(gone), 0.0)),
        None => {
            let shown = anim::ease(anim::progress(&ctx, anim::since(&ctx, id.with("in"), (), line.new), 0.0, anim::APPEAR));
            (shown, anim::RISE * (1.0 - shown))
        }
    };
    let stride = ROW_HEIGHT + GAP;
    let rect = slot.translate(shift + Vec2::new(shake, line.drop * stride));
    // Clicks go where the card is drawn, also while it slides into a closing gap.
    let response = ui.interact(rect, id.with("row"), Sense::click_and_drag());
    let mut ignored = None;
    let action = if line.ghost.is_some() { &mut ignored } else { action };
    if response.double_clicked() {
        *action = Some((item.id, RowAction::Show));
    } else if response.clicked() {
        *action = Some((item.id, RowAction::Select));
    }
    // Its place in the list, for its order menu; a ghost has none.
    let items = app.queue.items();
    let place = items.iter().position(|other| other.id == item.id).filter(|_| line.ghost.is_none());
    if let Some(index) = place {
        response.context_menu(|ui| order_menu(ui, item, index, items.len(), action));
    }
    // Pulses only in view: the list lays out one card past its bottom edge.
    let seen = ui.is_rect_visible(rect);
    // Near the list's top and bottom edges a card fades, so that the list melts into the page.
    let clip = ui.clip_rect();
    let edge = anim::edge_fade(rect, clip);
    let hover = anim::presence(&ctx, id.with("hover"), response.hovered() && line.ghost.is_none(), 0.15);
    // A card being dragged stays dimmed in its place until it is dropped.
    let opacity = if response.dragged() { 0.5 * opacity } else { opacity } * edge;
    let lifted = rect.translate(Vec2::new(0.0, -1.5 * hover));
    if seen {
        let painter = ui.painter().clone();
        let painter = painter.with_clip_rect(clip);
        let fade = |c: Color32| c.gamma_multiply(opacity);
        let shadow = card_shadow(ui, hover);
        let mut shade = shadow.as_shape(lifted, 14.0);
        shade.fill = fade(shade.fill);
        painter.add(shade);
        let fill = p.card.lerp_to_gamma(p.hover, 0.25 * hover);
        let flash = match item.status {
            QueueItemStatus::Completed => Some(p.green),
            QueueItemStatus::Failed(_) => Some(p.red),
            _ => None,
        };
        let fill = match flash.filter(|_| t < 1.0) {
            Some(color) => fill.lerp_to_gamma(color, 0.22 * (1.0 - anim::ease(t))),
            None => fill,
        };
        painter.rect(lifted, 14.0, fade(fill), Stroke::new(1.0, fade(p.border.lerp_to_gamma(p.dim, 0.5 * hover))));
    }
    let inner = egui::UiBuilder::new().max_rect(lifted.shrink2(Vec2::new(16.0, 9.0))).layout(Layout::top_down(Align::LEFT));
    let mut ui = ui.new_child(inner);
    ui.multiply_opacity(opacity);
    // The list spaces its cards; inside one the name, the bar and the status line sit close.
    ui.spacing_mut().item_spacing.y = 4.0;
    if app.selected == Some(item.id) && line.ghost.is_none() {
        ctx.data_mut(|d| d.insert_temp(super::ring_target_id(), lifted.expand(3.0)));
    }

    let item_id = egui::Id::new(("item", item.id));
    let (status, color) = status_badge(app, item, &p);
    let actions = match util::checking(line.item) {
        // Open, Show in its folder and Details: it leaves the list once its check is over.
        true => &row_actions(&item.status)[..3],
        false => row_actions(&item.status),
    };
    // A stream's card offers its track chooser too, left of the others.
    let tracks = crate::tracks::offered(item);
    // One whose file another download has from another host offers to join them (see `Twin`).
    let twin = app.twins.get(&item.id).filter(|twin| twin.offered);
    ui.horizontal(|ui| {
        status_icon(ui, item, waiting, app.stalled_for(item.id).is_some(), color, t);
        left_part(ui, (actions.len() + usize::from(tracks) + 2 * usize::from(twin.is_some()) + 1) as f32 * 30.0, |ui| {
            let mark = match item.priority {
                Priority::High => Some((icon::CARET_DOUBLE_UP, p.amber, "High priority: starts before the other waiting downloads, and gets more of a speed limit")),
                Priority::Low => Some((icon::CARET_DOUBLE_DOWN, p.dim, "Low priority: starts after the other waiting downloads, and gets less of a speed limit")),
                Priority::Normal => None,
            };
            if let Some((glyph, color, tip)) = mark {
                ui.add(egui::Label::new(RichText::new(glyph).color(color)).selectable(false)).on_hover_text(tip);
            }
            let name = egui::Label::new(bold(&item.filename).size(14.5).color(p.strong)).truncate().selectable(false);
            ui.add(name).on_hover_text(&item.filename);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let more = icon_button(ui, icon::DOTS_THREE_VERTICAL, "More: order, priority, new link", true);
            let menu = id.with("menu");
            if more.clicked() {
                ui.memory_mut(|m| m.toggle_popup(menu));
            }
            if let Some(index) = place {
                egui::popup_below_widget(ui, menu, &more, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                    ui.set_min_width(170.0);
                    order_menu(ui, item, index, items.len(), action);
                });
            }
            for &(glyph, tip, what) in actions.iter().rev() {
                if icon_button(ui, glyph, tip, true).clicked() {
                    *action = Some((item.id, what));
                }
            }
            if tracks && icon_button(ui, icon::SLIDERS_HORIZONTAL, "Choose tracks…", true).clicked() {
                *action = Some((item.id, RowAction::Tracks));
            }
            if let Some(twin) = twin {
                if icon_button(ui, icon::ARROWS_SPLIT, "Keep apart: download it on its own", true).clicked() {
                    *action = Some((item.id, RowAction::KeepApart));
                }
                let tip = format!("Download from both: its link becomes a second server for #{}, which downloads one file from both", twin.of);
                if icon_button(ui, icon::ARROWS_MERGE, &tip, true).clicked() {
                    *action = Some((item.id, RowAction::Join));
                }
            }
        });
    });
    ui.add_space(3.0);
    let bar = match item.status {
        QueueItemStatus::Completed => p.green,
        QueueItemStatus::Downloading => p.accent,
        QueueItemStatus::Failed(_) => p.red,
        _ => p.dim,
    };
    let bar = anim::color(&ctx, item_id.with("bar"), bar, 0.35);
    let chunks: &[ChunkSnapshot] = app.jobs.get(&item.id).map_or(&[], |view| &view.chunks);
    let arriving = downloading && seen && !waiting && app.stalled_for(item.id).is_none();
    segment_bar(&mut ui, item_id, item, chunks, bar, arriving);
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let chip = anim::color(&ctx, item_id.with("status chip"), color, 0.35);
        ui.add(egui::Label::new(bold(status).size(12.0).color(chip)).selectable(false));
        let (text, color) = match &item.status {
            QueueItemStatus::Failed(_) => (truncate_chars(&failure_text(item).unwrap_or_default(), 200), if item.link_expired() { p.amber } else { p.red }),
            QueueItemStatus::AuthRequired => ("Authorization header not saved: open Details to enter it again and resume".to_string(), p.amber),
            _ => (row_summary(app, item, eased_progress(ui, item_id, item.progress_ratio as f32) as f64), p.muted),
        };
        let label = ui.add(egui::Label::new(RichText::new(text).small().color(color)).truncate().selectable(false));
        match &item.status {
            QueueItemStatus::Failed(error) => {
                label.on_hover_text(error);
            }
            _ if !item.notes.is_empty() => {
                label.on_hover_text(item.notes.join("\n"));
            }
            _ => {}
        }
    });
    response
}

/// When everything still to come (see `DownloadQueue::remaining`) is done at the speed the
/// running downloads have together: "at least" when some have no known size, which are left out;
/// None without an estimate (no speed yet, or no size known).
fn finish_time(left: Remaining) -> Option<String> {
    let secs = util::eta_secs(left.bytes, 0, left.speed)?;
    Some(match left.unknown {
        0 => format!("All done in about {}", format_duration(secs)),
        n => format!("All done in at least {} ({} size unknown)", format_duration(secs), n),
    })
}

/// The list's status filters, by number (`App::queue_filter`).
const FILTERS: [&str; 5] = ["All", "Running", "Waiting", "Failed", "Done"];

/// Whether a download of `status` is in the filter `group` (see [`FILTERS`]).
fn in_group(group: usize, status: &QueueItemStatus) -> bool {
    match group {
        1 => status.is_active(),
        2 => matches!(status, QueueItemStatus::Queued | QueueItemStatus::Paused),
        3 => matches!(status, QueueItemStatus::Failed(_) | QueueItemStatus::AuthRequired),
        4 => *status == QueueItemStatus::Completed,
        _ => true,
    }
}

/// Whether the list shows `item` under the filter `group` and `search` (lower case), which looks
/// in its name and its links.
fn listed(item: &QueueItem, group: usize, search: &str) -> bool {
    let found = || item.filename.to_lowercase().contains(search) || item.urls.iter().any(|url| url.as_str().to_lowercase().contains(search));
    in_group(group, &util::shown(item).status) && (search.is_empty() || found())
}

/// The downloads the list shows, top to bottom: for Up and Down.
pub(super) fn visible_ids(app: &App) -> Vec<usize> {
    let search = app.queue_search.trim().to_lowercase();
    app.queue.items().iter().filter(|item| listed(item, app.queue_filter, &search)).map(|item| item.id).collect()
}

/// Whether the box for adding many links at once shows (from the toolbar's menu).
fn many_links_id() -> egui::Id {
    egui::Id::new("many links shown")
}

/// The toolbar over the cards: the status filters with their counts, a search, Clear completed,
/// and a menu with the rest (Clear all, running the queue by itself and how many at once, what
/// to do when it is done, adding many links at once); and, while downloads run, when everything
/// will be done.
fn toolbar(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let items = app.queue.items();
    let counts: Vec<usize> = (0..FILTERS.len()).map(|group| items.iter().filter(|item| in_group(group, &util::shown(item).status)).count()).collect();
    let running = app.queue.active_count();
    let finish = (running > 0).then(|| finish_time(app.queue.remaining(app.settings.auto_run_queue))).flatten();
    // On a narrow page the search and the buttons go on a line of their own, under the filters.
    let narrow = ui.available_width() < 820.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (group, name) in FILTERS.into_iter().enumerate() {
            let label = match group {
                0 => name.to_string(),
                _ => format!("{} {}", name, counts[group]),
            };
            ui.selectable_value(&mut app.queue_filter, group, label);
        }
        if !narrow {
            ui.add_space(8.0);
            search_and_tools(app, ui, 200.0);
        }
    });
    if narrow {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            search_and_tools(app, ui, 360.0);
        });
    }
    // Under the filters, small: when it will all be done, and what happens then.
    if finish.is_some() || app.when_done != WhenDone::Nothing {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            if let Some(text) = &finish {
                let about = "The bytes left of the running downloads and of those waiting to start, at the speed the running ones \
                             have together; paused and stopped ones are left out.";
                ui.label(RichText::new(text).small().color(p.muted)).on_hover_text(about);
            }
            if app.when_done != WhenDone::Nothing {
                ui.label(RichText::new(format!("Then: {}", app.when_done.name().to_lowercase())).small().color(p.amber));
            }
        });
    }
}

/// The toolbar's search (at most `widest` wide), Clear completed and its menu, on the right.
fn search_and_tools(app: &mut App, ui: &mut Ui, widest: f32) {
    let p = palette(ui);
    ui.label(RichText::new(icon::MAGNIFYING_GLASS).size(15.0).color(p.muted));
    let width = (ui.available_width() - 190.0).clamp(90.0, widest);
    ui.add(egui::TextEdit::singleline(&mut app.queue_search).hint_text("Filter by name or link").desired_width(width));
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.menu_button(icon::DOTS_THREE, |ui| {
            ui.set_min_width(230.0);
            if ui.button(format!("{} Clear all", icon::BROOM)).on_hover_text("Remove every download that is not running (files are kept)").clicked() {
                app.clear_queue(false);
                ui.close_menu();
            }
            if ui.button(format!("{} Add many links…", icon::LIST_PLUS)).clicked() {
                ui.data_mut(|d| d.insert_temp(many_links_id(), true));
                ui.close_menu();
            }
            ui.separator();
            ui.horizontal(|ui| {
                toggle(ui, &mut app.settings.auto_run_queue).on_hover_text("Start queued downloads by themselves");
                ui.label("Run the queue,");
                ui.add(egui::DragValue::new(&mut app.settings.max_concurrent).range(1..=8));
                ui.label("at once");
            });
            ui.separator();
            // For this session only; it applies once.
            ui.label(RichText::new("When the queue is done").small().color(p.muted));
            for (choice, name) in WhenDone::CHOICES {
                if ui.selectable_label(app.when_done == choice, name).clicked() {
                    app.when_done = choice;
                    ui.close_menu();
                }
            }
        });
        if ui.add(button("Clear completed")).clicked() {
            app.clear_queue(true);
        }
    });
}

/// The box for many links at once: one download a line, mirrors of one file on one line.
fn many_links(app: &mut App, ui: &mut Ui) {
    if !ui.data(|d| d.get_temp::<bool>(many_links_id())).unwrap_or(false) {
        return;
    }
    let p = palette(ui);
    ui.add_space(8.0);
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(bold("Add many links").color(p.strong));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, icon::X, "Close", true).clicked() {
                    ui.data_mut(|d| d.insert_temp(many_links_id(), false));
                }
            });
        });
        let about = "One download per line; links to the same file from several servers go on one line, separated by \
                     spaces. A .torrent or .metalink (link, file path, or file dropped on the window) adds every file it \
                     lists, a playlist or channel every video; a link with a range (img[001-120].jpg, [a-f], [0-100:10]) \
                     every link it stands for. Uses the save folder and Settings.";
        ui.add(egui::Label::new(RichText::new(about).small().color(p.muted)).wrap());
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut app.queue_input)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("https://example.com/file1.iso\nhttps://example.com/file2.zip https://mirror.example.org/file2.zip"),
        );
        ui.add_space(6.0);
        if ui.add(primary_button(&format!("{} Add to queue", icon::PLUS), p.accent)).clicked() {
            app.add_queue_input();
        }
        if let Some(error) = app.queue_error.clone() {
            error_alert(ui, &error);
        }
    });
}

/// The empty list teaches: the welcome for a new user, then what can go in the bar, with three
/// things to try that fill it in.
fn empty_state(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    if !app.settings.welcomed {
        super::add::welcome(app, ui);
    }
    anim::fade_in(ui, egui::Id::new("queue empty"), 0.0, anim::RISE, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(26.0);
            ui.label(RichText::new(icon::DOWNLOAD_SIMPLE).size(30.0).color(p.dim));
            ui.label(bold("Nothing downloading yet").size(16.0).color(p.text));
            let about = "Paste a link in the bar above, drop a file on the window, or press Ctrl+V anywhere.";
            ui.label(RichText::new(keys(about)).color(p.muted));
            ui.add_space(14.0);
            ui.label(RichText::new("Try one of these").small().color(p.muted));
            ui.add_space(4.0);
        });
        let tries = ["A file", "A video", "A torrent"].map(|name| kinds::KINDS.iter().find(|kind| kind.name == name));
        ui.horizontal_wrapped(|ui| {
            let width: f32 = 3.0 * 220.0;
            ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
            for (n, kind) in tries.into_iter().flatten().enumerate() {
                anim::fade_in(ui, egui::Id::new(("try", n)), anim::stagger(n + 1), anim::RISE, |ui| {
                    let text = RichText::new(format!("{} {}", kind.glyph, kind.name)).color(p.text);
                    let chosen = ui.add_sized([210.0, 40.0], button(text)).on_hover_text(kind.example);
                    if chosen.clicked() {
                        app.url_input = kind.example.to_string();
                        app.form_error = None;
                    }
                });
            }
        });
    });
}

pub(super) fn queue_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    if !app.queue.items().is_empty() {
        toolbar(app, ui);
    }
    many_links(app, ui);
    ui.add_space(8.0);
    let items = app.queue.items();
    let search = app.queue_search.trim().to_lowercase();
    let group = app.queue_filter;
    let filtering = group != 0 || !search.is_empty();

    let mut action = None;
    let shown: &App = app;
    let ctx = ui.ctx().clone();
    let list = egui::Id::new("queue rows");
    let mut rows: Rows = ctx.data_mut(|d| d.remove_temp(list)).unwrap_or_default();
    let now = ctx.input(|i| i.time);
    let new = track(&mut rows, items, shown.queue.revision(), ctx.cumulative_pass_nr(), now, anim::reduced(&ctx));
    if !rows.ghosts.is_empty() || !rows.moves.is_empty() {
        ctx.request_repaint();
    }
    let (lines, closing) = lines(items, &rows, &new, now);
    let lines: Vec<_> = lines.into_iter().filter(|line| !filtering || listed(line.item, group, &search)).collect();
    if lines.is_empty() {
        ctx.data_mut(|d| d.insert_temp(list, rows));
        if items.is_empty() {
            egui::ScrollArea::vertical().id_salt("empty downloads").auto_shrink([false, false]).show(ui, |ui| empty_state(app, ui));
        } else {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                anim::fade_in(ui, egui::Id::new(("queue empty", "filtered")), 0.0, anim::RISE, |ui| {
                    ui.label(RichText::new(icon::MAGNIFYING_GLASS).size(28.0).color(p.dim));
                    ui.label(bold("No download matches").color(p.text));
                    ui.label(RichText::new("Clear the filter above to see them all.").color(p.muted));
                });
            });
        }
        return;
    }
    ui.spacing_mut().item_spacing.y = GAP;
    let scroll = egui::ScrollArea::vertical().id_salt("queue_rows").auto_shrink([false, false]).max_height(ui.available_height().max(120.0));
    scroll.show_rows(ui, ROW_HEIGHT, lines.len(), |ui, range| {
        ui.set_max_width(ui.available_width() - 10.0);
        let stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
        // Where the list's first card is, in view or not.
        let top = ui.cursor().min.y - range.start as f32 * stride;
        let mut dragged = None;
        let first = ui.cursor().min;
        for line in &lines[range.clone()] {
            let row = queue_row(shown, ui, line, &mut action);
            // A narrowed list is not reordered by dragging: its gaps are not the queue's.
            if !filtering && line.ghost.is_none() && (row.dragged() || row.drag_stopped()) {
                dragged = Some((line.item.id, row.drag_stopped()));
            }
        }
        // The selection's outline sits on its card as drawn, and slides over from the card
        // selected before: from where it was then, relative to the new card as that moves.
        if let Some(index) = lines.iter().position(|line| line.ghost.is_none() && shown.selected == Some(line.item.id)) {
            let row = index as f32 + lines[index].drop;
            let key = list.with("selection");
            let t = anim::changed(ui.ctx(), key, shown.selected, 0.2);
            let (last, from) = ui.data(|d| d.get_temp::<(f32, f32)>(key.with("at"))).unwrap_or((row, 0.0));
            let from = if t == 0.0 { last - row } else { from };
            let y = row + from * (1.0 - anim::ease(t));
            ui.data_mut(|d| d.insert_temp(key.with("at"), (y, from)));
            let at = y - range.start as f32;
            let rect = Rect::from_min_size(first + Vec2::new(0.0, at * stride), Vec2::new(ui.available_width(), ROW_HEIGHT));
            ui.painter().rect(rect, 14.0, p.accent.gamma_multiply(0.07), Stroke::new(1.5, p.link.gamma_multiply(0.85)));
        }
        // A card being dragged lands in the gap nearest the pointer, which a line shows;
        // dropped there, it moves.
        if let (Some((id, dropped)), Some(pointer)) = (dragged, ui.ctx().pointer_latest_pos()) {
            let from = items.iter().position(|item| item.id == id).unwrap_or_default();
            let gap = ((pointer.y - top) / stride).round().clamp(0.0, items.len() as f32) as usize;
            let to = if gap > from { gap - 1 } else { gap };
            if to != from {
                let y = top + gap as f32 * stride - ui.spacing().item_spacing.y / 2.0;
                let (left, right) = (ui.max_rect().left() + 4.0, ui.max_rect().right() - 4.0);
                ui.painter().line_segment([Pos2::new(left, y), Pos2::new(right, y)], Stroke::new(2.0, p.accent));
                ui.painter().circle_filled(Pos2::new(left, y), 4.0, p.accent);
                if dropped {
                    action = Some((id, RowAction::MoveTo(to)));
                }
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        // The list's end comes up with the gap closing under its last line, so that a list
        // scrolled to its end does not jump as a removed card's room goes.
        ui.expand_to_include_y(ui.min_rect().bottom() + closing * stride);
    });
    ctx.data_mut(|d| d.insert_temp(list, rows));

    let Some((id, action)) = action else { return };
    app.selected = Some(id);
    let target = app.queue.get_item(id).and_then(|item| item.target_path.clone());
    match action {
        RowAction::Select => {}
        RowAction::Start => {
            app.start_job(id, false);
        }
        RowAction::Pause => app.pause_job(id),
        RowAction::Show => app.show_job(id),
        RowAction::Next => {
            app.queue.download_next(id);
        }
        RowAction::MoveTo(index) => {
            app.queue.move_item(id, index);
        }
        RowAction::Priority(priority) => {
            if let Some(item) = app.queue.get_item_mut(id) {
                item.priority = priority;
            }
        }
        RowAction::Remove => {
            app.remove_job(id);
            if app.focused == Some(id) {
                app.focused = None;
            }
        }
        RowAction::Open => {
            if let Some(path) = target {
                report(app, util::open_path(&path));
            }
        }
        RowAction::Reveal => {
            if let Some(path) = target {
                report(app, util::reveal_in_folder(&path));
            }
        }
        RowAction::NewLink => {
            app.show_job(id);
            app.new_link = Some(crate::NewLink::new(id));
        }
        RowAction::OpenPage => {
            if let Some(page) = app.queue.get_item(id).and_then(page_of) {
                ui.ctx().open_url(egui::OpenUrl::new_tab(page));
            }
        }
        RowAction::Tracks => {
            app.show_job(id);
            // Open already: listing again would throw away what is ticked.
            let open = app.tracks.as_ref().is_some_and(|chooser| chooser.item == Some(id));
            if let Some(url) = app.queue.get_item(id).and_then(|item| item.urls.first().cloned()).filter(|_| !open) {
                app.list_tracks(Some(id), url);
            }
        }
        RowAction::Join => app.join_twin(id),
        RowAction::KeepApart => app.keep_apart(id),
    }
}

/// Opens the toolbar's ⋯ menu and clicks `entry` in it.
#[cfg(test)]
pub(super) fn from_toolbar_menu(h: &mut super::harness::Harness, entry: &str) {
    h.click(icon::DOTS_THREE);
    // A menu is laid out unseen the first time.
    h.frames(2);
    h.click(entry);
    h.frames(2);
}

#[cfg(test)]
mod tests {
    use hyperfetch_core::engine::DownloadOptions;
    use hyperfetch_core::queue::DownloadQueue;

    use super::super::harness::Harness;
    use super::*;

    fn add(queue: &mut DownloadQueue, name: &str) -> usize {
        queue.add_item(vec![url::Url::parse(&format!("https://example.com/{}", name)).unwrap()], DownloadOptions::default())
    }

    /// What to do when the queue is done is picked in the toolbar's menu; once it is done, the
    /// countdown shows over every page until Cancel.
    #[test]
    fn the_queue_done_choice_counts_down_until_cancelled() {
        let mut h = Harness::new();
        h.add("first.iso");
        assert!(h.settle());
        from_toolbar_menu(&mut h, "Shut down");
        assert_eq!(h.app.when_done, WhenDone::ShutDown);
        assert!(h.find("Then: shut down").is_some(), "{:?}", h.texts());
        let id = h.add("big.iso");
        h.progress(id, 0.5);
        h.app.queue.remove_item(1);
        h.app.handle_event(crate::AppEvent::JobFinished { id, result: Ok((std::env::temp_dir().join("big.iso"), Some(1))) });
        assert!(h.app.countdown.is_some());
        h.app.tab = Tab::Settings;
        assert!(h.settle());
        assert!(h.texts().iter().any(|(text, _)| text.starts_with("The queue is done. Shutting down in ")), "{:?}", h.texts());
        h.click("Cancel");
        assert!(h.app.countdown.is_none());
        h.wait_for_background();
    }

    /// While downloads run, the toolbar says when everything to come is done at their speed
    /// together: "about", or "at least" when some have no known size yet, which are left out;
    /// waiting ones count only when the queue starts them by itself. With nothing running it
    /// says nothing.
    #[test]
    fn the_header_says_when_the_queue_will_be_done() {
        let mut h = Harness::new();
        h.app.settings.auto_run_queue = true;
        let [a, _] = ["a.iso", "b.iso"].map(|name| h.add(name));
        let done = |h: &mut Harness| {
            h.frames(2);
            h.texts().into_iter().map(|(text, _)| text).find(|text| text.starts_with("All done"))
        };
        assert_eq!(done(&mut h), None, "nothing runs");
        // 50 MiB left at 3 MB/s.
        h.progress(a, 0.5);
        assert_eq!(done(&mut h).as_deref(), Some("All done in at least 00:18 (1 size unknown)"));
        h.app.settings.auto_run_queue = false;
        assert_eq!(done(&mut h).as_deref(), Some("All done in about 00:18"));
        h.finish(a, None);
        assert_eq!(done(&mut h), None);
    }

    /// "Why is it slow?" in a running download's details lists what holds it back: here the
    /// speed limit, which it runs at. The fix lifts the limit for this download, which pauses and
    /// at once starts again from where it was, without it.
    #[test]
    fn why_is_it_slow_finds_the_speed_limit_and_lifts_it() {
        let mut h = Harness::new();
        // Started again, it fails at once instead of fetching anything.
        h.app.engines = std::sync::Arc::new(|_: Vec<url::Url>, _: DownloadOptions| Err("not downloaded here".to_string()));
        let id = h.add("big.iso");
        h.app.queue.get_item_mut(id).unwrap().options.max_speed = Some(3_000_000);
        // At 3 MB/s.
        h.progress(id, 0.4);
        h.app.focused = Some(id);
        h.frames(10);
        assert!(h.find("What holds it back").is_none());
        h.click("Why is it slow?");
        h.frames(30);
        let lines: Vec<String> = h.texts().into_iter().map(|(text, _)| text).collect();
        assert!(lines.iter().any(|text| text.starts_with("Your speed limit holds it")), "{lines:?}");
        h.click("Lift the limit for it");
        let item = h.app.queue.get_item(id).unwrap();
        assert_eq!((item.options.max_speed, item.status.clone()), (None, QueueItemStatus::Pausing));
        h.app.handle_event(crate::AppEvent::JobFinished { id, result: Err(hyperfetch_core::engine::CANCELLED.to_string()) });
        assert_eq!(h.app.queue.get_item(id).unwrap().status, QueueItemStatus::Downloading, "started again at once");
        h.wait_for(|app| matches!(app.queue.get_item(id).unwrap().status, QueueItemStatus::Failed(_)));
        h.wait_for_background();
    }

    /// A running download's details say on one line why it has the connections it has, as the
    /// engine's Autopilot last told it; its timeline says it too.
    #[test]
    fn a_running_download_says_why_it_has_its_connections() {
        let mut h = Harness::new();
        let id = h.add("big.iso");
        h.progress(id, 0.4);
        let why = "2 connections: the server limits each user to about 3.8 MiB/s, so more would not help";
        h.app.jobs.get_mut(&id).unwrap().autopilot = Some(why.to_string());
        h.app.focused = Some(id);
        h.frames(10);
        let lines: Vec<String> = h.texts().into_iter().map(|(text, _)| text).collect();
        let at = lines.iter().position(|text| text == "Autopilot").expect("labelled");
        assert_eq!(lines.get(at + 1).map(String::as_str), Some(why), "{lines:?}");
        // Further down its page.
        let events = h.app.ctx.data(|d| d.get_temp::<Timeline>(timeline_id())).unwrap().events[&id].iter().map(|(_, what)| what.clone()).collect::<Vec<_>>();
        assert!(events.contains(&format!("Autopilot: {why}")), "in its timeline: {events:?}");
    }

    /// A link added while another download, running or queued, has its file from another host
    /// (the same name, size and bytes, neither with a credential) waits, its card offering to
    /// download the one file from both; joined, its link is the other's second server and it
    /// leaves the queue, which Undo takes back. Another size, other bytes, a credential or a
    /// stopped download is no twin; Keep apart lets one start on its own.
    #[test]
    fn the_same_file_from_another_host_is_offered_to_join() {
        let server = tokio::runtime::Runtime::new().unwrap();
        let port = server.block_on(crate::tests::serve(std::sync::Arc::new(vec![7u8; 4096]))).port();
        let mut h = Harness::new();
        h.app.settings.auto_run_queue = false;
        let first = h.app.add_download(&format!("http://127.0.0.1:{port}/same.bin"), "", "").unwrap();
        assert!(h.app.twins.is_empty(), "nothing else has its file");
        h.app.queue.get_item_mut(first).unwrap().total_bytes = 4096;
        let twin = h.app.add_download(&format!("http://localhost:{port}/same.bin"), "", "").unwrap();
        h.wait_for(|app| app.twins.get(&twin).is_some_and(|twin| twin.offered));
        h.frames(10);
        let lines: Vec<String> = h.texts().into_iter().map(|(text, _)| text).collect();
        assert!(lines.iter().any(|text| text.contains(&format!("waits: #{first} has this file from 127.0.0.1; download it from both?"))), "{lines:?}");
        h.click(icon::ARROWS_MERGE);
        assert!(h.app.queue.get_item(twin).is_none());
        let urls: Vec<String> = h.app.queue.get_item(first).unwrap().urls.iter().map(|url| url.to_string()).collect();
        assert_eq!(urls, [format!("http://127.0.0.1:{port}/same.bin"), format!("http://localhost:{port}/same.bin")]);
        // Undo puts it back and takes its link back.
        assert!(h.app.removed.is_some(), "Undo is offered");
        h.app.undo_remove();
        assert!(h.app.queue.get_item(twin).is_some());
        assert_eq!(h.app.queue.get_item(first).unwrap().urls.len(), 1);
        h.app.remove_job(twin);
        h.app.removed = None;

        // The same size, other bytes: no offer.
        let other_port = server.block_on(crate::tests::serve(std::sync::Arc::new(vec![8u8; 4096]))).port();
        let other_bytes = h.app.add_download(&format!("http://localhost:{other_port}/same.bin"), "", "").unwrap();
        assert!(h.app.twins.contains_key(&other_bytes), "compared first");
        h.wait_for(|app| !app.twins.contains_key(&other_bytes));
        h.app.remove_job(other_bytes);
        h.app.removed = None;
        // A credential on either, or a download stopped: nothing is asked.
        let signed = h.app.add_download(&format!("http://localhost:{port}/same.bin?v=4"), "", "Bearer secret").unwrap();
        assert!(!h.app.twins.contains_key(&signed), "its credential would go to the other's server");
        h.app.remove_job(signed);
        h.app.queue.get_item_mut(first).unwrap().status = QueueItemStatus::Paused;
        let stopped = h.app.add_download(&format!("http://localhost:{port}/same.bin?v=5"), "", "").unwrap();
        assert!(!h.app.twins.contains_key(&stopped), "only into a download running or queued");
        h.app.remove_job(stopped);
        h.app.queue.get_item_mut(first).unwrap().status = QueueItemStatus::Queued;
        h.app.removed = None;

        // Another size: no offer.
        h.app.queue.get_item_mut(first).unwrap().total_bytes = 1000;
        let other = h.app.add_download(&format!("http://localhost:{port}/same.bin?v=2"), "", "").unwrap();
        assert!(h.app.twins.contains_key(&other), "asked first");
        h.wait_for(|app| !app.twins.contains_key(&other));
        h.app.remove_job(other);
        // Without its Undo banner, whose X closes it.
        h.app.removed = None;

        h.app.queue.get_item_mut(first).unwrap().total_bytes = 4096;
        let apart = h.app.add_download(&format!("http://localhost:{port}/same.bin?v=3"), "", "").unwrap();
        h.wait_for(|app| app.twins.get(&apart).is_some_and(|twin| twin.offered));
        // The removed cards faded out and their gaps closed (see `GHOST`), so the card stays put.
        h.frames(60);
        h.click(icon::ARROWS_SPLIT);
        assert!(h.app.twins.is_empty() && h.app.queue.get_item(apart).is_some());
        h.wait_for_background();
    }

    /// A download whose bytes are in shows as done while the site's checksum is checked: its card
    /// says Done and opens its file, with no Pause nor Remove, and the bar counts it done.
    #[test]
    fn a_download_in_place_shows_as_done() {
        let mut h = Harness::new();
        let id = h.add("disk.img");
        h.progress(id, 1.0);
        let path = std::env::temp_dir().join("disk.img");
        let snapshot = hyperfetch_core::engine::EngineSnapshot { total_bytes: 100 << 20, downloaded_bytes: 100 << 20, target_path: Some(path), in_place: true, ..Default::default() };
        h.app.queue.apply_snapshot(id, &snapshot);
        h.app.tab = crate::Tab::Downloads;
        h.frames(30);
        let lines: Vec<String> = h.texts().into_iter().map(|(text, _)| text).collect();
        assert!(lines.iter().any(|text| text == "Done") && !lines.iter().any(|text| text == "Downloading"), "{lines:?}");
        assert!(lines.iter().any(|text| text.contains("1 done")), "{lines:?}");
        assert!(h.find(icon::FILE).is_some() && h.find(icon::PAUSE).is_none() && h.find_near("disk.img", icon::X).is_none(), "{lines:?}");
    }

    /// A running download's own speed limit and connections, set in its details, are kept with it
    /// and handed to the running engine at once, without pausing it.
    #[test]
    fn a_running_downloads_own_limits_apply_at_once() {
        let mut h = Harness::new();
        let id = h.add("big.iso");
        h.progress(id, 0.4);
        h.app.focused = Some(id);
        h.frames(10);
        for _ in 0..20 {
            if h.find("Apply").is_some() {
                break;
            }
            h.hover(egui::Pos2::new(h.size.x * 0.8, h.size.y * 0.6));
            h.frame(vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::Vec2::new(0.0, -300.0), modifiers: egui::Modifiers::NONE }]);
            h.frames(20);
        }
        assert!(h.find("For this download").is_some());
        // As if 1 MB/s and 4 connections were typed into its boxes.
        h.app.ctx.data_mut(|d| d.insert_temp(egui::Id::new(("own limits", id)), (1.0_f64, 4_usize)));
        h.frames(2);
        h.click("Apply");
        let item = h.app.queue.get_item(id).unwrap();
        assert_eq!((item.options.own_speed, item.options.num_connections, item.status.clone()), (Some(1 << 20), 4, QueueItemStatus::Downloading));
        let live = &h.app.jobs[&id].running.as_ref().unwrap().live;
        assert_eq!((live.speed(), live.connections()), (Some(1 << 20), Some(4)));
    }

    /// The timeline notes a download's news as it comes: added, started, finished; not its
    /// progress.
    #[test]
    fn the_timeline_notes_what_happens() {
        let mut h = Harness::new();
        let id = h.add("big.iso");
        h.frames(2);
        h.progress(id, 0.4);
        h.frames(2);
        h.progress(id, 0.6);
        h.frames(2);
        h.finish(id, None);
        h.frames(2);
        let events = h.app.ctx.data(|d| d.get_temp::<Timeline>(timeline_id())).unwrap().events[&id].iter().map(|(_, what)| what.clone()).collect::<Vec<_>>();
        let (at, total) = (format_bytes(40 << 20), format_bytes(100 << 20));
        assert_eq!(events, ["Added to the list".to_string(), format!("Resumed at {at}"), format!("Finished: {total}")]);
    }

    /// Each line's file name, whether it is a ghost, and how many rows down it is pushed.
    fn shown(queue: &DownloadQueue, rows: &Rows, now: f64) -> Vec<(String, bool, f32)> {
        lines(queue.items(), rows, &[], now).0.iter().map(|line| (line.item.filename.clone(), line.ghost.is_some(), line.drop)).collect()
    }

    /// Removed downloads linger where they were, fade out, then close their gap as the cards
    /// under them slide up; added ones are new. Nothing is new or removed when the list starts
    /// over, and there are no ghosts under Reduce motion.
    #[test]
    fn removed_rows_linger_as_ghosts_and_added_ones_are_new() {
        let mut queue = DownloadQueue::new();
        let ids: Vec<usize> = ["a", "b", "c", "d"].iter().map(|name| add(&mut queue, name)).collect();
        let mut rows = Rows::default();
        assert!(track(&mut rows, queue.items(), queue.revision(), 1, 0.0, false).is_empty(), "the first sight is not news");
        assert!(track(&mut rows, queue.items(), queue.revision(), 2, 0.5, false).is_empty());

        queue.remove_item(ids[1]);
        queue.remove_item(ids[3]);
        let e = add(&mut queue, "e");
        assert_eq!(track(&mut rows, queue.items(), queue.revision(), 3, 1.0, false), [e]);
        let row = |name: &str, ghost, drop| (name.to_string(), ghost, drop);
        let all = [row("a", false, 0.0), row("b", true, 0.0), row("c", false, 0.0), row("d", true, 0.0), row("e", false, 0.0)];
        assert_eq!(shown(&queue, &rows, 1.0), all, "the ghosts stay where they were");

        // Half way through closing their gaps: c is pushed down by b's, e by both.
        track(&mut rows, queue.items(), queue.revision(), 4, 1.0 + GHOST * 0.75, false);
        let closing = shown(&queue, &rows, 1.0 + GHOST * 0.75);
        let names: Vec<_> = closing.iter().map(|(name, ghost, _)| (name.as_str(), *ghost)).collect();
        assert_eq!(names, [("a", false), ("c", false), ("e", false)]);
        let gap = 1.0 - anim::ease(0.5);
        assert!(closing[0].2 == 0.0 && (closing[1].2 - gap).abs() < 1e-6 && (closing[2].2 - 2.0 * gap).abs() < 1e-6, "{closing:?}");

        track(&mut rows, queue.items(), queue.revision(), 5, 1.01 + GHOST, false);
        assert!(rows.ghosts.is_empty() && shown(&queue, &rows, 1.01 + GHOST).iter().all(|(_, ghost, drop)| !ghost && *drop == 0.0));

        // The page was away for a while (frames went by without the list): it starts over.
        queue.remove_item(ids[0]);
        add(&mut queue, "f");
        assert!(track(&mut rows, queue.items(), queue.revision(), 9, 5.0, false).is_empty());
        assert!(rows.ghosts.is_empty());

        queue.remove_item(ids[2]);
        track(&mut rows, queue.items(), queue.revision(), 10, 6.0, true);
        assert!(rows.ghosts.is_empty(), "no ghosts under Reduce motion");
    }

    /// A card's status changes when its kind does, not its progress or error text: then it runs
    /// from 0.0 to 1.0, and a failure shakes it; retrying does not. Seen first, nothing moves.
    #[test]
    fn a_row_reacts_to_a_new_status_only() {
        let ctx = egui::Context::default();
        let at = |time: f64, item: &QueueItem| {
            let mut out = (0.0, 0.0);
            let _ = ctx.run(egui::RawInput { time: Some(time), ..Default::default() }, |ctx| out = row_motion(ctx, item, false));
            out
        };
        let mut queue = DownloadQueue::new();
        let id = add(&mut queue, "a.iso");
        queue.mark_started(id);
        let mut item = queue.get_item(id).unwrap().clone();
        assert_eq!(at(0.0, &item), (1.0, 0.0), "seen first");
        item.progress_ratio = 0.5;
        item.downloaded_bytes = 1 << 20;
        assert_eq!(at(0.1, &item), (1.0, 0.0), "progress is no news");

        item.status = QueueItemStatus::Completed;
        assert_eq!(at(1.0, &item).0, 0.0);
        assert!((at(1.0 + 0.45 / 2.0, &item).0 - 0.5).abs() < 1e-3);
        assert_eq!(at(2.0, &item), (1.0, 0.0));

        item.status = QueueItemStatus::Failed("404".to_string());
        at(3.0, &item);
        let (t, shake) = at(3.05, &item);
        assert!(t < 1.0 && shake != 0.0, "a failure shakes: {shake}");
        assert_eq!(at(4.0, &item), (1.0, 0.0));
        item.status = QueueItemStatus::Failed("timed out".to_string());
        assert_eq!(at(4.1, &item), (1.0, 0.0), "another error is the same status");

        item.status = QueueItemStatus::Downloading;
        at(5.0, &item);
        let (t, shake) = at(5.05, &item);
        assert!(t < 1.0 && shake == 0.0, "a retry does not shake");
    }

    /// The Downloads page with downloads named item-00.iso and on, its list scrolled `by` points
    /// down with the mouse wheel, settled; their ids.
    fn scrolled(h: &mut Harness, rows: usize, by: f32) -> Vec<usize> {
        let ids = (0..rows).map(|n| h.add(&format!("item-{n:02}.iso"))).collect();
        h.frames(30);
        h.hover(h.find("item-00.iso").unwrap().center());
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, -by), modifiers: egui::Modifiers::NONE };
        h.frame(vec![wheel]);
        // The wheel's turn is eased over several frames.
        h.frames(60);
        h.leave();
        assert!(h.settle());
        ids
    }

    /// While the gap of removed cards closes, a card under it is drawn lower than its place in
    /// the list: a click where it shows selects it, and the selection's outline sits on it.
    #[test]
    fn a_row_sliding_into_a_closing_gap_takes_clicks_where_it_shows() {
        let mut h = Harness::new();
        for name in ["a.iso", "b.iso", "c.iso"] {
            let id = h.add(name);
            h.progress(id, 1.0);
            h.finish(id, None);
        }
        let run = h.add("run.iso");
        h.progress(run, 0.5);
        h.frames(60);
        h.app.clear_queue(true);
        // Past the ghosts' fade, a quarter into the closing.
        h.frames(16);
        h.click_at(h.find("run.iso").unwrap().center());
        assert_eq!(h.app.selected, Some(run));
        h.frame(Vec::new());
        let drawn = h.find("run.iso").unwrap().top();
        let selection = egui::Id::new("queue rows").with("selection").with("at");
        let (y, _) = h.app.ctx.data(|d| d.get_temp::<(f32, f32)>(selection)).unwrap();
        h.frames(30);
        let down = drawn - h.find("run.iso").unwrap().top();
        assert!(down > 20.0, "still sliding: {down}");
        // Within the card's lift under the pointer.
        assert!((y * (ROW_HEIGHT + GAP) - down).abs() < 2.0, "the outline is {y} rows down, the card {down} pt");
    }

    /// With the list scrolled to its end, a card removed near the bottom closes its room without
    /// a jump: the cards above come down smoothly, those under it stay about where they were.
    #[test]
    fn a_list_scrolled_to_its_end_closes_a_gap_without_jumping() {
        let mut h = Harness::new();
        scrolled(&mut h, 12, 2000.0);
        let at = |h: &Harness| [h.find("item-09.iso").unwrap().top(), h.find("item-11.iso").unwrap().top()];
        let [above, under] = at(&h);
        h.click_at(h.find_near("item-10.iso", icon::X).unwrap().center());
        h.leave();
        assert_eq!(h.app.queue.items().len(), 11);
        let mut last = above;
        for _ in 0..40 {
            h.frame(Vec::new());
            let [now_above, now_under] = at(&h);
            assert!((now_above - last).abs() < 24.0, "the cards above jump: {last} to {now_above}");
            assert!((now_under - under).abs() < 24.0, "the cards under move: {under} to {now_under}");
            last = now_above;
        }
        assert!((last - above - (ROW_HEIGHT + GAP)).abs() < 0.5, "the cards above end a card lower: {above} to {last}");
        assert_eq!(at(&h)[1], under);
    }

    /// A running download in the card the list lays out just past its bottom edge asks for no
    /// frames: nothing that moves shows. (Scrolled a third of a card, that card is out of sight.)
    #[test]
    fn a_running_row_out_of_view_keeps_still() {
        let mut h = Harness::new();
        let ids = scrolled(&mut h, 12, 30.0);
        let row = |h: &Harness, id: usize| h.app.ctx.read_response(egui::Id::new(("queue row", id)).with("row"));
        let hidden = ids.iter().copied().find(|&id| row(&h, id).is_some_and(|r| !r.interact_rect.is_positive()));
        let hidden = hidden.expect("a card laid out out of view");
        h.progress(hidden, 0.5);
        assert_eq!(h.frames(60), Duration::MAX);
    }

    /// `n` frames, and what `f` sees after each.
    fn over<R>(h: &mut Harness, n: usize, f: impl Fn(&Harness) -> R) -> Vec<R> {
        (0..n).map(|_| (h.frame(Vec::new()), f(&*h)).1).collect()
    }

    /// A download added while the list shows fades into its card rising from a little below,
    /// and the Downloads count in the rail pops; then frames stop. Under Reduce motion both are
    /// in place at once.
    #[test]
    fn a_new_download_rises_into_its_row_and_the_count_pops() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            h.add("a.iso");
            assert!(h.settle());
            let place = h.find("a.iso").unwrap().top() + ROW_HEIGHT + GAP;
            h.add("b.iso");
            let seen = over(&mut h, 30, |h| (h.find("b.iso").map(|rect| rect.top()), h.find_near("Downloads", "2").unwrap().height()));
            let tops: Vec<f32> = seen.iter().filter_map(|(top, _)| *top).collect();
            let heights: Vec<f32> = seen.iter().map(|(_, height)| *height).collect();
            let (rest, last) = (heights[heights.len() - 1], tops[tops.len() - 1]);
            assert_eq!(last, place);
            if reduce {
                assert!(tops.len() == 30 && tops.iter().all(|&top| top == place), "in place at once: {tops:?}");
                assert!(heights.iter().all(|&height| height == rest), "no pop: {heights:?}");
            } else {
                assert!(tops[0] > place + 2.0 && tops.windows(2).all(|w| w[0] >= w[1]), "rises: {tops:?}");
                assert!(heights.iter().any(|&height| height > rest + 1.0), "the count pops: {heights:?}");
            }
            assert_eq!(h.frames(60), Duration::MAX, "and keeps still");
        }
    }

    /// Removing a download fades its card out, then the cards under it glide up into its room,
    /// none jumping; then frames stop. Under Reduce motion they are in their new places at once.
    #[test]
    fn rows_glide_up_into_a_removed_rows_room() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            for name in ["a.iso", "b.iso", "c.iso", "d.iso"] {
                h.add(name);
            }
            assert!(h.settle());
            let stride = ROW_HEIGHT + GAP;
            let place = h.find("d.iso").unwrap().top() - stride;
            h.click_at(h.find_near("b.iso", icon::X).unwrap().center());
            assert_eq!(h.app.queue.items().len(), 3);
            let tops = over(&mut h, 40, |h| h.find("d.iso").unwrap().top());
            assert_eq!(tops[tops.len() - 1], place);
            if reduce {
                assert!(tops.iter().all(|&top| top == place), "at once: {tops:?}");
            } else {
                assert_eq!(tops[0], place + stride, "it waits while the removed card fades");
                assert!(tops.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < stride / 3.0), "glides: {tops:?}");
            }
            h.leave();
            // The Undo note asks for a frame when its time is up: closed, it is out of the way.
            h.app.removed = None;
            assert_eq!(h.frames(30), Duration::MAX, "and keeps still");
        }
    }

    /// A server of two files by the first segment of their path, as a download's probe asks for
    /// them: `same/…`, of 1000 bytes, and `other/…`, of 2000. Its address.
    fn sizes_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        // Two probes of a HEAD and a GET each, and room for retries.
        std::thread::spawn(move || {
            for mut socket in listener.incoming().take(16).flatten() {
                let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                let (mut head, mut buf) = (Vec::new(), [0u8; 1024]);
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head);
                let size = if head.contains(" /other/") { 2000 } else { 1000 };
                let answer = if head.starts_with("HEAD ") {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                } else {
                    format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/{size}\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
                };
                let _ = socket.write_all(answer.as_bytes());
            }
        });
        base
    }

    /// A download whose link expired says so on its card, with New link… in its ⋯ menu. There a
    /// link to a file of another size is refused, saying why, and nothing changes; one to the same
    /// file is taken, its resume state keeps what was downloaded, and it resumes.
    #[test]
    fn an_expired_link_takes_a_new_one_once_it_is_the_same_file() {
        use hyperfetch_core::state::DownloadState;
        let mut h = Harness::new();
        // Resumed, it fails at once instead of fetching anything.
        h.app.engines = std::sync::Arc::new(|_: Vec<url::Url>, _: DownloadOptions| Err("not downloaded here".to_string()));
        let server = sizes_server();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("talk.mp4");
        // What the engine left: 400 bytes of 1000, the link it had them from.
        let old = "https://cdn.example/talk.mp4?sig=old";
        let state_path = DownloadState::state_file_path(&dir.path().join("talk.mp4.part"));
        std::fs::write(dir.path().join("talk.mp4.part"), [0u8; 1000]).unwrap();
        let mut state = DownloadState::new("talk.mp4".into(), 1000, 256 << 10, vec![old.into()]);
        state.completed_ranges.push(hyperfetch_core::range::ByteRange::new(0, 399).unwrap());
        state.save_atomic(&state_path).unwrap();
        let saved = std::fs::read(&state_path).unwrap();
        let from_page = DownloadOptions { referer: Some("https://page.example/talk".into()), ..Default::default() };
        let id = h.app.queue.add_item(vec![url::Url::parse(old).unwrap()], from_page);
        h.app.queue.mark_started(id);
        let snapshot = hyperfetch_core::engine::EngineSnapshot {
            total_bytes: 1000,
            downloaded_bytes: 400,
            progress_ratio: 0.4,
            target_path: Some(target.clone()),
            ..Default::default()
        };
        h.app.queue.apply_snapshot(id, &snapshot);
        h.app.queue.finish(id, Err(format!("{old}: HTTP 403 Forbidden")));
        h.frames(5);
        assert!(h.find(LINK_EXPIRED).is_some() && h.find("Link expired").is_some(), "the card says so");

        from_menu(&mut h, "talk.mp4", "New link…");
        h.frames(20);
        assert!(h.find("Open the page").is_some(), "the page it came from is known");
        h.paste(&format!("{server}other/talk.mp4?sig=new"));
        h.click("Use this link");
        h.wait_for(|app| app.new_link.as_ref().is_some_and(|new_link| new_link.refused.is_some()));
        h.frames(3);
        let why = "This link is for a different file: 2.0 KiB, not 1000 bytes";
        assert!(h.texts().iter().any(|(text, _)| text == why), "{:?}", h.texts());
        let item = h.app.queue.get_item(id).unwrap();
        assert!(item.link_expired() && item.urls[0].as_str() == old, "nothing changes");
        assert_eq!(std::fs::read(&state_path).unwrap(), saved);

        let fresh = format!("{server}same/renamed.mp4?sig=new");
        h.app.new_link.as_mut().unwrap().text = fresh.clone();
        h.frames(2);
        h.click("Use this link");
        h.wait_for(|app| app.new_link.is_none());
        let item = h.app.queue.get_item(id).unwrap().clone();
        assert_eq!(item.urls, [url::Url::parse(&fresh).unwrap()]);
        assert_eq!((item.options.output_path.as_deref(), item.downloaded_bytes), (Some(target.as_path()), 400), "into its own file");
        let state = DownloadState::load_from_path(&state_path).unwrap().unwrap();
        assert!(state.mirrors.contains(&fresh) && state.completed_ranges.len() == 1, "{:?}", state.mirrors);
        assert!(matches!(&h.app.notice, Some(Ok(notice)) if notice.starts_with("New link taken")), "{:?}", h.app.notice);
        // It resumed: the stand-in engine fails it at once.
        h.wait_for(|app| app.queue.get_item(id).unwrap().status == QueueItemStatus::Failed("not downloaded here".to_string()));
        h.wait_for_background();
    }

    /// The queue's file names, top to bottom.
    fn order(h: &Harness) -> Vec<String> {
        h.app.queue.items().iter().map(|item| item.filename.clone()).collect()
    }

    /// Opens the ⋯ menu of `name`'s card and clicks `entry` in it.
    fn from_menu(h: &mut Harness, name: &str, entry: &str) {
        h.click_at(h.find_near(name, icon::DOTS_THREE_VERTICAL).unwrap().center());
        // A menu is laid out unseen the first time.
        h.frames(2);
        h.click(entry);
    }

    /// "Download next" in a waiting card's ⋯ menu puts it first among the waiting downloads (a
    /// paused one keeps its place); the cards glide to their new places, none jumping, then
    /// frames stop. Under Reduce motion they are in place at once.
    #[test]
    fn download_next_moves_a_row_up_and_the_rows_glide_there() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            let [paused, ..] = ["paused.iso", "b.iso", "c.iso", "d.iso"].map(|name| h.add(name));
            h.app.queue.mark_started(paused);
            h.app.queue.mark_pausing(paused);
            h.app.queue.finish(paused, Err(hyperfetch_core::engine::CANCELLED.to_string()));
            assert!(h.settle());
            let stride = ROW_HEIGHT + GAP;
            let place = h.find("b.iso").unwrap().top();
            from_menu(&mut h, "d.iso", "Download next");
            assert_eq!(order(&h), ["paused.iso", "d.iso", "b.iso", "c.iso"]);
            let tops = over(&mut h, 40, |h| (h.find("d.iso").unwrap().top(), h.find("c.iso").unwrap().top()));
            let (d, c): (Vec<f32>, Vec<f32>) = tops.into_iter().unzip();
            // c ends under the pointer, lifted a little.
            assert!(d[d.len() - 1] == place && (c[c.len() - 1] - place - 2.0 * stride).abs() <= 1.5, "{d:?} {c:?}");
            if reduce {
                assert!(d.iter().all(|&top| top == place) && c.iter().all(|&top| (top - place - 2.0 * stride).abs() <= 1.5), "at once: {d:?} {c:?}");
            } else {
                assert_eq!(d[0], place + 2.0 * stride, "it starts from where it was");
                assert!(d.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < stride / 2.0), "glides up: {d:?}");
                assert!(c[0] < c[c.len() - 1] && c.windows(2).all(|w| w[1] >= w[0]), "c glides down: {c:?}");
            }
            h.leave();
            assert_eq!(h.frames(30), Duration::MAX, "and keeps still");
            assert_eq!(h.find("c.iso").unwrap().top(), place + 2.0 * stride, "set down");
        }
    }

    /// A waiting card's ⋯ menu sets its priority: a High or Low one shows a mark by its name, a
    /// Normal one none, and the High one starts first.
    #[test]
    fn a_waiting_row_takes_a_priority_from_its_menu() {
        let mut h = Harness::new();
        let [_, b] = ["a.iso", "b.iso"].map(|name| h.add(name));
        assert!(h.settle());
        let marked = |h: &Harness, glyph: &str| {
            let name = h.find("b.iso").unwrap();
            h.find(glyph).is_some_and(|mark| (mark.center().y - name.center().y).abs() < 4.0 && mark.right() <= name.left())
        };
        assert!(!marked(&h, icon::CARET_DOUBLE_UP));
        from_menu(&mut h, "b.iso", "High");
        assert_eq!(h.app.queue.get_item(b).unwrap().priority, Priority::High);
        h.frames(2);
        assert!(marked(&h, icon::CARET_DOUBLE_UP), "a mark before its name");
        assert_eq!(h.app.queue.next_to_start(1, |_| false), Some(b), "it starts first");
        from_menu(&mut h, "b.iso", "Low");
        h.frames(2);
        assert!(marked(&h, icon::CARET_DOUBLE_DOWN) && !marked(&h, icon::CARET_DOUBLE_UP));
        from_menu(&mut h, "b.iso", "Normal");
        h.frames(2);
        assert!(h.find(icon::CARET_DOUBLE_UP).is_none() && h.find(icon::CARET_DOUBLE_DOWN).is_none(), "a Normal one shows none");
    }

    /// A card dragged over the list shows where it would land, as a line in the gap nearest the
    /// pointer; dropped there it moves, and once it has glided into place frames stop. A card
    /// dropped on its own place stays.
    #[test]
    fn a_dragged_row_lands_where_the_line_shows() {
        let mut h = Harness::new();
        let ids = ["a.iso", "b.iso", "c.iso", "d.iso"].map(|name| h.add(name));
        assert!(h.settle());
        let b = h.app.ctx.read_response(egui::Id::new(("queue row", ids[1])).with("row")).unwrap().rect;
        let landing = h.find("b.iso").unwrap().top();
        // A point of the line in the gap above b, as painted.
        let dot = |h: &Harness| {
            let [r, _, blue, _] = h.shot(Some(Rect::from_min_size(Pos2::new(b.left() + 4.0, b.top() - GAP / 2.0), Vec2::splat(1.0)))).get_pixel(0, 0).0;
            blue > 180 && r < 120
        };
        assert!(!dot(&h));
        let (from, to) = (h.find("d.iso").unwrap().center(), Pos2::new(b.center().x, b.top() + 10.0));
        h.press(from);
        for n in 1..=6 {
            h.hover(from.lerp(to, n as f32 / 6.0));
        }
        assert!(dot(&h), "the line shows the gap above b");
        assert_eq!(order(&h), ["a.iso", "b.iso", "c.iso", "d.iso"], "nothing moves before the drop");
        h.release(to);
        assert_eq!(order(&h), ["a.iso", "d.iso", "b.iso", "c.iso"]);
        h.leave();
        assert!(h.settle());
        assert_eq!(h.find("d.iso").unwrap().top(), landing);
        assert!(!dot(&h), "the line is gone");

        let a = h.find("a.iso").unwrap().center();
        h.press(a);
        h.hover(a + Vec2::new(0.0, 10.0));
        h.hover(a + Vec2::new(0.0, 20.0));
        h.release(a + Vec2::new(0.0, 20.0));
        assert_eq!(order(&h), ["a.iso", "d.iso", "b.iso", "c.iso"], "dropped on its own place");
    }

    /// A download that completes flashes its card green as its check draws itself; one that
    /// fails flashes red and shakes. Then frames stop. Under Reduce motion nothing flashes or
    /// moves.
    #[test]
    fn a_finished_row_glows_and_a_failed_one_shakes() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            let [done, failed] = ["done.iso", "failed.iso"].map(|name| h.add(name));
            h.progress(done, 0.9);
            h.progress(failed, 0.4);
            h.frames(60);
            // Inside the card, left of its icon, where only the card shows.
            let spot = |h: &Harness| {
                let row = h.app.ctx.read_response(egui::Id::new(("queue row", done)).with("row")).unwrap().rect;
                let pixel = h.shot(Some(Rect::from_min_size(row.left_center() + Vec2::new(5.0, 0.0), Vec2::splat(1.0)))).get_pixel(0, 0).0;
                [pixel[0], pixel[1], pixel[2]]
            };
            let card = [DARK.card.r(), DARK.card.g(), DARK.card.b()];
            assert_eq!(spot(&h), card);
            h.finish(done, None);
            h.finish(failed, Some("The server answered 404 Not Found"));
            let (mut xs, mut glow) = (Vec::new(), Vec::new());
            for n in 0..50 {
                h.frame(Vec::new());
                xs.push(h.find("failed.iso").unwrap().left());
                if n % 5 == 0 {
                    glow.push(spot(&h));
                }
            }
            let rest = xs[xs.len() - 1];
            assert_eq!(glow[glow.len() - 1], card, "the glow is gone");
            if reduce {
                assert!(glow.iter().all(|&g| g == card) && xs.iter().all(|&x| x == rest), "no glow or shake: {glow:?} {xs:?}");
            } else {
                let greens: Vec<u8> = glow.iter().map(|g| g[1]).collect();
                assert!(greens[0] > card[1] + 10 && greens.windows(2).all(|w| w[1] <= w[0]), "glows and fades: {greens:?}");
                assert!(xs.iter().any(|&x| (x - rest).abs() > 1.0), "shakes: {xs:?}");
            }
            assert_eq!(h.frames(2), Duration::MAX, "and keeps still");
        }
    }

    /// The chunk map paints each connection's piece in its own colour. When a connection takes
    /// over the end of another's piece, a light flashes on the cut and the new piece turns from
    /// the other's colour to its own; then frames stop. Under Reduce motion it has its own colour
    /// at once and nothing flashes.
    #[test]
    fn the_chunk_map_colours_each_connection_and_splits_a_stolen_piece() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            let id = h.add("ubuntu.iso");
            h.progress(id, 0.5);
            h.app.focused = Some(id);
            // Paused, so that nothing else in its details moves.
            h.app.jobs.get_mut(&id).unwrap().running = None;
            h.app.queue.get_item_mut(id).unwrap().status = QueueItemStatus::Paused;
            assert!(h.settle());
            let map = h.app.ctx.read_response(egui::Id::new(("chunk map", id))).unwrap().rect;
            // The colour shown `share` of the way along the map.
            let at = |h: &Harness, share: f32| {
                let spot = Rect::from_min_size(Pos2::new(map.left() + share * map.width(), map.center().y), Vec2::splat(1.0));
                let [r, g, b, _] = h.shot(Some(spot)).get_pixel(0, 0).0;
                Color32::from_rgb(r, g, b)
            };
            // Each quarter is half done: its first eighth filled.
            for n in 0..4 {
                assert_eq!(at(&h, n as f32 / 4.0 + 1.0 / 16.0), DARK.lanes[n], "connection {n}");
            }

            // Connection 4 takes the last quarter of the second piece and gets 80 % of it.
            let quarter = (100u64 << 20) / 4;
            let cut = quarter + quarter * 3 / 4;
            let chunks = &mut h.app.jobs.get_mut(&id).unwrap().chunks;
            (chunks[1].range_end, chunks[1].total_bytes) = (cut - 1, cut - quarter);
            let status = "Worker 4".to_string();
            chunks.push(ChunkSnapshot { id: 4, range_start: cut, range_end: 2 * quarter - 1, downloaded_bytes: quarter / 5, total_bytes: quarter / 4, status, worker_id: Some(4) });
            let (stolen, seam) = ((cut + quarter / 10) as f32 / (4 * quarter) as f32, cut as f32 / (4 * quarter) as f32);
            let delay = h.frame(Vec::new());
            // The light's edge is smoothed, so the pixel under it shows a little of the piece; no
            // connection's colour or the card is that light in every channel.
            let white = |color: Color32| color.r().min(color.g()).min(color.b()) > 150;
            if reduce {
                assert!(at(&h, stolen) == DARK.lanes[4] && !white(at(&h, seam)), "split at once: {:?} {:?}", at(&h, stolen), at(&h, seam));
                // The connections' table grows a scroll bar, for which egui takes a frame or two.
                assert!(delay == Duration::MAX || (0..3).any(|_| h.frame(Vec::new()) == Duration::MAX), "then still");
            } else {
                assert!(delay < Duration::MAX && at(&h, stolen) != DARK.lanes[4], "turning");
                assert!(white(at(&h, seam)), "the cut flashes: {:?}", at(&h, seam));
                assert!(h.settle());
                assert!(at(&h, stolen) == DARK.lanes[4] && !white(at(&h, seam)), "split");
            }
        }
    }

    /// The filter row narrows the list by text in a name or link and by status; a narrowed list
    /// that shows nothing says so.
    #[test]
    fn the_list_is_filtered_by_text_and_status() {
        let mut h = Harness::new();
        let (_, zip) = (h.add("a.iso"), h.add("b.zip"));
        h.frames(40);
        assert!(h.find("a.iso").is_some() && h.find("b.zip").is_some());
        assert!(h.find("Running 0").is_some() && h.find("Waiting 2").is_some(), "{:?}", h.texts());

        h.app.queue_search = " ZIP ".to_string();
        h.frames(10);
        assert!(h.find("a.iso").is_none() && h.find("b.zip").is_some());
        h.app.queue_search.clear();
        h.app.queue.mark_started(zip);
        h.frames(2);
        h.click("Running 1");
        assert_eq!(h.app.queue_filter, 1);
        h.frames(10);
        assert!(h.find("a.iso").is_none() && h.find("b.zip").is_some());
        h.app.queue_filter = 4;
        h.frames(40);
        assert!(h.find("b.zip").is_none() && h.find("No download matches").is_some(), "{:?}", h.texts());
    }

    /// The empty list teaches: three things to try, each filling the link bar, which then says
    /// what it is.
    #[test]
    fn the_empty_list_offers_things_to_try() {
        let mut h = Harness::new();
        assert!(h.settle());
        assert!(h.find("Nothing downloading yet").is_some() && h.find("Try one of these").is_some(), "{:?}", h.texts());
        h.click("A torrent");
        assert_eq!(h.app.url_input, "https://webtorrent.io/torrents/big-buck-bunny.torrent");
        assert!(h.settle());
        assert!(h.texts().iter().filter(|(text, _)| text == "A torrent").count() >= 1);
        assert!(h.find("from webtorrent.io").is_some(), "{:?}", h.texts());
    }
}
