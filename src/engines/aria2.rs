use super::*;

/// Stamps the file(s) a finished direct download wrote with the
/// Mark-of-the-Web (see [`crate::motw`]).
///
/// Both engines are covered for a *direct link*: aria2 always pins the target
/// name itself, and for yt-dlp `build_with` pins the same name with `-o`
/// (`ytdlp_direct_output`), so the file on disk is known in both cases. What is
/// not stamped: torrents (the file names come from the torrent) and yt-dlp
/// media pages (the title decides the name). The stamp matters for what a user
/// is likely to open - installers, archives, documents.
pub fn mark_direct_download(job: &Job, started: std::time::SystemTime) {
    if is_bittorrent_input(&job.url) || !ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str()) {
        return;
    }
    let name = match job.engine {
        Engine::Aria2 => aria2_job_name(job),
        // The pinned `-o` value is the same name cut to 150 characters; yt-dlp
        // only doubles `%` for its own template parsing, the file keeps one.
        Engine::YtDlp => match ytdlp_direct_output(job) {
            Some(_) => limit_name(&aria2_job_name(job), YTDLP_OUTPUT_LIMIT),
            None => return,
        },
    };
    if !name.is_empty() {
        crate::motw::mark_new_files(&absolute_out_dir(&job.out_dir), &name, &job.url, started);
    }
}

/// Clears what a refused aria2c attempt left in the download folder, so the
/// yt-dlp retry starts clean. aria2c preallocates its target (and writes a
/// `.aria2` control file) before the first byte arrives: the leftover is a
/// full-size file of zeros that yt-dlp would take for a finished download
/// ("has already been downloaded") and report as success.
///
/// Only files written since `started` are removed, so an older file of the
/// same name is never touched. Returns `false` when the pinned name is still
/// taken by such an older file: yt-dlp would skip the download over it, so
/// the retry must not run.
pub fn discard_failed_aria2_attempt(job: &Job, started: std::time::SystemTime) -> bool {
    let dir = absolute_out_dir(&job.out_dir);
    let name = aria2_job_name(job);
    if name.is_empty() {
        return true;
    }
    // Clock granularity and a start time taken just before the process spawns.
    let floor = started
        .checked_sub(std::time::Duration::from_secs(2))
        .unwrap_or(started);
    let written_since = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified().map(|modified| (meta.is_file(), modified)))
            .is_ok_and(|(is_file, modified)| is_file && modified >= floor)
    };
    for candidate in crate::motw::possible_names(&name) {
        let target = dir.join(&candidate);
        let control = dir.join(format!("{candidate}.aria2"));
        // The control file goes with its target, and also alone: aria2c may
        // have written it for a file that was never created.
        if written_since(&target) || written_since(&control) {
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_file(&control);
        }
    }
    !dir.join(&name).exists()
}

