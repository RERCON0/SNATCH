use crate::{tr, tr_format};
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
    let Ok(meta) = p.metadata() else { return false };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
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
        // A UNC PATH entry would SMB-authenticate on the is_executable()
        // probe below; skip it before touching the filesystem.
        if crate::engines::is_unc_path(&dir) {
            continue;
        }
        let cand = dir.join(exe_name(name));
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

pub fn bootstrap_dir() -> Result<PathBuf, String> {
    config_dir().map(|p| p.join("bin"))
}

fn bootstrap_path(name: &str) -> Result<PathBuf, String> {
    bootstrap_dir().map(|p| p.join(exe_name(name)))
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
    // Same UNC rule as PATH entries and the bootstrap dir (config_dir): a
    // share-valued %LOCALAPPDATA% is never probed.
    let Some(local) = crate::config::local_app_data() else {
        return Vec::new();
    };
    let root = local.join("Microsoft").join("WinGet");
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
            // A UNC candidate would SMB-authenticate to the remote host on
            // the is_file() probe below (NetNTLMv2 leak) - refuse it first.
            if crate::engines::is_unc_path(&p) {
                crate::errln(tr_format!(
                    "⚠ {key} points to a network UNC path: {candidate:?} — skipping.",
                    "⚠ {key} указывает на сетевой UNC-путь: {candidate:?} — пропускаю."
                ));
            } else if is_executable(&p) {
                return Some(p);
            } else {
                crate::errln(tr_format!("⚠ {key} points to a missing, empty or non-executable file: {candidate:?} — continuing discovery.", "⚠ {key} указывает на отсутствующий, пустой или неисполнимый файл: {candidate:?} — продолжаю обычный поиск."
                ));
            }
        }
    }

    if let Ok(managed) = bootstrap_path(name) {
        if is_executable(&managed) {
            return Some(managed);
        }
        // A crashed swap can leave only the recovery copy.
        if cfg!(windows) && name == "aria2c" && is_executable(&managed.with_extension("exe.old")) {
            return Some(managed);
        }
    }

    if let Some(p) = which(name) {
        return Some(p);
    }

    winget_candidates(name).into_iter().max_by_key(|p| mtime(p))
}

/// Shared with installers only during process creation, so an aria2 two-rename
/// swap cannot race a spawn. Running aria2 downloads may still be updated.
pub fn lock_for_spawn(program: &Path) -> Result<Option<std::fs::File>, String> {
    lock_for_spawn_within(program, std::time::Duration::from_secs(3))
}

/// GUI thread variant: never wait for an installer or sleep on the UI thread.
pub fn try_lock_for_spawn(program: &Path) -> Result<Option<std::fs::File>, String> {
    lock_for_spawn_within(program, std::time::Duration::ZERO)
}

fn lock_for_spawn_within(
    program: &Path,
    wait: std::time::Duration,
) -> Result<Option<std::fs::File>, String> {
    use fs4::FileExt;
    let Some(parent) = program.parent() else {
        return Ok(None);
    };
    let tool = match program.file_name().and_then(|n| n.to_str()) {
        Some("aria2c.exe") => "aria2c",
        Some("yt-dlp.exe") => "yt-dlp",
        _ => return Ok(None),
    };
    if crate::engines::is_unc_path(program) {
        return Err(tr!(
            "Network tool paths are not supported",
            "Сетевой путь загрузчика не поддерживается"
        )
        .into());
    }
    let path = parent.join(format!("{tool}.install.lock"));
    let managed = bootstrap_dir().is_ok_and(|dir| dir == parent);
    let recovering =
        tool == "aria2c" && !program.exists() && program.with_extension("exe.old").is_file();
    // Managed spawns create/open the stable inode atomically, including the
    // first-install and manually removed lock cases. External tools need no
    // writable adjacent file unless an installer already owns that lock.
    let lock = match crate::config::open_lock_file(&path, managed || recovering) {
        Ok(lock) => lock,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !managed && !recovering => {
            return Ok(None)
        }
        Err(e) => {
            return Err(tr_format!(
                "startup lock for {tool}: {e}",
                "блокировка запуска {tool}: {e}"
            ))
        }
    };
    let deadline = std::time::Instant::now() + wait;
    loop {
        match FileExt::try_lock(&lock) {
            Ok(()) => break,
            Err(fs4::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25))
            }
            Err(e) => {
                return Err(tr_format!(
                    "{tool} is being installed; retry after installation ({e})",
                    "{tool} устанавливается; повторите запуск после установки ({e})"
                ))
            }
        }
    }
    if tool == "aria2c" && !program.exists() {
        let old = program.with_extension("exe.old");
        if old.is_file() {
            std::fs::rename(old, program)
                .map_err(|e| tr_format!("recovering aria2c: {e}", "восстановление aria2c: {e}"))?;
        }
    }
    Ok(Some(lock))
}

