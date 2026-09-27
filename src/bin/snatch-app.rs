#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use snatch_rs::config::{home_dir, Config};
use snatch_rs::engines::{
    self, detect_engine, is_unc_path, looks_like_auth, parse_progress, preflight_warning, validate_url, Job,
    COOKIES_BROWSERS, FORMATS,
};
use snatch_rs::setup::{install_aria2, install_yt_dlp};
use snatch_rs::tools::{bootstrap_dir, Toolchain};
use snatch_rs::ui::{clip, safe_read_dirs};
use snatch_rs::{BANNER, TELEGRAM_URL};

const APP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " — by rercon prod.");
const MAX_LOG_LINES: usize = 500;
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

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Setup,
}

#[derive(Clone, Copy)]
enum StatusKind {
    None,
    Ok,
    Warn,
    Err,
}

enum Msg {
    Log(String),
    ErrLine(String),
    Progress(f32),
    TorrentStatus(f32, String),
    TorrentMeta(String),
    TorrentName(String),
    TorrentLog(String),
    Done(i32),
    SetupLog(String),
    /// (успех, обновлённая цепочка инструментов - discover делается в том же
    /// фоновом потоке, а не на UI-потоке)
    SetupDone(bool, Toolchain),
    ToolsReady(Toolchain),
}

/// Win32 job object with KILL_ON_JOB_CLOSE, hand-rolled FFI (the project
/// deliberately has no windows/winapi crate). The spawned loader is assigned
/// to a job for the lifetime of its monitor thread, which fixes two orphan
/// classes at once:
/// - "отменить" used to `Child::kill()` only the direct child; yt-dlp's own
///   children (ffmpeg mid-merge, aria2c as external downloader) survived and
///   kept writing to the output folder;
/// - if the GUI itself dies (crash, task-manager kill), the OS closes the
///   last job handle and takes the whole downloader tree with it.
///
/// `TerminateJobObject` on cancel kills the tree in one call.
#[cfg(windows)]
mod job_object {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[repr(C)]
    struct IoCounters {
        read_ops: u64,
        write_ops: u64,
        other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }

    #[repr(C)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct ExtendedLimitInformation {
        basic: BasicLimitInformation,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    pub struct Job {
        handle: Handle,
    }

    // The handle is process-global state; it is created on the UI thread and
    // then owned exclusively by one monitor thread.
    unsafe impl Send for Job {}

    impl Job {
        /// Returns None (and leaks nothing) if any step fails - the caller
        /// falls back to plain Child::kill semantics.
        pub fn create_and_assign(process: Handle) -> Option<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: ExtendedLimitInformation = std::mem::zeroed();
                info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<ExtendedLimitInformation>() as u32,
                );
                if ok == 0 || AssignProcessToJobObject(handle, process) == 0 {
                    CloseHandle(handle);
                    return None;
                }
                Some(Self { handle })
            }
        }

        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.handle, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                // Last handle closes -> KILL_ON_JOB_CLOSE reaps whatever of
                // the tree is still alive.
                CloseHandle(self.handle);
            }
        }
    }
}

/// Non-Windows placeholder so the monitor-loop code below stays cfg-free.
#[cfg(not(windows))]
mod job_object {
    pub struct Job;

    impl Job {
        pub fn create_and_assign(_process: *mut std::ffi::c_void) -> Option<Self> {
            None
        }
        pub fn terminate(&self) {}
    }
}

struct LastPlan {
    url: String,
    engine: String,
    out_dir: String,
}

struct DirBrowser {
    open: bool,
    current: PathBuf,
    entries: Vec<PathBuf>,
    listed_for: Option<PathBuf>,
}

fn torrent_console_line(line: &str) -> Option<Msg> {
    let line = line.trim();
    if line.is_empty() || line.contains("Download Progress Summary as of")
        || line.chars().all(|c| matches!(c, '=' | '-')) {
        return None;
    }
    if line.starts_with("FILE:") && line.contains("[MEMORY][METADATA]") {
        let name = line.split_once("[DL]").map_or("", |(_, name)| name.trim());
        return Some(Msg::TorrentMeta(name.to_string()));
    }
    if let Some(name) = engines::aria2_name_from_file(line) {
        return Some(Msg::TorrentName(name));
    }
    if let (Some(p), Some(stat)) = (parse_progress(line), engines::aria2_stat(line)) {
        return Some(Msg::TorrentStatus(p, stat));
    }
    if line.contains("Allocating disk space") {
        return Some(Msg::TorrentLog("Выделение места на диске…".into()));
    }
    let lower = line.to_ascii_lowercase();
    if ["error", "warning", "failed", "aborted", "exception"]
        .iter().any(|hint| lower.contains(hint)) {
        return Some(Msg::ErrLine(line.to_string()));
    }
    None
}

