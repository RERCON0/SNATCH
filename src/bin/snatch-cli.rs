//! `snatch` terminal entry point - was a Rust port of the retired Python
//! CLI's `cli.py`/`ui.py` (now the only CLI), built on the same shared
//! `snatch_rs` lib the GUI uses. The old Python-era `down` alias is gone:
//! it pointed at the exact same entry point and only doubled build/test time.

use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::Parser;
use inquire::{Confirm, Select, Text};

use snatch_rs::config::Config;
use snatch_rs::engines::{
    self, build, detect_engine, preflight_warning, validate_url, Job, RunResult,
    COOKIES_BROWSERS, ENGINE_LABELS, FORMATS,
};
use snatch_rs::tools::Toolchain;
use snatch_rs::ui::{clip, safe_read_dirs};
use snatch_rs::{errln, outln, BANNER, TELEGRAM_URL};

const APP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " — by rercon prod.");
const WINDOW_TITLE: &str = "\x1b]0;SNATCH — by rercon prod.\x07";
const MAX_BROWSE_ENTRIES: usize = 100;
// inquire's Select defaults to an English help line ("↑↓ to move, enter to
// select, type to filter") with no way to localize it globally - it's a
// per-prompt-type default text, not something RenderConfig covers - so every
// Select::new(...) below adds this explicitly.
const SELECT_HELP: &str = "↑↓ — выбор, Enter — подтвердить, начните печатать — фильтр";
// ask_link's list has no free-text item to filter toward (typing a URL just
// filters everything out, so Enter has nothing to submit and looks dead -
// this is the "нажимаю Enter, ноль эмоций" bug); its own help text drops the
// filter hint to match `.without_filtering()` below.
const SELECT_HELP_PLAIN: &str = "↑↓ — выбор, Enter — подтвердить";
const RETRY_CODE: i32 = 1;

#[derive(Parser)]
#[command(
    name = "snatch",
    version = APP_VERSION,
    about = "SNATCH — мини-комбайн для скачивания: yt-dlp + aria2c в одном CLI."
)]
struct Args {
    /// Ссылка (если не указать — спросит интерактивно)
    url: Option<String>,
    /// Папка сохранения (без вопросов)
    #[arg(short = 'o', long)]
    output: Option<String>,
    /// Движок (по умолчанию — авто-подсказка)
    #[arg(short = 'e', long, value_parser = ["yt-dlp", "aria2"])]
    engine: Option<String>,
    /// Формат для yt-dlp
    #[arg(short = 'f', long, value_parser = ["best", "1080p", "audio"])]
    format: Option<String>,
    /// Пропустить все вопросы (нужны url и output)
    #[arg(short = 'y', long)]
    yes: bool,
    /// Откуда взять куки (chrome, firefox, edge, brave, opera, vivaldi, safari,
    /// chromium, whale) — для "Sign in to confirm you're not a bot" и
    /// возрастных ограничений
    #[arg(long = "cookies-from-browser", value_name = "BROWSER")]
    cookies_browser: Option<String>,
    /// Забыть последние ссылки и папки
    #[arg(long)]
    clear_history: bool,
    /// Не докачивать прерванное, начать файл с начала — лечит протухший .part
    /// («Invalid data» при склейке). В GUI такой повтор происходит автоматически.
    #[arg(long)]
    no_continue: bool,
}

struct Plan {
    url: String,
    engine: String,
    fmt: String,
    out_dir: String,
    cookies_browser: Option<String>,
    no_continue: bool,
}

fn plan_from_args(args: &Args) -> Option<Plan> {
    let (Some(url), Some(output)) = (args.url.clone(), args.output.clone()) else {
        errln("Для режима -y нужны ссылка и --output.");
        return None;
    };
    let engine = args.engine.clone().unwrap_or_else(|| detect_engine(&url).to_string());
    let fmt = args.format.clone().unwrap_or_else(|| "best".to_string());
    Some(Plan {
        url,
        engine,
        fmt,
        out_dir: output,
        cookies_browser: args.cookies_browser.clone(),
        no_continue: args.no_continue,
    })
}

