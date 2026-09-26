use std::path::{Path, PathBuf};

use crate::config::config_dir;
use crate::engines::expanduser;

#[derive(Clone)]
pub struct Toolchain {
    pub yt_dlp: Option<PathBuf>,
    pub aria2c: Option<PathBuf>,
}

fn env_override(name: &str) -> Option<&'static str> {
    match name {
        "yt-dlp" => Some("SNATCH_YT_DLP"),
        "aria2c" => Some("SNATCH_ARIA2C"),
        _ => None,
    }
}

pub fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn is_executable(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    }
    #[cfg(not(unix))]
    true
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(exe_name(name));
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

pub fn bootstrap_dir() -> PathBuf {
    config_dir().join("bin")
}

fn bootstrap_path(name: &str) -> PathBuf {
    bootstrap_dir().join(exe_name(name))
}

fn mtime(p: &Path) -> std::time::SystemTime {
    p.metadata()
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(p) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn winget_candidates(name: &str) -> Vec<PathBuf> {
    let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let root = Path::new(&local).join("Microsoft").join("WinGet");
    let prefix = if name == "yt-dlp" { "yt-dlp" } else { "aria2" };
    let mut out = Vec::new();

    if let Ok(entries) = std::fs::read_dir(root.join("Packages")) {
        let mut pkgs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(prefix))
            })
            .collect();
        pkgs.sort();
        for pkg in pkgs {
            let direct = pkg.join(exe_name(name));
            if is_executable(&direct) {
                out.push(direct);
            }
            if name == "aria2c" {
                for l1 in subdirs(&pkg) {
                    let c = l1.join("aria2c.exe");
                    if is_executable(&c) {
                        out.push(c);
                    }
                    for l2 in subdirs(&l1) {
                        let c = l2.join("aria2c.exe");
                        if is_executable(&c) {
                            out.push(c);
                        }
                    }
                }
            }
        }
    }

    let link = root.join("Links").join(exe_name(name));
    if is_executable(&link) {
        out.push(link);
    }
    out
}

pub fn find(name: &str) -> Option<PathBuf> {
    if let Some(key) = env_override(name) {
        if let Some(candidate) = std::env::var_os(key).filter(|s| !s.is_empty()) {
            let p = expanduser(Path::new(&candidate));
            if p.is_file() {
                return Some(p);
            }
            crate::errln(format!(
                "⚠ {key} указывает на несуществующий файл: {candidate:?} — продолжаю обычный поиск."
            ));
        }
    }

    let managed = bootstrap_path(name);
    if is_executable(&managed) {
        return Some(managed);
    }

    if let Some(p) = which(name) {
        return Some(p);
    }

    winget_candidates(name).into_iter().max_by_key(|p| mtime(p))
}

impl Toolchain {
    pub fn discover() -> Self {
        Self { yt_dlp: find("yt-dlp"), aria2c: find("aria2c") }
    }

