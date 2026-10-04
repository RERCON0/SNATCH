use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use fs4::FileExt;

pub const MAX_HISTORY: usize = 15;

/// A real config.json is a few KB (15+15 history entries). Anything past
/// this is corrupt or planted and is ignored rather than read whole on
/// every download (Config::load runs on every start and before each run).
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Longest a save waits for another SNATCH process's config.lock. save() runs
/// on the GUI thread: an untimed LockFileEx behind a stalled peer froze the
/// whole window. Losing one history/theme write beats a hung UI.
const SAVE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Only NotFound is an absent configuration. Every other failure must prevent
/// save's read/merge/write from replacing real data with a default snapshot.
pub(crate) fn read_capped(p: &Path, max: u64) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; opts.custom_flags(libc::O_NONBLOCK); }
    let file = match opts.open(p) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "config.json не обычный файл или превышает лимит размера"));
    }
    let mut text = String::new();
    file.take(max + 1).read_to_string(&mut text)?;
    if text.len() as u64 > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "config.json превышает лимит размера"));
    }
    Ok(Some(text))
}

pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
    builder.create(path)
}

pub(crate) fn open_lock_file(path: &Path, create: bool) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(create).truncate(false).read(true).write(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW); }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "файл блокировки не является обычным файлом"));
    }
    Ok(file)
}

/// Exclusive lock, polled instead of blocking so it can give up.
fn lock_within(file: &std::fs::File, wait: std::time::Duration) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match FileExt::try_lock(file) {
            Ok(()) => return Ok(()),
            Err(fs4::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(fs4::TryLockError::WouldBlock) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "config.lock занят другим окном SNATCH",
                ));
            }
            Err(fs4::TryLockError::Error(e)) => return Err(e),
        }
    }
}

pub fn home_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// A base directory from the environment that may be touched at all: set,
/// non-empty and NOT a UNC share. Stat'ing `\\host\share` already sends the
/// user's NetNTLMv2 response to that host, and this dir also holds the
/// self-installed yt-dlp/aria2c that get executed - the same rule PATH
/// entries and SNATCH_YT_DLP/SNATCH_ARIA2C follow in tools.rs.
pub(crate) fn local_base(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| !crate::engines::is_unc_path(p))
}

/// %LOCALAPPDATA%, unless it is unset or points at a network share.
pub fn local_app_data() -> Option<PathBuf> {
    local_base(std::env::var_os("LOCALAPPDATA"))
}

pub fn config_dir() -> Result<PathBuf, String> {
    config_dir_from(
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("XDG_CONFIG_HOME"),
        home_dir().map(PathBuf::into_os_string),
    )
}

fn config_dir_from(
    local_app_data: Option<std::ffi::OsString>,
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    if let Some(base) = local_base(local_app_data).or_else(|| local_base(xdg_config_home)) {
        return Ok(base.join("snatch"));
    }
    if let Some(home) = local_base(home) {
        return Ok(home.join(".config").join("snatch"));
    }
    Err("Нет безопасной папки настроек: задайте локальный LOCALAPPDATA, XDG_CONFIG_HOME или HOME/USERPROFILE. Общая временная папка не используется.".into())
}

#[derive(Serialize)]
pub struct Config {
    pub default_dir: String,
    pub last_dir: String,
    pub urls: Vec<String>,
    pub dirs: Vec<String>,
    /// Incremented by clear-history so a stale GUI can't resurrect old links.
    pub history_epoch: u64,
    /// GUI theme, persisted so it doesn't reset to dark every launch.
    pub dark_mode: bool,
    #[serde(skip)]
    history_cleared: Cell<bool>,
    /// A stale GUI may save its theme after another process cleared history.
    /// Only remember_* calls since the last save may introduce new entries.
    #[serde(skip)]
    url_dirty: Cell<bool>,
    #[serde(skip)]
    dir_dirty: Cell<bool>,
    #[serde(skip)]
    default_dir_snapshot: RefCell<String>,
    #[serde(skip)]
    theme_snapshot: Cell<bool>,
}