/// `tc: None` discovers lazily, AFTER `validate_url` succeeds - mirrors
/// Python's `_download(plan, cfg, tc: Toolchain | None = None)`, which only
/// resolved `Toolchain.discover()` past that same validation. The `-y`
/// single-shot path relies on this: a bad URL fails fast without paying for
/// a PATH/winget scan it's about to throw away.
fn download(plan: &Plan, cfg: &mut Config, tc: Option<&Toolchain>) -> RunResult {
    let url = match validate_url(&plan.url) {
        Ok(u) => u,
        Err(e) => {
            errln(format!("✘ {e}"));
            return RunResult { code: 2, auth_hint: false };
        }
    };
    let discovered;
    let tc: &Toolchain = match tc {
        Some(t) => t,
        None => {
            discovered = Toolchain::discover();
            &discovered
        }
    };

    let job = Job {
        engine: plan.engine.clone(),
        url: url.clone(),
        out_dir: PathBuf::from(&plan.out_dir),
        fmt: plan.fmt.clone(),
        cookies_browser: plan.cookies_browser.clone(),
    };
    for warn in preflight_warning(&job) {
        errln(format!("⚠ {warn}"));
    }

    let mut cmd = match build(&job, tc) {
        Ok(c) => c,
        Err(e) => {
            errln(format!("✘ {e}"));
            return RunResult { code: 2, auth_hint: false };
        }
    };
    if plan.no_continue && plan.engine == "yt-dlp" {
        // Same insertion the GUI does: build() always ends with ["--", url],
        // so len-2 lands the flag after every option and before the URL.
        let at = cmd.len().saturating_sub(2);
        cmd.insert(at, "--no-continue".into());
    }

    let result = engines::run(&cmd, plan.engine == "yt-dlp");
    match result.code {
        0 => {
            cfg.remember_url(&url);
            cfg.remember_dir(&plan.out_dir);
            cfg.save();
            outln(format!("✔ Готово: {}", plan.out_dir));
        }
        130 => outln("Прервано пользователем (файл можно докачать той же командой)."),
        127 => errln("✘ Не удалось запустить загрузчик (бинарник пропал или не исполняем)."),
        c => errln(format!("✘ Ошибка (код {c}).")),
    }
    result
}

fn should_offer_cookies(plan: &Plan, code: i32, auth_hint: bool) -> bool {
    code == RETRY_CODE && auth_hint && plan.engine == "yt-dlp"
}

fn retry_with_cookies(plan: Plan, result: RunResult, cfg: &mut Config, tc: &Toolchain) -> RunResult {
    retry_with_cookies_inner(
        plan,
        result,
        cfg,
        |question, default| Confirm::new(question).with_default(default).prompt().ok(),
        || {
            let mut browsers: Vec<&str> = COOKIES_BROWSERS.to_vec();
            browsers.sort_unstable();
            Select::new("Браузер:", browsers)
                .with_help_message(SELECT_HELP)
                .prompt()
                .ok()
                .map(|b| b.to_string())
        },
        |p, c| download(p, c, Some(tc)),
    )
}

/// `confirm_fn`/`select_browser_fn`/`download_fn` are injected so the retry
/// loop (declined / no browser chosen / gives-up-after-second-failure) can be
/// exercised in tests without a real terminal - the Rust equivalent of
/// Python's `monkeypatch.setattr(cli.questionary, "confirm", ...)`.
fn retry_with_cookies_inner(
    mut plan: Plan,
    mut result: RunResult,
    cfg: &mut Config,
    mut confirm_fn: impl FnMut(&str, bool) -> Option<bool>,
    mut select_browser_fn: impl FnMut() -> Option<String>,
    mut download_fn: impl FnMut(&Plan, &mut Config) -> RunResult,
) -> RunResult {
    while should_offer_cookies(&plan, result.code, result.auth_hint) {
        let had_cookies = plan.cookies_browser.is_some();
        let question = if had_cookies {
            format!("\nНе скачалось (код {}). Попробовать с куками другого браузера?", result.code)
        } else {
            format!(
                "\nНе скачалось (код {}). Похоже, сайту нужна авторизация: можно взять куки \
                 из браузера и повторить. Попробовать?",
                result.code
            )
        };
        let Some(true) = confirm_fn(&question, !had_cookies) else {
            return result;
        };
        let Some(browser) = select_browser_fn() else {
            return result;
        };
        plan.cookies_browser = Some(browser);
        result = download_fn(&plan, cfg);
    }
    result
}

// ---- interactive prompts (port of ui.py) -----------------------------------

enum LinkChoice {
    New,
    Existing(String),
}

