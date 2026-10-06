use std::path::{Path, PathBuf};

/// Invisible format characters that can disguise text: bidi embeddings,
/// overrides and isolates (U+202E turns "Track \u{202E}3pm.exe" into what
/// reads as "Track exe.mp3"), directional marks, zero-width space / word
/// joiners, BOM and soft hyphen. `char::is_control` is category Cc only and
/// lets all of these through. ZWJ/ZWNJ (U+200C/U+200D) stay: emoji sequences
/// and several scripts need them, and they cannot reorder text.
pub fn is_disguising_format(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{180E}' | '\u{200B}' | '\u{200E}' | '\u{200F}'
        | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
        | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}'
        | '\u{1BCA0}'..='\u{1BCA3}' | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

pub fn clip(text: &str, width: usize) -> String {
    // Labels may come from a hand-edited config or a remote filename. Never
    // pass C0/DEL controls (notably ESC/OSC) into terminal menus or widgets,
    // nor invisible bidi/format characters that make a name read as another.
    let cleaned: String = text
        .trim()
        .chars()
        .filter(|c| !is_disguising_format(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
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
/// to `safe_read_dirs`. This caps entry count, not filesystem latency; GUI
/// callers run the checked variant on a worker thread.
pub fn safe_read_dirs_limited(path: &Path, examine_limit: usize) -> (Vec<PathBuf>, bool) {
    try_read_dirs_limited(path, examine_limit).unwrap_or_default()
}

pub fn try_read_dirs_limited(
    path: &Path,
    examine_limit: usize,
) -> std::io::Result<(Vec<PathBuf>, bool)> {
    if crate::engines::is_unc_path(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Сетевая UNC-папка не поддерживается",
        ));
    }
    let mut truncated = false;
    let mut out: Vec<PathBuf> = Vec::new();
    {
        let entries = std::fs::read_dir(path)?;
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
            if !entry
                .metadata()
                .map(|m| meta_hidden_or_system(&m))
                .unwrap_or(false)
            {
                out.push(entry.path());
            }
        }
    }
    out.sort_by_cached_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    Ok((out, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_listing_is_an_error_not_an_empty_directory() {
        let file = std::env::temp_dir().join(format!("snatch-not-a-dir-{}", std::process::id()));
        std::fs::write(&file, b"file").unwrap();
        assert!(try_read_dirs_limited(&file, 100).is_err());
        std::fs::remove_file(file).ok();
    }

    #[test]
    fn clip_shortens_long_text() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 10), "abc");
        assert_eq!(clip("  spaced  ", 20), "spaced");
        assert_eq!(
            clip("name\x1b]0;fake title\x07.zip", 80),
            "name ]0;fake title .zip"
        );
    }

    #[test]
    fn clip_drops_invisible_bidi_and_format_characters() {
        // U+202E would display "Track \u{202E}3pm.exe" as "Track exe.mp3".
        assert_eq!(clip("Track \u{202E}3pm.exe", 80), "Track 3pm.exe");
        for c in [
            '\u{200E}',
            '\u{200F}',
            '\u{2066}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{200B}',
            '\u{034F}',
            '\u{206A}',
            '\u{FFF9}',
            '\u{1BCA0}',
            '\u{E0020}',
        ] {
            assert_eq!(clip(&format!("a{c}b"), 80), "ab", "U+{:04X}", c as u32);
        }
        // ZWJ is part of emoji sequences and some scripts: kept.
        assert_eq!(clip("👨\u{200D}👩", 80), "👨\u{200D}👩");
        // Real RTL letters are text, not format characters.
        assert_eq!(clip("שלום", 80), "שלום");
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