fn str_field(obj: Option<&serde_json::Map<String, Value>>, key: &str) -> String {
    obj.and_then(|o| o.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn list_field(obj: Option<&serde_json::Map<String, Value>>, key: &str) -> Vec<String> {
    // Saves never write more than MAX_HISTORY entries; a longer list is a
    // hand-edited or corrupt file and is not worth materialising.
    obj.and_then(|o| o.get(key))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).take(MAX_HISTORY).collect())
        .unwrap_or_default()
}

fn bool_field(obj: Option<&serde_json::Map<String, Value>>, key: &str, default: bool) -> bool {
    obj.and_then(|o| o.get(key)).and_then(Value::as_bool).unwrap_or(default)
}

pub fn sanitize(data: &Value) -> Config {
    let obj = data.as_object();
    Config {
        default_dir: str_field(obj, "default_dir"),
        last_dir: str_field(obj, "last_dir"),
        urls: list_field(obj, "urls"),
        dirs: list_field(obj, "dirs"),
        history_epoch: obj.and_then(|o| o.get("history_epoch"))
            .and_then(Value::as_u64).unwrap_or(0),
        dark_mode: bool_field(obj, "dark_mode", true),
        history_cleared: Cell::new(false),
        url_dirty: Cell::new(false),
        dir_dirty: Cell::new(false),
        default_dir_snapshot: RefCell::new(str_field(obj, "default_dir")),
        theme_snapshot: Cell::new(bool_field(obj, "dark_mode", true)),
    }
}
/// History is a convenience list, not a credential store: the userinfo part
/// of an authority (`https://user:token@host/...`) is dropped before a URL
/// is remembered, so config.json never carries secrets. Running such a link
/// from the history needs the credentials re-entered - safe by default.
fn redact_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let (authority, tail) = match rest.split_once('/') {
        Some((authority, tail)) => (authority, format!("/{tail}")),
        None => (rest, String::new()),
    };
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{tail}"),
        None => url.to_string(),
    }
}

fn dedup_front(list: Vec<String>, value: String) -> Vec<String> {
    let head = value.clone();
    let mut out = vec![value];
    out.extend(list.into_iter().filter(|u| *u != head));
    out.truncate(MAX_HISTORY);
    out
}

fn merge_history(local: &[String], disk: &[String], same_epoch: bool, dirty: bool) -> Vec<String> {
    if !dirty {
        return disk.to_vec();
    }
    // The first local entry is the just-completed download. Then take other
    // processes' recent entries before the rest of this process's stale list.
    let mut merged = Vec::new();
    let old_local = if same_epoch { &local[local.len().min(1)..] } else { &[] };
    for value in local.iter().take(1).chain(disk).chain(old_local) {
        if !merged.contains(value) {
            merged.push(value.clone());
        }
        if merged.len() == MAX_HISTORY { break; }
    }
    merged
}

impl Config {
    pub fn path() -> Result<PathBuf, String> {
        config_dir().map(|p| p.join("config.json"))
    }

    pub fn try_load() -> Result<Config, String> {
        let path = Self::path()?;
        Self::load_checked(&path).map_err(|e| format!("Не удалось прочитать настройки {}: {e}", path.display()))
    }

    pub fn load() -> Config {
        match Self::try_load() {
            Ok(cfg) => cfg,
            Err(e) => { crate::errln(format!("⚠ {e}")); Self::defaults() }
        }
    }

    pub fn load_from(p: &Path) -> Config {
        match Self::load_checked(p) {
            Ok(cfg) => cfg,
            Err(e) => { crate::errln(format!("⚠ Не удалось прочитать настройки {}: {e}; файл сохранён без изменений", p.display())); Self::defaults() }
        }
    }