fn local_torrent_path(path: &Path) -> Result<String, String> {
    let supported = path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
        ["torrent", "metalink", "meta4"].iter().any(|allowed| ext.eq_ignore_ascii_case(allowed))
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
    cfg: Config,
    tc: Toolchain,
    /// Startup discovery may finish after an installer run; its stale result
    /// must never replace the toolchain found by SetupDone.
    setup_started: bool,
    phase: Phase,
    progress: Option<f32>,
    torrent_name: Option<String>,
    status: String,
    status_kind: StatusKind,
    warns: Vec<String>,
    log: Vec<String>,
    show_log_window: bool,
    auth_seen: bool,
    missing_control_file: bool,
    cancelled: bool,
    offer_retry: bool,
    last_plan: Option<LastPlan>,
    last_job: Option<Job>,
    last_no_continue: bool,
    resumed_seen: bool,
    show_url_history: bool,
    show_dir_history: bool,
    browser: DirBrowser,
    drives: Vec<PathBuf>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cancel_flag: Arc<AtomicBool>,
    /// Set when the user closed the window mid-download: the close is vetoed
    /// (CancelClose) until the job is actually killed, then drain() reissues
    /// Close - otherwise the process exits while the loader keeps running.
    close_requested: bool,
    #[cfg(windows)]
    maximized: bool,
    /// Bumped on every push_log; the log window re-joins its text only when
    /// this differs from log_cache_rev instead of every frame.
    log_rev: u64,
    log_cache_rev: u64,
    log_cache: String,
    dark_mode: bool,
}

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
    fonts.font_data.insert("mono-button".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO).tweak(egui::FontTweak {
            y_offset_factor: 0.0, ..Default::default()
        }));
    fonts.families.insert(egui::FontFamily::Name("button".into()), vec!["mono-button".to_owned()]);
    fonts.font_data.insert("mono-title".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO).tweak(egui::FontTweak {
            y_offset_factor: -0.08, ..Default::default()
        }));
    fonts
        .families
        .insert(egui::FontFamily::Name("title".into()), vec!["mono-title".to_owned()]);
    ctx.set_fonts(fonts);
    ctx.set_visuals(terminal_visuals(dark));
    // item_spacing.y is the *tight* rhythm (label to its own field, mockup's
    // ~6-7px); the *loose* rhythm between field groups (mockup's ~16-18px)
    // is added explicitly via ui.add_space() between groups - one uniform
    // spacing value can't express both.
    ctx.style_mut(|style| {
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
        // egui's defaults (Body/Button 14, Small 10) are all a step above the
        // mockup, which is why every string in the app read larger than its
        // HTML twin: mockup sizes are input/select/checkbox 13, .btn 12.5,
        // label.tag/.hint 12, footer/banner 11.
        let mut ts = style.text_styles.clone();
        ts.insert(egui::TextStyle::Body, egui::FontId::new(13.0, egui::FontFamily::Proportional));
        ts.insert(egui::TextStyle::Button, egui::FontId::new(12.5, egui::FontFamily::Name("button".into())));
        ts.insert(egui::TextStyle::Small, egui::FontId::new(13.0, egui::FontFamily::Proportional));
        ts.insert(egui::TextStyle::Monospace, egui::FontId::new(13.0, egui::FontFamily::Monospace));
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
            rgb(0x0b, 0x0b, 0x0e), rgb(0x0e, 0x0e, 0x11), rgb(0x11, 0x11, 0x14),
            rgb(0x1e, 0x1e, 0x24), rgb(0x33, 0x33, 0x3c),
            rgb(0xd6, 0xd6, 0xda), rgb(0x83, 0x83, 0x8c),
            rgb(0x59, 0xd6, 0x8c), rgb(0xe0, 0xb3, 0x4d), rgb(0xe0, 0x65, 0x5c),
        )
    } else {
        (
            rgb(0xf3, 0xf3, 0xf1), rgb(0xff, 0xff, 0xff), rgb(0xfb, 0xfb, 0xfa),
            rgb(0xda, 0xda, 0xd6), rgb(0xb5, 0xb5, 0xb0),
            rgb(0x1c, 0x1c, 0x1e), rgb(0x5c, 0x5c, 0x62),
            rgb(0x1a, 0x7f, 0x37), rgb(0x9a, 0x67, 0x00), rgb(0xcf, 0x22, 0x2e),
        )
    };

    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.window_rounding = egui::Rounding::ZERO;
    v.menu_rounding = egui::Rounding::ZERO;
    v.window_fill = bg_lift;
    v.window_stroke = egui::Stroke::new(1.0, line);
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
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 45);
    v.selection.stroke = egui::Stroke::new(1.0, accent);

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
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, line);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, text_dim);

    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, line);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, text_dim);

    v.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, line_bright);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text);
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.weak_bg_fill = field;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, text);
    v.widgets.active.expansion = 0.0;

    v.widgets.open.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0, line_bright);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, text);

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
        .stroke(egui::Stroke::new(1.0, accent))
        .fill(egui::Color32::TRANSPARENT)
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
    let dot_color = if selected { accent } else { ui.visuals().weak_text_color() };
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
    ui.label(egui::RichText::new(format!("› {text}")).color(ui.visuals().weak_text_color()).size(12.0));
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
    egui::RichText::new(text).font(egui::FontId::new(13.0, egui::FontFamily::Name("title".into())))
}

