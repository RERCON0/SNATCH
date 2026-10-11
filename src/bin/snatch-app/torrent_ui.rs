use super::*;

/// The pick modal itself (list or error state). Free of `SnatchApp` so its
/// geometry can be tested; returns the frame's actions and the window rect.
pub(super) fn torrent_pick_window(
    ctx: &egui::Context,
    pick: &mut TorrentPick,
) -> (PickActions, Option<egui::Rect>) {
    let mut act = PickActions::default();
    // Never wider than the window: at the default 460px a fixed 560px dialog
    // pushed the folder toggles/checkboxes off-screen. Frame margins + a gap.
    let screen_w = ctx.screen_rect().width();
    let frame_w = ctx.style().spacing.window_margin.sum().x + 24.0;
    let width = (screen_w - frame_w).clamp(200.0, 560.0);
    let shown = egui::Window::new(window_title(tr!("TORRENT FILE SELECTION", "ВЫБОР ФАЙЛОВ ТОРРЕНТА")))
        .collapsible(false)
        .resizable(false)
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_width(width);
            if let Some(err) = &pick.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
                if let Some(hint) = &pick.hint { ui.colored_label(ui.visuals().warn_fg_color, hint); }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let accent = ui.visuals().hyperlink_color;
                        let all = ui.add(accent_button(accent, tr!("[ download all ]", "[ скачать всё ]")));
                        let cancel = ui.button(tr!("[ cancel ]", "[ отмена ]"));
                        act.download_all = all.clicked();
                        act.cancel = cancel.clicked();
                        act.buttons.extend([all.rect, cancel.rect]);
                    });
                });
                return;
            }
            let Some(info) = &pick.info else { return };
            ui.weak(tr_format!("“{}” · {} files · {} total", "«{}» · файлов {} · всего {}",
                clip(&info.name, 60),
                info.files.len(),
                torrent::human_size(info.total)
            ));
            ui.weak(tr_format!("Folder: {}", "Папка: {}", clip(&pick.job.out_dir.to_string_lossy(), 80)));
            ui.add_space(4.0);
            torrent_tree_rows(ui, &mut pick.tree, &mut pick.selected, &mut pick.view);
            let count = pick.view.selected;
            if count > 0 && count < pick.selected.len() {
                ui.add_space(4.0);
                ui.weak(tr!("Unselected neighboring files may be partially downloaded — this is how torrents work.", "Соседние невыбранные файлы могут появиться частично скачанными — так устроены торренты."));
            }
            if let Some(hint) = &pick.hint {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().warn_fg_color, hint);
            }
            ui.add_space(6.0);
            // Selection buttons and actions on separate rows: at the default
            // 460px width one row cannot hold all four, and the right-aligned
            // pair slid over "снять все" (its clicks landed on "отмена").
            ui.horizontal(|ui| {
                let all = ui.button(tr!("[ select all ]", "[ все ]"));
                let none = ui.button(tr!("[ clear all ]", "[ снять все ]"));
                act.all = all.clicked();
                act.none = none.clicked();
                act.buttons.extend([all.rect, none.rect]);
            });
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let accent = ui.visuals().hyperlink_color;
                    let confirm =
                        ui.add_enabled(count > 0, accent_button(accent, tr_format!("[ download ({count}) ]", "[ скачать ({count}) ]")));
                    let cancel = ui.button(tr!("[ cancel ]", "[ отмена ]"));
                    act.confirm = confirm.clicked();
                    act.cancel = cancel.clicked();
                    act.buttons.extend([confirm.rect, cancel.rect]);
                });
            });
        });
    (act, shown.map(|r| r.response.rect))
}

/// Visible rows of the pick tree, used by the row geometry test.
#[cfg(test)]
pub(super) fn torrent_visible_rows(
    nodes: &[torrent::TorrNode],
    depth: usize,
    prefix: &mut Vec<usize>,
    rows: &mut Vec<(Vec<usize>, usize)>,
) {
    for (i, n) in nodes.iter().enumerate() {
        prefix.push(i);
        rows.push((prefix.clone(), depth));
        if n.file.is_none() && n.expanded {
            torrent_visible_rows(&n.children, depth + 1, prefix, rows);
        }
        prefix.pop();
    }
}

/// Height of one pick-list row: a single interact-size control (checkbox /
/// ▶ toggle). `show_rows` positions rows by this number - the old fixed 22px
/// against real 28px rows made the scrollbar jump and clipped the last rows.
pub(super) fn torrent_row_height(ui: &egui::Ui) -> f32 {
    ui.spacing().interact_size.y
}

