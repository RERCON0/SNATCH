//! Torrent support on top of aria2c: listing a torrent's files so the user can
//! pick a subset before the download starts.
//!
//! - `--show-files` gives the file list (idx|path + size on the next line),
//!   but only for a LOCAL .torrent/.metalink: a magnet first gets its
//!   metadata from the swarm (`--bt-metadata-only`), an http(s) link is
//!   downloaded - both into a private temp dir;
//! - `--select-file=1,3` downloads only the chosen indices;
//! - the fetched .torrent then becomes the download's own input
//!   ([`Choice::meta`]): aria2 downloads exactly what was listed and does not
//!   fetch the metadata a second time.
//!
//! Live stats stay on aria2's `--summary-interval` console output - never
//! `--enable-rpc`, which turns aria2 into a daemon that does not exit after
//! the download.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::job_object;

/// Upper bound for one listing: a magnet waits for peers to hand out its
/// metadata, which can take a while on a thin swarm.
pub const METADATA_TIMEOUT: Duration = Duration::from_secs(120);

/// A real .torrent is kilobytes to a few MB. Anything bigger behind an
/// http(s) link is not one and must not be buffered whole in memory.
const MAX_TORRENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LISTING_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 1024 * 1024;
const MAX_LISTED_FILES: usize = 50_000;
// Bound the derived folder tree too: 50k individually nested 64-level paths
// can otherwise expand into millions of nodes despite the input byte cap.
const MAX_TREE_COMPONENTS: usize = 256_000;

const CANCELLED: &str = "получение списка файлов отменено";

/// Deepest folder level the tree shows; anything below is folded into one
/// "a/b/c" name. Every tree walk (build, counts, rows, drop) recurses per
/// level, and a hostile torrent can nest thousands of folders - enough to
/// overflow the UI thread's stack and take every running download with it.
pub const MAX_TREE_DEPTH: usize = 64;

/// `--select-file` budget. Windows caps a whole command line at 32767 chars;
/// ranges keep normal selections tiny, so only thousands of scattered single
/// files can get near this - those are refused with a clear message instead
/// of failing to start with "os error 206".
const MAX_SELECT_SPEC_LEN: usize = 16_000;

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// 1-based index as aria2 names it (--select-file uses these).
    pub index: usize,
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentInfo {
    pub name: String,
    pub total: u64,
    pub files: Vec<FileEntry>,
}

/// A .torrent fetched for a magnet / http(s) input. Every record of one job
/// (queued, running, paused) shares it; the private temp dir holding the
/// file is removed once the last of them is dropped.
#[derive(Debug, Clone)]
pub struct Meta(Arc<FetchedTorrent>);

#[derive(Debug)]
struct FetchedTorrent {
    path: PathBuf,
    _dir: TempDir,
}

impl Meta {
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Marks the metadata as in use (every start/resume of its job), so
    /// another SNATCH instance's leftover sweep never mistakes the dir of a
    /// long-paused job for a stale one.
    pub fn touch(&self) {
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(self.path()) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
    }
}

/// What the user picked for one torrent job. The default (every file, no
/// fetched metadata) is also what every non-torrent job carries.
#[derive(Debug, Clone, Default)]
pub struct Choice {
    /// aria2 file indices (`--select-file`); None = every file.
    pub files: Option<Vec<usize>>,
    /// Metadata fetched while listing, used as aria2's input instead of the
    /// magnet/URL; None = aria2 fetches it itself.
    pub meta: Option<Meta>,
}

/// Result of [`show_files`].
pub struct Listing {
    pub info: TorrentInfo,
    /// Some for magnet/http inputs (see [`Choice::meta`]), None for a local
    /// file - that one is the user's own and is only ever read.
    pub meta: Option<Meta>,
}

/// Parse the output of `aria2c --show-files <local .torrent>`:
///
/// ```text
/// Name: snatch-test
/// Total Length: 29KiB (30,200)
/// Files:
/// idx|path/length
/// ===+======================================
///   1|./snatch-test/dir/hello.txt
///    |25KiB (26,000)
/// ---+--------------------------------------
///   2|./snatch-test/readme.txt
///    |4.1KiB (4,200)
/// ```
pub fn parse_show_files(output: &str) -> Result<TorrentInfo, String> {
    // A torrent aria2 cannot read still exits 0 with "Exception: ..." on
    // stdout, and that message may quote attacker-controlled text shaped
    // like a listing: never parse such output as one.
    if let Some(line) = output.lines().map(str::trim).find(|l| l.starts_with("Exception:")) {
        return Err(format!("aria2 не смог прочитать торрент: {}", crate::ui::clip(line, 160)));
    }
    let mut name = String::new();
    let mut total = 0u64;
    let mut files: Vec<FileEntry> = Vec::new();
    let mut components = 0usize;
    let mut in_files = false;
    for line in output.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("Name: ") {
            name = rest.trim().to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("Total Length: ") {
            total = parse_size(rest).unwrap_or(0);
            continue;
        }
        if line.starts_with("idx|path/length") {
            // A malicious Comment field can contain its own fake "Files"
            // section; only the LAST header wins, and the indices must come
            // out as exactly 1..N (validated below).
            files.clear();
            components = 0;
            in_files = true;
            continue;
        }
        if !in_files {
            continue;
        }
        if line.starts_with("---") || line.starts_with("===") || line.starts_with(">>>") {
            continue;
        }
        // Entry line: "  1|./name/path" — the size follows on the next line
        // as "   |25KiB (26,000)". A path may itself contain '|', so split on
        // the FIRST one only.
        if let Some((idx, path)) = line.split_once('|') {
            let idx = idx.trim();
            let path = path.trim();
            if idx.is_empty() {
                // size line for the previous entry
                if let Some(entry) = files.last_mut() {
                    entry.size = parse_size(path).unwrap_or(0);
                }
                continue;
            }
            if let Ok(index) = idx.parse::<usize>() {
                if files.len() >= MAX_LISTED_FILES {
                    return Err("слишком много файлов для списка — скачайте торрент целиком".into());
                }
                let clean = path.strip_prefix("./").unwrap_or(path);
                components += clean.split('/').filter(|s| !s.is_empty()).count().min(MAX_TREE_DEPTH);
                if components > MAX_TREE_COMPONENTS {
                    return Err("слишком сложное дерево файлов для списка — скачайте торрент целиком".into());
                }
                // Drop the leading "<name>/" that aria2 adds to every path.
                let clean = clean.strip_prefix(&format!("{name}/")).unwrap_or(clean);
                files.push(FileEntry { index, path: clean.to_string(), size: 0 });
            }
        }
    }
    if files.is_empty() {
        return Err("aria2 не вернул список файлов (не удалось получить метаданные?)".into());
    }
    for (n, file) in files.iter().enumerate() {
        if file.index != n + 1 {
            return Err("подозрительный список файлов (непоследовательные индексы)".into());
        }
    }
    let summed = files.iter().try_fold(0u64, |sum, file| sum.checked_add(file.size))
        .ok_or("размеры в списке файлов переполняют u64")?;
    if total == 0 { total = summed; }
    Ok(TorrentInfo { name, total, files })
}

