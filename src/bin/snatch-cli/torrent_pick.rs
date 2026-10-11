use super::*;

/// Outcome of [`ask_torrent_files`].
pub(super) enum TorrentAsk {
    /// Download this link with that selection.
    Use(torrent::Choice),
    /// Leave just this link out; the other links of the session stay.
    Skip,
    /// The user cancelled the whole interactive flow (Esc).
    Cancel,
}

/// Release panics abort without Drop, so terminal cleanup also has a panic
/// hook. During metadata/picking, console Ctrl+C cancels the active operation.
mod picker_console {
    use std::sync::atomic::{AtomicBool, Ordering};
    pub static CANCEL: AtomicBool = AtomicBool::new(false);
    static LISTEN: AtomicBool = AtomicBool::new(false);
    static RAW: AtomicBool = AtomicBool::new(false);
    static ALTERNATE: AtomicBool = AtomicBool::new(false);

    pub fn restore() {
        if !RAW.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        if ALTERNATE.swap(false, Ordering::SeqCst) {
            let _ = crossterm::execute!(out, crossterm::terminal::LeaveAlternateScreen);
        }
        let _ = crossterm::execute!(out, crossterm::cursor::Show);
    }

    #[cfg(windows)]
    unsafe extern "system" fn control(event: u32) -> i32 {
        if event <= 1 && LISTEN.load(Ordering::SeqCst) {
            CANCEL.store(true, Ordering::SeqCst);
            return 1;
        }
        restore();
        0
    }

    pub struct InterruptGuard;
    impl InterruptGuard {
        pub fn new() -> Self {
            static INIT: std::sync::Once = std::sync::Once::new();
            INIT.call_once(|| {
                let previous = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    restore();
                    previous(info);
                }));
                #[cfg(windows)]
                unsafe {
                    extern "system" {
                        fn SetConsoleCtrlHandler(
                            handler: Option<unsafe extern "system" fn(u32) -> i32>,
                            add: i32,
                        ) -> i32;
                    }
                    SetConsoleCtrlHandler(Some(control), 1);
                }
            });
            CANCEL.store(false, Ordering::SeqCst);
            LISTEN.store(true, Ordering::SeqCst);
            Self
        }
    }
    impl Drop for InterruptGuard {
        fn drop(&mut self) {
            LISTEN.store(false, Ordering::SeqCst);
        }
    }

    pub fn raw_started(alternate: bool) {
        ALTERNATE.store(alternate, Ordering::SeqCst);
        RAW.store(true, Ordering::SeqCst);
    }
}

