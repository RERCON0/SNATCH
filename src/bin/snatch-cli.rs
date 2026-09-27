//! `snatch` terminal entry point - was a Rust port of the retired Python
//! CLI's `cli.py`/`ui.py` (now the only CLI), built on the same shared
//! `snatch_rs` lib the GUI uses. The old Python-era `down` alias is gone:
//! it pointed at the exact same entry point and only doubled build/test time.

use std::collections::VecDeque;
use std::fmt;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use inquire::{Confirm, Select, Text};

use snatch_rs::config::Config;
use snatch_rs::engines::{
    self, batch_name, build, detect_engine, is_unc_path, preflight_warning,
    validate_cookies_browser, validate_url, Job, RunResult, COOKIES_BROWSERS, ENGINE_LABELS, FORMATS,
};
#[cfg(windows)]
use snatch_rs::setup::{install_aria2, install_yt_dlp};
#[cfg(windows)]
use snatch_rs::tools::bootstrap_dir;
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
    /// Ссылки; несколько ссылок скачиваются параллельно
    #[arg(value_name = "URL")]
    urls: Vec<String>,
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
    /// Одновременных загрузок при нескольких ссылках (1–16)
    #[arg(short = 'j', long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=16))]
    jobs: u8,
    /// Откуда взять куки (chrome, firefox, edge, brave, opera, vivaldi, safari,
    /// chromium, whale) — для "Sign in to confirm you're not a bot" и
    /// возрастных ограничений
    #[arg(long = "cookies-from-browser", value_name = "BROWSER")]
    cookies_browser: Option<String>,
    /// Забыть последние ссылки и папки
    #[arg(long)]
    clear_history: bool,
    /// Установить загрузчики в папку SNATCH (Windows)
    #[arg(long)]
    install_tools: bool,
    /// Не докачивать прерванное, начать файл с начала — лечит протухший .part
    /// («Invalid data» при склейке). В GUI такой повтор происходит автоматически.
    #[arg(long)]
    no_continue: bool,
}

#[derive(Clone)]
struct Plan {
    url: String,
    engine: String,
    fmt: String,
    out_dir: String,
    cookies_browser: Option<String>,
    no_continue: bool,
}

fn plan_from_args(args: &Args) -> Option<Plan> {
    let (Some(url), Some(output)) = (args.urls.first(), args.output.as_ref()) else {
        errln("Для режима -y нужны хотя бы одна ссылка и --output.");
        return None;
    };
    let engine = args.engine.clone().unwrap_or_else(|| detect_engine(url).to_string());
    let fmt = args.format.clone().unwrap_or_else(|| "best".to_string());
    Some(Plan {
        url: url.clone(),
        engine,
        fmt,
        out_dir: output.clone(),
        cookies_browser: args.cookies_browser.clone(),
        no_continue: args.no_continue,
    })
}

fn plans_from_args(args: &Args) -> Option<Vec<Plan>> {
    let first = plan_from_args(args)?;
    let mut plans = vec![first];
    for url in args.urls.iter().skip(1) {
        plans.push(Plan {
            url: url.clone(),
            engine: args.engine.clone().unwrap_or_else(|| detect_engine(url).to_string()),
            ..plans[0].clone()
        });
    }
    Some(plans)
}

fn cli_command(
    job: &Job,
    tc: &Toolchain,
    no_continue: bool,
    batch: bool,
) -> Result<Vec<std::ffi::OsString>, String> {
    let mut cmd = build(job, tc)?;
    let at = cmd.len().saturating_sub(2);
    if no_continue && job.engine == "yt-dlp" {
        cmd.insert(at, "--no-continue".into());
    }
    // aria2 prints a newline-terminated progress summary even when stdout is
    // piped. The default interval (60s) makes magnet metadata look stuck.
    if job.engine == "aria2" {
        cmd.insert(at, "--summary-interval=2".into());
    } else if batch {
        cmd.insert(at, "--newline".into());
    }
    Ok(cmd)
}

/// `tc: None` discovers lazily, AFTER `validate_url` succeeds - mirrors
/// Python's `_download(plan, cfg, tc: Toolchain | None = None)`, which only
/// resolved `Toolchain.discover()` past that same validation. The `-y`
/// single-shot path relies on this: a bad URL fails fast without paying for
/// a PATH/winget scan it's about to throw away.
fn download(plan: &Plan, cfg: &mut Config, tc: Option<&Toolchain>) -> RunResult {
    download_with_discovery(plan, cfg, tc, Toolchain::discover)
}

fn download_with_discovery(
    plan: &Plan, cfg: &mut Config, tc: Option<&Toolchain>,
    discover: impl FnOnce() -> Toolchain,
) -> RunResult {
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
            discovered = discover();
            &discovered
        }
    };

    let clean_plan = Plan { url, ..plan.clone() };
    run_batch(std::slice::from_ref(&clean_plan), cfg, tc, 1)
        .into_iter().next().expect("one queued job has one result")
}

