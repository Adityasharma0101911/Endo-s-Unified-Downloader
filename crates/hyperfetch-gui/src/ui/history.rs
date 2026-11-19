use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::*;

/// Height of a download's row in the Library, and of a day's heading.
const ROW: f32 = 60.0;
const HEADING: f32 = 34.0;
/// The space between two rows.
const SPACE: f32 = 4.0;

/// One line of the Library: a heading over the downloads of a stretch of days (its name and how
/// many), or a download, by its place in the history.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Line {
    Heading(&'static str, usize),
    Entry(usize),
}

impl Line {
    fn height(self) -> f32 {
        match self {
            Line::Heading(..) => HEADING,
            Line::Entry(_) => ROW,
        }
    }
}

/// What the Library shows for one search of one history: its lines, newest first under their
/// headings, and where each one's top is. Worked out again only when the search, the history or
/// the day changes, so that a long history costs nothing a frame.
#[derive(Default)]
struct Listing {
    key: u64,
    lines: Vec<Line>,
    tops: Vec<f32>,
    height: f32,
}

/// The local day (days since 1970), and the seconds the local clock is ahead of UTC.
fn local_day() -> (i64, i64) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (_, minute) = crate::desktop::local_now();
    // The local minute of the day against the UTC one gives the offset, to the minute.
    let utc_minute = (now % 86_400) / 60;
    let mut offset = (i64::from(minute) - utc_minute).rem_euclid(1440);
    if offset > 720 {
        offset -= 1440;
    }
    ((now + offset * 60).div_euclid(86_400), offset * 60)
}

/// When `entry` finished (or started, when it never did), in Unix seconds.
fn when(entry: &HistoryEntry) -> u64 {
    entry.completed_at.unwrap_or(entry.started_at)
}

/// The heading of a download from `days` days ago.
fn stretch(days: i64) -> &'static str {
    match days {
        ..=0 => "Today",
        1 => "Yesterday",
        2..=6 => "Earlier this week",
        7..=30 => "This month",
        _ => "Older",
    }
}

/// The lines for `entries` matching `search` (a `util::history_search_key`), newest first, under
/// a heading for each stretch of days, `today` being the local day and `offset` the local clock's.
fn listing(entries: &[HistoryEntry], search: &str, today: i64, offset: i64) -> (Vec<Line>, Vec<f32>, f32) {
    let found = |e: &HistoryEntry| {
        search.is_empty()
            || e.file_name.to_lowercase().contains(search)
            || e.urls.iter().any(|u| u.to_lowercase().contains(search))
            || e.blake3_hash.as_ref().is_some_and(|h| h.to_lowercase().contains(search))
    };
    let mut shown: Vec<usize> = (0..entries.len()).filter(|&n| found(&entries[n])).collect();
    shown.sort_by_key(|&n| std::cmp::Reverse(when(&entries[n])));
    let day = |n: usize| (when(&entries[n]) as i64 + offset).div_euclid(86_400);
    let mut lines = Vec::with_capacity(shown.len() + 5);
    for (n, &index) in shown.iter().enumerate() {
        let name = stretch(today - day(index));
        if n == 0 || stretch(today - day(shown[n - 1])) != name {
            let count = shown[n..].iter().take_while(|&&other| stretch(today - day(other)) == name).count();
            lines.push(Line::Heading(name, count));
        }
        lines.push(Line::Entry(index));
    }
    let mut tops = Vec::with_capacity(lines.len());
    let mut y = 0.0;
    for line in &lines {
        tops.push(y);
        y += line.height() + SPACE;
    }
    (lines, tops, (y - SPACE).max(0.0))
}

