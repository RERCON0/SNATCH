use super::*;

impl SnatchApp {
    pub(super) fn ui_language_button(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        size: [f32; 2],
    ) {
        if ui
            .add_sized(size, egui::Button::new(tr!("RU", "EN")).frame(false))
            .on_hover_text(tr!("Switch to Russian", "Переключить на английский"))
            .clicked()
        {
            let language = match self.cfg.language {
                Language::En => Language::Ru,
                Language::Ru => Language::En,
            };
            self.cfg.set_language(language);
            i18n::set_language(language);
            if !self.save_config() {
                self.set_status(
                    StatusKind::Warn,
                    tr!(
                        "Language was not saved — see log",
                        "Язык не сохранён — см. журнал"
                    ),
                );
            }
            ctx.request_repaint();
        }
    }

    #[cfg(windows)]
    pub(super) fn ui_title_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Borderless Windows viewport: a roomier version of the HTML titlebar, flat,
        // with a thin line below. The draggable area excludes the window
        // buttons so a click there never accidentally moves the window.
        egui::Panel::top("app_titlebar")
            .exact_size(42.0)
            .show_separator_line(true)
            .frame(
                egui::Frame::NONE
                    .fill(ctx.global_style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(14, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let title_width =
                        (ui.available_width() - 3.0 * 40.0 - THEME_BTN_WIDTH - 40.0).max(0.0);
                    let (rect, drag) = ui.allocate_exact_size(
                        egui::vec2(title_width, 42.0),
                        egui::Sense::click_and_drag(),
                    );
                    ui.painter().text(
                        egui::pos2(rect.left(), rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        "SNATCH — by rercon prod.",
                        egui::FontId::new(13.0, egui::FontFamily::Name("title".into())),
                        ui.visuals().weak_text_color(),
                    );
                    if drag.double_clicked() {
                        self.maximized =
                            !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
                    } else if drag.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    // Button-styled text uses the "mono-button" font family
                    // (set up above for its y-offset tweak), which is just
                    // Cascadia Mono with no fallback font behind it at all -
                    // unlike ─/□/×/▶/▸/●/○ elsewhere, ☀/🌙 aren't glyphs
                    // Cascadia actually has, so they rendered as tofu boxes.
                    // Plain Cyrillic text needs no fallback: this font backs
                    // every other Russian label in the app already. Label
                    // names the mode a click switches *to* (dark is too dark
                    // to read outdoors in daylight, this is the way out).
                    self.ui_language_button(ui, ctx, [40.0, 38.0]);
                    let label = if self.dark_mode {
                        tr!("light", "день")
                    } else {
                        tr!("dark", "ночь")
                    };
                    if ui
                        .add_sized(
                            [THEME_BTN_WIDTH, 38.0],
                            egui::Button::new(label).frame(false),
                        )
                        .on_hover_text(if self.dark_mode {
                            tr!("Light theme", "Светлая тема")
                        } else {
                            tr!("Dark theme", "Тёмная тема")
                        })
                        .clicked()
                    {
                        self.toggle_theme(ctx);
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("─").frame(false))
                        .on_hover_text(tr!("Minimize", "Свернуть"))
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("□").frame(false))
                        .on_hover_text(tr!("Maximize / restore", "Развернуть / восстановить"))
                        .clicked()
                    {
                        self.maximized =
                            !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("×").frame(false))
                        .on_hover_text(tr!("Close", "Закрыть"))
                        .clicked()
                    {
                        self.cancel_torrent_pick();
                        if !self.jobs.is_empty() {
                            self.close_requested = true;
                            self.cancel_all();
                        } else {
                            // Setup is an in-process worker. Closing the app
                            // stops it immediately; the next install resumes
                            // any incomplete .part files (Range) instead of
                            // restarting them. No child process needs to be
                            // reaped here.
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                });
            });
    }