enum BatchEvent {
    Line(usize, String),
    Name(usize, String),
    Progress(usize, String),
    Finished(usize, String, String, RunResult),
}

fn clean_loader_text(text: &str) -> std::borrow::Cow<'_, str> {
    let cleaned = engines::sanitize_child_output(text);
    if cleaned.contains('\r') {
        std::borrow::Cow::Owned(cleaned.replace('\r', "\n"))
    } else {
        cleaned
    }
}

struct BatchView {
    name: String,
    status: String,
    last_printed: Option<Instant>,
}

struct BatchDisplay {
    multi: Option<MultiProgress>,
    bars: Vec<ProgressBar>,
    views: Vec<BatchView>,
    colors: bool,
}

fn highlight_progress(status: &str) -> String {
    const GREEN: &str = "\x1b[92m";
    const CYAN: &str = "\x1b[96m";
    const RESET: &str = "\x1b[0m";
    if status.starts_with("[download]") {
        let mut out = status.to_string();
        if let Some(at) = out.find(" at ") {
            let start = at + 4;
            let end = out[start..].find(' ').map_or(out.len(), |i| start + i);
            out.insert_str(end, RESET);
            out.insert_str(start, CYAN);
        }
        if let Some(end) = out.find('%') {
            // Advance past the WHOLE whitespace char: for a multi-byte
            // whitespace (e.g. NNBSP) `i + 1` is not a char boundary and
            // insert_str would panic.
            let start = out[..end].rfind(char::is_whitespace)
                .map_or(0, |i| i + out[i..].chars().next().map_or(1, char::len_utf8));
            out.insert_str(end + 1, RESET);
            out.insert_str(start, GREEN);
        }
        return out;
    }
    let mut fields = status.split(" · ");
    let Some(percent) = fields.next() else { return status.to_string(); };
    if !percent.ends_with('%') || !percent[..percent.len()-1].chars().all(|c| c.is_ascii_digit()) {
        return status.to_string();
    }
    let mut out = format!("{GREEN}{percent}{RESET}");
    for field in fields {
        out.push_str(" · ");
        if field.starts_with('↓') {
            out.push_str(&format!("{CYAN}{field}{RESET}"));
            continue;
        }
        if let Some((downloaded, total)) = field.split_once('/') {
            if !downloaded.contains(' ') {
                out.push_str(&format!("{GREEN}{downloaded}{RESET}/{total}"));
                continue;
            }
        }
        out.push_str(field);
    }
    out
}

impl BatchDisplay {
    fn new(plans: &[Plan]) -> Self {
        let terminal = std::io::stdout().is_terminal();
        let colors = terminal && vt_processing_enabled();
        let multi = terminal.then(|| MultiProgress::with_draw_target(ProgressDrawTarget::stdout()));
        let mut bars = Vec::new();
        let mut views = Vec::new();
        for (id, plan) in plans.iter().enumerate() {
            views.push(BatchView { name: batch_name(&plan.url), status: "в очереди".into(), last_printed: None });
            if let Some(multi) = &multi {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_style(ProgressStyle::with_template("{spinner:.green} {wide_msg}").expect("valid style"));
                bar.set_message(Self::line(id, plans.len(), &views[id], colors));
                bar.enable_steady_tick(Duration::from_millis(120));
                bars.push(bar);
            } else {
                outln(Self::line(id, plans.len(), &views[id], false));
            }
        }
        Self { multi, bars, views, colors }
    }

    fn line(id: usize, total: usize, view: &BatchView, colors: bool) -> String {
        let status = if colors { highlight_progress(&view.status) } else { view.status.clone() };
        format!("[{} / {}] {} — {}", id + 1, total, clip(&view.name, 38), status)
    }

    fn update(&mut self, id: usize, status: Option<String>, name: Option<String>) {
        let total = self.views.len();
        let view = &mut self.views[id];
        if let Some(name) = name { view.name = name; }
        if let Some(status) = status {
            // The generic allocation notice can arrive from stderr after a
            // more useful byte counter on stdout; don't overwrite the latter.
            if status != "выделение места на диске…" || !view.status.contains("выделение места ") {
                view.status = status;
            }
        }
        let line = Self::line(id, total, view, self.colors);
        if self.multi.is_some() {
            self.bars[id].set_message(line);
        } else if view.last_printed.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            outln(line);
            view.last_printed = Some(Instant::now());
        }
    }

    fn diagnostic(&self, id: usize, text: &str) {
        // YM metadata comes directly from the network rather than a sanitized
        // child pipe. Never let track titles inject terminal ESC/OSC sequences.
        let safe = engines::sanitize_child_output(text);
        let line = format!("[{} / {}] {}: {safe}", id + 1, self.views.len(), clip(&self.views[id].name, 38));
        if let Some(multi) = &self.multi {
            let _ = multi.println(line);
        } else {
            outln(line);
        }
    }

    fn finished(&mut self, id: usize, status: String) {
        self.views[id].status = status;
        let line = Self::line(id, self.views.len(), &self.views[id], self.colors);
        if self.multi.is_some() {
            self.bars[id].finish_with_message(line);
        } else {
            outln(line);
        }
    }
}

