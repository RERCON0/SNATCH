#![cfg_attr(windows, windows_subsystem = "windows")]

use snatch_rs::i18n::{self, Language};
use snatch_rs::{tr, tr_format};
use std::collections::VecDeque;
use std::io::BufReader;
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
    self, detect_engine, looks_like_auth, parse_progress, preflight_warning, reaches_unc,
    validate_url, Engine, Format, Job, COOKIES_BROWSERS,
};
use snatch_rs::links::{self, LinkInfo};
use snatch_rs::setup::{
    install_aria2_with, install_deno_with, install_ffmpeg_with, install_yt_dlp_with, SetupProgress,
    SetupStage,
};
use snatch_rs::tools::{bootstrap_dir, Toolchain};
use snatch_rs::ui::{clip, try_read_dirs_limited};
use snatch_rs::{job_object, torrent};
use snatch_rs::{BANNER, TELEGRAM_URL};

mod dir_browser;
mod events;
mod form;
mod frame;
mod launch;
mod model;
mod session;
mod style;
mod torrent_ui;
mod view;
use model::*;
use style::*;

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

// "Terminal Native" palette (the approved mockup) - accent/OK/warn/error
// hues now live inside terminal_visuals() since dark and light mode each need
// their own values for contrast; read them back via ui.visuals().hyperlink_color
// (accent/OK), .warn_fg_color, .error_fg_color rather than a fixed constant.

struct SnatchApp {
    url: String,
    /// One debounced worker and one result; editing links cannot create an
    /// unbounded thread/cache queue. A result for an old URL is discarded.
    link_input: String,
    link_changed_at: Instant,
    link_in_flight: Option<String>,
    link_info: Option<(String, LinkInfo)>,
    engine_mode: EngineMode,
    fmt: Format,
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
    setup_progress: Option<SetupProgress>,
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
    /// (available width bits, font size, block width, row height) of the
    /// banner art: recomputed only when the width changes.
    banner_fit: Option<(u32, f32, f32, f32)>,
    /// `needs_link_probe` for the URL it was computed for. Asking again costs
    /// a file-system stat for a local path, and it is asked every frame.
    probe_memo: std::cell::RefCell<Option<(String, bool)>>,
}

/// The same default as the CLI's `-j`, so the two frontends behave alike out
/// of the box.
const MAX_CONCURRENT: usize = engines::DEFAULT_JOBS as usize;

/// How long starting a download waits for the download folder to answer (see
/// the use in `spawn_job`).
const FOLDER_IO_TIMEOUT: Duration = Duration::from_secs(5);

impl SnatchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cfg, warning) = match Config::try_load() {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(e)),
        };
        i18n::set_language(cfg.language);
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
            // Probe ffmpeg/Deno here, so the first download does not scan PATH
            // on the UI thread.
            engines::refresh_helper_presence();
        });
        let mut app = Self::from_config(cfg, tx, rx);
        if let Some(warning) = warning {
            app.push_log(tr_format!(
                "⚠ {warning}; the settings file was left unchanged",
                "⚠ {warning}; файл настроек сохранён без изменений"
            ));
            app.set_status(
                StatusKind::Warn,
                tr!(
                    "Could not read settings — see log",
                    "Не удалось прочитать настройки — см. журнал"
                ),
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
        let drives = snatch_rs::ui::drive_roots();
        let dark_mode = cfg.dark_mode;
        Self {
            url: String::new(),
            link_input: String::new(),
            link_changed_at: Instant::now(),
            link_in_flight: None,
            link_info: None,
            engine_mode: EngineMode::Auto,
            fmt: Format::Best,
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
            banner_fit: None,
            probe_memo: std::cell::RefCell::new(None),
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
            self.set_status(
                StatusKind::Warn,
                tr!(
                    "Theme settings were not saved — see log",
                    "Настройки темы не сохранены — см. журнал"
                ),
            );
        }
    }

    fn save_config(&mut self) -> bool {
        match self.cfg.try_save_now() {
            Ok(()) => true,
            Err(e) => {
                self.push_log(tr_format!(
                    "⚠ Could not save settings: {e}",
                    "⚠ Не удалось сохранить настройки: {e}"
                ));
                self.show_log_window = true;
                false
            }
        }
    }

    fn effective_engine(&self) -> Engine {
        match self.engine_mode {
            EngineMode::Auto => self
                .resolved_link()
                .map(|info| info.engine(&self.url))
                .unwrap_or_else(|| detect_engine(&self.url)),
            EngineMode::YtDlp => Engine::YtDlp,
            EngineMode::Aria2 => Engine::Aria2,
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
                self.set_status(
                    StatusKind::Ok,
                    tr_format!("Selected file: {}", "Выбран файл: {}", clip(&name, 64)),
                );
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

    fn resolved_link(&self) -> Option<&LinkInfo> {
        self.link_info
            .as_ref()
            .filter(|(url, _)| *url == engines::clean_url(&self.url))
            .map(|(_, info)| info)
    }

    fn needs_link_probe(&self) -> bool {
        if !matches!(self.engine_mode, EngineMode::Auto | EngineMode::Aria2) {
            return false;
        }
        let mut memo = self.probe_memo.borrow_mut();
        if let Some((url, needs)) = memo.as_ref() {
            if *url == self.url {
                return *needs;
            }
        }
        let needs = validate_url(&self.url).is_ok_and(|url| links::needs_probe(&url));
        *memo = Some((self.url.clone(), needs));
        needs
    }

    fn link_ready(&self) -> bool {
        !self.needs_link_probe() || self.resolved_link().is_some()
    }

    fn refresh_link(&mut self, ctx: &egui::Context) {
        let url = engines::clean_url(&self.url);
        if self.link_input != url {
            self.link_input = url.clone();
            self.link_changed_at = Instant::now();
            self.link_info = None;
        }
        if self.close_requested || self.phase == Phase::Setup || self.link_ready() {
            return;
        }
        let debounce = Duration::from_millis(350);
        if self.link_changed_at.elapsed() < debounce {
            ctx.request_repaint_after(debounce.saturating_sub(self.link_changed_at.elapsed()));
            return;
        }
        if self.link_in_flight.is_some() {
            return;
        }
        self.link_in_flight = Some(url.clone());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let info = links::resolve(&url);
            let _ = tx.send(Msg::LinkResolved(url, info));
            ctx.request_repaint();
        });
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
const ICON_PNG: &[u8] = include_bytes!("../../../icons/icon-256.png");

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
    #[link(name = "user32")]
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
            tr!("SNATCH — internal error", "SNATCH — внутренняя ошибка"),
            &tr_format!(
                "SNATCH could not continue.\n\n{info}",
                "SNATCH не смог продолжить работу.\n\n{info}"
            ),
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
            tr!("SNATCH — could not open window", "SNATCH — не удалось открыть окно"),
            &tr_format!("Reason: {e}\n\nIn a virtual machine or RDP session, OpenGL 2.1+ may be unavailable. Enable 3D acceleration in your VM settings or run SNATCH on a regular desktop.\n\nThe CLI (snatch.exe) does not require graphics.", "Причина: {e}\n\nНа виртуальной машине или в RDP-сеансе это обычно означает, \
                 что недоступен OpenGL 2.1+. Включите 3D-ускорение в настройках ВМ либо \
                 запустите программу на обычном рабочем столе.\n\n\
                 CLI-версия (snatch.exe) от графики не зависит и работает всегда."
            ),
        );
    }
    result
}

#[cfg(test)]
mod tests;
