use std::borrow::Cow;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::home_dir;
use crate::tools::{which, Toolchain};

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

pub const COOKIES_BROWSERS: &[&str] = &[
    "brave", "chrome", "chromium", "edge", "firefox", "opera", "safari", "vivaldi", "whale",
];

pub const ENGINE_LABELS: [(&str, &str); 2] = [
    ("yt-dlp", "yt-dlp — видео и стримы (YouTube и ещё тысячи сайтов)"),
    ("aria2", "aria2c — прямые ссылки, torrent, magnet"),
];

pub const FORMATS: [(&str, &str); 3] = [
    ("best", "Лучшее качество"),
    ("1080p", "Видео до 1080p"),
    ("audio", "Только аудио (mp3)"),
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
    let lower = line.to_lowercase();
    // Require a diagnostics context: both loaders prefix real problems
    // ("ERROR: ..." / "WARNING: ..." in yt-dlp, "... ERROR - ..." in aria2).
    // Without this, a filename or URI merely containing e.g. "forbidden"
    // ("https://host/forbidden-songs.mp3" echoed in an unrelated failure)
    // flips the auth heuristic and the app nags about browser cookies.
    let dominated = lower.contains("error") || lower.contains("warning");
    dominated && AUTH_HINTS.iter().any(|p| lower.contains(p))
}

