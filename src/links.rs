//! Header-only discovery for download endpoints, and conservative media presets.
//! No cookies, response bodies or yt-dlp extraction are needed to inspect a link.
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::engines::Engine;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, CONTENT_DISPOSITION, CONTENT_TYPE, RANGE};
use reqwest::Url;

/// A server-supplied name that is safe as one local file on Windows and Unix.
/// Its private field prevents locks and argv from receiving an unchecked path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadName(String);

impl DownloadName {
    pub fn parse(name: &str) -> Option<Self> {
        if name.is_empty()
            || name.len() > 240
            || name.starts_with('.')
            || name.ends_with(['.', ' '])
            || name.chars().any(|c| {
                c.is_control()
                    || "/\\<>:\"|?*".contains(c)
                    || matches!(c as u32, 0x200e..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069)
            })
        {
            return None;
        }
        let stem = name.split('.').next()?.trim_end().to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        }) {
            return None;
        }
        Some(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct LinkInfo {
    pub direct: bool,
    pub name: Option<DownloadName>,
    /// True when the server actually answered: `direct` and `name` are then
    /// its verdict rather than the timeout/error fallback, so callers can
    /// tell the user the guess came from the URL alone.
    pub checked: bool,
}

impl LinkInfo {
    pub fn engine(&self, url: &str) -> Engine {
        if self.direct && !aria2_tls_unsupported(url) {
            Engine::Aria2
        } else {
            crate::engines::detect_engine(url)
        }
    }
}

fn host_is(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|s| s.ends_with('.'))
}

/// Hosts whose servers aria2c 1.37 for Windows cannot finish a TLS handshake
/// with ("SSL/TLS handshake failure", whatever options are given): its bundled
/// WinTLS client lacks what Meta's CDN requires, while yt-dlp (OpenSSL)
/// connects fine. Measured against each of these hosts; other sites that fail
/// the same way are caught at run time (`engines::looks_like_aria2_tls_failure`).
const ARIA2_TLS_INCOMPATIBLE: &[&str] = &[
    "instagram.com",
    "cdninstagram.com",
    "fbcdn.net",
    "facebook.com",
    "threads.com",
    "threads.net",
];

/// Hosts that turned aria2c away during this run (a TLS failure, or an HTTP
/// 403/429 that yt-dlp does not get). The next link to such a host skips aria2
/// at once instead of failing the same way first.
static REFUSED_HOSTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
const MAX_REMEMBERED_HOSTS: usize = 64;

fn host_of(url: &str) -> Option<String> {
    let url = Url::parse(&crate::engines::clean_url(url)).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str().map(str::to_ascii_lowercase)
}

/// Remembers, for the rest of this run, that the host of `url` refused aria2c.
/// Besides skipping aria2 for later links, this keeps the yt-dlp retry of the
/// refused link itself from handing the file straight back to aria2c as its
/// external downloader.
pub fn remember_aria2_refused(url: &str) {
    let Some(host) = host_of(url) else {
        return;
    };
    let mut hosts = REFUSED_HOSTS.lock().unwrap_or_else(|e| e.into_inner());
    if !hosts.contains(&host) {
        if hosts.len() >= MAX_REMEMBERED_HOSTS {
            hosts.remove(0);
        }
        hosts.push(host);
    }
}

/// Whether `url` points at a host aria2c cannot reach: one that fails its TLS
/// handshake on Windows (a fixed list, measured), or one that turned aria2c
/// away earlier in this run.
pub fn aria2_tls_unsupported(url: &str) -> bool {
    let Some(host) = host_of(url) else {
        return false;
    };
    if REFUSED_HOSTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&host)
    {
        return true;
    }
    cfg!(windows) && ARIA2_TLS_INCOMPATIBLE.iter().any(|d| host_is(&host, d))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaPreset {
    General,
    Instagram,
    Audio,
}

/// UI presets, not claims about the formats available in a particular post.
/// Unknown sites keep every yt-dlp option; explicit CLI flags remain available.
pub fn media_preset(url: &str) -> MediaPreset {
    let Ok(url) = Url::parse(&crate::engines::clean_url(url)) else {
        return MediaPreset::General;
    };
    let host = url.host_str().unwrap_or("");
    if host_is(host, "instagram.com") {
        MediaPreset::Instagram
    } else if [
        "soundcloud.com",
        "bandcamp.com",
        "mixcloud.com",
        "audiomack.com",
        "freesound.org",
    ]
    .iter()
    .any(|d| host_is(host, d))
    {
        MediaPreset::Audio
    } else {
        MediaPreset::General
    }
}

