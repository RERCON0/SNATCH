use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256, Sha512};

const YT_DLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const YT_DLP_SUMS_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-512SUMS";
const ARIA2_API: &str = "https://api.github.com/repos/aria2/aria2/releases/latest";
const MAX_ARIA2_ZIP_BYTES: u64 = 32 * 1024 * 1024;
const MAX_YT_DLP_BYTES: u64 = 256 * 1024 * 1024;
// Official aria2 1.37.0 win-64bit release. GitHub's API reports digest:null
// for this older release, so keep an independently pinned ZIP hash in source.
const PINNED_ARIA2_ZIP: &str = "aria2-1.37.0-win-64bit-build1.zip";
const PINNED_ARIA2_SHA256: &str = "67d015301eef0b612191212d564c5bb0a14b5b9c4796b76454276a4d28d9b288";

/// Sanity cap for the extracted aria2c.exe (real one is a few MB). Without a
/// limit, `io::copy` out of the zip would happily fill the disk from a
/// deflate bomb served by a compromised/mirrored "release".
const MAX_ARIA2C_BYTES: u64 = 64 * 1024 * 1024;

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("snatch-rs/", env!("CARGO_PKG_VERSION")))
        // reqwest has NO timeouts by default: a half-dead connection (proxy
        // accepted TCP, then went silent) blocks read() forever - in the GUI
        // that's the Setup phase hung with no cancel button.
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())
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
    Permanent,
}

impl FailureKind {
    /// Whether a `.part` file may still be useful after this failure.
    fn keeps_part(self) -> bool {
        matches!(self, Self::Transient)
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
        Self { kind: FailureKind::Transient, message: message.into() }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self { kind: FailureKind::Permanent, message: message.into() }
    }

    fn from_http_status(status: u16, message: impl Into<String>) -> Self {
        Self { kind: FailureKind::from_http_status(status), message: message.into() }
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

/// GET with an optional `Range: bytes=<range_from>-` resume header.
fn send_get(
    c: &reqwest::blocking::Client,
    url: &str,
    label: &str,
    range_from: u64,
) -> Result<reqwest::blocking::Response, DownloadFailure> {
    let mut req = c.get(url);
    if range_from > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={range_from}-"));
    }
    req.send()
        .map_err(|e| DownloadFailure::transient(format!("не удалось скачать {label}: {e}")))
}

fn download(c: &reqwest::blocking::Client, url: &str, dest: &Path, label: &str, max: u64) -> Result<(), DownloadFailure> {
    match download_inner(c, url, dest, label, max) {
        Ok(()) => Ok(()),
        Err(f) => {
            // Transient network failures keep a non-empty `.part` so the next
            // attempt can Range-resume it. Permanent failures (4xx, size cap)
            // and empty leftovers are removed - an empty part resumes nothing.
            let keep = f.kind.keeps_part()
                && std::fs::metadata(dest).map(|m| m.len() > 0).unwrap_or(false);
            if !keep {
                let _ = std::fs::remove_file(dest);
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
) -> Result<(), DownloadFailure> {
    let part_len = resumable_part_len(dest, max);
    let mut resp = send_get(c, url, label, part_len)?;
    let mut plan = resume_plan(part_len, resp.status().as_u16());
    if let Some(ResumePlan::Refetch) = plan {
        // 416: the part is stale or already complete. Truncate it and retry
        // once without Range - a 416 error body must never be streamed as
        // content. Truncating first also means a failed retry cannot leave the
        // same stale part to 416 forever.
        File::create(dest).map_err(|e| {
            DownloadFailure::permanent(format!("не удалось создать {}: {e}", dest.display()))
        })?;
        resp = send_get(c, url, label, 0)?;
        plan = resume_plan(0, resp.status().as_u16());
    }

    let (mut file, offset, expected) = match plan {
        Some(ResumePlan::Append { offset }) => {
            let file = OpenOptions::new().append(true).open(dest).map_err(|e| {
                DownloadFailure::permanent(format!("не удалось открыть {} для дозаписи: {e}", dest.display()))
            })?;
            (file, offset, resp.content_length().unwrap_or(0))
        }
        Some(ResumePlan::Restart) => {
            let file = File::create(dest).map_err(|e| {
                DownloadFailure::permanent(format!("не удалось создать {}: {e}", dest.display()))
            })?;
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
            ProgressStyle::with_template("{spinner} {wide_bar} {bytes}/{total_bytes} (осталось {eta})")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        )
    } else {
        // No Content-Length: a determinate bar would read "N/0".
        ProgressBar::new_spinner()
    };
    if offset > 0 {
        bar.set_position(offset);
    }
    let mut buf = [0u8; 65536];
    let mut written = offset;
    loop {
        let n = resp.read(&mut buf).map_err(|e| DownloadFailure::transient(format!("сбой чтения {label}: {e}")))?;
        if n == 0 {
            break;
        }
        if n as u64 > max - written {
            return Err(DownloadFailure::permanent(format!("{label} больше {max} байт — загрузка остановлена")));
        }
        file.write_all(&buf[..n]).map_err(|e| DownloadFailure::permanent(format!("сбой записи {label}: {e}")))?;
        written += n as u64;
        bar.inc(n as u64);
    }
    bar.finish_and_clear();
    Ok(())
}

/// Stream a file through a hasher in 64 KiB chunks: yt-dlp.exe can be up to
/// MAX_YT_DLP_BYTES, and reading it whole would spike memory for no reason.
fn file_hash_hex<D: Digest>(path: &Path, what: &str) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
    let mut hasher = D::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("сбой чтения {what} {}: {e}", path.display()))?;
        if n == 0 { break; }
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
    let asset = release["assets"].as_array()
        .and_then(|assets| assets.iter().find(|a| {
            a["name"].as_str().is_some_and(|name| name.contains("win-64bit") && name.ends_with(".zip"))
        }))
        .ok_or("в релизе aria2 не найден win-64bit zip")?;
    let name = asset["name"].as_str().unwrap_or_default();
    let size = asset["size"].as_u64().ok_or("у aria2 zip нет размера в GitHub API")?;
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
    let url = asset["browser_download_url"].as_str().ok_or("у aria2 zip нет URL")?;
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("некорректный URL aria2: {e}"))?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com")
        || parsed.username() != "" || parsed.password().is_some()
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

fn copy_limited(reader: impl Read, mut writer: impl Write, max: u64) -> Result<(), String> {
    let n = std::io::copy(&mut reader.take(max + 1), &mut writer)
        .map_err(|e| format!("сбой распаковки: {e}"))?;
    if n > max { return Err(format!("aria2c.exe больше {max} байт — похоже на подмену или битый архив")); }
    Ok(())
}

pub fn install_yt_dlp(bin: &Path) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Автоустановка yt-dlp поддерживается только на Windows.".to_string());
    }
    match install_yt_dlp_inner(bin) {
        Ok(p) => Ok(p),
        Err(f) => {
            // Transient failures keep `yt-dlp.exe.part` for the next attempt's
            // Range-resume; only permanent ones clean it up.
            if !f.kind.keeps_part() {
                let _ = std::fs::remove_file(bin.join("yt-dlp.exe.part"));
            }
            Err(f.message)
        }
    }
}