/// The Library's listing for the search typed now, from the last frame's when nothing changed.
fn current(app: &App, ctx: &egui::Context) -> Arc<Listing> {
    let search = util::history_search_key(&app.history_search);
    let (today, offset) = local_day();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // shortcut: an entry changed in place, keeping the count and both ends, is not noticed until
    // the next change; a revision counter on the history would be exact.
    (&search, app.history.len(), app.history.first().map(|e| &e.id), app.history.last().map(|e| &e.id), today).hash(&mut hasher);
    let key = hasher.finish();
    let id = egui::Id::new("library listing");
    if let Some(listing) = ctx.data(|d| d.get_temp::<Arc<Listing>>(id)).filter(|listing| listing.key == key) {
        return listing;
    }
    let (lines, tops, height) = listing(&app.history, &search, today, offset);
    let listing = Arc::new(Listing { key, lines, tops, height });
    ctx.data_mut(|d| d.insert_temp(id, Arc::clone(&listing)));
    listing
}

/// `unix` seconds as the local date and time a row shows: the time alone today, the day and
/// month this year, else the full date.
fn shown_time(unix: u64, today: i64, offset: i64) -> String {
    if unix == 0 {
        return String::new();
    }
    let local = unix as i64 + offset;
    let (day, secs) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let time = format!("{:02}:{:02}", secs / 3600, secs / 60 % 60);
    if day == today {
        return time;
    }
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS[(m - 1) as usize];
    let this_year = shown_time_year(today);
    if y == this_year { format!("{d} {month}, {time}") } else { format!("{d} {month} {y}") }
}

/// The year of local day `day`.
fn shown_time_year(day: i64) -> i64 {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

/// The Library: every finished download with its links and checksums. A search over names, links
/// and hashes; checking a file; and the downloads newest first under a heading for each stretch
/// of days, the heading of the rows at the top staying pinned there. Only the rows in view are
/// laid out, so ten thousand cost what twenty do.
pub(super) fn library_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    page_title(Tab::Library, ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::MAGNIFYING_GLASS).size(16.0).color(p.muted));
        let search = egui::TextEdit::singleline(&mut app.history_search)
            .hint_text("Search by file name, link or checksum")
            .desired_width((ui.available_width() - 330.0).max(140.0));
        ui.add(search);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.menu_button(icon::DOTS_THREE, |ui| {
                let clear = egui::Button::new(RichText::new(format!("{} Clear history", icon::TRASH)).color(p.red));
                if ui.add(clear).on_hover_text("Forget every download listed here; the files are kept").clicked() {
                    app.update_history(|history| history.clear());
                    ui.close_menu();
                }
            });
            if ui.add(button(format!("{} Refresh", icon::ARROWS_CLOCKWISE))).clicked() {
                app.refresh_history();
            }
            let can_verify = !app.dialog_open && !app.verifying && app.repair.is_none();
            let verify = button(format!("{} Check a file…", icon::SHIELD_CHECK));
            if ui.add_enabled(can_verify, verify).on_hover_text("Check a file against what its download recorded, and repair it").clicked() {
                app.pending_dialog = Some(Dialog::VerifyFile);
            }
        });
    });
    verification_card(app, ui);
    ui.add_space(12.0);

    let listing = current(app, ui.ctx());
    if listing.lines.is_empty() {
        let (title, about) = match app.history_search.trim().is_empty() {
            true => ("Nothing downloaded yet", "Finished downloads are kept here with their links and checksums, to check, repair or get again later."),
            false => ("Nothing matches", "Clear the search above to see every download."),
        };
        anim::fade_in(ui, egui::Id::new(("library empty", title)), 0.0, anim::RISE, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label(RichText::new(icon::BOOKS).size(30.0).color(p.dim));
                ui.label(bold(title).size(16.0).color(p.text));
                ui.label(RichText::new(about).color(p.muted));
            });
        });
        return;
    }
    let (today, offset) = local_day();
    let opened = since_opened(ui.ctx());
    let mut action = None;
    let shown: &App = app;
    egui::ScrollArea::vertical().id_salt("library rows").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        ui.set_height(listing.height);
        let width = ui.available_width() - 10.0;
        let origin = ui.max_rect().min;
        let first = listing.tops.partition_point(|&top| top + ROW < viewport.min.y);
        let clip = ui.clip_rect();
        for n in first..listing.lines.len() {
            let top = listing.tops[n];
            if top > viewport.max.y {
                break;
            }
            let line = listing.lines[n];
            let rect = Rect::from_min_size(origin + Vec2::new(0.0, top), Vec2::new(width, line.height()));
            // The rows rise in one after another as the page opens.
            let t = anim::ease(anim::progress(ui.ctx(), opened, anim::stagger(n - first), anim::APPEAR));
            let rect = rect.translate(anim::RISE * (1.0 - t));
            // Near the list's top and bottom edges a row fades, so that the list melts into the page.
            let edge = ((rect.center().y - clip.top()).min(clip.bottom() - rect.center().y) / (line.height() * 0.5)).clamp(0.0, 1.0);
            let opacity = t * (0.35 + 0.65 * edge);
            match line {
                Line::Heading(name, count) => heading(ui, rect, name, count, opacity, false),
                Line::Entry(index) => {
                    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
                    row.multiply_opacity(opacity);
                    library_row(shown, &mut row, &shown.history[index], today, offset, &mut action);
                }
            }
        }
        // The heading of the rows at the top stays there, until the next one pushes it up.
        let pinned = listing.lines[..first.min(listing.lines.len() - 1) + 1].iter().rposition(|line| matches!(line, Line::Heading(..)));
        if let Some(at) = pinned.filter(|&at| listing.tops[at] < viewport.min.y) {
            let Line::Heading(name, count) = listing.lines[at] else { return };
            let next = listing.lines[at + 1..].iter().position(|line| matches!(line, Line::Heading(..))).map(|k| listing.tops[at + 1 + k]);
            let y = next.map_or(viewport.min.y, |next| viewport.min.y.min(next - HEADING - SPACE));
            let rect = Rect::from_min_size(origin + Vec2::new(0.0, y), Vec2::new(width, HEADING));
            heading(ui, rect, name, count, 1.0, true);
        }
    });
    if let Some(what) = action {
        act(app, what);
    }
}