    #[cfg(windows)]
    pub(super) fn ui_resize_edges(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized)) {
            return;
        }
        let screen = ctx.content_rect();
        let e = 6.0;
        let w = screen.width();
        let h = screen.height();
        let handles = [
            (
                egui::pos2(0.0, 0.0),
                egui::vec2(e, e),
                egui::ResizeDirection::NorthWest,
            ),
            (
                egui::pos2(e, 0.0),
                egui::vec2(w - 2.0 * e, e),
                egui::ResizeDirection::North,
            ),
            (
                egui::pos2(w - e, 0.0),
                egui::vec2(e, e),
                egui::ResizeDirection::NorthEast,
            ),
            (
                egui::pos2(0.0, e),
                egui::vec2(e, h - 2.0 * e),
                egui::ResizeDirection::West,
            ),
            (
                egui::pos2(w - e, e),
                egui::vec2(e, h - 2.0 * e),
                egui::ResizeDirection::East,
            ),
            (
                egui::pos2(0.0, h - e),
                egui::vec2(e, e),
                egui::ResizeDirection::SouthWest,
            ),
            (
                egui::pos2(e, h - e),
                egui::vec2(w - 2.0 * e, e),
                egui::ResizeDirection::South,
            ),
            (
                egui::pos2(w - e, h - e),
                egui::vec2(e, e),
                egui::ResizeDirection::SouthEast,
            ),
        ];
        for (i, (pos, size, direction)) in handles.into_iter().enumerate() {
            egui::Area::new(egui::Id::new(("window_resize", i)))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ctx, |ui| {
                    let (_, response) = ui.allocate_exact_size(size, egui::Sense::drag());
                    if response.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                    }
                });
        }
    }

    pub(super) fn ui_url_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        tag_label(ui, tr!("link", "ссылка"));
        let mut toggle_hist = false;
        #[cfg(windows)]
        let mut pick_torrent = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Never gate this on history being non-empty: with the ghost
                // theme a disabled button is indistinguishable from an
                // enabled one, so an empty history read as "the button is
                // broken". The window opens anyway and explains itself.
                if ui
                    .add_enabled(self.phase != Phase::Setup, egui::Button::new("▾"))
                    .on_hover_text(tr!("Recent links", "Последние ссылки"))
                    .clicked()
                {
                    toggle_hist = true;
                }
                #[cfg(windows)]
                if ui
                    .add_enabled(
                        self.phase != Phase::Setup,
                        egui::Button::new(tr!("file", "файл")),
                    )
                    .on_hover_text(tr!(
                        "Choose a local .torrent file",
                        "Выбрать локальный .torrent-файл"
                    ))
                    .clicked()
                {
                    pick_torrent = true;
                }
                let edit = egui::TextEdit::singleline(&mut self.url)
                    .font(field_font())
                    .desired_width(f32::INFINITY)
                    // Default TextEdit margin is (4,2) - the mockup's inputs
                    // use `padding: 9px 11px`, noticeably roomier.
                    .margin(egui::Margin::symmetric(11, 9))
                    .hint_text(tr!(
                        "https://… magnet:… or a .torrent path",
                        "https://… magnet:… или путь к .torrent"
                    ));
                // Stays enabled while a job is Running (not just Idle): a
                // queue means the next link can be typed/pasted without
                // waiting for the active download to finish.
                ui.add_enabled(self.phase != Phase::Setup, edit);
            });
        });
        #[cfg(windows)]
        if pick_torrent {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter(
                    tr!("Torrents and metalinks", "Торренты и металинки"),
                    &["torrent", "metalink", "meta4"],
                )
                .pick_file()
            {
                self.set_torrent_file(&path);
            }
        }
        if toggle_hist {
            self.show_url_history = !self.show_url_history;
        }
        if self.show_url_history {
            let urls: Vec<String> = self.cfg.urls.iter().take(8).cloned().collect();
            let mut picked = None;
            let mut open = true;
            let window = egui::Window::new(window_title(tr!("Recent links", "Последние ссылки")))
                .open(&mut open)
                .resizable(false)
                .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
                .show(ctx, |ui| {
                    if urls.is_empty() {
                        ui.weak(tr!(
                            "(history is empty — links appear after successful downloads)",
                            "(история пуста — ссылки появятся после успешных скачиваний)"
                        ));
                    }
                    for u in &urls {
                        if ui.selectable_label(false, clip(u, 64)).clicked() {
                            picked = Some(u.clone());
                        }
                    }
                });
            self.show_url_history = open
                && (toggle_hist
                    || !window
                        .as_ref()
                        .is_some_and(|w| clicked_outside(ctx, w.response.rect)));
            if let Some(u) = picked {
                self.url = u;
                self.show_url_history = false;
            }
        }
    }

    pub(super) fn ui_engine_row(&mut self, ui: &mut egui::Ui) -> bool {
        // Not gated on Idle: a Running job doesn't own these fields, the next
        // queued link does, so движок/формат/папка must stay editable.
        let enabled = self.phase != Phase::Setup;
        let mut dir_history_opened = false;
        ui.add_space(6.0);
        tag_label(ui, tr!("engine", "движок"));
        ui.horizontal(|ui| {
            let buttons = ui.add_enabled_ui(enabled, |ui| {
                // Flat "● label" / "○ label" - see engine_choice's doc comment
                // for why this replaced both radio_value and selectable_value.
                if engine_choice(
                    ui,
                    tr!("auto", "авто"),
                    self.engine_mode == EngineMode::Auto,
                )
                .clicked()
                {
                    self.engine_mode = EngineMode::Auto;
                }
                if engine_choice(ui, "yt-dlp", self.engine_mode == EngineMode::YtDlp).clicked() {
                    self.engine_mode = EngineMode::YtDlp;
                }
                if engine_choice(ui, "aria2c", self.engine_mode == EngineMode::Aria2).clicked() {
                    self.engine_mode = EngineMode::Aria2;
                }
            });
            if self.engine_mode == EngineMode::Auto {
                let guess = self.effective_engine();
                if !self.url.trim().is_empty() {
                    let guess_label = if !self.link_ready() {
                        tr!("checking link…", "проверяю ссылку…")
                    } else {
                        guess.as_str()
                    };
                    // A check that was needed but never reached the server:
                    // the guess comes from the address alone, and saying so is
                    // better than letting "auto" imply a verified answer.
                    let unchecked = self.link_ready()
                        && self.needs_link_probe()
                        && self.resolved_link().is_some_and(|info| !info.checked);
                    let guess_text = if unchecked {
                        format!("→ {guess_label} · {}", tr!("not checked", "не проверено"))
                    } else {
                        format!("→ {guess_label}")
                    };
                    // Two guesses at a font-tweak fix (the default label font,
                    // then the "button" family) each moved this the wrong way
                    // or overshot - the buttons' own vertical centering isn't
                    // something a font offset on a *different* widget can
                    // reliably match. Instead, read back the actual rendered
                    // rect of the button row and paint this text at that
                    // exact center - no font-metric guessing involved.
                    let color = ui.visuals().weak_text_color();
                    let font = egui::FontId::new(12.0, egui::FontFamily::Proportional);
                    let galley = ui.painter().layout_no_wrap(guess_text, font, color);
                    // Exact rect-center math landed a hair high - likely the
                    // "→" glyph's own ink sits above its box's true center,
                    // not a layout issue. Small fixed nudge down.
                    let pos = egui::pos2(
                        ui.cursor().left(),
                        buttons.response.rect.center().y - galley.size().y / 2.0 + 2.0,
                    );
                    ui.painter().galley(pos, galley.clone(), color);
                    ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                }
            }
        });
        let engine = self.effective_engine();
        ui.add_space(6.0);
        tag_label(ui, tr!("format", "формат"));
        if engine == Engine::YtDlp && self.link_ready() {
            self.fmt = engines::ui_format_for_url(&self.url, self.fmt);
            let formats = engines::formats_for_url(&self.url);
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt("fmt")
                    .selected_text(
                        formats
                            .iter()
                            .find(|(k, _)| *k == self.fmt)
                            .map_or(Format::Best.label(), |(_, l)| *l),
                    )
                    .show_ui(ui, |ui| {
                        for (key, label) in formats {
                            ui.selectable_value(&mut self.fmt, key, label);
                        }
                    });
            });
        } else {
            ui.weak(if !self.link_ready() {
                tr!(
                    "Checking the download link…",
                    "Проверяю ссылку на загрузку…"
                )
            } else {
                tr!(
                    "Original file (no conversion)",
                    "Исходный файл (без преобразования)"
                )
            });
        }
        if let Some(name) = self.resolved_link().and_then(|info| info.name.as_ref()) {
            ui.weak(clip(name.as_str(), 72));
        }
        ui.add_space(6.0);
        tag_label(ui, tr!("folder", "папка"));
        ui.horizontal(|ui| {
            let mut open_browser = false;
            let mut toggle_hist = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Same as the url-history ▾: an empty history must not look
                // like a dead button.
                if ui
                    .add_enabled(enabled, egui::Button::new("▾"))
                    .on_hover_text(tr!("Recent folders", "Последние папки"))
                    .clicked()
                {
                    toggle_hist = true;
                }
                if ui
                    .add_enabled(enabled, egui::Button::new(tr!("browse", "обзор")))
                    .on_hover_text(tr!(
                        "Choose a download folder",
                        "Выбрать папку для скачивания"
                    ))
                    .clicked()
                {
                    open_browser = true;
                }
                ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut self.out_dir)
                        .font(field_font())
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(11, 9)),
                );
            });
            if open_browser {
                self.open_dir_browser();
            }
            if toggle_hist {
                self.show_dir_history = !self.show_dir_history;
                dir_history_opened = self.show_dir_history;
            }
        });
        dir_history_opened
    }

    pub(super) fn ui_dir_history(&mut self, ctx: &egui::Context, opened_this_frame: bool) {
        if self.torrent_pick.is_some() || self.close_requested {
            self.show_dir_history = false;
            return;
        }
        if !self.show_dir_history {
            return;
        }
        let dirs: Vec<String> = self.cfg.dirs.iter().take(8).cloned().collect();
        let mut picked = None;
        let mut open = true;
        let window = egui::Window::new(window_title(tr!("Recent folders", "Последние папки")))
            .open(&mut open)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 200.0])
            .show(ctx, |ui| {
                if dirs.is_empty() {
                    ui.weak(tr!(
                        "(history is empty — folders appear after successful downloads)",
                        "(история пуста — папки появятся после успешных скачиваний)"
                    ));
                }
                for d in &dirs {
                    if ui.selectable_label(false, clip(d, 64)).clicked() {
                        picked = Some(d.clone());
                    }
                }
            });
        self.show_dir_history = open
            && (opened_this_frame
                || !window
                    .as_ref()
                    .is_some_and(|w| clicked_outside(ctx, w.response.rect)));
        if let Some(d) = picked {
            self.out_dir = d;
            self.show_dir_history = false;
        }
    }

    pub(super) fn ui_log_window(&mut self, ctx: &egui::Context, opened_this_frame: bool) {
        if !self.show_log_window {
            return;
        }
        // Re-join only when the log actually changed. egui's galley cache is
        // keyed on the full LayoutJob (text included): an identical string is
        // a cheap cache hit, but joining ~500 lines EVERY frame produced a
        // new String every frame anyway and any change forced a full
        // re-layout + re-tessellation of the whole log.
        if self.log_cache_rev != self.log_rev {
            self.log_cache = self.log.join("\n");
            self.log_cache_rev = self.log_rev;
        }
        let mut open = true;
        let window = egui::Window::new(window_title(tr!("Log", "Журнал")))
            .open(&mut open)
            .resizable(true)
            .default_size([560.0, 320.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show_terminal(ui, |ui| {
                        ui.label(
                            egui::RichText::new(self.log_cache.as_str())
                                .monospace()
                                .size(11.0)
                                .weak(),
                        );
                    });
            });
        self.show_log_window = open
            && (opened_this_frame
                || !window
                    .as_ref()
                    .is_some_and(|w| clicked_outside(ctx, w.response.rect)));
    }
}