fn read_batch_pipe(
    reader: impl std::io::Read,
    id: usize,
    aria2: bool,
    tx: &mpsc::Sender<BatchEvent>,
    auth: &AtomicBool,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut last_progress = None::<Instant>;
    let mut last_file = None::<String>;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let decoded_bytes = engines::decode_child_bytes(&buf);
        let decoded = clean_loader_text(&decoded_bytes);
        let mut latest = None;
        for line in decoded.lines().map(str::trim).filter(|s| !s.is_empty()) {
            if engines::looks_like_auth(line) { auth.store(true, Ordering::Relaxed); }
            if line.contains("Download Progress Summary as of")
                || line.chars().all(|c| c == '-' || c == '=')
            { continue; }
            if line.starts_with("FILE:") && line.contains("[MEMORY][METADATA]") {
                latest = Some("получение метаданных торрента…".into());
            } else if let Some(name) = engines::aria2_name_from_file(line) {
                if last_file.as_deref() != Some(&name) {
                    last_file = Some(name.clone());
                    let _ = tx.send(BatchEvent::Name(id, name));
                }
            } else if let Some(stat) = engines::aria2_stat(line) {
                latest = Some(stat);
            } else if engines::parse_progress(line).is_some() {
                latest = Some(clip(line, 90));
            } else if line.contains("Allocating disk space") {
                let _ = tx.send(BatchEvent::Progress(id, "выделение места на диске…".into()));
            } else if !line.starts_with("[#") && !line.starts_with("[FileAlloc:") {
                // Нижний регистр не считаем заранее: для yt-dlp он не нужен, а
                // для aria2 большинство строк ни одного из слов не содержит.
                let worth_reporting = !aria2
                    || ["error", "warning", "failed", "aborted", "exception"]
                        .iter()
                        .any(|hint| engines::contains_ignore_ascii_case(line, hint));
                if worth_reporting {
                    let _ = tx.send(BatchEvent::Line(id, line.to_string()));
                }
            }
        }
        if let Some(stat) = latest {
            if last_progress.is_none_or(|t| t.elapsed() >= Duration::from_millis(400)) {
                last_progress = Some(Instant::now());
                let _ = tx.send(BatchEvent::Progress(id, stat));
            }
        }
    }
}

fn run_batch_job(
    id: usize,
    plan: &Plan,
    tc: &Toolchain,
    tx: &mpsc::Sender<BatchEvent>,
) -> RunResult {
    let url = match validate_url(&plan.url) {
        Ok(url) => url,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(id, format!("✘ {e}")));
            return RunResult { code: 2, auth_hint: false };
        }
    };
    let job = Job {
        engine: plan.engine.clone(),
        url,
        out_dir: PathBuf::from(&plan.out_dir),
        fmt: plan.fmt.clone(),
        cookies_browser: plan.cookies_browser.clone(),
    };
    for warn in preflight_warning(&job) {
        let _ = tx.send(BatchEvent::Line(id, format!("⚠ {warn}")));
    }
    let cmd = match cli_command(&job, tc, plan.no_continue, true) {
        Ok(cmd) => cmd,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(id, format!("✘ {e}")));
            return RunResult { code: 2, auth_hint: false };
        }
    };
    let child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(
                id,
                format!("✘ Не удалось запустить загрузчик: {e}"),
            ));
            return RunResult { code: 127, auth_hint: false };
        }
    };
    let auth = Arc::new(AtomicBool::new(false));
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let err_tx = tx.clone();
    let err_auth = auth.clone();
    let aria2 = job.engine == "aria2";
    let err_reader = std::thread::spawn(move || {
        read_batch_pipe(stderr, id, aria2, &err_tx, &err_auth)
    });
    read_batch_pipe(stdout, id, aria2, tx, &auth);
    let status = child.wait();
    let _ = err_reader.join();
    RunResult {
        code: status.map(|s| s.code().unwrap_or(130)).unwrap_or(127),
        auth_hint: auth.load(Ordering::Relaxed),
    }
}

