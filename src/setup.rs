use std::fs::File;
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

fn download(c: &reqwest::blocking::Client, url: &str, dest: &Path, label: &str, max: u64) -> Result<(), String> {
    match download_inner(c, url, dest, label, max) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Never leave a truncated .part behind on failure.
            let _ = std::fs::remove_file(dest);
            Err(e)
        }
    }
}

fn download_inner(
    c: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    label: &str,
    max: u64,
) -> Result<(), String> {
    let mut resp = c.get(url).send().map_err(|e| format!("не удалось скачать {label}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("не удалось скачать {label}: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    if total > max {
        return Err(format!("{label}: заявленный размер {total} байт превышает лимит {max}"));
    }
    let bar = if total > 0 {
        ProgressBar::new(total).with_style(
            ProgressStyle::with_template("{spinner} {wide_bar} {bytes}/{total_bytes} (осталось {eta})")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        )
    } else {
        // No Content-Length: a determinate bar would read "N/0".
        ProgressBar::new_spinner()
    };
    let mut file = File::create(dest).map_err(|e| format!("не удалось создать {}: {e}", dest.display()))?;
    let mut buf = [0u8; 65536];
    let mut written = 0;
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("сбой чтения {label}: {e}"))?;
        if n == 0 {
            break;
        }
        if n as u64 > max - written {
            return Err(format!("{label} больше {max} байт — загрузка остановлена"));
        }
        file.write_all(&buf[..n]).map_err(|e| format!("сбой записи {label}: {e}"))?;
        written += n as u64;
        bar.inc(n as u64);
    }
    bar.finish_and_clear();
    Ok(())
}

fn sha512_hex(path: &Path) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("не удалось прочитать {}: {e}", path.display()))?;
    let mut hasher = Sha512::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("сбой чтения {}: {e}", path.display()))?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
        Err(e) => {
            let _ = std::fs::remove_file(bin.join("yt-dlp.exe.part"));
            Err(e)
        }
    }
}

fn install_yt_dlp_inner(bin: &Path) -> Result<PathBuf, String> {
    let c = client()?;
    let dest = bin.join("yt-dlp.exe");
    let tmp = bin.join("yt-dlp.exe.part");
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Скачиваю yt-dlp.exe (официальный релиз)…");
    }
    download(&c, YT_DLP_URL, &tmp, "yt-dlp.exe", MAX_YT_DLP_BYTES)?;

    let sums = c
        .get(YT_DLP_SUMS_URL)
        .send()
        .map_err(|e| format!("не удалось скачать SHA2-512SUMS: {e}"))?;
    if !sums.status().is_success() {
        // Without this check a 403/404 body gets parsed as the sums file and
        // surfaces as the misleading "no entry for yt-dlp.exe" error.
        return Err(format!("не удалось скачать SHA2-512SUMS: HTTP {}", sums.status()));
    }
    let sums_text = sums.text().map_err(|e| format!("не удалось прочитать SHA2-512SUMS: {e}"))?;
    let expected = yt_dlp_checksum(&sums_text).ok_or("в SHA2-512SUMS нет записи для yt-dlp.exe")?;

    let got = sha512_hex(&tmp)?;
    if !got.eq_ignore_ascii_case(expected) {
        let _ = std::fs::remove_file(&tmp);
        return Err("контрольная сумма SHA-512 не совпала — файл не сохранён".to_string());
    }

    std::fs::rename(&tmp, &dest).map_err(|e| format!("не удалось установить yt-dlp.exe: {e}"))?;
    Ok(dest)
}

pub fn install_aria2(bin: &Path) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Автоустановка aria2c поддерживается только на Windows.".to_string());
    }
    match install_aria2_inner(bin) {
        Ok(p) => Ok(p),
        Err(e) => {
            let _ = std::fs::remove_file(bin.join("aria2c.exe.part"));
            let _ = std::fs::remove_file(bin.join("aria2.zip.part"));
            Err(e)
        }
    }
}

fn install_aria2_inner(bin: &Path) -> Result<PathBuf, String> {
    let c = client()?;
    if std::io::stdout().is_terminal() {
        crate::outln("⬇ Ищу свежий релиз aria2…");
    }
    let api = c
        .get(ARIA2_API)
        .send()
        .map_err(|e| format!("не удалось получить список релизов aria2: {e}"))?;
    if !api.status().is_success() {
        return Err(format!("не удалось получить список релизов aria2: HTTP {}", api.status()));
    }
    let release: serde_json::Value = api
        .json()
        .map_err(|e| format!("не удалось разобрать ответ API релизов aria2: {e}"))?;
    let (url, expected_sha256) = aria2_asset(&release)?;

    let zip_path = bin.join("aria2.zip.part");
    download(&c, &url, &zip_path, "aria2 (zip)", MAX_ARIA2_ZIP_BYTES)?;
    let got = sha256_hex(&zip_path)?;
    if !got.eq_ignore_ascii_case(&expected_sha256) {
        return Err("SHA-256 архива aria2 не совпала — установка остановлена".into());
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
    extracted?;
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
        let mut out = Vec::new();
        assert!(copy_limited(&b"12345"[..], &mut out, 4).is_err());
        assert!(out.len() <= 5);
        let mut out = Vec::new();
        copy_limited(&b"1234"[..], &mut out, 4).unwrap();
        assert_eq!(out, b"1234");
    }
}
