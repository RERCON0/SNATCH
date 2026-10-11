use super::*;

impl SnatchApp {
    pub(super) fn open_dir_browser(&mut self) {
        if self.close_requested || self.torrent_pick.is_some() {
            return;
        }
        // Cheap bitmask query - a drive plugged in since startup shows up.
        self.drives = snatch_rs::ui::drive_roots();
        let typed = PathBuf::from(self.out_dir.trim());
        if reaches_unc(&typed) {
            self.set_status(
                StatusKind::Err,
                tr!(
                    "Cannot open a network UNC folder. Choose a local folder.",
                    "Сетевую UNC-папку нельзя открывать. Выберите локальную папку."
                ),
            );
            return;
        }
        // Probe only volumes the OS currently reports as logical drives. A
        // stale out_dir on a removed USB stick / disconnected mapping would
        // otherwise block the UI thread inside GetFileAttributes until the
        // device times out - the same stall class drive_roots() avoids at
        // startup (see its comment above). Unknown roots open at home.
        #[cfg(windows)]
        let root_known = path_drive_letter(&typed).is_some_and(|letter| {
            self.drives
                .iter()
                .any(|d| path_drive_letter(d) == Some(letter))
        });
        #[cfg(not(windows))]
        let root_known = true;
        let start = if !root_known || typed.as_os_str().is_empty() {
            home_dir().unwrap_or_else(|| PathBuf::from("."))
        } else {
            typed
        };
        self.browser.current = start;
        self.browser.listed_for = None;
        self.browser.resolve_initial = true;
        self.browser.entries.clear();
        self.browser.error = None;
        self.browser.open = true;
    }