/// Keep at most `jobs` independent loader processes alive, and serialize all
/// terminal output and config writes on this (main) thread. Each worker owns
/// a different Job and pipe pair; one noisy torrent cannot overwrite another
/// torrent's progress or cookie hint.
fn run_batch(plans: &[Plan], cfg: &mut Config, tc: &Toolchain, jobs: usize) -> Vec<RunResult> {
    run_batch_with(plans, cfg, tc, jobs, run_batch_job)
}

fn run_batch_with(
    plans: &[Plan],
    cfg: &mut Config,
    tc: &Toolchain,
    jobs: usize,
    run: impl Fn(usize, &Plan, &Toolchain, &mpsc::Sender<BatchEvent>) -> RunResult + Sync,
) -> Vec<RunResult> {
    let queue = Arc::new(Mutex::new((0..plans.len()).collect::<VecDeque<_>>()));
    let (tx, rx) = mpsc::channel();
    let mut results: Vec<Option<RunResult>> = (0..plans.len()).map(|_| None).collect();
    let mut missing_control = vec![false; plans.len()];
    let mut display = BatchDisplay::new(plans);
    std::thread::scope(|scope| {
        let run = &run;
        for _ in 0..jobs.min(plans.len()) {
            let queue = queue.clone();
            let tx = tx.clone();
            scope.spawn(move || loop {
                let Some(id) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                let plan = &plans[id];
                let _ = tx.send(BatchEvent::Progress(id, "подключение…".to_string()));
                let result = run(id, plan, tc, &tx);
                let _ = tx.send(BatchEvent::Finished(
                    id,
                    plan.url.clone(),
                    plan.out_dir.clone(),
                    result,
                ));
            });
        }
        drop(tx);
        for event in rx {
            match event {
                BatchEvent::Line(id, line) => {
                    if plans[id].engine == "aria2" && engines::aria2_missing_control(&line) {
                        if !missing_control[id] {
                            missing_control[id] = true;
                            display.diagnostic(id, engines::ARIA2_MISSING_CONTROL_HINT);
                        }
                    } else if plans[id].engine != "aria2"
                        || !(line.contains("Exception caught") || line.starts_with("(OK):")
                            || line.starts_with("If there are any errors")) {
                        display.diagnostic(id, &line);
                    }
                }
                BatchEvent::Name(id, name) => display.update(id, None, Some(name)),
                BatchEvent::Progress(id, line) => display.update(id, Some(line), None),
                BatchEvent::Finished(id, url, dir, result) => {
                    if result.code == 0 {
                        cfg.remember_url(&url);
                        cfg.remember_dir(&dir);
                        cfg.save();
                        let dir = clip(&engines::sanitize_child_output(&dir), 90);
                        display.finished(id, format!("✔ Готово: {dir}"));
                    } else if missing_control[id] {
                        display.finished(id, "✘ Файлы уже есть, но нет .aria2 — выберите пустую папку".into());
                    } else {
                        display.finished(id, format!("✘ Ошибка (код {}). См. сообщения выше.", result.code));
                    }
                    results[id] = Some(result);
                }
            }
        }
    });
    results
        .into_iter()
        .map(|r| r.unwrap_or(RunResult { code: 127, auth_hint: false }))
        .collect()
}

fn batch_exit_code(results: &[RunResult]) -> i32 {
    results.iter().find(|r| r.code != 0).map_or(0, |r| r.code)
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

fn split_links(text: &str) -> Result<Vec<String>, &'static str> {
    let mut urls = Vec::new();
    let mut current = String::new();
    let mut quoted = None;
    for c in text.chars() {
        match (quoted, c) {
            (Some(q), ch) if q == ch => quoted = None,
            (None, '\'' | '"') => quoted = Some(c),
            (None, ch) if ch.is_whitespace() => {
                if !current.is_empty() { urls.push(std::mem::take(&mut current)); }
            }
            _ => current.push(c),
        }
    }
    if quoted.is_some() { return Err("Не закрыты кавычки вокруг ссылки или пути к .torrent."); }
    if !current.is_empty() { urls.push(current); }
    Ok(urls)
}

fn ask_links(cfg: &Config) -> Option<Vec<String>> {
    loop {
        let text = Text::new("Ссылки через пробел (Enter — история):").prompt().ok()?;
        if !text.trim().is_empty() {
            match split_links(&text) {
                Ok(urls) if !urls.is_empty() => return Some(urls),
                // Quotes/whitespace only: nothing to download - ask again
                // instead of "succeeding" with a zero-URL queue.
                Ok(_) => continue,
                Err(e) => { errln(format!("✘ {e}")); continue; }
            }
        }
        if cfg.urls.is_empty() { return None; }
        let mut choices = vec![LinkChoice::New];
        choices.extend(cfg.urls.iter().take(6).cloned().map(LinkChoice::Existing));
        let pick = Select::new("Последние ссылки:", choices)
            .without_filtering()
            .with_help_message(SELECT_HELP_PLAIN)
            .prompt()
            .ok()?;
        if let LinkChoice::Existing(url) = pick { return Some(vec![url]); }
    }
}