fn clicked_outside(ctx: &egui::Context, rect: egui::Rect) -> bool {
    ctx.input(|i| i.pointer.any_pressed()
        && i.pointer.interact_pos().is_some_and(|pos| !rect.contains(pos)))
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
        ui.painter()
            .line_segment([egui::pos2(x, y), egui::pos2(x_end, y)], egui::Stroke::new(1.0, color));
        x += dash + gap;
    }
}

/// Fractional OS scaling (125/150%) is where the design fell apart: egui
/// rasterized 13pt as 16.25 blurry physical px and every measurement taken
/// from the mockup grew by the scale factor, so the app never matched the
/// HTML side by side. Pinning pixels_per_point to a constant makes one
/// design px equal UI_SCALE physical px on any display, whatever the OS
/// reports. UI_SCALE is larger than 1.0: the original 1:1 version read too
/// small, so the whole interface gets a global nudge instead of per-widget
/// size edits. The viewport is sized in logical px (OS-scaled), so the
/// desired physical size is divided back by the native scale.
const UI_SCALE: f32 = 1.18;

fn pin_pixel_scale(ctx: &egui::Context, resize_viewport: bool) {
    if ctx.pixels_per_point() == UI_SCALE {
        return;
    }
    let native = ctx.native_pixels_per_point().unwrap_or(1.0);
    ctx.set_pixels_per_point(UI_SCALE);
    if resize_viewport {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            660.0 * UI_SCALE / native,
            640.0 * UI_SCALE / native,
        )));
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
        560.0 * UI_SCALE / native,
        520.0 * UI_SCALE / native,
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

