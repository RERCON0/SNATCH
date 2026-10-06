use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256, Sha512};

const YT_DLP_API: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";
const MAX_RELEASE_METADATA: u64 = 1024 * 1024;
const ARIA2_API: &str = "https://api.github.com/repos/aria2/aria2/releases/latest";
const MAX_ARIA2_ZIP_BYTES: u64 = 32 * 1024 * 1024;
const MAX_YT_DLP_BYTES: u64 = 256 * 1024 * 1024;
// Official aria2 1.37.0 win-64bit release. GitHub's API reports digest:null
// for this older release, so keep an independently pinned ZIP hash in source.
const PINNED_ARIA2_ZIP: &str = "aria2-1.37.0-win-64bit-build1.zip";
const PINNED_ARIA2_SHA256: &str =
    "67d015301eef0b612191212d564c5bb0a14b5b9c4796b76454276a4d28d9b288";
// ffmpeg comes from the gyan.dev release builds (the canonical Windows
// builds): a fixed HTTPS URL plus its official `.sha256` sidecar, fetched
// first so a 100+ MB download is never started unverifiably.
const FFMPEG_ARCHIVE_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
const FFMPEG_CHECKSUM_URL: &str =
    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip.sha256";
const MAX_FFMPEG_ZIP_BYTES: u64 = 300 * 1024 * 1024;
const MAX_FFMPEG_EXE_BYTES: u64 = 300 * 1024 * 1024;
// Deno is yt-dlp's recommended JS runtime: YouTube now needs one to solve
// its player challenges (nsig), and without it yt-dlp can fall back to
// limited formats. The GitHub API asset carries a sha256 digest.
const DENO_API: &str = "https://api.github.com/repos/denoland/deno/releases/latest";
const DENO_ZIP_NAME: &str = "deno-x86_64-pc-windows-msvc.zip";
const MAX_DENO_ZIP_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DENO_EXE_BYTES: u64 = 256 * 1024 * 1024;

/// Sanity cap for the extracted aria2c.exe (real one is a few MB). Without a
/// limit, `io::copy` out of the zip would happily fill the disk from a
/// deflate bomb served by a compromised/mirrored "release".
const MAX_ARIA2C_BYTES: u64 = 64 * 1024 * 1024;

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("snatch-rs/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("слишком много перенаправлений загрузчика");
            }
            match attempt.previous().first() {
                Some(initial) if trusted_redirect(initial, attempt.url()) => attempt.follow(),
                _ => attempt.error("перенаправление загрузчика вне официального HTTPS-источника"),
            }
        }))
        // reqwest has NO timeouts by default: a half-dead connection (proxy
        // accepted TCP, then went silent) blocks read() forever - in the GUI
        // that's the Setup phase hung with no cancel button.
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())
}

/// Installer requests may follow the official release CDN, but never an HTTP
/// downgrade or an unrelated host. User download URLs use a separate client.
fn trusted_redirect(initial: &reqwest::Url, next: &reqwest::Url) -> bool {
    if next.scheme() != "https"
        || next.port_or_known_default() != Some(443)
        || !next.username().is_empty()
        || next.password().is_some()
    {
        return false;
    }
    match initial.host_str() {
        Some("api.github.com") => next.host_str() == Some("api.github.com"),
        Some("github.com") => matches!(
            next.host_str(),
            Some(
                "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        ),
        Some("www.gyan.dev" | "gyan.dev") => {
            matches!(next.host_str(), Some("www.gyan.dev" | "gyan.dev"))
        }
        _ => false,
    }
}

/// Why a download/install attempt failed, from the partial file's point of
/// view. `Transient` = transport-level trouble (connect/read errors, timeouts,
/// HTTP 408/429/5xx): the `.part` is kept so the next attempt can resume it
/// with a `Range` request. `Permanent` = the request itself is bad (4xx, size
/// caps) or the downloaded bytes failed validation (checksum mismatch, bad
/// release metadata): the `.part` cannot be resumed and is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    Transient,
    /// A verified executable is busy; retry after the active download ends.
    Deferred,
    Permanent,
}

impl FailureKind {
    /// Whether a `.part` file may still be useful after this failure.
    fn keeps_part(self) -> bool {
        matches!(self, Self::Transient | Self::Deferred)
    }

    /// HTTP status classification: 408/429 and 5xx are "try again later"
    /// answers; every other non-success status is treated as permanent.
    fn from_http_status(status: u16) -> Self {
        match status {
            408 | 429 => Self::Transient,
            s if (500..=599).contains(&s) => Self::Transient,
            _ => Self::Permanent,
        }
    }
}

/// A failed download/install attempt: the user-facing message plus whether the
/// `.part` file should survive for a later Range-resume.
#[derive(Debug)]
struct DownloadFailure {
    kind: FailureKind,
    message: String,
}

impl DownloadFailure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Transient,
            message: message.into(),
        }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Permanent,
            message: message.into(),
        }
    }

    fn from_http_status(status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::from_http_status(status),
            message: message.into(),
        }
    }
}

/// What one attempt does with the destination file, derived purely from the
/// length of an existing `.part` and the HTTP status of the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumePlan {
    /// (Re)start from byte 0: no part at all, or the server ignored the Range
    /// (200) and sent the full body.
    Restart,
    /// The server honored `Range: bytes=<offset>-`: append its body there.
    Append { offset: u64 },
    /// 416: the local offset is past the remote end (stale or already complete
    /// `.part`). Truncate and issue one fresh request without Range.
    Refetch,
}

/// Resume decision:
/// - no part (len 0): a plain GET was sent, any 2xx body streams from 0;
/// - part present: the request carried `Range: bytes=<len>-`, so 206 appends,
///   200 (range ignored) restarts and 416 refetches;
/// - any other combination is not a usable response.
fn resume_plan(part_len: u64, status: u16) -> Option<ResumePlan> {
    match (part_len, status) {
        (0, s) if (200..=299).contains(&s) => Some(ResumePlan::Restart),
        (len, 206) => Some(ResumePlan::Append { offset: len }),
        (_, 200) => Some(ResumePlan::Restart),
        (len, 416) if len > 0 => Some(ResumePlan::Refetch),
        _ => None,
    }
}

/// Byte offset a `.part` can be resumed from: 0 when the file is absent,
/// empty, not a regular file, or larger than the sanity cap (stale junk must
/// not defeat the size check or drive a bogus Range request).
fn resumable_part_len(dest: &Path, max: u64) -> u64 {
    std::fs::metadata(dest)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .filter(|len| *len > 0 && *len <= max)
        .unwrap_or(0)
}

/// GET with an optional `Range: bytes=<range_from>-` and `If-Range`. Some
/// CDNs ignore If-Range, so download_inner also checks the 206 validator.
fn send_get(
    c: &reqwest::blocking::Client,
    url: &str,
    label: &str,
    range_from: u64,
    validator: Option<&str>,
) -> Result<reqwest::blocking::Response, DownloadFailure> {
    let mut req = c.get(url);
    if range_from > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={range_from}-"));
        if let Some(validator) = validator {
            req = req.header(reqwest::header::IF_RANGE, validator);
        }
    }
    req.send()
        .map_err(|e| DownloadFailure::transient(format!("не удалось скачать {label}: {e}")))
}

