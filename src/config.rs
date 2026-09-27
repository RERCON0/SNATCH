use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use fs4::FileExt;

pub const MAX_HISTORY: usize = 15;

pub fn home_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

pub fn config_dir() -> PathBuf {
    let non_empty = |v: Option<std::ffi::OsString>| v.filter(|s| !s.is_empty());
    if let Some(base) = non_empty(std::env::var_os("LOCALAPPDATA")) {
        return Path::new(&base).join("snatch");
    }
    if let Some(base) = non_empty(std::env::var_os("XDG_CONFIG_HOME")) {
        return Path::new(&base).join("snatch");
    }
    home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config").join("snatch")
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
    history_cleared: bool,
}

fn str_field(obj: Option<&serde_json::Map<String, Value>>, key: &str) -> String {
    obj.and_then(|o| o.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn list_field(obj: Option<&serde_json::Map<String, Value>>, key: &str) -> Vec<String> {
    obj.and_then(|o| o.get(key))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
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
        history_cleared: false,
    }
}
fn dedup_front(list: Vec<String>, value: String) -> Vec<String> {
    let head = value.clone();
    let mut out = vec![value];
    out.extend(list.into_iter().filter(|u| *u != head));
    out.truncate(MAX_HISTORY);
    out
}

fn merge_history(local: &[String], disk: &[String], same_epoch: bool) -> Vec<String> {
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
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    pub fn load() -> Config {
        Self::load_from(&Self::path())
    }

    pub fn load_from(p: &Path) -> Config {
        let data = std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null);
        let mut cfg = sanitize(&data);
        if cfg.default_dir.is_empty() {
            cfg.default_dir = home_dir()
                .map(|h| h.join("Downloads").to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        cfg
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    pub fn save_to(&self, p: &Path) {
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Lock a stable sibling file, not config.json: the config itself
            // is atomically replaced on each save, changing the locked inode.
            // Hold this lock through reload, merge and rename. OS locks are
            // released on process exit, unlike create_new marker files.
            let lock_path = p.with_extension("lock");
            let lock = std::fs::OpenOptions::new()
                .create(true).read(true).write(true).open(lock_path)?;
            FileExt::lock(&lock)?;
            let disk = Self::load_from(p);
            let merged = if self.history_cleared {
                Config {
                    default_dir: self.default_dir.clone(), last_dir: self.last_dir.clone(),
                    urls: self.urls.clone(), dirs: self.dirs.clone(),
                    history_epoch: disk.history_epoch.saturating_add(1),
                    dark_mode: self.dark_mode,
                    history_cleared: false,
                }
            } else {
                let same_epoch = self.history_epoch == disk.history_epoch;
                Config {
                    default_dir: self.default_dir.clone(),
                    last_dir: self.last_dir.clone(),
                    urls: merge_history(&self.urls, &disk.urls, same_epoch),
                    dirs: merge_history(&self.dirs, &disk.dirs, same_epoch),
                    history_epoch: disk.history_epoch,
                    dark_mode: self.dark_mode,
                    history_cleared: false,
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
        if let Err(e) = result {
            crate::errln(format!("⚠ Не удалось сохранить настройки: {e}"));
        }
    }

    pub fn remember_url(&mut self, url: &str) {
        self.urls = dedup_front(std::mem::take(&mut self.urls), url.to_string());
    }

    pub fn remember_dir(&mut self, directory: &str) {
        let d = crate::engines::expanduser(Path::new(directory))
            .to_string_lossy()
            .into_owned();
        self.last_dir = d.clone();
        self.dirs = dedup_front(std::mem::take(&mut self.dirs), d);
    }

    pub fn clear_history(&mut self) {
        self.history_cleared = true;
        self.urls = Vec::new();
        self.dirs = Vec::new();
        self.last_dir = String::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
