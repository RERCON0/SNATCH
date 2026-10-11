use super::*;

// --seed-time=0: without it aria2c finishes the download and then keeps
// seeding until ratio 1.0 (its default) - the process never exits, the GUI
// sits in "Running" at 100% forever and the CLI never returns the terminal.
pub const ARIA2_RESUME: &[&str] = &[
    "-c",
    "--max-tries=10",
    "--retry-wait=2",
    "--auto-file-renaming=false",
    "--seed-time=0",
];
pub const ARIA2_WORKERS: &[&str] = &["-x", "16", "-s", "16", "-k", "1M"];

pub(super) fn format_flags(fmt: Format) -> Vec<&'static str> {
    match fmt {
        Format::P2160 => vec!["-f", "bv*[height<=2160]+ba/b[height<=2160]"],
        Format::P1440 => vec!["-f", "bv*[height<=1440]+ba/b[height<=1440]"],
        Format::P1080 => vec!["-f", "bv*[height<=1080]+ba/b[height<=1080]"],
        Format::P720 => vec!["-f", "bv*[height<=720]+ba/b[height<=720]"],
        Format::P480 => vec!["-f", "bv*[height<=480]+ba/b[height<=480]"],
        Format::Audio => vec!["-x", "--audio-format", "mp3"],
        // Extract Opus from WebM so its container can carry a cover.
        // yt-dlp's "best" extraction stream-copies AAC/Opus without re-encoding.
        Format::AudioSource => vec!["-f", "ba/b", "-x", "--audio-format", "best"],
        Format::Best | Format::Subs => vec![],
    }
}

pub fn build(job: &Job, tc: &Toolchain) -> Result<Vec<OsString>, String> {
    build_with(job, tc, &RunExtras::default())
}

/// Extractor plugin shipped with SNATCH: Threads posts, which yt-dlp itself
/// cannot read (see the file for how it works).
pub(super) const THREADS_PLUGIN: &str = include_str!("../../plugins/snatch_threads.py");

/// Folder given to yt-dlp's `--plugin-dirs`. yt-dlp expects
/// `<dir>/<package>/yt_dlp_plugins/extractor/<plugin>.py`.
pub fn ytdlp_plugin_dir() -> Result<PathBuf, String> {
    config_dir().map(|dir| dir.join("yt-dlp-plugins"))
}

pub(super) fn ytdlp_plugin_file(root: &Path) -> PathBuf {
    root.join("snatch")
        .join("yt_dlp_plugins")
        .join("extractor")
        .join("snatch_threads.py")
}

/// Writes the bundled plugin when it is missing or differs from this build's
/// copy (an update of SNATCH updates the plugin). Atomic, so a yt-dlp that
/// starts at the same moment never reads half a file.
pub(super) fn install_ytdlp_plugin(root: &Path) -> std::io::Result<()> {
    let file = ytdlp_plugin_file(root);
    if std::fs::read_to_string(&file).is_ok_and(|current| current == THREADS_PLUGIN) {
        return Ok(());
    }
    let dir = file.parent().unwrap_or(root);
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("snatch_threads.py.{}.tmp", std::process::id()));
    std::fs::write(&tmp, THREADS_PLUGIN)?;
    std::fs::rename(&tmp, &file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The plugin folder, once the plugin is in it. Without it yt-dlp still works,
/// it just has no Threads support, so a failure here is not an error.
pub(super) fn prepared_ytdlp_plugin_dir() -> Option<PathBuf> {
    let root = ytdlp_plugin_dir().ok()?;
    let _ = install_ytdlp_plugin(&root);
    ytdlp_plugin_file(&root).is_file().then_some(root)
}

/// The output name for a direct link downloaded by yt-dlp (see build_with):
/// the URL's file name, at most [`YTDLP_OUTPUT_LIMIT`] characters (the
/// extension kept), with `%` doubled because yt-dlp reads `-o` as a template.
///
/// `None` (yt-dlp then names the file itself) when the name cannot be given to
/// the template as is. yt-dlp also runs the template through
/// `expanduser`/`expandvars`, and neither has an escape: `$TEMP.mp4` would be
/// saved as `Temp.mp4` in the parent of the TEMP folder and `~x.mp4` as `x.mp4`
/// in the parent of the home folder - outside the download folder, steered by whoever
/// controls the link.
pub(super) fn ytdlp_direct_output(job: &Job) -> Option<String> {
    if !is_direct_download(&job.url) {
        return None;
    }
    let name = limit_name(&aria2_job_name(job), YTDLP_OUTPUT_LIMIT);
    if name.is_empty() || name.contains('$') || name.starts_with('~') {
        return None;
    }
    Some(name.replace('%', "%%"))
}

/// Longest `-o` name handed to yt-dlp: signed CDN links carry the file name in
/// the path, and the template must stay inside the file-name limit.
pub(super) const YTDLP_OUTPUT_LIMIT: usize = 150;

/// `name` cut to `max` characters in its stem, keeping a short extension.
pub(super) fn limit_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= 16 => name.split_at(dot),
        _ => (name, ""),
    };
    let keep = max.saturating_sub(extension.chars().count());
    format!("{}{extension}", stem.chars().take(keep).collect::<String>())
}