    // Shared by both binaries. The GUI has its own proactive "не найдены
    // загрузчики" banner with an install button that fires before this is
    // ever reached in practice, but the CLI *routinely* hits this (e.g. only
    // aria2c is missing and the user picks that engine) - so the message
    // can't presuppose a GUI button the terminal doesn't have.
    pub fn require(&self, name: &str) -> Result<PathBuf, String> {
        let path = if name == "yt-dlp" { self.yt_dlp.clone() } else { self.aria2c.clone() };
        path.ok_or_else(|| {
            let pkg = if name == "aria2c" { "aria2.aria2" } else { "yt-dlp.yt-dlp" };
            let env_hint = env_override(name)
                .map(|e| format!(" или задайте путь через переменную окружения {e}"))
                .unwrap_or_default();
            format!("Не найден «{name}». Установите его (winget install {pkg}){env_hint}.")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `cargo test` runs tests in this file's process on parallel threads by
    // default. PATH/LOCALAPPDATA/SNATCH_* are process-global, so any test
    // that reads or writes them needs this held for its whole body - held
    // via `_guard`'s drop, not manual unlocking - or it can flip a sibling's
    // "not found"/"found" outcome depending on scheduling.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Snapshots the given env vars and restores them on drop - including on
    /// a failed `assert!` unwinding through the test, which plain
    /// restore-after-assert code does not survive (a panic skips everything
    /// after it in the same function, so the next test under ENV_LOCK would
    /// inherit this test's half-set PATH/LOCALAPPDATA).
    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl EnvRestore {
        fn capture(keys: &[&'static str]) -> Self {
            Self(keys.iter().map(|&k| (k, std::env::var_os(k))).collect())
        }
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (k, v) in self.0.drain(..) {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn require_raises_when_missing() {
        let tc = Toolchain { yt_dlp: None, aria2c: None };
        let err = tc.require("yt-dlp").unwrap_err();
        assert!(err.contains("winget install"));
        assert!(err.contains("SNATCH_YT_DLP"));
    }

    #[test]
    fn require_returns_path() {
        let tc = Toolchain { yt_dlp: Some(PathBuf::from("/usr/bin/yt-dlp")), aria2c: None };
        assert_eq!(tc.require("yt-dlp").unwrap(), PathBuf::from("/usr/bin/yt-dlp"));
    }

    #[test]
    fn which_finds_system_binary() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let name = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(which(name).is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    // Python's version also asserted the printed warning text via `capsys`;
    // Rust's std has no stdout/stderr capture for unit tests without an
    // extra crate, so this only re-checks the fallback return value.
    #[test]
    fn env_override_missing_file_falls_back_to_which() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::capture(&["PATH", "LOCALAPPDATA", "SNATCH_YT_DLP"]);
        let dir = std::env::temp_dir().join(format!("snatch-rs-tools-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real_exe = dir.join(exe_name("yt-dlp"));
        std::fs::write(&real_exe, b"").unwrap();
        std::env::set_var("PATH", &dir);
        // find() checks a self-installed %LOCALAPPDATA%\snatch\bin copy
        // before PATH (setup.rs's bootstrap install) - point it at the same
        // empty dir so that layer doesn't pre-empt the PATH fallback this
        // test is actually about.
        std::env::set_var("LOCALAPPDATA", &dir);
        std::env::set_var("SNATCH_YT_DLP", dir.join("gone.exe"));

        assert_eq!(find("yt-dlp"), Some(real_exe));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_found_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::capture(&["PATH", "LOCALAPPDATA", "SNATCH_YT_DLP", "SNATCH_ARIA2C"]);
        let dir = std::env::temp_dir().join(format!("snatch-rs-tools-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::remove_var("SNATCH_YT_DLP");
        std::env::remove_var("SNATCH_ARIA2C");
        std::env::set_var("PATH", &dir); // empty dir: `which` finds nothing
        std::env::set_var("LOCALAPPDATA", &dir); // no bootstrap bin/, no WinGet\Packages either

        assert_eq!(find("aria2c"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_override_used() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::capture(&["LOCALAPPDATA", "SNATCH_YT_DLP"]);
        let dir = std::env::temp_dir().join(format!("snatch-rs-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Same bootstrap-dir precedence note as above: without this, a real
        // self-installed yt-dlp on the machine running the test would win
        // over SNATCH_YT_DLP and break the first assertion below.
        std::env::set_var("LOCALAPPDATA", &dir);
        let exe = dir.join("my-yt-dlp.exe");
        std::fs::write(&exe, b"").unwrap();
        std::env::set_var("SNATCH_YT_DLP", &exe);
        assert_eq!(find("yt-dlp"), Some(exe.clone()));
        std::env::set_var("SNATCH_YT_DLP", dir.join("gone.exe"));
        // The old `assert_ne!(find(..), Some(gone))` passed for ANY outcome
        // (find can never return a non-existent file) - assert the actual
        // contract instead: the fallback is either nothing or a real file.
        let fallback = find("yt-dlp");
        assert!(fallback.is_none_or(|p| p.is_file() && p != dir.join("gone.exe")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
