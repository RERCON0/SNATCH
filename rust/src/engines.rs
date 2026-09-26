use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::home_dir;
use crate::tools::{which, Toolchain};

pub const ARIA2_RESUME: &[&str] = &["-c", "--max-tries=10", "--retry-wait=2", "--auto-file-renaming=false"];
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
    AUTH_HINTS.iter().any(|p| lower.contains(p))
}

pub fn parse_progress(line: &str) -> Option<f32> {
    if let Some(i) = line.find("%)") {
        if let Some(start) = line[..i].rfind('(') {
            if let Ok(p) = line[start + 1..i].parse::<f32>() {
                if (0.0..=100.0).contains(&p) {
                    return Some(p / 100.0);
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
    let suffix = suffix_of(&path_from_url(&u));
    if TORRENT_FILES.contains(&suffix.as_str()) || FILE_EXT.contains(&suffix.as_str()) {
        "aria2"
    } else {
        "yt-dlp"
    }
}

pub fn is_direct_download(url: &str) -> bool {
    let scheme = scheme_of(url);
    ALLOWED_SCHEMES.contains(&scheme.as_str())
        && FILE_EXT.contains(&suffix_of(&path_from_url(url)).as_str())
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
    let mut child = match Command::new(&cmd[0])
        .args(&cmd[1..])
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
            // Detect auth hints on a lossy decode (the frozen yt-dlp.exe emits
            // cp1251, so strict UTF-8 via `lines()` would Err here and the old
            // `else { break }` closed this pipe), but pass the ORIGINAL bytes
            // straight through to our stderr untouched.
            let line = String::from_utf8_lossy(&buf);
            if !auth_hint && looks_like_auth(&line) {
                auth_hint = true;
            }
            let _ = sink.write_all(&buf);
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
            "This video is age-restricted",
            "Private video. Sign in",
            "This video is members-only",
            "Login required",
            "HTTP Error 403: Forbidden",
            "ERROR: requires authentication",
            "Join this channel to get access",
        ] {
            assert!(looks_like_auth(line), "{line}");
        }
        for line in [
            "ERROR: Unsupported URL: https://x",
            "ERROR: Video unavailable",
            "HTTP Error 404: Not Found",
            "unable to download webpage: timeout",
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
