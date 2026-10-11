use super::*;

pub const FILE_EXT: &[&str] = &[
    ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst", ".lz4", ".iso", ".exe", ".msi",
    ".msix", ".appx", ".apk", ".deb", ".rpm", ".dmg", ".pkg", ".whl", ".jar", ".bin", ".img",
    ".pdf", ".epub", ".fb2", ".txt", ".csv", ".mp4", ".mkv", ".webm", ".avi", ".mov", ".flv",
    ".ts", ".m4v", ".mp3", ".m4a", ".aac", ".flac", ".ogg", ".wav", ".jpg", ".jpeg", ".png",
    ".webp", ".gif",
];

pub const ALLOWED_SCHEMES: &[&str] = &["http", "https", "ftp"];
pub const TORRENT_FILES: &[&str] = &[".torrent", ".metalink", ".meta4"];

pub const COOKIES_BROWSERS: &[&str] = &[
    "brave", "chrome", "chromium", "edge", "firefox", "opera", "safari", "vivaldi", "whale",
];

pub fn clean_url(raw: &str) -> String {
    raw.trim().trim_matches(['"', '\'']).trim().to_string()
}

pub(super) fn has_control_chars(u: &str) -> bool {
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
pub(super) fn is_unc_text(s: &str) -> bool {
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

/// Longest chain of links followed when looking for a share behind a folder.
const MAX_LINK_HOPS: u32 = 8;

/// [`is_unc_path`] for a folder that may lead to a share through a symlink or
/// junction (a link in the download folder pointing at //host/share): the lexical test cannot see
/// that, and listing, creating or locking such a folder would authenticate to
/// the remote host just the same. Only the links themselves are read
/// (`symlink_metadata`, `read_link`) - never what they point to - so the check
/// stays off the network. A chain that is too long or a link that cannot be
/// read counts as unsafe.
pub fn reaches_unc(path: &Path) -> bool {
    reaches_unc_via(path, 0, &inspect_component)
}

/// What one component of a path is, as far as links go.
pub(super) enum LinkState {
    /// Does not exist: nothing below it exists either.
    Missing,
    Plain,
    /// A symlink or junction and what it points at (`Err`: unreadable).
    Link(std::io::Result<PathBuf>),
}

fn inspect_component(path: &Path) -> LinkState {
    match std::fs::symlink_metadata(path) {
        Err(_) => LinkState::Missing,
        Ok(meta) if meta.file_type().is_symlink() => LinkState::Link(std::fs::read_link(path)),
        Ok(_) => LinkState::Plain,
    }
}

/// [`reaches_unc`] with the filesystem behind `inspect`, so the chain logic
/// can be tested with links that cannot be created without a privilege.
pub(super) fn reaches_unc_via(
    path: &Path,
    hops: u32,
    inspect: &dyn Fn(&Path) -> LinkState,
) -> bool {
    use std::path::Component as Part;
    if is_unc_path(path) || hops >= MAX_LINK_HOPS {
        return true;
    }
    let mut so_far = PathBuf::new();
    for part in path.components() {
        so_far.push(part);
        // A drive or root is not a link; asking about one would only cost a
        // round trip when it is a mapped network drive.
        if matches!(part, Part::Prefix(_) | Part::RootDir) {
            continue;
        }
        match inspect(&so_far) {
            LinkState::Missing => break,
            LinkState::Plain => {}
            LinkState::Link(Err(_)) => return true,
            LinkState::Link(Ok(target)) => {
                let resolved = match so_far.parent() {
                    Some(parent) if target.is_relative() => parent.join(&target),
                    _ => target,
                };
                if reaches_unc_via(&resolved, hops + 1, inspect) {
                    return true;
                }
            }
        }
    }
    false
}

pub(super) fn scheme_of(u: &str) -> String {
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

pub(super) fn netloc_of(u: &str) -> String {
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

pub(super) fn path_from_url(u: &str) -> String {
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

pub(super) fn suffix_of(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or("");
    match file.rfind('.') {
        Some(i) if i > 0 => file[i..].to_lowercase(),
        _ => String::new(),
    }
}

pub(super) fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(super) fn percent_decode(s: &str) -> String {
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

pub(super) fn valid_xt_value(v: &str) -> bool {
    let Some(rest) = v.get(..4) else { return false };
    if !rest.eq_ignore_ascii_case("urn:") {
        return false;
    }
    let rest = &v[4..];
    let Some(i) = rest.find(':') else {
        return false;
    };
    let scheme = &rest[..i];
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let hash = &rest[i + 1..];
    (16..=100).contains(&hash.len()) && hash.chars().all(|c| c.is_ascii_alphanumeric())
}

pub(super) fn has_valid_xt(magnet: &str) -> bool {
    let query = magnet.split_once('?').map(|(_, q)| q).unwrap_or("");
    query.split('&').filter(|s| !s.is_empty()).any(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        k.eq_ignore_ascii_case("xt") && valid_xt_value(&percent_decode(v))
    })
}

pub fn validate_url(url: &str) -> Result<String, String> {
    let u = clean_url(url);
    if u.is_empty() {
        return Err(tr!("Empty link.", "Пустая ссылка.").to_string());
    }
    if has_control_chars(&u) {
        return Err(tr!(
            "The link contains control characters (a line break?). Copy it again.",
            "Ссылка содержит управляющие символы (перенос строки?) — скопируйте её заново."
        )
        .to_string());
    }
    if u.starts_with('-') {
        return Err(tr!(
            "A link cannot start with “-” (this looks like an option, not a URL).",
            "Ссылка не может начинаться с «-» (похоже на опцию, а не на URL)."
        )
        .to_string());
    }
    // Must come before the .torrent is_file() probe below: stat'ing a UNC
    // path already authenticates to the remote host.
    if is_unc_text(&u) {
        return Err(
            tr!("Network UNC paths are not supported: accessing them sends your Windows credentials to a remote server. Save the .torrent file locally or use an http(s)/ftp/magnet link.", "Сетевые UNC-пути (\\\\сервер\\шара) не поддерживаются: обращение к ним \
             отправляет ваши учётные данные Windows на чужой сервер. Сохраните \
             .torrent-файл локально или используйте http(s)/ftp/magnet-ссылку.")
                .to_string(),
        );
    }
    if u.to_lowercase().starts_with("magnet:") {
        if !has_valid_xt(&u) {
            return Err(
                tr!("Invalid magnet link (no valid xt=urn:...). Some “&” characters may have been lost when pasting into the terminal — paste the link again or use -y.", "Ссылка magnet повреждена (не найден корректный xt=urn:...). Похоже, часть \
                 символов «&» потерялась при вставке в терминал — вставьте ссылку ещё раз \
                 или передайте её через -y.")
                    .to_string(),
            );
        }
        return Ok(u);
    }
    let scheme = scheme_of(&u);
    let netloc = netloc_of(&u);
    if ALLOWED_SCHEMES.contains(&scheme.as_str()) && !netloc.is_empty() {
        if netloc.contains('[') && !netloc.contains(']') {
            return Err(tr_format!(
                "Could not parse link: {u:?}",
                "Не получается разобрать ссылку: {u:?}"
            ));
        }
        return Ok(u);
    }
    if TORRENT_FILES.contains(&suffix_of(&u).as_str()) && Path::new(&u).is_file() {
        return Ok(u);
    }
    Err(tr_format!(
        "Unsupported link: {u:?} (expected http(s)://, ftp://, magnet: or a .torrent file).",
        "Не понимаю ссылку: {u:?} (нужен http(s)://, ftp://, magnet: или .torrent-файл)."
    ))
}

pub fn detect_engine(url: &str) -> Engine {
    let u = clean_url(url);
    if scheme_of(&u) == "magnet" {
        return Engine::Aria2;
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
    if TORRENT_FILES.contains(&suffix.as_str())
        || (FILE_EXT.contains(&suffix.as_str()) && !crate::links::aria2_tls_unsupported(&u))
    {
        Engine::Aria2
    } else {
        // Also the answer for a direct file on a host aria2c cannot talk TLS
        // to: yt-dlp downloads direct links itself.
        Engine::YtDlp
    }
}

/// A direct file link that aria2c is the right tool for: a known file type on
/// a host it can reach (see [`crate::links::aria2_tls_unsupported`]).
pub(super) fn direct_via_aria2(url: &str) -> bool {
    is_direct_download(url) && !crate::links::aria2_tls_unsupported(url)
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
        return Err(tr_format!(
            "The cookies browser name contains control characters: {value:?}",
            "Имя браузера для куки содержит управляющие символы: {value:?}"
        ));
    }
    let parts: Vec<&str> = value.split(['+', ':']).collect();
    let head = parts[0].to_lowercase();
    if !COOKIES_BROWSERS.contains(&head.as_str()) {
        return Err(tr_format!("Unknown cookies browser: {value:?} (supported: {}; specify a profile as chrome:Profile 1)", "Неизвестный браузер для куки: {value:?} (поддерживаются: {}; \
             профиль можно указать так: chrome:Profile 1)",
            COOKIES_BROWSERS.join(", ")
        ));
    }
    if parts[1..].iter().any(|p| p.contains("..")) {
        return Err(tr_format!(
            "The cookies profile/container cannot contain “..”: {value:?}",
            "Профиль/контейнер для куки не может содержать «..»: {value:?}"
        ));
    }
    // yt-dlp opens the profile folder itself, and stat'ing a share already
    // sends the user's NetNTLMv2 response to that host (see is_unc_text).
    if parts[1..].iter().any(|p| is_unc_text(p)) {
        return Err(tr_format!(
            "The cookies profile cannot be a network UNC path: {value:?}",
            "Профиль для куки не может быть сетевым UNC-путём: {value:?}"
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
