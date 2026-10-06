#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use snatch_rs::scrollbar::ScrollAreaExt;

use snatch_rs::config::{home_dir, Config};
use snatch_rs::engines::{
    self, detect_engine, is_unc_path, looks_like_auth, parse_progress, preflight_warning,
    validate_url, Job, COOKIES_BROWSERS, FORMATS,
};
use snatch_rs::setup::{
    install_aria2_with, install_deno_with, install_ffmpeg_with, install_yt_dlp_with, SetupProgress,
};
use snatch_rs::tools::{bootstrap_dir, Toolchain};
use snatch_rs::ui::{clip, try_read_dirs_limited};
use snatch_rs::{job_object, torrent};
use snatch_rs::{BANNER, TELEGRAM_URL};

const APP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " — by rercon prod.");
const MAX_LOG_LINES: usize = 500;
/// Folder browser: examine at most this many directory entries per listing
/// (the rendered list is separately capped at 500 rows). Bounds navigation
/// on huge or slow directories; `safe_read_dirs_limited` reports truncation
/// so the UI can say so honestly.
const DIR_BROWSER_SCAN_CAP: usize = 4000;
// Wider than the 40px window-control buttons (─/□/×): "день"/"ночь" at the
// title bar's 12.5pt Cascadia Mono need more room than a single glyph.
#[cfg(windows)]
const THEME_BTN_WIDTH: f32 = 56.0;

// "Terminal Native" palette (mockups/b-terminal.html) - accent/OK/warn/error
// hues now live inside terminal_visuals() since dark and light mode each need
// their own values for contrast; read them back via ui.visuals().hyperlink_color
// (accent/OK), .warn_fg_color, .error_fg_color rather than a fixed constant.

#[derive(PartialEq, Clone, Copy)]
enum EngineMode {
    Auto,
    YtDlp,
    Aria2,
}

/// Only tracks the installer now - "is a download running" is
/// `!self.jobs.is_empty()`, since there can be several at once.
#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Setup,
}

#[derive(Clone, Copy)]
enum StatusKind {
    None,
    Ok,
    Warn,
    Err,
}

/// Every per-job variant is tagged with that job's id so drain() can route
/// it to the right entry in `self.jobs` - concurrent downloads share the one
/// channel. Setup* /ToolsReady stay untagged: the installer is a single
/// global operation, not a job.
enum Msg {
    Log(u64, String),
    ErrLine(u64, String),
    Progress(u64, f32),
    TorrentStatus(u64, f32, String),
    /// File list resolved for a torrent job (metadata fetched off-thread).
    /// The tree is built on the listing thread: a huge/hostile torrent must
    /// not stall (or overflow) the UI thread.
    TorrentFiles(
        u64,
        Result<(torrent::Listing, Vec<torrent::TorrNode>), String>,
    ),
    DirListed(PathBuf, PathBuf, Result<(Vec<PathBuf>, bool), String>),
    TorrentMeta(u64, String),
    TorrentName(u64, String),
    TorrentLog(u64, String),
    Done(u64, i32),
    SetupLog(String),
    /// Live installer progress: asset label, bytes done, total if known.
    SetupProgress(String, u64, Option<u64>),
    /// (успех, обновлённая цепочка инструментов - discover делается в том же
    /// фоновом потоке, а не на UI-потоке)
    SetupDone(bool, Toolchain),
    ToolsReady(Toolchain),
}

/// One running or just-finished-and-being-retried download. Everything that
/// used to be a scalar field on SnatchApp (progress, torrent_name, per-run
/// auth_seen/resumed_seen/...) now lives here instead, one per concurrent job.
struct ActiveJob {
    id: u64,
    job: Job,
    /// Torrent name once known, else the URL clipped - used for the row
    /// header and to prefix this job's lines in the shared log.
    label: String,
    progress: Option<f32>,
    status: String,
    status_kind: StatusKind,
    warns: Vec<String>,
    cancel_flag: Arc<AtomicBool>,
    cancelled: bool,
    /// Set by the "пауза" button - kills the process the same way `cancelled`
    /// does, but finish_job() moves the job into `SnatchApp::paused` instead
    /// of dropping it, so one click on "продолжить" respawns it. Works
    /// because yt-dlp/aria2c already continue an interrupted download from
    /// its own partial file (see resumed_seen/no_continue below); pausing
    /// doesn't need any extra IPC to the child, just an ordinary kill+restart.
    pausing: bool,
    auth_seen: bool,
    missing_control_file: bool,
    resumed_seen: bool,
    /// Whether *this* run already used --no-continue (a from-scratch retry
    /// of a corrupt resume) - distinct from resumed_seen, which is whether
    /// yt-dlp printed "Resuming download" during this run.
    no_continue: bool,
    /// Torrent file selection + fetched metadata, kept so a paused job
    /// resumes with the same selection (default for every other job).
    select: torrent::Choice,
}

impl ActiveJob {
    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_kind = kind;
        self.status = text.into();
    }
}

/// A job stopped via "пауза" instead of "отмена" - just enough to redisplay
/// it and respawn it unchanged when the user clicks "продолжить".
#[derive(Clone)]
struct PausedJob {
    job: Job,
    label: String,
    progress: Option<f32>,
    status: String,
    select: torrent::Choice,
    no_continue: bool,
}

#[derive(Clone)]
struct QueuedJob {
    job: Job,
    select: torrent::Choice,
    no_continue: bool,
}

/// A torrent job waiting in the file-pick modal: first while its file list
/// is fetched off-thread, then while the user ticks the files.
struct TorrentPick {
    id: u64,
    job: Job,
    /// Set on cancel/close: the listing thread kills its aria2c at once
    /// instead of fetching metadata nobody will use.
    cancel: Arc<AtomicBool>,
    info: Option<torrent::TorrentInfo>,
    /// Metadata fetched for a magnet/http(s) input; the download reuses it.
    meta: Option<torrent::Meta>,
    selected: Vec<bool>,
    tree: Vec<torrent::TorrNode>,
    view: torrent::TreeView,
    /// The listing failed: the modal says why and offers the whole torrent
    /// instead of silently switching to it.
    error: Option<String>,
    /// Why "скачать" was refused (a selection too scattered to pass).
    hint: Option<String>,
}

impl TorrentPick {
    /// Still fetching the file list: the form stays usable for running jobs
    /// and the run row shows a cancel; the modal only opens once there is a
    /// list (or an error) to act on.
    fn loading(&self) -> bool {
        self.info.is_none() && self.error.is_none()
    }
}

/// What a frame of the pick modal asked for.
#[derive(Default)]
struct PickActions {
    confirm: bool,
    cancel: bool,
    all: bool,
    none: bool,
    download_all: bool,
    /// Rects of the dialog's buttons this frame - the layout test checks
    /// they stay inside the dialog and never overlap.
    #[cfg_attr(not(test), allow(dead_code))]
    buttons: Vec<egui::Rect>,
}