/// A heading over the rows of a stretch of days, on a band of its own so that it reads over the
/// rows it is pinned above.
fn heading(ui: &mut Ui, rect: Rect, name: &str, count: usize, opacity: f32, pinned: bool) {
    let p = palette(ui);
    let painter = ui.painter();
    // Pinned, its band is solid and reaches the list's top edge: no row shows through or above it.
    let band = if pinned { Rect::from_min_max(rect.min, rect.max - Vec2::new(0.0, 3.0)) } else { rect.shrink2(Vec2::new(0.0, 3.0)) };
    painter.rect_filled(band, 10.0, if pinned { p.side } else { p.side.gamma_multiply(0.94 * opacity) });
    let text = painter.text(rect.left_center() + Vec2::new(14.0, 0.0), Align2::LEFT_CENTER, name, FontId::new(13.0, semibold()), p.text.gamma_multiply(opacity));
    let count = if count == 1 { "1 download".to_string() } else { format!("{count} downloads") };
    painter.text(text.right_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, count, FontId::proportional(12.0), p.dim.gamma_multiply(opacity));
}

/// What a row asked for, done once the list is drawn.
enum Act {
    Remove(String),
    Again(Vec<String>),
    Verify(std::path::PathBuf, u64),
    Reveal(std::path::PathBuf),
    Copy(String),
    Subtitles(std::path::PathBuf),
}