impl fmt::Display for LinkChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkChoice::New => write!(f, "Вставить новую ссылку"),
            LinkChoice::Existing(u) => write!(f, "{}", clip(u, 70)),
        }
    }
}

fn ask_link(cfg: &Config, preset: Option<String>) -> Option<String> {
    if let Some(p) = preset {
        return Some(p.trim().to_string());
    }
    let mut choices = vec![LinkChoice::New];
    choices.extend(cfg.urls.iter().take(6).cloned().map(LinkChoice::Existing));
    // inquire::Select filters options by keystrokes by default. Typing/pasting
    // a URL straight into this list (its only escape hatch, "Вставить новую
    // ссылку", never matches a URL) filters every option out, so Enter has no
    // highlighted answer to submit and silently does nothing. questionary's
    // select (the Python original) doesn't filter on typed input at all,
    // which is why this only reproduces here.
    let pick = Select::new("Ссылка:", choices)
        .without_filtering()
        .with_help_message(SELECT_HELP_PLAIN)
        .prompt()
        .ok()?;
    match pick {
        LinkChoice::Existing(u) => Some(u),
        LinkChoice::New => {
            let text = Text::new("Ссылка:").prompt().ok()?;
            let text = text.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
    }
}

/// Which engine is offered first (the guess) vs second, when both tools are
/// available and the user actually has to choose. Pulled out of `ask_engine`
/// so the ordering is directly testable without going through `Select`.
fn engine_guess_pair(url: &str) -> (&'static str, &'static str) {
    let guess = detect_engine(url);
    let other = if guess == "yt-dlp" { "aria2" } else { "yt-dlp" };
    (guess, other)
}

fn ask_engine(url: &str, tc: &Toolchain) -> Option<String> {
    let available: Vec<&str> = [("yt-dlp", tc.yt_dlp.is_some()), ("aria2", tc.aria2c.is_some())]
        .into_iter()
        .filter_map(|(name, has)| has.then_some(name))
        .collect();
    if available.is_empty() {
        return None;
    }
    if available.len() == 1 {
        return Some(available[0].to_string());
    }
    let (guess, other) = engine_guess_pair(url);
    let label_of = |e: &'static str| -> &'static str {
        ENGINE_LABELS.iter().find(|(k, _)| *k == e).map(|(_, l)| *l).unwrap_or(e)
    };
    struct EngineOpt(&'static str, String);
    impl fmt::Display for EngineOpt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.1)
        }
    }
    let choices = vec![
        EngineOpt(guess, format!("{}  (рекомендуется)", label_of(guess))),
        EngineOpt(other, label_of(other).to_string()),
    ];
    Select::new("Чем скачивать:", choices).with_help_message(SELECT_HELP).prompt().ok().map(|c| c.0.to_string())
}

fn ask_format() -> Option<String> {
    struct FmtOpt(&'static str, &'static str);
    impl fmt::Display for FmtOpt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.1)
        }
    }
    let choices: Vec<FmtOpt> = FORMATS.iter().map(|(k, l)| FmtOpt(k, l)).collect();
    Select::new("Формат (yt-dlp):", choices).with_help_message(SELECT_HELP).prompt().ok().map(|c| c.0.to_string())
}

enum DirEntryChoice {
    Select(PathBuf),
    Up,
    More(usize),
    Down(PathBuf),
}

impl fmt::Display for DirEntryChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirEntryChoice::Select(p) => write!(f, "✓ Выбрать эту папку ({})", p.display()),
            DirEntryChoice::Up => write!(f, "↑ Наверх"),
            // Honest wording: the list is alphabetical and the tail is never
            // rendered - "поднимитесь выше" could not reveal it.
            DirEntryChoice::More(n) => write!(
                f,
                "… показаны первые {MAX_BROWSE_ENTRIES} папок по алфавиту (всего {n})"
            ),
            DirEntryChoice::Down(p) => {
                write!(f, "📁 {}", p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default())
            }
        }
    }
}