/// Sidecar holding the server version (ETag / Last-Modified) of the bytes in
/// a `.part`, saved when the part was started.
fn validator_path(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".validator");
    PathBuf::from(name)
}

fn read_validator(part: &Path) -> Option<String> {
    crate::config::read_capped(&validator_path(part), 256)
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.len() <= 256 && v.bytes().all(|b| (0x20..0x7f).contains(&b)))
}

/// A `.part` and its validator always go together.
fn remove_part(part: &Path) {
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(validator_path(part));
}

/// What If-Range can be bound to: a strong ETag (If-Range must not use a weak
/// one), else Last-Modified. None: the part can never be resumed safely.
fn response_validator(resp: &reqwest::blocking::Response) -> Option<String> {
    let header = |name| resp.headers().get(name).and_then(|v| v.to_str().ok());
    header(reqwest::header::ETAG)
        .filter(|etag| !etag.starts_with("W/"))
        .or_else(|| header(reqwest::header::LAST_MODIFIED))
        .map(str::to_string)
}

/// Start offset of a 206 (`Content-Range: bytes <start>-<end>/<total>`).
fn content_range_start(resp: &reqwest::blocking::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes ")?
        .split_once('-')?
        .0
        .parse()
        .ok()
}

/// Full length from a 416's `Content-Range: bytes */<total>`.
fn unsatisfiable_total(resp: &reqwest::blocking::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes */")?
        .parse()
        .ok()
}

/// Live installer progress for frontends without a terminal: which asset is
/// being fetched and how many bytes are already on disk. `total` is `None`
/// while the size is not known yet (release lookup, chunked body).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupProgress {
    pub label: String,
    pub done: u64,
    pub total: Option<u64>,
}

fn download(
    c: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    label: &str,
    max: u64,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<(), DownloadFailure> {
    match download_inner(c, url, dest, label, max, report) {
        Ok(()) => Ok(()),
        Err(f) => {
            // Transient network failures keep a non-empty `.part` so the next
            // attempt can Range-resume it. Permanent failures (4xx, size cap)
            // and empty leftovers are removed - an empty part resumes nothing.
            let keep = f.kind.keeps_part()
                && std::fs::metadata(dest)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
            if !keep {
                remove_part(dest);
            }
            Err(f)
        }
    }
}

fn download_inner(
    c: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    label: &str,
    max: u64,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<(), DownloadFailure> {
    // Resume only a part whose server version is known: the yt-dlp URL is
    // releases/latest/, so an old part may belong to a previous release, and
    // its head plus a new release's tail is a file that never existed (its
    // checksum always fails). Without a saved validator, start over.
    let validator = read_validator(dest);
    let part_len = if validator.is_some() {
        resumable_part_len(dest, max)
    } else {
        0
    };
    let mut resp = send_get(c, url, label, part_len, validator.as_deref())?;
    let mut plan = resume_plan(part_len, resp.status().as_u16());
    if let Some(ResumePlan::Refetch) = plan {
        if unsatisfiable_total(&resp) == Some(part_len)
            && response_validator(&resp).as_deref() == validator.as_deref()
        {
            // 416 for exactly our length: the part may already be complete
            // (e.g. only the checksum fetch failed). Keep it; the checksum
            // still decides whether it matches the current release.
            return Ok(());
        }
        // 416: the part is stale. Truncate it and retry once without Range -
        // a 416 error body must never be streamed as content. Truncating
        // first also means a failed retry cannot leave the same stale part to
        // 416 forever.
        File::create(dest).map_err(|e| {
            DownloadFailure::permanent(format!("не удалось создать {}: {e}", dest.display()))
        })?;
        resp = send_get(c, url, label, 0, None)?;
        plan = resume_plan(0, resp.status().as_u16());
    }
    if let Some(ResumePlan::Append { offset }) = plan {
        if content_range_start(&resp) != Some(offset)
            || response_validator(&resp).as_deref() != validator.as_deref()
        {
            // GitHub's release CDN has returned 206 even for a stale
            // If-Range. Check the response's ETag/Last-Modified ourselves as
            // well as the start offset, or we might splice two releases.
            // No response validator means we cannot safely append either.
            File::create(dest).map_err(|e| {
                DownloadFailure::permanent(format!(
                    "не удалось очистить устаревшую часть {}: {e}",
                    dest.display()
                ))
            })?;
            resp = send_get(c, url, label, 0, None)?;
            plan = resume_plan(0, resp.status().as_u16());
        }
    }

    let (mut file, offset, expected) = match plan {
        Some(ResumePlan::Append { offset }) => {
            let file = OpenOptions::new().append(true).open(dest).map_err(|e| {
                DownloadFailure::permanent(format!(
                    "не удалось открыть {} для дозаписи: {e}",
                    dest.display()
                ))
            })?;
            (file, offset, resp.content_length().unwrap_or(0))
        }
        Some(ResumePlan::Restart) => {
            let file = File::create(dest).map_err(|e| {
                DownloadFailure::permanent(format!("не удалось создать {}: {e}", dest.display()))
            })?;
            // Record which server version this part holds, for If-Range.
            match response_validator(&resp) {
                Some(validator) => {
                    let _ = std::fs::write(validator_path(dest), validator);
                }
                None => {
                    let _ = std::fs::remove_file(validator_path(dest));
                }
            }
            (file, 0, resp.content_length().unwrap_or(0))
        }
        // `Refetch` was resolved above; `None` means the response is not
        // usable (non-success, or a resume answer we cannot follow).
        _ => {
            let status = resp.status();
            return Err(DownloadFailure::from_http_status(
                status.as_u16(),
                format!("не удалось скачать {label}: HTTP {status}"),
            ));
        }
    };

    let total = offset + expected;
    if total > max {
        return Err(DownloadFailure::permanent(format!(
            "{label}: заявленный размер {total} байт превышает лимит {max}"
        )));
    }
    if offset > 0 && std::io::stdout().is_terminal() {
        // Honest note: the existing part is continued, not restarted.
        crate::outln(format!("↻ Докачиваю {label} с {offset} Б…"));
    }
    let bar = if expected > 0 {
        ProgressBar::new(total).with_style(
            ProgressStyle::with_template(
                "{spinner} {wide_bar} {bytes}/{total_bytes} (осталось {eta})",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
        )
    } else {
        // No Content-Length: a determinate bar would read "N/0".
        ProgressBar::new_spinner()
    };
    if offset > 0 {
        bar.set_position(offset);
    }
    let known_total = (expected > 0).then_some(total);
    let mut last_report = Instant::now();
    report(SetupProgress {
        label: label.to_string(),
        done: offset,
        total: known_total,
    });
    let mut buf = [0u8; 65536];
    let mut written = offset;
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| DownloadFailure::transient(format!("сбой чтения {label}: {e}")))?;
        if n == 0 {
            break;
        }
        if n as u64 > max - written {
            return Err(DownloadFailure::permanent(format!(
                "{label} больше {max} байт — загрузка остановлена"
            )));
        }
        file.write_all(&buf[..n])
            .map_err(|e| DownloadFailure::permanent(format!("сбой записи {label}: {e}")))?;
        written += n as u64;
        bar.inc(n as u64);
        // GUI feedback: throttled so a fast link cannot flood the channel.
        if last_report.elapsed() >= Duration::from_millis(120) {
            last_report = Instant::now();
            report(SetupProgress {
                label: label.to_string(),
                done: written,
                total: known_total,
            });
        }
    }
    bar.finish_and_clear();
    report(SetupProgress {
        label: label.to_string(),
        done: written,
        total: known_total,
    });
    Ok(())
}

/// Stream a file through a hasher in 64 KiB chunks: yt-dlp.exe can be up to
/// MAX_YT_DLP_BYTES, and reading it whole would spike memory for no reason.
fn file_hash_hex<D: Digest>(path: &Path, what: &str) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
    let mut hasher = D::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("сбой чтения {what} {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut hex = String::with_capacity(128);
    for b in hasher.finalize() {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

fn sha512_hex(path: &Path) -> Result<String, String> {
    file_hash_hex::<Sha512>(path, "файла")
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    file_hash_hex::<Sha256>(path, "архива")
}

fn aria2_asset(release: &serde_json::Value) -> Result<(String, String), String> {
    let asset = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets.iter().find(|a| {
                a["name"]
                    .as_str()
                    .is_some_and(|name| name.contains("win-64bit") && name.ends_with(".zip"))
            })
        })
        .ok_or("в релизе aria2 не найден win-64bit zip")?;
    let name = asset["name"].as_str().unwrap_or_default();
    let size = asset["size"]
        .as_u64()
        .ok_or("у aria2 zip нет размера в GitHub API")?;
    if size == 0 || size > MAX_ARIA2_ZIP_BYTES {
        return Err(format!("размер aria2 zip ({size} байт) выходит за лимит"));
    }
    let expected = if name == PINNED_ARIA2_ZIP {
        PINNED_ARIA2_SHA256.to_string()
    } else {
        let digest = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:"))
            .ok_or("Новый релиз aria2 не публикует SHA-256. Обновите SNATCH или установите aria2 вручную.")?;
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Некорректная SHA-256 в метаданных aria2".into());
        }
        digest.to_string()
    };
    let url = asset["browser_download_url"]
        .as_str()
        .ok_or("у aria2 zip нет URL")?;
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("некорректный URL aria2: {e}"))?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || parsed.username() != ""
        || parsed.password().is_some()
        || !parsed.path().starts_with("/aria2/aria2/releases/download/")
        || !parsed.path().ends_with(&format!("/{name}"))
    {
        return Err("aria2 zip должен скачиваться с официального GitHub-релиза".into());
    }
    Ok((url.to_string(), expected))
}