/// "29KiB (30,200)" or "4.1KiB (4,200)" -> bytes from the parenthesised
/// exact number when present, else the human-readable prefix.
pub fn parse_size(text: &str) -> Option<u64> {
    if let Some(open) = text.find('(') {
        if let Some(close) = text[open..].find(')') {
            let exact: String = text[open + 1..open + close].chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = exact.parse::<u64>() {
                return Some(n);
            }
        }
    }
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "" => 1.0,
        "kib" | "kb" => 1024.0,
        "mib" | "mb" => 1024.0 * 1024.0,
        "gib" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    let value = num * mult;
    if !value.is_finite() || value < 0.0 || value >= u64::MAX as f64 { return None; }
    Some(value as u64)
}

/// Private, uniquely named temp dir, removed with its contents on drop.
/// `DirBuilder::create` (not `create_dir_all`) refuses a pre-existing path,
/// so nothing planted under a guessed name in a shared /tmp is ever reused.
#[derive(Debug)]
struct TempDir(PathBuf);

const TEMP_PREFIX: &str = "snatch-torrent-";

/// A leftover dir of another process is swept once nothing in it was used
/// for this long. Live jobs refresh their .torrent on every start
/// ([`Meta::touch`]), so only a week-long pause can lose it - and then the
/// download falls back to the magnet, or asks to pick again (http).
const STALE_TEMP_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

