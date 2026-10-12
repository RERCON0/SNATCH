use super::*;

pub fn preflight_warning(job: &Job) -> Vec<String> {
    let (has_ffmpeg, has_js) = helper_presence();
    preflight_warning_with_ffmpeg(
        job,
        job.engine == Engine::YtDlp && has_ffmpeg,
        job.engine == Engine::YtDlp && has_js,
    )
}

/// (ffmpeg, Deno) found on PATH or in the private bin dir. The answer is
/// cached because probing stats every PATH entry, but it is NOT permanent: the
/// in-app installer adds both mid-process, so it calls
/// [`refresh_helper_presence`] when it finishes.
pub(super) static HELPER_PRESENCE: std::sync::Mutex<Option<(bool, bool)>> =
    std::sync::Mutex::new(None);

pub(super) fn probe_helpers() -> (bool, bool) {
    (
        bootstrap_or_path_exists("ffmpeg"),
        bootstrap_or_path_exists("deno"),
    )
}

pub(super) fn helper_presence() -> (bool, bool) {
    let cached = *HELPER_PRESENCE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = cached {
        return found;
    }
    // Probed outside the lock: a slow PATH entry must not stall other callers.
    let found = probe_helpers();
    *HELPER_PRESENCE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(found);
    found
}

/// Re-probe ffmpeg/Deno now. Call it from a worker thread - at startup (so the
/// first download does not scan PATH on the UI thread) and after the
/// installer put new tools in place.
pub fn refresh_helper_presence() {
    let found = probe_helpers();
    *HELPER_PRESENCE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(found);
}

/// True when the tool is in the private bin dir (setup installs ffmpeg and
/// Deno there next to yt-dlp) or on PATH - the two places yt-dlp itself
/// looks for them. `prepend_bootstrap_path` puts the same dir on the child's
/// PATH, so what is detected here is also what yt-dlp will find.
pub(super) fn bootstrap_or_path_exists(name: &str) -> bool {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    which(name).is_some() || crate::tools::bootstrap_dir().is_ok_and(|b| b.join(&file).is_file())
}

/// The private bin dir must be on the child's PATH: yt-dlp finds ffmpeg and
/// the JS runtime (Deno) only through PATH, not next to its own exe.
pub fn prepend_bootstrap_path(command: &mut std::process::Command) {
    let Ok(bin) = crate::tools::bootstrap_dir() else {
        return;
    };
    if !bin.is_dir() {
        return;
    }
    let mut paths = vec![bin];
    if let Some(current) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&current));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

pub(super) fn preflight_warning_with_ffmpeg(
    job: &Job,
    has_ffmpeg: bool,
    has_js: bool,
) -> Vec<String> {
    let mut warns = Vec::new();
    if job.engine != Engine::YtDlp {
        if job.cookies_browser.is_some() {
            warns.push(
                tr!("Browser cookies only apply to yt-dlp — ignored for this engine.", "Куки из браузера применимы только к yt-dlp — для этого движка они проигнорированы.")
                    .to_string(),
            );
        }
        if job.fmt != Format::Best {
            warns.push(
                tr!(
                    "Format only applies to yt-dlp — ignored for aria2.",
                    "Формат применим только к yt-dlp — для aria2 он проигнорирован."
                )
                .to_string(),
            );
        }
    } else {
        if !has_ffmpeg {
            if job.fmt == Format::Audio {
                warns.push(
                    tr!(
                        "ffmpeg not found — mp3 extraction and cover embedding may fail. Install tools from the GUI or with `snatch --install-tools`.",
                        "Не найден ffmpeg — извлечение mp3 и встраивание обложки могут не сработать. Установите инструменты в GUI или через `snatch --install-tools`."
                    )
                    .to_string(),
                );
            } else if job.fmt == Format::AudioSource {
                warns.push(
                    tr!(
                        "ffmpeg not found — original audio extraction and cover embedding may fail. Install tools from the GUI or with `snatch --install-tools`.",
                        "Не найден ffmpeg — извлечение исходного аудио и встраивание обложки могут не сработать. Установите инструменты в GUI или через `snatch --install-tools`."
                    )
                    .to_string(),
                );
            } else if job.fmt == Format::Subs {
                warns.push(
                    tr!(
                        "ffmpeg not found — subtitles will stay in the source format (vtt) instead of .srt. Install tools from the GUI or with `snatch --install-tools`.",
                        "Не найден ffmpeg — субтитры останутся в исходном формате (vtt), без конвертации в .srt. Установите инструменты в GUI или через `snatch --install-tools`."
                    )
                    .to_string(),
                );
            } else {
                warns.push(
                    tr!("ffmpeg not found — video/audio merging is unavailable. yt-dlp will choose a single-track format (quality may be lower).", "Не найден ffmpeg — склейка видео и аудио недоступна, yt-dlp выберет \
                      однодорожный формат (качество может быть ниже).")
                        .to_string(),
                );
            }
        }
        if !has_js {
            warns.push(
                tr!("JavaScript runtime (Deno) not found — some YouTube formats may be unavailable. Install tools from the GUI or with `snatch --install-tools`.", "Не найден JS-рантайм (Deno) — на YouTube часть форматов может быть \
                  недоступна. Установите загрузчики кнопкой в GUI или `snatch --install-tools`.")
                    .to_string(),
            );
        }
    }
    // E-1/F1: collision feedback. A bare file is renamed around; a foreign
    // file+control pair stops the job (aria2 would auto-continue it).
    let out = absolute_out_dir(&job.out_dir);
    if reaches_unc(&out) {
        warns.push(
            tr!(
                "Network UNC folders are not supported — choose a local folder.",
                "Сетевая UNC-папка не поддерживается — выберите локальную папку."
            )
            .into(),
        );
        return warns;
    }
    if let Some(part) = ytdlp_foreign_part(job, &out) {
        warns.push(tr_format!(
            "“{part}” is a partial download from another link — downloading will be stopped so yt-dlp cannot continue it. Inspect and remove it (or choose another folder).",
            "«{part}» — незавершённая загрузка с другой ссылки: загрузка будет остановлена, чтобы yt-dlp не продолжил её. Проверьте и удалите файл (или выберите другую папку)."
        ));
    }
    if job.engine == Engine::Aria2 || (job.engine == Engine::YtDlp && direct_via_aria2(&job.url)) {
        if let Some(name) = aria2_foreign_pair(job, &out) {
            warns.push(tr_format!("“{name}” has resume data from another download — downloading will be stopped so aria2 cannot resume someone else's file. Inspect and remove them (or choose another folder).", "В папке есть «{name}» с данными докачки от другой загрузки — загрузка будет \
                 остановлена, чтобы aria2 не продолжил чужой файл. Проверьте и удалите их \
                 (или выберите другую папку)."
            ));
        } else if aria2_foreign_target(job, &out) {
            warns.push(
                tr!("A file with the same name already exists without resume data — it will not be overwritten. The new download will use a different filename.", "В папке уже есть файл с таким же именем, но без данных докачки — он не будет \
                 перезаписан: новая загрузка сохранится рядом под другим именем.")
                    .to_string(),
            );
        }
    }
    warns
}