pub fn offers_subtitles(url: &str) -> bool {
    media_preset(url) == MediaPreset::General
}

pub fn needs_probe(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") || crate::engines::is_bittorrent_input(url) {
        return false;
    }
    // A path with a known file extension already names its file and picks
    // aria2 on its own; probing it would only add a request per link.
    if crate::engines::is_direct_download(url) {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    // Do not waste requests (or trigger a login check) on familiar extractors.
    // This is only a probe fast path, never a whitelist of yt-dlp-supported sites.
    ![
        "youtube.com",
        "youtu.be",
        "youtube-nocookie.com",
        "vimeo.com",
        "twitch.tv",
        "instagram.com",
        "tiktok.com",
        "twitter.com",
        "x.com",
        "facebook.com",
        "fb.watch",
        "soundcloud.com",
        "bandcamp.com",
        "mixcloud.com",
        "audiomack.com",
        "freesound.org",
    ]
    .iter()
    .any(|d| host_is(host, d))
}

fn client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .user_agent(concat!("SNATCH/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(6))
                .referer(false)
                .redirect(redirect_policy())
                .build()
                .ok()
        })
        .as_ref()
}

/// Follow at most five HTTP(S) hops. A redirect to any other scheme (ftp://,
/// data:, ...) is left unfollowed, and the 3xx answer then counts as "this is
/// not a direct file link" instead of an error.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 || !matches!(attempt.url().scheme(), "http" | "https") {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

pub fn resolve(url: &str) -> LinkInfo {
    if !needs_probe(url) {
        return LinkInfo::default();
    }
    client().map(|c| resolve_with(c, url)).unwrap_or_default()
}

/// One retry: a cold-start DNS/TLS hiccup must not silently downgrade the
/// engine recommendation to the URL-only guess, and a second attempt is cheap
/// next to a wrong engine. A response - even a refusal - counts as an answer.
fn resolve_with(client: &Client, url: &str) -> LinkInfo {
    match resolve_once(client, url) {
        Ok(info) => info,
        Err(()) => resolve_once(client, url).unwrap_or_default(),
    }
}

fn resolve_once(client: &Client, url: &str) -> Result<LinkInfo, ()> {
    let start = Instant::now();
    let mut response = client.head(url).send().map_err(|_| ())?;
    // Some download servers refuse HEAD. Inspect GET headers only: even when
    // Range is ignored we never read the payload (which may be gigabytes).
    if matches!(response.status().as_u16(), 405 | 501) {
        // A GET - unlike a HEAD - counts as a download on many servers, and a
        // signed or one-time link (a query string, embedded credentials) can
        // be spent by it before the real download starts. Such a link keeps
        // the URL-only guess, reported as unchecked.
        if is_single_use_shaped(url) {
            return Ok(LinkInfo::default());
        }
        drop(response);
        let left = Duration::from_secs(6)
            .checked_sub(start.elapsed())
            .ok_or(())?;
        response = client
            .get(url)
            .header(RANGE, "bytes=0-0")
            .timeout(left)
            .send()
            .map_err(|_| ())?;
    }
    if response.status().is_server_error()
        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        // A throttled or broken server says nothing about the link type:
        // retry once, then hand back the URL-only guess, unverified.
        return Err(());
    }
    if !response.status().is_success() {
        // 403/404/...: the server answered, and that answer is a verdict -
        // the URL alone decides the engine in that case.
        return Ok(LinkInfo {
            checked: true,
            ..LinkInfo::default()
        });
    }
    let mut info = from_headers(response.url(), response.headers());
    info.checked = true;
    Ok(info)
}

/// Whether `url` looks like a link that carries its own authorization: a
/// query string (signatures, tokens, expiry stamps) or credentials.
fn is_single_use_shaped(url: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|u| u.query().is_some() || !u.username().is_empty() || u.password().is_some())
}