pub fn parse_progress(line: &str) -> Option<f32> {
    // aria2's "(NN%)" summary shape, gated on its "[#<gid> " marker. Without
    // the gate ANY line containing "(N%)" parses as progress - e.g. a yt-dlp
    // "[download] Destination: ...(100%).mp4" whose title happens to carry a
    // percentage would jump the bar to that value.
    if line.contains("[#") {
        if let Some(i) = line.find("%)") {
            if let Some(start) = line[..i].rfind('(') {
                if let Ok(p) = line[start + 1..i].parse::<f32>() {
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
            // 0xA0-0xBF (minus the two above) match Latin-1 exactly.
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
fn is_unc_text(s: &str) -> bool {
    s.starts_with("\\\\") || s.starts_with("//")
}

fn path_is_unc(p: &Path) -> bool {
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
    // Percent-decode first: https://host/file%2Ezip is a .zip just like the
    // literal spelling and belongs to aria2.
    let suffix = suffix_of(&percent_decode(&path_from_url(&u)));
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
        "1080p" => vec!["-f", "bv*[height<=1080]+ba/b[height<=1080]"],
        "audio" => vec!["-x", "--audio-format", "mp3"],
        _ => vec![],
    }
}

pub fn build(job: &Job, tc: &Toolchain) -> Result<Vec<OsString>, String> {
    if job.engine == "yt-dlp" {
        if let Some(cb) = &job.cookies_browser {
            validate_cookies_browser(cb)?;
        }
    }

    let expanded = expanduser(&job.out_dir);
    // Before create_dir_all: even a failed mkdir on a UNC path performs SMB
    // authentication against the remote host (NetNTLMv2 leak), and a
    // *successful* one would silently write the user's downloads to a
    // stranger's share (poisoned config/history or a pasted path).
    if path_is_unc(&expanded) {
        return Err(format!(
            "Папка сохранения {} — сетевой UNC-путь. Выберите локальную папку \
             (UNC отклоняется, чтобы Windows не отправляла учётные данные на \
             чужой сервер).",
            expanded.display()
        ));
    }
    let out = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    };
    std::fs::create_dir_all(&out)
        .map_err(|e| format!("Не удалось создать папку {}: {e}", out.display()))?;

    let mut cmd: Vec<OsString> = Vec::new();
    if job.engine == "aria2" {
        cmd.push(tc.require("aria2c")?.into_os_string());
        cmd.push("--no-conf".into());
        cmd.push("-d".into());
        cmd.push(out.into_os_string());
        for a in ARIA2_RESUME.iter().chain(ARIA2_WORKERS) {
            cmd.push((*a).into());
        }
        cmd.push("--".into());
        cmd.push(OsString::from(&job.url));
        return Ok(cmd);
    }

    cmd.push(tc.require("yt-dlp")?.into_os_string());
    cmd.push("--ignore-config".into());
    cmd.push("-P".into());
    cmd.push(out.into_os_string());
    cmd.push("--no-playlist".into());
    if let Some(cb) = &job.cookies_browser {
        cmd.push("--cookies-from-browser".into());
        cmd.push(cb.into());
    }
    for a in format_flags(&job.fmt) {
        cmd.push(a.into());
    }
    if let Some(aria2c) = &tc.aria2c {
        if is_direct_download(&job.url) {
            cmd.push("--external-downloader".into());
            cmd.push(aria2c.as_os_str().to_os_string());
            cmd.push("--external-downloader-args".into());
            cmd.push(
                format!("{} {} --no-conf", ARIA2_RESUME.join(" "), ARIA2_WORKERS.join(" "))
                    .into(),
            );
        }
    }
    cmd.push("--".into());
    cmd.push(OsString::from(&job.url));
    Ok(cmd)
}

pub fn preflight_warning(job: &Job) -> Vec<String> {
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
    } else if which("ffmpeg").is_none() {
        if job.fmt == "audio" {
            warns.push("Не найден ffmpeg — извлечение mp3 может не сработать.".to_string());
        } else {
            warns.push(
                "Не найден ffmpeg — склейка видео и аудио недоступна, yt-dlp выберет \
                 однодорожечный формат (качество может быть ниже)."
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
    let mut child = match Command::new(program)
        .args(&cmd[1..])
        // Same nudge as the GUI: makes a *non-frozen* (python-based) yt-dlp
        // emit UTF-8 into pipes; the frozen yt-dlp.exe ignores it, which the
        // lossy decode below tolerates.
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stderr(if capture_stderr { Stdio::piped() } else { Stdio::inherit() })
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return RunResult { code: 127, auth_hint: false },
    };

    let mut auth_hint = false;
    if let Some(err) = child.stderr.take() {
        let mut reader = BufReader::new(err);
        let mut sink = std::io::stderr();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // Detect auth hints on the decoded line (`lines()` would Err on
            // the cp1251 bytes the frozen yt-dlp.exe emits and - via the old
            // `else { break }` - close this pipe out from under the child).
            let line = decode_child_bytes(&buf);
            if !auth_hint && looks_like_auth(&line) {
                auth_hint = true;
            }
            // Neutralize C0 control characters (except \n\t\r) before echoing:
            // server-controlled text (video titles inside yt-dlp errors) must
            // not inject raw ESC sequences into the user's terminal. Writing
            // the decoded String (not raw bytes) also renders correctly on a
            // Windows console: std re-encodes it via WriteConsoleW, whereas
            // raw cp1251 bytes would turn into mojibake/U+FFFD.
            let cleaned: String = line
                .chars()
                .map(|c| {
                    if (c as u32) < 0x20 && !matches!(c, '\n' | '\t' | '\r') {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            let _ = sink.write_all(cleaned.as_bytes());
            let _ = sink.flush();
        }
    }

    let code = match child.wait() {
        Ok(status) => status.code().unwrap_or(130),
        Err(_) => 127,
    };
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
        // stat'ing these would leak the user's NetNTLMv2 hash to "evil.com"
        for bad in [
            r"\\evil.com\share\movie.torrent",
            "//evil.com/share/movie.torrent",
            r"\\?\UNC\evil.com\share\movie.torrent",
        ] {
            assert!(validate_url(bad).is_err(), "{bad}");
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
        assert!(build(&job, &tc_ytdlp()).is_err());
        let job = Job { out_dir: PathBuf::from("//evil.com/share"), ..job };
        assert!(build(&job, &tc_ytdlp()).is_err());
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
        std::fs::remove_dir_all(&out).ok();
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
        assert_eq!(parse_progress("[Merger] Merging (50%) something"), None);
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
}