fn browse_dir(start: PathBuf) -> Option<String> {
    let mut current = if start.exists() { start } else { snatch_rs::config::home_dir().unwrap_or(start) };
    loop {
        let all_dirs = safe_read_dirs(&current);
        let shown = all_dirs.len().min(MAX_BROWSE_ENTRIES);
        let mut choices = vec![DirEntryChoice::Select(current.clone()), DirEntryChoice::Up];
        choices.extend(all_dirs[..shown].iter().cloned().map(DirEntryChoice::Down));
        if all_dirs.len() > MAX_BROWSE_ENTRIES {
            choices.push(DirEntryChoice::More(all_dirs.len()));
        }
        let ans = Select::new(&format!("Папка: {}", current.display()), choices).with_help_message(SELECT_HELP).prompt().ok()?;
        match ans {
            DirEntryChoice::Select(p) => return Some(p.to_string_lossy().into_owned()),
            DirEntryChoice::Up => {
                if let Some(parent) = current.parent() {
                    current = parent.to_path_buf();
                }
            }
            DirEntryChoice::More(_) => {}
            DirEntryChoice::Down(p) => current = p,
        }
    }
}

enum DirChoice {
    Last(String),
    Downloads,
    Desktop,
    Cwd,
    Browse,
}

impl fmt::Display for DirChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirChoice::Last(d) => write!(f, "Последняя: {}", clip(d, 70)),
            DirChoice::Downloads => write!(f, "Загрузки"),
            DirChoice::Desktop => write!(f, "Рабочий стол"),
            DirChoice::Cwd => write!(f, "Текущая папка"),
            DirChoice::Browse => write!(f, "Выбрать в проводнике…"),
        }
    }
}

fn ask_dir(cfg: &Config) -> Option<String> {
    let home = snatch_rs::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut choices = Vec::new();
    if !cfg.last_dir.is_empty() && Path::new(&cfg.last_dir).exists() {
        choices.push(DirChoice::Last(cfg.last_dir.clone()));
    }
    choices.push(DirChoice::Downloads);
    choices.push(DirChoice::Desktop);
    choices.push(DirChoice::Cwd);
    choices.push(DirChoice::Browse);

    let pick = Select::new("Куда сохранять:", choices).with_help_message(SELECT_HELP).prompt().ok()?;
    match pick {
        DirChoice::Last(d) => Some(d),
        DirChoice::Downloads => Some(home.join("Downloads").to_string_lossy().into_owned()),
        DirChoice::Desktop => Some(home.join("Desktop").to_string_lossy().into_owned()),
        DirChoice::Cwd => Some(cwd.to_string_lossy().into_owned()),
        DirChoice::Browse => {
            let start = if !cfg.last_dir.is_empty() { PathBuf::from(&cfg.last_dir) } else { PathBuf::from(&cfg.default_dir) };
            browse_dir(start)
        }
    }
}

/// The plan summary line shown above "Скачать?" - split out from
/// `confirm_plan` so its content (does it mention the cookies browser, does
/// aria2 drop the yt-dlp-only bits) is directly assertable in tests.
fn confirm_message(engine: &str, fmt: &str, out_dir: &str, cookies_browser: Option<&str>) -> String {
    if engine == "yt-dlp" {
        let label = FORMATS.iter().find(|(k, _)| *k == fmt).map(|(_, l)| *l).unwrap_or(fmt);
        let mut m = format!(" yt-dlp · {label} · → {}", clip(out_dir, 40));
        if let Some(b) = cookies_browser {
            m.push_str(&format!(" · 🍪 куки: {b}"));
        }
        m
    } else {
        format!(" aria2c → {}", clip(out_dir, 40))
    }
}

fn confirm_plan(engine: &str, fmt: &str, out_dir: &str, url: &str, cookies_browser: Option<&str>) -> bool {
    let line = clip(url, 60);
    let msg = confirm_message(engine, fmt, out_dir, cookies_browser);
    // questionary.confirm() (the Python original) defaults to Yes-on-Enter by
    // its own library default (default=True), even though ui.py never passes
    // it explicitly. inquire::Confirm has no implicit default - without
    // .with_default(true), an empty Enter isn't a valid answer at all and
    // inquire rejects it with "Invalid answer, try typing 'y'...", so this
    // prompt looked dead on plain Enter where the Python one just said yes.
    Confirm::new(&format!("{msg}\n  {line}\nСкачать?")).with_default(true).prompt().unwrap_or(false)
}