fn known_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    crate::engines::FILE_EXT
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn from_headers(url: &Url, headers: &HeaderMap) -> LinkInfo {
    let disposition = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok());
    let attachment = disposition.is_some_and(|v| {
        v.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("attachment")
    });
    let header_name = disposition.and_then(disposition_name);
    let url_name = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(decode_percent)
        .and_then(|s| DownloadName::parse(&s));
    let mime = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let file_type = matches!(
        mime.as_str(),
        "application/octet-stream"
            | "application/x-msdownload"
            | "application/x-msdos-program"
            | "application/vnd.microsoft.portable-executable"
            | "application/zip"
            | "application/x-7z-compressed"
            | "application/x-rar-compressed"
            | "application/pdf"
            | "application/epub+zip"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-tar"
    ) || mime.starts_with("audio/") && !mime.contains("mpegurl")
        || mime.starts_with("video/")
        || mime.starts_with("image/");
    let html = matches!(mime.as_str(), "text/html" | "application/xhtml+xml");
    let name = header_name.or(url_name);
    let direct =
        attachment || !html && (file_type || name.as_ref().is_some_and(|n| known_file(n.as_str())));
    LinkInfo {
        direct,
        name: if direct { name } else { None },
        // The caller (resolve_once) sets `checked`; from_headers itself is
        // also used on synthetic headers in tests where it stays false.
        checked: false,
    }
}