fn install_yt_dlp_inner(bin: &Path) -> Result<PathBuf, DownloadFailure> {
    let c = client().map_err(DownloadFailure::permanent)?;
    let dest = bin.join("yt-dlp.exe");
    let tmp = bin.join("yt-dlp.exe.part");
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Скачиваю yt-dlp.exe (официальный релиз)…");
    }
    download(&c, YT_DLP_URL, &tmp, "yt-dlp.exe", MAX_YT_DLP_BYTES)?;

    let sums = c
        .get(YT_DLP_SUMS_URL)
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
    let sums_text = sums
        .text()
        .map_err(|e| DownloadFailure::transient(format!("не удалось прочитать SHA2-512SUMS: {e}")))?;
    let expected = yt_dlp_checksum(&sums_text)
        .ok_or_else(|| DownloadFailure::permanent("в SHA2-512SUMS нет записи для yt-dlp.exe"))?;

    let got = sha512_hex(&tmp).map_err(DownloadFailure::permanent)?;
    if !got.eq_ignore_ascii_case(expected) {
        let _ = std::fs::remove_file(&tmp);
        return Err(DownloadFailure::permanent("контрольная сумма SHA-512 не совпала — файл не сохранён"));
    }

    std::fs::rename(&tmp, &dest)
        .map_err(|e| DownloadFailure::permanent(format!("не удалось установить yt-dlp.exe: {e}")))?;
    Ok(dest)
}

pub fn install_aria2(bin: &Path) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Автоустановка aria2c поддерживается только на Windows.".to_string());
    }
    match install_aria2_inner(bin) {
        Ok(p) => Ok(p),
        Err(f) => {
            // Transient failures keep both `.part` files for the next attempt's
            // Range-resume; only permanent ones clean them up.
            if !f.kind.keeps_part() {
                let _ = std::fs::remove_file(bin.join("aria2c.exe.part"));
                let _ = std::fs::remove_file(bin.join("aria2.zip.part"));
            }
            Err(f.message)
        }
    }
}

