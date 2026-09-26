use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha512};

const YT_DLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const YT_DLP_SUMS_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-512SUMS";
const ARIA2_API: &str = "https://api.github.com/repos/aria2/aria2/releases/latest";

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

fn download(c: &reqwest::blocking::Client, url: &str, dest: &Path, label: &str) -> Result<(), String> {
    match download_inner(c, url, dest, label) {
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
) -> Result<(), String> {
    let mut resp = c.get(url).send().map_err(|e| format!("не удалось скачать {label}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("не удалось скачать {label}: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
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
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("сбой чтения {label}: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| format!("сбой записи {label}: {e}"))?;
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
    download(&c, YT_DLP_URL, &tmp, "yt-dlp.exe")?;

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
    if !got.eq_ignore_ascii_case(&expected) {
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
    let url = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets.iter().find_map(|a| {
                let name = a["name"].as_str().unwrap_or("");
                if name.contains("win-64bit") && name.ends_with(".zip") {
                    a["browser_download_url"].as_str().map(String::from)
                } else {
                    None
                }
            })
        })
        .ok_or("в релизе aria2 не найден win-64bit zip")?;

    let zip_path = bin.join("aria2.zip.part");
    download(&c, &url, &zip_path, "aria2 (zip)")?;

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
