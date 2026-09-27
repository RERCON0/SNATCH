use std::path::{Path, PathBuf};

pub fn clip(text: &str, width: usize) -> String {
    // Labels may come from a hand-edited config or a remote filename. Never
    // pass C0/DEL controls (notably ESC/OSC) into terminal menus or widgets.
    let cleaned: String = text.trim().chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = cleaned.trim();
    if t.chars().count() <= width {
        t.to_string()
    } else {
        let s: String = t.chars().take(width.saturating_sub(1)).collect();
        format!("{s}…")
    }
}

/// Windows: FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM. Explorer hides
/// these (System Volume Information, Recovery, …); the folder browser
/// shouldn't show them either - picking one only produces a confusing
/// access-denied deep inside the download.
#[cfg(windows)]
fn meta_hidden_or_system(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    m.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

#[cfg(not(windows))]
fn meta_hidden_or_system(_m: &std::fs::Metadata) -> bool {
    false
}

/// Full directory listing. The CLI's `More(n)` pagination depends on the
/// real total, so this must never cap the scan.
pub fn safe_read_dirs(path: &Path) -> Vec<PathBuf> {
    safe_read_dirs_limited(path, usize::MAX).0
}

/// `safe_read_dirs` with a bounded scan: enumeration stops once
/// `examine_limit` directory entries have been examined, and the bool
/// reports whether more entries remained. The cap counts raw directory
/// entries, before filtering; filters and sorting are otherwise identical
/// to `safe_read_dirs`. The GUI uses this so a huge or slow directory can't
/// stall the UI thread for an unbounded time.
pub fn safe_read_dirs_limited(path: &Path, examine_limit: usize) -> (Vec<PathBuf>, bool) {
    if crate::engines::is_unc_path(path) {
        return (Vec::new(), false);
    }
    let mut truncated = false;
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for (examined, entry) in entries.flatten().enumerate() {
            if examined >= examine_limit {
                truncated = true;
                break;
            }
            // Name check first: `.hidden`/`$SYS` reject without any attribute
            // work. A non-UTF-8 name keeps the old behaviour (not filtered).
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with('.') || n.starts_with('$'))
            {
                continue;
            }
            // One DirEntry serves both remaining checks: on Windows
            // file_type() and metadata() come from the data cached by the
            // directory enumeration, so unlike the old Path::is_dir() +
            // fs::metadata() pair these add no extra path-based queries.
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            if !entry.metadata().map(|m| meta_hidden_or_system(&m)).unwrap_or(false) {
                out.push(entry.path());
            }
        }
    }
    out.sort_by_cached_key(|p| {
        p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
    });
    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_shortens_long_text() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 10), "abc");
        assert_eq!(clip("  spaced  ", 20), "spaced");
        assert_eq!(clip("name\x1b]0;fake title\x07.zip", 80), "name ]0;fake title .zip");
    }

    #[test]
    fn safe_read_dirs_filters_hidden_and_system() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("visible")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::create_dir_all(dir.join("$SYS")).unwrap();
        std::fs::write(dir.join("file.txt"), b"x").unwrap();
        let names: Vec<String> = safe_read_dirs(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["visible".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn safe_read_dirs_limited_caps_and_reports_truncation() {
        let dir = std::env::temp_dir().join(format!("snatch-rs-dirs-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for i in 0..10 {
            std::fs::create_dir_all(dir.join(format!("dir-{i:02}"))).unwrap();
        }

        // Below the cap: full list, same as the unbounded variant, no flag.
        let (all, truncated) = safe_read_dirs_limited(&dir, 100);
        assert_eq!(all.len(), 10);
        assert!(!truncated);
        assert_eq!(all, safe_read_dirs(&dir));

        // Exactly the entry count: everything was examined, nothing remained.
        let (exact, truncated) = safe_read_dirs_limited(&dir, 10);
        assert_eq!(exact.len(), 10);
        assert!(!truncated);

        // Past the cap: enumeration stops at the limit and the flag is set.
        let (capped, truncated) = safe_read_dirs_limited(&dir, 4);
        assert!(truncated);
        assert_eq!(capped.len(), 4);
        assert!(capped.iter().all(|p| all.contains(p)));

        // Zero limit on a non-empty directory lists nothing but reports it.
        let (none, truncated) = safe_read_dirs_limited(&dir, 0);
        assert!(none.is_empty());
        assert!(truncated);

        std::fs::remove_dir_all(&dir).ok();
    }
}