    fn load_checked(p: &Path) -> std::io::Result<Config> {
        let Some(text) = read_capped(p, MAX_CONFIG_BYTES)? else { return Ok(Self::defaults()); };
        let data: Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
        if !data.is_object() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "config.json должен содержать JSON-объект"));
        }
        Ok(Self::with_default_dir(sanitize(&data)))
    }

    fn defaults() -> Config { Self::with_default_dir(sanitize(&Value::Null)) }

    fn with_default_dir(mut cfg: Config) -> Config {
        if cfg.default_dir.is_empty() {
            cfg.default_dir = home_dir()
                .map(|h| h.join("Downloads").to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        cfg.default_dir_snapshot.replace(cfg.default_dir.clone());
        cfg
    }

    pub fn save(&self) {
        if let Err(e) = self.try_save() { crate::errln(format!("⚠ Не удалось сохранить настройки: {e}")); }
    }

    pub fn try_save(&self) -> Result<(), String> {
        let path = Self::path()?;
        self.try_save_to(&path)
    }

    /// The GUI must not spend three seconds waiting for a peer's lock.
    pub fn try_save_now(&self) -> Result<(), String> {
        let path = Self::path()?;
        self.try_save_to_within(&path, std::time::Duration::ZERO).map_err(|e| e.to_string())
    }

    pub fn try_save_to(&self, p: &Path) -> Result<(), String> {
        self.try_save_to_within(p, SAVE_LOCK_WAIT).map_err(|e| e.to_string())
    }

    pub fn save_to(&self, p: &Path) {
        self.save_to_within(p, SAVE_LOCK_WAIT);
    }

    fn save_to_within(&self, p: &Path, lock_wait: std::time::Duration) {
        if let Err(e) = self.try_save_to_within(p, lock_wait) {
            crate::errln(format!("⚠ Не удалось сохранить настройки: {e}"));
        }
    }

    fn try_save_to_within(&self, p: &Path, lock_wait: std::time::Duration) -> std::io::Result<()> {
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = p.parent() {
                create_private_dir(parent)?;
            }
            // Lock a stable sibling file, not config.json: the config itself
            // is atomically replaced on each save, changing the locked inode.
            // Hold this lock through reload, merge and rename. OS locks are
            // released on process exit, unlike create_new marker files.
            let lock_path = p.with_extension("lock");
            let lock = open_lock_file(&lock_path, true)?;
            lock_within(&lock, lock_wait)?;
            let disk = Self::load_checked(p)?;
            let default_dir = if self.default_dir != *self.default_dir_snapshot.borrow() {
                self.default_dir.clone()
            } else { disk.default_dir.clone() };
            let dark_mode = if self.dark_mode != self.theme_snapshot.get() { self.dark_mode } else { disk.dark_mode };
            let merged = if self.history_cleared.get() {
                Config {
                    default_dir: default_dir.clone(), last_dir: self.last_dir.clone(),
                    urls: self.urls.clone(), dirs: self.dirs.clone(),
                    history_epoch: disk.history_epoch.saturating_add(1),
                    dark_mode,
                    history_cleared: Cell::new(false),
                    url_dirty: Cell::new(false), dir_dirty: Cell::new(false),
                    default_dir_snapshot: RefCell::new(default_dir),
                    theme_snapshot: Cell::new(dark_mode),
                }
            } else {
                let same_epoch = self.history_epoch == disk.history_epoch;
                Config {
                    default_dir: default_dir.clone(),
                    // Like urls/dirs: only a remember_dir() since the last
                    // save may overwrite last_dir. A stale window saving an
                    // unrelated setting must not resurrect a directory that
                    // --clear-history (or a newer download) replaced.
                    last_dir: if self.dir_dirty.get() { self.last_dir.clone() } else { disk.last_dir.clone() },
                    urls: merge_history(&self.urls, &disk.urls, same_epoch, self.url_dirty.get()),
                    dirs: merge_history(&self.dirs, &disk.dirs, same_epoch, self.dir_dirty.get()),
                    history_epoch: disk.history_epoch,
                    dark_mode,
                    history_cleared: Cell::new(false),
                    url_dirty: Cell::new(false), dir_dirty: Cell::new(false),
                    default_dir_snapshot: RefCell::new(default_dir),
                    theme_snapshot: Cell::new(dark_mode),
                }
            };
            // Unique tmp name (pid + nanos): with the old fixed "config.tmp",
            // two instances (GUI + CLI) saving concurrently would truncate
            // each other's tmp and rename a half-written file into place.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let tmp = p.with_file_name(format!("config-{}-{stamp}.tmp", std::process::id()));
            let payload = serde_json::to_string_pretty(&merged)
                .map_err(std::io::Error::other)?;
            {
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                use std::io::Write;
                let mut f = opts.open(&tmp)?;
                f.write_all(payload.as_bytes())?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            std::fs::rename(&tmp, p).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        })();
        if result.is_ok() {
            self.history_cleared.set(false);
            self.url_dirty.set(false);
            self.dir_dirty.set(false);
            self.default_dir_snapshot.replace(self.default_dir.clone());
            self.theme_snapshot.set(self.dark_mode);
        }
        result
    }

    pub fn remember_url(&mut self, url: &str) {
        let url = redact_credentials(url);
        self.urls = dedup_front(std::mem::take(&mut self.urls), url);
        self.url_dirty.set(true);
    }

    pub fn remember_dir(&mut self, directory: &str) {
        let d = crate::engines::expanduser(Path::new(directory))
            .to_string_lossy()
            .into_owned();
        self.last_dir = d.clone();
        self.dirs = dedup_front(std::mem::take(&mut self.dirs), d);
        self.dir_dirty.set(true);
    }

    pub fn clear_history(&mut self) {
        self.history_cleared.set(true);
        self.url_dirty.set(false);
        self.dir_dirty.set(false);
        self.urls = Vec::new();
        self.dirs = Vec::new();
        self.last_dir = String::new();
    }
}