pub fn build_with(job: &Job, tc: &Toolchain, extras: &RunExtras) -> Result<Vec<OsString>, String> {
    if job.engine == Engine::YtDlp {
        if let Some(cb) = &job.cookies_browser {
            validate_cookies_browser(cb)?;
        }
    }

    let out = absolute_out_dir(&job.out_dir);
    // Before create_dir_all: even a failed mkdir on a UNC path performs SMB
    // authentication against the remote host (NetNTLMv2 leak), and a
    // *successful* one would silently write the user's downloads to a
    // stranger's share (poisoned config/history or a pasted path).
    if reaches_unc(&out) {
        return Err(tr_format!("Download folder {} is a network UNC path. Choose a local folder (UNC paths are rejected to avoid sending Windows credentials to a remote server).", "Папка сохранения {} — сетевой UNC-путь. Выберите локальную папку \
             (UNC отклоняется, чтобы Windows не отправляла учётные данные на \
             чужой сервер).",
            out.display()
        ));
    }
    std::fs::create_dir_all(&out).map_err(|e| {
        tr_format!(
            "Could not create folder {}: {e}",
            "Не удалось создать папку {}: {e}",
            out.display()
        )
    })?;

    let mut cmd: Vec<OsString> = Vec::new();
    if job.engine == Engine::Aria2 {
        cmd.push(tc.require("aria2c")?.into_os_string());
        cmd.push("--no-conf".into());
        cmd.push("-d".into());
        cmd.push(out.clone().into_os_string());
        // Resolve the saved name before this build: a validated server name
        // or, without discovery, the percent-decoded URL basename. Pin with -o
        // so the name the lock/origin records were computed for is exactly
        // the file aria2 writes - otherwise a foreign same-name pair slips
        // past aria2_foreign_pair and aria2 continues a stranger's bytes.
        // A URL without a file name in its path gets the deterministic
        // fallback pin (see aria2_pinned_name): skipping the pin for it
        // would silently disable every one of those checks.
        if !is_bittorrent_input(&job.url) {
            cmd.push("-o".into());
            cmd.push(aria2_job_name(job).into());
        }
        // F1: a same-name file WITH a .aria2 control file is auto-continued by
        // aria2 even without -c, silently mixing two sources' bytes. Our own
        // partials carry an origin fingerprint; anything else must stop here.
        if let Some(name) = aria2_foreign_pair(job, &out) {
            return Err(tr_format!("“{name}” and its .aria2 control file belong to an interrupted download from another URL. aria2 would resume someone else's file. Inspect and remove them (or choose another folder), then retry.", "«{name}» и его файл докачки .aria2 оставлены прерванной загрузкой другого URL — \
                 aria2 продолжил бы чужой файл. Проверьте и удалите их (или выберите другую \
                 папку), после чего повторите."
            ));
        }
        // -c only affects HTTP/FTP; keep torrent/magnet options as before.
        let resume = !ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str())
            || aria2_owned_partial(job, &out);
        for a in aria2_flags(resume).iter().chain(ARIA2_WORKERS) {
            cmd.push((*a).into());
        }
        if is_bittorrent_input(&job.url) {
            // aria2 normally preallocates every file and saves its .aria2
            // control file only once a minute. If the GUI is killed during
            // preallocation (e.g. by an AV), it leaves huge files without
            // control data; the next run safely refuses them with code 13.
            cmd.push("--file-allocation=none".into());
            cmd.push("--auto-save-interval=1".into());
        }
        validate_aria2_extras(&extras.extra_aria2)?;
        for a in &extras.extra_aria2 {
            cmd.push(a.clone().into());
        }
        cmd.push("--".into());
        cmd.push(OsString::from(&job.url));
        return Ok(cmd);
    }

    cmd.push(tc.require("yt-dlp")?.into_os_string());
    cmd.push("--ignore-config".into());
    // yt-dlp loads plugins (Python code) from ./yt-dlp-plugins, next to its exe
    // and from the user profile by default: a folder planted in the current
    // directory would run with this process's rights. Only SNATCH's own
    // plugin folder is allowed.
    cmd.push("--no-plugin-dirs".into());
    if let Some(dir) = prepared_ytdlp_plugin_dir() {
        cmd.push("--plugin-dirs".into());
        cmd.push(dir.into_os_string());
    }
    cmd.push("-P".into());
    cmd.push(out.clone().into_os_string());
    // A direct link has no title: yt-dlp would build the file name from the
    // WHOLE url (query string included), which overruns the file-name limit on
    // signed CDN links. Name it after the URL's own file name, like aria2 does.
    if let Some(name) = ytdlp_direct_output(job) {
        cmd.push("-o".into());
        cmd.push(name.into());
    }
    cmd.push(OsString::from(if extras.playlist {
        "--yes-playlist"
    } else {
        "--no-playlist"
    }));
    // The "subs" format is subtitles-only: pure text, no media. yt-dlp
    // writes the requested sidecar files and skips the video itself with
    // --skip-download; --convert-subs srt turns captions into plain text
    // (ffmpeg is required - without it yt-dlp keeps the source vtt).
    let subs_only = job.fmt == Format::Subs;
    if extras.subs || subs_only {
        if subs_only {
            cmd.push("--skip-download".into());
        }
        cmd.push("--write-subs".into());
        cmd.push("--write-auto-subs".into());
        cmd.push("--sub-langs".into());
        cmd.push(extras.effective_sub_langs().into());
        if subs_only {
            cmd.push("--convert-subs".into());
            cmd.push("srt".into());
        }
    }
    if job.fmt.is_audio() {
        // Use the downloader's existing postprocessors, with no new runtime.
        // JPEG covers work with both ID3 and MP4/Vorbis picture tags. A source
        // without artwork still downloads normally. Explicit extras can opt out.
        for a in [
            "--embed-metadata",
            "--embed-thumbnail",
            "--convert-thumbnails",
            "jpg",
            "--no-embed-info-json",
        ] {
            cmd.push(a.into());
        }
    }
    for a in &extras.extra_ytdlp {
        cmd.push(a.clone().into());
    }
    if let Some(cb) = &job.cookies_browser {
        cmd.push("--cookies-from-browser".into());
        cmd.push(cb.into());
    }
    for a in format_flags(job.fmt) {
        cmd.push(a.into());
    }
    if let Some(aria2c) = &tc.aria2c {
        if direct_via_aria2(&job.url) {
            // F1: aria2 auto-continues a same-name foreign partial even
            // without -c; refuse instead of mixing two sources' bytes.
            if let Some(name) = aria2_foreign_pair(job, &out) {
                return Err(tr_format!("“{name}” and its .aria2 control file belong to an interrupted download from another URL. The external aria2 downloader would resume someone else's file. Inspect and remove them (or choose another folder), then retry.", "«{name}» и его файл докачки .aria2 оставлены прерванной загрузкой другого \
                     URL — внешний загрузчик aria2 продолжил бы чужой файл. Проверьте и удалите \
                     их (или выберите другую папку), после чего повторите."
                ));
            }
            // yt-dlp decides the actual destination name, which may differ
            // from the URL basename. Never enable -c on that unknown target.
            cmd.push("--external-downloader".into());
            cmd.push(aria2c.as_os_str().to_os_string());
            cmd.push("--external-downloader-args".into());
            cmd.push(
                format!(
                    "{} {} --no-conf",
                    aria2_flags(false).join(" "),
                    ARIA2_WORKERS.join(" ")
                )
                .into(),
            );
        }
    }
    cmd.push("--".into());
    cmd.push(OsString::from(&job.url));
    Ok(cmd)
}

