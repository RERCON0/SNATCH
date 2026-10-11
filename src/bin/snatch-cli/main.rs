//! `snatch` terminal entry point - was a Rust port of the retired Python
//! CLI's `cli.py`/`ui.py` (now the only CLI), built on the same shared
//! `snatch_rs` lib the GUI uses. The old Python-era `down` alias is gone:
//! it pointed at the exact same entry point and only doubled build/test time.

use snatch_rs::i18n::{self, Language};
use snatch_rs::{tr, tr_format, tr_write};
use std::collections::VecDeque;
use std::fmt;
use std::io::{BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use clap::builder::TypedValueParser;
use clap::{CommandFactory, Parser};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use inquire::{Confirm, Select, Text};

use snatch_rs::config::Config;
use snatch_rs::engines::{
    self, detect_engine, preflight_warning, reaches_unc, validate_cookies_browser, validate_url,
    Engine, Format, Job, RunResult, COOKIES_BROWSERS,
};
use snatch_rs::links::{self, DownloadName, LinkInfo};
#[cfg(windows)]
use snatch_rs::setup::{install_aria2, install_deno, install_ffmpeg, install_yt_dlp};
#[cfg(windows)]
use snatch_rs::tools::bootstrap_dir;
use snatch_rs::tools::Toolchain;
use snatch_rs::torrent;
use snatch_rs::ui::{clip, safe_read_dirs};
use snatch_rs::{errln, outln, BANNER, TELEGRAM_URL};

mod args;
mod batch;
mod display;
mod prompts;
mod retry;
mod torrent_pick;
use args::*;
use batch::*;
use display::*;
use prompts::*;
use retry::*;
use torrent_pick::*;

const APP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " — by rercon prod.");
const WINDOW_TITLE: &str = "\x1b]0;SNATCH — by rercon prod.\x07";
const MAX_BROWSE_ENTRIES: usize = 100;

const RETRY_CODE: i32 = 1;