    pub(super) fn ui_dir_browser(&mut self, ctx: &egui::Context) {
        if self.torrent_pick.is_some() || self.close_requested {
            self.browser.open = false;
            return;
        }
        if !self.browser.open {
            return;
        }
        let opened_this_frame = self.browser.listed_for.is_none();
        if self.browser.listed_for.as_ref() != Some(&self.browser.current)
            && self.browser.in_flight.is_none()
        {
            let requested = self.browser.current.clone();
            let initial = self.browser.resolve_initial;
            self.browser.resolve_initial = false;
            self.browser.in_flight = Some(requested.clone());
            self.browser.entries.clear();
            self.browser.error = None;
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            // One active scan; newer navigation coalesces in browser.current.
            // Both the initial is_dir probe and enumeration stay off the UI.
            std::thread::spawn(move || {
                let mut resolved = requested.clone();
                if initial && !reaches_unc(&resolved) && !resolved.is_dir() {
                    if let Some(parent) =
                        resolved.parent().filter(|p| !reaches_unc(p) && p.is_dir())
                    {
                        resolved = parent.to_path_buf();
                    }
                }
                let result = try_read_dirs_limited(&resolved, DIR_BROWSER_SCAN_CAP)
                    .map_err(|e| e.to_string());
                let _ = tx.send(Msg::DirListed(requested, resolved, result));
                ctx.request_repaint();
            });
        }

        let mut still_open = true;
        let mut cancel_clicked = false;
        let mut chosen: Option<PathBuf> = None;
        let mut navigate: Option<PathBuf> = None;
        let cur = self.browser.current.clone();
        let entries = &self.browser.entries;
        let truncated = self.browser.truncated;
        let loading = self.browser.listed_for.as_ref() != Some(&cur);
        let error = self.browser.error.clone();
        let home = home_dir().unwrap_or_default();
        let desktop = snatch_rs::config::desktop_dir().unwrap_or_else(|| home.join("Desktop"));
        let downloads =
            snatch_rs::config::downloads_dir().unwrap_or_else(|| home.join("Downloads"));
        let drives = self.drives.clone();

        let window = egui::Window::new(window_title(tr!("Choose folder", "Выбор папки")))
            .open(&mut still_open)
            .resizable(true)
            .default_size([480.0, 440.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button(tr!("Home", "Дом")).clicked() {
                        navigate = Some(home.clone());
                    }
                    if ui.button(tr!("Desktop", "Рабочий стол")).clicked() {
                        navigate = Some(desktop.clone());
                    }
                    if ui.button(tr!("Downloads", "Загрузки")).clicked() {
                        navigate = Some(downloads.clone());
                    }
                    for d in &drives {
                        let label = d.to_string_lossy();
                        if ui.button(label.trim_end_matches('\\').to_string()).clicked() {
                            navigate = Some(d.clone());
                        }
                    }
                });
                ui.separator();
                ui.label(egui::RichText::new(cur.display().to_string()).strong());
                ui.separator();
                // Plain rows (selectable_label paints nothing until hovered)
                // instead of bordered buttons: a scrollable list of ~500
                // identically-boxed buttons read as one undifferentiated
                // wall - nothing marked this as a *list* rather than a grid
                // of actions.
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, false])
                    .show_terminal(ui, |ui| {
                        if let Some(parent) = cur.parent() {
                            if ui.selectable_label(false, tr!("↑ Parent folder", "↑ Наверх")).clicked() {
                                navigate = Some(parent.to_path_buf());
                            }
                        }
                        let shown = entries.len().min(500);
                        for d in &entries[..shown] {
                            let name = d.file_name().unwrap_or_default().to_string_lossy();
                            if ui.selectable_label(false, name.as_ref()).clicked() {
                                navigate = Some(d.clone());
                            }
                        }
                        if loading {
                            slow_spinner(ui); ui.weak(tr!("Reading folder…", "Читаю каталог…"));
                        } else if let Some(error) = &error {
                            ui.colored_label(ui.visuals().error_fg_color, tr_format!("Could not read folder: {error}", "Не удалось прочитать каталог: {error}"));
                        } else if entries.is_empty() {
                            if truncated {
                                ui.weak(tr_format!("… no subfolders among the first {DIR_BROWSER_SCAN_CAP} entries", "… среди первых {DIR_BROWSER_SCAN_CAP} записей вложенных папок нет"
                                ));
                            } else {
                                ui.weak(tr!("(no subfolders)", "(нет вложенных папок)"));
                            }
                        } else if truncated {
                            // The scan stopped at DIR_BROWSER_SCAN_CAP: the
                            // tail is simply not listed, no navigation
                            // reveals it - say so instead of pretending the
                            // list is complete.
                            ui.weak(tr_format!("… this folder is very large: showing the first {shown} subfolders among {DIR_BROWSER_SCAN_CAP} entries — choose a subfolder or reopen the folder", "… каталог очень большой: из первых {DIR_BROWSER_SCAN_CAP} записей \
                                 показаны первые {shown} папок — выберите подпапку или откройте \
                                 папку заново"
                            ));
                        } else if entries.len() > shown {
                            // The old "поднимитесь выше" advice was a lie:
                            // the list is alphabetical and the tail is simply
                            // never rendered, no navigation reveals it.
                            ui.weak(tr_format!("… showing the first {shown} folders alphabetically ({} total)", "… показаны первые {shown} папок по алфавиту (всего {})",
                                entries.len()
                            ));
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    // The one button that actually closes the dialog with a
                    // result gets the same accent treatment as "СКАЧАТЬ" -
                    // everything else here was equally weighted, so it was
                    // easy to navigate somewhere and not notice you still
                    // had to confirm.
                    let accent = ui.visuals().hyperlink_color;
                    if ui.add_enabled(!loading && error.is_none(), accent_button(accent, tr!("Choose this folder", "Выбрать эту папку"))).clicked() {
                        chosen = Some(cur.clone());
                    }
                    if ui.button(tr!("Cancel", "Отмена")).clicked() {
                        cancel_clicked = true;
                    }
                });
            });

        if cancel_clicked {
            still_open = false;
        }
        // Must be assigned before the `chosen` branch below, not after: this
        // reflects only the native window chrome / Cancel closing the dialog.
        // Confirming a folder doesn't touch `still_open` (the egui::Window
        // itself was never told to close), so writing it last would clobber
        // the `open = false` a confirmed pick sets, leaving the window stuck
        // open even though out_dir had already been updated.
        self.browser.open = still_open
            && (opened_this_frame
                || !window
                    .as_ref()
                    .is_some_and(|w| clicked_outside(ctx, w.response.rect)));
        if let Some(p) = navigate {
            if !reaches_unc(&p) {
                self.browser.current = p;
            }
        }
        if let Some(p) = chosen {
            self.out_dir = p.to_string_lossy().into_owned();
            self.browser.open = false;
        }
    }
}
