use super::*;

pub fn aria2_missing_control_hint() -> &'static str {
    tr!("Files from this torrent already exist, but the .aria2 control file is missing. The download was stopped to protect your data. Choose a new empty folder; do not enable overwriting existing files.", "Файлы этой раздачи уже есть, но файл докачки .aria2 отсутствует. Чтобы не стереть данные, загрузка остановлена. Выберите новую пустую папку; не включайте перезапись существующих файлов.")
}

/// Поиск ASCII-подстроки без временной копии строки (в отличие от
/// `to_ascii_lowercase().contains(..)`): `needle` обязан быть непустым и уже
/// в нижнем регистре. Регистронезависимость ровно та же, что давал нижний
/// регистр: не-ASCII символы сравниваются побайтово, как и раньше.
pub fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    !needle.is_empty()
        && needle.len() <= hay.len()
        && hay
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle))
}

pub fn aria2_missing_control(line: &str) -> bool {
    contains_ignore_ascii_case(line, "errorcode=13")
        && contains_ignore_ascii_case(line, "control file")
        && contains_ignore_ascii_case(line, ".aria2")
        && contains_ignore_ascii_case(line, "does not exist")
}

/// Is this loader line about a DRM-protected stream? yt-dlp refuses to
/// download encrypted formats (SoundCloud tracks that only expose CBC/CTR
/// manifests, the occasional YouTube iOS format) and says "This video is DRM
/// protected" / "This format is DRM protected". Cookies cannot remove DRM, so
/// this must NOT feed the cookie hint - it gets its own explanation instead.
pub fn is_drm_protected(line: &str) -> bool {
    has_diagnostics_context(line) && contains_ignore_ascii_case(line, "drm protected")
}

/// Short status for a job that died on DRM ([`is_drm_protected`]).
pub fn drm_protected_short() -> &'static str {
    tr!(
        "DRM-protected stream — the downloader cannot save it",
        "поток защищён DRM — загрузчик не может его сохранить"
    )
}

/// Why a DRM failure cannot be fixed by the usual remedies: this is
/// encryption, not a login wall. Kept honest about the one case that has a
/// way forward (a single DRM format among playable ones). On SoundCloud the
/// DRM streams come from the monetized Go+ catalog (BuyDRM KeyOS), and a
/// guest can only ever get the track's 30-second preview.
pub fn drm_protected_note() -> &'static str {
    tr!(
        "The service serves an encrypted (DRM) stream; yt-dlp does not remove DRM, so cookies will not help. For a video, try another quality or format. On SoundCloud, monetized tracks are Go+ DRM — even with a subscription only the 30-second preview can be saved.",
        "Сервис отдаёт зашифрованный (DRM) поток; yt-dlp не снимает DRM, поэтому куки не помогут. Для видео попробуйте другое качество или формат. В SoundCloud платные треки каталога Go+ защищены DRM — даже с подпиской сохраняется максимум 30-секундное превью."
    )
}

pub(super) const AUTH_HINTS: &[&str] = &[
    "sign in to confirm",
    "not a bot",
    "age-restrict",
    "age restrict",
    "private video",
    "members-only",
    "member-only",
    "login required",
    "requires login",
    "requires a login",
    "authentication",
    "http error 403",
    "forbidden",
    "join this channel",
    "try using --cookies",
    // yt-dlp itself recommends cookies ("Use --cookies-from-browser or
    // --cookies for the authentication", Vimeo's "only works when
    // logged-in"): a hint that comes from the downloader is worth acting on.
    "--cookies",
    "cookies from a browser",
    // YouTube's rate limiting (429) is not a login demand, but a signed-in
    // browser session is the documented practical cure, so it belongs with
    // the cookie hints: "Download failed. Try cookies?" is the right offer.
    "http error 429",
    "too many requests",
];