impl SnatchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = Config::load();
        setup_theme(&cc.egui_ctx, cfg.dark_mode);
        pin_pixel_scale(&cc.egui_ctx, true);
        let out_dir = if !cfg.last_dir.is_empty() {
            cfg.last_dir.clone()
        } else {
            cfg.default_dir.clone()
        };
        let (tx, rx) = channel();
        // Bitmask query, no filesystem I/O: the old per-letter exists() probe
        // blocked startup for seconds when a mapped network drive was dead
        // (SMB redirector timeouts), with the window already created but not
        // yet painting.
        let drives = list_drives();
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
        let dark_mode = cfg.dark_mode;
        Self {
            url: String::new(),
            engine_mode: EngineMode::Auto,
            fmt: "best".to_string(),
            out_dir,
            use_cookies: false,
            cookies_browser: "chrome".to_string(),
            cfg,
            tc: Toolchain { yt_dlp: None, aria2c: None },
            setup_started: false,
            phase: Phase::Idle,
            progress: None,
            torrent_name: None,
            status: String::new(),
            status_kind: StatusKind::None,
            warns: Vec::new(),
            log: Vec::new(),
            show_log_window: false,
            auth_seen: false,
            missing_control_file: false,
            cancelled: false,
            offer_retry: false,
            last_plan: None,
            last_job: None,
            last_no_continue: false,
            resumed_seen: false,
            show_url_history: false,
            show_dir_history: false,
            browser: DirBrowser {
                open: false,
                current: PathBuf::new(),
                entries: Vec::new(),
                listed_for: None,
            },
            drives,
            tx,
            rx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            close_requested: false,
            #[cfg(windows)]
            maximized: false,
            log_rev: 0,
            log_cache_rev: 0,
            log_cache: String::new(),
            dark_mode,
        }
    }

    /// Flips the theme, applies it immediately and persists the choice so it
    /// doesn't reset to dark on the next launch.
    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.dark_mode = !self.dark_mode;
        ctx.set_visuals(terminal_visuals(self.dark_mode));
        self.cfg.dark_mode = self.dark_mode;
        self.cfg.save();
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
                let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
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

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Log(line) => {
                    let trimmed = line.trim_end();
                    // Exact yt-dlp prefix, not a substring search anywhere in
                    // the line: an uploader-chosen title containing
                    // "Resuming download" (arriving inside a Destination line)
                    // used to arm the automatic from-scratch retry for any
                    // subsequent failure of that download.
                    if trimmed.starts_with("[download] Resuming download") {
                        self.resumed_seen = true;
                    }
                    if !trimmed.is_empty() {
                        self.set_status(StatusKind::None, clip(trimmed, 96));
                        self.push_log(trimmed.to_string());
                    }
                }
                Msg::ErrLine(line) => {
                    if looks_like_auth(&line) {
                        self.auth_seen = true;
                    }
                    let trimmed = line.trim_end();
                    if engines::aria2_missing_control(trimmed) {
                        if !self.missing_control_file {
                            self.missing_control_file = true;
                            self.push_log(engines::ARIA2_MISSING_CONTROL_HINT.to_owned());
                        }
                    } else if !trimmed.is_empty() && !(trimmed.contains("Exception caught")
                        && self.last_plan.as_ref().is_some_and(|p| p.engine == "aria2")) {
                        self.push_log(trimmed.to_string());
                    }
                }
                Msg::Progress(p) => self.progress = Some(p),
                Msg::TorrentStatus(p, status) => {
                    self.progress = Some(p);
                    self.set_status(StatusKind::None, status);
                }
                Msg::TorrentMeta(name) => {
                    self.progress = None;
                    if !name.is_empty() {
                        self.push_log(format!("Торрент: {name}"));
                        self.torrent_name = Some(name);
                    }
                    self.set_status(StatusKind::None, "Получаю метаданные торрента…");
                }
                Msg::TorrentName(name) => self.torrent_name = Some(name),
                Msg::TorrentLog(line) => self.push_log(line),
                Msg::Done(code) => self.finish_download(code, ctx),
                Msg::SetupLog(line) => self.push_log(line),
                Msg::ToolsReady(tc) => apply_initial_discovery(&mut self.tc, self.setup_started, tc),
                Msg::SetupDone(ok, tc) => {
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
                }
            }
        }
        if self.phase != Phase::Idle {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        // The window close was vetoed while a job was being killed; now that
        // the phase is back to Idle, actually close.
        if self.close_requested && self.phase == Phase::Idle {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn finish_download(&mut self, code: i32, ctx: &egui::Context) {
        self.phase = Phase::Idle;
        self.progress = None;
        // Cancel that lost the race with a completed download still gets the
        // "Готово" branch: the file IS complete, claiming "отменено, можно
        // докачать" would send the user to re-download a finished file.
        let cancelled = std::mem::replace(&mut self.cancelled, false);
        if cancelled && code != 0 {
            self.set_status(StatusKind::Warn, "Отменено (файл можно докачать)");
            return;
        }
        if code == 13 && self.missing_control_file {
            self.offer_retry = false;
            self.set_status(StatusKind::Err, "Файлы уже есть без .aria2 — выберите новую пустую папку");
            self.show_log_window = true;
            return;
        }
        match code {
            0 => {
                if let Some(plan) = &self.last_plan {
                    self.cfg.remember_url(&plan.url);
                    self.cfg.remember_dir(&plan.out_dir);
                    self.cfg.save();
                    let dir = plan.out_dir.clone();
                    self.set_status(StatusKind::Ok, format!("Готово: {dir}"));
                } else {
                    self.set_status(StatusKind::Ok, "Готово");
                }
                self.offer_retry = false;
            }
            127 => self.set_status(
                StatusKind::Err,
                "Не удалось запустить загрузчик (бинарник пропал или не исполняем)",
            ),
            130 => self.set_status(StatusKind::Warn, "Прервано (файл можно докачать)"),
            c => {
                self.set_status(StatusKind::Err, format!("Ошибка (код {c}) — см. журнал"));
                self.show_log_window = true;
                self.offer_retry = c == 1 && self.auth_seen && self.last_plan.as_ref().is_some_and(|p| p.engine == "yt-dlp");
                // The run resumed a .part and still failed: the old part is
                // unusable (stale range / corrupt bytes), so retry the same
                // job once from scratch instead of making the user click.
                if self.resumed_seen
                    && !self.last_no_continue
                    && !self.offer_retry
                    && self.last_job.as_ref().is_some_and(|j| j.engine == "yt-dlp")
                {
                    if let Some(job) = self.last_job.clone() {
                        // Re-detect auth from the retry's own stderr instead of
                        // carrying the first attempt's hint into a possible
                        // cookies offer for a completely different failure.
                        self.auth_seen = false;
                        self.start_job(ctx, job, true);
                        if self.phase == Phase::Running {
                            self.set_status(
                                StatusKind::Warn,
                                "Докачка дала битый файл — повторяю с начала",
                            );
                        }
                    }
                }
            }
        }
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        self.offer_retry = false;
        self.auth_seen = false;

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
        // Only wipe the previous run's log once the new job is actually
        // valid: a typo in the URL field shouldn't destroy the log the user
        // may still be reading.
        self.clear_log();
        self.progress = None;
        let fmt = if engine == "yt-dlp" { self.fmt.clone() } else { "best".to_string() };
        let cookies = if engine == "yt-dlp" && self.use_cookies {
            Some(self.cookies_browser.clone())
        } else {
            None
        };

        let job = Job {
            engine: engine.clone(),
            url: url.clone(),
            out_dir: PathBuf::from(&self.out_dir),
            fmt,
            cookies_browser: cookies,
        };
        self.start_job(ctx, job, false);
    }

    /// Spawns the loader for an already-validated job. `no_continue` retries
    /// a resumed yt-dlp download from scratch: a stale/corrupt .part can't
    /// be validated by yt-dlp and poisons the run (Errno 22 mid-download or
    /// a merger "Invalid data" afterwards), so one clean retry heals it.
    fn start_job(&mut self, ctx: &egui::Context, job: Job, no_continue: bool) {
        let engine = job.engine.clone();
        let url = job.url.clone();
        self.progress = None;
        self.torrent_name = None;
        self.missing_control_file = false;
        self.warns = preflight_warning(&job);
        let mut cmd = match engines::build(&job, &self.tc) {
            Ok(c) => c,
            Err(e) => {
                self.set_status(StatusKind::Err, e);
                return;
            }
        };

        let insert_at = cmd.len().saturating_sub(2);
        if engine == "yt-dlp" {
            cmd.insert(insert_at, "--newline".into());
        } else {
            cmd.insert(insert_at, "--summary-interval=1".into());
        }
        if no_continue && engine == "yt-dlp" {
            cmd.insert(insert_at, "--no-continue".into());
        }

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
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(_) => {
                self.set_status(StatusKind::Err, "Не удалось запустить загрузчик");
                return;
            }
        };
        // Put the loader into a kill-on-close job object before it has
        // spawned children of its own (ffmpeg, aria2c-as-external-downloader)
        // - see mod job_object for the two orphan classes this closes.
        // Named proc_job: `job` is the engines::Job parameter below.
        let proc_job = {
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                job_object::Job::create_and_assign(child.as_raw_handle())
            }
            #[cfg(not(windows))]
            {
                job_object::Job::create_and_assign(std::ptr::null_mut())
            }
        };
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        self.last_plan = Some(LastPlan { url, engine, out_dir: self.out_dir.clone() });
        self.last_job = Some(job.clone());
        self.last_no_continue = no_continue;
        self.resumed_seen = false;
        self.phase = Phase::Running;
        self.cancelled = false;
        self.cancel_flag.store(false, Ordering::SeqCst);
        self.set_status(StatusKind::None, "Запускаю…");

        let tx = self.tx.clone();
        let cancel = self.cancel_flag.clone();
        let ctx_w = ctx.clone();
        let aria2 = job.engine == "aria2";
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
                    let line = engines::sanitize_child_output(&engines::decode_child_bytes(&buf));
                    if aria2 {
                        let mut latest = None;
                        for fragment in line.replace('\r', "\n").lines() {
                            if let Some(msg) = torrent_console_line(fragment) {
                                match msg {
                                    Msg::TorrentStatus(..) => latest = Some(msg),
                                    other => {
                                        if matches!(other, Msg::TorrentMeta(_)) { latest = None; }
                                        let _ = tx_out.send(other);
                                    }
                                }
                            }
                        }
                        if let Some(msg) = latest {
                            let due = last_progress_log
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(200));
                            if due {
                                if let Msg::TorrentStatus(_, ref stat) = msg {
                                    if last_torrent_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
                                        last_torrent_log = Some(Instant::now());
                                        let _ = tx_out.send(Msg::TorrentLog(stat.clone()));
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
                            let _ = tx_out.send(Msg::Progress(p));
                            let due = last_progress_log
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(500));
                            if due {
                                last_progress_log = Some(Instant::now());
                                let _ = tx_out.send(Msg::Log(line));
                                ctx_out.request_repaint();
                            }
                        }
                        None => {
                            let _ = tx_out.send(Msg::Log(line));
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
                    let line = engines::sanitize_child_output(&engines::decode_child_bytes(&buf));
                    let _ = tx_err.send(Msg::ErrLine(line));
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
                if cancel.load(Ordering::SeqCst) {
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
            let _ = h_out.join();
            let _ = h_err.join();
            let code = status.map(|s| s.code().unwrap_or(130)).unwrap_or(127);
            let _ = tx.send(Msg::Done(code));
            ctx_w.request_repaint();
        });
    }

    fn start_setup(&mut self, ctx: &egui::Context) {
        self.setup_started = true;
        self.phase = Phase::Setup;
        // A stale cookies-retry offer must not stay clickable through Setup
        // (it used to spawn a download on top of the installer, and the
        // SetupDone handler then flipped the phase out from under it).
        self.offer_retry = false;
        // Preflight warnings belong to the previous job, not to the installer.
        self.warns.clear();
        self.clear_log();
        self.set_status(StatusKind::None, "Устанавливаю загрузчики…");
        let tx = self.tx.clone();
        let ctx_w = ctx.clone();
        std::thread::spawn(move || {
            let bin = bootstrap_dir();
            let mut ok = std::fs::create_dir_all(&bin).is_ok();
            if ok {
                let _ = tx.send(Msg::SetupLog(format!(
                    "Устанавливаю загрузчики в {}",
                    bin.display()
                )));
                match install_yt_dlp(&bin) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("yt-dlp: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(format!("yt-dlp: ОШИБКА {e}")));
                        ok = false;
                    }
                }
                match install_aria2(&bin) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("aria2c: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(format!("aria2c: ОШИБКА {e}")));
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

    fn cancel(&mut self) {
        self.cancelled = true;
        self.cancel_flag.store(true, Ordering::SeqCst);
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
                    let title_width = (ui.available_width() - 3.0 * 40.0 - THEME_BTN_WIDTH).max(0.0);
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
                        self.maximized = !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
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
                    let label = if self.dark_mode { "день" } else { "ночь" };
                    if ui.add_sized([THEME_BTN_WIDTH, 38.0], egui::Button::new(label).frame(false))
                        .on_hover_text(if self.dark_mode { "Светлая тема" } else { "Тёмная тема" })
                        .clicked()
                    {
                        self.toggle_theme(ctx);
                    }
                    if ui.add_sized([40.0, 38.0], egui::Button::new("─").frame(false))
                        .on_hover_text("Свернуть")
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    if ui.add_sized([40.0, 38.0], egui::Button::new("□").frame(false))
                        .on_hover_text("Развернуть / восстановить")
                        .clicked()
                    {
                        self.maximized = !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
                    }
                    if ui.add_sized([40.0, 38.0], egui::Button::new("×").frame(false))
                        .on_hover_text("Закрыть")
                        .clicked()
                    {
                        match self.phase {
                            Phase::Running => {
                                self.close_requested = true;
                                self.cancel();
                            }
                            Phase::Setup => {
                                self.close_requested = true;
                                self.set_status(StatusKind::Warn, "Завершаю установку перед закрытием…");
                            }
                            Phase::Idle => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        }
                    }
                });
            });
    }

    #[cfg(windows)]
    fn ui_resize_edges(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized)) { return; }
        let screen = ctx.screen_rect();
        let e = 6.0;
        let w = screen.width();
        let h = screen.height();
        let handles = [
            (egui::pos2(0.0, 0.0), egui::vec2(e, e), egui::ResizeDirection::NorthWest),
            (egui::pos2(e, 0.0), egui::vec2(w - 2.0 * e, e), egui::ResizeDirection::North),
            (egui::pos2(w - e, 0.0), egui::vec2(e, e), egui::ResizeDirection::NorthEast),
            (egui::pos2(0.0, e), egui::vec2(e, h - 2.0 * e), egui::ResizeDirection::West),
            (egui::pos2(w - e, e), egui::vec2(e, h - 2.0 * e), egui::ResizeDirection::East),
            (egui::pos2(0.0, h - e), egui::vec2(e, e), egui::ResizeDirection::SouthWest),
            (egui::pos2(e, h - e), egui::vec2(w - 2.0 * e, e), egui::ResizeDirection::South),
            (egui::pos2(w - e, h - e), egui::vec2(e, e), egui::ResizeDirection::SouthEast),
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
        let mut pick_torrent = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Never gate this on history being non-empty: with the ghost
                // theme a disabled button is indistinguishable from an
                // enabled one, so an empty history read as "the button is
                // broken". The window opens anyway and explains itself.
                if ui
                    .add_enabled(self.phase == Phase::Idle, egui::Button::new("▾"))
                    .on_hover_text("Последние ссылки")
                    .clicked()
                {
                    toggle_hist = true;
                }
                #[cfg(windows)]
                if ui
                    .add_enabled(self.phase == Phase::Idle, egui::Button::new("файл"))
                    .on_hover_text("Выбрать локальный .torrent-файл")
                    .clicked()
                {
                    pick_torrent = true;
                }
                let edit = egui::TextEdit::singleline(&mut self.url)
                    .desired_width(f32::INFINITY)
                    // Default TextEdit margin is (4,2) - the mockup's inputs
                    // use `padding: 9px 11px`, noticeably roomier.
                    .margin(egui::Margin::symmetric(11.0, 9.0))
                    .hint_text("https://… magnet:… или путь к .torrent");
                ui.add_enabled(self.phase == Phase::Idle, edit);
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
        #[cfg(not(windows))]
        let _ = pick_torrent;
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
            self.show_url_history = open && (toggle_hist || !window
                .as_ref().is_some_and(|w| clicked_outside(ctx, w.response.rect)));
            if let Some(u) = picked {
                self.url = u;
                self.show_url_history = false;
            }
        }
    }

    fn ui_engine_row(&mut self, ui: &mut egui::Ui) -> bool {
        let enabled = self.phase == Phase::Idle;
        let mut dir_history_opened = false;
        ui.add_space(6.0);
        tag_label(ui, "движок");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(enabled, |ui| {
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
                    ui.weak(egui::RichText::new(format!("→ {guess}")).size(12.0));
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
                    .add_enabled(self.phase == Phase::Idle, egui::Button::new("▾"))
                    .on_hover_text("Последние папки")
                    .clicked()
                {
                    toggle_hist = true;
                }
                if ui
                    .add_enabled(
                        self.phase == Phase::Idle,
                        egui::Button::new("обзор"),
                    )
                    .on_hover_text("Выбрать папку для скачивания")
                    .clicked()
                {
                    open_browser = true;
                }
                ui.add_enabled(
                    self.phase == Phase::Idle,
                    egui::TextEdit::singleline(&mut self.out_dir)
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
        self.show_dir_history = open && (opened_this_frame || !window
            .as_ref().is_some_and(|w| clicked_outside(ctx, w.response.rect)));
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
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(self.log_cache.as_str())
                                .monospace()
                                .size(11.0)
                                .weak(),
                        );
                    });
            });
        self.show_log_window = open && (opened_this_frame || !window
            .as_ref().is_some_and(|w| clicked_outside(ctx, w.response.rect)));
    }

    fn open_dir_browser(&mut self) {
        // Cheap bitmask query - a drive plugged in since startup shows up.
        self.drives = list_drives();
        let typed = PathBuf::from(self.out_dir.trim());
        if is_unc_path(&typed) {
            self.set_status(StatusKind::Err, "Сетевую UNC-папку нельзя открывать. Выберите локальную папку.");
            return;
        }
        let start = if typed.is_dir() {
            typed
        } else if let Some(parent) = typed.parent().filter(|p| !is_unc_path(p) && p.is_dir()) {
            parent.to_path_buf()
        } else {
            home_dir().unwrap_or_else(|| PathBuf::from("."))
        };
        self.browser.current = start;
        self.browser.listed_for = None;
        self.browser.open = true;
    }

    fn ui_dir_browser(&mut self, ctx: &egui::Context) {
        if !self.browser.open {
            return;
        }
        let opened_this_frame = self.browser.listed_for.is_none();
        if self.browser.listed_for.as_ref() != Some(&self.browser.current) {
            self.browser.entries = safe_read_dirs(&self.browser.current);
            self.browser.listed_for = Some(self.browser.current.clone());
        }

        let mut still_open = true;
        let mut cancel_clicked = false;
        let mut chosen: Option<PathBuf> = None;
        let mut navigate: Option<PathBuf> = None;
        let cur = self.browser.current.clone();
        let entries = &self.browser.entries;
        let home = home_dir().unwrap_or_default();
        let drives = self.drives.clone();

        let window = egui::Window::new(window_title("Выбор папки"))
            .open(&mut still_open)
            .resizable(true)
            .default_size([480.0, 440.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
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
                        if ui.small_button(label.trim_end_matches('\\').to_string()).clicked() {
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
                    .show(ui, |ui| {
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
                        if entries.is_empty() {
                            ui.weak("(нет вложенных папок)");
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
                    if ui.add(accent_button(accent, "Выбрать эту папку")).clicked() {
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
        self.browser.open = still_open && (opened_this_frame || !window
            .as_ref().is_some_and(|w| clicked_outside(ctx, w.response.rect)));
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
        let yt = self.effective_engine() == "yt-dlp";
        ui.add_space(6.0);
        ui.add_enabled_ui(self.phase == Phase::Idle && yt, |ui| {
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

    fn ui_run_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        match self.phase {
            Phase::Running => {
                if let Some(name) = &self.torrent_name {
                    let accent = ui.visuals().hyperlink_color;
                    ui.label(egui::RichText::new(clip(name, 72)).size(12.0).color(accent));
                }
                match self.progress {
                    Some(p) => {
                        // ProgressBar hardcodes a pill shape (rounding =
                        // height/2) unless overridden - Rounding isn't part
                        // of Visuals for this widget, so the square-corners
                        // theme never reached it.
                        // .text() instead of .show_percentage(): that one
                        // truncates ((p*100) as usize), displaying "99%"
                        // until the very end; floor() rounds the same way
                        // but we control it (and 100% shows only at 100%).
                        ui.add(
                            egui::ProgressBar::new(p)
                                .text(format!("{}%", (p * 100.0).floor() as u32))
                                .rounding(egui::Rounding::ZERO)
                                .fill(ui.visuals().hyperlink_color)
                                .desired_height(36.0),
                        );
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(if self.status.starts_with("Получаю метаданные") {
                                "Метаданные торрента…"
                            } else {
                                "Подключение…"
                            });
                        });
                    }
                }
                // add_sized() with f32::INFINITY as the width draws the
                // button's border but renders no text at all (same egui quirk
                // documented below on the "СКАЧАТЬ" button) - a concrete width
                // doesn't have that problem.
                let width = ui.available_width();
                if ui.add_sized([width, 36.0], egui::Button::new("отменить")).clicked() {
                    self.cancel();
                }
            }
            Phase::Setup => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("скачиваю yt-dlp и aria2c…");
                });
            }
            Phase::Idle => {
                let can = !self.url.trim().is_empty()
                    && !self.out_dir.trim().is_empty()
                    && (self.tc.yt_dlp.is_some() || self.tc.aria2c.is_some());
                // `.cta { width: 100% }` in the mockup. add_sized() forces
                // Layout::centered_and_justified (confirmed in egui's
                // source), which is what actually centers the text - plain
                // min_size() just reserves space in the *ambient* layout
                // (Align::Min/left here), so the text stayed pinned left in
                // a much wider button. add_sized() with f32::INFINITY as the
                // width produced a button with a border but literally no
                // text at all; a concrete width doesn't have that problem
                // and still gets the forced centered layout.
                let width = ui.available_width();
                let accent = ui.visuals().hyperlink_color;
                let cta = egui::Button::new(
                    // The mockup's source text is lowercase, but its CSS has
                    // `.cta { text-transform: uppercase }` - the *rendered*
                    // (approved) look is "▶ СКАЧАТЬ", not "▶ скачать".
                    // Mockup's `.cta` is `font: 600 13px` - 16.0 here was
                    // never actually checked against it.
                    egui::RichText::new("▶ СКАЧАТЬ").color(accent).size(13.0),
                )
                .stroke(egui::Stroke::new(1.0, accent))
                .fill(egui::Color32::TRANSPARENT);
                let clicked = ui
                    .add_enabled_ui(can, |ui| ui.add_sized([width, 36.0], cta).clicked())
                    .inner;
                if clicked {
                    self.start_download(ctx);
                }
            }
        }
    }
}

impl eframe::App for SnatchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe re-applies the OS scale factor on DPI/monitor changes, so
        // the pin is re-asserted every frame, not just at startup.
        pin_pixel_scale(ctx, false);
        self.drain(ctx);
        let log_was_open = self.show_log_window;
        if self.phase == Phase::Idle {
            let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
            if let Some(path) = dropped {
                self.set_torrent_file(&path);
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.phase != Phase::Idle {
            // eframe exits the event loop in THIS frame unless the close is
            // vetoed - the old flag-only version let the process die before
            // the monitor thread (100ms poll) reached kill(), orphaning the
            // loader mid-download. Veto, cancel properly (job object kills
            // the whole tree), and drain() re-issues Close once Idle.
            self.close_requested = true;
            if self.phase == Phase::Running {
                self.cancel();
            } else {
                self.set_status(StatusKind::Warn, "Завершаю установку перед закрытием…");
            }
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
                            egui::RichText::new("t.me/ArtemMurzin").size(11.0),
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
            // The HTML window is 660px wide, with 26px content padding. A
            // resized native window may be wider; keep the form centred at
            // the same 608px maximum instead of stretching fields edge to edge.
            let width = ui.available_width().min(608.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.add_space(2.0);
                let lines: Vec<&str> = BANNER.trim().lines().collect();
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
                        self.phase == Phase::Idle,
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
                            .add_enabled(self.phase == Phase::Idle, egui::Button::new("обновить"))
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
            self.ui_url_row(ui, ctx);
            let dir_history_opened = self.ui_engine_row(ui);
            self.ui_cookies_row(ui);
            ui.add_space(10.0);
            self.ui_run_row(ui, ctx);
            self.ui_dir_history(ctx, dir_history_opened);
            self.ui_dir_browser(ctx);

            if !self.warns.is_empty() && self.phase != Phase::Idle {
                let warn = ui.visuals().warn_fg_color;
                for w in &self.warns {
                    ui.colored_label(warn, format!("! {w}"));
                }
            }

            // Gated on Idle: during Setup/Running this block used to stay
            // clickable and could spawn a second concurrent job sharing one
            // cancel flag and one message channel.
            if self.offer_retry && self.phase == Phase::Idle {
                ui.add_space(6.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Похоже, сайту нужна авторизация (бот-детект или возрастные ограничения).",
                );
                if ui.button("Повторить с куками из браузера").clicked() {
                    // One-shot: retry the PREVIOUS job with cookies, without
                    // flipping the persistent checkbox. A single (possibly
                    // provoked) 403 shouldn't silently opt every future
                    // download into reading the browser's cookie store.
                    if let Some(mut job) = self.last_job.clone() {
                        job.cookies_browser = Some(self.cookies_browser.clone());
                        self.offer_retry = false;
                        self.auth_seen = false;
                        self.clear_log();
                        self.start_job(ctx, job, false);
                    }
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

// icons/icon-256.png is icons/icon.png (1278x1230) pre-shrunk offline with
// the exact same center-crop + Lanczos3 pipeline this function used to run
// AT EVERY STARTUP - decoding the 1.5MB source and resampling it ~5x cost
// a visible chunk of launch time under opt-level="z" and bloated the exe.
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
    Some(egui::IconData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([660.0, 640.0])
        .with_min_inner_size([560.0, 520.0])
        .with_title("SNATCH — by rercon prod.");
    #[cfg(windows)]
    { viewport = viewport.with_decorations(false); }
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "SNATCH",
        options,
        Box::new(|cc| Ok(Box::new(SnatchApp::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_console_summary_cannot_replace_gui_status_with_separator() {
        assert!(torrent_console_line("*** Download Progress Summary as of now ***").is_none());
        assert!(torrent_console_line("============================").is_none());
        assert!(matches!(torrent_console_line("[#25017a 361MiB/6.8GiB(5%) CN:30 SD:5 DL:4.1MiB]"),
            Some(Msg::TorrentStatus(p, status)) if (p - 0.05).abs() < 0.001 && status.contains("361MiB/6.8GiB")));
        assert!(matches!(torrent_console_line("FILE: [MEMORY][METADATA][DL] Cuphead"),
            Some(Msg::TorrentMeta(name)) if name == "Cuphead"));
        assert!(matches!(torrent_console_line("Exception: errorCode=13 File x exists, but a control file(*.aria2) does not exist."),
            Some(Msg::ErrLine(line)) if engines::aria2_missing_control(&line)));
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
            engine: detect_engine(&url).into(), url, out_dir: dir.join("Downloads"),
            fmt: "best".into(), cookies_browser: None,
        };
        let tc = Toolchain { yt_dlp: None, aria2c: Some(PathBuf::from("aria2c")) };
        let cmd = engines::build(&job, &tc).unwrap();
        assert_eq!(cmd[cmd.len() - 2], "--");
        assert_eq!(cmd.last().unwrap(), file.as_os_str());
        assert!(local_torrent_path(&dir.join("not-a-torrent.txt")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn late_startup_discovery_does_not_undo_installed_tools() {
        let installed = PathBuf::from("installed-aria2c.exe");
        let mut tools = Toolchain { yt_dlp: None, aria2c: Some(installed.clone()) };
        apply_initial_discovery(&mut tools, true, Toolchain { yt_dlp: None, aria2c: None });
        assert_eq!(tools.aria2c, Some(installed));
        let mut not_installed = Toolchain { yt_dlp: None, aria2c: None };
        apply_initial_discovery(&mut not_installed, false, tools);
        assert!(not_installed.aria2c.is_some());
    }
}