impl Default for Config {
    fn default() -> Self { Self::defaults() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_reads_never_overwrite_the_existing_config() {
        let dir = std::env::temp_dir().join(format!("snatch-config-preserve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let oversized = json!({"urls": ["https://example.test/a"], "pad": "x".repeat(MAX_CONFIG_BYTES as usize)}).to_string().into_bytes();
        for bytes in [oversized, b"{\"urls\":\"x\"}\xff".to_vec(), b"{broken".to_vec()] {
            std::fs::write(&path, &bytes).unwrap();
            let mut cfg = Config::load_from(&path);
            cfg.remember_url("https://example.test/new");
            assert!(cfg.try_save_to(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(cfg.url_dirty.get(), "a rejected write must remain retryable");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn a_transient_read_lock_does_not_wipe_recovered_settings() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("snatch-config-sharing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let bytes = json!({"urls": ["https://example.test/a"], "dirs": ["D:/saved"], "last_dir": "D:/saved", "default_dir": "D:/default", "dark_mode": false}).to_string();
        std::fs::write(&path, &bytes).unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        let cfg = Config::load_from(&path);
        assert!(cfg.try_save_to(&path).is_err());
        drop(held);
        cfg.try_save_to(&path).unwrap();
        let saved = Config::load_from(&path);
        assert_eq!(saved.urls, ["https://example.test/a"]);
        assert_eq!(saved.last_dir, "D:/saved");
        assert_eq!(saved.default_dir, "D:/default");
        assert!(!saved.dark_mode);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_config_is_rejected_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("snatch-config-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let began = std::time::Instant::now();
        assert!(read_capped(&path, MAX_CONFIG_BYTES).is_err());
        assert!(std::time::Instant::now().duration_since(began) < std::time::Duration::from_secs(1));
        std::fs::remove_dir_all(dir).ok();
    }
    use serde_json::json;

    #[test]
    fn sanitize_non_dict_no_crash() {
        let cfg = sanitize(&json!([1, 2, 3]));
        assert_eq!(cfg.urls, Vec::<String>::new());
        assert_eq!(cfg.dirs, Vec::<String>::new());
        assert_eq!(cfg.last_dir, "");
    }

    #[test]
    fn sanitize_wrong_field_types() {
        let cfg = sanitize(&json!({"urls": "abc", "dirs": 5, "last_dir": 42, "default_dir": null}));
        assert_eq!(cfg.urls, Vec::<String>::new());
        assert_eq!(cfg.dirs, Vec::<String>::new());
        assert_eq!(cfg.last_dir, "");
        assert_eq!(cfg.default_dir, "");
    }

    #[test]
    fn sanitize_mixed_list_keeps_strings_only() {
        let cfg = sanitize(&json!({"urls": ["http://ok", 7, null]}));
        assert_eq!(cfg.urls, vec!["http://ok"]);
    }

    #[test]
    fn unknown_keys_ignored() {
        let cfg = sanitize(&json!({"hacker": true, "urls": []}));
        assert_eq!(cfg.urls, Vec::<String>::new());
    }

    #[test]
    fn history_cap_and_dedupe() {
        let mut cfg = sanitize(&Value::Null);
        for i in 0..20 {
            cfg.remember_url(&format!("http://x/{i}"));
        }
        assert_eq!(cfg.urls.len(), MAX_HISTORY);
        assert_eq!(cfg.urls[0], "http://x/19");
        cfg.remember_url("http://x/19");
        assert_eq!(cfg.urls.iter().filter(|u| *u == "http://x/19").count(), 1);
    }

    #[test]
    fn remember_dir_expands_user() {
        let mut cfg = sanitize(&Value::Null);
        cfg.remember_dir("~/Videos");
        assert!(!cfg.last_dir.contains('~') || home_dir().is_none());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let mut cfg = sanitize(&Value::Null);
        cfg.remember_url("https://example.com/v.mp4");
        cfg.remember_dir(dir.to_str().unwrap());
        cfg.save_to(&p);
        let again = Config::load_from(&p);
        assert_eq!(again.urls, cfg.urls);
        assert_eq!(again.dirs, cfg.dirs);
        assert_eq!(again.last_dir, cfg.last_dir);
        let tmp_leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(tmp_leftovers, 0, "save_to must not leave .tmp files behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_snapshots_merge_instead_of_losing_history() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-config-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let mut a = Config::load_from(&p);
        let mut b = Config::load_from(&p);
        let empty_snapshot = Config::load_from(&p);
        a.remember_url("https://example.com/a.zip");
        a.remember_dir("first");
        b.remember_url("https://example.com/b.zip");
        b.remember_dir("second");
        a.save_to(&p);
        b.save_to(&p);
        let saved = Config::load_from(&p);
        assert_eq!(saved.urls, ["https://example.com/b.zip", "https://example.com/a.zip"]);
        assert_eq!(saved.dirs.len(), 2);
        empty_snapshot.save_to(&p);
        assert_eq!(Config::load_from(&p).urls.len(), 2, "an empty snapshot is not a clear-history request");
        b.clear_history();
        b.save_to(&p);
        assert!(Config::load_from(&p).urls.is_empty(), "clear-history must not resurrect disk entries");
        // A long-lived GUI still holding its old snapshot must not bring back
        // the pre-clear tail when it saves a *new* completed download.
        a.remember_url("https://example.com/new.zip");
        a.save_to(&p);
        assert_eq!(Config::load_from(&p).urls, ["https://example.com/new.zip"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn stale_gui_theme_save_cannot_resurrect_cleared_history() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-clear-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let mut old_gui = Config::load_from(&p);
        old_gui.remember_url("https://host/old.mp3");
        old_gui.remember_dir("old-dir");
        old_gui.save_to(&p);
        let mut clearer = Config::load_from(&p);
        clearer.clear_history();
        clearer.save_to(&p);
        old_gui.dark_mode = false;
        old_gui.save_to(&p);
        let after_theme_save = Config::load_from(&p);
        assert!(after_theme_save.urls.is_empty());
        assert!(after_theme_save.dirs.is_empty());
        assert!(after_theme_save.last_dir.is_empty(), "stale last_dir restored");
        old_gui.remember_url("https://host/new.mp3");
        old_gui.save_to(&p);
        let after_new_download = Config::load_from(&p);
        assert_eq!(after_new_download.urls, ["https://host/new.mp3"]);
        assert!(after_new_download.dirs.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn two_writers_keep_both_urls_under_contention() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-config-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writers: Vec<_> = (0..2).map(|i| {
            let p = p.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut cfg = Config::load_from(&p);
                cfg.remember_url(&format!("https://host/{i}.zip"));
                barrier.wait();
                cfg.save_to(&p);
            })
        }).collect();
        for writer in writers { writer.join().unwrap(); }
        let saved = Config::load_from(&p);
        assert_eq!(saved.urls.len(), 2, "one snapshot must not replace the other");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_missing_file_defaults() {
        let p = std::env::temp_dir().join("snatch-rs-nope").join("config.json");
        let cfg = Config::load_from(&p);
        assert!(!cfg.default_dir.is_empty());
    }

    #[test]
    fn unc_bases_are_never_used_for_config_or_tools() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        // Local LOCALAPPDATA wins as before.
        assert_eq!(config_dir_from(os(r"C:\Users\u\AppData\Local"), None, os(r"C:\Users\u")).unwrap(),
            Path::new(r"C:\Users\u\AppData\Local").join("snatch"));
        // A share (or NT-prefix share) is skipped: probing it leaks NetNTLMv2,
        // and bin\ under it would hold executed tools.
        for share in [r"\\evil\share", "//evil/share", r"\??\UNC\evil\share", "/??/UNC/evil/share"] {
            let dir = config_dir_from(os(share), None, os(r"C:\Users\u")).unwrap();
            assert!(!crate::engines::is_unc_path(&dir), "{share} -> {dir:?}");
            assert_eq!(dir, Path::new(r"C:\Users\u").join(".config").join("snatch"));
            assert!(local_base(os(share)).is_none(), "{share} must not be probed");
        }
        // Nothing local at all: still never a share.
        assert!(config_dir_from(os(r"\\evil\a"), os(r"\\evil\b"), os(r"\\evil\c")).is_err());
        assert!(config_dir_from(None, None, None).is_err(), "never use shared temp or current directory");
    }

    #[test]
    fn oversized_config_is_ignored_and_lists_are_capped() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-config-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let mut big = String::from("{\"last_dir\": \"x\", \"pad\": \"");
        big.push_str(&"a".repeat(MAX_CONFIG_BYTES as usize));
        big.push_str("\"}");
        std::fs::write(&p, &big).unwrap();
        assert_eq!(Config::load_from(&p).last_dir, "", "an oversized file is not parsed");
        let urls: Vec<String> = (0..100).map(|i| format!("https://e.com/{i}")).collect();
        std::fs::write(&p, json!({"urls": urls}).to_string()).unwrap();
        assert_eq!(Config::load_from(&p).urls.len(), MAX_HISTORY);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_gives_up_on_a_lock_held_by_a_stalled_peer() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-config-lockwait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let held = std::fs::OpenOptions::new()
            .create(true).truncate(false).read(true).write(true).open(p.with_extension("lock")).unwrap();
        FileExt::lock(&held).unwrap();
        let mut cfg = Config::load_from(&p);
        cfg.dark_mode = false;
        let started = std::time::Instant::now();
        cfg.save_to_within(&p, std::time::Duration::from_millis(200));
        // Returned instead of blocking the (GUI) thread behind the peer...
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(!p.exists(), "nothing written without the lock");
        // ...and saves normally once the peer lets go.
        FileExt::unlock(&held).unwrap();
        cfg.save_to_within(&p, std::time::Duration::from_millis(200));
        assert!(!Config::load_from(&p).dark_mode);
        drop(held);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dark_mode_defaults_true_and_persists() {
        let cfg = sanitize(&Value::Null);
        assert!(cfg.dark_mode);
        let cfg = sanitize(&json!({"dark_mode": false}));
        assert!(!cfg.dark_mode);

        let dir = std::env::temp_dir().join(format!("snatch-rs-config-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let mut cfg = Config::load_from(&p);
        cfg.dark_mode = false;
        cfg.save_to(&p);
        assert!(!Config::load_from(&p).dark_mode);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn credentials_are_dropped_from_remembered_urls() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-redact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let mut cfg = Config::load_from(&p);
        cfg.remember_url("https://user:token@host.tld/a.zip?x=1");
        cfg.remember_url("https://token@host.tld/");
        cfg.remember_url("https://host.tld/plain?a=b@c");
        cfg.remember_url("magnet:?xt=urn:btih:abc");
        assert_eq!(cfg.urls[3], "https://host.tld/a.zip?x=1");
        assert_eq!(cfg.urls[2], "https://host.tld/");
        assert_eq!(cfg.urls[1], "https://host.tld/plain?a=b@c");
        assert_eq!(cfg.urls[0], "magnet:?xt=urn:btih:abc");
        std::fs::remove_dir_all(&dir).ok();
    }

}
