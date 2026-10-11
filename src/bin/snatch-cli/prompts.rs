use super::*;

// inquire's Select defaults to an English help line ("↑↓ to move, enter to
// select, type to filter") with no way to localize it globally - it's a
// per-prompt-type default text, not something RenderConfig covers - so every
// Select::new(...) below adds this explicitly.
pub(super) fn select_help() -> &'static str {
    tr!(
        "↑↓ to select, Enter to confirm, type to filter",
        "↑↓ — выбор, Enter — подтвердить, начните печатать — фильтр"
    )
}
// ask_link's list has no free-text item to filter toward (typing a URL just
// filters everything out, so Enter has nothing to submit and looks dead -
// this is the "нажимаю Enter, ноль эмоций" bug); its own help text drops the
// filter hint to match `.without_filtering()` below.
pub(super) fn select_help_plain() -> &'static str {
    tr!(
        "↑↓ to select, Enter to confirm",
        "↑↓ — выбор, Enter — подтвердить"
    )
}

// ---- interactive prompts (port of ui.py) -----------------------------------

pub(super) enum LinkChoice {
    New,
    Existing(String),
}

impl fmt::Display for LinkChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkChoice::New => tr_write!(f, "Enter a new link", "Вставить новую ссылку"),
            LinkChoice::Existing(u) => write!(f, "{}", clip(u, 70)),
        }
    }
}

pub(super) fn split_links(text: &str) -> Result<Vec<String>, &'static str> {
    let mut urls = Vec::new();
    let mut current = String::new();
    let mut quoted = None;
    for c in text.chars() {
        match (quoted, c) {
            (Some(q), ch) if q == ch => quoted = None,
            (None, '\'' | '"') => quoted = Some(c),
            (None, ch) if ch.is_whitespace() => {
                if !current.is_empty() {
                    urls.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if quoted.is_some() {
        return Err(tr!(
            "Unclosed quote around a URL or .torrent path.",
            "Не закрыты кавычки вокруг ссылки или пути к .torrent."
        ));
    }
    if !current.is_empty() {
        urls.push(current);
    }
    Ok(urls)
}

pub(super) fn ask_links(cfg: &Config) -> Option<Vec<String>> {
    loop {
        let text = Text::new(tr!(
            "Space-separated links (Enter for history):",
            "Ссылки через пробел (Enter — история):"
        ))
        .prompt()
        .ok()?;
        if !text.trim().is_empty() {
            match split_links(&text) {
                Ok(urls) if !urls.is_empty() => return Some(urls),
                // Quotes/whitespace only: nothing to download - ask again
                // instead of "succeeding" with a zero-URL queue.
                Ok(_) => continue,
                Err(e) => {
                    errln(format!("✘ {e}"));
                    continue;
                }
            }
        }
        if cfg.urls.is_empty() {
            return None;
        }
        let mut choices = vec![LinkChoice::New];
        choices.extend(cfg.urls.iter().take(6).cloned().map(LinkChoice::Existing));
        let pick = Select::new(tr!("Recent links:", "Последние ссылки:"), choices)
            .without_filtering()
            .with_help_message(select_help_plain())
            .prompt()
            .ok()?;
        if let LinkChoice::Existing(url) = pick {
            return Some(vec![url]);
        }
    }
}

/// Engines offered for a URL: the installed ones (yt-dlp/aria2), with the
/// auto-detected guess moved to the front.
pub(super) fn engine_offer(tc: &Toolchain, url: &str) -> Vec<Engine> {
    engine_offer_with_guess(tc, detect_engine(url))
}

pub(super) fn engine_offer_with_guess(tc: &Toolchain, guess: Engine) -> Vec<Engine> {
    let mut list = Vec::new();
    if tc.yt_dlp.is_some() {
        list.push(Engine::YtDlp);
    }
    if tc.aria2c.is_some() {
        list.push(Engine::Aria2);
    }
    if let Some(pos) = list.iter().position(|e| *e == guess) {
        list.remove(pos);
        list.insert(0, guess);
    }
    list
}

pub(super) fn ask_engine(url: &str, tc: &Toolchain, info: &LinkInfo) -> Option<Engine> {
    let list = if info.direct {
        engine_offer_with_guess(tc, info.engine(url))
    } else {
        engine_offer(tc, url)
    };
    if list.len() == 1 {
        return Some(list[0]);
    }
    struct EngineOpt(Engine, String);
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
                tr_format!("{}  (recommended)", "{}  (рекомендуется)", e.label())
            } else {
                e.label().to_string()
            };
            EngineOpt(*e, label)
        })
        .collect();
    Select::new(tr!("Download with:", "Чем скачивать:"), choices)
        .with_help_message(select_help())
        .prompt()
        .ok()
        .map(|c| c.0)
}