fn yt_dlp_checksum(sums: &str) -> Option<&str> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?;
        (name.trim_start_matches('*') == "yt-dlp.exe").then_some(hash)
    })
}

fn read_metadata(resp: impl Read, label: &str) -> Result<Vec<u8>, DownloadFailure> {
    let mut bytes = Vec::new();
    resp.take(MAX_RELEASE_METADATA + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| DownloadFailure::transient(format!("чтение {label}: {e}")))?;
    if bytes.len() as u64 > MAX_RELEASE_METADATA {
        return Err(DownloadFailure::permanent(format!(
            "{label}: ответ больше 1 МБ"
        )));
    }
    Ok(bytes)
}

fn release_json(
    c: &reqwest::blocking::Client,
    url: &str,
) -> Result<serde_json::Value, DownloadFailure> {
    let resp = c
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| DownloadFailure::transient(format!("релиз: {e}")))?;
    if !resp.status().is_success() {
        // GitHub uses 403 (not just 429) for exhausted API rate limits.
        // Other 403s remain permanent; transport/rate failures preserve parts.
        if resp.status().as_u16() == 403
            && (resp
                .headers()
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v == "0")
                || resp.headers().contains_key(reqwest::header::RETRY_AFTER))
        {
            return Err(DownloadFailure::transient(
                "GitHub API: превышен лимит запросов; повторите установку позже",
            ));
        }
        return Err(DownloadFailure::from_http_status(
            resp.status().as_u16(),
            format!("релиз: HTTP {}", resp.status()),
        ));
    }
    serde_json::from_slice(&read_metadata(resp, "API релизов")?)
        .map_err(|e| DownloadFailure::permanent(format!("ответ API релизов: {e}")))
}

fn yt_dlp_release_urls(release: &serde_json::Value) -> Result<(String, String), DownloadFailure> {
    let tag = release
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .filter(|t| {
            !t.is_empty()
                && *t != "."
                && *t != ".."
                && t.len() <= 80
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
        .ok_or_else(|| DownloadFailure::permanent("в API yt-dlp нет допустимого tag_name"))?;
    let base = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{tag}");
    Ok((format!("{base}/yt-dlp.exe"), format!("{base}/SHA2-512SUMS")))
}

fn retry_transient<T>(
    mut attempt: impl FnMut() -> Result<T, DownloadFailure>,
) -> Result<T, DownloadFailure> {
    for n in 0..3 {
        match attempt() {
            Err(f) if f.kind == FailureKind::Transient && n < 2 => {
                std::thread::sleep(Duration::from_secs(1 << n))
            }
            result => return result,
        }
    }
    unreachable!("last attempt always returns")
}

fn copy_limited(
    label: &str,
    reader: impl Read,
    mut writer: impl Write,
    max: u64,
) -> Result<(), String> {
    let n = std::io::copy(&mut reader.take(max + 1), &mut writer)
        .map_err(|e| format!("сбой распаковки: {e}"))?;
    if n > max {
        return Err(format!(
            "{label} больше {max} байт — похоже на подмену или битый архив"
        ));
    }
    Ok(())
}

/// First bare 64-hex token of a checksum sidecar. gyan.dev's `.sha256` holds
/// just the hash; some mirrors wrap it in "SHA256 (file) = <hash>".
fn sha256_from_sidecar(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|tok| tok.len() == 64)
        .map(str::to_string)
}

fn deno_asset(release: &serde_json::Value) -> Result<(String, String), String> {
    let asset = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a["name"].as_str() == Some(DENO_ZIP_NAME))
        })
        .ok_or("в релизе Deno нет архива для Windows x64")?;
    let size = asset["size"]
        .as_u64()
        .ok_or("у архива Deno нет размера в GitHub API")?;
    if size == 0 || size > MAX_DENO_ZIP_BYTES {
        return Err(format!("размер архива Deno ({size} байт) выходит за лимит"));
    }
    let digest = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .ok_or("релиз Deno не публикует SHA-256 — установите Deno вручную")?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Некорректная SHA-256 в метаданных Deno".into());
    }
    let url = asset["browser_download_url"]
        .as_str()
        .ok_or("у архива Deno нет URL")?;
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("некорректный URL Deno: {e}"))?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || parsed.username() != ""
        || parsed.password().is_some()
        || !parsed
            .path()
            .starts_with("/denoland/deno/releases/download/")
        || !parsed.path().ends_with(&format!("/{DENO_ZIP_NAME}"))
    {
        return Err("архив Deno должен скачиваться с официального GitHub-релиза".into());
    }
    Ok((url.to_string(), digest.to_string()))
}

/// ffmpeg (+ffprobe) for yt-dlp: merging video+audio, mp3 extraction and
/// subtitle embedding all go through it. Verified against the official
/// sidecar from the same HTTPS host before the archive is trusted.
pub fn install_ffmpeg(bin: &Path) -> Result<PathBuf, String> {
    install_ffmpeg_with(bin, &mut |_| {})
}

