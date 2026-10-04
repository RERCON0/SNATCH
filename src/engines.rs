use std::borrow::Cow;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use fs4::{FileExt, TryLockError};
use sha2::{Digest, Sha256};

use crate::config::{config_dir, home_dir};
use crate::tools::{which, Toolchain};
use crate::ui::clip;

// --seed-time=0: without it aria2c finishes the download and then keeps
// seeding until ratio 1.0 (its default) - the process never exits, the GUI
// sits in "Running" at 100% forever and the CLI never returns the terminal.
pub const ARIA2_RESUME: &[&str] =
    &["-c", "--max-tries=10", "--retry-wait=2", "--auto-file-renaming=false", "--seed-time=0"];
pub const ARIA2_WORKERS: &[&str] = &["-x", "16", "-s", "16", "-k", "1M"];

pub const FILE_EXT: &[&str] = &[
    ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst", ".lz4", ".iso",
    ".exe", ".msi", ".msix", ".appx", ".apk", ".deb", ".rpm", ".dmg", ".pkg",
    ".whl", ".jar", ".bin", ".img", ".pdf", ".epub", ".fb2", ".txt", ".csv",
    ".mp4", ".mkv", ".webm", ".avi", ".mov", ".flv", ".ts", ".m4v",
    ".mp3", ".m4a", ".aac", ".flac", ".ogg", ".wav",
    ".jpg", ".jpeg", ".png", ".webp", ".gif",
];

pub const ALLOWED_SCHEMES: &[&str] = &["http", "https", "ftp"];
pub const TORRENT_FILES: &[&str] = &[".torrent", ".metalink", ".meta4"];

pub const ARIA2_MISSING_CONTROL_HINT: &str = "Файлы этой раздачи уже есть, но файл докачки .aria2 отсутствует. Чтобы не стереть данные, загрузка остановлена. Выберите новую пустую папку; не включайте перезапись существующих файлов.";

/// Поиск ASCII-подстроки без временной копии строки (в отличие от
/// `to_ascii_lowercase().contains(..)`): `needle` обязан быть непустым и уже
/// в нижнем регистре. Регистронезависимость ровно та же, что давал нижний
/// регистр: не-ASCII символы сравниваются побайтово, как и раньше.
pub fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    !needle.is_empty()
        && needle.len() <= hay.len()
        && hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

pub fn aria2_missing_control(line: &str) -> bool {
    contains_ignore_ascii_case(line, "errorcode=13")
        && contains_ignore_ascii_case(line, "control file")
        && contains_ignore_ascii_case(line, ".aria2")
        && contains_ignore_ascii_case(line, "does not exist")
}

pub const COOKIES_BROWSERS: &[&str] = &[
    "brave", "chrome", "chromium", "edge", "firefox", "opera", "safari", "vivaldi", "whale",
];

pub const ENGINE_LABELS: [(&str, &str); 2] = [
    ("yt-dlp", "yt-dlp — видео и стримы (YouTube и ещё тысячи сайтов)"),
    ("aria2", "aria2c — прямые ссылки, torrent, magnet"),
];

pub const FORMATS: [(&str, &str); 8] = [
    ("best", "Лучшее качество"),
    ("2160p", "Видео до 2160p (4K)"),
    ("1440p", "Видео до 1440p"),
    ("1080p", "Видео до 1080p"),
    ("720p", "Видео до 720p"),
    ("480p", "Видео до 480p"),
    ("audio", "Аудио — mp3 (перекодирование)"),
    ("audio-src", "Аудио — исходное (m4a/opus, без перекодирования)"),
];

const AUTH_HINTS: &[&str] = &[
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
    "cookies from a browser",
];

pub fn looks_like_auth(line: &str) -> bool {
    // Require a diagnostics context: both loaders prefix real problems
    // ("ERROR: ..." / "WARNING: ..." in yt-dlp, "... ERROR - ..." in aria2).
    // Without this, a filename or URI merely containing e.g. "forbidden"
    // ("https://host/forbidden-songs.mp3" echoed in an unrelated failure)
    // flips the auth heuristic and the app nags about browser cookies.
    let dominated = contains_ignore_ascii_case(line, "error")
        || contains_ignore_ascii_case(line, "warning");
    dominated && AUTH_HINTS.iter().any(|p| contains_ignore_ascii_case(line, p))
}

/// aria2's progress summary starts the line; finding `[#` anywhere would
/// mistake a yt-dlp Destination containing `[#gid ... (NN%)]` for progress.
fn aria2_summary(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let summary = &line[..=line.find(']')?];
    let (gid, rest) = summary.strip_prefix("[#")?.split_once(' ')?;
    if gid.is_empty() || !gid.bytes().all(|b| b.is_ascii_hexdigit())
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
            return Some(format!("{percent}% · {amount} · выделение места {}", alloc.replace('(', " (")));
        }
    }
    let field = |key: &str| {
        summary.split_whitespace().find_map(|part| part.strip_prefix(key))
            .unwrap_or("–").trim_end_matches(']')
    };
    let dl = field("DL:");
    let peers = field("CN:");
    let seeds = field("SD:");
    Some(format!("{percent}% · {amount} · ↓{dl}/с · сиды {seeds} · соединения {peers}"))
}

pub fn aria2_name_from_file(line: &str) -> Option<String> {
    let file = line.strip_prefix("FILE:")?.trim();
    let multi_file = file.ends_with("more)");
    let path = if multi_file { file.rsplit_once(" (")?.0 } else { file };
    let mut parts = path.rsplit(['/', '\\']);
    let file = parts.next()?.trim();
    let name = if multi_file { parts.next().unwrap_or(file) } else { file };
    (!name.is_empty()).then(|| name.to_string())
}

/// Short display name for a URL before anything better is known (a torrent's
/// real name only arrives once aria2 reads its metadata) - shared by the
/// CLI's batch display and the GUI's per-job row/log-prefix label.
pub fn batch_name(url: &str) -> String {
    if let Some(hash) = url.split_once("btih:").map(|(_, s)| s.split('&').next().unwrap_or(s)) {
        return format!("magnet {}", clip(hash, 12));
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let segment = path.rsplit('/').next().unwrap_or(path);
    // Generic route segments (YouTube's /watch, most players' /embed or
    // /index.*) aren't a video name - they're the same for every link, so
    // showing them as the job label is worse than showing nothing. Prefer
    // the id/v query parameter instead; the real title arrives later via
    // yt-dlp's own "Destination:" line (see ytdlp_title_from_line).
    const GENERIC_SEGMENTS: &[&str] = &["watch", "embed", "player", "index.html", "index.php", ""];
    if GENERIC_SEGMENTS.contains(&segment) {
        if let Some(query) = url.split_once('?').map(|(_, q)| q.split('#').next().unwrap_or(q)) {
            for pair in query.split('&') {
                if let Some((key, val)) = pair.split_once('=') {
                    if (key == "v" || key == "id") && !val.is_empty() {
                        return val.to_string();
                    }
                }
            }
        }
    }
    if segment.is_empty() { "загрузка".to_string() } else { segment.to_string() }
}

/// Pull the real media title out of a yt-dlp stdout line, once it announces
/// one - used to upgrade a job's label from `batch_name`'s URL guess (which,
/// for a `/watch` URL, is at best a video id) to the actual title, mirroring
/// how TorrentMeta/TorrentName upgrade an aria2/torrent job's label.
pub fn ytdlp_title_from_line(line: &str) -> Option<String> {
    let path = if let Some(rest) = line.strip_prefix("[download] Destination: ") {
        rest
    } else if let Some(rest) = line.strip_prefix("[download] ") {
        rest.strip_suffix(" has already been downloaded")?
    } else if let Some(rest) = line.strip_prefix("[Merger] Merging formats into \"") {
        rest.strip_suffix('"')?
    } else {
        return None;
    };
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    (!stem.is_empty()).then(|| stem.to_string())
}

pub struct RunResult {
    pub code: i32,
    pub auth_hint: bool,
}

/// Decode one line of child-pipe bytes best-effort: strict UTF-8 first
/// (aria2c, and a non-frozen yt-dlp honoring our PYTHONIOENCODING=utf-8,
/// both emit UTF-8), falling back to cp1251 - the Windows ANSI codepage the
/// frozen yt-dlp.exe actually writes (it ignores the env vars). Plain
/// from_utf8_lossy turned every cp1251 special into U+FFFD: em-dashes in
/// video titles and, worse, Russian text inside yt-dlp error messages were
/// unreadable mojibake in the GUI log and the CLI echo.
pub fn decode_child_bytes(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => Cow::Owned(cp1251_to_utf8(bytes)),
    }
}

/// Strip terminal escape sequences supplied by downloaded titles/filenames,
/// preserving CR/LF so a progress line can still update in place in the CLI.
/// Чистую строку (типичный вывод загрузчиков) возвращает заимствованной —
/// без аллокации и копирования на каждую строку.
pub fn sanitize_child_output(text: &str) -> Cow<'_, str> {
    let dirty = text
        .chars()
        .any(|c| c == '\x1b' || (c.is_control() && !matches!(c, '\r' | '\n' | '\t')));
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
                        if ('@'..='~').contains(&ch) { break; }
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
        } else if matches!(c, '\r' | '\n' | '\t') || !c.is_control() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    Cow::Owned(out)
}