fn main() -> std::process::ExitCode {
    let mut cfg = Config::load();
    i18n::set_language(cfg.language);
    let args = Args::parse();
    if let Some(language) = args.lang {
        i18n::set_language(language);
    }
    if args.help {
        let mut command = Args::command();
        if i18n::is_russian() {
            command = command
                .help_template("{about-with-newline}\nИспользование: {usage}\n\n{all-args}")
                .mut_args(|arg| {
                    let positional = arg.is_positional();
                    arg.help_heading(if positional {
                        "Аргументы"
                    } else {
                        "Опции"
                    })
                });
        }
        if let Err(e) = command.print_help() {
            errln(e.to_string());
            return exit_code(1);
        }
        outln("");
        return std::process::ExitCode::SUCCESS;
    }
    if args.version {
        outln(format!("snatch {APP_VERSION}"));
        return std::process::ExitCode::SUCCESS;
    }
    if let Some(language) = args.lang {
        cfg.set_language(language);
        if let Err(e) = cfg.try_save() {
            errln(e);
            return exit_code(1);
        }
        if args.urls.is_empty() && !args.install_tools && !args.clear_history && !args.yes {
            outln(tr_format!("Language: {}", "Язык: {}", language.label()));
            return std::process::ExitCode::SUCCESS;
        }
    }

    if let Some(browser) = &args.cookies_browser {
        if let Err(e) = validate_cookies_browser(browser) {
            errln(format!("✘ {e}"));
            return exit_code(2);
        }
    }

    if args.clear_history {
        cfg.clear_history();
        if let Err(e) = cfg.try_save() {
            errln(tr_format!(
                "✘ History was not cleared: {e}",
                "✘ История не очищена: {e}"
            ));
            return exit_code(1);
        }
        outln(tr!("History cleared.", "История очищена."));
        return std::process::ExitCode::SUCCESS;
    }

    if args.install_tools {
        #[cfg(not(windows))]
        {
            errln(tr!(
                "Automatic tool installation is only supported on Windows.",
                "Автоустановка загрузчиков поддерживается только на Windows."
            ));
            return exit_code(2);
        }
        #[cfg(windows)]
        {
            let dir = match bootstrap_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    errln(e);
                    return exit_code(2);
                }
            };
            if let Err(e) = std::fs::create_dir_all(&dir) {
                errln(tr_format!(
                    "✘ Could not create {}: {e}",
                    "✘ Не удалось создать {}: {e}",
                    dir.display()
                ));
                return exit_code(2);
            }
            let mut failed = false;
            for name in ["yt-dlp", "aria2c", "ffmpeg", "Deno"] {
                let result = match name {
                    "yt-dlp" => install_yt_dlp(&dir),
                    "aria2c" => install_aria2(&dir),
                    "ffmpeg" => install_ffmpeg(&dir),
                    _ => install_deno(&dir),
                };
                match result {
                    Ok(path) => outln(format!("✔ {name}: {}", path.display())),
                    Err(e) => {
                        errln(format!("✘ {name}: {e}"));
                        failed = true;
                    }
                }
            }
            return exit_code(if failed { 1 } else { 0 });
        }
    }

    if args.yes {
        let (code, cookies_hint) =
            match plans_from_args(&args).map(|plans| prepare_plans(plans, args.engine.is_none())) {
                Some(plans) if plans.len() == 1 => {
                    let result = download(&plans[0], &mut cfg, None);
                    (
                        result.code,
                        result.auth_hint
                            && plans[0].engine == Engine::YtDlp
                            && plans[0].cookies_browser.is_none(),
                    )
                }
                Some(plans) => {
                    let tc = Toolchain::discover();
                    let results = run_batch(&plans, &mut cfg, &tc, usize::from(args.jobs));
                    // -y cannot ask interactively; results all share the one
                    // --cookies-from-browser value, so "already using cookies"
                    // applies to the whole batch. Cookies are a yt-dlp remedy
                    // only - an aria2 failure must not advertise them.
                    let cookies_hint = plans.iter().zip(&results).any(|(plan, result)| {
                        result.auth_hint
                            && plan.engine == Engine::YtDlp
                            && plan.cookies_browser.is_none()
                    });
                    (batch_exit_code(&results), cookies_hint)
                }
                None => (2, false),
            };
        if cookies_hint && code != 0 {
            errln(tr!(
                "Tip: the site may require sign-in or rate-limit the requests (HTTP 429). Retry with `--cookies-from-browser chrome` — or run without -y to pick a browser interactively.",
                "Подсказка: сайту может требоваться вход или он ограничил запросы (429). Повторите с `--cookies-from-browser chrome` — либо запустите без -y и выберите браузер из списка."
            ));
        }
        return exit_code(code);
    }

    // Terminal check BEFORE Toolchain::discover(): when stdin/stdout are
    // piped there's nothing to prompt anyway, and discovery stats every PATH
    // entry - no reason to pay for that scan just to print the refusal.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        errln(
            tr!("✘ Interactive mode requires a terminal. Use Windows Terminal/cmd or -y with a URL and -o.", "✘ Интерактивный режим требует настоящий терминал. \
             Используй Windows Terminal/cmd или режим -y со ссылкой и -o."),
        );
        return exit_code(2);
    }

    let tc = Toolchain::discover();
    if tc.yt_dlp.is_none() && tc.aria2c.is_none() {
        errln(tr!(
            "✘ No downloaders found. Install with snatch --install-tools or winget.",
            "✘ Не найдено загрузчиков. Установи через snatch --install-tools или winget."
        ));
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
                None => {
                    outln(tr!("Canceled.", "Отменено."));
                    return exit_code(130);
                }
            }
        } else {
            std::mem::take(&mut preset_urls)
        };
        let out_dir = match &args.output {
            Some(o) => o.clone(),
            None => match ask_dir(&cfg) {
                Some(d) => d,
                None => {
                    outln(tr!("Canceled.", "Отменено."));
                    return exit_code(130);
                }
            },
        };
        let total = urls.len();
        let mut plans = Vec::with_capacity(total);
        for (i, url) in urls.into_iter().enumerate() {
            outln(tr_format!(
                "\nSetup {}/{}: {}",
                "\nНастройка {}/{}: {}",
                i + 1,
                total,
                clip(&url, 65)
            ));
            let Some(mut plan) = collect(
                &tc,
                url,
                &out_dir,
                args.cookies_browser.clone(),
                args.engine,
                args.format,
            ) else {
                outln(tr!("Canceled.", "Отменено."));
                return exit_code(130);
            };
            plan.no_continue = args.no_continue;
            plans.push(plan);
        }
        outln(queue_summary(&plans, args.jobs));
        if !Confirm::new(tr!("Download everything?", "Скачать всё?"))
            .with_default(true)
            .prompt()
            .unwrap_or(false)
        {
            outln(tr!("Canceled.", "Отменено."));
            return exit_code(130);
        }
        // Network metadata is fetched only after the whole queue is confirmed.
        let mut picked = Vec::with_capacity(plans.len());
        for mut plan in plans {
            match ask_torrent_files(&tc, &plan) {
                TorrentAsk::Use(choice) => plan.torrent = choice,
                TorrentAsk::Skip => {
                    outln(tr!("Link skipped.", "Ссылка пропущена."));
                    continue;
                }
                TorrentAsk::Cancel => {
                    outln(tr!("Canceled.", "Отменено."));
                    return exit_code(130);
                }
            }
            picked.push(plan);
        }
        let mut plans = picked;
        if plans.is_empty() {
            outln(tr!("Nothing to download.", "Нечего скачивать."));
            continue;
        }
        let extras = match extras_from_args(&args) {
            Ok(extras) => extras,
            Err(e) => {
                errln(format!("✘ {e}"));
                return exit_code(2);
            }
        };
        for plan in &mut plans {
            plan.extras = extras.clone();
        }
        let code = if plans.len() == 1 {
            let result = download(&plans[0], &mut cfg, Some(&tc));
            let plan = plans.remove(0);
            retry_with_cookies(plan, result, &mut cfg, &tc).code
        } else {
            let mut results = run_batch(&plans, &mut cfg, &tc, usize::from(args.jobs));
            for (plan, result) in plans.into_iter().zip(&mut results) {
                if should_offer_cookies(&plan, result.code, result.auth_hint) {
                    *result = retry_with_cookies(
                        plan.clone(),
                        RunResult {
                            code: result.code,
                            auth_hint: result.auth_hint,
                        },
                        &mut cfg,
                        &tc,
                    );
                }
            }
            batch_exit_code(&results)
        };
        match Confirm::new(tr!(
            "\nDownload something else?",
            "\nСкачать ещё что-нибудь?"
        ))
        .with_default(false)
        .prompt()
        {
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

#[cfg(test)]
mod tests;