/// Interactive file selection for a torrent plan (the `-y` flow never asks).
pub(super) fn ask_torrent_files(tc: &Toolchain, plan: &Plan) -> TorrentAsk {
    use TorrentAsk::{Cancel, Skip, Use};
    if plan.engine != Engine::Aria2 || !engines::is_bittorrent_input(&plan.url) {
        return Use(torrent::Choice::default());
    }
    let Some(aria2c) = &tc.aria2c else {
        return Use(torrent::Choice::default());
    };
    // Same gate the download itself applies, and BEFORE anything touches the
    // input: a \\host\share path must not even be probed (SMB auth leak),
    // and a malformed link should not cost a metadata fetch first. An invalid
    // link skips the picker; download() reports the reason.
    let Ok(url) = validate_url(&plan.url) else {
        return Use(torrent::Choice::default());
    };
    outln(tr!(
        "⌛ fetching torrent file list (magnets may take a minute; Ctrl+C to cancel)…",
        "⌛ получаю список файлов торрента (для magnet это может занять минуту; Ctrl+C — отмена)…"
    ));
    let _interrupt = picker_console::InterruptGuard::new();
    let listing = match torrent::show_files(
        aria2c,
        &url,
        torrent::METADATA_TIMEOUT,
        &picker_console::CANCEL,
    ) {
        Ok(listing) => listing,
        Err(_) if picker_console::CANCEL.load(Ordering::SeqCst) => return Cancel,
        Err(e) => {
            drop(_interrupt);
            errln(format!("⚠ {e}"));
            // Ask instead of silently switching to the whole torrent. "Нет"
            // drops only this link; Esc still cancels the whole flow.
            return match Confirm::new(tr!(
                "File list is unavailable. Download the entire torrent?",
                "Список файлов недоступен. Скачать торрент целиком?"
            ))
            .with_default(true)
            .with_help_message(tr!(
                "answer “no” to skip this link",
                "«нет» — пропустить эту ссылку"
            ))
            .prompt()
            {
                Ok(true) => Use(torrent::Choice::default()),
                Ok(false) => Skip,
                Err(_) => Cancel,
            };
        }
    };
    drop(_interrupt);
    let info = &listing.info;
    // Every outcome keeps the fetched metadata: the download then reuses it
    // instead of asking the swarm for it a second time.
    let all = torrent::Choice {
        name: Some(info.name.clone()),
        files: None,
        meta: listing.meta.clone(),
    };
    if info.files.len() <= 1 {
        return Use(all);
    }
    outln(tr_format!(
        "Torrent “{}”: {} files, {} total",
        "Торрент «{}»: файлов {}, всего {}",
        clip(&info.name, 50),
        info.files.len(),
        torrent::human_size(info.total)
    ));
    // Default is the whole torrent (what the release intends); the full
    // file picker only opens when the user explicitly asks for it - dumping
    // hundreds of entries unprompted is unreadable.
    match Confirm::new(&tr_format!(
        "Download all {} files as shared?",
        "Скачать все {} файлов (как в раздаче)?",
        info.files.len()
    ))
    .with_default(true)
    .with_help_message(tr!(
        "answer “no” to select individual files",
        "ответьте «нет», чтобы выбрать отдельные файлы"
    ))
    .prompt()
    {
        Ok(true) => return Use(all),
        Ok(false) => {}
        Err(_) => return Cancel,
    }
    // Partial selection: interactive folders-and-files tree.
    outln(tr!(
        "↑↓ move · →/← expand · Space select · Enter confirm · Esc cancel",
        "↑↓ движение · →/← раскрыть · Space отметить · Enter подтвердить · Esc отмена"
    ));
    match pick_torrent_files(info) {
        Ok(Some(sel)) => {
            outln(partial_neighbours_note());
            Use(torrent::Choice {
                name: Some(info.name.clone()),
                files: Some(sel),
                meta: listing.meta,
            })
        }
        Ok(None) => Use(all),
        Err(()) => Cancel,
    }
}

/// aria2 downloads whole pieces, and a piece crossing a file boundary also
/// writes into the unselected neighbour - which then looks full-size but is
/// incomplete. Said once at pick time and again when the job finishes.
pub(super) fn partial_neighbours_note() -> &'static str {
    tr!(
        "ℹ unselected neighboring files may be partially downloaded — this is how torrents work",
        "ℹ соседние невыбранные файлы могут появиться частично скачанными — так устроены торренты"
    )
}

/// Interactive tree picker for torrent files (qbittorrent-style: folders
/// collapse, Space toggles a whole subtree). Returns the chosen aria2 file
/// indices; Err on cancel.
pub(super) fn picker_view_rows(height: u16) -> usize {
    height.saturating_sub(5) as usize
}