fn install_aria2_inner(bin: &Path) -> Result<PathBuf, DownloadFailure> {
    let c = client().map_err(DownloadFailure::permanent)?;
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Ищу свежий релиз aria2…");
    }
    let api = c
        .get(ARIA2_API)
        .send()
        .map_err(|e| DownloadFailure::transient(format!("не удалось получить список релизов aria2: {e}")))?;
    if !api.status().is_success() {
        return Err(DownloadFailure::from_http_status(
            api.status().as_u16(),
            format!("не удалось получить список релизов aria2: HTTP {}", api.status()),
        ));
    }
    let release: serde_json::Value = api
        .json()
        .map_err(|e| DownloadFailure::permanent(format!("не удалось разобрать ответ API релизов aria2: {e}")))?;
    let (url, expected_sha256) = aria2_asset(&release).map_err(DownloadFailure::permanent)?;

    let zip_path = bin.join("aria2.zip.part");
    download(&c, &url, &zip_path, "aria2 (zip)", MAX_ARIA2_ZIP_BYTES)?;
    let got = sha256_hex(&zip_path).map_err(DownloadFailure::permanent)?;
    if !got.eq_ignore_ascii_case(&expected_sha256) {
        // The archive is unusable and must not be resumed later.
        let _ = std::fs::remove_file(&zip_path);
        return Err(DownloadFailure::permanent("SHA-256 архива aria2 не совпала — установка остановлена"));
    }

    let dest = bin.join("aria2c.exe");
    let tmp = bin.join("aria2c.exe.part");
    let extracted = (|| -> Result<(), String> {
        let file = File::open(&zip_path).map_err(|e| format!("не удалось открыть скачанный zip: {e}"))?;
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
            let mut entry =
                archive.by_index(entry_idx).map_err(|e| format!("не удалось прочитать zip: {e}"))?;
            let mut out =
                File::create(&tmp).map_err(|e| format!("не удалось создать {}: {e}", tmp.display()))?;
            copy_limited(&mut entry, &mut out, MAX_ARIA2C_BYTES)?;
        }
        std::fs::rename(&tmp, &dest).map_err(|e| format!("не удалось установить aria2c.exe: {e}"))
    })();
    // The archive file handle must be dropped before Windows lets us delete
    // it, so cleanup happens after the extraction block regardless of outcome
    // (previously the zip leaked in bin\ on every failure path).
    let _ = std::fs::remove_file(&zip_path);
    extracted.map_err(DownloadFailure::permanent)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aria2_release_requires_verifiable_bounded_official_zip() {
        let asset = |name: &str, size: u64, digest: serde_json::Value, url: &str| {
            json!({"assets": [{"name": name, "size": size, "digest": digest,
                "browser_download_url": url}]})
        };
        let pinned_url = format!("https://github.com/aria2/aria2/releases/download/release-1.37.0/{PINNED_ARIA2_ZIP}");
        assert_eq!(aria2_asset(&asset(PINNED_ARIA2_ZIP, 2475379, serde_json::Value::Null, &pinned_url))
            .unwrap().1, PINNED_ARIA2_SHA256);
        let name = "aria2-2.0-win-64bit-build1.zip";
        let future_url = format!("https://github.com/aria2/aria2/releases/download/release-2.0/{name}");
        assert!(aria2_asset(&asset(name, 2500000, serde_json::Value::Null, &future_url)).is_err());
        let hash = "a".repeat(64);
        assert_eq!(aria2_asset(&asset(name, 2500000, json!(format!("sha256:{hash}")), &future_url))
            .unwrap().1, hash);
        assert!(aria2_asset(&asset(name, MAX_ARIA2_ZIP_BYTES + 1, json!(format!("sha256:{hash}")), &future_url)).is_err());
        assert!(aria2_asset(&asset(name, 2500000, json!(format!("sha256:{hash}")),
            "https://evil.example/aria2.zip")).is_err());
    }

    #[test]
    fn sha256_of_downloaded_archive_is_computed_in_chunks() {
        let dir = std::env::temp_dir().join(format!("snatch-aria-checksum-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("archive.zip");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_hex(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checksum_selects_exact_executable_not_a_similar_name() {
        let sums = "aaa  other-yt-dlp.exe\nbbb *yt-dlp.exe\nccc  yt-dlp.exe.bad\n";
        assert_eq!(yt_dlp_checksum(sums), Some("bbb"));
        assert_eq!(yt_dlp_checksum("aaa yt-dlp.exe.bad"), None);
    }

    #[test]
    fn bounded_extraction_rejects_more_than_the_limit() {
        // Exactly max+1 bytes must be written before the cap trips: if the
        // `take(max + 1)` bound regressed, all 5 input bytes would land.
        let mut out = Vec::new();
        assert!(copy_limited(&b"12345"[..], &mut out, 4).is_err());
        assert_eq!(out.len(), 5);
        let mut out = Vec::new();
        copy_limited(&b"1234"[..], &mut out, 4).unwrap();
        assert_eq!(out, b"1234");
    }

    #[test]
    fn resume_plan_maps_part_length_and_status() {
        // No part: a plain GET was sent; any 2xx body streams from byte 0.
        assert_eq!(resume_plan(0, 200), Some(ResumePlan::Restart));
        // Part present + 206: append at the current part length.
        assert_eq!(resume_plan(1234, 206), Some(ResumePlan::Append { offset: 1234 }));
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
            assert_eq!(FailureKind::from_http_status(status), FailureKind::Transient, "HTTP {status}");
        }
        // Other non-success statuses (4xx) -> delete the part.
        for status in [400, 403, 404, 410, 416] {
            assert_eq!(FailureKind::from_http_status(status), FailureKind::Permanent, "HTTP {status}");
        }
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