fn act(app: &mut App, what: Act) {
    match what {
        Act::Remove(id) => app.update_history(move |history| {
            history.remove_entry(&id);
        }),
        Act::Again(urls) => match util::redownload_input(&urls) {
            Ok(input) => {
                app.new_download();
                app.url_input = input;
                app.tab = Tab::Downloads;
                app.focused = None;
            }
            Err(e) => app.notice = Some(Err(e)),
        },
        Act::Verify(path, size) => app.verify(VerifyRequest { path, expected_size: (size > 0).then_some(size), checksum: None }),
        Act::Reveal(path) => report(app, util::reveal_in_folder(&path)),
        Act::Copy(link) => app.copy_text(link),
        Act::Subtitles(video) => app.find_subtitles(video),
    }
}

/// Seconds since the Library opened: since a frame that drew it after one that did not.
fn since_opened(ctx: &egui::Context) -> f32 {
    let id = egui::Id::new("history opened");
    let pass = ctx.cumulative_pass_nr();
    let visit = ctx.data_mut(|d| {
        let (last, visit) = d.get_temp_mut_or_default::<(u64, u64)>(id);
        if *last + 1 != pass {
            *visit += 1;
        }
        *last = pass;
        *visit
    });
    anim::since(ctx, id.with("visit"), visit, true)
}

/// One download in the Library: what became of it (by its icon and, unless it completed, in
/// words), its name, size, when, where from and its checksum, and its buttons; it lights up under
/// the pointer.
fn library_row(app: &App, ui: &mut Ui, entry: &HistoryEntry, today: i64, offset: i64, action: &mut Option<Act>) {
    let p = palette(ui);
    let rect = ui.max_rect();
    let id = egui::Id::new(("library row", &entry.id));
    let hovered = ui.rect_contains_pointer(rect);
    let hover = anim::presence(ui.ctx(), id.with("hover"), hovered, 0.15);
    let painter = ui.painter();
    painter.rect(rect, 12.0, p.card.lerp_to_gamma(p.hover, 0.3 * hover), Stroke::new(1.0, p.border.gamma_multiply(0.6 + 0.4 * hover)));
    let (glyph, color, word) = match &entry.status {
        HistoryStatus::Completed => (icon::CHECK, p.green, None),
        HistoryStatus::Failed(_) => (icon::WARNING_CIRCLE, p.red, Some("Failed")),
        HistoryStatus::Cancelled => (icon::X, p.muted, Some("Cancelled")),
    };
    ui.add_space(14.0);
    let (badge, _) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::hover());
    ui.painter().circle_filled(badge.center(), 16.0, color.gamma_multiply(0.14));
    anim::paint_icon(ui.painter(), badge.center(), glyph, 16.0, color, 0.0);
    ui.add_space(4.0);
    let idle = !app.verifying && app.repair.is_none();
    // A finished video can look for its subtitles (see `App::find_subtitles`).
    let video = matches!(entry.status, HistoryStatus::Completed) && hyperfetch_core::postprocess::is_video(&entry.file_path);
    let buttons = if video { 6.0 } else { 5.0 } * 32.0 + 12.0;
    left_part(ui, buttons, |ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(egui::Label::new(bold(&entry.file_name).color(p.strong)).truncate()).on_hover_text(&entry.file_name);
            let mut meta = Vec::new();
            if entry.file_size > 0 {
                meta.push(format_bytes(entry.file_size));
            }
            let time = shown_time(when(entry), today, offset);
            if !time.is_empty() {
                meta.push(time);
            }
            if let Some(host) = entry.urls.first().and_then(|u| url::Url::parse(u).ok()).and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string())) {
                meta.push(host);
            }
            if let Some(hash) = &entry.blake3_hash {
                meta.push(format!("BLAKE3 {}", truncate_chars(hash, 12)));
            }
            let path = entry.file_path.display().to_string();
            // Its state in its colour, the rest quiet.
            let font = egui::TextStyle::Small.resolve(ui.style());
            let mut job = egui::text::LayoutJob::default();
            if let Some(word) = word {
                job.append(word, 0.0, egui::TextFormat::simple(font.clone(), color));
                if !meta.is_empty() {
                    job.append("  ·  ", 0.0, egui::TextFormat::simple(font.clone(), p.muted));
                }
            }
            job.append(&meta.join("  ·  "), 0.0, egui::TextFormat::simple(font, p.muted));
            let line = egui::Label::new(job).truncate();
            ui.add(line).on_hover_text(match &entry.status {
                HistoryStatus::Failed(error) => format!("{error}\n{path}"),
                _ => path,
            });
        });
    });
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.add_space(8.0);
        if icon_button(ui, icon::TRASH, "Remove from the Library (the file is kept)", true).clicked() {
            *action = Some(Act::Remove(entry.id.clone()));
        }
        if icon_button(ui, icon::DOWNLOAD_SIMPLE, "Download again", true).clicked() {
            *action = Some(Act::Again(entry.urls.clone()));
        }
        if icon_button(ui, icon::SHIELD_CHECK, "Check and repair the file", idle).clicked() {
            *action = Some(Act::Verify(entry.file_path.clone(), entry.file_size));
        }
        if icon_button(ui, icon::FOLDER_OPEN, "Show in its folder", true).clicked() {
            *action = Some(Act::Reveal(entry.file_path.clone()));
        }
        if video && icon_button(ui, icon::SUBTITLES, "Find subtitles on OpenSubtitles", true).clicked() {
            *action = Some(Act::Subtitles(entry.file_path.clone()));
        }
        if let Some(url) = entry.urls.first() {
            if icon_button(ui, icon::COPY, "Copy the link", true).clicked() {
                *action = Some(Act::Copy(url.clone()));
            }
        }
    });
}