/// Engines offered for a URL: every toolchain-backed engine (yt-dlp/aria2),
/// with the auto-detected guess moved to front.
fn engine_offer(tc: &Toolchain, url: &str) -> Vec<&'static str> {
    let mut list: Vec<&'static str> = Vec::new();
    if tc.yt_dlp.is_some() {
        list.push("yt-dlp");
    }
    if tc.aria2c.is_some() {
        list.push("aria2");
    }
    let guess = detect_engine(url);
    if let Some(pos) = list.iter().position(|e| *e == guess) {
        list.remove(pos);
        list.insert(0, guess);
    }
    list
}

fn ask_engine(url: &str, tc: &Toolchain) -> Option<String> {
    let list = engine_offer(tc, url);
    if list.is_empty() {
        return None;
    }
    if list.len() == 1 {
        return Some(list[0].to_string());
    }
    let label_of = |e: &str| -> String {
        ENGINE_LABELS
            .iter()
            .find(|(k, _)| *k == e)
            .map(|(_, l)| l.to_string())
            .unwrap_or_else(|| e.to_string())
    };
    struct EngineOpt(&'static str, String);
    impl fmt::Display for EngineOpt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.1)
        }
    }
    let choices: Vec<EngineOpt> = list
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let label = if i == 0 {
                format!("{}  (рекомендуется)", label_of(e))
            } else {
                label_of(e)
            };
            EngineOpt(e, label)
        })
        .collect();
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
            // File names are remote-controlled (archive contents!): never let
            // them inject raw escape sequences into the terminal menu.
            DirEntryChoice::Select(p) => {
                write!(f, "✓ Выбрать эту папку ({})", clip(&p.display().to_string(), 90))
            }
            DirEntryChoice::Up => write!(f, "↑ Наверх"),
            // Honest wording: the list is alphabetical and the tail is never
            // rendered - "поднимитесь выше" could not reveal it.
            DirEntryChoice::More(n) => write!(
                f,
                "… показаны первые {MAX_BROWSE_ENTRIES} папок по алфавиту (всего {n})"
            ),
            DirEntryChoice::Down(p) => {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                write!(f, "📁 {}", clip(&name, 60))
            }
        }
    }
}

