use super::*;

/// `--engine`: only the identifiers [`Engine::as_str`] spells, parsed into the enum.
pub(super) fn engine_parser() -> impl clap::builder::TypedValueParser<Value = Engine> {
    clap::builder::PossibleValuesParser::new(Engine::ALL.map(Engine::as_str))
        .map(|name| Engine::parse(&name).expect("clap only passes known engine names"))
}

/// `--format`: only the identifiers [`Format::as_str`] spells, parsed into the enum.
pub(super) fn format_parser() -> impl clap::builder::TypedValueParser<Value = Format> {
    clap::builder::PossibleValuesParser::new(Format::ALL.map(Format::as_str))
        .map(|name| Format::parse(&name).expect("clap only passes known format names"))
}

#[derive(Parser)]
#[command(
    name = "snatch",
    disable_help_flag = true,
    disable_version_flag = true,
    version = APP_VERSION,
    about = tr!("SNATCH — a lightweight downloader: yt-dlp + aria2c in one CLI.", "SNATCH — лёгкий загрузчик: yt-dlp + aria2c в одном CLI.")
)]
pub(super) struct Args {
    #[arg(long, value_name = "en|ru", value_parser = Language::parse,
            help = tr!("Set and remember the interface language", "Выбрать и сохранить язык интерфейса"))]
    pub(super) lang: Option<Language>,
    #[arg(short = 'h', long, help = tr!("Print help", "Показать справку"))]
    pub(super) help: bool,
    #[arg(short = 'V', long, help = tr!("Print version", "Показать версию"))]
    pub(super) version: bool,
    #[arg(value_name = "URL")]
    #[arg(help = tr!("Links; multiple links are downloaded concurrently", "Ссылки; несколько ссылок скачиваются параллельно"))]
    pub(super) urls: Vec<String>,
    #[arg(short = 'o', long)]
    #[arg(help = tr!("Download folder", "Папка сохранения (без вопросов)"))]
    pub(super) output: Option<String>,
    #[arg(short = 'e', long, value_parser = engine_parser())]
    #[arg(help = tr!("Downloader (default: automatic)", "Движок (по умолчанию — авто-подсказка)"))]
    pub(super) engine: Option<Engine>,
    #[arg(short = 'f', long, value_parser = format_parser())]
    #[arg(help = tr!("yt-dlp format", "Формат для yt-dlp"))]
    pub(super) format: Option<Format>,
    #[arg(short = 'y', long)]
    #[arg(help = tr!("Skip prompts (requires a URL and --output)", "Пропустить все вопросы (нужны url и output)"))]
    pub(super) yes: bool,
    #[arg(short = 'j', long, default_value_t = engines::DEFAULT_JOBS, value_parser = clap::value_parser!(u8).range(1..=16))]
    #[arg(help = tr!("Concurrent downloads for multiple links (1–16)", "Одновременных загрузок при нескольких ссылках (1–16)"))]
    pub(super) jobs: u8,
    #[arg(long = "cookies-from-browser", value_name = "BROWSER")]
    #[arg(help = tr!("Use a browser session (e.g. chrome or firefox; profile: chrome:Profile 1)", "Откуда взять куки (chrome, firefox, edge, brave, opera, vivaldi, safari, chromium, whale) — для \"Sign in to confirm you're not a bot\" и возрастных ограничений"))]
    pub(super) cookies_browser: Option<String>,
    #[arg(long)]
    #[arg(help = tr!("Clear recent links and folders", "Забыть последние ссылки и папки"))]
    pub(super) clear_history: bool,
    #[arg(long)]
    #[arg(help = tr!("Install yt-dlp, aria2c, ffmpeg and Deno in the SNATCH tools folder (Windows)", "Установить загрузчики и помощники (yt-dlp, aria2c, ffmpeg, Deno) в собственную папку SNATCH (Windows)"))]
    pub(super) install_tools: bool,
    #[arg(long)]
    #[arg(help = tr!("Restart an interrupted yt-dlp download from scratch instead of resuming", "Не докачивать прерванное (yt-dlp), начать файл с начала — лечит протухший .part («Invalid data» при склейке). В GUI такой повтор происходит автоматически."))]
    pub(super) no_continue: bool,
    #[arg(long)]
    #[arg(help = tr!("Download subtitles alongside the video (yt-dlp)", "Скачивать субтитры рядом с видео (yt-dlp)"))]
    pub(super) subs: bool,
    #[arg(long, value_name = "LANGS")]
    #[arg(help = tr!("Subtitle languages (default: ru,en)", "Языки субтитров, например ru,en (по умолчанию ru,en)"))]
    pub(super) sub_langs: Option<String>,
    #[arg(long)]
    #[arg(help = tr!("Download the entire playlist (default: single video)", "Скачивать плейлист целиком (по умолчанию — только одиночное видео)"))]
    pub(super) playlist: bool,
    #[arg(long, value_name = "ARGS")]
    #[arg(help = tr!("Extra yt-dlp command-line arguments", "Дополнительные аргументы yt-dlp, как в командной строке"))]
    pub(super) yt_dlp_args: Option<String>,
    #[arg(long, value_name = "ARGS")]
    #[arg(help = tr!("Extra aria2c arguments (safe options only)", "Дополнительные аргументы aria2c"))]
    pub(super) aria2_args: Option<String>,
}

