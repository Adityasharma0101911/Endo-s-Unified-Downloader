use super::*;

pub(super) fn history_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::MAGNIFYING_GLASS).size(16.0).color(p.muted));
            let search = egui::TextEdit::singleline(&mut app.history_search)
                .hint_text("Filter by file name, link or hash")
                .desired_width((ui.available_width() - 360.0).max(120.0));
            ui.add(search);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(button(RichText::new(format!("{} Clear history", icon::TRASH)).color(p.red))).clicked() {
                    app.update_history(|history| history.clear());
                }
                if ui.add(button(format!("{} Refresh", icon::ARROWS_CLOCKWISE))).clicked() {
                    app.refresh_history();
                }
                let can_verify = !app.dialog_open && !app.verifying && app.repair.is_none();
                let verify = button(format!("{} Verify a file…", icon::SHIELD_CHECK));
                if ui.add_enabled(can_verify, verify).on_hover_text("Check a file against what its download recorded").clicked() {
                    app.pending_dialog = Some(Dialog::VerifyFile);
                }
            });
        });
        verification_card(app, ui);
    });

    ui.add_space(12.0);
    let search = util::history_search_key(&app.history_search);
    let entries: Vec<HistoryEntry> = app
        .history
        .iter()
        .filter(|e| {
            search.is_empty()
                || e.file_name.to_lowercase().contains(&search)
                || e.urls.iter().any(|u| u.to_lowercase().contains(&search))
                || e.blake3_hash.as_ref().is_some_and(|h| h.to_lowercase().contains(&search))
        })
        .cloned()
        .collect();
    card(ui).inner_margin(4.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if entries.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new(icon::CLOCK_COUNTER_CLOCKWISE).size(28.0).color(p.dim));
                ui.label(RichText::new("No downloads recorded in history yet.").color(p.muted));
                ui.add_space(30.0);
            });
            return;
        }
        // The rows rise in one after another as the page opens.
        let opened = since_opened(ui.ctx());
        for (n, entry) in entries.iter().enumerate() {
            let t = anim::progress(ui.ctx(), opened, anim::stagger(n), anim::APPEAR);
            anim::shifted(ui, anim::ease(t), anim::RISE, |ui| {
                if n > 0 {
                    ui.separator();
                }
                history_row(app, ui, entry);
            });
        }
    });
}

/// Seconds since the History page opened: since a frame that drew it after one that did not.
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

fn history_row(app: &mut App, ui: &mut Ui, entry: &HistoryEntry) {
    let p = palette(ui);
    let idle = !app.verifying && app.repair.is_none();
    Frame::none().inner_margin(Margin::symmetric(12.0, 6.0)).show(ui, |ui| {
        ui.horizontal(|ui| {
            let (badge, color) = match entry.status {
                HistoryStatus::Completed => ("COMPLETED", p.green),
                HistoryStatus::Failed(_) => ("FAILED", p.red),
                HistoryStatus::Cancelled => ("CANCELLED", p.muted),
            };
            chip(ui, badge, color);
            left_part(ui, 5.0 * 30.0, |ui| {
                ui.add(egui::Label::new(bold(&entry.file_name).color(p.strong)).truncate()).on_hover_text(&entry.file_name);
                ui.label(RichText::new(format_bytes(entry.file_size)).small().color(p.muted));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, icon::TRASH, "Remove from the history", true).clicked() {
                    let id = entry.id.clone();
                    app.update_history(move |history| {
                        history.remove_entry(&id);
                    });
                }
                if icon_button(ui, icon::DOWNLOAD_SIMPLE, "Download again", true).clicked() {
                    match util::redownload_input(&entry.urls) {
                        Ok(input) => {
                            app.new_download();
                            app.url_input = input;
                            app.tab = Tab::Downloader;
                        }
                        Err(e) => app.notice = Some(Err(e)),
                    }
                }
                if icon_button(ui, icon::SHIELD_CHECK, "Verify & repair", idle).clicked() {
                    app.verify(VerifyRequest {
                        path: entry.file_path.clone(),
                        expected_size: (entry.file_size > 0).then_some(entry.file_size),
                        checksum: None,
                    });
                }
                if icon_button(ui, icon::FOLDER_OPEN, "Show in its folder", true).clicked() {
                    report(app, util::reveal_in_folder(&entry.file_path));
                }
                if let Some(url) = entry.urls.first() {
                    if icon_button(ui, icon::COPY, "Copy the link", true).clicked() {
                        app.copy_text(url.clone());
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            if let Some(hash) = &entry.blake3_hash {
                ui.label(RichText::new(format!("BLAKE3 {}", truncate_chars(hash, 19))).small().monospace().color(p.dim)).on_hover_text(hash);
            }
            let path = entry.file_path.display().to_string();
            ui.add(egui::Label::new(RichText::new(&path).small().monospace().color(p.dim)).truncate()).on_hover_text(path);
        });
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
            anim::spinner(ui, 16.0, p.accent);
            ui.label(RichText::new(format!("Verifying {}... hashing a large file can take a while", name)).color(p.muted));
        });
    }

    if let Some(verification) = &app.verification {
        let result = &verification.result;
        let verdict = util::verdict(result);
        let (badge, color) = match verdict {
            Verdict::Verified => ("VERIFIED", p.green),
            Verdict::Incomplete => ("INCOMPLETE", p.amber),
            Verdict::Mismatch => ("CHECKSUM MISMATCH", p.red),
            Verdict::Unverified => ("UNVERIFIED", p.muted),
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
        anim::shifted(ui, shown, anim::DROP, |ui| Frame::none()
            .fill(color.gamma_multiply(0.08))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
            .inner_margin(12.0)
            .rounding(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    chip(ui, badge, color);
                    ui.label(bold(name).color(p.strong));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        dismiss = ui.add_enabled(repair_progress.is_none(), button("Dismiss")).clicked();
                        if verdict == Verdict::Incomplete && repair_progress.is_none() {
                            repair = ui
                                .add_enabled(can_repair, primary_button("Repair missing chunks now", p.accent))
                                .on_disabled_hover_text("No download URLs are recorded for exactly this file")
                                .clicked();
                        }
                    });
                });
                ui.label(RichText::new(message).color(p.text));

                if let Some((done, total)) = repair_progress {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        cancel = ui.add(button(RichText::new("Cancel repair").color(p.red))).clicked();
                        let ratio = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
                        let text = format!("Repairing missing chunks: {} / {}", format_bytes(done), format_bytes(total));
                        let size = Vec2::new(ui.available_width(), 20.0);
                        progress_bar(ui, egui::Id::new("repair"), size, ratio, p.accent, true, Some(text.into()));
                    });
                }
            }));

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