impl Toolchain {
    pub fn discover() -> Self {
        Self {
            yt_dlp: find("yt-dlp"),
            aria2c: find("aria2c"),
        }
    }

    // Shared by both binaries. The GUI has its own proactive "не найдены
    // загрузчики" banner with an install button that fires before this is
    // ever reached in practice, but the CLI *routinely* hits this (e.g. only
    // aria2c is missing and the user picks that engine) - so the message
    // can't presuppose a GUI button the terminal doesn't have.
    pub fn require(&self, name: &str) -> Result<PathBuf, String> {
        let path = if name == "yt-dlp" {
            self.yt_dlp.clone()
        } else {
            self.aria2c.clone()
        };
        path.ok_or_else(|| {
            let pkg = if name == "aria2c" {
                "aria2.aria2"
            } else {
                "yt-dlp.yt-dlp"
            };
            let env_hint = env_override(name)
                .map(|e| {
                    tr_format!(
                        " or set its path with the {e} environment variable",
                        " или задайте путь через переменную окружения {e}"
                    )
                })
                .unwrap_or_default();
            tr_format!(
                "“{name}” not found. Install it (winget install {pkg}){env_hint}.",
                "Не найден «{name}». Установите его (winget install {pkg}){env_hint}."
            )
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
        let tc = Toolchain {
            yt_dlp: None,
            aria2c: None,
        };
        let err = tc.require("yt-dlp").unwrap_err();
        assert!(err.contains("winget install"));
        assert!(err.contains("SNATCH_YT_DLP"));
    }

    #[test]
    fn require_returns_path() {
        let tc = Toolchain {
            yt_dlp: Some(PathBuf::from("/usr/bin/yt-dlp")),
            aria2c: None,
        };
        assert_eq!(
            tc.require("yt-dlp").unwrap(),
            PathBuf::from("/usr/bin/yt-dlp")
        );
    }

    #[test]
    fn an_empty_executable_is_skipped_and_recovery_is_serialized_with_installers() {
        use fs4::FileExt;
        let dir = std::env::temp_dir().join(format!("snatch-tool-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("aria2c.exe");
        std::fs::write(&exe, b"").unwrap();
        assert!(!is_executable(&exe));
        std::fs::remove_file(&exe).unwrap();
        std::fs::write(exe.with_extension("exe.old"), b"previous build").unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("aria2c.install.lock"))
            .unwrap();
        FileExt::try_lock(&lock).unwrap();
        assert!(
            try_lock_for_spawn(&exe).is_err(),
            "installer must exclude recovery and spawn"
        );
        assert!(!exe.exists());
        drop(lock);
        let guard = try_lock_for_spawn(&exe).unwrap();
        assert_eq!(std::fs::read(exe).unwrap(), b"previous build");
        drop(guard);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_install_lock_does_not_skip_backup_recovery() {
        let dir = std::env::temp_dir().join(format!(
            "snatch-missing-install-lock-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("aria2c.exe");
        std::fs::write(exe.with_extension("exe.old"), b"previous build").unwrap();
        let guard = try_lock_for_spawn(&exe).unwrap();
        assert!(dir.join("aria2c.install.lock").is_file());
        assert_eq!(std::fs::read(exe).unwrap(), b"previous build");
        drop(guard);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_recovery_copy_is_preserved_and_can_be_restored_on_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir =
            std::env::temp_dir().join(format!("snatch-tool-locked-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("aria2c.exe");
        let old = exe.with_extension("exe.old");
        std::fs::write(&old, b"previous build").unwrap();
        std::fs::write(dir.join("aria2c.install.lock"), b"").unwrap();
        // Deny delete/rename while still permitting ordinary reads.
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&old)
            .unwrap();
        let error = try_lock_for_spawn(&exe).unwrap_err();
        assert!(error.contains("recovering aria2c"), "{error}");
        assert_eq!(std::fs::read(&old).unwrap(), b"previous build");
        assert!(!exe.exists());
        drop(reader);
        let guard = try_lock_for_spawn(&exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"previous build");
        drop(guard);
        std::fs::remove_dir_all(dir).ok();
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
        let dir =
            std::env::temp_dir().join(format!("snatch-rs-tools-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real_exe = dir.join(exe_name("yt-dlp"));
        std::fs::write(&real_exe, b"executable fixture").unwrap();
        #[cfg(unix)]
        {
            // `which` requires the executable bit on Unix; fs::write's 0o644
            // would make this test Windows-only despite the cfg(unix) branches.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
        let _restore =
            EnvRestore::capture(&["PATH", "LOCALAPPDATA", "SNATCH_YT_DLP", "SNATCH_ARIA2C"]);
        let dir =
            std::env::temp_dir().join(format!("snatch-rs-tools-empty-{}", std::process::id()));
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
        std::fs::write(&exe, b"executable fixture").unwrap();
        #[cfg(unix)]
        {
            // `find` requires the executable bit on Unix; fs::write's 0o644
            // would make this test fail on Linux/macOS.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
