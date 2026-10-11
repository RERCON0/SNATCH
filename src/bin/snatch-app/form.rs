use super::*;

impl SnatchApp {
    pub(super) fn ui_cookies_row(&mut self, ui: &mut egui::Ui) {
        let engine = self.effective_engine();
        ui.add_space(6.0);
        let yt = engine == Engine::YtDlp;
        ui.add_enabled_ui(self.phase != Phase::Setup && yt, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut self.use_cookies,
                    tr!("browser cookies", "куки из браузера"),
                );
                ui.add_enabled_ui(self.use_cookies, |ui| {
                    egui::ComboBox::from_id_salt("browser")
                        .selected_text(self.cookies_browser.clone())
                        .show_ui(ui, |ui| {
                            for b in COOKIES_BROWSERS {
                                ui.selectable_value(&mut self.cookies_browser, b.to_string(), *b);
                            }
                        });
                });
            });
        });
    }

    /// yt-dlp extras: subtitles, playlists and free-form engine arguments.
    pub(super) fn ui_extras_row(&mut self, ui: &mut egui::Ui) {
        let enabled = self.phase != Phase::Setup;
        let engine = self.effective_engine();
        ui.add_space(6.0);
        tag_label(ui, tr!("options", "опции"));
        ui.horizontal(|ui| {
            if engine == Engine::YtDlp && self.link_ready() && links::offers_subtitles(&self.url) {
                ui.add_enabled_ui(enabled, |ui| {
                    ui.checkbox(&mut self.subs, tr!("subtitles", "субтитры"))
                        .on_hover_text(tr!(
                            "Download manual and automatic subtitles alongside the video",
                            "Скачивать субтитры (ручные и автоматические) рядом с видео"
                        ));
                    // The subtitles-only format downloads subtitles on its own;
                    // its language field must stay visible without the checkbox.
                    if self.subs || self.fmt == Format::Subs {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.sub_langs)
                                .font(field_font())
                                .desired_width(70.0),
                        )
                        .on_hover_text(tr!(
                            "Subtitle languages, e.g. ru,en",
                            "Языки субтитров, например ru,en"
                        ));
                    }
                });
            }
            ui.add_enabled_ui(enabled, |ui| {
                ui.checkbox(&mut self.playlist, tr!("playlist", "плейлист"))
                    .on_hover_text(tr!(
                        "Download the entire playlist instead of a single video",
                        "Скачать плейлист целиком, а не одиночное видео"
                    ));
            });
        });
        ui.add_enabled_ui(enabled, |ui| {
            ui.collapsing(tr!("extra arguments", "доп. аргументы"), |ui| {
                ui.horizontal(|ui| {
                    ui.label("yt-dlp");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.extra_ytdlp)
                            .font(field_font())
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("aria2c");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.extra_aria2)
                            .font(field_font())
                            .desired_width(f32::INFINITY),
                    );
                });
            });
        });
    }

    pub(super) fn ui_run_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // One block per concurrent job - up to MAX_CONCURRENT of these, each
        // with its own name/progress/cancel, so cancelling the second
        // download doesn't require cancelling the first one first.
        let mut pause_id = None;
        let mut cancel_id = None;
        // Both sets reserve 172px for controls so active and paused bars align.
        // The resume label needs more room than pause/cancel.
        const ACTIVE_BTN_W: f32 = 86.0;
        const RESUME_BTN_W: f32 = 100.0;
        const REMOVE_BTN_W: f32 = 72.0;
        for (i, aj) in self.jobs.iter().enumerate() {
            if i > 0 {
                ui.add_space(10.0);
            }
            let accent = ui.visuals().hyperlink_color;
            ui.label(
                egui::RichText::new(clip(&aj.label, 72))
                    .size(12.0)
                    .color(accent),
            );
            let waiting = if aj.metadata_wait {
                tr!("Torrent metadata…", "Метаданные торрента…")
            } else {
                tr!("Connecting…", "Подключение…")
            };
            let (pause, cancel) = job_controls(
                ui,
                aj.progress,
                accent,
                false,
                waiting,
                (tr!("pause", "пауза"), ACTIVE_BTN_W),
                (tr!("cancel", "отмена"), ACTIVE_BTN_W),
            );
            if pause {
                pause_id = Some(aj.id);
            }
            if cancel {
                cancel_id = Some(aj.id);
            }
            if !aj.status.is_empty() {
                ui.weak(clip(&aj.status, 90));
            }
            for w in &aj.warns {
                ui.colored_label(ui.visuals().warn_fg_color, format!("! {w}"));
            }
        }
        if let Some(id) = pause_id {
            self.pause_job(id);
        }
        if let Some(id) = cancel_id {
            self.cancel_job(id);
        }
        if !self.jobs.is_empty() {
            ui.add_space(8.0);
        }

        let mut resume_idx = None;
        let mut drop_idx = None;
        for (i, pj) in self.paused.iter().enumerate() {
            if i > 0 {
                ui.add_space(10.0);
            }
            let accent = ui.visuals().hyperlink_color;
            let warn = ui.visuals().warn_fg_color;
            ui.label(
                egui::RichText::new(clip(&pj.label, 72))
                    .size(12.0)
                    .color(accent),
            );
            let (resume, remove) = job_controls(
                ui,
                pj.progress,
                warn,
                true,
                "",
                (tr!("resume", "продолжить"), RESUME_BTN_W),
                (tr!("remove", "убрать"), REMOVE_BTN_W),
            );
            if resume {
                resume_idx = Some(i);
            }
            if remove {
                drop_idx = Some(i);
            }
            if !pj.status.is_empty() {
                ui.weak(clip(&pj.status, 90));
            }
        }
        // The paused row that actually left the list this frame (a refused
        // resume keeps its row), for the index shift of "убрать" below.
        let mut resumed_row = None;
        if let Some(i) = resume_idx {
            if self.phase == Phase::Setup {
                // Resuming during setup would start a loader while the
                // installer is replacing the same binaries (start_setup
                // clears retry_offer for this exact hazard).
                self.set_status(
                    StatusKind::Err,
                    tr!(
                        "Wait for tool installation to finish",
                        "Дождитесь установки загрузчиков"
                    ),
                );
            } else {
                let pj = self.paused.remove(i);
                resumed_row = Some(i);
                if self.jobs.len() < MAX_CONCURRENT {
                    // A refused start (lock held, folder gone, a re-pick
                    // needed once its .torrent was cleaned up…) must neither
                    // lose the job nor hide spawn_job's reason behind
                    // "продолжаю…": it stays paused, with that error shown.
                    let started =
                        self.spawn_job(ctx, pj.job.clone(), pj.no_continue, pj.select.clone());
                    if started {
                        self.set_status(
                            StatusKind::None,
                            tr_format!("“{}”: resuming…", "«{}»: продолжаю…", pj.label),
                        );
                    } else {
                        self.paused.insert(i, pj);
                        resumed_row = None;
                    }
                } else if self.close_requested {
                    self.paused.insert(i, pj);
                    resumed_row = None;
                } else {
                    self.queue.push_back(QueuedJob {
                        job: pj.job,
                        select: pj.select,
                        no_continue: pj.no_continue,
                    });
                    self.set_status(
                        StatusKind::None,
                        tr_format!(
                            "Added to queue ({})",
                            "Добавлено в очередь ({})",
                            self.queue.len()
                        ),
                    );
                }
            }
        }
        if let Some(i) = drop_idx {
            // If resume and remove were clicked in the same frame, the first
            // removal shifted indices. The clicked row is now elsewhere.
            let idx = if resumed_row.is_some_and(|r| r < i) {
                i - 1
            } else {
                i
            };
            if resume_idx != Some(i) && idx < self.paused.len() {
                let pj = self.paused.remove(idx);
                if self.status == tr_format!("“{}” paused", "«{}» на паузе", pj.label)
                {
                    self.set_status(StatusKind::None, "");
                }
            }
        }
        if !self.paused.is_empty() {
            ui.add_space(8.0);
        }

        if self.phase == Phase::Setup {
            ui.horizontal(|ui| {
                slow_spinner(ui);
                ui.label(setup_progress_text(self.setup_progress.as_ref()));
            });
            return;
        }

        // While a torrent's file list loads, the CTA spot shows that (with a
        // cancel) - the job controls above stay usable meanwhile.
        if self.torrent_pick.as_ref().is_some_and(TorrentPick::loading) {
            let mut cancel = false;
            ui.horizontal(|ui| {
                slow_spinner(ui);
                ui.weak(tr!(
                    "fetching torrent file list…",
                    "получаю список файлов торрента…"
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    cancel = ui.button(tr!("[ cancel ]", "[ отмена ]")).clicked();
                });
            });
            if cancel {
                self.abort_torrent_pick();
            }
            self.ui_queue_count(ui);
            return;
        }

        // Same button regardless of what's already running: start_download()
        // starts immediately while there's a free concurrent slot, or queues
        // behind the active jobs once MAX_CONCURRENT is reached (see its doc
        // comment) - the label just says which one is about to happen.
        // `.cta { width: 100% }` in the mockup. add_sized() forces
        // Layout::centered_and_justified (confirmed in egui's source), which
        // is what actually centers the text - plain min_size() just reserves
        // space in the *ambient* layout (Align::Min/left here), so the text
        // stayed pinned left in a much wider button. add_sized() with
        // f32::INFINITY as the width produced a button with a border but
        // literally no text at all; a concrete width doesn't have that
        // problem and still gets the forced centered layout.
        let can = !self.url.trim().is_empty()
            && !self.out_dir.trim().is_empty()
            && self.link_ready()
            && match self.effective_engine() {
                // Enabling the CTA while only the *other* engine is installed
                // would build a command that fails and silently drops the link.
                Engine::YtDlp => self.tc.yt_dlp.is_some(),
                Engine::Aria2 => self.tc.aria2c.is_some(),
            };
        let width = ui.available_width();
        let accent = ui.visuals().hyperlink_color;
        // The mockup's source text is lowercase, but its CSS has
        // `.cta { text-transform: uppercase }` - the *rendered* (approved)
        // look is "▶ СКАЧАТЬ", not "▶ скачать". Mockup's `.cta` is
        // `font: 600 13px` - 16.0 here was never actually checked against it.
        let label = if self.jobs.len() < MAX_CONCURRENT {
            tr!("▶ DOWNLOAD", "▶ СКАЧАТЬ")
        } else {
            tr!("+ ADD TO QUEUE", "+ В ОЧЕРЕДЬ")
        };
        let cta = egui::Button::new(egui::RichText::new(label).color(accent).size(13.0))
            .stroke(egui::Stroke::new(1.0_f32, accent))
            .fill(egui::Color32::TRANSPARENT);
        let clicked = ui
            .add_enabled_ui(can, |ui| ui.add_sized([width, 36.0], cta).clicked())
            .inner;
        if clicked {
            self.start_download(ctx);
        }
        self.ui_queue_count(ui);
    }

    pub(super) fn ui_queue_count(&self, ui: &mut egui::Ui) {
        if !self.queue.is_empty() {
            ui.add_space(4.0);
            ui.weak(tr_format!("Queued: {}", "В очереди: {}", self.queue.len()));
        }
    }
}