/// Failures whose usual cure is cookies from a signed-in browser: sign-in
/// and bot checks, age gates, and YouTube's HTTP 429 rate limiting. (The
/// name says "auth" for history; the 403/429 entries already stretch it,
/// and every consumer - CLI offer, GUI retry, --yes tip - treats the hint
/// as "browser cookies may fix this", not as a diagnosis.)
pub fn looks_like_auth(line: &str) -> bool {
    // Require the loaders' own diagnostics shape: "ERROR:" / "WARNING:"
    // lines (yt-dlp), aria2's "... ERROR - ..." and "[ERROR]/[WARNING]"
    // tags. A bare "error" anywhere is not enough: a video titled
    // "How to fix HTTP Error 429 - Too Many Requests" arrives inside a
    // regular Destination line, and such remote text must not arm the
    // cookie hint for an unrelated later failure.
    has_diagnostics_context(line)
        && AUTH_HINTS
            .iter()
            .any(|p| contains_ignore_ascii_case(line, p))
}

/// yt-dlp/ffmpeg diagnostics that say a RESUMED partial file is unusable
/// (stale range, corrupt bytes in the old head): the only failures that a
/// from-scratch restart can heal. A dropped connection or a full disk fails
/// the same way again, so those must keep the partial instead of discarding
/// hours of progress.
pub(super) const CORRUPT_PARTIAL_HINTS: &[&str] = &[
    "invalid data found",
    "errno 22",
    "invalid argument",
    "moov atom not found",
    "error opening input",
    "unable to resume",
    "http error 416",
    "requested range not satisfiable",
    "file is corrupt",
    "conversion failed",
];

pub fn looks_like_corrupt_partial(line: &str) -> bool {
    has_diagnostics_context(line)
        && CORRUPT_PARTIAL_HINTS
            .iter()
            .any(|p| contains_ignore_ascii_case(line, p))
}

/// aria2c's own report that a TLS handshake failed. Its Windows build ships a
/// TLS client that many modern hosts refuse; yt-dlp (OpenSSL) can usually
/// still download the same direct link.
pub fn looks_like_aria2_tls_failure(line: &str) -> bool {
    // The phrase sits on aria2's indented `  -> [SocketCore.cc:..] errorCode=1`
    // continuation line, which carries no [ERROR] marker of its own, so the
    // diagnostics-context test the other classifiers use would reject it.
    contains_ignore_ascii_case(line, "ssl/tls handshake failure")
}

/// aria2c's report that the host turned it away with HTTP 403 or 429. Some
/// CDNs and wikis refuse aria2's Windows TLS fingerprint outright (Wikimedia
/// answers it with 429 while serving yt-dlp and browsers the same file), so a
/// plain refusal is worth one try with the other downloader too.
fn looks_like_aria2_http_refusal(line: &str) -> bool {
    // "  -> [HttpSkipResponseCommand.cc:239] errorCode=22 The response status
    // is not successful. status=429"
    contains_ignore_ascii_case(line, "the response status is not successful")
        && (line.contains("status=429") || line.contains("status=403"))
}

/// The host refused aria2c (TLS handshake failure or HTTP 403/429) - the cue
/// to retry a direct link once with yt-dlp.
pub fn looks_like_aria2_refusal(line: &str) -> bool {
    looks_like_aria2_tls_failure(line) || looks_like_aria2_http_refusal(line)
}

/// The longest child-output line kept whole. Real lines are tens to a few
/// hundred bytes; anything past this is cut so one runaway line cannot grow
/// the buffer without bound.
pub const MAX_CHILD_LINE: usize = 64 * 1024;

/// `BufRead::read_until(b'\n', ..)` that never holds more than `max` bytes of
/// one line: the rest of an over-long line is read and discarded. Returns the
/// number of stream bytes consumed (0 = end of stream).
pub fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize> {
    let mut consumed = 0;
    loop {
        let (line_done, used) = {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                return Ok(consumed);
            }
            let (chunk, line_done) = match available.iter().position(|&b| b == b'\n') {
                Some(end) => (&available[..=end], true),
                None => (available, false),
            };
            let room = max.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
            (line_done, chunk.len())
        };
        reader.consume(used);
        consumed += used;
        if line_done {
            return Ok(consumed);
        }
    }
}

pub(super) fn has_diagnostics_context(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("ERROR:")
        || trimmed.starts_with("WARNING:")
        || line.contains(" ERROR - ")
        || contains_ignore_ascii_case(line, "[error]")
        || contains_ignore_ascii_case(line, "[warning]")
}