impl TempDir {
    fn new() -> Result<TempDir, String> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        static SWEPT: std::sync::Once = std::sync::Once::new();
        let base = std::env::temp_dir();
        SWEPT.call_once(|| sweep_stale_temp_dirs(&base, STALE_TEMP_AGE));
        for _ in 0..32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let dir = base.join(format!(
                "{TEMP_PREFIX}{}-{}-{nanos}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            match builder.create(&dir) {
                Ok(()) => return Ok(TempDir(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("временная папка: {e}")),
            }
        }
        Err("временная папка: не удалось подобрать свободное имя".into())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A hard exit (window closed mid-listing, Ctrl+C in the CLI) skips the
/// TempDir drop. Remove such leftovers of OTHER processes once nothing in
/// them changed for `max_age`: only our exact
/// "snatch-torrent-<pid>-<seq>-<nanos>" names, only real directories (never
/// a symlink), never this process's own.
fn sweep_stale_temp_dirs(base: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    let own = std::process::id().to_string();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(TEMP_PREFIX)) else { continue };
        let parts: Vec<&str> = rest.split('-').collect();
        let ours = parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !ours || parts[0] == own {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !meta.is_dir() {
            continue;
        }
        // Newest of the dir and its files: Meta::touch refreshes the
        // .torrent inside, which does not move the dir's own mtime.
        let newest = std::fs::read_dir(entry.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|f| f.metadata().ok()?.modified().ok())
            .chain(meta.modified().ok())
            .max();
        let idle = newest.and_then(|m| m.elapsed().ok()).is_some_and(|age| age > max_age);
        if idle {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Starts a windowless aria2c inside a kill-on-close job object: if this
/// process dies mid-listing (window closed, Ctrl+C), the OS takes aria2c
/// with it instead of leaving a metadata fetch running in the background.
fn spawn_hidden(mut command: Command) -> Result<(Child, Option<job_object::Job>), String> {
    let _install_lock = crate::tools::lock_for_spawn(Path::new(command.get_program()))?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    job_object::spawn(&mut command, true).map_err(|e| format!("запуск/защита дерева aria2c: {e}"))
}

enum Waited {
    Exited,
    TimedOut,
    Cancelled,
    Failed(String),
}

/// Polls `child` until it exits; cancellation, read failure or timeout reaps it.
fn wait_child(child: &mut Child, timeout: Duration, cancel: &AtomicBool, pipe_failed: Option<&AtomicBool>) -> Waited {
    fn reap(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Waited::Exited,
            Ok(None) => {}
            Err(e) => {
                reap(child);
                return Waited::Failed(format!("aria2c: {e}"));
            }
        }
        if cancel.load(Ordering::Relaxed) {
            reap(child);
            return Waited::Cancelled;
        }
        if pipe_failed.is_some_and(|failed| failed.load(Ordering::Acquire)) {
            reap(child);
            return Waited::Failed("ошибка чтения списка файлов aria2c".into());
        }
        if start.elapsed() > timeout {
            reap(child);
            return Waited::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What `--show-files` reads: the user's own local file (read only, never
/// moved or deleted) or a copy fetched into a private temp dir.
enum Source {
    Local(PathBuf),
    Fetched(Meta),
}

fn resolve(aria2c: &Path, input: &str, timeout: Duration, cancel: &AtomicBool) -> Result<Source, String> {
    let path = Path::new(input);
    // Before is_file(): even probing \\host\share performs SMB auth against
    // that host (NetNTLMv2 leak) - same rule as engines::build.
    if crate::engines::is_unc_path(path) {
        return Err("сетевой UNC-путь не поддерживается — скопируйте .torrent на этот компьютер".into());
    }
    if path.is_file() {
        return Ok(Source::Local(path.to_path_buf()));
    }
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("magnet:") {
        return fetch_magnet(aria2c, input, timeout, cancel).map(Source::Fetched);
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return fetch_http(input, cancel).map(Source::Fetched);
    }
    Err("этот торрент-вход нельзя разобрать заранее".into())
}

/// Metadata-only aria2 run: fetches just the .torrent from the swarm into a
/// private temp dir and exits on its own.
fn fetch_magnet(aria2c: &Path, magnet: &str, timeout: Duration, cancel: &AtomicBool) -> Result<Meta, String> {
    let dir = TempDir::new()?;
    let mut command = Command::new(aria2c);
    command
        .arg("--no-conf")
        .arg("--bt-metadata-only=true")
        .arg("--bt-save-metadata=true")
        // aria2's own stop condition as well: a dead magnet (no peers) ends
        // by itself even if nothing is left to reap this process.
        .arg(format!("--bt-stop-timeout={}", timeout.as_secs().max(1)))
        .arg("-d")
        .arg(dir.path())
        .arg("--")
        .arg(magnet)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (mut child, _job) = spawn_hidden(command)?;
    match wait_child(&mut child, timeout, cancel, None) {
        Waited::Exited => {}
        Waited::TimedOut => return Err("не удалось получить метаданные торрента (таймаут)".into()),
        Waited::Cancelled => return Err(CANCELLED.into()),
        Waited::Failed(e) => return Err(e),
    }
    // The dir is private and fresh, so the only .torrent in it is ours
    // ("<infohash>.torrent").
    let path = find_torrent(dir.path()).ok_or_else(|| "не удалось получить метаданные торрента из роя".to_string())?;
    Ok(Meta(Arc::new(FetchedTorrent { path, _dir: dir })))
}

/// Downloads a user-chosen .torrent (size-capped) into a private temp dir.
/// Unlike Yandex API-supplied URLs, these may point to any user-selected LAN,
/// tracker or CDN host. Cross-host HTTP redirects are intentional; there are
/// no browser/session credentials on this client.
fn fetch_http(url: &str, cancel: &AtomicBool) -> Result<Meta, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("snatch/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let bytes = runtime.block_on(crate::http::get_bytes(&client, url, MAX_TORRENT_BYTES, cancel))
        .map_err(|e| if e == "__cancelled__" { CANCELLED.into() } else { e })?;
    if !bytes.starts_with(b"d") {
        return Err("по ссылке не торрент-файл".into());
    }
    let dir = TempDir::new()?;
    let path = dir.path().join("remote.torrent");
    std::fs::write(&path, &bytes).map_err(|e| format!("временная папка: {e}"))?;
    Ok(Meta(Arc::new(FetchedTorrent { path, _dir: dir })))
}

fn find_torrent(dir: &Path) -> Option<PathBuf> {
    // `extension() == "torrent"` also skips a half-written "x.torrent__temp".
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
    })
}

/// List the files of a magnet / .torrent (local or http(s)) via
/// `aria2c --show-files`. Magnets need metadata from the swarm first, so
/// this can take a while: it is bounded by `timeout` and stops (killing its
/// aria2c) as soon as `cancel` is set.
pub fn show_files(aria2c: &Path, input: &str, timeout: Duration, cancel: &AtomicBool) -> Result<Listing, String> {
    let source = resolve(aria2c, input, timeout, cancel)?;
    let local = match &source {
        Source::Local(path) => path.as_path(),
        Source::Fetched(meta) => meta.path(),
    };
    if cancel.load(Ordering::Relaxed) { return Err(CANCELLED.into()); }
    if std::fs::metadata(local).map_err(|e| e.to_string())?.len() > MAX_TORRENT_BYTES {
        return Err("слишком большой торрент для списка — скачайте его целиком".into());
    }
    let mut command = Command::new(aria2c);
    command
        .arg("--no-conf")
        .arg("--show-files")
        .arg("--")
        .arg(local)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (child, proc_job) = spawn_hidden(command)?;
    let (stdout, stderr) = collect_listing(child, proc_job, timeout, cancel,
        (MAX_LISTING_BYTES, MAX_STDERR_BYTES))?;
    let info = parse_show_files(&String::from_utf8_lossy(&stdout)).map_err(|e| {
        let stderr = String::from_utf8_lossy(&stderr);
        let hint = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        if hint.is_empty() { e } else { format!("{e} ({})", crate::ui::clip(hint, 160)) }
    })?;
    let meta = match source {
        Source::Local(_) => None,
        Source::Fetched(meta) => Some(meta),
    };
    Ok(Listing { info, meta })
}

fn collect_listing(mut child: Child, proc_job: Option<job_object::Job>,
    timeout: Duration, cancel: &AtomicBool, limits: (u64, u64)) -> Result<(Vec<u8>, Vec<u8>), String> {
    // Drain both pipes on reader threads: a torrent with hundreds of files
    // makes --show-files write far more than the OS pipe buffer, and waiting
    // for exit without reading would deadlock the child mid-write (the UI
    // then sits on "получаю список файлов" until the timeout).
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let pipe_failed = Arc::new(AtomicBool::new(false));
    let out_failed = pipe_failed.clone();
    let out_thread = std::thread::spawn(move || {
        let result = read_listing(&mut stdout_pipe, limits.0);
        if result.is_err() { out_failed.store(true, Ordering::Release); }
        result
    });
    let err_failed = pipe_failed.clone();
    let err_thread = std::thread::spawn(move || {
        let result = read_listing(&mut stderr_pipe, limits.1);
        if result.is_err() { err_failed.store(true, Ordering::Release); }
        result
    });
    // Closing an overflowing pipe alone does not make a child exit: it may
    // ignore write errors and keep the other pipe open. Stop without waiting
    // for the metadata timeout, and retain the reader's actual size/I/O error.
    let waited = wait_child(&mut child, timeout, cancel, Some(&pipe_failed));
    if let Some(job) = &proc_job { job.terminate(); }
    let stdout = out_thread.join().unwrap_or_else(|_| Err("поток списка файлов завершился с ошибкой".into()));
    let stderr = err_thread.join().unwrap_or_else(|_| Err("поток диагностики завершился с ошибкой".into()));
    match waited {
        Waited::Exited => {}
        Waited::TimedOut => return Err("aria2c не выдал список файлов (таймаут)".into()),
        Waited::Cancelled => return Err(CANCELLED.into()),
        Waited::Failed(e) => return Err(stdout.err().or_else(|| stderr.err()).unwrap_or(e)),
    }
    let stdout = stdout?;
    let stderr = stderr?;
    Ok((stdout, stderr))
}

fn read_listing(reader: impl Read, max: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader.take(max + 1).read_to_end(&mut bytes).map_err(|e| format!("чтение списка: {e}"))?;
    if bytes.len() as u64 > max { return Err("слишком большой список файлов торрента — скачайте всё".into()); }
    Ok(bytes)
}

pub fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.2} GiB", b / KIB / KIB / KIB)
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / KIB / KIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// One node of the torrent file tree (qbittorrent-style collapsible view).
#[derive(Default)]
pub struct TorrNode {
    pub name: String,
    pub size: u64,
    pub expanded: bool,
    /// Some(index into `TorrentInfo::files`) for leaf entries.
    pub file: Option<usize>,
    pub children: Vec<TorrNode>,
    /// Refreshed only when a picker changes its selection, never per frame.
    pub selection: (usize, usize),
}

pub type TreeRow = (Vec<usize>, usize);

pub struct TreeView {
    pub rows: Vec<TreeRow>,
    pub rows_dirty: bool,
    pub selection_dirty: bool,
    pub selected: usize,
}

impl Default for TreeView {
    fn default() -> Self {
        Self { rows: Vec::new(), rows_dirty: true, selection_dirty: true, selected: 0 }
    }
}

impl TreeView {
    pub fn prepare(&mut self, tree: &mut [TorrNode], selected: &[bool]) {
        fn counts(nodes: &mut [TorrNode], selected: &[bool]) -> (usize, usize) {
            nodes.iter_mut().fold((0, 0), |(a, b), node| {
                node.selection = match node.file {
                    Some(i) => (selected.get(i).copied().unwrap_or(false) as usize, 1),
                    None => counts(&mut node.children, selected),
                };
                (a + node.selection.0, b + node.selection.1)
            })
        }
        fn flatten(nodes: &[TorrNode], depth: usize, prefix: &mut Vec<usize>, rows: &mut Vec<TreeRow>) {
            for (i, node) in nodes.iter().enumerate() {
                prefix.push(i);
                rows.push((prefix.clone(), depth));
                if node.file.is_none() && node.expanded { flatten(&node.children, depth + 1, prefix, rows); }
                prefix.pop();
            }
        }
        if self.selection_dirty {
            self.selected = counts(tree, selected).0;
            self.selection_dirty = false;
        }
        if self.rows_dirty {
            self.rows.clear();
            flatten(tree, 0, &mut Vec::new(), &mut self.rows);
            self.rows_dirty = false;
        }
    }
}

impl TorrNode {
    /// (selected, total) file counters under this node.
    pub fn selected_count(&self, selected: &[bool]) -> (usize, usize) {
        match self.file {
            Some(i) => (selected.get(i).copied().unwrap_or(false) as usize, 1),
            None => self.children.iter().fold((0, 0), |(a, b), c| {
                let (x, y) = c.selected_count(selected);
                (a + x, b + y)
            }),
        }
    }

    pub fn set_all(&self, selected: &mut [bool], value: bool) {
        if let Some(i) = self.file {
            if let Some(s) = selected.get_mut(i) {
                *s = value;
            }
        } else {
            for c in &self.children {
                c.set_all(selected, value);
            }
        }
    }
}

enum TreeEntry {
    File { index: usize, size: u64 },
    Dir { size: u64, children: Level },
}

/// One folder level while building the tree.
#[derive(Default)]
struct Level {
    entries: BTreeMap<String, TreeEntry>,
    /// Key of the folder standing for a torrent folder name - the name
    /// itself, or "name (n)" when a same-named file got there first - so
    /// every later file of that folder lands in the same one.
    dir_for: HashMap<String, String>,
    /// Next " (n)" to try per base name: thousands of identical names cost
    /// O(1) each instead of re-probing (2), (3), … every time.
    next_suffix: HashMap<String, usize>,
}

impl Level {
    /// `base` if free, else the first free "base (n)".
    fn free_key(&mut self, base: &str) -> String {
        if !self.entries.contains_key(base) {
            return base.to_string();
        }
        let mut n = self.next_suffix.get(base).copied().unwrap_or(2);
        loop {
            let cand = format!("{base} ({n})");
            n += 1;
            if !self.entries.contains_key(&cand) {
                self.next_suffix.insert(base.to_string(), n);
                return cand;
            }
        }
    }

    /// The child level for folder `name`, created on first use.
    fn dir(&mut self, name: &str, size: u64) -> &mut Level {
        let key = match self.dir_for.get(name) {
            Some(key) => key.clone(),
            None => {
                let key = self.free_key(name);
                self.entries
                    .insert(key.clone(), TreeEntry::Dir { size: 0, children: Level::default() });
                self.dir_for.insert(name.to_string(), key.clone());
                key
            }
        };
        match self.entries.get_mut(&key) {
            Some(TreeEntry::Dir { size: total, children }) => {
                *total = total.saturating_add(size);
                children
            }
            _ => unreachable!("dir_for only ever names folders"),
        }
    }
}

/// Folders-and-files tree from the flat aria2 listing ("/"-separated paths).
/// Built over sorted maps (O(n log n)) and emitted folders-first. Duplicate
/// names (a repeated path, a file and a folder sharing a name) never
/// overwrite each other - the later entry gets a " (2)" suffix - and paths
/// deeper than [`MAX_TREE_DEPTH`] are folded so no walk can blow the stack.
pub fn build_torrent_tree(files: &[FileEntry]) -> Vec<TorrNode> {
    let mut root = Level::default();
    for (idx, entry) in files.iter().enumerate() {
        let mut parts: Vec<&str> = entry.path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let folded;
        if parts.len() > MAX_TREE_DEPTH {
            folded = parts[MAX_TREE_DEPTH - 1..].join("/");
            parts.truncate(MAX_TREE_DEPTH - 1);
            parts.push(&folded);
        }
        let (leaf, dirs) = parts.split_last().expect("non-empty");
        let mut level = &mut root;
        for dir in dirs {
            level = level.dir(dir, entry.size);
        }
        let key = level.free_key(leaf);
        level.entries.insert(key, TreeEntry::File { index: idx, size: entry.size });
    }
    fn to_nodes(level: Level) -> Vec<TorrNode> {
        let mut dirs = Vec::new();
        let mut leaves = Vec::new();
        for (name, entry) in level.entries {
            match entry {
                TreeEntry::Dir { size, children } => dirs.push(TorrNode {
                    name,
                    size,
                    expanded: false,
                    file: None,
                    children: to_nodes(children),
                    selection: (0, 0),
                }),
                TreeEntry::File { index, size } => leaves.push(TorrNode {
                    name,
                    size,
                    expanded: false,
                    file: Some(index),
                    children: Vec::new(),
                    selection: (0, 0),
                }),
            }
        }
        dirs.extend(leaves);
        dirs
    }
    to_nodes(root)
}

/// `--select-file` value for the chosen aria2 indices: sorted, deduplicated
/// and folded into ranges ("1-9998,10000"), so unticking one file of a huge
/// torrent stays short. Ok(None) = nothing chosen; Err = still too long for
/// a command line (thousands of scattered single files).
pub fn select_spec(indices: &[usize]) -> Result<Option<String>, String> {
    let mut sorted = indices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut spec = String::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        if !spec.is_empty() {
            spec.push(',');
        }
        if end == start {
            spec.push_str(&start.to_string());
        } else {
            spec.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    if spec.is_empty() {
        return Ok(None);
    }
    if spec.len() > MAX_SELECT_SPEC_LEN {
        return Err("слишком много разрозненных файлов в выборе — отметьте папки целиком или скачайте всё".into());
    }
    Ok(Some(spec))
}

/// Descend a tree by index path (rows of the CLI picker carry these).
pub fn node_at<'a>(nodes: &'a [TorrNode], path: &[usize]) -> &'a TorrNode {
    let mut cur = &nodes[path[0]];
    for &i in &path[1..] {
        cur = &cur.children[i];
    }
    cur
}

pub fn node_at_mut<'a>(nodes: &'a mut [TorrNode], path: &[usize]) -> &'a mut TorrNode {
    let (first, rest) = path.split_first().expect("empty path");
    let mut cur = &mut nodes[*first];
    for &i in rest {
        cur = &mut cur.children[i];
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_numeric_sizes_are_rejected_instead_of_wrapping_or_panicking() {
        assert_eq!(parse_size("999999999999999999999999999999999999999999999999GiB"), None);
        let output = "Name: huge\nFiles:\nidx|path/length\n  1|./huge/a\n   |x (18446744073709551615)\n  2|./huge/b\n   |1B (1)\n";
        assert!(parse_show_files(output).unwrap_err().contains("переполняют"));
    }

    #[test]
    #[ignore = "requires real aria2c in PATH"]
    fn real_aria2_rejects_newline_paths_and_keeps_real_indices_after_comments() {
        let aria2 = crate::tools::which("aria2c").expect("aria2c in PATH");
        let dir = TempDir::new().unwrap();
        for (name, comment, rejected) in [
            ("evil\nidx|path/length\n  1|./fake", "", true),
            ("good.bin", "Name: fake\nFiles:\nidx|path/length\n  1|./fake/x\n   |9B (9)\n", false),
        ] {
            let mut torrent = format!("d7:comment{}:{}4:infod5:filesld6:lengthi1e4:pathl{}:{}eee4:name4:test12:piece lengthi16384e6:pieces20:", comment.len(), comment, name.len(), name).into_bytes();
            torrent.extend_from_slice(&[0u8; 20]); torrent.extend_from_slice(b"ee");
            let path = dir.path().join("input.torrent"); std::fs::write(&path, torrent).unwrap();
            let result = show_files(&aria2, path.to_str().unwrap(), Duration::from_secs(5), &AtomicBool::new(false));
            if rejected { assert!(result.is_err(), "aria2 must reject injected path controls"); }
            else { let info = result.unwrap().info; assert_eq!(info.name, "test"); assert_eq!(info.files.len(), 1); assert_eq!(info.files[0].path, "good.bin"); assert_eq!(info.files[0].index, 1); }
        }
    }

    #[test]
    fn an_oversized_listing_stops_a_child_that_does_not_exit_on_broken_pipe() {
        let exe = std::env::current_exe().unwrap();
        for stream in ["stdout", "stderr"] {
            let mut command = Command::new(&exe);
            command.args(["--exact", "torrent::tests::listing_overflow_fixture", "--nocapture"])
                .env("SNATCH_LISTING_OVERFLOW_FIXTURE", stream)
                .current_dir(exe.parent().unwrap()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let (child, job) = job_object::spawn(&mut command, true).unwrap();
            let began = Instant::now();
            let error = collect_listing(child, job, Duration::from_secs(2), &AtomicBool::new(false), (1024, 1024)).unwrap_err();
            assert!(error.contains("слишком большой список"), "{stream}: {error}");
            assert!(began.elapsed() < Duration::from_secs(1), "overflow must stop the process before its timeout");
        }
    }

    #[test]
    fn listing_overflow_fixture() {
        use std::io::Write;
        let Ok(stream) = std::env::var("SNATCH_LISTING_OVERFLOW_FIXTURE") else { return };
        let bytes = [b'x'; 4096];
        if stream == "stderr" { let _ = std::io::stderr().write_all(&bytes); }
        else { let _ = std::io::stdout().write_all(&bytes); }
        // Deliberately ignore write errors and stay alive, holding the other
        // pipe open. Only the monitor's overflow stop should end this fixture.
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn listing_caps_reject_expansion_but_accept_the_exact_limit() {
        assert_eq!(read_listing(&b"1234"[..], 4).unwrap(), b"1234");
        assert!(read_listing(&b"12345"[..], 4).is_err());
        let mut text = "Name: huge\nidx|path/length\n".to_string();
        for i in 1..=MAX_LISTED_FILES + 1 { text.push_str(&format!("{i}|./huge/f{i}\n |1 (1)\n")); }
        assert!(parse_show_files(&text).unwrap_err().contains("слишком много"));
    }

    #[test]
    fn picker_cache_rebuilds_only_on_expansion_or_selection() {
        let files = vec![FileEntry { index: 1, path: "folder/a".into(), size: 1 },
            FileEntry { index: 2, path: "folder/b".into(), size: 2 }];
        let mut tree = build_torrent_tree(&files);
        let mut selected = vec![true, true];
        let mut view = TreeView::default();
        view.prepare(&mut tree, &selected);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(tree[0].selection, (2, 2));
        let ptr = view.rows[0].0.as_ptr();
        view.prepare(&mut tree, &selected);
        assert_eq!(ptr, view.rows[0].0.as_ptr(), "navigation must reuse row paths");
        tree[0].expanded = true; view.rows_dirty = true;
        view.prepare(&mut tree, &selected);
        assert_eq!(view.rows.len(), 3);
        selected[0] = false; view.selection_dirty = true;
        view.prepare(&mut tree, &selected);
        assert_eq!(tree[0].selection, (1, 2));
        assert_eq!(view.selected, 1);
    }

    #[test]
    fn http_metadata_cancels_before_a_stalled_server_replies() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x.torrent", listener.local_addr().unwrap());
        let (ready, rx) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut bytes = [0u8; 4096]; let _ = socket.read(&mut bytes).unwrap();
            ready.send(()).unwrap(); wait.recv().unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nde").ok();
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let worker = std::thread::spawn(move || fetch_http(&url, &flag));
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let began = Instant::now(); cancel.store(true, Ordering::SeqCst);
        assert!(worker.join().unwrap().is_err());
        assert!(began.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap(); server.join().unwrap();
    }

    /// Captured verbatim from `aria2c --show-files test.torrent` (1.37.0).
    const SHOW_FILES: &str = "\
*** BitTorrent File Information ***
Mode: multi
Announce:
 http://127.0.0.1:1/announce
Info Hash: c01abfff06149cb74765dca1b5226003eeb4e877
Piece Length: 16KiB
The Number of Pieces: 2
Total Length: 29KiB (30,200)
Name: snatch-test
Magnet URI: magnet:?xt=urn:btih:C01ABFFF06149CB74765DCA1B5226003EEB4E877&dn=snatch-test
Files:
idx|path/length
===+===========================================================================
  1|./snatch-test/dir/hello.txt
   |25KiB (26,000)
---+---------------------------------------------------------------------------
  2|./snatch-test/readme.txt
   |4.1KiB (4,200)
---+---------------------------------------------------------------------------
>>> Printing the contents of file 'test.torrent'...
";

    #[test]
    fn parses_show_files_listing() {
        let info = parse_show_files(SHOW_FILES).unwrap();
        assert_eq!(info.name, "snatch-test");
        assert_eq!(info.total, 30_200);
        assert_eq!(info.files.len(), 2);
        assert_eq!(info.files[0].index, 1);
        assert_eq!(info.files[0].path, "dir/hello.txt");
        assert_eq!(info.files[0].size, 26_000);
        assert_eq!(info.files[1].path, "readme.txt");
        assert_eq!(info.files[1].size, 4_200);
    }

    #[test]
    fn show_files_without_entries_is_an_error() {
        assert!(parse_show_files("Name: x\nTotal Length: 0\n").is_err());
    }

    #[test]
    fn comment_field_cannot_inject_a_fake_listing() {
        let evil = "Comment: x\nidx|path/length\n  1|./fake.txt\n   |1B (1)\nTotal Length: 10B (10)\nName: real\nFiles:\nidx|path/length\n===+===\n  1|./real/a.txt\n   |6B (6)\n  2|./b.txt\n   |4B (4)\n";
        let info = parse_show_files(evil).unwrap();
        assert_eq!(info.files.len(), 2);
        assert_eq!(info.files[0].path, "a.txt");
        assert_eq!(info.files[1].path, "b.txt");
    }

    #[test]
    fn non_sequential_indices_are_rejected() {
        let bad = "Name: x\nTotal Length: 10B (10)\nFiles:\nidx|path/length\n===+===\n  1|./a\n   |5B (5)\n  3|./b\n   |5B (5)\n";
        assert!(parse_show_files(bad).is_err());
    }

    #[test]
    fn local_input_is_read_in_place_never_copied_or_owned() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("mine.torrent");
        std::fs::write(&file, b"d4:infod0:e").unwrap();
        let input = file.to_string_lossy().into_owned();
        // aria2c is never started for a local file.
        let source = resolve(Path::new("aria2c-not-needed"), &input, METADATA_TIMEOUT, &AtomicBool::new(false));
        assert!(matches!(source, Ok(Source::Local(ref p)) if *p == file));
        drop(source);
        assert!(file.is_file(), "the user's own .torrent must survive the listing");
    }

    #[test]
    fn temp_dirs_are_unique_private_and_removed_on_drop() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        assert_ne!(a.path(), b.path());
        let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
        std::fs::write(pa.join("x.torrent"), b"d").unwrap();
        assert_eq!(find_torrent(&pa), Some(pa.join("x.torrent")));
        assert_eq!(find_torrent(&pb), None);
        drop(a);
        drop(b);
        assert!(!pa.exists() && !pb.exists());
    }

    #[test]
    fn find_torrent_skips_half_written_metadata() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("abc.torrent__temp"), b"d").unwrap();
        assert_eq!(find_torrent(dir.path()), None);
    }

    #[test]
    fn meta_lives_while_any_job_record_holds_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hash.torrent");
        std::fs::write(&path, b"d").unwrap();
        let meta = Meta(Arc::new(FetchedTorrent { path: path.clone(), _dir: dir }));
        let paused = Choice { files: Some(vec![1]), meta: Some(meta.clone()) };
        drop(meta);
        assert!(path.is_file());
        drop(paused);
        assert!(!path.exists());
    }

    #[test]
    fn choice_feeds_selection_and_fetched_metadata_to_aria2() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hash.torrent");
        std::fs::write(&path, b"d").unwrap();
        let out = dir.path().join("out");
        let meta = Meta(Arc::new(FetchedTorrent { path: path.clone(), _dir: dir }));
        let job = crate::engines::Job {
            engine: "aria2".into(),
            url: "magnet:?xt=urn:btih:c01abfff06149cb74765dca1b5226003eeb4e877".into(),
            out_dir: out,
            fmt: "best".into(),
            cookies_browser: None,
        };
        let tc = crate::tools::Toolchain { yt_dlp: None, aria2c: Some(PathBuf::from("aria2c")) };
        let choice = Choice { files: Some(vec![3, 1, 3]), meta: Some(meta) };
        let cmd = crate::engines::build_for_run(&job, &tc, &choice).unwrap();
        assert_eq!(cmd[cmd.len() - 2], "--");
        assert_eq!(cmd.last().unwrap(), path.as_os_str(), "aria2 must download what was listed");
        assert!(cmd.iter().any(|a| a == "--select-file=1,3"));
        // Metadata gone (temp cleaner during a long pause): fall back to the
        // magnet itself, aria2 fetches it again.
        std::fs::remove_file(&path).unwrap();
        let cmd = crate::engines::build_for_run(&job, &tc, &choice).unwrap();
        assert_eq!(cmd.last().unwrap(), job.url.as_str());
        // An http(s) .torrent may have changed since the pick: the old
        // indices must not be applied to whatever it serves now.
        let http = crate::engines::Job { url: "https://example.com/release.torrent".into(), ..job };
        let err = crate::engines::build_for_run(&http, &tc, &choice).unwrap_err();
        assert!(err.contains("выберите файлы заново"), "{err}");
        let all = Choice { files: None, ..choice };
        assert!(crate::engines::build_for_run(&http, &tc, &all).is_ok(), "all files need no indices");
    }

    #[test]
    fn aria2_exception_output_is_never_a_listing() {
        // An unreadable torrent: aria2 exits 0 and quotes the bad path, which
        // can itself be shaped like a listing.
        let out = "Exception: [x.cc:1] errorCode=26 bad path ./evil\nidx|path/length\n===+===\n  1|./small.txt\n   |1B (1)\n";
        let err = parse_show_files(out).unwrap_err();
        assert!(err.contains("не смог прочитать"), "{err}");
    }

    #[test]
    fn unc_inputs_are_refused_before_any_filesystem_probe() {
        for input in [
            r"\\attacker\share\x.torrent",
            "//attacker/share/x.torrent",
            r"\??\UNC\attacker\share\x.torrent",
        ] {
            let err = resolve(Path::new("aria2c"), input, METADATA_TIMEOUT, &AtomicBool::new(false))
                .err()
                .expect("UNC must be refused");
            assert!(err.contains("UNC"), "{input}: {err}");
        }
    }

    #[test]
    fn select_spec_folds_runs_into_ranges() {
        assert_eq!(select_spec(&[]).unwrap(), None);
        assert_eq!(select_spec(&[3, 1, 3]).unwrap().as_deref(), Some("1,3"));
        assert_eq!(select_spec(&[8, 1, 2, 3, 5, 7]).unwrap().as_deref(), Some("1-3,5,7-8"));
        // 10 000 files, one unticked: tiny instead of ~49 000 chars.
        let all_but_one: Vec<usize> = (1..=10_000).filter(|&i| i != 9_999).collect();
        assert_eq!(select_spec(&all_but_one).unwrap().as_deref(), Some("1-9998,10000"));
        // Thousands of scattered single files cannot fit a command line.
        let scattered: Vec<usize> = (1..=40_000).step_by(2).collect();
        assert!(select_spec(&scattered).is_err());
    }

    #[test]
    fn stale_temp_dirs_of_other_processes_are_swept() {
        let base = TempDir::new().unwrap();
        let own = std::process::id();
        let mk = |name: &str| {
            let p = base.path().join(name);
            std::fs::create_dir(&p).unwrap();
            p
        };
        let dead = mk("snatch-torrent-4000000001-1-1");
        let mine = mk(&format!("snatch-torrent-{own}-1-1"));
        let foreign = mk("snatch-torrent-notours");
        let file = base.path().join("snatch-torrent-4000000002-1-1");
        std::fs::write(&file, b"x").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale_temp_dirs(base.path(), Duration::ZERO);
        assert!(!dead.exists(), "a leftover of another process must go");
        assert!(mine.exists() && foreign.exists() && file.exists());
    }

    #[test]
    fn sweep_counts_file_times_and_touch_moves_them_forward() {
        use std::time::SystemTime;
        let base = TempDir::new().unwrap();
        let live = base.path().join("snatch-torrent-4000000003-1-1");
        std::fs::create_dir(&live).unwrap();
        let torrent = live.join("hash.torrent");
        std::fs::write(&torrent, b"d").unwrap();
        let set_mtime = |t: SystemTime| {
            let file = std::fs::OpenOptions::new().write(true).open(&torrent).unwrap();
            file.set_modified(t).unwrap();
        };
        // A recently used file keeps its whole dir, even past `max_age` of
        // the dir itself: the sweep looks at the files touch() refreshes.
        set_mtime(SystemTime::now() + Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale_temp_dirs(base.path(), Duration::ZERO);
        assert!(torrent.is_file(), "a recently used .torrent must survive the sweep");
        // touch() (every job start/resume) is what moves that time forward.
        set_mtime(SystemTime::now() - Duration::from_secs(24 * 3600));
        let meta = Meta(Arc::new(FetchedTorrent { path: torrent.clone(), _dir: TempDir(live.clone()) }));
        let before = SystemTime::now() - Duration::from_secs(1);
        meta.touch();
        assert!(std::fs::metadata(&torrent).unwrap().modified().unwrap() >= before);
    }

    #[test]
    #[ignore = "needs real aria2c + a torrent; set SNATCH_ARIA2C and SNATCH_TEST_TORRENT"]
    fn show_files_drains_large_listings() {
        let Ok(aria2c) = std::env::var("SNATCH_ARIA2C") else { return };
        let Ok(input) = std::env::var("SNATCH_TEST_TORRENT") else { return };
        let started = std::time::Instant::now();
        let listing = show_files(Path::new(&aria2c), &input, Duration::from_secs(30), &AtomicBool::new(false)).unwrap();
        // The user's own file is only read.
        assert!(Path::new(&input).is_file());
        // Hundreds of files = far more output than one pipe buffer; the old
        // read-after-exit code deadlocked here until the timeout.
        assert!(listing.info.files.len() > 100, "files: {}", listing.info.files.len());
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn file(index: usize, path: &str, size: u64) -> FileEntry {
        FileEntry { index, path: path.into(), size }
    }

    #[test]
    fn tree_folds_folders_and_sums_sizes() {
        let files = [
            file(1, "data/a.bin", 100),
            file(2, "data/sub/b.bin", 50),
            file(3, "top.txt", 7),
        ];
        let tree = build_torrent_tree(&files);
        assert_eq!(tree.len(), 2);
        let data = &tree[0];
        assert_eq!(data.name, "data");
        assert_eq!(data.size, 150);
        assert!(data.file.is_none());
        assert_eq!(data.children.len(), 2);
        let mut selected = vec![true; files.len()];
        assert_eq!(node_at(&tree, &[0]).selected_count(&selected), (2, 2));
        // Folders come first: data.children = [sub(dir), a.bin(file)].
        assert_eq!(node_at(&tree, &[0, 0, 0]).selected_count(&selected), (1, 1));
        let mut tree_mut = tree;
        node_at_mut(&mut tree_mut, &[0]).set_all(&mut selected, false);
        assert_eq!(selected, vec![false, false, true]);
    }

    #[test]
    fn duplicate_paths_never_overwrite_each_other() {
        let files = [
            file(1, "dup.txt", 5),
            file(2, "dup.txt", 7),
            file(3, "data", 1),
            file(4, "data/inner.bin", 2),
            file(5, "data/other.bin", 3),
        ];
        let tree = build_torrent_tree(&files);
        // "dup.txt" and "dup.txt (2)" are both present and selectable, and
        // the file named "data" gets disambiguated from a dir of that name.
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"dup.txt") && names.contains(&"dup.txt (2)"), "{names:?}");
        let root_leaves = tree.iter().filter(|n| n.file.is_some()).count();
        assert_eq!(root_leaves, 3);
        // Both files of the renamed folder share ONE renamed folder.
        let dirs: Vec<&TorrNode> = tree.iter().filter(|n| n.file.is_none()).collect();
        assert_eq!(dirs.len(), 1, "{names:?}");
        assert_eq!(dirs[0].name, "data (2)");
        assert_eq!(dirs[0].children.len(), 2);
        assert_eq!(dirs[0].size, 5);
    }

    #[test]
    fn hostile_nesting_is_folded_so_tree_walks_stay_shallow() {
        // 20 000 nested folders: aria2 accepts it; the old recursive build
        // overflowed a 1 MB main-thread stack at a few thousand levels. A
        // 256 KB thread proves every walk is now bounded by MAX_TREE_DEPTH.
        let path = format!("{}f.bin", "a/".repeat(20_000));
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let files = [file(1, &path, 7), file(2, "top.txt", 1)];
                let tree = build_torrent_tree(&files);
                fn depth(nodes: &[TorrNode]) -> usize {
                    nodes.iter().map(|n| 1 + depth(&n.children)).max().unwrap_or(0)
                }
                let mut selected = vec![true; files.len()];
                assert_eq!(tree[0].selected_count(&selected), (1, 1));
                tree[0].set_all(&mut selected, false);
                (depth(&tree), selected)
            })
            .unwrap();
        let (depth, selected) = handle.join().expect("tree walk must not overflow");
        assert_eq!(depth, MAX_TREE_DEPTH);
        assert_eq!(selected, vec![false, true]);
    }

    #[test]
    fn thousands_of_identical_names_build_in_linear_time() {
        let files: Vec<FileEntry> = (1..=20_000).map(|i| file(i, "a.bin", 1)).collect();
        let started = std::time::Instant::now();
        let tree = build_torrent_tree(&files);
        // Quadratic re-probing took ~5 s for 8 000 names even optimised.
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        assert_eq!(tree.len(), 20_000);
        assert!(tree.iter().any(|n| n.name == "a.bin (20000)"));
    }

    #[test]
    fn genuine_suffixed_folder_is_not_merged_into_a_renamed_one() {
        let files = [file(1, "a", 1), file(2, "a/x", 1), file(3, "a (2)/y", 1)];
        let tree = build_torrent_tree(&files);
        // The renamed "a" took "a (2)" first; the torrent's own "a (2)"
        // folder must stay a separate folder, not absorb "x".
        let dirs: Vec<&TorrNode> = tree.iter().filter(|n| n.file.is_none()).collect();
        assert_eq!(dirs.len(), 2);
        for dir in dirs {
            assert_eq!(dir.children.len(), 1, "{} mixes two folders", dir.name);
        }
    }
}