fn collect(cfg: &Config, tc: &Toolchain, link: Option<String>, cookies_browser: Option<String>) -> Option<Plan> {
    let url = ask_link(cfg, link)?;
    if url.is_empty() {
        return None;
    }
    let engine = ask_engine(&url, tc)?;
    let fmt = if engine == "yt-dlp" { ask_format()? } else { "best".to_string() };
    let out_dir = ask_dir(cfg)?;
    if out_dir.is_empty() {
        return None;
    }
    let shown_cookies = if engine == "yt-dlp" { cookies_browser.as_deref() } else { None };
    if !confirm_plan(&engine, &fmt, &out_dir, &url, shown_cookies) {
        return None;
    }
    Some(Plan { url, engine, fmt, out_dir, cookies_browser, no_continue: false })
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let mut cfg = Config::load();

    if args.clear_history {
        cfg.clear_history();
        cfg.save();
        outln("История очищена.");
        return std::process::ExitCode::SUCCESS;
    }

    if args.yes {
        let code = match plan_from_args(&args) {
            Some(plan) => download(&plan, &mut cfg, None).code,
            None => 2,
        };
        return exit_code(code);
    }

    // Terminal check BEFORE Toolchain::discover(): when stdin/stdout are
    // piped there's nothing to prompt anyway, and discovery stats every PATH
    // entry - no reason to pay for that scan just to print the refusal.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        errln(
            "✘ Интерактивный режим требует настоящий терминал. \
             Используй Windows Terminal/cmd или режим -y со ссылкой и -o.",
        );
        return exit_code(2);
    }

    let tc = Toolchain::discover();
    if tc.yt_dlp.is_none() && tc.aria2c.is_none() {
        errln("✘ Не найдено ни одного инструмента: поставь yt-dlp и/или aria2c (например, через winget).");
        return exit_code(2);
    }

    // The title escape is only printed once VT processing is actually on:
    // on a legacy conhost (no VT) it used to render as literal "←]0;SNATCH…"
    // garbage above the banner. inquire/crossterm enables VT later, at the
    // first prompt - too late for this line.
    if vt_processing_enabled() {
        print!("{WINDOW_TITLE}");
        let _ = std::io::stdout().flush();
    }
    outln(BANNER.trim());
    outln(format!("                        ✈ {TELEGRAM_URL}"));

    let mut preset_url = args.url.clone();
    loop {
        let Some(mut plan) = collect(&cfg, &tc, preset_url.take(), args.cookies_browser.clone())
        else {
            outln("Отменено.");
            return exit_code(130);
        };
        plan.no_continue = args.no_continue;
        let result = download(&plan, &mut cfg, Some(&tc));
        let result = retry_with_cookies(plan, result, &mut cfg, &tc);
        match Confirm::new("\nСкачать ещё что-нибудь?").with_default(false).prompt() {
            Ok(true) => continue,
            _ => return exit_code(result.code),
        }
    }
}

/// Turns on ENABLE_VIRTUAL_TERMINAL_PROCESSING for stdout; false when stdout
/// isn't a switchable console (redirect) or the legacy API refuses - caller
/// then skips the OSC title escape entirely.
#[cfg(windows)]
fn vt_processing_enabled() -> bool {
    use std::ffi::c_void;
    extern "system" {
        fn GetStdHandle(n_std_handle: i32) -> *mut c_void;
        fn GetConsoleMode(h: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: *mut c_void, mode: u32) -> i32;
    }
    const STD_OUTPUT_HANDLE: i32 = -11;
    const INVALID_HANDLE_VALUE: isize = -1;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE as *mut c_void {
            return false;
        }
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return false; // not a console
        }
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn vt_processing_enabled() -> bool {
    true
}

fn exit_code_u8(code: i32) -> u8 {
    // Child crash statuses arrive as negative i32s (0xC0000005 access
    // violation -> -1073741819); the old clamp(0,255) mapped ALL of them to
    // 0 - `snatch -y` in a script would report a crashed download as success.
    if (0..=255).contains(&code) {
        code as u8
    } else {
        1
    }
}

fn exit_code(code: i32) -> std::process::ExitCode {
    std::process::ExitCode::from(exit_code_u8(code))
}

// Port of tests/test_cli.py + tests/test_ui.py from the retired Python CLI.
// inquire's Select/Confirm have no monkeypatch equivalent (they open a real
// terminal via get_default_terminal()), so retry_with_cookies_inner and
// confirm_message/engine_guess_pair exist specifically to make this
// coverage possible without one - see their doc comments above.
#[cfg(test)]
mod tests {
    use super::*;

    fn base_plan() -> Plan {
        Plan {
            url: "u".to_string(),
            engine: "yt-dlp".to_string(),
            fmt: "best".to_string(),
            out_dir: "d".to_string(),
            cookies_browser: None,
            no_continue: false,
        }
    }

