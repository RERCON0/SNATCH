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
fn hidden_or_system(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    std::fs::metadata(p)
        .map(|m| m.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn hidden_or_system(_p: &Path) -> bool {
    false
}

pub fn safe_read_dirs(path: &Path) -> Vec<PathBuf> {
    if crate::engines::is_unc_path(path) {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = match std::fs::read_dir(path) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with('.') || n.starts_with('$'))
                    && !hidden_or_system(p)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by_cached_key(|p| {
        p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
    });
    out
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
}