/// [`install_ffmpeg`] with live progress for the GUI.
pub fn install_ffmpeg_with(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Автоустановка ffmpeg поддерживается только на Windows.".to_string());
    }
    let _lock = install_lock(bin, "ffmpeg")?;
    match retry_transient(|| install_ffmpeg_inner(bin, report)) {
        Ok(p) => Ok(p),
        Err(f) => {
            if !f.kind.keeps_part() {
                remove_part(&bin.join("ffmpeg.zip.part"));
                let _ = std::fs::remove_file(bin.join("ffmpeg.exe.part"));
                let _ = std::fs::remove_file(bin.join("ffprobe.exe.part"));
            }
            Err(f.message)
        }
    }
}

fn install_ffmpeg_inner(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, DownloadFailure> {
    let c = client().map_err(DownloadFailure::permanent)?;
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Скачиваю ffmpeg…");
    }
    report(SetupProgress {
        label: "ffmpeg".into(),
        done: 0,
        total: None,
    });
    // Sidecar first: never fetch the huge zip just to learn it is not
    // verifiable. Same host, HTTPS.
    let resp = c
        .get(FFMPEG_CHECKSUM_URL)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| DownloadFailure::transient(format!("ffmpeg sha256: {e}")))?;
    if !resp.status().is_success() {
        return Err(DownloadFailure::from_http_status(
            resp.status().as_u16(),
            format!("ffmpeg sha256: HTTP {}", resp.status()),
        ));
    }
    let text = String::from_utf8(read_metadata(resp, "ffmpeg sha256")?)
        .map_err(|e| DownloadFailure::permanent(format!("ffmpeg sha256 не UTF-8: {e}")))?;
    let expected = sha256_from_sidecar(&text)
        .ok_or_else(|| DownloadFailure::permanent("в ffmpeg .sha256 нет корректного хеша"))?;

    let zip_path = bin.join("ffmpeg.zip.part");
    download(
        &c,
        FFMPEG_ARCHIVE_URL,
        &zip_path,
        "ffmpeg (zip)",
        MAX_FFMPEG_ZIP_BYTES,
        report,
    )?;
    let got = sha256_hex(&zip_path).map_err(DownloadFailure::permanent)?;
    if !got.eq_ignore_ascii_case(&expected) {
        remove_part(&zip_path);
        return Err(DownloadFailure::permanent(
            "SHA-256 архива ffmpeg не совпала — установка остановлена",
        ));
    }

    let dest = bin.join("ffmpeg.exe");
    let tmp = bin.join("ffmpeg.exe.part");
    let probe_tmp = bin.join("ffprobe.exe.part");
    let extracted = (|| -> Result<(), String> {
        let file =
            File::open(&zip_path).map_err(|e| format!("не удалось открыть скачанный zip: {e}"))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("битый zip: {e}"))?;
        for (tool, tool_tmp) in [("ffmpeg.exe", &tmp), ("ffprobe.exe", &probe_tmp)] {
            let idx = (0..archive.len())
                .filter_map(|i| {
                    archive.by_index(i).ok().and_then(|e| {
                        let name = e.name().to_string();
                        if !e.is_dir() && name.ends_with(tool) {
                            Some((i, name.len()))
                        } else {
                            None
                        }
                    })
                })
                .min_by_key(|(_, len)| *len);
            let Some((idx, _)) = idx else {
                // ffprobe is optional (merge works without it); ffmpeg is not.
                if tool == "ffprobe.exe" {
                    continue;
                }
                return Err("внутри zip нет ffmpeg.exe".into());
            };
            {
                let mut entry = archive
                    .by_index(idx)
                    .map_err(|e| format!("не удалось прочитать zip: {e}"))?;
                let mut out = File::create(tool_tmp)
                    .map_err(|e| format!("не удалось создать {}: {e}", tool_tmp.display()))?;
                copy_limited(tool, &mut entry, &mut out, MAX_FFMPEG_EXE_BYTES)?;
            }
        }
        publish_tool(&tmp, &dest).map_err(|e| format!("не удалось установить ffmpeg.exe: {e}"))?;
        if probe_tmp.is_file() {
            publish_tool(&probe_tmp, &bin.join("ffprobe.exe"))
                .map_err(|e| format!("не удалось установить ffprobe.exe: {e}"))?;
        }
        Ok(())
    })();
    remove_part(&zip_path);
    if extracted.is_err() {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&probe_tmp);
    }
    extracted.map_err(DownloadFailure::permanent)?;
    Ok(dest)
}

/// Deno, yt-dlp's recommended JS runtime for YouTube's player challenges.
pub fn install_deno(bin: &Path) -> Result<PathBuf, String> {
    install_deno_with(bin, &mut |_| {})
}

/// [`install_deno`] with live progress for the GUI.
pub fn install_deno_with(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Автоустановка Deno поддерживается только на Windows.".to_string());
    }
    let _lock = install_lock(bin, "deno")?;
    match retry_transient(|| install_deno_inner(bin, report)) {
        Ok(p) => Ok(p),
        Err(f) => {
            if !f.kind.keeps_part() {
                remove_part(&bin.join("deno.zip.part"));
                let _ = std::fs::remove_file(bin.join("deno.exe.part"));
            }
            Err(f.message)
        }
    }
}

fn install_deno_inner(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, DownloadFailure> {
    let c = client().map_err(DownloadFailure::permanent)?;
    recover_tool(&bin.join("deno.exe"))
        .map_err(|e| DownloadFailure::transient(format!("восстановление deno: {e}")))?;
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Ищу свежий релиз Deno…");
    }
    report(SetupProgress {
        label: "Deno".into(),
        done: 0,
        total: None,
    });
    let release = release_json(&c, DENO_API)?;
    let (url, expected) = deno_asset(&release).map_err(DownloadFailure::permanent)?;

    let zip_path = bin.join("deno.zip.part");
    download(
        &c,
        &url,
        &zip_path,
        "Deno (zip)",
        MAX_DENO_ZIP_BYTES,
        report,
    )?;
    let got = sha256_hex(&zip_path).map_err(DownloadFailure::permanent)?;
    if !got.eq_ignore_ascii_case(&expected) {
        remove_part(&zip_path);
        return Err(DownloadFailure::permanent(
            "SHA-256 архива Deno не совпала — установка остановлена",
        ));
    }

    let dest = bin.join("deno.exe");
    let tmp = bin.join("deno.exe.part");
    let extracted = (|| -> Result<(), String> {
        let file =
            File::open(&zip_path).map_err(|e| format!("не удалось открыть скачанный zip: {e}"))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("битый zip: {e}"))?;
        let idx = (0..archive.len())
            .filter_map(|i| {
                archive.by_index(i).ok().and_then(|e| {
                    let name = e.name().to_string();
                    (!e.is_dir() && name.ends_with("deno.exe")).then_some((i, name.len()))
                })
            })
            .min_by_key(|(_, len)| *len)
            .ok_or("внутри zip нет deno.exe")?
            .0;
        {
            let mut entry = archive
                .by_index(idx)
                .map_err(|e| format!("не удалось прочитать zip: {e}"))?;
            let mut out = File::create(&tmp)
                .map_err(|e| format!("не удалось создать {}: {e}", tmp.display()))?;
            copy_limited("deno.exe", &mut entry, &mut out, MAX_DENO_EXE_BYTES)?;
        }
        publish_tool(&tmp, &dest).map_err(|e| format!("не удалось установить deno.exe: {e}"))
    })();
    remove_part(&zip_path);
    if extracted.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    extracted.map_err(DownloadFailure::permanent)?;
    Ok(dest)
}