pub(super) fn ask_format(url: &str) -> Option<Format> {
    struct FmtOpt(Format, &'static str);
    impl fmt::Display for FmtOpt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.1)
        }
    }
    let choices: Vec<FmtOpt> = engines::formats_for_url(url)
        .into_iter()
        .map(|(k, l)| FmtOpt(k, l))
        .collect();
    Select::new(tr!("Format (yt-dlp):", "Формат (yt-dlp):"), choices)
        .with_help_message(select_help())
        .prompt()
        .ok()
        .map(|c| c.0)
}

pub(super) enum DirEntryChoice {
    Select(PathBuf),
    Up,
    Drive(PathBuf),
    More(usize),
    Down(PathBuf),
}

impl fmt::Display for DirEntryChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // File names are remote-controlled (archive contents!): never let
            // them inject raw escape sequences into the terminal menu.
            DirEntryChoice::Select(p) => {
                tr_write!(
                    f,
                    "✓ Choose this folder ({})",
                    "✓ Выбрать эту папку ({})",
                    clip(&p.display().to_string(), 90)
                )
            }
            DirEntryChoice::Up => tr_write!(f, "↑ Parent folder", "↑ Наверх"),
            DirEntryChoice::Drive(p) => tr_write!(f, "💽 Drive {}", "💽 Диск {}", p.display()),
            // Honest wording: the list is alphabetical and the tail is never
            // rendered - "поднимитесь выше" could not reveal it.
            DirEntryChoice::More(n) => tr_write!(
                f,
                "… showing the first {MAX_BROWSE_ENTRIES} folders alphabetically ({n} total)",
                "… показаны первые {MAX_BROWSE_ENTRIES} папок по алфавиту (всего {n})"
            ),
            DirEntryChoice::Down(p) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                write!(f, "📁 {}", clip(&name, 60))
            }
        }
    }
}

pub(super) fn browse_dir(start: PathBuf) -> Option<String> {
    let fallback = snatch_rs::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut current = if !reaches_unc(&start) && start.exists() {
        start
    } else {
        fallback
    };
    if reaches_unc(&current) {
        return None;
    }
    loop {
        let all_dirs = safe_read_dirs(&current);
        let shown = all_dirs.len().min(MAX_BROWSE_ENTRIES);
        let mut choices = vec![DirEntryChoice::Select(current.clone())];
        if current.parent().is_some() {
            choices.push(DirEntryChoice::Up);
        }
        choices.extend(
            snatch_rs::ui::drive_roots()
                .into_iter()
                .map(DirEntryChoice::Drive),
        );
        choices.extend(all_dirs[..shown].iter().cloned().map(DirEntryChoice::Down));
        if all_dirs.len() > MAX_BROWSE_ENTRIES {
            choices.push(DirEntryChoice::More(all_dirs.len()));
        }
        let ans = Select::new(
            &tr_format!(
                "Folder: {}",
                "Папка: {}",
                clip(&current.display().to_string(), 90)
            ),
            choices,
        )
        .with_help_message(select_help())
        .prompt()
        .ok()?;
        match ans {
            DirEntryChoice::Select(p) => return Some(p.to_string_lossy().into_owned()),
            DirEntryChoice::Up => {
                if let Some(parent) = current.parent() {
                    current = parent.to_path_buf();
                }
            }
            DirEntryChoice::More(_) => {}
            DirEntryChoice::Down(p) | DirEntryChoice::Drive(p) => current = p,
        }
    }
}

pub(super) enum DirChoice {
    Last(String),
    Downloads,
    Desktop,
    Cwd,
    Browse,
}

impl fmt::Display for DirChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirChoice::Last(d) => tr_write!(f, "Last used: {}", "Последняя: {}", clip(d, 70)),
            DirChoice::Downloads => tr_write!(f, "Downloads", "Загрузки"),
            DirChoice::Desktop => tr_write!(f, "Desktop", "Рабочий стол"),
            DirChoice::Cwd => tr_write!(f, "Current folder", "Текущая папка"),
            DirChoice::Browse => tr_write!(f, "Browse folders…", "Выбрать в проводнике…"),
        }
    }
}