fn relay_stdout(mut reader: impl Read, mut writer: impl Write) -> std::io::Result<()> {
    let mut chunk = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 { break; }
        for &b in &chunk[..n] {
            if b == b'\n' || b == b'\r' {
                writer.write_all(sanitize_child_output(&decode_child_bytes(&line)).as_bytes())?;
                writer.write_all(&[b])?;
                writer.flush()?;
                line.clear();
            } else {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        writer.write_all(sanitize_child_output(&decode_child_bytes(&line)).as_bytes())?;
        writer.flush()?;
    }
    Ok(())
}

fn cp1251_to_utf8(bytes: &[u8]) -> String {
    /// cp1251 0x80-0x9F (0x98 is undefined -> U+FFFD).
    const SPECIAL: [char; 32] = [
        '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}',
        '\u{040B}', '\u{040F}', '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{FFFD}', '\u{2122}', '\u{0459}', '\u{203A}',
        '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    ];
    let mut out = String::with_capacity(bytes.len());
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
            0xA1 => '\u{040E}', 0xA2 => '\u{045E}', 0xA3 => '\u{0408}',
            0xA5 => '\u{0490}', 0xAA => '\u{0404}', 0xAF => '\u{0407}',
            0xB2 => '\u{0406}', 0xB3 => '\u{0456}', 0xB4 => '\u{0491}',
            0xB9 => '\u{2116}', 0xBA => '\u{0454}', 0xBC => '\u{0458}',
            0xBD => '\u{0405}', 0xBE => '\u{0455}', 0xBF => '\u{0457}',
            _ => char::from(b),
        };
        out.push(c);
    }
    out
}

#[derive(Clone)]
pub struct Job {
    pub engine: String,
    pub url: String,
    pub out_dir: PathBuf,
    pub fmt: String,
    pub cookies_browser: Option<String>,
}

pub fn clean_url(raw: &str) -> String {
    raw.trim().trim_matches(['"', '\'']).trim().to_string()
}

fn has_control_chars(u: &str) -> bool {
    u.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
}

/// True for `\\server\share` / `//server/share` style paths (including the
/// `\\?\UNC\` verbatim form, which also starts with `\\`). Touching such a
/// path - even just `is_file()` - makes Windows perform SMB authentication
/// against the remote host, leaking the user's NetNTLMv2 hash (offline
/// cracking / relay), so UNC inputs are refused before any filesystem call.
/// Also true for the NT object-manager prefix `\??\` (and `/??/`): Win32
/// passes it through untouched, so `\??\UNC\host\share` reaches the same SMB
/// share without starting with two separators. Nobody types a local path
/// that way, so every `\??\` path is treated as one we must not touch.
fn is_unc_text(s: &str) -> bool {
    let bytes = s.as_bytes();
    let sep = |b: u8| matches!(b, b'\\' | b'/');
    let double = bytes.len() >= 2 && sep(bytes[0]) && sep(bytes[1]);
    let nt_prefix = bytes.len() >= 4 && sep(bytes[0]) && &bytes[1..3] == b"??" && sep(bytes[3]);
    double || nt_prefix
}