    fn empty_cfg() -> Config {
        Config::load_from(&std::env::temp_dir().join("snatch-rs-cli-test-nonexistent.json"))
    }

    // -- should_offer_cookies (test_offers_only_on_child_auth_failure et al) --

    #[test]
    fn offers_only_on_child_auth_failure() {
        let plan = base_plan();
        assert!(should_offer_cookies(&plan, 1, true));
        assert!(!should_offer_cookies(&plan, 1, false));
        for code in [0, 2, 3, 127, 130, -9] {
            assert!(!should_offer_cookies(&plan, code, true), "code {code}");
        }
    }

    #[test]
    fn no_offer_for_aria2_engine() {
        let mut plan = base_plan();
        plan.engine = "aria2".to_string();
        assert!(!should_offer_cookies(&plan, 1, true));
    }

    #[test]
    fn offer_repeats_after_failed_cookies_attempt() {
        let mut plan = base_plan();
        plan.cookies_browser = Some("chrome".to_string());
        assert!(should_offer_cookies(&plan, 1, true));
    }

    // -- plan_from_args (test_plan_from_args_keeps_cookies_value, test_yes_mode_requires_url_and_output) --

    fn args_with(url: Option<&str>, output: Option<&str>, cookies_browser: Option<&str>) -> Args {
        Args {
            url: url.map(String::from),
            output: output.map(String::from),
            engine: None,
            format: None,
            yes: true,
            cookies_browser: cookies_browser.map(String::from),
            clear_history: false,
            no_continue: false,
        }
    }

    #[test]
    fn plan_from_args_carries_no_continue() {
        let mut args = args_with(Some("https://x"), Some("d"), None);
        assert!(!plan_from_args(&args).unwrap().no_continue);
        args.no_continue = true;
        assert!(plan_from_args(&args).unwrap().no_continue);
    }

    #[test]
    fn exit_code_maps_out_of_range_to_failure() {
        // A crashed child arrives as a negative i32 (0xC0000005 ->
        // -1073741819); it must never become exit status 0 ("success").
        assert_eq!(exit_code_u8(0), 0);
        assert_eq!(exit_code_u8(1), 1);
        assert_eq!(exit_code_u8(255), 255);
        assert_eq!(exit_code_u8(-1073741819), 1);
        assert_eq!(exit_code_u8(256), 1);
        assert_eq!(exit_code_u8(-1), 1);
    }

    #[test]
    fn plan_from_args_keeps_cookies_value() {
        let args = args_with(Some("https://x"), Some("d"), Some("Chrome:Profile 1"));
        let plan = plan_from_args(&args).unwrap();
        assert_eq!(plan.cookies_browser.as_deref(), Some("Chrome:Profile 1"));

        let args = args_with(Some("https://x"), Some("d"), None);
        assert_eq!(plan_from_args(&args).unwrap().cookies_browser, None);
    }

    #[test]
    fn plan_from_args_requires_url_and_output() {
        assert!(plan_from_args(&args_with(None, None, None)).is_none());
        assert!(plan_from_args(&args_with(Some("https://x"), None, None)).is_none());
        assert!(plan_from_args(&args_with(None, Some("d"), None)).is_none());
    }

    // -- download() validates before it would discover (test_yes_mode_bad_url_skips_tool_discovery) --
    //
    // Python's version proved this by making a fake Toolchain.discover() that
    // raised if called. inquire/Toolchain::discover() aren't mockable the
    // same way in Rust, so this only re-checks the observable contract (bad
    // URL -> code 2, validated before `tc` is ever touched) - the "skips
    // discovery" half is enforced by download()'s code order above, not by
    // this test.
    #[test]
    fn download_bad_url_fails_without_valid_toolchain() {
        let mut plan = base_plan();
        plan.url = "https://[::1".to_string();
        let mut cfg = empty_cfg();
        assert_eq!(download(&plan, &mut cfg, None).code, 2);
    }

    // -- retry_with_cookies_inner (test_retry_with_cookies_*, test_no_retry_without_auth_hint, test_retry_gives_up_after_second_failure) --