pub(super) fn ask_dir(cfg: &Config) -> Option<String> {
    let home = snatch_rs::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut choices = Vec::new();
    if !cfg.last_dir.is_empty()
        && !reaches_unc(Path::new(&cfg.last_dir))
        && Path::new(&cfg.last_dir).exists()
    {
        choices.push(DirChoice::Last(cfg.last_dir.clone()));
    }
    choices.push(DirChoice::Downloads);
    choices.push(DirChoice::Desktop);
    choices.push(DirChoice::Cwd);
    choices.push(DirChoice::Browse);

    let pick = Select::new(tr!("Save to:", "Куда сохранять:"), choices)
        .with_help_message(select_help())
        .prompt()
        .ok()?;
    match pick {
        DirChoice::Last(d) => Some(d),
        DirChoice::Downloads => Some(
            snatch_rs::config::downloads_dir()
                .unwrap_or_else(|| home.join("Downloads"))
                .to_string_lossy()
                .into_owned(),
        ),
        DirChoice::Desktop => Some(
            snatch_rs::config::desktop_dir()
                .unwrap_or_else(|| home.join("Desktop"))
                .to_string_lossy()
                .into_owned(),
        ),
        DirChoice::Cwd => Some(cwd.to_string_lossy().into_owned()),
        DirChoice::Browse => {
            let start = if !cfg.last_dir.is_empty() {
                PathBuf::from(&cfg.last_dir)
            } else {
                PathBuf::from(&cfg.default_dir)
            };
            browse_dir(start)
        }
    }
}

/// The plan summary line shown before starting the whole queue. The format
/// label comes from [`engines::formats_for_url`], so it matches the menu the
/// user just answered instead of the generic list.
pub(super) fn confirm_message(
    engine: Engine,
    url: &str,
    fmt: Format,
    out_dir: &str,
    cookies_browser: Option<&str>,
) -> String {
    match engine {
        Engine::YtDlp => {
            let label = engines::formats_for_url(url)
                .iter()
                .find(|(k, _)| *k == fmt)
                .map_or(fmt.label(), |(_, l)| *l);
            let mut m = format!(" yt-dlp · {label} · → {}", clip(out_dir, 40));
            if let Some(b) = cookies_browser {
                m.push_str(&tr_format!(" · 🍪 cookies: {b}", " · 🍪 куки: {b}"));
            }
            m
        }
        Engine::Aria2 => format!(" aria2c → {}", clip(out_dir, 40)),
    }
}

pub(super) fn queue_summary(plans: &[Plan], jobs: u8) -> String {
    let mut summary = tr_format!(
        "Jobs: {} · concurrent: {}",
        "Задач: {} · одновременно: {}",
        plans.len(),
        plans.len().min(usize::from(jobs))
    );
    for (i, plan) in plans.iter().enumerate() {
        let cookies = if plan.engine == Engine::YtDlp {
            plan.cookies_browser.as_deref()
        } else {
            None
        };
        summary.push_str(&format!(
            "\n  {}/{}. {}\n       {}",
            i + 1,
            plans.len(),
            clip(&plan.url, 60),
            confirm_message(plan.engine, &plan.url, plan.fmt, &plan.out_dir, cookies).trim()
        ));
    }
    summary
}

pub(super) fn collect(
    tc: &Toolchain,
    url: String,
    out_dir: &str,
    cookies_browser: Option<String>,
    engine_override: Option<Engine>,
    format_override: Option<Format>,
) -> Option<Plan> {
    if url.is_empty() {
        return None;
    }
    let info = if engine_override.is_none() || engine_override == Some(Engine::Aria2) {
        let info = links::resolve(&url);
        warn_unchecked_link(&url, &info);
        info
    } else {
        LinkInfo::default()
    };
    let engine = match engine_override {
        Some(engine) => engine,
        None => ask_engine(&url, tc, &info)?,
    };
    let fmt = if engine == Engine::YtDlp {
        match format_override {
            Some(format) => format,
            None => ask_format(&url)?,
        }
    } else {
        Format::Best
    };
    Some(Plan {
        url,
        download_name: if engine == Engine::Aria2 {
            info.name
        } else {
            None
        },
        engine,
        fmt,
        out_dir: out_dir.to_string(),
        cookies_browser,
        no_continue: false,
        torrent: torrent::Choice::default(),
        extras: engines::RunExtras::default(),
    })
}
