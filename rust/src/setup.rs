use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha512};

const YT_DLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const YT_DLP_SUMS_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-512SUMS";
const ARIA2_API: &str = "https://api.github.com/repos/aria2/aria2/releases/latest";

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("snatch-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

fn download(c: &reqwest::blocking::Client, url: &str, dest: &Path, label: &str) -> Result<(), String> {
    let mut resp = c.get(url).send().map_err(|e| format!("не удалось скачать {label}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("не удалось скачать {label}: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let bar = ProgressBar::new(total).with_style(
        ProgressStyle::with_template("{spinner} {wide_bar} {bytes}/{total_bytes} (осталось {eta})")
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
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

pub fn install_yt_dlp(bin: &Path) -> Result<PathBuf, String> {
    let c = client()?;
    let dest = bin.join("yt-dlp.exe");
    let tmp = bin.join("yt-dlp.exe.part");
    crate::outln("⬇ Скачиваю yt-dlp.exe (официальный релиз)…");
    download(&c, YT_DLP_URL, &tmp, "yt-dlp.exe")?;

    let expected = c
        .get(YT_DLP_SUMS_URL)
        .send()
        .and_then(|r| r.text())
        .map_err(|e| format!("не удалось скачать SHA2-512SUMS: {e}"))?
        .lines()
        .find_map(|line| {
            let mut it = line.split_whitespace();
            let hash = it.next()?;
            let name = it.next()?;
            if name.trim_start_matches('*') == "yt-dlp.exe" {
                Some(hash.to_string())
            } else {
                None
            }
        })
        .ok_or("в SHA2-512SUMS нет записи для yt-dlp.exe")?;

    let got = sha512_hex(&tmp)?;
    if !got.eq_ignore_ascii_case(&expected) {
        let _ = std::fs::remove_file(&tmp);
        return Err("контрольная сумма SHA-512 не совпала — файл не сохранён".to_string());
    }

    std::fs::rename(&tmp, &dest).map_err(|e| format!("не удалось установить yt-dlp.exe: {e}"))?;
    Ok(dest)
}

pub fn install_aria2(bin: &Path) -> Result<PathBuf, String> {
    let c = client()?;
    crate::outln("⬇ Ищу свежий релиз aria2…");
    let release: serde_json::Value = c
        .get(ARIA2_API)
        .send()
        .and_then(|r| r.json())
        .map_err(|e| format!("не удалось получить список релизов aria2: {e}"))?;
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

    let file = File::open(&zip_path).map_err(|e| e.to_string())?;
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

    let dest = bin.join("aria2c.exe");
    let tmp = bin.join("aria2c.exe.part");
    {
        let mut entry = archive.by_index(entry_idx).map_err(|e| e.to_string())?;
        let mut out = File::create(&tmp).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("сбой распаковки: {e}"))?;
    }
    std::fs::rename(&tmp, &dest).map_err(|e| format!("не удалось установить aria2c.exe: {e}"))?;
    let _ = std::fs::remove_file(&zip_path);
    Ok(dest)
}