fn verification_card(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    if app.verifying {
        let name = app
            .verify_request
            .as_ref()
            .and_then(|r| r.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            anim::spinner(ui, 16.0, p.link);
            ui.label(RichText::new(format!("Checking {}... reading a large file can take a while", name)).color(p.muted));
        });
    }

    if let Some(verification) = &app.verification {
        let result = &verification.result;
        let verdict = util::verdict(result);
        let (glyph, badge, color) = match verdict {
            Verdict::Verified => (icon::CHECK_CIRCLE, "Verified", p.green),
            Verdict::Incomplete => (icon::PUZZLE_PIECE, "Incomplete", p.amber),
            Verdict::Mismatch => (icon::WARNING_CIRCLE, "Checksum mismatch", p.red),
            Verdict::Unverified => (icon::QUESTION, "Not verified", p.muted),
        };
        let name = result.file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let message = result.status_message.clone();
        let can_repair = !verification.repair_urls.is_empty();
        let repair_progress = app.repair.as_ref().map(|r| *lock(&r.progress));
        let (mut dismiss, mut repair, mut cancel) = (false, false, false);

        ui.add_space(10.0);
        // A new result drops in.
        let shown = anim::since(ui.ctx(), egui::Id::new("verification"), (&result.file_path, &message), true);
        let shown = anim::ease(anim::progress(ui.ctx(), shown, 0.0, anim::APPEAR));
        anim::shifted(ui, shown, anim::DROP, |ui| {
            Frame::none().fill(p.card).stroke(Stroke::new(1.0, color.gamma_multiply(0.7))).inner_margin(14.0).rounding(14.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(glyph).size(18.0).color(color));
                    ui.label(bold(badge).color(color));
                    ui.add(egui::Label::new(bold(&name).color(p.strong)).truncate()).on_hover_text(&name);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        dismiss = ui.add_enabled(repair_progress.is_none(), button("Dismiss")).clicked();
                        if verdict == Verdict::Incomplete && repair_progress.is_none() {
                            repair = ui
                                .add_enabled(can_repair, primary_button("Repair the missing pieces", p.accent))
                                .on_disabled_hover_text("No download links are recorded for exactly this file")
                                .clicked();
                        }
                    });
                });
                ui.add(egui::Label::new(RichText::new(message).color(p.text)).wrap());

                if let Some((done, total)) = repair_progress {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        cancel = ui.add(button(RichText::new("Cancel repair").color(p.red))).clicked();
                        let ratio = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
                        let text = format!("Repairing the missing pieces: {} / {}", format_bytes(done), format_bytes(total));
                        let size = Vec2::new(ui.available_width(), 20.0);
                        progress_bar(ui, egui::Id::new("repair"), size, ratio, p.accent, true, Some(text.into()));
                    });
                }
            })
        });

        if dismiss {
            app.verification = None;
            app.verify_message = None;
        }
        if repair {
            app.start_repair();
        }
        if cancel {
            if let Some(repair) = &app.repair {
                repair.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    if let Some(message) = &app.verify_message {
        ui.add_space(4.0);
        ui.label(RichText::new(message).small().color(p.muted));
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness::Harness;
    use super::*;

    fn entry(n: usize, at: u64) -> HistoryEntry {
        HistoryEntry {
            id: format!("e{n}"),
            file_name: format!("file-{n:05}.iso"),
            file_path: std::env::temp_dir().join(format!("file-{n:05}.iso")),
            file_size: 1 << 20,
            downloaded_bytes: 1 << 20,
            urls: vec![format!("https://example.com/file-{n:05}.iso")],
            status: HistoryStatus::Completed,
            blake3_hash: None,
            sha256_hash: None,
            started_at: at,
            completed_at: Some(at),
            etag: None,
        }
    }

    /// Downloads come newest first, under a heading for each stretch of days with how many it
    /// holds; a search keeps only what matches, headings too.
    #[test]
    fn the_library_groups_by_day_newest_first() {
        let today = 20_000;
        let at = |days_ago: i64| ((today - days_ago) * 86_400 + 3600) as u64;
        let entries = vec![entry(0, at(40)), entry(1, at(0)), entry(2, at(1)), entry(3, at(0)), entry(4, at(3))];
        let (lines, tops, height) = listing(&entries, "", today, 0);
        use Line::*;
        assert_eq!(
            lines,
            [Heading("Today", 2), Entry(1), Entry(3), Heading("Yesterday", 1), Entry(2), Heading("Earlier this week", 1), Entry(4), Heading("Older", 1), Entry(0)]
        );
        assert_eq!((tops[1], tops[2]), (HEADING + SPACE, HEADING + SPACE + ROW + SPACE));
        assert_eq!(height, 4.0 * HEADING + 5.0 * ROW + 8.0 * SPACE);
        let (lines, ..) = listing(&entries, "file-00002", today, 0);
        assert_eq!(lines, [Heading("Yesterday", 1), Entry(2)]);
        assert_eq!(shown_time(at(0), today, 0), "01:00");
        assert_eq!(shown_time(86_400 * 19_000, today, 0), "8 Jan 2022");
    }

    /// Ten thousand downloads lay out only the rows in view, and a frame of them asks for nothing
    /// more once the page has risen in; scrolled down, the heading of the rows at the top stays
    /// pinned there.
    #[test]
    fn ten_thousand_rows_scroll_and_keep_their_heading() {
        let mut h = Harness::new();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        h.app.history = (0..10_000).map(|n| entry(n, now - n as u64 * 60)).collect();
        h.app.tab = Tab::Library;
        assert!(h.settle());
        let shown = h.texts().into_iter().filter(|(text, _)| text.starts_with("file-")).count();
        assert!((5..40).contains(&shown), "only the rows in view: {shown}");
        h.hover(h.find("file-00000.iso").unwrap().center());
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, -3000.0), modifiers: egui::Modifiers::NONE };
        h.frame(vec![wheel]);
        h.frames(60);
        h.leave();
        assert!(h.settle());
        assert!(h.find("file-00000.iso").is_none(), "scrolled away");
        assert!(h.texts().iter().any(|(text, _)| text == "Today" || text == "Yesterday"), "the heading stays: {:?}", h.texts());
    }
}
