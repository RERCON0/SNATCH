use super::*;

impl eframe::App for SnatchApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.window_state.save(storage);
    }

    fn persist_egui_memory(&self) -> bool {
        // Store native geometry only; input text and transient UI stay private.
        false
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Downloads, setup and cancellation continue while the window is hidden.
        self.drain(ctx);
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested {
            // An open pick dies with the window; its aria2c is stopped too.
            self.cancel_torrent_pick();
        }
        if close_requested && !self.jobs.is_empty() {
            // eframe exits the event loop in THIS frame unless the close is
            // vetoed - the old flag-only version let the process die before
            // the monitor thread (100ms poll) reached kill(), orphaning the
            // loader mid-download. Veto, cancel properly (job object kills
            // the whole tree), and drain() re-issues Close once everything's
            // actually done.
            self.close_requested = true;
            self.cancel_all();
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // eframe re-applies the OS scale factor on DPI/monitor changes, so
        // the pin is re-asserted every frame, not just at startup.
        pin_pixel_scale(ctx, false);
        // Center the final native size after the pinned UI scale reaches Windows.
        if self.startup_frames < 3 && ctx.current_pass_index() == 0 {
            self.startup_frames += 1;
            if self.startup_frames == 2 {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                    self.window_state.size.into(),
                ));
            }
            if self.startup_frames == 3 {
                #[cfg(windows)]
                placement::center(_frame);
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(
                    self.window_state.maximized,
                ));
                #[cfg(not(windows))]
                if let Some(center) = egui::ViewportCommand::center_on_screen(ctx) {
                    ctx.send_viewport_cmd(center);
                }
            }
            ctx.request_repaint();
        } else if self.startup_frames == 3 && ctx.current_pass_index() == 0 {
            self.window_state.observe(ctx);
        }
        let log_was_open = self.show_log_window;
        // A drop while the pick modal is open would swap the URL field from
        // under the torrent being picked.
        if self.phase != Phase::Setup && self.torrent_pick.is_none() {
            let dropped = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .map(|f| f.path().to_path_buf())
                    .next()
            });
            if let Some(path) = dropped {
                self.set_torrent_file(&path);
            }
        }

        #[cfg(windows)]
        self.ui_title_bar(ui);

        // The mockup's footer is its own full-width band, with a hairline at
        // the top and 26px side padding. Keeping it outside the scrollable
        // form also prevents long status/warning text from overlapping it.
        egui::Panel::bottom("app_footer")
            .frame(
                egui::Frame::NONE
                    .fill(ctx.global_style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(26, 10)),
            )
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.weak(egui::RichText::new(format!("snatch {APP_VERSION}")).size(11.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.hyperlink_to(
                            egui::RichText::new(TELEGRAM_URL.trim_start_matches("https://"))
                                .size(11.0),
                            TELEGRAM_URL,
                        );
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(ctx.global_style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(26, 18)),
            )
            .show(ui, |ui| {
            // Choosing files (or reading a listing error) is modal: the form
            // under the dim overlay takes no clicks/keys. Its window has a
            // cancel. While the list only loads, see the form rows below.
            let picking = self.torrent_pick.as_ref().is_some_and(|p| !p.loading());
            if picking {
                ui.disable();
            }
            // The HTML window is 660px wide, with 26px content padding. A
            // resized native window may be wider; keep the form centred at
            // the same 608px maximum instead of stretching fields edge to edge.
            let width = ui.available_width().min(608.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_terminal(ui, |ui| {
            #[cfg(not(windows))]
            self.ui_language_button(ui, ctx, [40.0, 24.0]);
            #[cfg(not(windows))]
            if ui.button(if self.dark_mode { tr!("Light theme", "Светлая тема") } else { tr!("Dark theme", "Тёмная тема") }).clicked() {
                self.toggle_theme(ctx);
            }
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.add_space(2.0);
                // Убираем только переносы: trim() съедает ведущий пробел
                // первой строки ASCII-арта, и она встаёт на символ левее остальных.
                let lines: Vec<&str> = BANNER.trim_matches(['\r', '\n']).lines().collect();
                let line_width = |ui: &egui::Ui, text: &str, size: f32| -> f32 {
                    ui.fonts_mut(|f| {
                        let job = egui::text::LayoutJob::single_section(
                            text.to_string(),
                            egui::TextFormat {
                                font_id: egui::FontId::monospace(size),
                                ..Default::default()
                            },
                        );
                        f.layout_job(job).size().x
                    })
                };
                let available = ui.available_width();
                // Fitting the art to the width costs up to a dozen text layouts
                // per line, and the answer only changes with the width.
                let (size, block_w, row_h) = match self.banner_fit {
                    Some((width, size, block_w, row_h)) if width == available.to_bits() => {
                        (size, block_w, row_h)
                    }
                    _ => {
                        let width_at = |size: f32| -> f32 {
                            lines
                                .iter()
                                .map(|line| line_width(ui, line, size))
                                .fold(0.0_f32, f32::max)
                        };
                        let mut size = 11.0_f32;
                        while size > 5.0 && width_at(size) > available {
                            size -= 0.5;
                        }
                        let block_w = width_at(size) + 2.0;
                        let row_h = ui.fonts_mut(|f| {
                            let job = egui::text::LayoutJob::single_section(
                                "█".to_string(),
                                egui::TextFormat {
                                    font_id: egui::FontId::monospace(size),
                                    ..Default::default()
                                },
                            );
                            f.layout_job(job).size().y
                        });
                        self.banner_fit = Some((available.to_bits(), size, block_w, row_h));
                        (size, block_w, row_h)
                    }
                };
                let block_h = row_h * lines.len() as f32;
                // top_down(Min): the outer panel already centers this whole block: if
                // it instead inherited the ambient Center alignment here, each row
                // (they're not equal-width - it's figlet art, not a padded rectangle)
                // would self-center against block_w and drift sideways from its
                // neighbours. Min pins every row's leading space to the same x=0.
                ui.allocate_ui_with_layout(
                    egui::vec2(block_w, block_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                        for line in &lines {
                            ui.label(
                                egui::RichText::new(*line).monospace().size(size).weak(),
                            );
                        }
                    },
                );
                ui.add_space(2.0);
            });

            ui.add_space(4.0);
            let mut missing: Vec<&str> = Vec::new();
            if self.tc.yt_dlp.is_none() {
                missing.push("yt-dlp");
            }
            if self.tc.aria2c.is_none() {
                missing.push("aria2c");
            }
            if !missing.is_empty() {
                // Actionable warning - this one earns the attention.
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    if cfg!(windows) {
                        tr_format!("Downloaders not found: {} — I can install them.", "Не найдены загрузчики: {} — могу скачать их сам.", missing.join(", "))
                    } else {
                        tr_format!("Downloaders not found: {} — install them manually.", "Не найдены загрузчики: {} — установите их вручную.", missing.join(", "))
                    },
                );
                if cfg!(windows) && ui
                    .add_enabled(
                        self.phase != Phase::Setup && self.jobs.is_empty() && self.torrent_pick.is_none(),
                        egui::Button::new(tr!("install tools", "установить загрузчики")),
                    )
                    .clicked()
                {
                    self.start_setup(ctx);
                }
            } else {
                // Routine status, nothing to act on most of the time - a
                // bordered/shadowed card here competed with the actual form
                // for attention, so it's just a quiet line now.
                let line = self.found_tools_line();
                ui.horizontal(|ui| {
                    ui.weak(line);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            // mockup's .btn is 12.5px with 7px 12px padding -
                            // .small() dropped it to the Small style and made
                            // the button visibly smaller than the HTML one.
                            .add_enabled(self.phase != Phase::Setup && self.jobs.is_empty() && self.torrent_pick.is_none(),
                                egui::Button::new(tr!("update", "обновить")).wrap_mode(egui::TextWrapMode::Extend))
                            .on_hover_text(tr!("Update yt-dlp, aria2c, ffmpeg and Deno (tools that are already current are not downloaded again)", "Обновить yt-dlp, aria2c, ffmpeg и Deno (актуальные заново не скачиваются)"))
                            .clicked()
                        {
                            self.start_setup(ctx);
                        }
                    });
                });
            }
            dashed_separator(ui);

            ui.add_space(14.0);
            // During a pick the link/engine/folder fields belong to it; the
            // job rows in ui_run_row stay live (pause/cancel while a magnet
            // fetches its metadata).
            let form_free = self.torrent_pick.is_none();
            let dir_history_opened = ui
                .add_enabled_ui(form_free, |ui| {
                    self.ui_url_row(ui, ctx);
                    self.refresh_link(ctx);
                    let opened = self.ui_engine_row(ui);
                    self.ui_cookies_row(ui);
                    self.ui_extras_row(ui);
                    opened
                })
                .inner;
            ui.add_space(10.0);
            self.ui_run_row(ui, ctx);
            self.ui_dir_history(ctx, dir_history_opened);
            if picking {
                // Middle order: above the panels, below the Foreground pick
                // window. Paint only - the form itself is disabled above.
                let screen = ctx.content_rect();
                ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Middle,
                    egui::Id::new("torrent-pick-dim"),
                ))
                .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(120));
            }
            self.ui_torrent_pick(ctx);
            self.ui_dir_browser(ctx);

            // retry_offer only ever comes from a job's own just-finished
            // failure, so it doesn't need a phase/slot gate to *show* - only
            // the click below needs to decide start-now vs queue.
            if self.retry_offer.is_some() {
                ui.add_space(6.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    tr!("This site may require sign-in or rate-limit the requests (bot detection, age restrictions, HTTP 429).", "Похоже, сайту нужна авторизация или он ограничил запросы (бот-детект, возрастные ограничения, 429)."),
                );
                // Not while a torrent is being picked: that modal owns the window.
                if ui
                    .add_enabled(
                        form_free,
                        egui::Button::new(tr!("Retry with browser cookies", "Повторить с куками из браузера")),
                    )
                    .clicked()
                {
                    // One-shot: retry with cookies without flipping the
                    // persistent checkbox. A single (possibly provoked) 403
                    // shouldn't silently opt every future download into
                    // reading the browser's cookie store.
                    self.retry_with_cookies(ctx);
                }
            }

            ui.add_space(6.0);
            if !self.status.is_empty() {
                let color = match self.status_kind {
                    StatusKind::Ok => ui.visuals().hyperlink_color,
                    StatusKind::Err => ui.visuals().error_fg_color,
                    StatusKind::Warn => ui.visuals().warn_fg_color,
                    StatusKind::None => ui.style().visuals.text_color(),
                };
                ui.colored_label(color, &self.status);
            }

            // The journal link belongs to the form content, above the footer.
            ui.add_space(14.0);
            if ui
                .add(
                    egui::Button::new(egui::RichText::new(tr!("▸ log", "▸ журнал")).color(ui.visuals().weak_text_color()))
                        .frame(false),
                )
                .clicked()
            {
                self.show_log_window = true;
            }
            });
                    },
                );
            });
        });
        self.ui_log_window(ctx, !log_was_open && self.show_log_window);
        #[cfg(windows)]
        self.ui_resize_edges(ctx);
    }
}