pub fn is_unc_path(p: &Path) -> bool {
    use std::path::{Component, Prefix};
    is_unc_text(&p.to_string_lossy())
        || matches!(
            p.components().next(),
            Some(Component::Prefix(pre))
                if matches!(pre.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
        )
}

fn scheme_of(u: &str) -> String {
    match u.find(':') {
        Some(i) if i >= 2 => {
            let s = &u[..i];
            let ok = s.starts_with(|c: char| c.is_ascii_alphabetic())
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.');
            if ok {
                return s.to_lowercase();
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn netloc_of(u: &str) -> String {
    let scheme = scheme_of(u);
    if scheme.is_empty() {
        return String::new();
    }
    let rest = &u[scheme.len() + 1..];
    let Some(rest) = rest.strip_prefix("//") else {
        return String::new();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest[..end].to_string()
}

fn path_from_url(u: &str) -> String {
    if let Some(idx) = u.find("://") {
        let rest = &u[idx + 3..];
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let after = &rest[auth_end..];
        let end = after.find(['?', '#']).unwrap_or(after.len());
        return after[..end].to_string();
    }
    let end = u.find(['?', '#']).unwrap_or(u.len());
    u[..end].to_string()
}

fn suffix_of(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or("");
    match file.rfind('.') {
        Some(i) if i > 0 => file[i..].to_lowercase(),
        _ => String::new(),
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn valid_xt_value(v: &str) -> bool {
    let Some(rest) = v.get(..4) else { return false };
    if !rest.eq_ignore_ascii_case("urn:") {
        return false;
    }
    let rest = &v[4..];
    let Some(i) = rest.find(':') else { return false };
    let scheme = &rest[..i];
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let hash = &rest[i + 1..];
    (16..=100).contains(&hash.len()) && hash.chars().all(|c| c.is_ascii_alphanumeric())
}

fn has_valid_xt(magnet: &str) -> bool {
    let query = magnet.split_once('?').map(|(_, q)| q).unwrap_or("");
    query.split('&').filter(|s| !s.is_empty()).any(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        k.eq_ignore_ascii_case("xt") && valid_xt_value(&percent_decode(v))
    })
}

pub fn validate_url(url: &str) -> Result<String, String> {
    let u = clean_url(url);
    if u.is_empty() {
        return Err("Пустая ссылка.".to_string());
    }
    if has_control_chars(&u) {
        return Err(
            "Ссылка содержит управляющие символы (перенос строки?) — скопируйте её заново."
                .to_string(),
        );
    }
    if u.starts_with('-') {
        return Err("Ссылка не может начинаться с «-» (похоже на опцию, а не на URL).".to_string());
    }
    // Must come before the .torrent is_file() probe below: stat'ing a UNC
    // path already authenticates to the remote host.
    if is_unc_text(&u) {
        return Err(
            "Сетевые UNC-пути (\\\\сервер\\шара) не поддерживаются: обращение к ним \
             отправляет ваши учётные данные Windows на чужой сервер. Сохраните \
             .torrent-файл локально или используйте http(s)/ftp/magnet-ссылку."
                .to_string(),
        );
    }
    if u.to_lowercase().starts_with("magnet:") {
        if !has_valid_xt(&u) {
            return Err(
                "Ссылка magnet повреждена (не найден корректный xt=urn:...). Похоже, часть \
                 символов «&» потерялась при вставке в терминал — вставьте ссылку ещё раз \
                 или передайте её через -y."
                    .to_string(),
            );
        }
        return Ok(u);
    }
    let scheme = scheme_of(&u);
    let netloc = netloc_of(&u);
    if ALLOWED_SCHEMES.contains(&scheme.as_str()) && !netloc.is_empty() {
        if netloc.contains('[') && !netloc.contains(']') {
            return Err(format!("Не получается разобрать ссылку: {u:?}"));
        }
        return Ok(u);
    }
    if TORRENT_FILES.contains(&suffix_of(&u).as_str()) && Path::new(&u).is_file() {
        return Ok(u);
    }
    Err(format!(
        "Не понимаю ссылку: {u:?} (нужен http(s)://, ftp://, magnet: или .torrent-файл)."
    ))
}

pub fn detect_engine(url: &str) -> &'static str {
    let u = clean_url(url);
    if scheme_of(&u) == "magnet" {
        return "aria2";
    }
    // A scheme-less local path may legally contain '#', so suffix checks for
    // it must use the whole string (path_from_url would truncate at '#').
    let suffix = if scheme_of(&u).is_empty() {
        suffix_of(&u)
    } else {
        // Percent-decode first: https://host/file%2Ezip is a .zip just like
        // the literal spelling and belongs to aria2.
        suffix_of(&percent_decode(&path_from_url(&u)))
    };
    if TORRENT_FILES.contains(&suffix.as_str()) || FILE_EXT.contains(&suffix.as_str()) {
        "aria2"
    } else {
        "yt-dlp"
    }
}

pub fn is_direct_download(url: &str) -> bool {
    let scheme = scheme_of(url);
    ALLOWED_SCHEMES.contains(&scheme.as_str())
        && FILE_EXT.contains(&suffix_of(&percent_decode(&path_from_url(url))).as_str())
}

pub fn is_bittorrent_input(url: &str) -> bool {
    if scheme_of(url) == "magnet" {
        return true;
    }
    let suffix = if scheme_of(url).is_empty() {
        suffix_of(url)
    } else {
        suffix_of(&percent_decode(&path_from_url(url)))
    };
    // metalink/meta4 files get the same no-preallocation + frequent
    // control-file saves as .torrent: both fill big files that would leak
    // preallocated bytes if the app dies mid-run.
    TORRENT_FILES.contains(&suffix.as_str())
}

pub fn validate_cookies_browser(value: &str) -> Result<(), String> {
    if has_control_chars(value) {
        return Err(format!(
            "Имя браузера для куки содержит управляющие символы: {value:?}"
        ));
    }
    let parts: Vec<&str> = value.split(['+', ':']).collect();
    let head = parts[0].to_lowercase();
    if !COOKIES_BROWSERS.contains(&head.as_str()) {
        return Err(format!(
            "Неизвестный браузер для куки: {value:?} (поддерживаются: {}; \
             профиль можно указать так: chrome:Profile 1)",
            COOKIES_BROWSERS.join(", ")
        ));
    }
    if parts[1..].iter().any(|p| p.contains("..")) {
        return Err(format!(
            "Профиль/контейнер для куки не может содержать «..»: {value:?}"
        ));
    }
    Ok(())
}

pub fn expanduser(p: &Path) -> PathBuf {
    if p.starts_with("~") {
        if let Some(home) = home_dir() {
            let mut out = home;
            for comp in p.components().skip(1) {
                out.push(comp);
            }
            return out;
        }
    }
    p.to_path_buf()
}

fn format_flags(fmt: &str) -> Vec<&'static str> {
    match fmt {
        "2160p" => vec!["-f", "bv*[height<=2160]+ba/b[height<=2160]"],
        "1440p" => vec!["-f", "bv*[height<=1440]+ba/b[height<=1440]"],
        "1080p" => vec!["-f", "bv*[height<=1080]+ba/b[height<=1080]"],
        "720p" => vec!["-f", "bv*[height<=720]+ba/b[height<=720]"],
        "480p" => vec!["-f", "bv*[height<=480]+ba/b[height<=480]"],
        "audio" => vec!["-x", "--audio-format", "mp3"],
        // bestaudio in its original container: no ffmpeg re-encode.
        "audio-src" => vec!["-f", "ba/b"],
        _ => vec![],
    }
}

/// Optional yt-dlp/aria2 behaviour carried in from the CLI/GUI. Deliberately
/// not part of `Job`: locks, owner marks and pause/resume key on the job
/// alone, so toggling these never changes a job's identity.
#[derive(Clone, Default)]
pub struct RunExtras {
    /// Download subtitles next to the video (manual + automatic captions).
    pub subs: bool,
    /// yt-dlp `--sub-langs` value; empty means the "ru,en" default.
    pub sub_langs: String,
    /// Follow playlists instead of refusing them.
    pub playlist: bool,
    /// Extra yt-dlp arguments, already split into argv tokens.
    pub extra_ytdlp: Vec<String>,
    /// Extra aria2c arguments, placed before the `--` terminator.
    pub extra_aria2: Vec<String>,
}

/// Splits a user-supplied "extra arguments" string into argv tokens: quotes
/// group, whitespace separates, nothing is expanded. The tokens go straight
/// to `Command::args` (no shell), so only what the user typed can run.
pub fn split_cli_args(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut token = false;
    for c in s.chars() {
        if c == '\0' {
            return Err("дополнительные аргументы содержат NUL".into());
        }
        match c {
            '"' | '\'' => { quoted = !quoted; token = true; }
            c if c.is_whitespace() && !quoted => {
                if token || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    token = false;
                }
            }
            c => { cur.push(c); token = true; }
        }
    }
    if quoted {
        return Err("незакрытая кавычка в дополнительных аргументах".into());
    }
    if token || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Absolute output directory without touching the filesystem (build() also
/// mkdirs it; preflight must not): shared by both for collision decisions.
fn absolute_out_dir(out_dir: &Path) -> PathBuf {
    let expanded = expanduser(out_dir);
    if is_unc_path(&expanded) || expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    }
}

/// File name aria2 saves a direct URL's payload under. aria2 takes the RAW
/// last path segment and percent-decodes it (`My%20File.zip` lands as
/// `My File.zip`, one pass only: `%2520` stays `%20`), but re-encodes the
/// characters it must not put on disk: separators always (`%2f`/`%2F` both
/// come back as `%2F`; its traversal probe saved `..%2F..%2Fpwned.bin`) and
/// the Windows-illegal set (`%3A` stays `%3A`). Reproduce that order so the
/// lock/origin records name the file aria2 actually writes; `build()` also
/// pins the result with `-o` for direct downloads, so a server-supplied
/// Content-Disposition name cannot make the file on disk differ either.
fn url_basename(url: &str) -> String {
    let raw = path_from_url(url);
    let raw = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if raw.is_empty() {
        return String::new();
    }
    let mut name = String::with_capacity(raw.len());
    for c in percent_decode(raw).chars() {
        match c {
            '/' => name.push_str("%2F"),
            '\\' => name.push_str("%5C"),
            _ if (cfg!(windows) && "<>:\"|?*".contains(c)) || c.is_control() => {
                name.push_str(&format!("%{:02X}", c as u32));
            }
            _ => name.push(c),
        }
    }
    if name == "." || name == ".." {
        return String::new();
    }
    name
}

/// Lock and ownership records live outside Downloads, shared by the CLI and
/// the GUI. A bare .aria2 file does NOT establish that its
/// source URL matches this job; aria2 -c can overwrite an unrelated partial.
fn aria2_state_dir() -> Result<PathBuf, String> {
    config_dir().map(|p| p.join("aria2-locks"))
}

/// NTFS compares file names without Unicode's context-sensitive final-sigma
/// lowercasing. Lowercase each scalar on its own: ΚΑΛΟΣ and καλοσ must hash
/// to the same lock even when a final Σ would become ς in str::to_lowercase.
pub(crate) fn ntfs_case_key(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

fn aria2_target_key(out: &Path, name: &str) -> String {
    let canonical = std::fs::canonicalize(out).unwrap_or_else(|_| out.to_path_buf());
    let path = canonical.to_string_lossy();
    let key = if cfg!(windows) { ntfs_case_key(&path) } else { path.into_owned() };
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    hash.update([0]);
    hash.update(if cfg!(windows) { ntfs_case_key(name) } else { name.to_string() }.as_bytes());
    format!("{:x}", hash.finalize())
}

fn aria2_origin_path(out: &Path, name: &str, state_dir: &Path) -> PathBuf {
    state_dir.join(format!("{}.origin", aria2_target_key(out, name)))
}

fn aria2_url_hash(url: &str) -> String {
    format!("{:x}", Sha256::digest(url.as_bytes()))
}

fn canonical_is_unc(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(path.components().next(), Some(Component::Prefix(p))
        if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))
}

fn aria2_owned_partial_in(url: &str, out: &Path, state_dir: &Path) -> bool {
    let name = url_basename(url);
    !name.is_empty()
        && out.join(&name).is_file()
        && out.join(format!("{name}.aria2")).is_file()
        && std::fs::read_to_string(aria2_origin_path(out, &name, state_dir))
            .is_ok_and(|fingerprint| fingerprint == aria2_url_hash(url))
}

fn aria2_owned_partial(url: &str, out: &Path) -> bool {
    aria2_state_dir().is_ok_and(|state| aria2_owned_partial_in(url, out, &state))
}

fn aria2_foreign_target(url: &str, out: &Path) -> bool {
    let name = url_basename(url);
    ALLOWED_SCHEMES.contains(&scheme_of(url).as_str())
        && !name.is_empty() && out.join(name).exists() && !aria2_owned_partial(url, out)
}

/// F1 (audit 4): a foreign pair - same-name file plus a `.aria2` control file
/// that our origin records cannot claim. aria2 auto-continues such a partial
/// even without `-c`, mixing another URL's bytes into the result, so the pair
/// must not be touched: refuse before the loader runs (delete is the user's
/// call, not ours). Returns the file name for messages.
fn aria2_foreign_pair(url: &str, out: &Path) -> Option<String> {
    if !ALLOWED_SCHEMES.contains(&scheme_of(url).as_str()) {
        return None;
    }
    let name = url_basename(url);
    if name.is_empty() {
        return None;
    }
    (out.join(&name).is_file()
        && out.join(format!("{name}.aria2")).is_file()
        && !aria2_owned_partial(url, out))
        .then_some(name)
}

/// Hold the OS lock from before build() until the child process exits. The
/// GUI tries once (never block its render thread); CLI jobs wait their turn.
/// Stable lock files are never deleted, or another process could lock a new
/// inode while an earlier download still holds the old one.
pub fn lock_aria2_target(job: &Job, wait: bool) -> Result<Option<std::fs::File>, String> {
    if job.engine != "aria2" || !ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str()) { return Ok(None); }
    lock_aria2_target_in(job, wait, &aria2_state_dir()?)
}

fn lock_aria2_target_in(job: &Job, wait: bool, state_dir: &Path) -> Result<Option<std::fs::File>, String> {
    if job.engine != "aria2" || !ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str()) {
        return Ok(None);
    }
    let name = url_basename(&job.url);
    if name.is_empty() { return Ok(None); }
    let out = absolute_out_dir(&job.out_dir);
    if is_unc_path(&out) || is_unc_path(state_dir) {
        return Err("Сетевая UNC-папка не подходит для загрузки или блокировок aria2".into());
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("папка загрузки aria2: {e}"))?;
    if canonical_is_unc(&std::fs::canonicalize(&out).map_err(|e| format!("папка загрузки aria2: {e}"))?) {
        return Err("Папка загрузки aria2 ведёт на сетевой UNC-путь".into());
    }
    std::fs::create_dir_all(state_dir).map_err(|e| format!("папка блокировок aria2: {e}"))?;
    let key = aria2_target_key(&out, &name);
    let lock = std::fs::OpenOptions::new().create(true).read(true).write(true)
        .truncate(false).open(state_dir.join(format!("{key}.lock")))
        .map_err(|e| format!("блокировка aria2: {e}"))?;
    if wait {
        FileExt::lock(&lock).map_err(|e| format!("блокировка aria2: {e}"))?;
    } else {
        match FileExt::try_lock(&lock) {
            Ok(()) => {},
            Err(TryLockError::WouldBlock) => return Err(format!(
                "Файл «{name}» уже скачивается в эту папку — повторите после завершения"
            )),
            Err(TryLockError::Error(e)) => return Err(format!("блокировка aria2: {e}")),
        }
    }
    // Claim only an empty slot. Never certify an existing file or .aria2 as
    // ours just because the URL currently points at the same basename.
    if !out.join(&name).exists() && !out.join(format!("{name}.aria2")).exists() {
        std::fs::write(aria2_origin_path(&out, &name, state_dir), aria2_url_hash(&job.url))
            .map_err(|e| format!("метка докачки aria2: {e}"))?;
    }
    Ok(Some(lock))
}

/// A fresh run never needs `-c`: auto-renaming stays on even when the file
/// did not exist at build time, closing the build→spawn collision race.
/// Resume is allowed only for a partial this app recorded for the SAME URL.
fn aria2_flags(owned_partial: bool) -> Vec<&'static str> {
    if owned_partial {
        ARIA2_RESUME.to_vec()
    } else {
        ARIA2_RESUME
            .iter().copied()
            .filter(|f| *f != "-c" && *f != "--auto-file-renaming=false")
            .chain(std::iter::once("--auto-file-renaming=true"))
            .collect()
    }
}

pub fn build(job: &Job, tc: &Toolchain) -> Result<Vec<OsString>, String> {
    build_with(job, tc, &RunExtras::default())
}

pub fn build_with(job: &Job, tc: &Toolchain, extras: &RunExtras) -> Result<Vec<OsString>, String> {
    // Нативные движки не строят внешнюю команду; без этого guard'а они молча
    // ушли бы по yt-dlp-ветке (else ниже) и получили бы чужие флаги.
    if job.engine != "yt-dlp" && job.engine != "aria2" {
        return Err(format!("Неизвестный движок: {:?}", job.engine));
    }
    if job.engine == "yt-dlp" {
        if let Some(cb) = &job.cookies_browser {
            validate_cookies_browser(cb)?;
        }
    }

    let out = absolute_out_dir(&job.out_dir);
    // Before create_dir_all: even a failed mkdir on a UNC path performs SMB
    // authentication against the remote host (NetNTLMv2 leak), and a
    // *successful* one would silently write the user's downloads to a
    // stranger's share (poisoned config/history or a pasted path).
    if is_unc_path(&out) {
        return Err(format!(
            "Папка сохранения {} — сетевой UNC-путь. Выберите локальную папку \
             (UNC отклоняется, чтобы Windows не отправляла учётные данные на \
             чужой сервер).",
            out.display()
        ));
    }
    std::fs::create_dir_all(&out)
        .map_err(|e| format!("Не удалось создать папку {}: {e}", out.display()))?;

    let mut cmd: Vec<OsString> = Vec::new();
    if job.engine == "aria2" {
        cmd.push(tc.require("aria2c")?.into_os_string());
        cmd.push("--no-conf".into());
        cmd.push("-d".into());
        cmd.push(out.clone().into_os_string());
        // aria2 picks the saved name itself: the percent-decoded URL
        // basename, or even the server's Content-Disposition. Pin it with -o
        // so the name the lock/origin records were computed for is exactly
        // the file aria2 writes - otherwise a foreign same-name pair slips
        // past aria2_foreign_pair and aria2 continues a stranger's bytes.
        if !is_bittorrent_input(&job.url) {
            let name = url_basename(&job.url);
            if !name.is_empty() {
                cmd.push("-o".into());
                cmd.push(name.into());
            }
        }
        // F1: a same-name file WITH a .aria2 control file is auto-continued by
        // aria2 even without -c, silently mixing two sources' bytes. Our own
        // partials carry an origin fingerprint; anything else must stop here.
        if let Some(name) = aria2_foreign_pair(&job.url, &out) {
            return Err(format!(
                "«{name}» и его файл докачки .aria2 оставлены прерванной загрузкой другого URL — \
                 aria2 продолжил бы чужой файл. Проверьте и удалите их (или выберите другую \
                 папку), после чего повторите."
            ));
        }
        // -c only affects HTTP/FTP; keep torrent/magnet options as before.
        let resume = !ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str())
            || aria2_owned_partial(&job.url, &out);
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
        for a in &extras.extra_aria2 {
            cmd.push(a.clone().into());
        }
        cmd.push("--".into());
        cmd.push(OsString::from(&job.url));
        return Ok(cmd);
    }

    cmd.push(tc.require("yt-dlp")?.into_os_string());
    cmd.push("--ignore-config".into());
    cmd.push("-P".into());
    cmd.push(out.clone().into_os_string());
    cmd.push(OsString::from(if extras.playlist { "--yes-playlist" } else { "--no-playlist" }));
    if extras.subs {
        cmd.push("--write-subs".into());
        cmd.push("--write-auto-subs".into());
        cmd.push("--sub-langs".into());
        let langs = if extras.sub_langs.trim().is_empty() { "ru,en" } else { extras.sub_langs.trim() };
        cmd.push(langs.into());
    }
    for a in &extras.extra_ytdlp {
        cmd.push(a.clone().into());
    }
    if let Some(cb) = &job.cookies_browser {
        cmd.push("--cookies-from-browser".into());
        cmd.push(cb.into());
    }
    for a in format_flags(&job.fmt) {
        cmd.push(a.into());
    }
    if let Some(aria2c) = &tc.aria2c {
        if is_direct_download(&job.url) {
            // F1: aria2 auto-continues a same-name foreign partial even
            // without -c; refuse instead of mixing two sources' bytes.
            if let Some(name) = aria2_foreign_pair(&job.url, &out) {
                return Err(format!(
                    "«{name}» и его файл докачки .aria2 оставлены прерванной загрузкой другого \
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
                format!("{} {} --no-conf", aria2_flags(false).join(" "), ARIA2_WORKERS.join(" "))
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
/// from the `--summary-interval` console output instead.)
///
/// [`Choice`]: crate::torrent::Choice
pub fn build_for_run(job: &Job, tc: &Toolchain, choice: &crate::torrent::Choice) -> Result<Vec<OsString>, String> {
    build_for_run_with(job, tc, choice, &RunExtras::default())
}

pub fn build_for_run_with(
    job: &Job,
    tc: &Toolchain,
    choice: &crate::torrent::Choice,
    extras: &RunExtras,
) -> Result<Vec<OsString>, String> {
    let mut cmd = build_with(job, tc, extras)?;
    if job.engine != "aria2" || !is_bittorrent_input(&job.url) {
        return Ok(cmd);
    }
    // build() ends with ["--", url]: the input stays after the terminator and
    // every flag goes before it, so aria2c never treats the input as an
    // option argument.
    if let Some(meta) = &choice.meta {
        if meta.path().is_file() {
            meta.touch();
            if let Some(input) = cmd.last_mut() {
                *input = meta.path().as_os_str().to_os_string();
            }
        } else if choice.files.is_some() && scheme_of(&job.url) != "magnet" {
            // Gone (a temp cleaner during a long pause). A magnet pins its
            // content by hash, so aria2 can simply fetch it again; an http(s)
            // .torrent may have changed since, and the picked indices would
            // then select different files.
            return Err("файлы выбирались по .torrent, которого больше нет — добавьте ссылку и выберите файлы заново".into());
        }
    }
    if let Some(sel) = &choice.files {
        if let Some(spec) = crate::torrent::select_spec(sel)? {
            let insert_at = cmd.len().saturating_sub(2);
            cmd.insert(insert_at, format!("--select-file={spec}").into());
        }
    }
    Ok(cmd)
}

pub fn preflight_warning(job: &Job) -> Vec<String> {
    // Helper presence does not change mid-process; probe once per run.
    static HAS_FFMPEG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static HAS_JS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let has_ffmpeg = *HAS_FFMPEG.get_or_init(|| bootstrap_or_path_exists("ffmpeg"));
    let has_js = *HAS_JS.get_or_init(|| bootstrap_or_path_exists("deno"));
    preflight_warning_with_ffmpeg(job, job.engine == "yt-dlp" && has_ffmpeg, job.engine == "yt-dlp" && has_js)
}

/// True when the tool is in the private bin dir (setup installs ffmpeg and
/// Deno there next to yt-dlp) or on PATH - the two places yt-dlp itself
/// looks for them. `prepend_bootstrap_path` puts the same dir on the child's
/// PATH, so what is detected here is also what yt-dlp will find.
fn bootstrap_or_path_exists(name: &str) -> bool {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    which(name).is_some() || crate::tools::bootstrap_dir().is_ok_and(|b| b.join(&file).is_file())
}

/// The private bin dir must be on the child's PATH: yt-dlp finds ffmpeg and
/// the JS runtime (Deno) only through PATH, not next to its own exe.
pub fn prepend_bootstrap_path(command: &mut std::process::Command) {
    let Ok(bin) = crate::tools::bootstrap_dir() else { return };
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

fn preflight_warning_with_ffmpeg(job: &Job, has_ffmpeg: bool, has_js: bool) -> Vec<String> {
    let mut warns = Vec::new();
    if job.engine != "yt-dlp" {
        if job.cookies_browser.is_some() {
            warns.push(
                "Куки из браузера применимы только к yt-dlp — для этого движка они проигнорированы."
                    .to_string(),
            );
        }
        if job.fmt != "best" {
            warns.push("Формат применим только к yt-dlp — для aria2 он проигнорирован.".to_string());
        }
    } else {
        if !has_ffmpeg {
            if job.fmt == "audio" {
                warns.push("Не найден ffmpeg — извлечение mp3 может не сработать.".to_string());
            } else {
                warns.push(
                    "Не найден ffmpeg — склейка видео и аудио недоступна, yt-dlp выберет \
                      однодорожный формат (качество может быть ниже)."
                        .to_string(),
                );
            }
        }
        if !has_js {
            warns.push(
                "Не найден JS-рантайм (Deno) — на YouTube часть форматов может быть \
                  недоступна. Установите загрузчики кнопкой в GUI или `snatch --install-tools`."
                    .to_string(),
            );
        }
    }
    // E-1/F1: collision feedback. A bare file is renamed around; a foreign
    // file+control pair stops the job (aria2 would auto-continue it).
    let out = absolute_out_dir(&job.out_dir);
    if is_unc_path(&out) {
        warns.push("Сетевая UNC-папка не поддерживается — выберите локальную папку.".into());
        return warns;
    }
    if job.engine == "aria2" || (job.engine == "yt-dlp" && is_direct_download(&job.url)) {
        if let Some(name) = aria2_foreign_pair(&job.url, &out) {
            warns.push(format!(
                "В папке есть «{name}» с данными докачки от другой загрузки — загрузка будет \
                 остановлена, чтобы aria2 не продолжил чужой файл. Проверьте и удалите их \
                 (или выберите другую папку)."
            ));
        } else if aria2_foreign_target(&job.url, &out) {
            warns.push(
                "В папке уже есть файл с таким же именем, но без данных докачки — он не будет \
                 перезаписан: новая загрузка сохранится рядом под другим именем."
                    .to_string(),
            );
        }
    }
    warns
}

pub fn run(cmd: &[OsString], capture_stderr: bool) -> RunResult {
    let Some(program) = cmd.first() else {
        return RunResult { code: 127, auth_hint: false };
    };
    let install_guard = match crate::tools::lock_for_spawn(Path::new(program)) {
        Ok(guard) => guard,
        Err(e) => { crate::errln(e); return RunResult { code: 127, auth_hint: false }; }
    };
    let mut command = Command::new(program);
    command
        .args(&cmd[1..])
        // Same nudge as the GUI: makes a *non-frozen* (python-based) yt-dlp
        // emit UTF-8 into pipes; the frozen yt-dlp.exe ignores it, which the
        // lossy decode below tolerates.
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, proc_job) = match crate::job_object::spawn(&mut command, false) {
        Ok(result) => result,
        Err(e) => {
            crate::errln(format!("✘ Защита дерева загрузчика: {e}"));
            return RunResult { code: 127, auth_hint: false };
        }
    };
    drop(install_guard);
    let stdout = child.stdout.take().expect("stdout piped");
    let stdout_thread = std::thread::spawn(move || relay_stdout(stdout, std::io::stdout()));
    let stderr = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        let mut auth_hint = false;
        if let Some(err) = stderr {
            let mut reader = BufReader::new(err);
            let mut sink = std::io::stderr();
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                // Decode before detecting hints: frozen yt-dlp may emit cp1251.
                let line = decode_child_bytes(&buf);
                if capture_stderr && !auth_hint && looks_like_auth(&line) {
                    auth_hint = true;
                }
                // Neutralize terminal escapes before echoing server text.
                let cleaned = sanitize_child_output(&line);
                let _ = sink.write_all(cleaned.as_bytes());
                let _ = sink.flush();
            }
        }
        auth_hint
    });
    let code = match child.wait() {
        Ok(status) => status.code().unwrap_or(130),
        Err(_) => 127,
    };
    if let Some(job) = &proc_job { job.terminate(); }
    match stdout_thread.join() {
        Ok(Ok(())) => {},
        Ok(Err(e)) => crate::errln(format!("⚠ Не удалось передать вывод загрузчика: {e}")),
        Err(_) => crate::errln("⚠ Поток stdout загрузчика завершился с паникой"),
    }
    let auth_hint = stderr_thread.join().unwrap_or_else(|_| {
        crate::errln("⚠ Поток stderr загрузчика завершился с паникой; подсказка авторизации недоступна"); false
    });
    RunResult { code, auth_hint }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_out() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("snatch-rs-test-{}-{}", std::process::id(), n))
    }

    fn tc_both() -> Toolchain {
        Toolchain {
            yt_dlp: Some(PathBuf::from("yt-dlp")),
            aria2c: Some(PathBuf::from("aria2c")),
        }
    }

    fn tc_ytdlp() -> Toolchain {
        Toolchain { yt_dlp: Some(PathBuf::from("yt-dlp")), aria2c: None }
    }

    fn tc_aria2() -> Toolchain {
        Toolchain { yt_dlp: None, aria2c: Some(PathBuf::from("aria2c")) }
    }

    const MAGNET_OK: &str = "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4";

    #[test]
    fn validate_accepts_supported() {
        assert_eq!(
            validate_url("https://example.com/v.mp4").unwrap(),
            "https://example.com/v.mp4"
        );
        assert_eq!(validate_url(&format!("  {MAGNET_OK} ")).unwrap(), MAGNET_OK);
        assert_eq!(validate_url("ftp://host/file.zip").unwrap(), "ftp://host/file.zip");
    }

    #[test]
    fn validate_accepts_magnet_with_tracker_and_name() {
        let magnet = format!("{MAGNET_OK}&tr=http%3A%2F%2Fbt.example.org%2Fann&dn=Some%20Name");
        assert_eq!(validate_url(&magnet).unwrap(), magnet);
    }

    #[test]
    fn validate_rejects_corrupted_magnet() {
        let corrupted = "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4Btr=\
             http%3A%2F%2Fbt.example.org%2Fanndn=Name";
        assert!(validate_url(corrupted).is_err());
    }

    #[test]
    fn validate_rejects_magnet_without_xt() {
        assert!(validate_url("magnet:?dn=NoHashHere").is_err());
    }

    #[test]
    fn validate_magnet_xt_case_insensitive() {
        let m = "magnet:?XT=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4B";
        assert_eq!(validate_url(m).unwrap(), m);
    }

    #[test]
    fn validate_magnet_percent_encoded_xt() {
        let m = "magnet:?xt=urn%3Abtih%3A73510898AF9039563184FAFE9CB0F186DE6AAA4B";
        assert!(validate_url(m).is_ok());
    }

    #[test]
    fn validate_rejects_unsupported_schemes() {
        for bad in ["file:///etc/passwd", "ws://host/x", "sftp://host/x", "ftps://host/x"] {
            assert!(validate_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validate_rejects_option_like() {
        assert!(validate_url("-x").is_err());
        assert!(validate_url("   ").is_err());
        assert!(validate_url("\"\"").is_err());
    }

    #[test]
    fn validate_rejects_control_chars() {
        assert!(validate_url("https://host.com/pa\nth").is_err());
        assert!(validate_url("https://host.com/pa\u{0}th").is_err());
    }

    #[test]
    fn validate_torrent_file() {
        let dir = tmp_out();
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.torrent");
        std::fs::write(&f, b"d4:infod0:e").unwrap();
        assert_eq!(validate_url(f.to_str().unwrap()).unwrap(), f.to_str().unwrap());
        let missing = dir.join("missing.torrent");
        assert!(validate_url(missing.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_rejects_unc_paths() {
        // stat'ing these would leak the user's NetNTLMv2 hash to "evil.com".
        // Assert the *guard's* message, not just is_err(): a missing guard
        // also fails via the unreachable host, so is_err() alone could pass
        // while the SMB stat this test exists to prevent actually happened.
        for bad in [
            r"\\evil.com\share\movie.torrent",
            "//evil.com/share/movie.torrent",
            r"\/evil.com\share\movie.torrent",
            r"/\evil.com\share\movie.torrent",
            r"\\?\UNC\evil.com\share\movie.torrent",
            // NT prefix: Win32 passes "\??\" straight through, so this
            // reaches \\evil.com\share without a leading double separator.
            r"\??\UNC\evil.com\share\movie.torrent",
            "/??/UNC/evil.com/share/movie.torrent",
        ] {
            assert!(is_unc_path(Path::new(bad)), "{bad}");
            let err = validate_url(bad).unwrap_err();
            assert!(err.contains("UNC"), "{bad}: {err}");
        }
    }

    #[test]
    fn ntfs_key_does_not_depend_on_final_sigma_context() {
        assert_eq!(ntfs_case_key("ΚΑΛΟΣ"), ntfs_case_key("καλοσ"));
        assert_eq!(ntfs_case_key("ΚΑΛΟΣ"), "καλοσ");
        #[cfg(windows)]
        {
            // An absolute directory avoids other parallel tests temporarily
            // changing the process-wide current working directory.
            let out = std::env::current_exe().unwrap();
            let out = out.parent().unwrap();
            assert_eq!(aria2_target_key(out, "ΚΑΛΟΣ"), aria2_target_key(out, "καλοσ"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn ntfs_sigma_aliases_share_a_key_for_the_same_actual_file() {
        let dir = tmp_out(); std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ΚΑΛΟΣ.mp3"), b"same file").unwrap();
        for name in ["καλοσ.mp3", "ΚΑΛΟΣ.mp3"] {
            assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"same file");
            assert_eq!(aria2_target_key(&dir, name), aria2_target_key(&dir, "ΚΑΛΟΣ.mp3"));
        }
        if dir.join("καλος.mp3").exists() {
            assert_eq!(aria2_target_key(&dir, "καλος.mp3"), aria2_target_key(&dir, "ΚΑΛΟΣ.mp3"));
        } else {
            // This volume distinguishes final sigma. Do not merge two
            // different files' .origin records merely by linguistic folding.
            std::fs::write(dir.join("καλος.mp3"), b"different file").unwrap();
            assert_ne!(aria2_target_key(&dir, "καλος.mp3"), aria2_target_key(&dir, "ΚΑΛΟΣ.mp3"));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn preflight_rejects_unc_before_collision_probes() {
        for path in [r"\\127.0.0.1\snatch-test", r"\??\UNC\127.0.0.1\snatch-test"] {
            let job = Job { engine: "aria2".into(), url: "https://example.test/video.mp4".into(),
                out_dir: path.into(), fmt: "best".into(), cookies_browser: None };
            let warnings = preflight_warning_with_ffmpeg(&job, true, true);
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("UNC"));
        }
    }

    #[test]
    fn build_rejects_unc_out_dir() {
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://youtube.com/watch?v=1".into(),
            out_dir: PathBuf::from(r"\\evil.com\share"),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let err = build(&job, &tc_ytdlp()).unwrap_err();
        assert!(err.contains("UNC"), "{err}");
        let job = Job { out_dir: PathBuf::from("//evil.com/share"), ..job };
        let err = build(&job, &tc_ytdlp()).unwrap_err();
        assert!(err.contains("UNC"), "{err}");
    }

    #[test]
    fn local_path_engine_detection_ignores_hash_and_supports_metalink() {
        // '#' is legal in local filenames; it must not truncate the suffix.
        assert_eq!(detect_engine("S01 #2.torrent"), "aria2");
        assert_eq!(detect_engine("movie.meta4"), "aria2");
        assert_eq!(detect_engine(r"C:\Torrents\[DL] Cuphead #1.torrent"), "aria2");
    }

    fn args_of(cmd: &[OsString]) -> Vec<String> {
        cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn aria2_never_resumes_a_foreign_same_name_file() {
        // E-1 (offensive audit): `-c` must not run on a same-name file that
        // has no `.aria2` control data of its own - that would silently
        // overwrite somebody else's file.
        let out = tmp_out();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("file.bin"), b"old data").unwrap();
        let job = Job {
            engine: "aria2".into(),
            url: "https://host/file.bin".into(),
            out_dir: out.clone(), fmt: "best".into(), cookies_browser: None,
        };
        let s = args_of(&build(&job, &tc_aria2()).unwrap());
        assert!(!s.contains(&"-c".to_string()), "{s:?}");
        assert!(s.contains(&"--auto-file-renaming=true".to_string()), "{s:?}");
        assert!(s.contains(&"--max-tries=10".to_string()), "{s:?}");
        assert!(s.contains(&"--seed-time=0".to_string()), "{s:?}");
        assert_eq!(std::fs::read(out.join("file.bin")).unwrap(), b"old data");
        assert!(preflight_warning(&job).iter().any(|w| w.contains("не будет перезаписан")));

        // A stray .aria2 is not proof of ownership: it could be another URL's.
        // aria2 auto-continues such a file+control pair EVEN WITHOUT -c
        // (audit F1), so the build must refuse rather than let two sources'
        // bytes mix - for the aria2 engine and the external-downloader branch.
        std::fs::write(out.join("file.bin.aria2"), b"ctl").unwrap();
        let err = build(&job, &tc_aria2()).unwrap_err();
        assert!(err.contains("докачки"), "{err}");
        assert!(preflight_warning(&job).iter().any(|w| w.contains("будет остановлена")));
        let as_ytdlp = Job { engine: "yt-dlp".into(), ..job.clone() };
        assert!(build(&as_ytdlp, &tc_both()).is_err());
        std::fs::remove_file(out.join("file.bin.aria2")).unwrap();

        // yt-dlp's external-downloader branch gets the collision-safe args too.
        let s = args_of(&build(&as_ytdlp, &tc_both()).unwrap());
        let i = s.iter().position(|a| a == "--external-downloader-args").unwrap();
        let args = &s[i + 1];
        assert!(args.contains("--auto-file-renaming=true"), "{args}");
        assert!(!args.contains("-c "), "{args}");
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn aria2_no_collision_starts_without_continue_even_if_file_appears_after_build() {
        let out = tmp_out();
        let job = Job {
            engine: "aria2".into(),
            url: "https://host/fresh.bin".into(),
            out_dir: out.clone(), fmt: "best".into(), cookies_browser: None,
        };
        let s = args_of(&build(&job, &tc_aria2()).unwrap());
        assert!(!s.contains(&"-c".to_string()), "{s:?}");
        assert!(s.contains(&"--auto-file-renaming=true".to_string()), "{s:?}");
        assert!(!preflight_warning(&job).iter().any(|w| w.contains("не будет перезаписан")));
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn percent_encoded_names_are_decoded_like_aria2() {
        // aria2 decodes the raw last segment once (`%20` -> space) but
        // re-encodes separators (uppercase) and, on Windows, illegal chars.
        assert_eq!(url_basename("https://h/b/My%20File.zip"), "My File.zip");
        assert_eq!(url_basename("https://h/b/a%2fb.bin"), "a%2Fb.bin");
        assert_eq!(url_basename("https://h/b/d%2520.bin"), "d%20.bin");
        assert_eq!(url_basename("https://h/b/..%2F..%2Fpwned.bin"), "..%2F..%2Fpwned.bin");
        assert_eq!(url_basename("https://h/b/"), "");
        assert_eq!(url_basename("https://h/b/%2E%2E"), "");
        #[cfg(windows)]
        assert_eq!(url_basename("https://h/b/cv%3Aads.bin"), "cv%3Aads.bin");
    }

    #[test]
    fn a_foreign_pair_under_an_encoded_name_is_caught_and_the_name_is_pinned() {
        let out = tmp_out();
        std::fs::create_dir_all(&out).unwrap();
        let url = "https://h/b/My%20File.zip";
        let name = "My File.zip";
        std::fs::write(out.join(name), b"foreign").unwrap();
        std::fs::write(out.join(format!("{name}.aria2")), b"ctl").unwrap();
        assert_eq!(aria2_foreign_pair(url, &out).as_deref(), Some(name));
        let job = Job {
            engine: "aria2".into(), url: url.into(),
            out_dir: out.clone(), fmt: "best".into(), cookies_browser: None,
        };
        assert!(build(&job, &tc_aria2()).is_err());
        assert!(preflight_warning(&job).iter().any(|w| w.contains("будет остановлена")));
        std::fs::remove_file(out.join(format!("{name}.aria2"))).unwrap();

        // Without the pair the download may start, but -o must pin the
        // decoded name so lock/origin records and the file on disk agree.
        let s = args_of(&build(&job, &tc_aria2()).unwrap());
        let i = s.iter().position(|a| a == "-o").expect("-o missing");
        assert_eq!(s[i + 1], name);
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn an_encoded_url_resumes_only_its_own_decoded_partial() {
        let out = tmp_out();
        let state = std::env::temp_dir().join(format!("snatch-owned-enc-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let url = "https://h/b/My%20File.zip";
        let name = url_basename(url);
        std::fs::write(out.join(&name), b"part").unwrap();
        std::fs::write(out.join(format!("{name}.aria2")), b"ctl").unwrap();
        assert!(!aria2_owned_partial_in(url, &out, &state));
        std::fs::write(aria2_origin_path(&out, &name, &state), aria2_url_hash(url)).unwrap();
        assert!(aria2_owned_partial_in(url, &out, &state));
        assert!(!aria2_owned_partial_in("https://h/b/Other%20File.zip", &out, &state));
        std::fs::remove_dir_all(&out).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn aria2_lock_claims_only_an_empty_target_and_requires_the_same_source_to_resume() {
        let out = tmp_out();
        let state = out.join("state");
        let job = Job { engine: "aria2".into(), url: "https://one.example/file.bin".into(),
            out_dir: out.clone(), fmt: "best".into(), cookies_browser: None };
        let lock = lock_aria2_target_in(&job, false, &state).unwrap().unwrap();
        assert!(lock_aria2_target_in(&job, false, &state).is_err(), "concurrent jobs must not share a basename");
        std::fs::write(out.join("file.bin"), b"partial").unwrap();
        std::fs::write(out.join("file.bin.aria2"), b"control").unwrap();
        assert!(aria2_owned_partial_in(&job.url, &out, &state));
        assert!(aria2_flags(true).contains(&"-c"));
        drop(lock);

        let other = Job { url: "https://other.example/file.bin".into(), ..job };
        let lock = lock_aria2_target_in(&other, false, &state).unwrap().unwrap();
        assert!(!aria2_owned_partial_in(&other.url, &out, &state));
        assert!(!aria2_flags(false).contains(&"-c"));
        drop(lock);
        assert!(aria2_owned_partial_in("https://one.example/file.bin", &out, &state),
            "a foreign URL must not re-claim the existing partial");
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn clean_url_strips_quotes() {
        assert_eq!(clean_url("  \"https://x\"  "), "https://x");
        assert_eq!(clean_url("'https://x'"), "https://x");
    }

    #[test]
    fn detect_engine_basics() {
        assert_eq!(detect_engine("magnet:?xt=1"), "aria2");
        assert_eq!(detect_engine("https://youtube.com/watch?v=1"), "yt-dlp");
        assert_eq!(detect_engine("https://host.com/file.zip"), "aria2");
        assert_eq!(detect_engine("https://host.com/file.MP4?x=1"), "aria2");
        assert_eq!(detect_engine("https://host/x.deb"), "aria2");
    }

    #[test]
    fn detect_engine_torrent_goes_to_aria2() {
        assert_eq!(detect_engine("C:\\torrents\\a.torrent"), "aria2");
        assert_eq!(detect_engine("a.torrent"), "aria2");
        assert_eq!(detect_engine("https://host/x.torrent"), "aria2");
        assert_eq!(detect_engine("https://host/x.meta4"), "aria2");
    }

    #[test]
    fn detect_engine_malformed_url_no_crash() {
        assert_eq!(detect_engine("https://[::1"), "yt-dlp");
        assert_eq!(detect_engine("https://[bad"), "yt-dlp");
    }

    #[test]
    fn validate_rejects_unparseable_url() {
        assert!(validate_url("https://[::1").is_err());
        assert!(validate_url("https://[bad").is_err());
        assert!(validate_url("https://[::1]/path").is_ok());
    }

    #[test]
    fn is_direct_download_basics() {
        assert!(is_direct_download("https://host/file.zip"));
        assert!(is_direct_download("ftp://host/file.iso"));
        assert!(!is_direct_download("https://youtube.com/watch?v=1"));
        assert!(!is_direct_download("magnet:?xt=1"));
        assert!(!is_direct_download("https://host/page.html"));
    }

    #[test]
    fn build_aria2() {
        let out = tmp_out();
        let job = Job {
            engine: "aria2".into(),
            url: "magnet:?xt=1".into(),
            out_dir: out.clone(),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let cmd = build(&job, &tc_aria2()).unwrap();
        let s: Vec<String> = cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert_eq!(s[1], "--no-conf");
        assert_eq!(s[s.iter().position(|x| x == "-d").unwrap() + 1], out.to_string_lossy());
        assert_eq!(s[s.len() - 2], "--");
        assert_eq!(s[s.len() - 1], "magnet:?xt=1");
        assert!(s.contains(&"--max-tries=10".to_string()));
        // without it aria2c seeds until ratio 1.0 and never exits
        assert!(s.contains(&"--seed-time=0".to_string()));
        assert!(s.contains(&"--file-allocation=none".to_string()));
        assert!(s.contains(&"--auto-save-interval=1".to_string()));
        assert!(!s.iter().any(|a| a.starts_with("--allow-overwrite")));
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn bittorrent_flags_apply_to_local_file_but_not_direct_http() {
        let out = tmp_out();
        let job = Job {
            engine: "aria2".into(),
            url: r"C:\Torrents\[DL] Cuphead [RUS + ENG].torrent".into(),
            out_dir: out.clone(), fmt: "best".into(), cookies_browser: None,
        };
        let bt = build(&job, &tc_aria2()).unwrap();
        assert!(bt.iter().any(|a| a == "--auto-save-interval=1"));
        let direct = Job { url: "https://host/file.zip".into(), ..job };
        let http = build(&direct, &tc_aria2()).unwrap();
        assert!(!http.iter().any(|a| a == "--auto-save-interval=1"));
        assert!(!http.iter().any(|a| a == "--file-allocation=none"));
        std::fs::remove_dir_all(out).ok();
    }

    #[test]
    fn build_ytdlp_page_no_external_downloader() {
        let out = tmp_out();
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://youtube.com/watch?v=1".into(),
            out_dir: out.clone(),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let cmd = build(&job, &tc_both()).unwrap();
        let s: Vec<String> = cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(s.contains(&"--ignore-config".to_string()));
        assert!(s.contains(&"--no-playlist".to_string()));
        assert!(!s.contains(&"--external-downloader".to_string()));
        assert_eq!(s[s.len() - 2..], ["--", "https://youtube.com/watch?v=1"]);
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_ytdlp_direct_link_uses_external_downloader() {
        let out = tmp_out();
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://host/file.zip".into(),
            out_dir: out.clone(),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let cmd = build(&job, &tc_both()).unwrap();
        let s: Vec<String> = cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        let i = s.iter().position(|x| x == "--external-downloader").unwrap();
        assert_eq!(s[i + 1], "aria2c");
        assert!(s[i + 3].contains("--no-conf"));
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_ytdlp_no_aria2_tool_no_external() {
        let out = tmp_out();
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://host/file.zip".into(),
            out_dir: out.clone(),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let cmd = build(&job, &tc_ytdlp()).unwrap();
        let s: Vec<String> = cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(!s.contains(&"--external-downloader".to_string()));
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_out_dir_neutralized() {
        // A relative out_dir that's literally an option-shaped string ("-P")
        // must not reach yt-dlp as anything option-shaped: build() resolves
        // it to an absolute path before it's ever placed after `-P`. Exercising
        // the relative branch means changing cwd; no other test in this crate
        // reads current_dir(), and it's restored before the first assert (the
        // first thing that could panic), so this stays safe under `cargo test`'s
        // default parallel threads.
        let base = tmp_out();
        std::fs::create_dir_all(&base).unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&base).unwrap();
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://x".into(),
            out_dir: PathBuf::from("-P"),
            fmt: "best".into(),
            cookies_browser: None,
        };
        let result = build(&job, &tc_ytdlp());
        std::env::set_current_dir(&prev_cwd).unwrap();
        let cmd = result.unwrap();
        let s: Vec<String> = cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        let value = &s[s.iter().position(|x| x == "-P").unwrap() + 1];
        assert!(!value.starts_with('-'), "{value:?}");
        assert!(Path::new(value).is_absolute(), "{value:?}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn build_missing_tool() {
        let out = tmp_out();
        let job = Job { engine: "yt-dlp".into(), url: "https://x".into(), out_dir: out.clone(), fmt: "best".into(), cookies_browser: None };
        assert!(build(&job, &tc_aria2()).is_err());
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn unknown_engine_is_not_dispatched_to_ytdlp() {
        let out = tmp_out();
        let job = Job { engine: "unknown".into(), url: "https://example.test/x".into(),
            out_dir: out.clone(), fmt: "best".into(), cookies_browser: None };
        assert!(build(&job, &tc_ytdlp()).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn formats_cover_caps_and_source_audio() {
        let keys: Vec<&str> = FORMATS.iter().map(|(k, _)| *k).collect();
        for k in ["best", "2160p", "1440p", "1080p", "720p", "480p", "audio", "audio-src"] {
            assert!(keys.contains(&k), "{k}");
        }
        assert!(format_flags("720p").contains(&"bv*[height<=720]+ba/b[height<=720]"));
        assert_eq!(format_flags("audio-src"), vec!["-f", "ba/b"]);
        assert!(format_flags("audio").contains(&"mp3"));
    }

    #[test]
    fn extra_args_split_like_a_simple_shell() {
        assert_eq!(split_cli_args("--limit-rate 1M --retry-wait=2").unwrap(),
                   vec!["--limit-rate", "1M", "--retry-wait=2"]);
        assert_eq!(split_cli_args("--paths \"D:\\My Video\" ''").unwrap(),
                   vec!["--paths", "D:\\My Video", ""]);
        assert_eq!(split_cli_args("").unwrap(), Vec::<String>::new());
        assert!(split_cli_args("\"unclosed").is_err());
    }

    #[test]
    fn build_applies_run_extras() {
        let out = tmp_out();
        let job = Job { engine: "yt-dlp".into(), url: "https://youtu.be/x".into(),
            out_dir: out.clone(), fmt: "720p".into(), cookies_browser: None };
        let extras = RunExtras {
            subs: true, sub_langs: "ru,en".into(), playlist: true,
            extra_ytdlp: vec!["--limit-rate".into(), "5M".into()], extra_aria2: vec![],
        };
        let s = args_of(&build_with(&job, &tc_ytdlp(), &extras).unwrap());
        assert!(s.contains(&"--yes-playlist".to_string()), "{s:?}");
        assert!(!s.contains(&"--no-playlist".to_string()));
        assert!(s.contains(&"--write-subs".to_string()));
        let i = s.iter().position(|a| a == "--sub-langs").unwrap();
        assert_eq!(s[i + 1], "ru,en");
        assert!(s.windows(2).any(|w| w == ["--limit-rate", "5M"]));
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_places_extra_aria2_args_before_the_terminator() {
        let out = tmp_out();
        let job = Job { engine: "aria2".into(), url: "https://h/f.bin".into(),
            out_dir: out.clone(), fmt: "best".into(), cookies_browser: None };
        let extras = RunExtras {
            extra_aria2: vec!["--max-connection-per-server=4".into()], ..RunExtras::default()
        };
        let s = args_of(&build_with(&job, &tc_aria2(), &extras).unwrap());
        let at = s.iter().position(|a| a == "--max-connection-per-server=4").unwrap();
        let term = s.iter().position(|a| a == "--").unwrap();
        assert!(at < term, "{s:?}");
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_mkdir_failure() {
        // Make a *file* sit where a path component of out_dir needs to be a
        // directory, so create_dir_all fails for real (no mocking needed).
        let base = tmp_out();
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://x".into(),
            out_dir: blocker.join("sub"),
            fmt: "best".into(),
            cookies_browser: None,
        };
        assert!(build(&job, &tc_ytdlp()).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn preflight_aria2_with_cookies_drops_cookies_flag() {
        let out = tmp_out();
        let job = Job {
            engine: "aria2".into(),
            url: "magnet:?xt=1".into(),
            out_dir: out.clone(),
            fmt: "best".into(),
            cookies_browser: Some("chrome".into()),
        };
        assert!(preflight_warning(&job).iter().any(|w| w.contains("yt-dlp")));
        let cmd = build(&job, &tc_aria2()).unwrap();
        assert!(!cmd.iter().any(|a| a == "--cookies-from-browser"));
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_format_flags() {
        let out = tmp_out();
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://youtube.com/watch?v=1".into(),
            out_dir: out.clone(),
            fmt: "1080p".into(),
            cookies_browser: None,
        };
        let cmd = build(&job, &tc_ytdlp()).unwrap();
        let s: Vec<String> = cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(s.contains(&"bv*[height<=1080]+ba/b[height<=1080]".to_string()));
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_cookies_browser_valid() {
        let out = tmp_out();
        for value in ["chrome", "Chrome", "chrome:Profile 1", "firefox+keyring:prof::C:\\container"]
        {
            let job = Job {
                engine: "yt-dlp".into(),
                url: "https://youtube.com/watch?v=1".into(),
                out_dir: out.clone(),
                fmt: "best".into(),
                cookies_browser: Some(value.into()),
            };
            let cmd = build(&job, &tc_ytdlp()).unwrap();
            let s: Vec<String> = cmd.iter().map(|x| x.to_string_lossy().into_owned()).collect();
            let i = s.iter().position(|x| x == "--cookies-from-browser").unwrap();
            assert_eq!(s[i + 1], value);
        }
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn build_rejects_bad_browser_name() {
        for bad in [
            "--exec=danger",
            "chrome;rm",
            "safari;rm -rf /",
            "",
            "nonsense",
            "somesite:prof",
            " chrome",
            "chrome:..",
            "chrome:../evil",
            "firefox:prof::..\\cnt",
            "chrome:pro\u{0}file",
            "chrome:a\nb",
        ] {
            assert!(validate_cookies_browser(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn preflight_warnings() {
        let out = tmp_out();
        let job = Job {
            engine: "aria2".into(),
            url: "magnet:?xt=1".into(),
            out_dir: out.clone(),
            fmt: "audio".into(),
            cookies_browser: Some("chrome".into()),
        };
        let warns = preflight_warning(&job);
        assert_eq!(warns.len(), 2);
        assert!(warns.iter().all(|w| w.contains("yt-dlp")));
        let job = Job {
            engine: "aria2".into(),
            url: "magnet:?xt=1".into(),
            out_dir: out.clone(),
            fmt: "best".into(),
            cookies_browser: None,
        };
        assert!(preflight_warning(&job).is_empty());
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn yt_dlp_warns_about_missing_ffmpeg_for_audio_and_video() {
        let job = Job {
            engine: "yt-dlp".into(),
            url: "https://video.example/123".into(),
            out_dir: PathBuf::from("out"),
            fmt: "best".into(),
            cookies_browser: None,
        };
        assert!(preflight_warning_with_ffmpeg(&job, false, true).iter().any(|w| w.contains("склейка")));
        assert!(preflight_warning_with_ffmpeg(&job, true, true).is_empty());
        assert!(preflight_warning_with_ffmpeg(&job, true, false).iter().any(|w| w.contains("Deno")));
        let audio = Job { fmt: "audio".into(), ..job };
        assert!(preflight_warning_with_ffmpeg(&audio, false, true).iter().any(|w| w.contains("mp3")));
    }

    #[test]
    fn auth_hint_patterns() {
        for line in [
            "ERROR: [youtube] x: Sign in to confirm you're not a bot",
            "ERROR: [youtube] x: This video is age-restricted",
            "ERROR: Private video. Sign in",
            "WARNING: This video is members-only",
            "ERROR: Login required",
            "ERROR: unable to download video data: HTTP Error 403: Forbidden",
            "ERROR: requires authentication",
            "ERROR: [youtube] x: Join this channel to get access",
        ] {
            assert!(looks_like_auth(line), "{line}");
        }
        for line in [
            "ERROR: Unsupported URL: https://x",
            "ERROR: Video unavailable",
            "HTTP Error 404: Not Found",
            "unable to download webpage: timeout",
            // diagnostics context required: a bare filename/URI that happens
            // to contain a hint phrase must not flip the heuristic
            "CUID#7 - Download aborted. URI=`https://host/forbidden-songs.mp3'",
            "Sign in to confirm you're not a bot",
        ] {
            assert!(!looks_like_auth(line), "{line}");
        }
    }

    #[test]
    fn parse_progress_ytdlp_and_aria2() {
        let close = |a: Option<f32>, b: f32| {
            assert!(a.is_some_and(|v| (v - b).abs() < 1e-4), "{a:?} != {b}");
        };
        close(
            parse_progress("[download]  42.3% of    9.99MiB at  1.2MiB/s ETA 00:05"),
            0.423,
        );
        close(parse_progress("[#50e13e 880KiB/0.9MiB(88%) CN:1 DL:812KiB]"), 0.88);
        close(parse_progress("[download] 100% of 1MiB"), 1.0);
        assert_eq!(parse_progress("[download] Destination: file"), None);
        assert_eq!(parse_progress("ERROR: Unsupported URL"), None);
        // a percentage inside a title/path is not progress
        assert_eq!(parse_progress("[download] Destination: C:\\v\\Progress (100%).mp4"), None);
        let title_with_aria2_marker = "[download] Destination: C:\\v\\Clip [#abc123 1MiB/2MiB(88%)].mp4";
        assert_eq!(parse_progress(title_with_aria2_marker), None);
        assert_eq!(aria2_stat(title_with_aria2_marker), None);
        assert_eq!(parse_progress("  [#50e13e 880KiB/0.9MiB(88%) CN:1]"), Some(0.88));
        assert_eq!(parse_progress("[#50e13e 880KiB/0.9MiB(oops%) CN:1]"), None);
        assert_eq!(aria2_stat("prefix [#50e13e 880KiB/0.9MiB(88%) CN:1]"), None);
        assert_eq!(parse_progress("[Merger] Merging (50%) something"), None);
    }

    #[test]
    fn aria2_status_distinguishes_allocation_from_download() {
        let allocating = "[#bc1f6e 0B/6.8GiB(0%) CN:0 SD:0 DL:0B] [FileAlloc:#bc1f6e 1.1GiB/3.3GiB(33%)]";
        assert_eq!(aria2_stat(allocating).as_deref(),
            Some("0% · 0B/6.8GiB · выделение места 1.1GiB/3.3GiB (33%)"));
        let downloading = "[#25017a 361MiB/6.8GiB(5%) CN:30 SD:5 DL:4.1MiB ETA:26m46s]";
        assert_eq!(aria2_stat(downloading).as_deref(),
            Some("5% · 361MiB/6.8GiB · ↓4.1MiB/с · сиды 5 · соединения 30"));
        assert_eq!(aria2_name_from_file("FILE: C:/Downloads/Cuphead_1.3.9/setup.bin (7more)").as_deref(),
            Some("Cuphead_1.3.9"));
    }

    #[test]
    fn aria2_missing_control_error_is_specific_not_every_code_13() {
        let detail = "Exception: [RequestGroup.cc:436] errorCode=13 File C:/Downloads/Cuphead exists, but a control file(*.aria2) does not exist.";
        assert!(aria2_missing_control(detail));
        assert!(!aria2_missing_control("errorCode=13 permission denied"));
        assert!(!aria2_missing_control("control file(*.aria2) does not exist"));
    }

    #[test]
    fn detect_engine_percent_encoded_suffix() {
        assert_eq!(detect_engine("https://host/file%2Ezip"), "aria2");
        assert!(is_direct_download("https://host/file%2Emp4"));
        assert_eq!(detect_engine("https://host/page%2Ehtml"), "yt-dlp");
    }

    #[test]
    fn run_empty_cmd_127() {
        assert_eq!(run(&[], false).code, 127);
    }

    #[test]
    fn decode_child_bytes_utf8_passthrough() {
        assert_eq!(decode_child_bytes(b"plain ascii\n").as_ref(), "plain ascii\n");
        assert_eq!(decode_child_bytes("UTF-8 строка — ok".as_bytes()).as_ref(), "UTF-8 строка — ok");
    }

    #[test]
    fn decode_child_bytes_cp1251_fallback() {
        // "Привет — мир" as the frozen yt-dlp.exe writes it on a cp1251
        // system: Cyrillic in 0xC0-0xFF, em-dash 0x97, curly quote 0x92.
        let cp1251: &[u8] = b"\xCF\xf0\xe8\xe2\xe5\xf2 \x97 \xec\xe8\xf0\x92";
        assert_eq!(decode_child_bytes(cp1251).as_ref(), "Привет — мир\u{2019}");
        // Ё/ё live outside the contiguous Cyrillic block
        assert_eq!(decode_child_bytes(b"\xa8\xb8").as_ref(), "\u{0401}\u{0451}");
        // undefined 0x98 decodes to the replacement char, not a panic
        assert_eq!(decode_child_bytes(b"\x98").as_ref(), "\u{FFFD}");
        // 0xA0-0xBF is not Latin-1 in cp1251: 0xB3="і", 0xBF="ї", 0xB9="№".
        // These byte runs are invalid UTF-8, so the cp1251 path is taken.
        assert_eq!(decode_child_bytes(b"\xb3\xc0").as_ref(), "\u{0456}\u{0410}");
        assert_eq!(decode_child_bytes(b"\xb9").as_ref(), "\u{2116}");
        assert_eq!(decode_child_bytes(b"\xbf").as_ref(), "\u{0457}");
    }

    #[test]
    fn relayed_stdout_keeps_cr_progress_without_terminal_escape_injection() {
        let source = b"[download] 10%\r\x1b]0;FAKE\x07[download] 20%\x1b[31m\n";
        let mut output = Vec::new();
        relay_stdout(&source[..], &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "[download] 10%\r[download] 20%\n");
    }

    #[test]
    fn run_missing_binary_127() {
        let cmd: Vec<OsString> =
            vec!["definitely-not-a-real-binary-xyz".into(), "arg".into()];
        assert_eq!(run(&cmd, false).code, 127);
    }

    #[test]
    fn run_captures_auth_hint() {
        let exe = if cfg!(windows) { "cmd.exe" } else { "sh" };
        let args: Vec<&str> = if cfg!(windows) {
            vec!["/C", "echo ERROR: Sign in to confirm you're not a bot 1>&2 & exit /b 1"]
        } else {
            vec!["-c", "echo 'ERROR: Sign in to confirm' >&2; exit 1"]
        };
        let mut cmd: Vec<OsString> = vec![exe.into()];
        cmd.extend(args.iter().map(|a| OsString::from(*a)));
        let r = run(&cmd, true);
        assert_eq!(r.code, 1);
        assert!(r.auth_hint);
    }

    #[test]
    fn run_no_auth_hint_generic_error() {
        let exe = if cfg!(windows) { "cmd.exe" } else { "sh" };
        let args: Vec<&str> = if cfg!(windows) {
            vec!["/C", "echo ERROR: Unsupported URL 1>&2 & exit /b 1"]
        } else {
            vec!["-c", "echo 'ERROR: Unsupported URL' >&2; exit 1"]
        };
        let mut cmd: Vec<OsString> = vec![exe.into()];
        cmd.extend(args.iter().map(|a| OsString::from(*a)));
        let r = run(&cmd, true);
        assert_eq!(r.code, 1);
        assert!(!r.auth_hint);
    }

    #[test]
    fn batch_name_uses_query_id_for_generic_route_segments() {
        assert_eq!(batch_name("https://youtube.com/watch?v=dQw4w9WgXcQ"), "dQw4w9WgXcQ");
        assert_eq!(batch_name("https://youtu.be/dQw4w9WgXcQ"), "dQw4w9WgXcQ");
        assert_eq!(batch_name("https://example.com/embed?id=abc123"), "abc123");
        assert_eq!(batch_name("https://example.com/some-video-title"), "some-video-title");
        assert_eq!(batch_name("https://example.com/watch"), "watch");
    }

    #[test]
    fn batch_name_still_handles_magnets() {
        let name = batch_name("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Foo");
        assert!(name.starts_with("magnet "));
    }

    #[test]
    fn ytdlp_title_from_line_reads_destination_and_merger_lines() {
        assert_eq!(
            ytdlp_title_from_line("[download] Destination: C:\\out\\Rick Astley - Never Gonna Give You Up.mp4"),
            Some("Rick Astley - Never Gonna Give You Up".to_string())
        );
        assert_eq!(
            ytdlp_title_from_line("[Merger] Merging formats into \"Some Title.mkv\""),
            Some("Some Title".to_string())
        );
        assert_eq!(
            ytdlp_title_from_line("/out/Already Downloaded.mp4 has already been downloaded"),
            None
        );
        assert_eq!(
            ytdlp_title_from_line("[download] /out/Already Downloaded.mp4 has already been downloaded"),
            Some("Already Downloaded".to_string())
        );
        assert_eq!(ytdlp_title_from_line("[download]  42.0% of 10.00MiB"), None);
        assert_eq!(ytdlp_title_from_line("some unrelated line"), None);
    }
}