    #[test]
    fn retry_with_cookies_retries_and_succeeds() {
        let mut cfg = empty_cfg();
        let mut seen: Vec<Option<String>> = Vec::new();
        let out = retry_with_cookies_inner(
            base_plan(),
            RunResult { code: 1, auth_hint: true },
            &mut cfg,
            |_q, _default| Some(true),
            || Some("chrome".to_string()),
            |plan, _cfg| {
                seen.push(plan.cookies_browser.clone());
                RunResult { code: 0, auth_hint: false }
            },
        );
        assert_eq!(out.code, 0);
        assert_eq!(seen, vec![Some("chrome".to_string())]);
    }

    #[test]
    fn retry_with_cookies_declined_returns_original() {
        let mut cfg = empty_cfg();
        let out = retry_with_cookies_inner(
            base_plan(),
            RunResult { code: 1, auth_hint: true },
            &mut cfg,
            |_q, _default| Some(false),
            || panic!("select must not be called"),
            |_p, _c| panic!("download must not be called"),
        );
        assert_eq!(out.code, 1);
    }

    #[test]
    fn retry_with_cookies_no_browser_selected() {
        let mut cfg = empty_cfg();
        let out = retry_with_cookies_inner(
            base_plan(),
            RunResult { code: 1, auth_hint: true },
            &mut cfg,
            |_q, _default| Some(true),
            || None,
            |_p, _c| panic!("download must not be called"),
        );
        assert_eq!(out.code, 1);
    }

    #[test]
    fn no_retry_without_auth_hint() {
        let mut cfg = empty_cfg();
        let out = retry_with_cookies_inner(
            base_plan(),
            RunResult { code: 1, auth_hint: false },
            &mut cfg,
            |_q, _default| panic!("confirm must not be called"),
            || panic!("select must not be called"),
            |_p, _c| panic!("download must not be called"),
        );
        assert_eq!(out.code, 1);
    }

    #[test]
    fn retry_gives_up_after_second_failure() {
        let mut cfg = empty_cfg();
        let mut confirms = vec![false, true]; // .pop() yields true first, then false
        let mut calls = 0;
        let out = retry_with_cookies_inner(
            base_plan(),
            RunResult { code: 1, auth_hint: true },
            &mut cfg,
            move |_q, _default| confirms.pop(),
            || Some("chrome".to_string()),
            |_p, _c| {
                calls += 1;
                RunResult { code: 1, auth_hint: true }
            },
        );
        assert_eq!(out.code, 1);
        assert_eq!(calls, 1);
    }

    // -- ask_engine (test_ask_engine_auto_returns_single_available, test_ask_engine_no_engines_returns_none) --
    // Both cases return before ever reaching Select::new(), so ask_engine
    // itself is directly callable here without a terminal.

    #[test]
    fn ask_engine_auto_returns_single_available() {
        let tc = Toolchain { yt_dlp: Some(PathBuf::from("yt-dlp")), aria2c: None };
        assert_eq!(ask_engine("https://youtube.com/watch?v=1", &tc).as_deref(), Some("yt-dlp"));
        let tc = Toolchain { yt_dlp: None, aria2c: Some(PathBuf::from("aria2c")) };
        assert_eq!(ask_engine("https://youtube.com/watch?v=1", &tc).as_deref(), Some("aria2"));
    }

    #[test]
    fn ask_engine_no_engines_returns_none() {
        let tc = Toolchain { yt_dlp: None, aria2c: None };
        assert_eq!(ask_engine("https://x", &tc), None);
    }

    // -- engine_guess_pair (test_ask_engine_offers_guess_first) --

    #[test]
    fn ask_engine_offers_guess_first() {
        assert_eq!(engine_guess_pair("https://host/f.zip"), ("aria2", "yt-dlp"));
        assert_eq!(engine_guess_pair("https://youtube.com/watch?v=1"), ("yt-dlp", "aria2"));
    }

    // -- confirm_message (test_confirm_mentions_cookies, test_confirm_aria2_has_no_cookies_line) --

    #[test]
    fn confirm_mentions_cookies() {
        let msg = confirm_message("yt-dlp", "best", "d", Some("chrome"));
        assert!(msg.contains("chrome"));
        let msg = confirm_message("yt-dlp", "best", "d", None);
        assert!(!msg.contains("chrome"));
    }

    #[test]
    fn confirm_aria2_has_no_cookies_line() {
        let msg = confirm_message("aria2", "best", "d", None);
        assert!(msg.contains("aria2c"));
        assert!(!msg.contains("chrome"));
    }
}