/// One installer per tool at a time, across processes (the GUI's «обновить»
/// next to `snatch --install-tools` in a terminal). Windows lets a second
/// writer keep its handle on `<tool>.part` while the first renames the
/// verified file into place, so its late writes used to land inside the
/// freshly checked exe - which was then executed. Held for the whole attempt,
/// cleanup included: the `.part` files belong to whoever holds it. Lock files
/// are never deleted (a new inode would not be the locked one).
fn install_lock(bin: &Path, tool: &str) -> Result<File, String> {
    use fs4::{FileExt, TryLockError};
    let path = bin.join(format!("{tool}.install.lock"));
    let file = crate::config::open_lock_file(&path, true)
        .map_err(|e| format!("блокировка установки {}: {e}", path.display()))?;
    match FileExt::try_lock(&file) {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(format!(
            "{tool} уже устанавливается в другом окне SNATCH — дождитесь окончания и повторите"
        )),
        Err(TryLockError::Error(e)) => Err(format!("блокировка установки {tool}: {e}")),
    }
}

/// `<tool>.exe.old`: where a running tool is moved aside.
fn old_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Puts a verified aria2c `.part` in place. Windows can move aside a running
/// aria2c.exe before publishing its replacement; aria2c does not read its
/// executable again after startup. Do NOT use this for PyInstaller yt-dlp:
/// it reads modules from its exe during downloads and moving that file breaks
/// the running process.
fn publish_tool(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    recover_tool(dest)?;
    let direct = match std::fs::rename(tmp, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(old_path(dest));
            return Ok(());
        }
        Err(e) => e,
    };
    let old = old_path(dest);
    let _ = std::fs::remove_file(&old);
    if std::fs::rename(dest, &old).is_err() {
        return Err(direct);
    }
    if let Err(e) = std::fs::rename(tmp, dest) {
        // Never leave the tool missing: put the previous one back.
        if let Err(restore) = std::fs::rename(&old, dest) {
            return Err(std::io::Error::new(
                e.kind(),
                format!(
                    "установка: {e}; восстановление: {restore}; резерв: {}",
                    old.display()
                ),
            ));
        }
        return Err(e);
    }
    let _ = std::fs::remove_file(&old);
    Ok(())
}

fn recover_tool(dest: &Path) -> std::io::Result<()> {
    let old = old_path(dest);
    if !dest.exists() && old.is_file() {
        std::fs::rename(old, dest)?;
    }
    Ok(())
}

pub fn install_yt_dlp(bin: &Path) -> Result<PathBuf, String> {
    install_yt_dlp_with(bin, &mut |_| {})
}

/// [`install_yt_dlp`] with live progress for the GUI; the CLI keeps its
/// indicatif bar and ignores the reporter.
pub fn install_yt_dlp_with(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Автоустановка yt-dlp поддерживается только на Windows.".to_string());
    }
    let _lock = install_lock(bin, "yt-dlp")?;
    match retry_transient(|| install_yt_dlp_inner(bin, report)) {
        Ok(p) => Ok(p),
        Err(f) => {
            // Transient failures keep `yt-dlp.exe.part` for the next attempt's
            // Range-resume; only permanent ones clean it up.
            if !f.kind.keeps_part() {
                remove_part(&bin.join("yt-dlp.exe.part"));
            }
            Err(f.message)
        }
    }
}

fn install_yt_dlp_inner(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, DownloadFailure> {
    let c = client().map_err(DownloadFailure::permanent)?;
    let dest = bin.join("yt-dlp.exe");
    let tmp = bin.join("yt-dlp.exe.part");
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Скачиваю yt-dlp.exe (официальный релиз)…");
    }
    report(SetupProgress {
        label: "yt-dlp.exe".into(),
        done: 0,
        total: None,
    });
    // Resolve the mutable latest alias once, then use one immutable release
    // for both assets. Publishing a new release between GETs cannot mix them.
    let (payload_url, sums_url) = yt_dlp_release_urls(&release_json(&c, YT_DLP_API)?)?;
    let sums = c
        .get(&sums_url)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| DownloadFailure::transient(format!("не удалось скачать SHA2-512SUMS: {e}")))?;
    if !sums.status().is_success() {
        // Without this check a 403/404 body gets parsed as the sums file and
        // surfaces as the misleading "no entry for yt-dlp.exe" error.
        return Err(DownloadFailure::from_http_status(
            sums.status().as_u16(),
            format!("не удалось скачать SHA2-512SUMS: HTTP {}", sums.status()),
        ));
    }
    let sums_text = String::from_utf8(read_metadata(sums, "SHA2-512SUMS")?)
        .map_err(|e| DownloadFailure::permanent(format!("SHA2-512SUMS не UTF-8: {e}")))?;
    let expected = yt_dlp_checksum(&sums_text)
        .ok_or_else(|| DownloadFailure::permanent("в SHA2-512SUMS нет записи для yt-dlp.exe"))?;
    download(
        &c,
        &payload_url,
        &tmp,
        "yt-dlp.exe",
        MAX_YT_DLP_BYTES,
        report,
    )?;
    let got = sha512_hex(&tmp).map_err(DownloadFailure::permanent)?;
    if !got.eq_ignore_ascii_case(expected) {
        remove_part(&tmp);
        return Err(DownloadFailure::permanent(
            "контрольная сумма SHA-512 не совпала — файл не сохранён",
        ));
    }

    // Never move aside a running PyInstaller image: it reads modules from its
    // exe while downloading. A failed direct replacement keeps the verified
    // part + validator; the next installation rechecks its checksum and tries
    // again once the running download has finished.
    std::fs::rename(&tmp, &dest).map_err(|e| DownloadFailure {
        kind: FailureKind::Deferred,
        message: format!("не удалось установить yt-dlp.exe (возможно, он занят загрузкой; повторите установку после её завершения): {e}"),
    })?;
    let _ = std::fs::remove_file(validator_path(&tmp));
    Ok(dest)
}

pub fn install_aria2(bin: &Path) -> Result<PathBuf, String> {
    install_aria2_with(bin, &mut |_| {})
}

/// [`install_aria2`] with live progress for the GUI.
pub fn install_aria2_with(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Автоустановка aria2c поддерживается только на Windows.".to_string());
    }
    let _lock = install_lock(bin, "aria2c")?;
    match retry_transient(|| install_aria2_inner(bin, report)) {
        Ok(p) => Ok(p),
        Err(f) => {
            // Transient failures keep both `.part` files for the next attempt's
            // Range-resume; only permanent ones clean them up.
            if !f.kind.keeps_part() {
                let _ = std::fs::remove_file(bin.join("aria2c.exe.part"));
                remove_part(&bin.join("aria2.zip.part"));
            }
            Err(f.message)
        }
    }
}