/// The pick modal itself (list or error state). Free of `SnatchApp` so its
/// geometry can be tested; returns the frame's actions and the window rect.
fn torrent_pick_window(
    ctx: &egui::Context,
    pick: &mut TorrentPick,
) -> (PickActions, Option<egui::Rect>) {
    let mut act = PickActions::default();
    // Never wider than the window: at the default 460px a fixed 560px dialog
    // pushed the folder toggles/checkboxes off-screen. Frame margins + a gap.
    let screen_w = ctx.screen_rect().width();
    let frame_w = ctx.style().spacing.window_margin.sum().x + 24.0;
    let width = (screen_w - frame_w).clamp(200.0, 560.0);
    let shown = egui::Window::new(window_title("ВЫБОР ФАЙЛОВ ТОРРЕНТА"))
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
                        let all = ui.add(accent_button(accent, "[ скачать всё ]"));
                        let cancel = ui.button("[ отмена ]");
                        act.download_all = all.clicked();
                        act.cancel = cancel.clicked();
                        act.buttons.extend([all.rect, cancel.rect]);
                    });
                });
                return;
            }
            let Some(info) = &pick.info else { return };
            ui.weak(format!(
                "«{}» · файлов {} · всего {}",
                clip(&info.name, 60),
                info.files.len(),
                torrent::human_size(info.total)
            ));
            ui.weak(format!("Папка: {}", clip(&pick.job.out_dir.to_string_lossy(), 80)));
            ui.add_space(4.0);
            torrent_tree_rows(ui, &mut pick.tree, &mut pick.selected, &mut pick.view);
            let count = pick.view.selected;
            if count > 0 && count < pick.selected.len() {
                ui.add_space(4.0);
                ui.weak("Соседние невыбранные файлы могут появиться частично скачанными — так устроены торренты.");
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
                let all = ui.button("[ все ]");
                let none = ui.button("[ снять все ]");
                act.all = all.clicked();
                act.none = none.clicked();
                act.buttons.extend([all.rect, none.rect]);
            });
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let accent = ui.visuals().hyperlink_color;
                    let confirm =
                        ui.add_enabled(count > 0, accent_button(accent, format!("[ скачать ({count}) ]")));
                    let cancel = ui.button("[ отмена ]");
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
fn torrent_visible_rows(
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
fn torrent_row_height(ui: &egui::Ui) -> f32 {
    ui.spacing().interact_size.y
}

/// One row of the pick list, held to exactly `row_h` for folders and files.
fn torrent_tree_row(
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
fn torrent_tree_rows(
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

struct DirBrowser {
    open: bool,
    current: PathBuf,
    entries: Vec<PathBuf>,
    /// The last listing hit `DIR_BROWSER_SCAN_CAP`: more entries may exist.
    truncated: bool,
    listed_for: Option<PathBuf>,
    in_flight: Option<PathBuf>,
    resolve_initial: bool,
    error: Option<String>,
}

fn torrent_console_line(id: u64, line: &str) -> Option<Msg> {
    let line = line.trim();
    if line.is_empty()
        || line.contains("Download Progress Summary as of")
        || line.chars().all(|c| matches!(c, '=' | '-'))
    {
        return None;
    }
    if line.starts_with("FILE:") && line.contains("[MEMORY][METADATA]") {
        let name = line.split_once("[DL]").map_or("", |(_, name)| name.trim());
        return Some(Msg::TorrentMeta(id, name.to_string()));
    }
    if let Some(name) = engines::aria2_name_from_file(line) {
        return Some(Msg::TorrentName(id, name));
    }
    if let (Some(p), Some(stat)) = (parse_progress(line), engines::aria2_stat(line)) {
        return Some(Msg::TorrentStatus(id, p, stat));
    }
    if line.contains("Allocating disk space") {
        return Some(Msg::TorrentLog(id, "Выделение места на диске…".into()));
    }
    let lower = line.to_ascii_lowercase();
    if ["error", "warning", "failed", "aborted", "exception"]
        .iter()
        .any(|hint| lower.contains(hint))
    {
        return Some(Msg::ErrLine(id, line.to_string()));
    }
    None
}

fn local_torrent_path(path: &Path) -> Result<String, String> {
    let supported = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["torrent", "metalink", "meta4"]
                .iter()
                .any(|allowed| ext.eq_ignore_ascii_case(allowed))
        });
    if !supported {
        return Err("Выберите локальный .torrent, .metalink или .meta4 файл.".into());
    }
    // Validation rejects UNC before touching the filesystem; spaces and
    // brackets in a local filename remain ordinary path characters.
    validate_url(&path.to_string_lossy())
}

fn apply_initial_discovery(current: &mut Toolchain, setup_started: bool, discovered: Toolchain) {
    if !setup_started {
        *current = discovered;
    }
}

struct SnatchApp {
    url: String,
    engine_mode: EngineMode,
    fmt: String,
    out_dir: String,
    use_cookies: bool,
    cookies_browser: String,
    /// yt-dlp extras; see `extras_from_fields` for how they reach build().
    subs: bool,
    sub_langs: String,
    playlist: bool,
    extra_ytdlp: String,
    extra_aria2: String,
    cfg: Config,
    tc: Toolchain,
    /// Startup discovery may finish after an installer run; its stale result
    /// must never replace the toolchain found by SetupDone.
    setup_started: bool,
    /// Last installer progress line (see `setup_progress_text`).
    setup_progress: Option<(String, u64, Option<u64>)>,
    phase: Phase,
    /// Currently running downloads, most recently started last. Up to
    /// MAX_CONCURRENT at once, matching the CLI's `-j` default.
    jobs: Vec<ActiveJob>,
    next_job_id: u64,
    next_pick_id: u64,
    torrent_pick: Option<TorrentPick>,
    /// A job that just finished with an auth-shaped failure, offering a
    /// cookies retry. Lives here (not in `jobs`, which only holds active
    /// ones) so the offer survives after the failed job is removed.
    retry_offer: Option<Job>,
    /// Jobs stopped via "пауза" - shown below the active ones with a
    /// "продолжить" button. Not in `jobs` (no process running) or `queue`
    /// (not waiting for a free slot on its own).
    paused: Vec<PausedJob>,
    /// App-level status line (setup results, "added to queue", the most
    /// recently finished job's outcome) - each job's own live progress text
    /// shows in its own row instead.
    status: String,
    status_kind: StatusKind,
    log: Vec<String>,
    show_log_window: bool,
    show_url_history: bool,
    show_dir_history: bool,
    browser: DirBrowser,
    drives: Vec<PathBuf>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Set when the user closed the window mid-download: the close is vetoed
    /// (CancelClose) until every job is actually killed, then drain() reissues
    /// Close - otherwise the process exits while loaders keep running.
    close_requested: bool,
    #[cfg(windows)]
    maximized: bool,
    /// Bumped on every push_log; the log window re-joins its text only when
    /// this differs from log_cache_rev instead of every frame.
    log_rev: u64,
    log_cache_rev: u64,
    log_cache: String,
    dark_mode: bool,
    /// Links added past MAX_CONCURRENT active jobs. FIFO, except an explicit
    /// cookies retry jumps the line (push_front) - see the retry button.
    queue: VecDeque<QueuedJob>,
}

/// CLI's `-j` defaults to 3 concurrent downloads; match it here so the two
/// frontends behave the same way out of the box.
const MAX_CONCURRENT: usize = 3;

/// "Terminal Native" theme (mockups/b-terminal.html, the approved direction):
/// monospace everywhere, thin 1px hairlines instead of thick borders, ghost
/// buttons (border only, no fill), and one accent color used sparingly.
///
/// The mockup's stack (`ui-monospace,"Cascadia Code","Consolas",monospace`)
/// resolves to Cascadia Mono in the browser, and ClearType stem-snaps its
/// Regular stems to ~1px, which is the thin look that got approved. egui has
/// no hinting/subpixel AA: the same Regular (wght 400, the default instance
/// of the variable ttf) rasterizes with 2px blurry stems and reads bold next
/// to the HTML. Cascadia Mono is a variable font (wght 200-700), so
/// rust/fonts/CascadiaMono-Light.ttf is the wght=300 instance baked to a
/// static ttf with fontTools - its thinner stems land at the mockup's visual
/// weight once egui's AA spreads them. Same family, same metrics, so nothing
/// else about the layout moves.
///
/// A system mono as the *only* font (Proportional and Monospace both) is what
/// caused the earlier "text sits above center" bug: its ascent/descent ratio
/// differs from the bundled font egui's centering math assumes. Fixed via
/// `FontTweak::y_offset_factor` instead of giving up on a monospace UI - it
/// nudges glyph *rendering* down without touching the row-height egui uses
/// to center the widget, so layout stays correct. MONO_Y_OFFSET is a guess
/// (a fraction of the font size); if text still reads high/low after
/// building, adjust it up/down and rebuild - nothing else needs to change.
const MONO_Y_OFFSET: f32 = 0.15;

// SIL OFL 1.1 (c) Microsoft Corporation, see fonts/OFL-notice.txt
const BUNDLED_MONO: &[u8] = include_bytes!("../../fonts/CascadiaMono-Light.ttf");

fn setup_theme(ctx: &egui::Context, dark: bool) {
    let mut fonts = egui::FontDefinitions::default();
    let tweak = egui::FontTweak {
        y_offset_factor: MONO_Y_OFFSET,
        ..Default::default()
    };
    let data = egui::FontData::from_static(BUNDLED_MONO).tweak(tweak);
    fonts.font_data.insert("mono-system".to_owned(), data);
    for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.insert(0, "mono-system".to_owned());
        }
    }
    // Buttons and title bars need a higher baseline than the body text. Keep
    // identical font metrics; only shift painted glyphs so borders and input
    // rows remain aligned while popup captions clear the separator below.
    fonts.font_data.insert(
        "mono-button".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO).tweak(egui::FontTweak {
            y_offset_factor: 0.0,
            ..Default::default()
        }),
    );
    fonts.families.insert(
        egui::FontFamily::Name("button".into()),
        vec!["mono-button".to_owned()],
    );
    // TextEdit's inner text: the boxed controls' no-nudge rendering. Body's
    // +0.15em glyph shift (below) reads fine in free-standing labels, but
    // inside an input's tight margins it sits visibly below the box centre;
    // boxed controls should also share one baseline with each other.
    fonts.font_data.insert(
        "mono-field".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO).tweak(egui::FontTweak {
            y_offset_factor: 0.0,
            ..Default::default()
        }),
    );
    fonts.families.insert(
        egui::FontFamily::Name("field".into()),
        vec!["mono-field".to_owned()],
    );
    fonts.font_data.insert(
        "mono-title".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO).tweak(egui::FontTweak {
            y_offset_factor: -0.08,
            ..Default::default()
        }),
    );
    fonts.families.insert(
        egui::FontFamily::Name("title".into()),
        vec!["mono-title".to_owned()],
    );
    ctx.set_fonts(fonts);
    // Visuals must go into BOTH style buckets too (see the all_styles_mut
    // comment below): ctx.set_visuals writes only the currently active
    // bucket, which at startup is egui's Dark fallback, while the first
    // rendered frame may already use the Light bucket on a light-themed OS.
    ctx.all_styles_mut(|style| {
        style.visuals = terminal_visuals(dark);
    });
    // item_spacing.y is the *tight* rhythm (label to its own field, mockup's
    // ~6-7px); the *loose* rhythm between field groups (mockup's ~16-18px)
    // is added explicitly via ui.add_space() between groups - one uniform
    // spacing value can't express both.
    //
    // all_styles_mut, not style_mut: egui 0.29 keeps a *separate* Style per
    // Theme (dark_style/light_style) and both style_mut/set_visuals act on
    // whichever one ctx.theme() currently resolves to - which tracks the OS
    // preference independently of our own `dark` toggle. setup_theme() only
    // runs once at startup, so if ctx.theme() later settles on the other
    // bucket (e.g. once eframe reports the real OS theme a frame or two in),
    // this spacing config would silently apply to a style nothing renders
    // with - that's what left the scroll bar on its unstyled default despite
    // the fix below. Writing both buckets up front sidesteps the whole
    // class of bug regardless of which one ends up active.
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        // TextEdit's 9px vertical margin made its row ~6px taller than the
        // neighbouring buttons. Raise their vertical padding to match.
        style.spacing.button_padding = egui::vec2(12.0, 10.0);
        style.spacing.interact_size.y = 28.0;
        // Title bar height = title row + window_margin.top/bottom (egui
        // window.rs), and the bar's bottom stroke is the line the title
        // glyphs were sitting on. Roomier vertical window margin pushes that
        // line away from the text in every popup; the horizontal part just
        // gives popup content some air.
        style.spacing.window_margin = egui::Margin::symmetric(10.0, 15.0);
        // Default egui scrollbars are a floating, translucent overlay that
        // pops up over content and hides when idle - fine for a generic app,
        // but it breaks this app's whole premise that nothing overlaps and
        // every element always reserves its own flat, bordered space. Use a
        // solid bar (own space, always opaque, square corners already come
        // from the widgets.* rounding=ZERO below) with a hairline-colored
        // handle (foreground_color -> fg_stroke) instead of bg_fill, which
        // here is the same color as the track and would be invisible.
        style.spacing.scroll = egui::style::ScrollStyle {
            foreground_color: true,
            ..egui::style::ScrollStyle::solid()
        };
        // egui's defaults (Body/Button 14, Small 10) are all a step above the
        // mockup, which is why every string in the app read larger than its
        // HTML twin: mockup sizes are input/select/checkbox 13, .btn 12.5,
        // label.tag/.hint 12, footer/banner 11.
        let mut ts = style.text_styles.clone();
        ts.insert(
            egui::TextStyle::Body,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Button,
            egui::FontId::new(12.5, egui::FontFamily::Name("button".into())),
        );
        ts.insert(
            egui::TextStyle::Small,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        );
        style.text_styles = ts;
    });
}

/// Same "Terminal Native" structure (square corners, hairline ghost buttons,
/// one accent used sparingly) in both modes - only the palette changes. The
/// dark accent/OK/error/warn hues are bright enough to pop on near-black but
/// read as washed-out and low-contrast as text/strokes on a light background,
/// so light mode uses deepened variants (values close to GitHub's light-theme
/// semantic colors, already accessibility-tuned) rather than reusing them.
fn terminal_visuals(dark: bool) -> egui::Visuals {
    let rgb = |r: u8, g: u8, b: u8| egui::Color32::from_rgb(r, g, b);
    let (bg, bg_lift, field, line, line_bright, text, text_dim, accent, warn, err) = if dark {
        (
            rgb(0x0b, 0x0b, 0x0e),
            rgb(0x0e, 0x0e, 0x11),
            rgb(0x11, 0x11, 0x14),
            rgb(0x1e, 0x1e, 0x24),
            rgb(0x33, 0x33, 0x3c),
            rgb(0xd6, 0xd6, 0xda),
            rgb(0x83, 0x83, 0x8c),
            rgb(0x59, 0xd6, 0x8c),
            rgb(0xe0, 0xb3, 0x4d),
            rgb(0xe0, 0x65, 0x5c),
        )
    } else {
        (
            rgb(0xf3, 0xf3, 0xf1),
            rgb(0xff, 0xff, 0xff),
            rgb(0xfb, 0xfb, 0xfa),
            rgb(0xda, 0xda, 0xd6),
            rgb(0xb5, 0xb5, 0xb0),
            rgb(0x1c, 0x1c, 0x1e),
            rgb(0x5c, 0x5c, 0x62),
            rgb(0x1a, 0x7f, 0x37),
            rgb(0x9a, 0x67, 0x00),
            rgb(0xcf, 0x22, 0x2e),
        )
    };

    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.window_rounding = egui::Rounding::ZERO;
    v.menu_rounding = egui::Rounding::ZERO;
    v.window_fill = bg_lift;
    v.window_stroke = egui::Stroke::new(1.0_f32, line);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.panel_fill = bg;
    v.extreme_bg_color = field; // TextEdit background - the one filled surface
    v.faint_bg_color = field;
    v.code_bg_color = field;
    v.hyperlink_color = accent;
    v.warn_fg_color = warn;
    v.error_fg_color = err;
    // Used for the text-selection highlight inside a TextEdit, and (via
    // interact_selectable) as a focus-outline color - not for "this choice
    // is picked" chrome, that's custom-drawn now (see `engine_choice`).
    v.selection.bg_fill =
        egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 45);
    v.selection.stroke = egui::Stroke::new(1.0_f32, accent);

    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = egui::Rounding::ZERO;
        // bg_fill backs checkboxes/sliders and must stay visible; weak_bg_fill
        // backs plain buttons and is what makes them "ghost" (border only).
        w.bg_fill = field;
    }
    v.widgets.noninteractive.weak_bg_fill = bg;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, line);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, text_dim);

    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, line);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, text_dim);

    v.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, line_bright);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, text);
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.weak_bg_fill = field;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, accent);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, text);
    v.widgets.active.expansion = 0.0;

    v.widgets.open.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, line_bright);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, text);

    v
}

/// The primary action in a view (Скачать, Выбрать эту папку): an outlined
/// accent "ghost" button rather than a filled block - matches the mockup's
/// CTA treatment and reads as *the* action without a heavy solid fill.
/// Takes the accent color explicitly (from `ui.visuals().hyperlink_color`)
/// since it has to work in both themes and builds the `Button` before it has
/// a `Ui` to read visuals from itself.
fn accent_button(accent: egui::Color32, text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(accent))
        .stroke(egui::Stroke::new(1.0_f32, accent))
        .fill(egui::Color32::TRANSPARENT)
}

/// `egui::Spinner` with a bounded repaint cadence. The stock widget calls
/// `Context::request_repaint()` ("because it is animated") on every paint,
/// which pins eframe/winit to the full refresh rate for as long as the
/// spinner is visible - minutes during torrent metadata resolution or the
/// Device Flow login wait. This draws the same arc (same size, color, 20
/// points, 240° sweep) but asks for the next frame ~66 ms out (~15 fps);
/// status updates and input still repaint immediately.
/// One line of installer feedback: asset, downloaded bytes and a percent
/// when the server declared a size. The old static "скачиваю…" gave no idea
/// whether to wait a minute or ten.
fn setup_progress_text(p: Option<&(String, u64, Option<u64>)>) -> String {
    match p {
        Some((label, done, Some(total))) if *total > 0 => format!(
            "{label}: {} из {} ({:.0}%)",
            torrent::human_size(*done),
            torrent::human_size(*total),
            *done as f64 * 100.0 / *total as f64,
        ),
        Some((label, done, _)) if *done > 0 => format!("{label}: {}", torrent::human_size(*done)),
        Some((label, _, _)) => format!("{label}: подготовка…"),
        None => "скачиваю yt-dlp, aria2c, ffmpeg и Deno…".to_string(),
    }
}

fn slow_spinner(ui: &mut egui::Ui) {
    let size = ui.style().spacing.interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::ProgressIndicator));
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.ctx().request_repaint_after(Duration::from_millis(66));
    let radius = (rect.height() / 2.0) - 2.0;
    let n_points = 20;
    let time = ui.input(|i| i.time);
    let start_angle = time * std::f64::consts::TAU;
    let end_angle = start_angle + 240f64.to_radians() * time.sin();
    let points: Vec<egui::Pos2> = (0..n_points)
        .map(|i| {
            let angle = egui::lerp(start_angle..=end_angle, i as f64 / n_points as f64);
            let (sin, cos) = angle.sin_cos();
            rect.center() + radius * egui::vec2(cos as f32, sin as f32)
        })
        .collect();
    let color = ui.visuals().strong_text_color();
    ui.painter()
        .add(egui::Shape::line(points, egui::Stroke::new(3.0_f32, color)));
}