fn decode_percent(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let h = char::from(*bytes.get(i + 1)?).to_digit(16)?;
            let l = char::from(*bytes.get(i + 2)?).to_digit(16)?;
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// RFC 6266/5987: UTF-8 filename* takes precedence over filename. Quoted
/// semicolons and escaped quotes are parsed, not mistaken for new parameters.
fn disposition_name(value: &str) -> Option<DownloadName> {
    if value.len() > 8192 {
        return None;
    }
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && c == '\\' {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if c == ';' && !quoted {
            parts.push(&value[start..i]);
            start = i + 1;
        }
    }
    if quoted || escaped {
        return None;
    }
    parts.push(&value[start..]);
    let mut plain = None;
    let mut extended = None;
    for part in parts.into_iter().skip(1) {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let raw = raw.trim();
        if key.trim().eq_ignore_ascii_case("filename*") {
            let mut fields = raw.splitn(3, '\'');
            if fields
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("utf-8"))
            {
                let _language = fields.next();
                extended = fields
                    .next()
                    .and_then(decode_percent)
                    .and_then(|s| DownloadName::parse(&s));
            }
        } else if key.trim().eq_ignore_ascii_case("filename") {
            let name = if let Some(s) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                let mut out = String::new();
                let mut chars = s.chars();
                while let Some(c) = chars.next() {
                    out.push(if c == '\\' { chars.next()? } else { c });
                }
                out
            } else {
                raw.to_string()
            };
            plain = DownloadName::parse(&name);
        }
    }
    extended.or(plain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn serve(responses: &[&str]) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/download", listener.local_addr().unwrap());
        let responses: Vec<String> = responses.iter().map(|s| s.to_string()).collect();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = Vec::new();
            'outer: for response in responses {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                // Fewer requests than responses prepared: let
                                // the caller's assertions say why.
                                break 'outer;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("test server: {e}"),
                    }
                };
                // The listener is non-blocking for the accept timeout; the
                // accepted socket must go back to blocking or read_line would
                // fail with WouldBlock before the client sends its request.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new((&mut stream).take(16 * 1024));
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    request.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                drop(reader);
                requests.push(request);
                if response.is_empty() {
                    // "" simulates a dropped connection: accept, send nothing.
                    continue;
                }
                stream
                    .write_all(format!("{response}Connection: close\r\n\r\n").as_bytes())
                    .unwrap();
                // Deliberately no response body, regardless of Content-Length.
            }
            requests
        });
        (url, handle)
    }

    fn test_client() -> Client {
        Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .redirect(redirect_policy())
            .build()
            .unwrap()
    }

    #[test]
    fn discovery_follows_redirects_and_picks_the_browser_filename_without_a_body() {
        let (url, server) = serve(&[
            "HTTP/1.1 302 Found\r\nLocation: /setup.exe\r\nContent-Length: 0\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-msdownload\r\nContent-Length: 236444448\r\nContent-Disposition: attachment; filename=\"browser-name.exe\"\r\n",
        ]);
        let info = resolve_with(&test_client(), &url);
        assert_eq!(info.engine(&url), Engine::Aria2);
        assert_eq!(info.name.unwrap().as_str(), "browser-name.exe");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("HEAD /download "));
        assert!(requests[1].starts_with("HEAD /setup.exe "));
        assert!(requests
            .iter()
            .all(|r| !r.to_lowercase().contains("cookie:")));
    }

    #[test]
    fn discovery_uses_final_url_without_disposition_and_get_headers_when_head_is_refused() {
        let (url, server) = serve(&[
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n",
            "HTTP/1.1 302 Found\r\nLocation: /real%20file.zip\r\nContent-Length: 0\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: 999999999\r\n",
        ]);
        let info = resolve_with(&test_client(), &url);
        assert_eq!(info.name.unwrap().as_str(), "real file.zip");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /download "));
        assert!(requests[1].to_lowercase().contains("range: bytes=0-0\r\n"));
    }

    #[test]
    fn errors_and_html_do_not_recommend_aria2_or_cause_get_retries() {
        for response in [
            "HTTP/1.1 403 Forbidden\r\nContent-Disposition: attachment; filename=setup.exe\r\nContent-Length: 0\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\n",
            "HTTP/1.1 302 Found\r\nLocation: ftp://example.test/setup.exe\r\nContent-Length: 0\r\n",
        ] {
            let (url, server) = serve(&[response]);
            let info = resolve_with(&test_client(), &url);
            assert_eq!(info.engine(&url), Engine::YtDlp);
            assert!(info.name.is_none());
            assert!(info.checked, "HTTP answer must count as a verdict: {response}");
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn a_transport_failure_is_retried_once_and_reported_as_unchecked() {
        // A dropped connection must not silently downgrade the recommendation.
        let (url, server) = serve(&[
            "",
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 5\r\n",
        ]);
        let info = resolve_with(&test_client(), &url);
        assert!(info.checked && info.direct);
        assert_eq!(server.join().unwrap().len(), 2);

        // Two failures in a row: the URL-only guess is handed back unverified.
        let (url, server) = serve(&["", ""]);
        let info = resolve_with(&test_client(), &url);
        assert!(!info.checked && !info.direct && info.name.is_none());
        assert_eq!(server.join().unwrap().len(), 2);

        // A 5xx (or 429) is not a verdict about the link: retry once, and if
        // the server stays broken, keep the guess unchecked instead of
        // claiming "this is not a file".
        let (url, server) = serve(&[
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-msdownload\r\nContent-Length: 5\r\n",
        ]);
        let info = resolve_with(&test_client(), &url);
        assert!(info.checked && info.direct);
        assert_eq!(server.join().unwrap().len(), 2);

        let (url, server) = serve(&[
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n",
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n",
        ]);
        let info = resolve_with(&test_client(), &url);
        assert!(!info.checked);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn download_names_reject_paths_devices_and_display_tricks() {
        for name in [
            "",
            ".",
            "..",
            "../setup.exe",
            "a/b.exe",
            r"C:\setup.exe",
            "a:ads",
            "NUL.exe",
            "con.txt",
            "COM1",
            "LPT².txt",
            "CONOUT$",
            "a.exe.",
            "a.exe ",
            "a\n.exe",
            "a\u{202e}exe",
            "a\u{2066}.exe",
        ] {
            assert!(DownloadName::parse(name).is_none(), "{name:?}");
        }
        assert!(DownloadName::parse(&"x".repeat(241)).is_none());
        for name in [
            "opencode-desktop-win-x64.exe",
            "My File.zip",
            "Установщик.exe",
            "COM10.zip",
        ] {
            assert_eq!(DownloadName::parse(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn disposition_supports_extended_names_and_quoted_semicolons() {
        for (header, expected) in [
            ("attachment; filename=setup.exe", "setup.exe"),
            ("attachment; filename=\"setup; x64.exe\"", "setup; x64.exe"),
            ("attachment; filename=fallback.exe; filename*=UTF-8'ru'%D1%82%D0%B5%D1%81%D1%82.exe", "тест.exe"),
            ("attachment; filename=My%20File.zip", "My%20File.zip"),
            ("attachment; filename=good.exe; filename*=UTF-8''..%2Fevil.exe", "good.exe"),
        ] {
            assert_eq!(disposition_name(header).unwrap().as_str(), expected);
        }
        for header in [
            "attachment",
            "attachment; filename=\"broken.exe",
            "attachment; filename*=UTF-8''%FF.exe",
            "attachment; filename=\"../evil.exe\"",
            "attachment; filename=\"NUL.exe\"",
        ] {
            assert!(disposition_name(header).is_none(), "{header}");
        }
    }

    #[test]
    fn header_discovery_distinguishes_files_from_web_pages_and_stream_manifests() {
        let url = Url::parse("https://host/download/stable/windows-x64-nsis").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/x-msdownload".parse().unwrap());
        headers.insert(
            CONTENT_DISPOSITION,
            "attachment; filename=\"opencode-desktop-win-x64.exe\""
                .parse()
                .unwrap(),
        );
        let info = from_headers(&url, &headers);
        assert_eq!(info.engine(url.as_str()), Engine::Aria2);
        assert_eq!(info.name.unwrap().as_str(), "opencode-desktop-win-x64.exe");
        headers.remove(CONTENT_DISPOSITION);
        let final_url = Url::parse("https://cdn.example/setup%20x64.exe?token=1").unwrap();
        assert_eq!(
            from_headers(&final_url, &headers).name.unwrap().as_str(),
            "setup x64.exe"
        );
        for mime in [
            "text/html",
            "application/xhtml+xml",
            "application/vnd.apple.mpegurl",
            "audio/x-mpegurl",
            "application/dash+xml",
            "application/json",
        ] {
            headers.insert(CONTENT_TYPE, mime.parse().unwrap());
            assert!(!from_headers(&url, &headers).direct, "{mime}");
        }
    }

    #[test]
    fn a_refused_head_on_a_signed_link_is_not_followed_by_a_get() {
        // Same server answer, with and without a query string: only the plain
        // link may be re-asked with a (download-shaped) GET.
        let (url, server) = serve(&["HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n"]);
        let signed = format!("{url}?token=abc&expires=1");
        let info = resolve_with(&test_client(), &signed);
        assert!(!info.checked && !info.direct && info.name.is_none());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].starts_with("HEAD "));
        assert!(is_single_use_shaped("https://user:pw@host/f.zip"));
        assert!(!is_single_use_shaped("https://host/f.zip"));
    }

    #[test]
    fn presets_and_probe_skips_require_real_host_boundaries() {
        assert_eq!(
            media_preset("https://www.instagram.com/reel/x/"),
            MediaPreset::Instagram
        );
        assert_eq!(
            media_preset("https://artist.bandcamp.com/track/x"),
            MediaPreset::Audio
        );
        for url in [
            "https://instagram.com.evil.test/reel/x/",
            "https://host/watch?site=instagram.com",
        ] {
            assert_eq!(media_preset(url), MediaPreset::General);
            assert!(needs_probe(url));
        }
        assert!(!needs_probe("https://www.youtube.com/watch?v=x"));
        assert!(!needs_probe("magnet:?xt=urn:btih:123"));
        assert!(!needs_probe("https://host/file.torrent"));
        assert!(!needs_probe("ftp://host/file.zip"));
        // A known extension already names the file: no probe, no extra request.
        assert!(!needs_probe("https://host/file.zip"));
        assert!(!needs_probe("https://host/file.MP4?x=1"));
        assert!(needs_probe(
            "https://opencode.ai/ru/download/stable/windows-x64-nsis"
        ));
        assert!(needs_probe("https://host/download.php?id=5"));
        // A known extension wins even on an extractor host: URL naming is enough.
        assert!(!needs_probe("https://www.youtube.com/setup.exe"));
    }

    #[test]
    fn a_host_that_refused_aria2_is_not_handed_to_it_again() {
        let url = "https://refusing-host.aria2-test.example/file.zip";
        assert!(!aria2_tls_unsupported(url));
        assert_eq!(crate::engines::detect_engine(url), Engine::Aria2);
        remember_aria2_refused(url);
        // Any link to that host (any case, any path) now goes to yt-dlp.
        assert!(aria2_tls_unsupported(
            "https://REFUSING-host.aria2-test.example/other.bin"
        ));
        assert_eq!(crate::engines::detect_engine(url), Engine::YtDlp);
        assert!(!crate::engines::is_bittorrent_input(url));
        // Other hosts and non-web links are unaffected.
        assert!(!aria2_tls_unsupported(
            "https://fine-host.aria2-test.example/file.zip"
        ));
        remember_aria2_refused("magnet:?xt=urn:btih:abc");
        assert!(!aria2_tls_unsupported("magnet:?xt=urn:btih:abc"));
    }
}