#[derive(Clone)]
pub(super) struct Plan {
    pub(super) url: String,
    pub(super) download_name: Option<DownloadName>,
    pub(super) engine: Engine,
    pub(super) fmt: Format,
    pub(super) out_dir: String,
    pub(super) cookies_browser: Option<String>,
    pub(super) no_continue: bool,
    /// Torrent file selection plus the metadata fetched while listing;
    /// default = every file, aria2 fetches the metadata itself.
    pub(super) torrent: torrent::Choice,
    pub(super) extras: engines::RunExtras,
}

pub(super) fn extras_from_args(args: &Args) -> Result<engines::RunExtras, String> {
    Ok(engines::RunExtras {
        subs: args.subs,
        sub_langs: args.sub_langs.clone().unwrap_or_default(),
        playlist: args.playlist,
        extra_ytdlp: args
            .yt_dlp_args
            .as_deref()
            .map(engines::split_cli_args)
            .transpose()?
            .unwrap_or_default(),
        extra_aria2: args
            .aria2_args
            .as_deref()
            .map(engines::split_cli_args)
            .transpose()?
            .unwrap_or_default(),
    })
}

pub(super) fn plan_from_args(args: &Args) -> Option<Plan> {
    let (Some(url), Some(output)) = (args.urls.first(), args.output.as_ref()) else {
        errln(tr!(
            "The -y mode requires at least one URL and --output.",
            "Для режима -y нужны хотя бы одна ссылка и --output."
        ));
        return None;
    };
    let engine = args.engine.unwrap_or_else(|| detect_engine(url));
    let fmt = args.format.unwrap_or_default();
    let extras = match extras_from_args(args) {
        Ok(extras) => extras,
        Err(e) => {
            errln(format!("✘ {e}"));
            return None;
        }
    };
    Some(Plan {
        url: url.clone(),
        download_name: None,
        engine,
        fmt,
        out_dir: output.clone(),
        cookies_browser: args.cookies_browser.clone(),
        no_continue: args.no_continue,
        torrent: torrent::Choice::default(),
        extras,
    })
}

pub(super) fn plans_from_args(args: &Args) -> Option<Vec<Plan>> {
    let first = plan_from_args(args)?;
    let mut plans = vec![first];
    for url in args.urls.iter().skip(1) {
        plans.push(Plan {
            url: url.clone(),
            download_name: None,
            engine: args.engine.unwrap_or_else(|| detect_engine(url)),
            fmt: plans[0].fmt,
            out_dir: plans[0].out_dir.clone(),
            cookies_browser: plans[0].cookies_browser.clone(),
            no_continue: plans[0].no_continue,
            torrent: torrent::Choice::default(),
            extras: plans[0].extras.clone(),
        });
    }
    Some(plans)
}

pub(super) fn apply_link_info(plan: &mut Plan, info: LinkInfo, auto: bool) {
    if auto {
        plan.engine = info.engine(&plan.url);
    }
    if plan.engine == Engine::Aria2 {
        plan.download_name = info.name;
    }
}

/// A check that was needed but never reached the server is reported: the
/// engine and file name chosen then come from the URL alone.
pub(super) fn warn_unchecked_link(url: &str, info: &LinkInfo) {
    if links::needs_probe(&engines::clean_url(url)) && !info.checked {
        errln(tr!(
            "⚠ Could not check the link type (network) — the engine was picked from the address; choose another below if needed.",
            "⚠ Не удалось проверить ссылку (сеть) — движок выбран по адресу; при необходимости выберите другой ниже."
        ));
    }
}

/// Link probes in flight at once: enough to hide a slow host, few enough that
/// a pasted list of hundreds of links does not open hundreds of connections.
const PROBE_THREADS: usize = 8;

pub(super) fn prepare_plans(mut plans: Vec<Plan>, auto: bool) -> Vec<Plan> {
    // Each probe can take several seconds on a slow host; run a few side by
    // side instead of one after another. Results are applied (and warnings
    // printed) in the original order.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let infos: Mutex<Vec<Option<LinkInfo>>> =
        Mutex::new(plans.iter().map(|_| None).collect::<Vec<_>>());
    std::thread::scope(|scope| {
        for _ in 0..plans.len().min(PROBE_THREADS) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(plan) = plans.get(i) else {
                    break;
                };
                let info = ((auto || plan.engine == Engine::Aria2)
                    && validate_url(&plan.url).is_ok())
                .then(|| links::resolve(&engines::clean_url(&plan.url)));
                infos.lock().unwrap_or_else(|e| e.into_inner())[i] = info;
            });
        }
    });
    let infos = infos.into_inner().unwrap_or_else(|e| e.into_inner());
    for (plan, info) in plans.iter_mut().zip(infos) {
        if let Some(info) = info {
            warn_unchecked_link(&plan.url, &info);
            apply_link_info(plan, info, auto);
        }
    }
    plans
}

pub(super) fn cli_command(
    job: &Job,
    tc: &Toolchain,
    no_continue: bool,
    batch: bool,
    choice: &torrent::Choice,
    extras: &engines::RunExtras,
) -> Result<Vec<std::ffi::OsString>, String> {
    let mut cmd = engines::build_for_run_with(job, tc, choice, extras)?;
    if no_continue && job.engine == Engine::YtDlp {
        engines::insert_option(&mut cmd, "--no-continue");
    }
    // aria2 prints a newline-terminated progress summary even when stdout is
    // piped. The default interval (60s) makes magnet metadata look stuck.
    if job.engine == Engine::Aria2 {
        engines::insert_option(&mut cmd, "--summary-interval=2");
    } else if batch {
        engines::insert_option(&mut cmd, "--newline");
    }
    Ok(cmd)
}