/// A fixed-width bar slot and two fixed-height buttons. `allocate_ui` cannot
/// reserve the empty/spinner slot here: it advances by the *used* child width,
/// shifting both buttons left when progress is not known yet. The usual 10px
/// vertical button padding also makes an add_sized([…, 28]) button 35px tall;
/// the second button then drifts down relative to the first and to the bar.
fn job_controls(
    ui: &mut egui::Ui,
    progress: Option<f32>,
    fill: egui::Color32,
    paused: bool,
    waiting_text: &str,
    first: (&str, f32),
    second: (&str, f32),
) -> (bool, bool) {
    const HEIGHT: f32 = 28.0;
    ui.scope(|ui| {
        ui.spacing_mut().button_padding.y = 5.0;
        ui.horizontal(|ui| {
            let gap = ui.spacing().item_spacing.x;
            let bar_w = (ui.available_width() - first.1 - second.1 - 2.0 * gap).max(40.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, HEIGHT), egui::Sense::hover());
            let mut bar_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            if let Some(p) = progress {
                bar_ui.add(
                    egui::ProgressBar::new(p)
                        .rounding(egui::Rounding::ZERO)
                        .fill(fill)
                        .desired_width(bar_w)
                        .desired_height(HEIGHT),
                );
            } else if paused {
                bar_ui.add(
                    egui::ProgressBar::new(0.0)
                        .rounding(egui::Rounding::ZERO)
                        .fill(fill)
                        .desired_width(bar_w)
                        .desired_height(HEIGHT),
                );
            } else {
                bar_ui.horizontal(|ui| {
                    slow_spinner(ui);
                    ui.weak(waiting_text);
                });
            }
            if paused {
                ui.painter().rect_stroke(
                    rect,
                    egui::Rounding::ZERO,
                    egui::Stroke::new(1.0_f32, fill),
                );
            }
            (
                ui.add_sized([first.1, HEIGHT], egui::Button::new(first.0))
                    .clicked(),
                ui.add_sized([second.1, HEIGHT], egui::Button::new(second.0))
                    .clicked(),
            )
        })
        .inner
    })
    .inner
}

/// Flat "● label" / "○ label" choice, no box at all (not radio_value's
/// circle-in-a-different-style-from-everything-else, not selectable_value's
/// filled pill) - this is what mockups/b-terminal.html's `.choices` is.
fn engine_choice(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let dot = if selected { "●" } else { "○" };
    let accent = ui.visuals().hyperlink_color;
    let color = if selected {
        accent
    } else {
        ui.visuals().widgets.inactive.fg_stroke.color
    };
    // Mockup's `.choices` is 13px text with an 11px ○/● in `::before` - two
    // sizes in one label, so a single RichText can't express it; a LayoutJob
    // (Button accepts it via WidgetText) can.
    // Inactive dot is dimmer than its label in the mockup
    // (`::before{color:--text-mute}` vs `span{color:--text-dim}`).
    let dot_color = if selected {
        accent
    } else {
        ui.visuals().weak_text_color()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &format!("{dot} "),
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(11.0),
            color: dot_color,
            ..Default::default()
        },
    );
    job.append(
        label,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(13.0),
            color,
            ..Default::default()
        },
    );
    let text: egui::WidgetText = job.into();
    // `Button::fill()`/`stroke()` each silently force `frame = true` (it's
    // documented on the methods themselves) - calling either after
    // `.frame(false)` re-enables the frame it just turned off, which is what
    // was drawing a border here (bright accent on the focused choice via
    // the `active` visuals, near-invisible dark-gray on the other two). With
    // frame genuinely off, fill/stroke are never painted, so just don't set
    // them.
    let button = egui::Button::new(text).frame(false);
    ui.add(button)
}

/// "› label" on its own line, above its control - mockups/b-terminal.html's
/// `label.tag` (`display:block`, so label and field never share a row). The
/// first port kept several labels inline with their control instead
/// (`ui.label("Движок"); <choices on the same horizontal>`), which is
/// exactly the "old architecture wearing new colors" complaint.
fn tag_label(ui: &mut egui::Ui, text: &str) {
    // mockup's label.tag is 12px, a step under the fields it captions
    ui.label(
        egui::RichText::new(format!("› {text}"))
            .color(ui.visuals().weak_text_color())
            .size(12.0),
    );
}

/// `egui::Window::new(title)` renders the title with `TextStyle::Heading`
/// (18pt, unstyled) unless the title already carries its own font - a
/// plain `&str` doesn't, so every popup title rendered noticeably larger
/// and in the un-tweaked fallback font, standing out from the rest of the
/// monospace UI around it. Forcing FontId::monospace here matches everything
/// else instead of falling back to Heading's default.
fn window_title(text: &str) -> egui::RichText {
    // 15pt made the title taller than the title bar's own line box, so the
    // glyphs rode on top of the window's top stroke; 13 sits inside it. The
    // "title" family is the un-tweaked face (see setup_theme).
    egui::RichText::new(text).font(egui::FontId::new(
        13.0,
        egui::FontFamily::Name("title".into()),
    ))
}

/// Input-field text: body size, but rendered with the boxed controls' font
/// ("field" family, no glyph nudge - see setup_theme) so the text sits
/// centred inside the field's margins.
fn field_font() -> egui::FontSelection {
    egui::FontSelection::FontId(egui::FontId::new(
        13.0,
        egui::FontFamily::Name("field".into()),
    ))
}

fn clicked_outside(ctx: &egui::Context, rect: egui::Rect) -> bool {
    ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|pos| !rect.contains(pos))
    })
}

/// Dashed 1px rule across the available width - mockups/b-terminal.html uses
/// this under the status line instead of a solid `ui.separator()`.
fn dashed_separator(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let y = rect.center().y;
    let (dash, gap) = (4.0, 3.0);
    let mut x = rect.left();
    while x < rect.right() {
        let x_end = (x + dash).min(rect.right());
        ui.painter().line_segment(
            [egui::pos2(x, y), egui::pos2(x_end, y)],
            egui::Stroke::new(1.0_f32, color),
        );
        x += dash + gap;
    }
}

/// Fractional OS scaling (125/150%) is where the design fell apart: egui
/// rasterized 13pt as 16.25 blurry physical px and every measurement taken
/// from the mockup grew by the scale factor, so the app never matched the
/// HTML side by side. Pinning pixels_per_point to a constant makes one
/// design px equal UI_SCALE physical px on any display, whatever the OS
/// reports. UI_SCALE is larger than 1.0 for readability. Viewport commands
/// take egui points and winit applies the effective scale itself; multiplying
/// those dimensions again would scale the window twice.
const UI_SCALE: f32 = 1.18;

fn pin_pixel_scale(ctx: &egui::Context, resize_viewport: bool) {
    if ctx.pixels_per_point() == UI_SCALE && !resize_viewport {
        return;
    }
    ctx.set_pixels_per_point(UI_SCALE);
    if resize_viewport {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(460.0, 720.0)));
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
        460.0, 600.0,
    )));
}

/// Drive letters from the GetLogicalDrives bitmask - defined letters only,
/// zero filesystem I/O (see new(): the exists() probe could hang startup on
/// dead network shares). Refreshed each time the folder browser opens, so a
/// USB stick plugged in mid-session shows up.
#[cfg(windows)]
fn list_drives() -> Vec<PathBuf> {
    extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| (mask >> i) & 1 == 1)
        .map(|i| PathBuf::from(format!("{}:\\", (b'A' + i) as char)))
        .collect()
}

#[cfg(not(windows))]
fn list_drives() -> Vec<PathBuf> {
    Vec::new()
}