fn install_aria2_inner(
    bin: &Path,
    report: &mut dyn FnMut(SetupProgress),
) -> Result<PathBuf, DownloadFailure> {
    let c = client().map_err(DownloadFailure::permanent)?;
    recover_tool(&bin.join("aria2c.exe"))
        .map_err(|e| DownloadFailure::transient(format!("восстановление aria2c: {e}")))?;
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Ищу свежий релиз aria2…");
    }
    report(SetupProgress {
        label: "aria2c".into(),
        done: 0,
        total: None,
    });
    let release = release_json(&c, ARIA2_API)?;
    let (url, expected_sha256) = aria2_asset(&release).map_err(DownloadFailure::permanent)?;

    let zip_path = bin.join("aria2.zip.part");
    download(
        &c,
        &url,
        &zip_path,
        "aria2 (zip)",
        MAX_ARIA2_ZIP_BYTES,
        report,
    )?;
    let got = sha256_hex(&zip_path).map_err(DownloadFailure::permanent)?;
    if !got.eq_ignore_ascii_case(&expected_sha256) {
        // The archive is unusable and must not be resumed later.
        remove_part(&zip_path);
        return Err(DownloadFailure::permanent(
            "SHA-256 архива aria2 не совпала — установка остановлена",
        ));
    }

    let dest = bin.join("aria2c.exe");
    let tmp = bin.join("aria2c.exe.part");
    let extracted = (|| -> Result<(), String> {
        let file =
            File::open(&zip_path).map_err(|e| format!("не удалось открыть скачанный zip: {e}"))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("битый zip: {e}"))?;
        let entry_idx = (0..archive.len())
            .filter_map(|i| {
                archive.by_index(i).ok().and_then(|e| {
                    let name = e.name().to_string();
                    if !e.is_dir() && name.ends_with("aria2c.exe") {
                        Some((i, name.len()))
                    } else {
                        None
                    }
                })
            })
            .min_by_key(|(_, len)| *len)
            .map(|(i, _)| i)
            .ok_or("внутри zip нет aria2c.exe")?;

        {
            let mut entry = archive
                .by_index(entry_idx)
                .map_err(|e| format!("не удалось прочитать zip: {e}"))?;
            let mut out = File::create(&tmp)
                .map_err(|e| format!("не удалось создать {}: {e}", tmp.display()))?;
            copy_limited("aria2c.exe", &mut entry, &mut out, MAX_ARIA2C_BYTES)?;
        }
        publish_tool(&tmp, &dest).map_err(|e| format!("не удалось установить aria2c.exe: {e}"))
    })();
    // The archive file handle must be dropped before Windows lets us delete
    // it, so cleanup happens after the extraction block regardless of outcome
    // (previously the zip leaked in bin\ on every failure path).
    remove_part(&zip_path);
    extracted.map_err(DownloadFailure::permanent)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    #[test]
    fn installer_redirects_stay_on_official_https_origins() {
        let github = reqwest::Url::parse(
            "https://github.com/yt-dlp/yt-dlp/releases/download/2026/yt-dlp.exe",
        )
        .unwrap();
        let api = reqwest::Url::parse(YT_DLP_API).unwrap();
        let gyan = reqwest::Url::parse(FFMPEG_ARCHIVE_URL).unwrap();
        let allowed =
            reqwest::Url::parse("https://release-assets.githubusercontent.com/asset").unwrap();
        assert!(trusted_redirect(&github, &allowed));
        assert!(trusted_redirect(&api, &api));
        assert!(trusted_redirect(&gyan, &gyan));
        assert!(!trusted_redirect(&api, &allowed));
        assert!(!trusted_redirect(&gyan, &allowed));
        for url in [
            "http://github.com/asset",
            "https://github.com:444/asset",
            "https://github.com.evil.example/asset",
            "https://user@github.com/asset",
            "https://evil.example/asset",
            "https://user:secret@www.gyan.dev/asset",
        ] {
            assert!(
                !trusted_redirect(&github, &reqwest::Url::parse(url).unwrap()),
                "{url}"
            );
        }
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn aria2_release_requires_verifiable_bounded_official_zip() {
        let asset = |name: &str, size: u64, digest: serde_json::Value, url: &str| {
            json!({"assets": [{"name": name, "size": size, "digest": digest,
                "browser_download_url": url}]})
        };
        let pinned_url = format!(
            "https://github.com/aria2/aria2/releases/download/release-1.37.0/{PINNED_ARIA2_ZIP}"
        );
        assert_eq!(
            aria2_asset(&asset(
                PINNED_ARIA2_ZIP,
                2475379,
                serde_json::Value::Null,
                &pinned_url
            ))
            .unwrap()
            .1,
            PINNED_ARIA2_SHA256
        );
        let name = "aria2-2.0-win-64bit-build1.zip";
        let future_url =
            format!("https://github.com/aria2/aria2/releases/download/release-2.0/{name}");
        assert!(aria2_asset(&asset(name, 2500000, serde_json::Value::Null, &future_url)).is_err());
        let hash = "a".repeat(64);
        assert_eq!(
            aria2_asset(&asset(
                name,
                2500000,
                json!(format!("sha256:{hash}")),
                &future_url
            ))
            .unwrap()
            .1,
            hash
        );
        assert!(aria2_asset(&asset(
            name,
            MAX_ARIA2_ZIP_BYTES + 1,
            json!(format!("sha256:{hash}")),
            &future_url
        ))
        .is_err());
        assert!(aria2_asset(&asset(
            name,
            2500000,
            json!(format!("sha256:{hash}")),
            "https://evil.example/aria2.zip"
        ))
        .is_err());
    }

    #[test]
    fn sha256_of_downloaded_archive_is_computed_in_chunks() {
        let dir = std::env::temp_dir().join(format!("snatch-aria-checksum-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("archive.zip");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_hex(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checksum_selects_exact_executable_not_a_similar_name() {
        let sums = "aaa  other-yt-dlp.exe\nbbb *yt-dlp.exe\nccc  yt-dlp.exe.bad\n";
        assert_eq!(yt_dlp_checksum(sums), Some("bbb"));
        assert_eq!(yt_dlp_checksum("aaa yt-dlp.exe.bad"), None);
    }

    #[test]
    fn ffmpeg_sidecar_parser_takes_the_hash_not_the_filename() {
        let bare = "a".repeat(64);
        assert_eq!(sha256_from_sidecar(&format!("{bare}\n")).unwrap(), bare);
        let named = "b".repeat(64);
        assert_eq!(
            sha256_from_sidecar(&format!("SHA256 (ffmpeg-release-essentials.zip) = {named}"))
                .unwrap(),
            named
        );
        assert!(sha256_from_sidecar("no hash here").is_none());
    }

    #[test]
    fn deno_release_requires_the_official_verified_windows_zip() {
        let asset = |name: &str, size: u64, digest: serde_json::Value, url: &str| {
            json!({"assets": [{"name": name, "size": size, "digest": digest,
                "browser_download_url": url}]})
        };
        let hash = "c".repeat(64);
        let good =
            format!("https://github.com/denoland/deno/releases/download/v2.9.7/{DENO_ZIP_NAME}");
        assert_eq!(
            deno_asset(&asset(
                DENO_ZIP_NAME,
                42630221,
                json!(format!("sha256:{hash}")),
                &good
            ))
            .unwrap()
            .1,
            hash
        );
        assert!(deno_asset(&asset(
            DENO_ZIP_NAME,
            42630221,
            serde_json::Value::Null,
            &good
        ))
        .is_err());
        assert!(deno_asset(&asset(
            DENO_ZIP_NAME,
            MAX_DENO_ZIP_BYTES + 1,
            json!(format!("sha256:{hash}")),
            &good
        ))
        .is_err());
        assert!(deno_asset(&asset(
            DENO_ZIP_NAME,
            42630221,
            json!(format!("sha256:{hash}")),
            "https://evil.example/deno.zip"
        ))
        .is_err());
        assert!(deno_asset(&asset(
            "deno-x86_64-apple-darwin.zip",
            100,
            json!(format!("sha256:{hash}")),
            &good
        ))
        .is_err());
    }

    #[test]
    fn bounded_extraction_rejects_more_than_the_limit() {
        // Exactly max+1 bytes must be written before the cap trips: if the
        // `take(max + 1)` bound regressed, all 5 input bytes would land.
        let mut out = Vec::new();
        assert!(copy_limited("тест", &b"12345"[..], &mut out, 4).is_err());
        assert_eq!(out.len(), 5);
        let mut out = Vec::new();
        copy_limited("тест", &b"1234"[..], &mut out, 4).unwrap();
        assert_eq!(out, b"1234");
    }

    #[test]
    fn resume_plan_maps_part_length_and_status() {
        // No part: a plain GET was sent; any 2xx body streams from byte 0.
        assert_eq!(resume_plan(0, 200), Some(ResumePlan::Restart));
        // Part present + 206: append at the current part length.
        assert_eq!(
            resume_plan(1234, 206),
            Some(ResumePlan::Append { offset: 1234 })
        );
        // Part present + 200: Range was ignored, overwrite from scratch.
        assert_eq!(resume_plan(1234, 200), Some(ResumePlan::Restart));
        // Part present + 416: stale or already complete part, truncate + refetch.
        assert_eq!(resume_plan(1234, 416), Some(ResumePlan::Refetch));
        // Unusable answers must not produce a plan.
        assert_eq!(resume_plan(1234, 404), None);
        assert_eq!(resume_plan(1234, 500), None);
        assert_eq!(resume_plan(0, 416), None);
        assert_eq!(resume_plan(0, 500), None);
    }

    #[test]
    fn only_transient_failures_keep_the_part() {
        assert!(FailureKind::Transient.keeps_part());
        assert!(!FailureKind::Permanent.keeps_part());
        // 5xx/408/429 are "try again later" answers -> keep the part.
        for status in [408, 429, 500, 502, 503] {
            assert_eq!(
                FailureKind::from_http_status(status),
                FailureKind::Transient,
                "HTTP {status}"
            );
        }
        // Other non-success statuses (4xx) -> delete the part.
        for status in [400, 403, 404, 410, 416] {
            assert_eq!(
                FailureKind::from_http_status(status),
                FailureKind::Permanent,
                "HTTP {status}"
            );
        }
    }

    /// Local HTTP server answering `n` requests (one per connection) with
    /// `respond(i, lowercased request)`; returns the URL and the requests seen.
    fn serve<F>(n: usize, respond: F) -> (String, std::thread::JoinHandle<Vec<String>>)
    where
        F: Fn(usize, &str) -> String + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/tool.exe", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for i in 0..n {
                let (mut sock, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    let k = sock.read(&mut buf).unwrap();
                    if k == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..k]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                sock.write_all(respond(i, &text).as_bytes()).unwrap();
                seen.push(text);
            }
            seen
        });
        (url, handle)
    }

    fn reply(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn installer_client_blocks_redirect_before_contacting_destination() {
        let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let target = format!("http://{}/payload", destination.local_addr().unwrap());
        let (url, server) = serve(1, move |_, _| {
            reply("302 Found", &format!("Location: {target}\r\n"), "")
        });
        let error = client().unwrap().get(url).send().unwrap_err();
        assert!(error.is_redirect());
        assert_eq!(server.join().unwrap().len(), 1);
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    fn part_in(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("snatch-setup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("tool.exe.part");
        (dir, part)
    }

    #[test]
    fn a_part_without_a_known_version_is_never_resumed() {
        let (dir, part) = part_in("novalidator");
        std::fs::write(&part, b"OLD-RELEASE-HEAD").unwrap();
        let (url, server) = serve(1, |_, _| reply("200 OK", "ETag: \"v2\"\r\n", "NEW-RELEASE"));
        download(&client().unwrap(), &url, &part, "tool", 1024, &mut |_| {}).unwrap();
        let seen = server.join().unwrap();
        assert!(
            !seen[0].contains("range:"),
            "no validator, no Range: {}",
            seen[0]
        );
        assert_eq!(std::fs::read(&part).unwrap(), b"NEW-RELEASE");
        assert_eq!(read_validator(&part).as_deref(), Some("\"v2\""));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resume_is_bound_to_the_saved_version_by_if_range() {
        // Same version: 206 appended.
        let (dir, part) = part_in("ifrange-same");
        std::fs::write(&part, b"ABC").unwrap();
        std::fs::write(validator_path(&part), "\"v1\"").unwrap();
        let (url, server) = serve(1, |_, _| {
            reply(
                "206 Partial Content",
                "Content-Range: bytes 3-5/6\r\nETag: \"v1\"\r\n",
                "DEF",
            )
        });
        download(&client().unwrap(), &url, &part, "tool", 1024, &mut |_| {}).unwrap();
        let seen = server.join().unwrap();
        assert!(
            seen[0].contains("range: bytes=3-") && seen[0].contains("if-range: \"v1\""),
            "{}",
            seen[0]
        );
        assert_eq!(std::fs::read(&part).unwrap(), b"ABCDEF");
        std::fs::remove_dir_all(dir).ok();

        // A newer release behind the same `latest` URL: the server ignores the
        // Range (200) and the old head is NOT kept.
        let (dir, part) = part_in("ifrange-changed");
        std::fs::write(&part, b"ABC").unwrap();
        std::fs::write(validator_path(&part), "\"v1\"").unwrap();
        let (url, server) = serve(1, |_, _| reply("200 OK", "ETag: \"v2\"\r\n", "XYZ123"));
        download(&client().unwrap(), &url, &part, "tool", 1024, &mut |_| {}).unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b"XYZ123");
        assert_eq!(read_validator(&part).as_deref(), Some("\"v2\""));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_206_for_the_wrong_bytes_restarts_instead_of_splicing() {
        let (dir, part) = part_in("wrong206");
        std::fs::write(&part, b"ABC").unwrap();
        std::fs::write(validator_path(&part), "\"v1\"").unwrap();
        let (url, server) = serve(2, |i, _| {
            if i == 0 {
                reply(
                    "206 Partial Content",
                    "Content-Range: bytes 0-2/6\r\n",
                    "ABC",
                )
            } else {
                reply("200 OK", "ETag: \"v1\"\r\n", "ABCDEF")
            }
        });
        download(&client().unwrap(), &url, &part, "tool", 1024, &mut |_| {}).unwrap();
        let seen = server.join().unwrap();
        assert!(
            !seen[1].contains("range:"),
            "the retry is a whole-file GET: {}",
            seen[1]
        );
        assert_eq!(std::fs::read(&part).unwrap(), b"ABCDEF");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_complete_part_is_kept_on_416_for_its_full_length() {
        // E.g. the SUMS fetch failed right after a full download: the next
        // attempt must not throw the finished part away and start over.
        let (dir, part) = part_in("complete416");
        std::fs::write(&part, b"ABCDEF").unwrap();
        std::fs::write(validator_path(&part), "\"v1\"").unwrap();
        let (url, server) = serve(1, |_, _| {
            reply(
                "416 Range Not Satisfiable",
                "Content-Range: bytes */6\r\nETag: \"v1\"\r\n",
                "",
            )
        });
        download(&client().unwrap(), &url, &part, "tool", 1024, &mut |_| {}).unwrap();
        assert_eq!(server.join().unwrap().len(), 1, "no second request");
        assert_eq!(std::fs::read(&part).unwrap(), b"ABCDEF");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn only_one_installer_per_tool_at_a_time() {
        let (dir, _) = part_in("installlock");
        let first = install_lock(&dir, "yt-dlp").unwrap();
        let busy = install_lock(&dir, "yt-dlp").unwrap_err();
        assert!(busy.contains("уже устанавливается"), "{busy}");
        // Another tool is independent.
        let other = install_lock(&dir, "aria2c").unwrap();
        drop(first);
        install_lock(&dir, "yt-dlp").unwrap();
        drop(other);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn a_running_aria2c_is_replaced_not_left_stale() {
        // A copy of PING.EXE stands in for a running aria2c.exe.
        let (dir, _) = part_in("running");
        let dest = dir.join("tool.exe");
        let system = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        std::fs::copy(system.join("System32").join("PING.EXE"), &dest).unwrap();
        let mut running = std::process::Command::new(&dest)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let tmp = dir.join("tool.exe.part");
        std::fs::write(&tmp, b"new verified build").unwrap();
        assert!(
            std::fs::rename(&tmp, &dest).is_err(),
            "precondition: a running image cannot be overwritten"
        );
        publish_tool(&tmp, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new verified build");
        assert!(!tmp.exists());
        running.kill().ok();
        running.wait().ok();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn stale_same_length_416_fetches_current_release() {
        let (dir, part) = part_in("stale416");
        std::fs::write(&part, b"ABCDEF").unwrap();
        std::fs::write(validator_path(&part), "\"old\"").unwrap();
        let (url, server) = serve(2, |i, _| {
            if i == 0 {
                reply(
                    "416 Range Not Satisfiable",
                    "Content-Range: bytes */6\r\nETag: \"new\"\r\n",
                    "",
                )
            } else {
                reply("200 OK", "ETag: \"new\"\r\n", "XYZ123")
            }
        });
        download(&client().unwrap(), &url, &part, "tool", 1024, &mut |_| {}).unwrap();
        assert_eq!(server.join().unwrap().len(), 2);
        assert_eq!(std::fs::read(&part).unwrap(), b"XYZ123");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_transient_503_retries_and_resumes_in_the_same_attempt() {
        let (dir, part) = part_in("retry-503");
        std::fs::write(&part, b"ABC").unwrap();
        std::fs::write(validator_path(&part), "\"v1\"").unwrap();
        let (url, server) = serve(2, |i, _| {
            if i == 0 {
                reply("503 Service Unavailable", "", "")
            } else {
                reply(
                    "206 Partial Content",
                    "ETag: \"v1\"\r\nContent-Range: bytes 3-5/6\r\n",
                    "DEF",
                )
            }
        });
        let c = client().unwrap();
        retry_transient(|| download(&c, &url, &part, "tool", 1024, &mut |_| {})).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("range: bytes=3-"));
        assert_eq!(std::fs::read(&part).unwrap(), b"ABCDEF");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn interrupted_publish_recovers_previous_binary_before_network() {
        let (dir, _) = part_in("recover-old");
        let dest = dir.join("aria2c.exe");
        std::fs::write(old_path(&dest), b"working binary").unwrap();
        recover_tool(&dest).unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"working binary");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn release_metadata_is_capped_and_assets_share_one_tag() {
        let err = read_metadata(
            &vec![b'x'; MAX_RELEASE_METADATA as usize + 1][..],
            "release",
        )
        .unwrap_err();
        assert_eq!(err.kind, FailureKind::Permanent);
        let (exe, sums) = yt_dlp_release_urls(&json!({"tag_name": "2026.08.19"})).unwrap();
        assert!(exe.contains("/download/2026.08.19/") && sums.contains("/download/2026.08.19/"));
        assert!(yt_dlp_release_urls(&json!({"tag_name": "../evil"})).is_err());
    }

    #[test]
    fn github_rate_limit_403_preserves_parts_but_an_ordinary_403_does_not() {
        for rate_limited in [false, true] {
            let (url, server) = serve(1, move |_, _| {
                reply(
                    "403 Forbidden",
                    if rate_limited {
                        "X-RateLimit-Remaining: 0\r\n"
                    } else {
                        ""
                    },
                    "{}",
                )
            });
            let failure = release_json(&client().unwrap(), &url).unwrap_err();
            assert_eq!(failure.kind.keeps_part(), rate_limited);
            server.join().unwrap();
        }
    }

    #[test]
    fn a_206_from_a_new_release_restarts_even_if_if_range_was_ignored() {
        let (dir, part) = part_in("wrong-etag");
        std::fs::write(&part, b"ABC").unwrap();
        std::fs::write(validator_path(&part), "\"v1\"").unwrap();
        let (url, server) = serve(2, |i, _| {
            if i == 0 {
                reply(
                    "206 Partial Content",
                    "Content-Range: bytes 3-5/6\r\nETag: \"v2\"\r\n",
                    "DEF",
                )
            } else {
                reply("200 OK", "ETag: \"v2\"\r\n", "XYZ123")
            }
        });
        download(&client().unwrap(), &url, &part, "tool", 1024, &mut |_| {}).unwrap();
        let seen = server.join().unwrap();
        assert!(seen[0].contains("if-range: \"v1\""));
        assert!(
            !seen[1].contains("range:"),
            "restart must fetch a whole file: {}",
            seen[1]
        );
        assert_eq!(std::fs::read(&part).unwrap(), b"XYZ123");
        assert_eq!(read_validator(&part).as_deref(), Some("\"v2\""));
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn a_running_yt_dlp_keeps_its_exe_and_verified_part_until_retry() {
        let (dir, _) = part_in("running-ytdlp");
        let dest = dir.join("yt-dlp.exe");
        let system = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let original = system.join("System32").join("PING.EXE");
        std::fs::copy(&original, &dest).unwrap();
        let mut running = std::process::Command::new(&dest)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let tmp = dir.join("yt-dlp.exe.part");
        std::fs::write(&tmp, b"new verified build").unwrap();
        assert!(
            std::fs::rename(&tmp, &dest).is_err(),
            "a running image cannot be replaced"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            std::fs::read(original).unwrap()
        );
        assert_eq!(std::fs::read(&tmp).unwrap(), b"new verified build");
        assert!(!old_path(&dest).exists());
        running.kill().ok();
        running.wait().unwrap();
        std::fs::rename(&tmp, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new verified build");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resumable_part_len_ignores_absent_empty_and_oversized_parts() {
        let dir = std::env::temp_dir().join(format!("snatch-resume-part-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tool.part");
        assert_eq!(resumable_part_len(&p, 100), 0, "absent file");
        std::fs::write(&p, b"").unwrap();
        assert_eq!(resumable_part_len(&p, 100), 0, "empty file");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(resumable_part_len(&p, 100), 3, "partial file");
        assert_eq!(resumable_part_len(&p, 3), 3, "exactly at the cap");
        assert_eq!(resumable_part_len(&p, 2), 0, "larger than the cap");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