/// aria2's progress summary starts the line; finding `[#` anywhere would
/// mistake a yt-dlp Destination containing `[#gid ... (NN%)]` for progress.
pub(super) fn aria2_summary(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let summary = &line[..=line.find(']')?];
    let (gid, rest) = summary.strip_prefix("[#")?.split_once(' ')?;
    if gid.is_empty()
        || !gid.bytes().all(|b| b.is_ascii_hexdigit())
        || !rest.split_whitespace().next()?.contains('/')
    {
        return None;
    }
    Some(summary)
}

pub fn parse_progress(line: &str) -> Option<f32> {
    if let Some(summary) = aria2_summary(line) {
        if let Some(i) = summary.find("%)") {
            if let Some(start) = summary[..i].rfind('(') {
                if let Ok(p) = summary[start + 1..i].parse::<f32>() {
                    if (0.0..=100.0).contains(&p) {
                        return Some(p / 100.0);
                    }
                }
            }
        }
    }
    if let Some(rest) = line.strip_prefix("[download]") {
        let rest = rest.trim_start();
        if let Some(end) = rest.find('%') {
            if let Ok(p) = rest[..end].trim().parse::<f32>() {
                if (0.0..=100.0).contains(&p) {
                    return Some(p / 100.0);
                }
            }
        }
    }
    None
}

/// Concise status for aria2's live summary. The leading [#gid] group holds
/// download progress; [FileAlloc:...] is a separate allocation stage, not a
/// second file's percentage. Used by both terminal and GUI frontends.
pub fn aria2_stat(line: &str) -> Option<String> {
    let summary = aria2_summary(line)?;
    let percent = (parse_progress(summary)? * 100.0).round() as u32;
    let amount = summary.split_whitespace().nth(1)?.split('(').next()?;
    if let Some(start) = line.find("[FileAlloc:") {
        let alloc = line[start..].split(']').next()?.split_whitespace().nth(1)?;
        if !alloc.ends_with("(100%)") {
            return Some(tr_format!(
                "{percent}% · {amount} · allocating space {}",
                "{percent}% · {amount} · выделение места {}",
                alloc.replace('(', " (")
            ));
        }
    }
    let field = |key: &str| {
        summary
            .split_whitespace()
            .find_map(|part| part.strip_prefix(key))
            .unwrap_or("–")
            .trim_end_matches(']')
    };
    let dl = field("DL:");
    let peers = field("CN:");
    let seeds = field("SD:");
    Some(tr_format!(
        "{percent}% · {amount} · ↓{dl}/s · seeds {seeds} · connections {peers}",
        "{percent}% · {amount} · ↓{dl}/с · сиды {seeds} · соединения {peers}"
    ))
}

/// The metadata status line names the torrent, while normal FILE lines only
/// describe whichever file/directory aria2 is reporting at that moment.
pub fn aria2_metadata_name(line: &str) -> Option<String> {
    let name = line
        .strip_prefix("FILE:")?
        .trim()
        .strip_prefix("[MEMORY][METADATA]")?
        .trim();
    let name = name.strip_prefix("[DL]").unwrap_or(name).trim();
    // Before the torrent's metadata arrives aria2 names it by the magnet's raw
    // `dn=` value, still form-encoded (`The+WIRED+CD`, `%20`).
    clean_display_name(&percent_decode(&name.replace('+', " ")))
}

/// The torrent's own name from a `FILE:` line, safe to show: file and folder
/// names come from the torrent, so control and direction-changing characters
/// are removed before the name becomes a label.
pub fn aria2_name_from_file(line: &str) -> Option<String> {
    let file = line.strip_prefix("FILE:")?.trim();
    let multi_file = file.ends_with("more)");
    let path = if multi_file {
        file.rsplit_once(" (")?.0
    } else {
        file
    };
    let mut parts = path.rsplit(['/', '\\']);
    let file = parts.next()?.trim();
    let name = if multi_file {
        parts.next().unwrap_or(file)
    } else {
        file
    };
    clean_display_name(name)
}