/// Drive letter (`C` etc.) of a rooted local path, uppercased for
/// comparison. `None` for UNC/device/relative paths - those aren't letters
/// `list_drives()` can ever report, so callers treat them as unknown roots.
#[cfg(windows)]
fn path_drive_letter(p: &Path) -> Option<u8> {
    match p.components().next()? {
        std::path::Component::Prefix(pre) => match pre.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => {
                Some(letter.to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Turns the GUI's option fields into the argv extras build() understands.
/// A bad quote is reported instead of silently running with different flags.
fn extras_from_fields(
    subs: bool,
    sub_langs: &str,
    playlist: bool,
    ytdlp: &str,
    aria2: &str,
) -> Result<engines::RunExtras, String> {
    Ok(engines::RunExtras {
        subs,
        sub_langs: sub_langs.to_string(),
        playlist,
        extra_ytdlp: engines::split_cli_args(ytdlp)?,
        extra_aria2: engines::split_cli_args(aria2)?,
    })
}

impl SnatchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cfg, warning) = match Config::try_load() {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(e)),
        };
        setup_theme(&cc.egui_ctx, cfg.dark_mode);
        pin_pixel_scale(&cc.egui_ctx, true);
        let (tx, rx) = channel();
        // Bitmask query, no filesystem I/O: the old per-letter exists() probe
        // blocked startup for seconds when a mapped network drive was dead
        // (SMB redirector timeouts), with the window already created but not
        // yet painting.
        // Toolchain::discover() stats every PATH entry and scans the WinGet
        // package dirs - same dead-network-share stall risk, so it runs off
        // the UI thread; the CTA stays disabled until ToolsReady arrives
        // (it requires a tool anyway).
        let tc_tx = tx.clone();
        let tc_ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            let tc = Toolchain::discover();
            let _ = tc_tx.send(Msg::ToolsReady(tc));
            tc_ctx.request_repaint();
        });
        let mut app = Self::from_config(cfg, tx, rx);
        if let Some(warning) = warning {
            app.push_log(format!("⚠ {warning}; файл настроек сохранён без изменений"));
            app.set_status(
                StatusKind::Warn,
                "Не удалось прочитать настройки — см. журнал",
            );
            app.show_log_window = true;
        }
        app
    }

    fn from_config(cfg: Config, tx: Sender<Msg>, rx: Receiver<Msg>) -> Self {
        let out_dir = if !cfg.last_dir.is_empty() {
            cfg.last_dir.clone()
        } else {
            cfg.default_dir.clone()
        };
        let drives = list_drives();
        let dark_mode = cfg.dark_mode;
        Self {
            url: String::new(),
            engine_mode: EngineMode::Auto,
            fmt: "best".to_string(),
            out_dir,
            use_cookies: false,
            cookies_browser: "chrome".to_string(),
            subs: false,
            sub_langs: "ru,en".to_string(),
            playlist: false,
            extra_ytdlp: String::new(),
            extra_aria2: String::new(),
            cfg,
            tc: Toolchain {
                yt_dlp: None,
                aria2c: None,
            },
            setup_started: false,
            setup_progress: None,
            phase: Phase::Idle,
            jobs: Vec::new(),
            next_job_id: 0,
            next_pick_id: 0,
            torrent_pick: None,
            retry_offer: None,
            paused: Vec::new(),
            status: String::new(),
            status_kind: StatusKind::None,
            log: Vec::new(),
            show_log_window: false,
            show_url_history: false,
            show_dir_history: false,
            browser: DirBrowser {
                open: false,
                current: PathBuf::new(),
                entries: Vec::new(),
                truncated: false,
                listed_for: None,
                in_flight: None,
                resolve_initial: false,
                error: None,
            },
            drives,
            tx,
            rx,
            close_requested: false,
            #[cfg(windows)]
            maximized: false,
            log_rev: 0,
            log_cache_rev: 0,
            log_cache: String::new(),
            dark_mode,
            queue: VecDeque::new(),
        }
    }

    /// Flips the theme, applies it immediately and persists the choice so it
    /// doesn't reset to dark on the next launch.
    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.dark_mode = !self.dark_mode;
        // Both buckets: whichever one egui's theme() resolves to later must
        // carry the chosen palette, not just the active one right now.
        ctx.all_styles_mut(|style| {
            style.visuals = terminal_visuals(self.dark_mode);
        });
        self.cfg.dark_mode = self.dark_mode;
        if !self.save_config() {
            self.set_status(StatusKind::Warn, "Настройки темы не сохранены — см. журнал");
        }
    }

    fn save_config(&mut self) -> bool {
        match self.cfg.try_save_now() {
            Ok(()) => true,
            Err(e) => {
                self.push_log(format!("⚠ Не удалось сохранить настройки: {e}"));
                self.show_log_window = true;
                false
            }
        }
    }

    fn effective_engine(&self) -> &'static str {
        match self.engine_mode {
            EngineMode::Auto => detect_engine(&self.url),
            EngineMode::YtDlp => "yt-dlp",
            EngineMode::Aria2 => "aria2",
        }
    }

    fn set_torrent_file(&mut self, path: &Path) {
        match local_torrent_path(path) {
            Ok(url) => {
                self.url = url;
                self.engine_mode = EngineMode::Auto;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                self.set_status(StatusKind::Ok, format!("Выбран файл: {}", clip(&name, 64)));
            }
            Err(e) => self.set_status(StatusKind::Err, e),
        }
    }

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > MAX_LOG_LINES {
            let extra = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..extra);
        }
        self.log_rev += 1;
    }

    fn clear_log(&mut self) {
        self.log.clear();
        self.log_rev += 1;
    }

    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_kind = kind;
        self.status = text.into();
    }

    fn job_mut(&mut self, id: u64) -> Option<&mut ActiveJob> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    /// Prefixes a job's line with its label so concurrent jobs stay
    /// distinguishable in the one shared log window. Falls back to an
    /// unprefixed line if the job already finished (id no longer in `jobs`)
    /// by the time this drains - can happen for the last couple of lines a
    /// reader thread queues right as Done races in behind them.
    fn push_job_log(&mut self, id: u64, line: String) {
        match self.job_mut(id).map(|j| j.label.clone()) {
            Some(label) if !label.is_empty() => self.push_log(format!("[{label}] {line}")),
            _ => self.push_log(line),
        }
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Log(id, line) => {
                    let trimmed = line.trim_end().to_string();
                    // Exact yt-dlp prefix, not a substring search anywhere in
                    // the line: an uploader-chosen title containing
                    // "Resuming download" (arriving inside a Destination line)
                    // used to arm the automatic from-scratch retry for any
                    // subsequent failure of that download.
                    if trimmed.starts_with("[download] Resuming download") {
                        if let Some(j) = self.job_mut(id) {
                            j.resumed_seen = true;
                        }
                    }
                    if let Some(title) = engines::ytdlp_title_from_line(&trimmed) {
                        if let Some(j) = self.job_mut(id) {
                            j.label = title;
                        }
                    }
                    if !trimmed.is_empty() {
                        // yt-dlp's own speed/ETA estimator occasionally comes
                        // up empty for one tick - especially under several
                        // concurrent downloads competing for I/O - and prints
                        // that tick as literally "Unknown B/s ETA Unknown"
                        // (confirmed in the CLI too, so this is yt-dlp's own
                        // output, not something this app generates or can
                        // fix). Just not updating the status line for that
                        // one tick keeps the last real speed/ETA on screen
                        // instead of flashing "Unknown" every second or two;
                        // the percentage keeps moving regardless (Progress is
                        // sent unconditionally, separately from this line).
                        if !trimmed.contains("Unknown B/s") {
                            if let Some(j) = self.job_mut(id) {
                                j.set_status(StatusKind::None, clip(&trimmed, 96));
                            }
                        }
                        self.push_job_log(id, trimmed);
                    }
                }
                Msg::ErrLine(id, line) => {
                    if looks_like_auth(&line) {
                        if let Some(j) = self.job_mut(id) {
                            j.auth_seen = true;
                        }
                    }
                    let trimmed = line.trim_end().to_string();
                    if engines::aria2_missing_control(&trimmed) {
                        let already = self.job_mut(id).is_some_and(|j| j.missing_control_file);
                        if !already {
                            if let Some(j) = self.job_mut(id) {
                                j.missing_control_file = true;
                            }
                            self.push_job_log(id, engines::ARIA2_MISSING_CONTROL_HINT.to_owned());
                        }
                    } else if !trimmed.is_empty() {
                        let aria2_exception = trimmed.contains("Exception caught")
                            && self.job_mut(id).is_some_and(|j| j.job.engine == "aria2");
                        if !aria2_exception {
                            self.push_job_log(id, trimmed);
                        }
                    }
                }
                Msg::Progress(id, p) => {
                    if let Some(j) = self.job_mut(id) {
                        j.progress = Some(p);
                    }
                }
                Msg::DirListed(requested, resolved, result) => {
                    self.browser.in_flight = None;
                    if self.browser.open && self.browser.current == requested {
                        self.browser.current = resolved.clone();
                        self.browser.listed_for = Some(resolved);
                        match result {
                            Ok((entries, truncated)) => {
                                self.browser.entries = entries;
                                self.browser.truncated = truncated;
                                self.browser.error = None;
                            }
                            Err(e) => {
                                self.browser.entries.clear();
                                self.browser.error = Some(e);
                            }
                        }
                    }
                }
                Msg::TorrentStatus(id, p, status) => {
                    if let Some(j) = self.job_mut(id) {
                        j.progress = Some(p);
                        j.set_status(StatusKind::None, status);
                    }
                }
                Msg::TorrentFiles(id, result) => {
                    let is_current = self.torrent_pick.as_ref().is_some_and(|p| p.id == id);
                    if is_current {
                        match result {
                            Ok((listing, tree)) => {
                                let single =
                                    listing.info.files.len() <= 1 && self.phase != Phase::Setup;
                                if let Some(pick) = self.torrent_pick.as_mut() {
                                    pick.selected = vec![true; listing.info.files.len()];
                                    pick.tree = tree;
                                    pick.view = torrent::TreeView::default();
                                    pick.info = Some(listing.info);
                                    pick.meta = listing.meta;
                                }
                                if single {
                                    self.finish_torrent_pick(ctx, None);
                                }
                            }
                            Err(e) => {
                                // The modal shows why and lets the user pick
                                // "скачать всё" or cancel - no silent switch.
                                if let Some(pick) = self.torrent_pick.as_mut() {
                                    pick.error = Some(e);
                                }
                            }
                        }
                    }
                }
                Msg::TorrentMeta(id, name) => {
                    if let Some(j) = self.job_mut(id) {
                        j.progress = None;
                    }
                    if !name.is_empty() {
                        self.push_job_log(id, format!("Торрент: {name}"));
                        if let Some(j) = self.job_mut(id) {
                            j.label = name;
                        }
                    }
                    if let Some(j) = self.job_mut(id) {
                        j.set_status(StatusKind::None, "Получаю метаданные торрента…");
                    }
                }
                Msg::TorrentName(id, name) => {
                    if let Some(j) = self.job_mut(id) {
                        j.label = name;
                    }
                }
                Msg::TorrentLog(id, line) => self.push_job_log(id, line),
                Msg::Done(id, code) => self.finish_job(id, code, ctx),
                Msg::SetupLog(line) => self.push_log(line),
                Msg::SetupProgress(label, done, total) => {
                    self.setup_progress = Some((label, done, total));
                }
                Msg::ToolsReady(tc) => {
                    apply_initial_discovery(&mut self.tc, self.setup_started, tc)
                }
                Msg::SetupDone(ok, tc) => {
                    self.setup_progress = None;
                    self.tc = tc;
                    // Only leave Setup from Setup: an unconditional Idle here
                    // could re-enable the CTA on top of an already-running
                    // job (the retry button used to make that reachable).
                    if self.phase == Phase::Setup {
                        self.phase = Phase::Idle;
                    }
                    if ok {
                        self.set_status(StatusKind::Ok, "Загрузчики установлены");
                    } else {
                        self.set_status(StatusKind::Err, "Не всё удалось установить — см. журнал");
                        self.show_log_window = true;
                    }
                    self.fill_free_slots(ctx);
                }
            }
        }
        if self.phase == Phase::Setup || !self.jobs.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        // The window close was vetoed while jobs were being killed; now that
        // everything's actually finished, close for real.
        if self.close_requested && self.phase == Phase::Idle && self.jobs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// One specific job (by id) finished - success, failure or cancelled.
    /// Concurrent siblings in `self.jobs` are untouched.
    fn finish_job(&mut self, id: u64, code: i32, ctx: &egui::Context) {
        let Some(pos) = self.jobs.iter().position(|j| j.id == id) else {
            // Already removed (e.g. a duplicate Done somehow) - nothing to do.
            return;
        };
        let aj = self.jobs.remove(pos);
        let label = clip(&aj.label, 60);

        // Cancel that lost the race with a completed download still gets the
        // "Готово" branch: the file IS complete, claiming "отменено, можно
        // докачать" would send the user to re-download a finished file.
        if aj.cancelled && code != 0 {
            let text = format!("«{label}» отменено (файл можно докачать)");
            self.set_status(StatusKind::Warn, text);
            self.fill_free_slots(ctx);
            return;
        }
        // Same "still finished on its own" race as cancel above: if it hit
        // code 0 right as pause was requested, treat it as done, not paused.
        if aj.pausing && code != 0 {
            self.set_status(StatusKind::None, format!("«{label}» на паузе"));
            self.paused.push(PausedJob {
                job: aj.job.clone(),
                label,
                progress: aj.progress,
                status: aj.status,
                select: aj.select,
                no_continue: false,
            });
            self.fill_free_slots(ctx);
            return;
        }
        if code == 13 && aj.missing_control_file {
            self.set_status(
                StatusKind::Err,
                format!("«{label}»: файлы уже есть без .aria2 — выберите новую пустую папку"),
            );
            self.show_log_window = true;
            self.fill_free_slots(ctx);
            return;
        }
        match code {
            0 => {
                self.cfg.remember_url(&aj.job.url);
                self.cfg.remember_dir(&aj.job.out_dir.to_string_lossy());
                let persisted = self.save_config();
                let mut text = format!("«{label}» готово: {}", aj.job.out_dir.display());
                if aj.select.files.is_some() {
                    // aria2 writes whole pieces: unselected neighbours of the
                    // chosen files can exist yet be incomplete.
                    text.push_str(" · соседние невыбранные файлы могут быть неполными");
                }
                if !persisted {
                    text.push_str(" · история не сохранена — см. журнал");
                }
                self.set_status(
                    if persisted {
                        StatusKind::Ok
                    } else {
                        StatusKind::Warn
                    },
                    text,
                );
            }
            127 => self.set_status(
                StatusKind::Err,
                format!(
                    "«{label}»: не удалось запустить загрузчик (бинарник пропал или не исполняем)"
                ),
            ),
            130 => self.set_status(
                StatusKind::Warn,
                format!("«{label}» прервано (файл можно докачать)"),
            ),
            c => {
                self.set_status(
                    StatusKind::Err,
                    format!("«{label}»: ошибка (код {c}) — см. журнал"),
                );
                self.show_log_window = true;
                let offer_retry = c == 1 && aj.auth_seen && aj.job.engine == "yt-dlp";
                // The run resumed a .part and still failed: the old part is
                // unusable (stale range / corrupt bytes), so retry the same
                // job once from scratch instead of making the user click.
                let retry_from_scratch =
                    aj.resumed_seen && !aj.no_continue && !offer_retry && aj.job.engine == "yt-dlp";
                if offer_retry {
                    // Only the most recent auth failure offers a retry - a
                    // stacked "3 jobs want cookies" list would need its own
                    // per-job UI for one-shot, rarely-needed friction.
                    self.retry_offer = Some(aj.job.clone());
                } else if retry_from_scratch {
                    if self.spawn_job(ctx, aj.job.clone(), true, aj.select.clone()) {
                        self.set_status(
                            StatusKind::Warn,
                            format!("«{label}»: докачка дала битый файл — повторяю с начала"),
                        );
                    } else {
                        self.retain_failed_start(aj.job, aj.select, true);
                    }
                }
            }
        }
        self.fill_free_slots(ctx);
    }

    fn retain_failed_start(&mut self, job: Job, select: torrent::Choice, no_continue: bool) {
        self.paused.push(PausedJob {
            label: engines::batch_name(&job.url),
            job,
            progress: None,
            status: self.status.clone(),
            select,
            no_continue,
        });
    }

    fn retire_offers(&mut self, url: &str) {
        if self.retry_offer.as_ref().is_some_and(|job| job.url == url) {
            self.retry_offer = None;
        }
    }

    fn retry_with_cookies(&mut self, ctx: &egui::Context) {
        let Some(mut job) = self.retry_offer.clone() else {
            return;
        };
        job.cookies_browser = Some(self.cookies_browser.clone());
        if self.dispatch_job(ctx, job, torrent::Choice::default()) {
            self.retry_offer = None;
        }
    }

    /// Starts queued jobs until MAX_CONCURRENT is reached or the queue runs
    /// out. Called after any job finishes, freeing a slot.
    fn fill_free_slots(&mut self, ctx: &egui::Context) {
        if self.close_requested || self.phase == Phase::Setup {
            return;
        }
        while self.jobs.len() < MAX_CONCURRENT {
            let Some(queued) = self.queue.pop_front() else {
                break;
            };
            if !self.spawn_job(
                ctx,
                queued.job.clone(),
                queued.no_continue,
                queued.select.clone(),
            ) {
                self.retain_failed_start(queued.job, queued.select, queued.no_continue);
            }
        }
    }

    /// The "▶ СКАЧАТЬ" / "+ В ОЧЕРЕДЬ" button always calls this. Starts right
    /// away while there's a free concurrent slot (MAX_CONCURRENT, matching
    /// the CLI's `-j` default of 3 at once); once full, queues instead of
    /// locking the form - engine is auto-detected per link right now
    /// (whatever the current движок/формат/папка choice resolves to),
    /// matching the CLI's `-j` batch where each URL picks its own engine but
    /// shares format/folder.
    fn start_download(&mut self, ctx: &egui::Context) {
        if self.close_requested {
            self.set_status(StatusKind::Warn, "Приложение закрывается");
            return;
        }
        if self.phase == Phase::Setup {
            self.set_status(StatusKind::Err, "Дождитесь установки загрузчиков");
            return;
        }
        let engine = self.effective_engine().to_string();
        let url = match validate_url(&self.url) {
            Ok(u) => u,
            Err(e) => {
                self.set_status(StatusKind::Err, e);
                return;
            }
        };
        if self.out_dir.trim().is_empty() {
            self.set_status(StatusKind::Err, "Укажите папку сохранения");
            return;
        }
        let fmt = if engine == "yt-dlp" {
            self.fmt.clone()
        } else {
            "best".to_string()
        };
        let cookies = if !self.use_cookies {
            None
        } else {
            match engine.as_str() {
                "yt-dlp" => Some(self.cookies_browser.clone()),
                _ => None,
            }
        };
        if self.url_in_flight(&url) {
            self.set_status(
                StatusKind::Err,
                "Эта ссылка уже скачивается, в очереди или на паузе",
            );
            return;
        }
        let job = Job {
            engine,
            url,
            out_dir: PathBuf::from(&self.out_dir),
            fmt,
            cookies_browser: cookies,
        };
        // Torrents resolve their file list first: the user ticks files in a
        // modal before the actual download starts. URL and stale offers are
        // left untouched until the pick ends (start_picked / cancel).
        if job.engine == "aria2" && engines::is_bittorrent_input(&job.url) {
            // The modal disables the form, so this is only a safety net: a
            // second pick must never replace the open one and its job.
            if self.torrent_pick.is_some() {
                self.set_status(
                    StatusKind::Err,
                    "Сначала завершите выбор файлов открытого торрента",
                );
                return;
            }
            self.begin_torrent_pick(ctx, job);
            return;
        }
        // A failed-job offer is not an active job. If the user starts that
        // link manually, retire the stale offer; otherwise clicking its retry
        // button afterward could start a second writer on the same file.
        // Cleared once the job is valid and either starting or queued - not
        // on a validation error above, so a typo doesn't lose what's typed.
        let url = job.url.clone();
        if self.dispatch_job(ctx, job, torrent::Choice::default()) {
            self.retire_offers(&url);
            self.url.clear();
        }
    }

    /// Whether this link is already running, queued or paused.
    fn url_in_flight(&self, url: &str) -> bool {
        self.jobs.iter().any(|j| j.job.url == url)
            || self.queue.iter().any(|j| j.job.url == url)
            || self.paused.iter().any(|p| p.job.url == url)
            || self.torrent_pick.as_ref().is_some_and(|p| p.job.url == url)
    }

    /// Final step of a torrent pick: start (or queue) the job with what the
    /// user chose. The pick can stay open for minutes, so duplicates are
    /// checked again here, and only now are the URL field and stale offers
    /// for this link consumed. Returns whether the job is running or queued.
    fn start_picked(&mut self, ctx: &egui::Context, job: Job, choice: torrent::Choice) -> bool {
        if self.url_in_flight(&job.url) {
            self.set_status(
                StatusKind::Err,
                "Эта ссылка уже скачивается, в очереди или на паузе",
            );
            return false;
        }
        // Before dispatch: a queued or failed start overrides it.
        self.set_status(StatusKind::None, "Скачиваю торрент…");
        let url = job.url.clone();
        let started = self.dispatch_job(ctx, job, choice);
        if started {
            self.retire_offers(&url);
            if self.url.trim() == url {
                self.url.clear();
            }
        }
        started
    }

    /// Retain the fetched metadata and selection when a lock/spawn refuses
    /// this attempt, so retrying the modal never repeats a slow magnet fetch.
    fn finish_torrent_pick(&mut self, ctx: &egui::Context, files: Option<Vec<usize>>) {
        if let Some(mut pick) = self.torrent_pick.take() {
            let choice = torrent::Choice {
                files,
                meta: pick.meta.clone(),
            };
            if !self.start_picked(ctx, pick.job.clone(), choice) {
                pick.hint = Some(self.status.clone());
                self.torrent_pick = Some(pick);
            }
        }
    }

    /// Closes the pick modal without starting anything. A listing still in
    /// flight is told to stop, which kills its aria2c right away.
    fn cancel_torrent_pick(&mut self) -> Option<Job> {
        let pick = self.torrent_pick.take()?;
        pick.cancel.store(true, Ordering::Relaxed);
        Some(pick.job)
    }

    /// Queue-or-start for jobs that are ready to run. Returns whether the job
    /// is now running or queued (false: spawn_job refused it).
    fn dispatch_job(&mut self, ctx: &egui::Context, job: Job, select: torrent::Choice) -> bool {
        if self.close_requested {
            self.set_status(StatusKind::Warn, "Приложение закрывается");
            return false;
        }
        if self.url_in_flight(&job.url) {
            self.set_status(
                StatusKind::Warn,
                "Эта ссылка уже скачивается, ожидает или находится в выборе файлов",
            );
            return false;
        }
        if self.jobs.len() < MAX_CONCURRENT {
            self.spawn_job(ctx, job, false, select)
        } else {
            self.queue.push_back(QueuedJob {
                job,
                select,
                no_continue: false,
            });
            self.set_status(
                StatusKind::None,
                format!("Добавлено в очередь ({})", self.queue.len()),
            );
            true
        }
    }

    /// Fetch the torrent file list off-thread, then show the pick modal.
    fn begin_torrent_pick(&mut self, ctx: &egui::Context, job: Job) {
        if self.close_requested || self.phase == Phase::Setup {
            self.set_status(
                StatusKind::Warn,
                "Новая загрузка недоступна во время закрытия или установки",
            );
            return;
        }
        self.browser.open = false;
        self.show_dir_history = false;
        self.show_url_history = false;
        let id = self.next_pick_id;
        self.next_pick_id += 1;
        let aria2c = self.tc.aria2c.clone();
        let url = job.url.clone();
        let tx = self.tx.clone();
        let ctx_bg = ctx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_bg = cancel.clone();
        std::thread::spawn(move || {
            let result = match aria2c {
                Some(path) => {
                    torrent::show_files(&path, &url, torrent::METADATA_TIMEOUT, &cancel_bg).map(
                        |listing| {
                            let tree = torrent::build_torrent_tree(&listing.info.files);
                            (listing, tree)
                        },
                    )
                }
                None => Err("aria2c не найден — установите загрузчики".to_string()),
            };
            let _ = tx.send(Msg::TorrentFiles(id, result));
            ctx_bg.request_repaint();
        });
        self.torrent_pick = Some(TorrentPick {
            id,
            job,
            cancel,
            info: None,
            meta: None,
            selected: Vec::new(),
            tree: Vec::new(),
            view: torrent::TreeView::default(),
            error: None,
            hint: None,
        });
        self.set_status(StatusKind::None, "Получаю список файлов торрента…");
    }

    /// Spawns the loader for an already-validated job as a new concurrent
    /// entry in `self.jobs`. `no_continue` retries a resumed yt-dlp download
    /// from scratch: a stale/corrupt .part can't be validated by yt-dlp and
    /// poisons the run (Errno 22 mid-download or a merger "Invalid data"
    /// afterwards), so one clean retry heals it. Returns whether the job is
    /// now running (false: refused, with the reason already in the status).
    fn spawn_job(
        &mut self,
        ctx: &egui::Context,
        job: Job,
        no_continue: bool,
        select: torrent::Choice,
    ) -> bool {
        // The close flow vetoes the window close until `jobs` is empty. A
        // spawn racing in from a still-clickable button right after × would
        // add an uncancelled process past cancel_all() and defer closing
        // until that download finishes.
        if self.close_requested {
            self.set_status(
                StatusKind::Warn,
                "Приложение закрывается — загрузка не запущена",
            );
            return false;
        }
        if self.phase == Phase::Setup {
            self.set_status(StatusKind::Err, "Дождитесь установки загрузчиков");
            return false;
        }
        if self.url_in_flight(&job.url) {
            self.set_status(
                StatusKind::Warn,
                "Эта ссылка уже скачивается, ожидает или находится в выборе файлов",
            );
            return false;
        }
        let label = engines::batch_name(&job.url);
        let warns = preflight_warning(&job);

        let output_lock = match engines::lock_aria2_target(&job, false) {
            Ok(lock) => lock,
            Err(e) => {
                self.set_status(StatusKind::Err, format!("«{label}»: {e}"));
                return false;
            }
        };
        let extras = match extras_from_fields(
            self.subs,
            &self.sub_langs,
            self.playlist,
            &self.extra_ytdlp,
            &self.extra_aria2,
        ) {
            Ok(extras) => extras,
            Err(e) => {
                self.set_status(StatusKind::Err, format!("«{label}»: {e}"));
                return false;
            }
        };
        let mut cmd = match engines::build_for_run_with(&job, &self.tc, &select, &extras) {
            Ok(cmd) => cmd,
            Err(e) => {
                self.set_status(StatusKind::Err, format!("«{label}»: {e}"));
                return false;
            }
        };

        let insert_at = cmd.len().saturating_sub(2);
        if job.engine == "yt-dlp" {
            cmd.insert(insert_at, "--newline".into());
        } else {
            cmd.insert(insert_at, "--summary-interval=1".into());
        }
        if no_continue && job.engine == "yt-dlp" {
            cmd.insert(insert_at, "--no-continue".into());
        }

        let install_guard = match snatch_rs::tools::try_lock_for_spawn(Path::new(&cmd[0])) {
            Ok(guard) => guard,
            Err(e) => {
                self.set_status(StatusKind::Err, e);
                return false;
            }
        };
        let mut command = Command::new(&cmd[0]);
        command
            .args(&cmd[1..])
            // Best-effort nudge for a *non-frozen* yt-dlp (e.g. one run as
            // `python -m yt_dlp` via SNATCH_YT_DLP): when its stdout/stderr is
            // piped rather than a real console, Python falls back to the ANSI
            // codepage (cp1251 on a Cyrillic Windows). NOTE: the official
            // yt-dlp.exe is a PyInstaller-frozen exe that IGNORES both of
            // these, so it emits cp1251 regardless - the real safeguard is the
            // lossy byte-reader on the pipes below, which tolerates those
            // non-UTF-8 bytes instead of abandoning (and closing) the pipe.
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // yt-dlp/aria2c are console-subsystem exes; this GUI has no
            // console of its own (windows_subsystem = "windows" above), so
            // without this flag Windows allocates a brand new console
            // window for the child - the CMD flash on every download.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        engines::prepend_bootstrap_path(&mut command);
        let (mut child, proc_job) = match job_object::spawn(&mut command, true) {
            Ok(result) => result,
            Err(e) => {
                self.set_status(
                    StatusKind::Err,
                    format!("«{label}»: запуск/защита загрузчика: {e}"),
                );
                self.push_log(format!("✘ Загрузчик остановлен: {e}"));
                return false;
            }
        };
        // Put the loader into a kill-on-close job object before it has
        // spawned children of its own (ffmpeg, aria2c-as-external-downloader)
        // - see mod job_object for the two orphan classes this closes.
        // Named proc_job: `job` is the engines::Job parameter below.
        drop(install_guard);
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let id = self.next_job_id;
        self.next_job_id += 1;
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let aria2 = job.engine == "aria2";
        self.jobs.push(ActiveJob {
            id,
            job: job.clone(),
            label,
            progress: None,
            status: "Запускаю…".to_string(),
            status_kind: StatusKind::None,
            warns,
            cancel_flag: cancel_flag.clone(),
            cancelled: false,
            pausing: false,
            auth_seen: false,
            missing_control_file: false,
            resumed_seen: false,
            no_continue,
            select,
        });

        let tx = self.tx.clone();
        let ctx_w = ctx.clone();
        std::thread::spawn(move || {
            let tx_out = tx.clone();
            let ctx_out = ctx_w.clone();
            let h_out = std::thread::spawn(move || {
                // Read raw bytes and decode lossily - never `BufRead::lines()`.
                // The frozen yt-dlp.exe writes cp1251 (it ignores the
                // PYTHONIOENCODING/PYTHONUTF8 we set), so a title with an
                // em-dash reaches us as 0x97, which is invalid UTF-8. lines()
                // returns Err on it and the old `else { break }` dropped the
                // reader, closing the pipe; yt-dlp's next write then died with
                // `OSError: [Errno 22] Invalid argument` and aborted the
                // download. read_until + from_utf8_lossy keeps draining the
                // pipe no matter what bytes arrive.
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                // yt-dlp with --newline emits a progress line per downloaded
                // chunk - dozens per second on a fast link. Logging ALL of
                // them (a) evicted the meaningful lines ([info], Destination,
                // Merger, warnings) out of the 500-line cap within seconds of
                // a big download, and (b) invalidated the log-window text
                // every frame, forcing epaint to re-layout+re-tessellate the
                // whole 500-line galley on each new line. The progress BAR
                // still gets every tick (cheap, one widget); the text log and
                // status line get at most ~2 progress lines per second.
                // Non-progress lines always pass through immediately.
                let mut last_progress_log: Option<Instant> = None;
                let mut last_torrent_log: Option<Instant> = None;
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let decoded = engines::decode_child_bytes(&buf);
                    let line = engines::sanitize_child_output(&decoded);
                    if aria2 {
                        let mut latest = None;
                        for fragment in line.replace('\r', "\n").lines() {
                            if let Some(msg) = torrent_console_line(id, fragment) {
                                match msg {
                                    Msg::TorrentStatus(..) => latest = Some(msg),
                                    other => {
                                        if matches!(other, Msg::TorrentMeta(..)) {
                                            latest = None;
                                        }
                                        let _ = tx_out.send(other);
                                    }
                                }
                            }
                        }
                        if let Some(msg) = latest {
                            let due = last_progress_log
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(200));
                            if due {
                                if let Msg::TorrentStatus(_, _, ref stat) = msg {
                                    if last_torrent_log
                                        .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
                                    {
                                        last_torrent_log = Some(Instant::now());
                                        let _ = tx_out.send(Msg::TorrentLog(id, stat.clone()));
                                    }
                                }
                                last_progress_log = Some(Instant::now());
                                let _ = tx_out.send(msg);
                                ctx_out.request_repaint();
                            }
                        }
                        continue;
                    }
                    match parse_progress(&line) {
                        Some(p) => {
                            let _ = tx_out.send(Msg::Progress(id, p));
                            let due = last_progress_log
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(500));
                            if due {
                                last_progress_log = Some(Instant::now());
                                let _ = tx_out.send(Msg::Log(id, line.to_string()));
                                ctx_out.request_repaint();
                            }
                        }
                        None => {
                            let _ = tx_out.send(Msg::Log(id, line.to_string()));
                            ctx_out.request_repaint();
                        }
                    }
                }
            });
            let tx_err = tx.clone();
            let h_err = std::thread::spawn(move || {
                // Same lossy byte-read as stdout: stderr can carry the very
                // same non-UTF-8 cp1251 bytes inside error/warning text.
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let line = engines::sanitize_child_output(&engines::decode_child_bytes(&buf))
                        .into_owned();
                    let _ = tx_err.send(Msg::ErrLine(id, line));
                    // Running already schedules repaint every 150ms in drain;
                    // per-line requests here would redraw the entire window
                    // for every noisy warning from aria2c.
                }
            });

            let child = Arc::new(std::sync::Mutex::new(child));
            // Held (not used) until the thread ends: dropping the job closes
            // its last handle, and KILL_ON_JOB_CLOSE then reaps whatever of
            // the process tree is still alive (e.g. an ffmpeg left mid-merge
            // by a cancelled yt-dlp). If this process dies outright, the OS
            // closes the handle for us - same effect, no orphaned seeders.
            let proc_job = proc_job;
            let status = loop {
                if cancel_flag.load(Ordering::SeqCst) {
                    let mut c = child.lock().unwrap();
                    if let Some(j) = &proc_job {
                        j.terminate();
                    }
                    let _ = c.kill();
                    break c.wait();
                }
                match child.lock().unwrap().try_wait() {
                    Ok(Some(st)) => break Ok(st),
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(e) => break Err(e),
                }
            };
            let code = status.map(|s| s.code().unwrap_or(130)).unwrap_or(127);
            // F4 (audit 4): reap whatever is left of the tree BEFORE waiting
            // on the pipes. If the direct child died abnormally while a
            // grandchild (ffmpeg / external aria2c) still holds the inherited
            // handles, the joins below would block unbounded - and Done
            // already sent would have freed the GUI slot for a duplicate of
            // the same job. A normal exit leaves an empty tree, so this
            // terminate is a no-op there.
            if let Some(j) = &proc_job {
                j.terminate();
            }
            // aria2 has exited (and its tree is now stopped), so release its
            // basename before Done starts the next GUI queue entry for the
            // same destination.
            drop(output_lock);
            let _ = h_out.join();
            let _ = h_err.join();
            let _ = tx.send(Msg::Done(id, code));
            ctx_w.request_repaint();
        });
        true
    }

    fn start_setup(&mut self, ctx: &egui::Context) {
        self.setup_started = true;
        self.phase = Phase::Setup;
        self.setup_progress = None;
        // Keep refused jobs/offers available after installation; spawn_job's
        // Setup guard prevents the old overlapping-installer retry race.
        self.clear_log();
        self.set_status(StatusKind::None, "Устанавливаю загрузчики…");
        let tx = self.tx.clone();
        let ctx_w = ctx.clone();
        std::thread::spawn(move || {
            let tx_prog = tx.clone();
            let mut report = move |p: SetupProgress| {
                let _ = tx_prog.send(Msg::SetupProgress(p.label, p.done, p.total));
            };
            let bin = match bootstrap_dir() {
                Ok(bin) => bin,
                Err(e) => {
                    let _ = tx.send(Msg::SetupLog(e));
                    let _ = tx.send(Msg::SetupDone(false, Toolchain::discover()));
                    ctx_w.request_repaint();
                    return;
                }
            };
            let mut ok = std::fs::create_dir_all(&bin).is_ok();
            if ok {
                let _ = tx.send(Msg::SetupLog(format!(
                    "Устанавливаю загрузчики в {}",
                    bin.display()
                )));
                match install_yt_dlp_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("yt-dlp: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(format!("yt-dlp: ОШИБКА {e}")));
                        ok = false;
                    }
                }
                match install_aria2_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("aria2c: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(format!("aria2c: ОШИБКА {e}")));
                        ok = false;
                    }
                }
                match install_ffmpeg_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("ffmpeg: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(format!("ffmpeg: ОШИБКА {e}")));
                        ok = false;
                    }
                }
                match install_deno_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("Deno: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(format!("Deno: ОШИБКА {e}")));
                        ok = false;
                    }
                }
            } else {
                let _ = tx.send(Msg::SetupLog("Не удалось создать папку bin".to_string()));
            }
            // Re-discover in THIS thread: Toolchain::discover() walks PATH and
            // the WinGet dirs, and doing that on the UI thread froze the
            // window for as long as the scan took (dead network PATH entries:
            // seconds).
            let tc = Toolchain::discover();
            let _ = tx.send(Msg::SetupDone(ok, tc));
            ctx_w.request_repaint();
        });
    }

    /// Cancels exactly one active job - picking the wrong one among several
    /// running at once was the whole point of moving this off a single
    /// shared flag.
    fn cancel_job(&mut self, id: u64) {
        if let Some(j) = self.job_mut(id) {
            j.cancelled = true;
            j.cancel_flag.store(true, Ordering::SeqCst);
            let label = j.label.clone();
            self.set_status(
                StatusKind::Warn,
                format!("«{}»: останавливаю…", clip(&label, 60)),
            );
        }
    }

    /// Pauses exactly one active job: kills its process the same way cancel
    /// does, but finish_job() sees `pausing` and keeps the job around in
    /// `self.paused` (with a "продолжить" button) instead of dropping it -
    /// see the field doc on ActiveJob::pausing for why no extra IPC to the
    /// child is needed.
    fn pause_job(&mut self, id: u64) {
        if let Some(j) = self.job_mut(id) {
            j.pausing = true;
            j.cancel_flag.store(true, Ordering::SeqCst);
            let label = j.label.clone();
            self.set_status(
                StatusKind::None,
                format!("«{}»: ставлю на паузу…", clip(&label, 60)),
            );
        }
    }

    /// Cancels every active job - used when the whole app is closing, not
    /// from the per-job cancel button.
    fn cancel_all(&mut self) {
        for j in &mut self.jobs {
            j.cancelled = true;
            j.cancel_flag.store(true, Ordering::SeqCst);
        }
        self.set_status(StatusKind::Warn, "Останавливаю…");
    }

    fn found_tools_line(&self) -> String {
        let mut found = Vec::new();
        if self.tc.yt_dlp.is_some() {
            found.push("yt-dlp");
        }
        if self.tc.aria2c.is_some() {
            found.push("aria2c");
        }
        if found.is_empty() {
            String::new()
        } else {
            format!("$ найдены: {}", found.join(", "))
        }
    }

    #[cfg(windows)]
    fn ui_title_bar(&mut self, ctx: &egui::Context) {
        // Borderless Windows viewport: a roomier version of the HTML titlebar, flat,
        // with a thin line below. The draggable area excludes the window
        // buttons so a click there never accidentally moves the window.
        egui::TopBottomPanel::top("app_titlebar")
            .exact_height(42.0)
            .show_separator_line(true)
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(14.0, 0.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let title_width =
                        (ui.available_width() - 3.0 * 40.0 - THEME_BTN_WIDTH).max(0.0);
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
                    let label = if self.dark_mode {
                        "день"
                    } else {
                        "ночь"
                    };
                    if ui
                        .add_sized(
                            [THEME_BTN_WIDTH, 38.0],
                            egui::Button::new(label).frame(false),
                        )
                        .on_hover_text(if self.dark_mode {
                            "Светлая тема"
                        } else {
                            "Тёмная тема"
                        })
                        .clicked()
                    {
                        self.toggle_theme(ctx);
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("─").frame(false))
                        .on_hover_text("Свернуть")
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("□").frame(false))
                        .on_hover_text("Развернуть / восстановить")
                        .clicked()
                    {
                        self.maximized =
                            !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("×").frame(false))
                        .on_hover_text("Закрыть")
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
    fn ui_resize_edges(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized)) {
            return;
        }
        let screen = ctx.screen_rect();
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

    fn ui_url_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        tag_label(ui, "ссылка");
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
                    .on_hover_text("Последние ссылки")
                    .clicked()
                {
                    toggle_hist = true;
                }
                #[cfg(windows)]
                if ui
                    .add_enabled(self.phase != Phase::Setup, egui::Button::new("файл"))
                    .on_hover_text("Выбрать локальный .torrent-файл")
                    .clicked()
                {
                    pick_torrent = true;
                }
                let edit = egui::TextEdit::singleline(&mut self.url)
                    .font(field_font())
                    .desired_width(f32::INFINITY)
                    // Default TextEdit margin is (4,2) - the mockup's inputs
                    // use `padding: 9px 11px`, noticeably roomier.
                    .margin(egui::Margin::symmetric(11.0, 9.0))
                    .hint_text("https://… magnet:… или путь к .torrent");
                // Stays enabled while a job is Running (not just Idle): a
                // queue means the next link can be typed/pasted without
                // waiting for the active download to finish.
                ui.add_enabled(self.phase != Phase::Setup, edit);
            });
        });
        #[cfg(windows)]
        if pick_torrent {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Торренты и металинки", &["torrent", "metalink", "meta4"])
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
            let window = egui::Window::new(window_title("Последние ссылки"))
                .open(&mut open)
                .resizable(false)
                .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
                .show(ctx, |ui| {
                    if urls.is_empty() {
                        ui.weak("(история пуста — ссылки появятся после успешных скачиваний)");
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

    fn ui_engine_row(&mut self, ui: &mut egui::Ui) -> bool {
        // Not gated on Idle: a Running job doesn't own these fields, the next
        // queued link does, so движок/формат/папка must stay editable.
        let enabled = self.phase != Phase::Setup;
        let mut dir_history_opened = false;
        ui.add_space(6.0);
        tag_label(ui, "движок");
        ui.horizontal(|ui| {
            let buttons = ui.add_enabled_ui(enabled, |ui| {
                // Flat "● label" / "○ label" - see engine_choice's doc comment
                // for why this replaced both radio_value and selectable_value.
                if engine_choice(ui, "авто", self.engine_mode == EngineMode::Auto).clicked() {
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
                    let guess_label = guess;
                    // Two guesses at a font-tweak fix (the default label font,
                    // then the "button" family) each moved this the wrong way
                    // or overshot - the buttons' own vertical centering isn't
                    // something a font offset on a *different* widget can
                    // reliably match. Instead, read back the actual rendered
                    // rect of the button row and paint this text at that
                    // exact center - no font-metric guessing involved.
                    let color = ui.visuals().weak_text_color();
                    let font = egui::FontId::new(12.0, egui::FontFamily::Proportional);
                    let galley =
                        ui.painter()
                            .layout_no_wrap(format!("→ {guess_label}"), font, color);
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
        tag_label(ui, "формат");
        ui.add_enabled_ui(enabled && engine == "yt-dlp", |ui| {
            egui::ComboBox::from_id_salt("fmt")
                .selected_text(
                    FORMATS
                        .iter()
                        .find(|(k, _)| *k == self.fmt)
                        .map(|(_, l)| *l)
                        .unwrap_or("best"),
                )
                .show_ui(ui, |ui| {
                    for (key, label) in FORMATS {
                        ui.selectable_value(&mut self.fmt, key.to_string(), label);
                    }
                });
        });
        ui.add_space(6.0);
        tag_label(ui, "папка");
        ui.horizontal(|ui| {
            let mut open_browser = false;
            let mut toggle_hist = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Same as the url-history ▾: an empty history must not look
                // like a dead button.
                if ui
                    .add_enabled(enabled, egui::Button::new("▾"))
                    .on_hover_text("Последние папки")
                    .clicked()
                {
                    toggle_hist = true;
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("обзор"))
                    .on_hover_text("Выбрать папку для скачивания")
                    .clicked()
                {
                    open_browser = true;
                }
                ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut self.out_dir)
                        .font(field_font())
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(11.0, 9.0)),
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

    fn ui_dir_history(&mut self, ctx: &egui::Context, opened_this_frame: bool) {
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
        let window = egui::Window::new(window_title("Последние папки"))
            .open(&mut open)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 200.0])
            .show(ctx, |ui| {
                if dirs.is_empty() {
                    ui.weak("(история пуста — папки появятся после успешных скачиваний)");
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

    fn ui_log_window(&mut self, ctx: &egui::Context, opened_this_frame: bool) {
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
        let window = egui::Window::new(window_title("Журнал"))
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

    fn open_dir_browser(&mut self) {
        if self.close_requested || self.torrent_pick.is_some() {
            return;
        }
        // Cheap bitmask query - a drive plugged in since startup shows up.
        self.drives = list_drives();
        let typed = PathBuf::from(self.out_dir.trim());
        if is_unc_path(&typed) {
            self.set_status(
                StatusKind::Err,
                "Сетевую UNC-папку нельзя открывать. Выберите локальную папку.",
            );
            return;
        }
        // Probe only volumes the OS currently reports as logical drives. A
        // stale out_dir on a removed USB stick / disconnected mapping would
        // otherwise block the UI thread inside GetFileAttributes until the
        // device times out - the same stall class list_drives() avoids at
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

    fn ui_dir_browser(&mut self, ctx: &egui::Context) {
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
                if initial && !is_unc_path(&resolved) && !resolved.is_dir() {
                    if let Some(parent) =
                        resolved.parent().filter(|p| !is_unc_path(p) && p.is_dir())
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
        let drives = self.drives.clone();

        let window = egui::Window::new(window_title("Выбор папки"))
            .open(&mut still_open)
            .resizable(true)
            .default_size([480.0, 440.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Дом").clicked() {
                        navigate = Some(home.clone());
                    }
                    if ui.button("Рабочий стол").clicked() {
                        navigate = Some(home.join("Desktop"));
                    }
                    if ui.button("Загрузки").clicked() {
                        navigate = Some(home.join("Downloads"));
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
                            if ui.selectable_label(false, "↑ Наверх").clicked() {
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
                            slow_spinner(ui); ui.weak("Читаю каталог…");
                        } else if let Some(error) = &error {
                            ui.colored_label(ui.visuals().error_fg_color, format!("Не удалось прочитать каталог: {error}"));
                        } else if entries.is_empty() {
                            if truncated {
                                ui.weak(format!(
                                    "… среди первых {DIR_BROWSER_SCAN_CAP} записей вложенных папок нет"
                                ));
                            } else {
                                ui.weak("(нет вложенных папок)");
                            }
                        } else if truncated {
                            // The scan stopped at DIR_BROWSER_SCAN_CAP: the
                            // tail is simply not listed, no navigation
                            // reveals it - say so instead of pretending the
                            // list is complete.
                            ui.weak(format!(
                                "… каталог очень большой: из первых {DIR_BROWSER_SCAN_CAP} записей \
                                 показаны первые {shown} папок — выберите подпапку или откройте \
                                 папку заново"
                            ));
                        } else if entries.len() > shown {
                            // The old "поднимитесь выше" advice was a lie:
                            // the list is alphabetical and the tail is simply
                            // never rendered, no navigation reveals it.
                            ui.weak(format!(
                                "… показаны первые {shown} папок по алфавиту (всего {})",
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
                    if ui.add_enabled(!loading && error.is_none(), accent_button(accent, "Выбрать эту папку")).clicked() {
                        chosen = Some(cur.clone());
                    }
                    if ui.button("Отмена").clicked() {
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
            if !is_unc_path(&p) {
                self.browser.current = p;
            }
        }
        if let Some(p) = chosen {
            self.out_dir = p.to_string_lossy().into_owned();
            self.browser.open = false;
        }
    }

    fn ui_cookies_row(&mut self, ui: &mut egui::Ui) {
        let engine = self.effective_engine();
        ui.add_space(6.0);
        let yt = engine == "yt-dlp";
        ui.add_enabled_ui(self.phase != Phase::Setup && yt, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.use_cookies, "куки из браузера");
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
    fn ui_extras_row(&mut self, ui: &mut egui::Ui) {
        let enabled = self.phase != Phase::Setup;
        let engine = self.effective_engine();
        ui.add_space(6.0);
        tag_label(ui, "опции");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(enabled && engine == "yt-dlp", |ui| {
                ui.checkbox(&mut self.subs, "субтитры")
                    .on_hover_text("Скачивать субтитры (ручные и автоматические) рядом с видео");
                if self.subs {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.sub_langs)
                            .font(field_font())
                            .desired_width(70.0),
                    )
                    .on_hover_text("Языки субтитров, например ru,en");
                }
            });
            ui.add_enabled_ui(enabled, |ui| {
                ui.checkbox(&mut self.playlist, "плейлист")
                    .on_hover_text("Скачать плейлист целиком, а не одиночное видео");
            });
        });
        ui.add_enabled_ui(enabled, |ui| {
            ui.collapsing("доп. аргументы", |ui| {
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

    fn ui_run_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
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
            let waiting = if aj.status.starts_with("Получаю метаданные") {
                "Метаданные торрента…"
            } else {
                "Подключение…"
            };
            let (pause, cancel) = job_controls(
                ui,
                aj.progress,
                accent,
                false,
                waiting,
                ("пауза", ACTIVE_BTN_W),
                ("отмена", ACTIVE_BTN_W),
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
                ("продолжить", RESUME_BTN_W),
                ("убрать", REMOVE_BTN_W),
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
                self.set_status(StatusKind::Err, "Дождитесь установки загрузчиков");
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
                        self.set_status(StatusKind::None, format!("«{}»: продолжаю…", pj.label));
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
                        format!("Добавлено в очередь ({})", self.queue.len()),
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
                if self.status == format!("«{}» на паузе", pj.label) {
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
                ui.weak("получаю список файлов торрента…");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    cancel = ui.button("[ отмена ]").clicked();
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
            && match self.effective_engine() {
                // Enabling the CTA while only the *other* engine is installed
                // would build a command that fails and silently drops the link.
                "yt-dlp" => self.tc.yt_dlp.is_some(),
                "aria2" => self.tc.aria2c.is_some(),
                _ => false,
            };
        let width = ui.available_width();
        let accent = ui.visuals().hyperlink_color;
        // The mockup's source text is lowercase, but its CSS has
        // `.cta { text-transform: uppercase }` - the *rendered* (approved)
        // look is "▶ СКАЧАТЬ", not "▶ скачать". Mockup's `.cta` is
        // `font: 600 13px` - 16.0 here was never actually checked against it.
        let label = if self.jobs.len() < MAX_CONCURRENT {
            "▶ СКАЧАТЬ"
        } else {
            "+ В ОЧЕРЕДЬ"
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

    fn ui_queue_count(&self, ui: &mut egui::Ui) {
        if !self.queue.is_empty() {
            ui.add_space(4.0);
            ui.weak(format!("В очереди: {}", self.queue.len()));
        }
    }

    /// Torrent file-selection modal (list or error), shown once loading is
    /// done; the loading phase lives in the run row. Drawn in the Foreground
    /// order so update()'s dim overlay (Middle) stays beneath it.
    fn ui_torrent_pick(&mut self, ctx: &egui::Context) {
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
            pick.hint = Some("Дождитесь установки загрузчиков".into());
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
                pick.hint = Some("Выберите хотя бы один файл".into());
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
    fn abort_torrent_pick(&mut self) {
        if let Some(job) = self.cancel_torrent_pick() {
            if self.url.trim().is_empty() {
                self.url = job.url;
            }
        }
        self.set_status(StatusKind::None, "Выбор файлов отменён");
    }
}

impl eframe::App for SnatchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe re-applies the OS scale factor on DPI/monitor changes, so
        // the pin is re-asserted every frame, not just at startup.
        pin_pixel_scale(ctx, false);
        self.drain(ctx);
        let log_was_open = self.show_log_window;
        // A drop while the pick modal is open would swap the URL field from
        // under the torrent being picked.
        if self.phase != Phase::Setup && self.torrent_pick.is_none() {
            let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
            if let Some(path) = dropped {
                self.set_torrent_file(&path);
            }
        }
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

        #[cfg(windows)]
        self.ui_title_bar(ctx);

        // The mockup's footer is its own full-width band, with a hairline at
        // the top and 26px side padding. Keeping it outside the scrollable
        // form also prevents long status/warning text from overlapping it.
        egui::TopBottomPanel::bottom("app_footer")
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(26.0, 10.0)),
            )
            .show_separator_line(true)
            .show(ctx, |ui| {
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
                egui::Frame::none()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(26.0, 18.0)),
            )
            .show(ctx, |ui| {
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
            if ui.button(if self.dark_mode { "Светлая тема" } else { "Тёмная тема" }).clicked() {
                self.toggle_theme(ctx);
            }
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.add_space(2.0);
                // Убираем только переносы: trim() съедает ведущий пробел
                // первой строки ASCII-арта, и она встаёт на символ левее остальных.
                let lines: Vec<&str> = BANNER.trim_matches(['\r', '\n']).lines().collect();
                let line_width = |ui: &egui::Ui, text: &str, size: f32| -> f32 {
                    ui.fonts(|f| {
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
                let row_h = ui.fonts(|f| {
                    let job = egui::text::LayoutJob::single_section(
                        "█".to_string(),
                        egui::TextFormat {
                            font_id: egui::FontId::monospace(size),
                            ..Default::default()
                        },
                    );
                    f.layout_job(job).size().y
                });
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
                        format!("Не найдены загрузчики: {} — могу скачать их сам.", missing.join(", "))
                    } else {
                        format!("Не найдены загрузчики: {} — установите их вручную.", missing.join(", "))
                    },
                );
                if cfg!(windows) && ui
                    .add_enabled(
                        self.phase != Phase::Setup && self.jobs.is_empty() && self.torrent_pick.is_none(),
                        egui::Button::new("установить загрузчики"),
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
                                egui::Button::new("обновить").wrap_mode(egui::TextWrapMode::Extend))
                            .on_hover_text("Перескачать свежие yt-dlp и aria2c")
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
                let screen = ctx.screen_rect();
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
                    "Похоже, сайту нужна авторизация (бот-детект или возрастные ограничения).",
                );
                if ui.button("Повторить с куками из браузера").clicked() {
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
                    egui::Button::new(egui::RichText::new("▸ журнал").color(ui.visuals().weak_text_color()))
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

// icons/icon-256.png is exported from the same artwork as icon-app.ico
// with a Lanczos3 resize. Decoding and resizing the source at
// every startup would slow launch and bloat the executable.
// egui::IconData wants a square with a side that's a multiple of 4; 256x256
// is its documented recommendation. The crop/resize branch stays as a
// fallback if the asset is ever replaced by a non-256 one again. Decode
// failure degrades to "no icon" instead of the old expect(): release builds
// are panic="abort" with no console, so that expect was a silent startup
// death with no window and no message.
const ICON_PNG: &[u8] = include_bytes!("../../icons/icon-256.png");

fn app_icon() -> Option<egui::IconData> {
    use image::GenericImageView;
    let img = image::load_from_memory(ICON_PNG).ok()?;
    let (w, h) = img.dimensions();
    let img = if w == 256 && h == 256 {
        img
    } else {
        let side = w.min(h);
        img.crop_imm((w - side) / 2, (h - side) / 2, side, side)
            .resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
    };
    let rgba = img.into_rgba8();
    Some(egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// A windows-subsystem exe has no console to print to, so a startup failure
/// (most often "no OpenGL 2.1+" in a bare VM or an RDP session) used to kill
/// the process with no window and no error at all. Show it in a MessageBox.
#[cfg(windows)]
fn fatal_dialog(title: &str, text: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut c_void,
            text: *const u16,
            caption: *const u16,
            mb_type: u32,
        ) -> i32;
    }
    const MB_ICONERROR: u32 = 0x10;
    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    };
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide(title).as_ptr(),
            MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn fatal_dialog(_title: &str, _text: &str) {}

fn main() -> eframe::Result {
    // panic="abort" still runs the hook before aborting: surface internal
    // panics instead of dying silently without a console.
    std::panic::set_hook(Box::new(|info| {
        fatal_dialog(
            "SNATCH — внутренняя ошибка",
            &format!("SNATCH не смог продолжить работу.\n\n{info}"),
        );
    }));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([460.0, 720.0])
        .with_min_inner_size([460.0, 600.0])
        .with_title("SNATCH — by rercon prod.");
    #[cfg(windows)]
    {
        viewport = viewport.with_decorations(false);
    }
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let result = eframe::run_native(
        "SNATCH",
        options,
        Box::new(|cc| Ok(Box::new(SnatchApp::new(cc)))),
    );
    if let Err(e) = &result {
        fatal_dialog(
            "SNATCH — не удалось открыть окно",
            &format!(
                "Причина: {e}\n\nНа виртуальной машине или в RDP-сеансе это обычно означает, \
                 что недоступен OpenGL 2.1+. Включите 3D-ускорение в настройках ВМ либо \
                 запустите программу на обычном рабочем столе.\n\n\
                 CLI-версия (snatch.exe) от графики не зависит и работает всегда."
            ),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> SnatchApp {
        let (tx, rx) = channel();
        SnatchApp::from_config(
            snatch_rs::config::sanitize(&serde_json::Value::Null),
            tx,
            rx,
        )
    }

    fn test_job(url: &str) -> Job {
        Job {
            engine: "yt-dlp".into(),
            url: url.into(),
            out_dir: std::env::temp_dir(),
            fmt: "best".into(),
            cookies_browser: None,
        }
    }

    #[test]
    fn setup_progress_text_shows_bytes_percent_and_a_fallback() {
        let p = (
            "ffmpeg".to_string(),
            50 * 1024 * 1024,
            Some(100 * 1024 * 1024),
        );
        let t = setup_progress_text(Some(&p));
        assert!(t.contains("ffmpeg") && t.contains("50%"), "{t}");
        let p = ("yt-dlp.exe".to_string(), 1024, None);
        assert!(setup_progress_text(Some(&p)).contains("yt-dlp.exe"));
        assert_eq!(
            setup_progress_text(None),
            "скачиваю yt-dlp, aria2c, ffmpeg и Deno…"
        );
    }

    #[test]
    fn gui_extras_split_args_and_reject_a_bad_quote() {
        let e = extras_from_fields(true, "ru,en", true, "--limit-rate 5M", "").unwrap();
        assert!(e.subs && e.playlist);
        assert_eq!(e.sub_langs, "ru,en");
        assert_eq!(e.extra_ytdlp, ["--limit-rate", "5M"]);
        assert!(e.extra_aria2.is_empty());
        assert!(extras_from_fields(false, "", false, "\"oops", "").is_err());
    }

    #[test]
    fn refused_starts_keep_typed_urls_offers_and_queued_checkpoints() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let job = test_job("https://example.test/watch");
        app.url = job.url.clone();
        app.out_dir = job.out_dir.to_string_lossy().into_owned();
        app.retry_offer = Some(job.clone());
        app.start_download(&ctx);
        assert_eq!(app.url, job.url);
        assert!(app.retry_offer.is_some());
        app.retry_with_cookies(&ctx);
        assert!(app.retry_offer.is_some());
        app.queue.push_back(QueuedJob {
            job: job.clone(),
            select: torrent::Choice {
                files: Some(vec![1, 3]),
                meta: None,
            },
            no_continue: true,
        });
        app.fill_free_slots(&ctx);
        assert_eq!(app.paused.len(), 1);
        assert_eq!(app.paused[0].job.url, job.url);
        assert_eq!(
            app.paused[0].select.files.as_deref(),
            Some([1, 3].as_slice())
        );
        assert!(app.paused[0].no_continue);
        assert!(!app.paused[0].status.is_empty());
    }

    #[test]
    fn a_refused_automatic_restart_stays_retryable_without_a_false_status() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let job = test_job("https://example.test/watch");
        app.jobs.push(ActiveJob {
            id: 1,
            job: job.clone(),
            label: "video".into(),
            progress: Some(0.8),
            status: String::new(),
            status_kind: StatusKind::None,
            warns: vec![],
            cancel_flag: Arc::new(AtomicBool::new(false)),
            cancelled: false,
            pausing: false,
            auth_seen: false,
            missing_control_file: false,
            resumed_seen: true,
            no_continue: false,
            select: torrent::Choice::default(),
        });
        app.finish_job(1, 1, &ctx);
        assert!(app.jobs.is_empty());
        assert_eq!(app.paused.len(), 1);
        assert_eq!(app.paused[0].job.url, job.url);
        assert!(app.paused[0].no_continue);
        assert!(!app.status.contains("повторяю с начала"));
        assert!(matches!(app.status_kind, StatusKind::Err));
    }

    #[test]
    fn closing_refuses_picks_and_dispatch_before_any_side_effect() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.close_requested = true;
        let job = test_job("https://example.test/watch");
        assert!(!app.dispatch_job(&ctx, job.clone(), torrent::Choice::default()));
        app.begin_torrent_pick(&ctx, job);
        assert!(app.torrent_pick.is_none());
        assert!(app.queue.is_empty());
        assert!(app.jobs.is_empty());
        assert_eq!(app.next_pick_id, 0);
    }

    #[test]
    fn an_open_pick_blocks_duplicate_downloads_and_directory_popups() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let job = test_job("https://example.test/watch");
        app.torrent_pick = Some(TorrentPick {
            id: 1,
            job: job.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
            info: None,
            meta: None,
            selected: vec![],
            tree: vec![],
            view: torrent::TreeView::default(),
            error: None,
            hint: None,
        });
        assert!(app.url_in_flight(&job.url));
        app.retry_offer = Some(job);
        app.retry_with_cookies(&ctx);
        assert!(app.retry_offer.is_some());
        assert!(app.jobs.is_empty());
        app.open_dir_browser();
        assert!(!app.browser.open);
    }

    #[test]
    fn viewport_sizes_are_in_egui_points_without_a_second_scale() {
        for native in [1.0, 1.25, 2.0] {
            let ctx = egui::Context::default();
            let mut input = egui::RawInput::default();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .native_pixels_per_point = Some(native);
            let output = ctx.run(input, |ctx| pin_pixel_scale(ctx, true));
            let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
            assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::InnerSize(size) if *size == egui::vec2(460.0, 720.0))));
            assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::MinInnerSize(size) if *size == egui::vec2(460.0, 600.0))));
        }
    }

    #[test]
    fn directory_errors_are_visible_and_stale_scans_do_not_replace_navigation() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let current = std::env::temp_dir().join("wanted");
        let previous = std::env::temp_dir().join("previous");
        app.browser.open = true;
        app.browser.current = current.clone();
        app.browser.in_flight = Some(previous.clone());
        app.tx
            .send(Msg::DirListed(
                previous.clone(),
                previous,
                Ok((vec!["stale".into()], false)),
            ))
            .unwrap();
        app.drain(&ctx);
        assert_eq!(app.browser.current, current);
        assert!(app.browser.entries.is_empty());
        app.tx
            .send(Msg::DirListed(
                current.clone(),
                current.clone(),
                Err("test access denied".into()),
            ))
            .unwrap();
        app.drain(&ctx);
        assert_eq!(app.browser.listed_for, Some(current));
        assert_eq!(app.browser.error.as_deref(), Some("test access denied"));
    }

    #[test]
    fn torrent_console_summary_cannot_replace_gui_status_with_separator() {
        assert!(torrent_console_line(1, "*** Download Progress Summary as of now ***").is_none());
        assert!(torrent_console_line(1, "============================").is_none());
        assert!(
            matches!(torrent_console_line(1, "[#25017a 361MiB/6.8GiB(5%) CN:30 SD:5 DL:4.1MiB]"),
            Some(Msg::TorrentStatus(1, p, status)) if (p - 0.05).abs() < 0.001 && status.contains("361MiB/6.8GiB"))
        );
        assert!(
            matches!(torrent_console_line(1, "FILE: [MEMORY][METADATA][DL] Cuphead"),
            Some(Msg::TorrentMeta(1, name)) if name == "Cuphead")
        );
        assert!(
            matches!(torrent_console_line(1, "Exception: errorCode=13 File x exists, but a control file(*.aria2) does not exist."),
            Some(Msg::ErrLine(1, line)) if engines::aria2_missing_control(&line))
        );
    }

    #[test]
    fn long_local_torrent_filename_is_a_valid_aria2_job() {
        let dir = std::env::temp_dir().join(format!("snatch-gui-torrent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("[DL] Cuphead [L] [RUS + ENG + 7] (2017, Arcade) (1.3.9 + 1 DLC) [GOG] [rutracker-5768660].torrent");
        std::fs::write(&file, b"d4:infod0:e").unwrap();
        let url = local_torrent_path(&file).unwrap();
        assert_eq!(url, file.to_string_lossy());
        assert_eq!(detect_engine(&url), "aria2");
        let job = Job {
            engine: detect_engine(&url).into(),
            url,
            out_dir: dir.join("Downloads"),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let tc = Toolchain {
            yt_dlp: None,
            aria2c: Some(PathBuf::from("aria2c")),
        };
        let cmd = engines::build(&job, &tc).unwrap();
        assert_eq!(cmd[cmd.len() - 2], "--");
        assert_eq!(cmd.last().unwrap(), file.as_os_str());
        assert!(local_torrent_path(&dir.join("not-a-torrent.txt")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn job_controls_keep_same_height_with_and_without_progress() {
        let ctx = egui::Context::default();
        setup_theme(&ctx, true);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(460.0, 600.0),
            )),
            ..Default::default()
        };
        let mut heights = Vec::new();
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let green = ui.visuals().hyperlink_color;
                let yellow = ui.visuals().warn_fg_color;
                for (progress, paused, fill, labels) in [
                    (Some(0.3), false, green, (("пауза", 86.0), ("отмена", 86.0))),
                    (None, false, green, (("пауза", 86.0), ("отмена", 86.0))),
                    (
                        Some(0.3),
                        true,
                        yellow,
                        (("продолжить", 100.0), ("убрать", 72.0)),
                    ),
                ] {
                    let top = ui.cursor().top();
                    let _ = job_controls(
                        ui,
                        progress,
                        fill,
                        paused,
                        "Подключение…",
                        labels.0,
                        labels.1,
                    );
                    heights.push(ui.cursor().top() - top);
                }
            });
        });
        for height in heights {
            assert!(
                (height - 34.0).abs() < 1.0,
                "row + spacing must be 28 + 6, got {height}"
            );
        }
    }

    #[test]
    fn torrent_pick_rows_have_the_height_show_rows_assumes() {
        let ctx = egui::Context::default();
        setup_theme(&ctx, true);
        let files = [
            torrent::FileEntry {
                index: 1,
                path: "folder/a.bin".into(),
                size: 10,
            },
            torrent::FileEntry {
                index: 2,
                path: "folder/sub/b.bin".into(),
                size: 20,
            },
            torrent::FileEntry {
                index: 3,
                path: "top.txt".into(),
                size: 5,
            },
        ];
        let mut tree = torrent::build_torrent_tree(&files);
        tree[0].expanded = true;
        let mut selected = vec![true; files.len()];
        let mut rows = Vec::new();
        torrent_visible_rows(&tree, 0, &mut Vec::new(), &mut rows);
        // folder, folder/sub, folder/a.bin, top.txt: two folder rows (▶ toggle
        // + checkbox) and two file rows (checkbox only).
        assert_eq!(rows.len(), 4);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 600.0),
            )),
            ..Default::default()
        };
        let (mut heights, mut pitch) = (Vec::new(), 0.0);
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let row_h = torrent_row_height(ui);
                pitch = row_h + ui.spacing().item_spacing.y;
                for row in &rows {
                    let top = ui.cursor().top();
                    torrent_tree_row(ui, &mut tree, &mut selected, row, row_h);
                    heights.push(ui.cursor().top() - top);
                }
            });
        });
        for height in heights {
            assert!(
                (height - pitch).abs() < 0.5,
                "every row must advance {pitch}, got {height}"
            );
        }
    }

    #[test]
    fn torrent_pick_window_fits_the_default_window() {
        let ctx = egui::Context::default();
        setup_theme(&ctx, true);
        let files: Vec<torrent::FileEntry> = (1..=30)
            .map(|i| torrent::FileEntry {
                index: i,
                path: format!(
                    "Очень длинное имя папки релиза {}/вложенная папка с длинным именем/файл номер {i} с длинным названием.mkv",
                    i % 3
                ),
                size: 1 << 30,
            })
            .collect();
        let mut tree = torrent::build_torrent_tree(&files);
        for node in &mut tree {
            node.expanded = true;
        }
        let mut pick = TorrentPick {
            id: 0,
            job: Job {
                engine: "aria2".into(),
                url: "magnet:?xt=urn:btih:c01abfff06149cb74765dca1b5226003eeb4e877".into(),
                out_dir: PathBuf::from("."),
                fmt: "best".into(),
                cookies_browser: None,
            },
            cancel: Arc::new(AtomicBool::new(false)),
            info: Some(torrent::TorrentInfo {
                name: "Очень длинное имя торрента, которое не помещается в одну строку диалога"
                    .into(),
                total: 30 << 30,
                files: files.clone(),
            }),
            meta: None,
            selected: vec![true; files.len()],
            tree,
            view: torrent::TreeView::default(),
            error: None,
            hint: None,
        };
        // The app's default (and minimum) window is 460 points wide.
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(460.0, 640.0));
        let frame = |pick: &mut TorrentPick| {
            let (mut rect, mut buttons) = (None, Vec::new());
            // A Window settles its size over the first frames.
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    let (act, shown) = torrent_pick_window(ctx, pick);
                    (rect, buttons) = (shown, act.buttons);
                });
            }
            (rect.expect("the pick window is shown"), buttons)
        };
        let check =
            |what: &str, (dialog, buttons): (egui::Rect, Vec<egui::Rect>), expected: usize| {
                assert!(
                    screen.contains_rect(dialog),
                    "{what} dialog {dialog:?} must fit {screen:?}"
                );
                assert_eq!(buttons.len(), expected, "{what}: buttons drawn");
                for (i, a) in buttons.iter().enumerate() {
                    assert!(
                        dialog.contains_rect(*a),
                        "{what}: button {a:?} outside {dialog:?}"
                    );
                    for b in &buttons[i + 1..] {
                        let both = a.intersect(*b);
                        // An overlap made a click on one button hit the other.
                        assert!(
                            both.width() <= 0.0 || both.height() <= 0.0,
                            "{what}: {a:?} overlaps {b:?}"
                        );
                    }
                }
            };
        check("list", frame(&mut pick), 4);
        pick.info = None;
        pick.error = Some("не удалось получить метаданные торрента (таймаут)".into());
        check("error", frame(&mut pick), 2);
    }

    #[test]
    fn late_startup_discovery_does_not_undo_installed_tools() {
        let installed = PathBuf::from("installed-aria2c.exe");
        let mut tools = Toolchain {
            yt_dlp: None,
            aria2c: Some(installed.clone()),
        };
        apply_initial_discovery(
            &mut tools,
            true,
            Toolchain {
                yt_dlp: None,
                aria2c: None,
            },
        );
        assert_eq!(tools.aria2c, Some(installed));
        let mut not_installed = Toolchain {
            yt_dlp: None,
            aria2c: None,
        };
        apply_initial_discovery(&mut not_installed, false, tools);
        assert!(not_installed.aria2c.is_some());
    }
}