fn browse_dir(start: PathBuf) -> Option<String> {
    let fallback = snatch_rs::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut current = if !is_unc_path(&start) && start.exists() { start } else { fallback };
    if is_unc_path(&current) { return None; }
    loop {
        let all_dirs = safe_read_dirs(&current);
        let shown = all_dirs.len().min(MAX_BROWSE_ENTRIES);
        let mut choices = vec![DirEntryChoice::Select(current.clone()), DirEntryChoice::Up];
        choices.extend(all_dirs[..shown].iter().cloned().map(DirEntryChoice::Down));
        if all_dirs.len() > MAX_BROWSE_ENTRIES {
            choices.push(DirEntryChoice::More(all_dirs.len()));
        }
        let ans = Select::new(&format!("Папка: {}", clip(&current.display().to_string(), 90)), choices).with_help_message(SELECT_HELP).prompt().ok()?;
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
    if !cfg.last_dir.is_empty() && !is_unc_path(Path::new(&cfg.last_dir))
        && Path::new(&cfg.last_dir).exists() {
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

/// The plan summary line shown before starting the whole queue.
fn confirm_message(engine: &str, fmt: &str, out_dir: &str, cookies_browser: Option<&str>) -> String {
    match engine {
        "yt-dlp" => {
            let label = FORMATS.iter().find(|(k, _)| *k == fmt).map(|(_, l)| *l).unwrap_or(fmt);
            let mut m = format!(" yt-dlp · {label} · → {}", clip(out_dir, 40));
            if let Some(b) = cookies_browser {
                m.push_str(&format!(" · 🍪 куки: {b}"));
            }
            m
        }
        _ => format!(" aria2c → {}", clip(out_dir, 40)),
    }
}

fn queue_summary(plans: &[Plan], jobs: u8) -> String {
    let mut summary = format!("Задач: {} · одновременно: {}", plans.len(), plans.len().min(usize::from(jobs)));
    for (i, plan) in plans.iter().enumerate() {
        let cookies = if plan.engine == "yt-dlp" { plan.cookies_browser.as_deref() } else { None };
        summary.push_str(&format!("\n  {}/{}. {}\n       {}", i + 1, plans.len(), clip(&plan.url, 60),
            confirm_message(&plan.engine, &plan.fmt, &plan.out_dir, cookies).trim()));
    }
    summary
}

fn collect(tc: &Toolchain, url: String, out_dir: &str, cookies_browser: Option<String>) -> Option<Plan> {
    if url.is_empty() {
        return None;
    }
    let engine = ask_engine(&url, tc)?;
    let fmt = if engine == "yt-dlp" { ask_format()? } else { "best".to_string() };
    Some(Plan { url, engine, fmt, out_dir: out_dir.to_string(), cookies_browser, no_continue: false })
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let mut cfg = Config::load();

    if let Some(browser) = &args.cookies_browser {
        if let Err(e) = validate_cookies_browser(browser) {
            errln(format!("✘ {e}"));
            return exit_code(2);
        }
    }

    if args.clear_history {
        cfg.clear_history();
        cfg.save();
        outln("История очищена.");
        return std::process::ExitCode::SUCCESS;
    }

    if args.install_tools {
        #[cfg(not(windows))]
        {
            errln("Автоустановка загрузчиков поддерживается только на Windows.");
            return exit_code(2);
        }
        #[cfg(windows)]
        {
            let dir = bootstrap_dir();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                errln(format!("✘ Не удалось создать {}: {e}", dir.display()));
                return exit_code(2);
            }
            let mut failed = false;
            for name in ["yt-dlp", "aria2c"] {
                let result = if name == "yt-dlp" { install_yt_dlp(&dir) } else { install_aria2(&dir) };
                match result {
                    Ok(path) => outln(format!("✔ {name}: {}", path.display())),
                    Err(e) => { errln(format!("✘ {name}: {e}")); failed = true; }
                }
            }
            return exit_code(if failed { 1 } else { 0 });
        }
    }

    if args.yes {
        let code = match plans_from_args(&args) {
            Some(plans) if plans.len() == 1 => download(&plans[0], &mut cfg, None).code,
            Some(plans) => {
                let tc = Toolchain::discover();
                batch_exit_code(&run_batch(&plans, &mut cfg, &tc, usize::from(args.jobs)))
            }
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
        errln("✘ Не найдено загрузчиков. Установи через snatch --install-tools или winget.");
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
    // Merge the tagline with the Telegram link on one line instead of a
    // separate "✈ https://..." line below it - saves a line of banner
    // height. Keep the "https://" scheme here (unlike the GUI's shortened
    // hyperlink label): most terminals only auto-linkify a bare URL when it
    // has a scheme, so a trimmed "t.me/rercon" wouldn't be clickable.
    // trim_matches только переносы, не trim(): trim() съел бы ведущий пробел первой
    // строки ASCII-арта (" ____ …") и весь баннер уезжал бы на символ влево.
    let banner = BANNER.trim_matches(['\r', '\n']);
    let (art, tagline) = banner.rsplit_once('\n').unwrap_or(("", banner));
    outln(art);
    outln(format!("{tagline} | {TELEGRAM_URL}"));

    let mut preset_urls = args.urls.clone();
    loop {
        let urls = if preset_urls.is_empty() {
            match ask_links(&cfg) {
                Some(urls) => urls,
                None => { outln("Отменено."); return exit_code(130); }
            }
        } else {
            std::mem::take(&mut preset_urls)
        };
        let out_dir = match &args.output {
            Some(o) => o.clone(),
            None => match ask_dir(&cfg) {
                Some(d) => d,
                None => {
                    outln("Отменено.");
                    return exit_code(130);
                }
            },
        };
        let total = urls.len();
        let mut plans = Vec::with_capacity(total);
        for (i, url) in urls.into_iter().enumerate() {
            outln(format!("\nНастройка {}/{}: {}", i + 1, total, clip(&url, 65)));
            let Some(mut plan) = collect(&tc, url, &out_dir, args.cookies_browser.clone()) else {
                outln("Отменено.");
                return exit_code(130);
            };
            plan.no_continue = args.no_continue;
            plans.push(plan);
        }
        outln(queue_summary(&plans, args.jobs));
        if !Confirm::new("Скачать всё?").with_default(true).prompt().unwrap_or(false) {
            outln("Отменено.");
            return exit_code(130);
        }
        let code = if plans.len() == 1 {
            let result = download(&plans[0], &mut cfg, Some(&tc));
            retry_with_cookies(plans.remove(0), result, &mut cfg, &tc).code
        } else {
            let mut results = run_batch(&plans, &mut cfg, &tc, usize::from(args.jobs));
            for (plan, result) in plans.into_iter().zip(&mut results) {
                if should_offer_cookies(&plan, result.code, result.auth_hint) {
                    *result = retry_with_cookies(
                        plan,
                        RunResult { code: result.code, auth_hint: result.auth_hint },
                        &mut cfg,
                        &tc,
                    );
                }
            }
            batch_exit_code(&results)
        };
        match Confirm::new("\nСкачать ещё что-нибудь?").with_default(false).prompt() {
            Ok(true) => continue,
            _ => return exit_code(code),
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
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn banner_preserves_ascii_art_indent() {
        let first = BANNER.trim_matches(['\r', '\n']).lines().next().unwrap();
        assert!(first.starts_with(" ____"), "{first:?}");
        assert!(BANNER.contains("ultimate combine"));
    }

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
            urls: url.into_iter().map(String::from).collect(),
            output: output.map(String::from),
            engine: None,
            format: None,
            yes: true,
            jobs: 3,
            cookies_browser: cookies_browser.map(String::from),
            clear_history: false,
            install_tools: false,
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

    #[test]
    fn multiple_urls_select_engines_independently() {
        let mut args = args_with(Some("https://video.example/watch?id=1"), Some("downloads"), None);
        args.urls.push("magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4".into());
        args.urls.push("https://host/file.iso".into());
        let plans = plans_from_args(&args).unwrap();
        assert_eq!(plans.len(), 3);
        assert_eq!(plans.iter().map(|p| p.engine.as_str()).collect::<Vec<_>>(), ["yt-dlp", "aria2", "aria2"]);
        assert!(plans.iter().all(|p| p.out_dir == "downloads"));
        assert_eq!(batch_exit_code(&[RunResult { code: 0, auth_hint: false }, RunResult { code: 1, auth_hint: false }]), 1);
    }

    #[test]
    fn interactive_links_split_preserves_magnets_and_quoted_torrent_paths() {
        let links = split_links("https://host/a.zip magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4&dn=Game%20One \"C:\\My Torrents\\Game 2.torrent\"").unwrap();
        assert_eq!(links.len(), 3);
        assert!(links[1].contains("&dn=Game%20One"));
        assert_eq!(links[2], "C:\\My Torrents\\Game 2.torrent");
        assert!(split_links("'C:\\My Torrents\\Game.torrent").is_err());
    }

    #[test]
    fn interactive_queue_summary_counts_and_lists_each_engine() {
        let mut args = args_with(Some("https://host/a.zip"), Some("Downloads"), None);
        args.urls.extend(["https://example.com/watch?v=2".into(), "https://host/b.zip".into()]);
        let plans = plans_from_args(&args).unwrap();
        let text = queue_summary(&plans, 2);
        assert!(text.contains("Задач: 3 · одновременно: 2"));
        assert!(text.contains("1/3.") && text.contains("2/3.") && text.contains("3/3."));
        assert_eq!(text.matches("aria2c →").count(), 2);
        assert_eq!(text.matches("yt-dlp ·").count(), 1);
    }

    #[test]
    fn cli_progress_options_do_not_change_shared_gui_builder() {
        let dir = std::env::temp_dir().join(format!("snatch-cli-opts-{}", std::process::id()));
        let tc = Toolchain { yt_dlp: None, aria2c: Some(PathBuf::from("aria2c")) };
        let job = Job {
            engine: "aria2".into(),
            url: "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4".into(),
            out_dir: dir.clone(),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let cmd = cli_command(&job, &tc, false, true).unwrap();
        assert_eq!(cmd[cmd.len() - 2], "--");
        assert!(cmd.iter().any(|arg| arg == "--summary-interval=2"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn batch_torrent_output_keeps_names_stats_and_allocation_without_terminal_noise() {
        let (tx, rx) = mpsc::channel();
        let auth = AtomicBool::new(false);
        let output = b"09/27 03:10 [\x1b[1;32mNOTICE\x1b[0m] Allocating disk space. See --file-allocation option\n\
                       FILE: C:/Downloads/Cuphead_1.3.9/setup-1.bin (7more)\n\
                       *** Download Progress Summary as of now ***\n\
                       [#bc1f6e 0B/6.8GiB(0%) CN:0 SD:0 DL:0B] [FileAlloc:#bc1f6e 1.1GiB/3.3GiB(33%)]\n";
        read_batch_pipe(&output[..], 0, true, &tx, &auth);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[1], BatchEvent::Name(0, name) if name == "Cuphead_1.3.9"));
        assert!(matches!(&events[2], BatchEvent::Progress(0, line) if line.contains("выделение места 1.1GiB/3.3GiB (33%)")));
        assert!(!events.iter().any(|e| matches!(e, BatchEvent::Line(..))));
        assert!(!clean_loader_text("\x1b[1;32mNOTICE\x1b[0m").contains("[1;32m"));
        let stat = engines::aria2_stat("[#50e13e 880KiB/0.9MiB(88%) CN:1 SD:4 DL:812KiB UL:14KiB]").unwrap();
        assert!(stat.contains("88% · 880KiB/0.9MiB · ↓812KiB/с · сиды 4 · соединения 1"), "{stat}");
    }

    #[test]
    fn batch_colors_only_tty_progress_numbers() {
        let view = BatchView {
            name: "movie.mkv".into(),
            status: "42% · 420MiB/1GiB · ↓3MiB/с · сиды 8".into(),
            last_printed: None,
        };
        let colored = BatchDisplay::line(0, 2, &view, true);
        assert!(colored.contains("\x1b[92m42%\x1b[0m"));
        assert!(colored.contains("\x1b[92m420MiB\x1b[0m/1GiB"));
        assert!(colored.contains("\x1b[96m↓3MiB/с\x1b[0m"));
        let plain = BatchDisplay::line(0, 2, &view, false);
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains("42% · 420MiB/1GiB · ↓3MiB/с"));
    }

    #[test]
    fn batch_reports_each_failure_in_original_url_order() {
        let mut args = args_with(Some("not-a-url"), Some("unused"), None);
        args.urls.push("also-not-a-url".into());
        let plans = plans_from_args(&args).unwrap();
        let mut cfg = empty_cfg();
        let tc = Toolchain { yt_dlp: None, aria2c: None };
        let results = run_batch(&plans, &mut cfg, &tc, 2);
        assert_eq!(results.iter().map(|r| r.code).collect::<Vec<_>>(), [2, 2]);
        assert_eq!(batch_exit_code(&results), 2);
        assert!(cfg.urls.is_empty());
    }

    #[test]
    fn batch_respects_worker_limit_and_keeps_results_by_job() {
        let mut args = args_with(Some("https://host/one.zip"), Some("unused"), None);
        args.urls.extend(["https://host/two.zip".into(), "https://host/three.zip".into()]);
        let plans = plans_from_args(&args).unwrap();
        let mut cfg = empty_cfg();
        let tc = Toolchain { yt_dlp: None, aria2c: None };
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let results = run_batch_with(&plans, &mut cfg, &tc, 2, |id, _, _, _| {
            let n = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(n, Ordering::SeqCst);
            let start = Instant::now();
            while peak.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(20));
            active.fetch_sub(1, Ordering::SeqCst);
            RunResult { code: id as i32 + 1, auth_hint: false }
        });
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(results.iter().map(|r| r.code).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn jobs_flag_rejects_zero_and_more_than_sixteen() {
        assert!(Args::try_parse_from(["snatch", "--jobs", "0"]).is_err());
        assert!(Args::try_parse_from(["snatch", "--jobs", "17"]).is_err());
        assert_eq!(Args::try_parse_from(["snatch", "-j", "2"]).unwrap().jobs, 2);
    }

    // Bad URLs must fail before the potentially slow PATH/winget discovery.
    #[test]
    fn download_bad_url_fails_without_valid_toolchain() {
        let mut plan = base_plan();
        plan.url = "https://[::1".to_string();
        let mut cfg = empty_cfg();
        assert_eq!(download_with_discovery(&plan, &mut cfg, None,
            || panic!("bad URL must not discover tools")).code, 2);
    }

    // -- retry_with_cookies_inner (test_retry_with_cookies_*, test_no_retry_without_auth_hint, retry_second_decline_returns_failure) --

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
    fn retry_second_decline_returns_failure() {
        // The loop is user-driven (no retry cap): the first offer is accepted
        // (login) and the retried download fails again; declining the second
        // offer must return that failure as-is.
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

    // -- engine_offer / ask_engine --

    #[test]
    fn engine_offer_puts_guess_first() {
        let tc = Toolchain { yt_dlp: Some(PathBuf::from("y")), aria2c: Some(PathBuf::from("a")) };
        assert_eq!(engine_offer(&tc, "https://youtube.com/watch?v=1"), ["yt-dlp", "aria2"]);
        assert_eq!(engine_offer(&tc, "https://host/f.zip"), ["aria2", "yt-dlp"]);
        let none = Toolchain { yt_dlp: None, aria2c: None };
        assert!(engine_offer(&none, "https://x").is_empty());
        let one = Toolchain { yt_dlp: Some(PathBuf::from("y")), aria2c: None };
        assert_eq!(engine_offer(&one, "https://youtube.com/watch?v=1"), ["yt-dlp"]);
    }

    #[test]
    fn ask_engine_single_available_needs_no_terminal() {
        let tc = Toolchain { yt_dlp: Some(PathBuf::from("y")), aria2c: None };
        assert_eq!(ask_engine("https://youtube.com/watch?v=1", &tc).as_deref(), Some("yt-dlp"));
        let tc = Toolchain { yt_dlp: None, aria2c: None };
        assert_eq!(ask_engine("https://x", &tc), None);
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