/// Metadata and percent-decoded display names must not inject terminal
/// controls. This changes only display text, never the URL or saved path.
pub(super) fn clean_display_name(name: &str) -> Option<String> {
    let clean = sanitize_child_output(name);
    let name: String = clean
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

pub(super) fn magnet_name(url: &str) -> Option<String> {
    if scheme_of(url) != "magnet" {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    let (_, name) = parsed.query_pairs().find(|(key, _)| key == "dn")?;
    clean_display_name(&name)
}

/// Prefer the parsed torrent title; a FILE summary can refer to the save folder
/// or an arbitrary nested file, so it must never replace a torrent's title.
pub fn download_name(url: &str, choice: &crate::torrent::Choice) -> String {
    if is_bittorrent_input(url) {
        if let Some(name) = choice.name.as_deref().and_then(clean_display_name) {
            return name;
        }
    }
    batch_name(url)
}

/// Short display name for a URL before anything better is known (a torrent's
/// real name only arrives once aria2 reads its metadata) - shared by the
/// CLI's batch display and the GUI's per-job row/log-prefix label.
pub fn batch_name(url: &str) -> String {
    if let Some(name) = magnet_name(url) {
        return name;
    }
    if let Some(hash) = url
        .split_once("btih:")
        .map(|(_, s)| s.split('&').next().unwrap_or(s))
    {
        return format!("magnet {}", clip(hash, 12));
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    // The last NON-EMPTY path segment: `https://host/reel/ID/` is named `ID`,
    // not blank. A URL that is only an authority (`https://host/`) has none.
    // A local path (a dropped .torrent) may use Windows separators.
    let mut segments: Vec<&str> = if path.contains("://") {
        path.split('/').collect()
    } else {
        path.split(['/', '\\']).collect()
    };
    while segments.last() == Some(&"") {
        segments.pop();
    }
    // "scheme:", "" and the host come first in a split URL.
    let before_path = if path.contains("://") { 3 } else { 0 };
    let segment = match segments.last() {
        Some(last) if segments.len() > before_path => *last,
        _ => "",
    };
    // Generic route segments (YouTube's /watch, most players' /embed or
    // /index.*) aren't a video name - they're the same for every link, so
    // showing them as the job label is worse than showing nothing. Prefer
    // the id/v query parameter instead; the real title arrives later via
    // yt-dlp's own "Destination:" line (see ytdlp_title_from_line).
    const GENERIC_SEGMENTS: &[&str] = &["watch", "embed", "player", "index.html", "index.php", ""];
    if GENERIC_SEGMENTS.contains(&segment) {
        if let Some(query) = url
            .split_once('?')
            .map(|(_, q)| q.split('#').next().unwrap_or(q))
        {
            for pair in query.split('&') {
                if let Some((key, val)) = pair.split_once('=') {
                    if (key == "v" || key == "id") && !val.is_empty() {
                        return val.to_string();
                    }
                }
            }
        }
    }
    if segment.is_empty() {
        tr!("download", "загрузка").to_string()
    } else {
        // Shown as the file would be named, not as `%D0%9C%D0%BE...`.
        clean_display_name(&percent_decode(segment)).unwrap_or_else(|| segment.to_string())
    }
}

/// Pull the real media title out of a yt-dlp stdout line, once it announces
/// one - used to upgrade a job's label from `batch_name`'s URL guess (which,
/// for a `/watch` URL, is at best a video id) to the actual title, mirroring
/// how TorrentMeta/TorrentName upgrade an aria2/torrent job's label.
/// Subtitle downloads announce subtitles through the generic "Destination:"
/// line and, before it, "[info] Writing video subtitles to:"; both name the
/// written file, so either can serve as the title source. Their "<lang>"
/// segment ("Title [id].en.vtt") is not part of the title.
pub fn ytdlp_title_from_line(line: &str) -> Option<String> {
    let (path, subtitle) =
        if let Some(rest) = line.strip_prefix("[info] Writing video subtitles to: ") {
            (rest, true)
        } else if let Some(rest) = line.strip_prefix("[download] Destination: ") {
            (rest, has_subtitle_extension(rest))
        } else if let Some(rest) = line.strip_prefix("[download] ") {
            let rest = rest.strip_suffix(" has already been downloaded")?;
            (rest, has_subtitle_extension(rest))
        } else {
            let rest = line.strip_prefix("[Merger] Merging formats into \"")?;
            (rest.strip_suffix('"')?, false)
        };
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    // yt-dlp downloads video and audio into separate `Title [id].f251.webm`
    // files before merging them; the format tag is not part of the title.
    let stem = match stem.rsplit_once('.') {
        Some((head, tag)) if head.ends_with(']') && tag.len() > 1 && tag.starts_with('f') => head,
        _ => stem,
    };
    let stem = if subtitle {
        strip_subtitle_language(stem)
    } else {
        stem
    };
    (!stem.is_empty()).then(|| stem.to_string())
}

/// Extensions yt-dlp writes for subtitles; media files never use them, so a
/// "Destination:" line with one is a subtitle and may carry a language tag.
pub(super) fn has_subtitle_extension(path: &str) -> bool {
    matches!(
        path.rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .as_deref(),
        Some("vtt" | "srt" | "ass" | "lrc" | "ttml" | "dfxp" | "json3")
    )
}

/// "<title> [<id>].<lang>" -> "<title> [<id>]" for language-shaped tags
/// ("en", "ru", "pt-BR", "zh-Hans", "es-419"); anything else is left as is.
pub(super) fn strip_subtitle_language(stem: &str) -> &str {
    let Some((base, tag)) = stem.rsplit_once('.') else {
        return stem;
    };
    let mut parts = tag.split('-');
    let head = parts.next().unwrap_or_default();
    let head_ok = (2..=3).contains(&head.len()) && head.chars().all(|c| c.is_ascii_alphabetic());
    let tail_ok = parts
        .all(|p| !p.is_empty() && p.len() <= 8 && p.chars().all(|c| c.is_ascii_alphanumeric()));
    if head_ok && tail_ok {
        base
    } else {
        stem
    }
}

pub struct RunResult {
    pub code: i32,
    pub auth_hint: bool,
}

/// Watches yt-dlp's subtitle lines for the subtitles-only format ("subs").
/// yt-dlp exits 0 when the video has none of the requested subtitles
/// ("There are no subtitles for the requested languages"), which for a user
/// who asked for text files means an empty folder, not a finished download.
/// Shared by the CLI (pipe readers) and the GUI (log drain).
#[derive(Default)]
pub struct SubtitleWatch {
    written: std::sync::atomic::AtomicBool,
    missing: std::sync::atomic::AtomicBool,
}

impl SubtitleWatch {
    /// Cheap to call for every child-output line: lines that do not mention
    /// subtitles at all leave after one case-insensitive scan.
    pub fn observe(&self, line: &str) {
        use std::sync::atomic::Ordering;
        if !contains_ignore_ascii_case(line, "subtitle") {
            return;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("[info] Writing video subtitles to: ")
            || trimmed.starts_with("[info] Video subtitle")
        {
            // A fresh write or a subtitle file that is already on disk:
            // either way the requested artifact exists.
            self.written.store(true, Ordering::Relaxed);
        } else if trimmed.starts_with("[info] There are no subtitles for the requested languages") {
            self.missing.store(true, Ordering::Relaxed);
        }
    }

    pub fn written(&self) -> bool {
        self.written.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn missing(&self) -> bool {
        self.missing.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// True when a finished run must be reported as a failure instead of
    /// success: subtitles-only requested, yt-dlp exited 0, it said none of
    /// the requested languages exist, and nothing was written.
    pub fn nothing_written(&self, fmt: Format, code: i32) -> bool {
        fmt == Format::Subs && code == 0 && self.missing() && !self.written()
    }
}

/// Decode one line of child-pipe bytes best-effort: strict UTF-8 first
/// (aria2c, and a non-frozen yt-dlp honoring our PYTHONIOENCODING=utf-8,
/// both emit UTF-8), falling back to the system ANSI code page - what the
/// frozen yt-dlp.exe actually writes (it ignores the env vars). Plain
/// from_utf8_lossy turned every ANSI special into U+FFFD: em-dashes in
/// video titles and, worse, Russian text inside yt-dlp error messages were
/// unreadable mojibake in the GUI log and the CLI echo.
pub fn decode_child_bytes(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => Cow::Owned(decode_codepage(bytes, ansi_codepage())),
    }
}

/// The Windows ANSI code page of this machine. Elsewhere children write UTF-8
/// and this fallback only needs to be deterministic, so it stays cp1251.
pub(super) fn ansi_codepage() -> u32 {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetACP() -> u32;
        }
        // SAFETY: argument-free Win32 query.
        unsafe { GetACP() }
    }
    #[cfg(not(windows))]
    {
        1251
    }
}

/// `bytes` in code page `codepage`, as UTF-8. cp1251 is a built-in table (no
/// allocation of an intermediate UTF-16 buffer on the common Russian setup);
/// any other Windows code page - an English or German Windows is no longer an
/// unusual host now that the interface is bilingual - goes through the OS.
pub(super) fn decode_codepage(bytes: &[u8], codepage: u32) -> String {
    #[cfg(windows)]
    if codepage != 1251 {
        if let Some(text) = windows_codepage_to_utf8(bytes, codepage) {
            return text;
        }
    }
    let _ = codepage;
    cp1251_to_utf8(bytes)
}

#[cfg(windows)]
pub(super) fn windows_codepage_to_utf8(bytes: &[u8], codepage: u32) -> Option<String> {
    extern "system" {
        fn MultiByteToWideChar(
            codepage: u32,
            flags: u32,
            source: *const u8,
            source_len: i32,
            destination: *mut u16,
            destination_len: i32,
        ) -> i32;
    }
    if bytes.is_empty() {
        return Some(String::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: both calls get a valid source slice and a destination whose
    // length is exactly what the first (sizing) call reported.
    unsafe {
        let needed = MultiByteToWideChar(codepage, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut wide = vec![0u16; needed as usize];
        let written =
            MultiByteToWideChar(codepage, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), needed);
        if written <= 0 {
            return None;
        }
        wide.truncate(written as usize);
        Some(String::from_utf16_lossy(&wide))
    }
}

/// Strip terminal escape sequences and invisible bidi/format characters
/// supplied by downloaded titles/filenames, preserving CR/LF so a progress
/// line can still update in place in the CLI. Чистую строку (типичный вывод
/// загрузчиков) возвращает заимствованной — без аллокации и копирования на
/// каждую строку.
pub fn sanitize_child_output(text: &str) -> Cow<'_, str> {
    let dirty = text.chars().any(|c| {
        c == '\x1b'
            || crate::ui::is_disguising_format(c)
            || (c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    });
    if !dirty {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for ch in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' || (ch == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if crate::ui::is_disguising_format(c) {
            // Invisible bidi/format controls let a remote title read as a
            // different one ("Track\u{202E}3pm.exe" shows as "Track exe.mp3").
            // They render as nothing, so drop them instead of echoing them.
        } else if matches!(c, '\r' | '\n' | '\t') || !c.is_control() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    Cow::Owned(out)
}

pub(super) fn cp1251_to_utf8(bytes: &[u8]) -> String {
    /// cp1251 0x80-0x9F (0x98 is undefined -> U+FFFD).
    const SPECIAL: [char; 32] = [
        '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}',
        '\u{040B}', '\u{040F}', '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{FFFD}', '\u{2122}', '\u{0459}', '\u{203A}',
        '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    ];
    // Every cp1251 byte can become three UTF-8 bytes (…, €, ™): a one-to-one
    // capacity reallocates on any line with a special character.
    let mut out = String::with_capacity(bytes.len().saturating_mul(3));
    for &b in bytes {
        let c = match b {
            0x00..=0x7F => b as char,
            0x80..=0x9F => SPECIAL[(b - 0x80) as usize],
            0xA8 => '\u{0401}', // Ё
            0xB8 => '\u{0451}', // ё
            // 0xC0-0xFF: the Cyrillic block, contiguous from U+0410.
            0xC0..=0xFF => char::from_u32(0x0410 + b as u32 - 0xC0).unwrap_or('\u{FFFD}'),
            // The rest of 0xA0-0xBF mostly matches Latin-1, but cp1251
            // differs on these bytes (e.g. 0xB3 is Ukrainian "і", not "³").
            0xA1 => '\u{040E}',
            0xA2 => '\u{045E}',
            0xA3 => '\u{0408}',
            0xA5 => '\u{0490}',
            0xAA => '\u{0404}',
            0xAF => '\u{0407}',
            0xB2 => '\u{0406}',
            0xB3 => '\u{0456}',
            0xB4 => '\u{0491}',
            0xB9 => '\u{2116}',
            0xBA => '\u{0454}',
            0xBC => '\u{0458}',
            0xBD => '\u{0405}',
            0xBE => '\u{0455}',
            0xBF => '\u{0457}',
            _ => char::from(b),
        };
        out.push(c);
    }
    out
}