/// Like [`build`], but applies the user's torrent [`Choice`]: `--select-file`
/// for a partial selection, and the .torrent fetched while listing as aria2's
/// input instead of the magnet/URL - aria2 downloads exactly what was listed
/// and does not fetch the metadata twice. (No RPC: with `--enable-rpc` aria2
/// runs as a daemon and never exits after the download - live stats come
/// from the console summary instead, which the caller turns on with
/// `--summary-interval`: this function does not set it.)
///
/// [`Choice`]: crate::torrent::Choice
pub fn build_for_run(
    job: &Job,
    tc: &Toolchain,
    choice: &crate::torrent::Choice,
) -> Result<Vec<OsString>, String> {
    build_for_run_with(job, tc, choice, &RunExtras::default())
}

pub fn build_for_run_with(
    job: &Job,
    tc: &Toolchain,
    choice: &crate::torrent::Choice,
    extras: &RunExtras,
) -> Result<Vec<OsString>, String> {
    let mut cmd = build_with(job, tc, extras)?;
    if job.engine != Engine::Aria2 || !is_bittorrent_input(&job.url) {
        return Ok(cmd);
    }
    if let Some(meta) = &choice.meta {
        if meta.path().is_file() {
            meta.touch();
            set_input(&mut cmd, meta.path().as_os_str().to_os_string());
        } else if choice.files.is_some() && scheme_of(&job.url) != "magnet" {
            // Gone (a temp cleaner during a long pause). A magnet pins its
            // content by hash, so aria2 can simply fetch it again; an http(s)
            // .torrent may have changed since, and the picked indices would
            // then select different files.
            return Err(tr!("the .torrent used for file selection no longer exists — add the link and select files again", "файлы выбирались по .torrent, которого больше нет — добавьте ссылку и выберите файлы заново").into());
        }
    }
    if let Some(sel) = &choice.files {
        if let Some(spec) = crate::torrent::select_spec(sel)? {
            insert_option(&mut cmd, format!("--select-file={spec}"));
        }
    }
    Ok(cmd)
}

/// Every command line [`build_with`] returns ends with `["--", input]`: flags
/// go before the terminator, the URL/path stays after it, so a downloader can
/// never read the input as an option. These two helpers are the only places
/// that rely on that shape.
pub(super) fn assert_input_tail(cmd: &[OsString]) {
    debug_assert!(
        cmd.len() >= 2 && cmd[cmd.len() - 2] == "--",
        "a built command must end with `--` and its input"
    );
}

/// Adds a flag in front of the `--` terminator.
pub fn insert_option(cmd: &mut Vec<OsString>, option: impl Into<OsString>) {
    assert_input_tail(cmd);
    let at = cmd.len().saturating_sub(2);
    cmd.insert(at, option.into());
}

/// Replaces the input that follows the `--` terminator.
pub(super) fn set_input(cmd: &mut [OsString], input: OsString) {
    assert_input_tail(cmd);
    if let Some(last) = cmd.last_mut() {
        *last = input;
    }
}