pub(super) fn pick_torrent_files(info: &torrent::TorrentInfo) -> Result<Option<Vec<usize>>, ()> {
    use crossterm::event::{self, Event, KeyCode};
    use crossterm::{cursor, execute, queue, terminal};
    use std::io::Write;

    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            picker_console::restore();
        }
    }

    let mut tree = torrent::build_torrent_tree(&info.files);
    let mut selected = vec![true; info.files.len()];
    let mut cursor = 0usize;
    // Why the last Enter was refused (shown under the list until a key).
    let mut note: Option<String> = None;
    // Without VT (legacy conhost) crossterm makes the alternate screen a
    // separate WinAPI buffer while write!(stdout) keeps drawing into the
    // original one: the picker would be invisible. There it redraws the main
    // screen instead, as before the alternate screen existed.
    let alternate = vt_processing_enabled();
    let _interrupt = picker_console::InterruptGuard::new();
    terminal::enable_raw_mode().map_err(|_| ())?;
    picker_console::raw_started(alternate);
    let _guard = RawGuard;
    let mut out = std::io::stdout();
    if alternate {
        let _ = execute!(out, terminal::EnterAlternateScreen);
    }
    let clear = if alternate {
        terminal::ClearType::FromCursorDown
    } else {
        terminal::ClearType::All
    };
    // Drop keys buffered by the previous inquire prompt: a stale Enter in
    // the queue would otherwise confirm the picker instantly, leaving the
    // drawn tree behind while the next prompt silently takes over input.
    while event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }
    // Some keys (and their release events) can still be in flight right
    // after the previous prompt; a short pause plus a second drain catches
    // the late ones, so the picker cannot be confirmed by a ghost Enter.
    std::thread::sleep(std::time::Duration::from_millis(80));
    while event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }

    let mut view_cache = torrent::TreeView::default();
    loop {
        view_cache.prepare(&mut tree, &selected);
        let rows = &view_cache.rows;
        if cursor >= rows.len() {
            cursor = rows.len().saturating_sub(1);
        }
        let (cols, term_h) = terminal::size().unwrap_or((100, 30));
        let view = picker_view_rows(term_h);
        let start = cursor.saturating_sub(view.saturating_sub(1));
        let width = (cols as usize).saturating_sub(1);
        let _ = queue!(
            out,
            cursor::MoveTo(0, 0),
            terminal::Clear(clear),
            cursor::Hide
        );
        let header = tr!("FILE SELECTION — ↑↓ move · →/← expand · Space select · Enter confirm · Esc cancel", "ВЫБОР ФАЙЛОВ — ↑↓ движение · →/← раскрыть · Space отметить · Enter подтвердить · Esc отмена");
        if view == 0 {
            let _ = write!(
                out,
                "{}",
                clip_cells(
                    tr!(
                        "Increase terminal height (Esc to cancel)",
                        "Увеличьте высоту терминала (Esc — отмена)"
                    ),
                    width
                )
            );
        } else {
            let _ = write!(out, "{}\r\n\r\n", clip_cells(header, width));
        }
        for (ri, (path, depth)) in rows.iter().enumerate().skip(start).take(view) {
            let node = torrent::node_at(&tree, path);
            let mark = if node.file.is_none() {
                if node.expanded {
                    "▼"
                } else {
                    "▶"
                }
            } else {
                " "
            };
            let (sel, total) = node.selection;
            let box_ = if node.file.is_some() {
                if sel > 0 { "[x]" } else { "[ ]" }.to_string()
            } else if total > 0 && sel == total {
                "[x]".to_string()
            } else if sel > 0 {
                format!("[{sel}/{total}]")
            } else {
                "[ ]".to_string()
            };
            // ">" marks the cursor row in plain text: on a legacy console
            // (no VT) crossterm's Reverse attribute is a no-op, so the
            // highlight alone left no visible cursor at all. Size leads the
            // name so clipping a long name never hides it.
            let line = format!(
                "{}{}{} {} {} · {}",
                if ri == cursor { "> " } else { "  " },
                if *depth > 6 {
                    format!("{}… ", "  ".repeat(6))
                } else {
                    "  ".repeat(*depth)
                },
                mark,
                box_,
                torrent::human_size(node.size),
                clip(&node.name, 120)
            );
            let line = clip_cells(&line, width);
            if ri == cursor {
                let _ = queue!(
                    out,
                    crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse)
                );
            }
            let _ = write!(out, "{line}\r\n");
            if ri == cursor {
                let _ = queue!(
                    out,
                    crossterm::style::SetAttribute(crossterm::style::Attribute::Reset)
                );
            }
        }
        if let Some(text) = note.as_ref().filter(|_| view > 0) {
            let _ = write!(out, "\r\n{}\r\n", clip_cells(text, width));
        }
        let _ = out.flush();

        if picker_console::CANCEL.load(Ordering::SeqCst) {
            return Err(());
        }
        if !event::poll(std::time::Duration::from_millis(100)).map_err(|_| ())? {
            continue;
        }
        match event::read() {
            Ok(Event::Key(key)) => {
                // Ignore release/repeat events: only real presses navigate.
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                note = None;
                match key.code {
                    KeyCode::Up => cursor = cursor.saturating_sub(1),
                    KeyCode::Down => cursor = (cursor + 1).min(rows.len().saturating_sub(1)),
                    KeyCode::Left => {
                        if let Some((path, _)) = rows.get(cursor) {
                            let node = torrent::node_at_mut(&mut tree, path);
                            if node.file.is_none() {
                                view_cache.rows_dirty |= node.expanded;
                                node.expanded = false;
                            }
                        }
                    }
                    KeyCode::Right => {
                        if let Some((path, _)) = rows.get(cursor) {
                            let node = torrent::node_at_mut(&mut tree, path);
                            if node.file.is_none() {
                                view_cache.rows_dirty |= !node.expanded;
                                node.expanded = true;
                            }
                        }
                    }
                    KeyCode::Char(' ') => {
                        if let Some((path, _)) = rows.get(cursor) {
                            let node = torrent::node_at_mut(&mut tree, path);
                            if let Some(fi) = node.file {
                                if let Some(s) = selected.get_mut(fi) {
                                    *s = !*s;
                                }
                            } else {
                                let (sel, total) = node.selection;
                                node.set_all(&mut selected, sel != total);
                            }
                            view_cache.selection_dirty = true;
                        }
                    }
                    KeyCode::Enter => {
                        if view == 0 {
                            continue;
                        }
                        let chosen: Vec<usize> = info
                            .files
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| selected.get(*i).copied().unwrap_or(false))
                            .map(|(_, f)| f.index)
                            .collect();
                        // Nothing ticked: keep the picker open (Esc cancels).
                        if chosen.is_empty() {
                            note = Some(
                                tr!(
                                    "Select at least one file (Esc to cancel)",
                                    "Выберите хотя бы один файл (Esc — отмена)"
                                )
                                .into(),
                            );
                            continue;
                        }
                        if chosen.len() == info.files.len() {
                            return Ok(None);
                        }
                        // Too many scattered files for one command line: say so
                        // here, where the selection can still be changed.
                        if let Err(e) = torrent::select_spec(&chosen) {
                            note = Some(format!("⚠ {e}"));
                            continue;
                        }
                        return Ok(Some(chosen));
                    }
                    KeyCode::Esc | KeyCode::Char('q') => return Err(()),
                    KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        return Err(())
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(_) => return Err(()),
        }
    }
}

/// `clip` by terminal cells instead of chars: CJK names (common in torrents)
/// take two cells per char, so a char-count clip still wrapped the row and
/// broke the picker's one-row-per-line viewport math.
pub(super) fn clip_cells(text: &str, cells: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let width = |c: char| c.width().unwrap_or(0);
    if text.chars().map(width).sum::<usize>() <= cells {
        return text.to_string();
    }
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        // Keep one cell for the ellipsis.
        if used + width(c) + 1 > cells {
            break;
        }
        used += width(c);
        out.push(c);
    }
    out.push('…');
    out
}

// Port of tests/test_cli.py + tests/test_ui.py from the retired Python CLI.
// inquire's Select/Confirm have no monkeypatch equivalent (they open a real
// terminal via get_default_terminal()), so retry_with_cookies_inner and
// confirm_message/engine_guess_pair exist specifically to make this
// coverage possible without one - see their doc comments above.