/// Absolute output directory without touching the filesystem (build() also
/// mkdirs it; preflight must not): shared by both for collision decisions.
pub(super) fn absolute_out_dir(out_dir: &Path) -> PathBuf {
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
/// pins the resolved server name (or this fallback) with `-o` for direct
/// downloads, so locks/ownership always refer to the actual target.
pub(super) fn url_basename(url: &str) -> String {
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
    // Windows drops trailing dots and spaces when it creates a file, so
    // `clip.mp4.` is really `clip.mp4`: the lock, origin and foreign-pair
    // records (and the pinned `-o`) must name the file that ends up on disk.
    if cfg!(windows) {
        name.truncate(name.trim_end_matches(['.', ' ']).len());
    }
    if name == "." || name == ".." {
        return String::new();
    }
    name
}

/// The name SNATCH pins an aria2 job to. A URL whose path yields no file name
/// ("https://host/", ".../dir/", ".../..") gets a deterministic synthetic
/// one: it keeps the lock/origin/foreign-pair records meaningful (all three
/// are keyed by the target name) and keeps -o pinning the written file.
/// Without it aria2 itself would pick the name - the server's
/// Content-Disposition included - and a pre-existing same-name partial pair
/// would be silently continued, exactly what the F1 guard exists to stop.
pub(super) fn aria2_pinned_name(url: &str) -> String {
    let name = url_basename(url);
    if !name.is_empty() {
        return name;
    }
    format!("snatch-{}.bin", &aria2_url_hash(url)[..12])
}

pub(super) fn aria2_job_name(job: &Job) -> String {
    job.download_name
        .as_ref()
        .map(|n| n.as_str().to_string())
        .unwrap_or_else(|| aria2_pinned_name(&job.url))
}

/// Lock and ownership records live outside Downloads, shared by CLI/GUI and
/// both editions of SNATCH. A bare .aria2 file does NOT establish that its
/// source URL matches this job; aria2 -c can overwrite an unrelated partial.
pub(super) fn aria2_state_dir() -> Result<PathBuf, String> {
    config_dir().map(|p| p.join("aria2-locks"))
}

/// NTFS compares file names without Unicode's context-sensitive final-sigma
/// lowercasing. Lowercase each scalar on its own: ΚΑΛΟΣ and καλοσ must hash
/// to the same lock even when a final Σ would become ς in str::to_lowercase.
pub(crate) fn ntfs_case_key(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

pub(super) fn aria2_target_key(out: &Path, name: &str) -> String {
    let canonical = std::fs::canonicalize(out).unwrap_or_else(|_| out.to_path_buf());
    let path = canonical.to_string_lossy();
    let key = if cfg!(windows) {
        ntfs_case_key(&path)
    } else {
        path.into_owned()
    };
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    hash.update([0]);
    hash.update(
        if cfg!(windows) {
            ntfs_case_key(name)
        } else {
            name.to_string()
        }
        .as_bytes(),
    );
    hex_digest(&hash.finalize())
}

pub(super) fn aria2_origin_path(out: &Path, name: &str, state_dir: &Path) -> PathBuf {
    state_dir.join(format!("{}.origin", aria2_target_key(out, name)))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

pub(super) fn aria2_url_hash(url: &str) -> String {
    hex_digest(&Sha256::digest(url.as_bytes()))
}

pub(super) fn canonical_is_unc(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(path.components().next(), Some(Component::Prefix(p))
        if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))
}

pub(super) fn aria2_owned_partial_in(job: &Job, out: &Path, state_dir: &Path) -> bool {
    let name = aria2_job_name(job);
    !name.is_empty()
        && out.join(&name).is_file()
        && out.join(format!("{name}.aria2")).is_file()
        && std::fs::read_to_string(aria2_origin_path(out, &name, state_dir))
            .is_ok_and(|fingerprint| fingerprint == aria2_url_hash(&job.url))
}

pub(super) fn aria2_owned_partial(job: &Job, out: &Path) -> bool {
    aria2_state_dir().is_ok_and(|state| aria2_owned_partial_in(job, out, &state))
}

/// The file yt-dlp keeps a pinned direct download in until it is complete:
/// `<-o name>.part`. `None` for anything that is not such a download.
fn ytdlp_part_path(job: &Job, out: &Path) -> Option<PathBuf> {
    ytdlp_direct_output(job)?;
    let name = limit_name(&aria2_job_name(job), YTDLP_OUTPUT_LIMIT);
    Some(out.join(format!("{name}.part")))
}

/// yt-dlp continues whatever `<name>.part` it finds, whoever left it, and never
/// asks where the bytes came from: a partial of another link under the same
/// name ends up inside this one's file, with a clean exit. Only a partial this
/// app recorded for the SAME URL (the origin record the lock writes) may be
/// continued; anything else must stop before yt-dlp runs. Returns the file
/// name for messages.
pub(super) fn ytdlp_foreign_part_in(job: &Job, out: &Path, state_dir: &Path) -> Option<String> {
    let part = ytdlp_part_path(job, out)?;
    if !part.is_file() {
        return None;
    }
    let owned = std::fs::read_to_string(aria2_origin_path(out, &aria2_job_name(job), state_dir))
        .is_ok_and(|fingerprint| fingerprint == aria2_url_hash(&job.url));
    (!owned).then(|| {
        part.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    })
}

pub(super) fn ytdlp_foreign_part(job: &Job, out: &Path) -> Option<String> {
    match aria2_state_dir() {
        Ok(state) => ytdlp_foreign_part_in(job, out, &state),
        // Ownership cannot be checked: treat the partial as someone else's.
        Err(_) => ytdlp_part_path(job, out)
            .filter(|part| part.is_file())
            .map(|part| {
                part.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
    }
}

/// Jobs whose target is guarded by the same-name lock and the origin record:
/// aria2's own downloads, and direct links yt-dlp saves under a pinned name.
fn needs_target_lock(job: &Job) -> bool {
    (job.engine == Engine::Aria2 || ytdlp_direct_output(job).is_some())
        && ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str())
}

pub(super) fn aria2_foreign_target(job: &Job, out: &Path) -> bool {
    let name = aria2_job_name(job);
    ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str())
        && !name.is_empty()
        && out.join(name).exists()
        && !aria2_owned_partial(job, out)
}

/// F1 (audit 4): a foreign pair - same-name file plus a `.aria2` control file
/// that our origin records cannot claim. aria2 auto-continues such a partial
/// even without `-c`, mixing another URL's bytes into the result, so the pair
/// must not be touched: refuse before the loader runs (delete is the user's
/// call, not ours). Returns the file name for messages.
pub(super) fn aria2_foreign_pair(job: &Job, out: &Path) -> Option<String> {
    if !ALLOWED_SCHEMES.contains(&scheme_of(&job.url).as_str()) {
        return None;
    }
    let name = aria2_job_name(job);
    if name.is_empty() {
        return None;
    }
    (out.join(&name).is_file()
        && out.join(format!("{name}.aria2")).is_file()
        && !aria2_owned_partial(job, out))
    .then_some(name)
}

/// Hold the OS lock from before build() until the child process exits. The
/// GUI tries once (never block its render thread); CLI jobs wait their turn.
/// Lock files stay in place while in use, or another process could lock a new
/// inode while an earlier download still holds the old one; only a record idle
/// for [`ARIA2_STATE_MAX_AGE`] (and not held) is swept.
pub fn lock_aria2_target(job: &Job, wait: bool) -> Result<Option<std::fs::File>, String> {
    if !needs_target_lock(job) {
        return Ok(None);
    }
    let state_dir = aria2_state_dir()?;
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| sweep_aria2_state(&state_dir, ARIA2_STATE_MAX_AGE));
    lock_aria2_target_in(job, wait, &state_dir)
}

/// Lock and origin records are one tiny file per (folder, name) pair and
/// nothing else removes them. A record untouched for this long belongs to a
/// download nobody is going to resume.
pub(super) const ARIA2_STATE_MAX_AGE: std::time::Duration =
    std::time::Duration::from_secs(180 * 24 * 3600);

/// Drops `.origin` records and `.lock` files idle for `max_age`. A lock that is
/// currently held is never removed (it belongs to a running download), and the
/// handle is closed before deleting, as Windows requires.
pub(super) fn sweep_aria2_state(state_dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path.extension().and_then(|e| e.to_str());
        if !matches!(extension, Some("lock" | "origin")) {
            continue;
        }
        let idle = entry.metadata().is_ok_and(|meta| {
            meta.is_file()
                && meta
                    .modified()
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > max_age)
        });
        if !idle {
            continue;
        }
        if extension == Some("lock") {
            let free = crate::config::open_lock_file(&path, false)
                .is_ok_and(|file| FileExt::try_lock(&file).is_ok());
            if !free {
                continue;
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

pub(super) fn lock_aria2_target_in(
    job: &Job,
    wait: bool,
    state_dir: &Path,
) -> Result<Option<std::fs::File>, String> {
    if !needs_target_lock(job) {
        return Ok(None);
    }
    let name = aria2_job_name(job);
    if name.is_empty() {
        return Ok(None);
    }
    let out = absolute_out_dir(&job.out_dir);
    if reaches_unc(&out) || is_unc_path(state_dir) {
        return Err(tr!(
            "A network UNC folder cannot be used for downloads or aria2 locks",
            "Сетевая UNC-папка не подходит для загрузки или блокировок aria2"
        )
        .into());
    }
    std::fs::create_dir_all(&out)
        .map_err(|e| tr_format!("aria2 download folder: {e}", "папка загрузки aria2: {e}"))?;
    if canonical_is_unc(
        &std::fs::canonicalize(&out)
            .map_err(|e| tr_format!("aria2 download folder: {e}", "папка загрузки aria2: {e}"))?,
    ) {
        return Err(tr!(
            "The aria2 download folder resolves to a network UNC path",
            "Папка загрузки aria2 ведёт на сетевой UNC-путь"
        )
        .into());
    }
    std::fs::create_dir_all(state_dir)
        .map_err(|e| tr_format!("aria2 lock folder: {e}", "папка блокировок aria2: {e}"))?;
    let key = aria2_target_key(&out, &name);
    let lock = crate::config::open_lock_file(&state_dir.join(format!("{key}.lock")), true)
        .map_err(|e| tr_format!("aria2 lock: {e}", "блокировка aria2: {e}"))?;
    if wait {
        FileExt::lock(&lock).map_err(|e| tr_format!("aria2 lock: {e}", "блокировка aria2: {e}"))?;
    } else {
        match FileExt::try_lock(&lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(tr_format!(
                    "“{name}” is already downloading to this folder — retry when it finishes",
                    "Файл «{name}» уже скачивается в эту папку — повторите после завершения"
                ))
            }
            Err(TryLockError::Error(e)) => {
                return Err(tr_format!("aria2 lock: {e}", "блокировка aria2: {e}"))
            }
        }
    }
    // Claim only an empty slot. Never certify an existing file or .aria2 as
    // ours just because the URL currently points at the same basename.
    // (a yt-dlp download also needs its `<name>.part` slot to be free)
    let part_taken = ytdlp_part_path(job, &out).is_some_and(|part| part.exists());
    if !out.join(&name).exists() && !out.join(format!("{name}.aria2")).exists() && !part_taken {
        std::fs::write(
            aria2_origin_path(&out, &name, state_dir),
            aria2_url_hash(&job.url),
        )
        .map_err(|e| tr_format!("aria2 resume marker: {e}", "метка докачки aria2: {e}"))?;
    }
    Ok(Some(lock))
}

/// A fresh run never needs `-c`: auto-renaming stays on even when the file
/// did not exist at build time, closing the build→spawn collision race.
/// Resume is allowed only for a partial this app recorded for the SAME URL.
pub(super) fn aria2_flags(owned_partial: bool) -> Vec<&'static str> {
    if owned_partial {
        ARIA2_RESUME.to_vec()
    } else {
        ARIA2_RESUME
            .iter()
            .copied()
            .filter(|f| *f != "-c" && *f != "--auto-file-renaming=false")
            .chain(std::iter::once("--auto-file-renaming=true"))
            .collect()
    }
}