/// One row of the pick list, held to exactly `row_h` for folders and files.
pub(super) fn torrent_tree_row(
    ui: &mut egui::Ui,
    tree: &mut [torrent::TorrNode],
    selected: &mut [bool],
    row: &(Vec<usize>, usize),
    row_h: f32,
) -> (bool, bool) {
    let mut expanded_changed = false;
    let mut selection_changed = false;
    let (path, depth) = row;
    let (is_dir, name, size, expanded, leaf, sel, total) = {
        let node = torrent::node_at(tree, path);
        let (sel, total) = node.selection;
        (
            node.file.is_none(),
            node.name.clone(),
            node.size,
            node.expanded,
            node.file,
            sel,
            total,
        )
    };
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), row_h), layout, |ui| {
        ui.set_min_height(row_h);
        // A long name ends in "…" at the dialog's edge instead of widening
        // the row past it.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        ui.add_space((*depth).min(6) as f32 * 14.0);
        if *depth > 6 {
            ui.label("…");
        }
        if is_dir {
            let marker = if expanded { "▼" } else { "▶" };
            if ui
                .add(
                    egui::Button::new(marker)
                        .frame(false)
                        .min_size(egui::vec2(18.0, 18.0)),
                )
                .clicked()
            {
                torrent::node_at_mut(tree, path).expanded = !expanded;
                expanded_changed = true;
            }
            // A partly picked folder must not look unpicked: "–" box, and a
            // click ticks the whole folder.
            let partial = sel > 0 && sel < total;
            let mut all = total > 0 && sel == total;
            if ui
                .add(egui::Checkbox::new(&mut all, "").indeterminate(partial))
                .changed()
            {
                torrent::node_at_mut(tree, path).set_all(selected, all);
                selection_changed = true;
            }
            // Size and the partial count lead: a long name is what gets
            // truncated at the dialog's edge, never them.
            let count = if partial {
                format!("{sel}/{total} · ")
            } else {
                String::new()
            };
            ui.label(format!(
                "{} · {count}{}",
                torrent::human_size(size),
                clip(&name, 120)
            ));
        } else {
            ui.add_space(18.0);
            if let Some(fi) = leaf {
                if fi < selected.len() {
                    selection_changed |= ui
                        .checkbox(
                            &mut selected[fi],
                            format!("{} · {}", torrent::human_size(size), clip(&name, 120)),
                        )
                        .changed();
                }
            }
        }
    });
    (expanded_changed, selection_changed)
}

/// The pick list itself: flattened rows + a virtualized scroll area.
pub(super) fn torrent_tree_rows(
    ui: &mut egui::Ui,
    tree: &mut [torrent::TorrNode],
    selected: &mut [bool],
    view: &mut torrent::TreeView,
) {
    view.prepare(tree, selected);
    let row_h = torrent_row_height(ui);
    egui::ScrollArea::vertical()
        .max_height(380.0)
        .auto_shrink([false, true])
        .show_terminal_rows(ui, row_h, view.rows.len(), |ui, range| {
            for row in &view.rows[range] {
                let (expanded, selection) = torrent_tree_row(ui, tree, selected, row, row_h);
                view.rows_dirty |= expanded;
                view.selection_dirty |= selection;
            }
        });
}

impl SnatchApp {
    /// Torrent file-selection modal (list or error), shown once loading is
    /// done; the loading phase lives in the run row. Drawn in the Foreground
    /// order so update()'s dim overlay (Middle) stays beneath it.
    pub(super) fn ui_torrent_pick(&mut self, ctx: &egui::Context) {
        let Some(pick) = self.torrent_pick.as_mut() else {
            return;
        };
        if pick.loading() {
            return;
        }
        let (act, _) = torrent_pick_window(ctx, pick);
        if act.all || act.none {
            pick.selected.iter_mut().for_each(|s| *s = act.all);
            pick.view.selection_dirty = true;
            pick.hint = None;
        }
        // Never start aria2c while the installer is replacing it: keep the
        // pick open and say why.
        if (act.confirm || act.download_all) && self.phase == Phase::Setup {
            pick.hint = Some(
                tr!(
                    "Wait for tool installation to finish",
                    "Дождитесь установки загрузчиков"
                )
                .into(),
            );
            return;
        }
        if act.confirm {
            let chosen: Vec<usize> = pick
                .info
                .iter()
                .flat_map(|info| info.files.iter().enumerate())
                .filter(|(i, _)| pick.selected.get(*i).copied().unwrap_or(false))
                .map(|(_, f)| f.index)
                .collect();
            let every = chosen.len() == pick.selected.len();
            if chosen.is_empty() {
                pick.hint =
                    Some(tr!("Select at least one file", "Выберите хотя бы один файл").into());
                return;
            }
            // Refuse a too-scattered selection here, while it can be changed,
            // not as a failed start after the modal is gone.
            if let Err(e) = torrent::select_spec(&chosen) {
                pick.hint = Some(e);
            } else {
                let files = if every { None } else { Some(chosen) };
                self.finish_torrent_pick(ctx, files);
            }
            return;
        }
        if act.download_all {
            self.finish_torrent_pick(ctx, None);
            return;
        }
        if act.cancel {
            self.abort_torrent_pick();
        }
    }

    /// The user backed out of a pick (modal or loading row): stop it and
    /// give the link back to the URL field.
    pub(super) fn abort_torrent_pick(&mut self) {
        if let Some(job) = self.cancel_torrent_pick() {
            if self.url.trim().is_empty() {
                self.url = job.url;
            }
        }
        self.set_status(
            StatusKind::None,
            tr!("File selection canceled", "Выбор файлов отменён"),
        );
    }
}
