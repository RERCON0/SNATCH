use std::path::{Path, PathBuf};

pub fn clip(text: &str, width: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= width {
        t.to_string()
    } else {
        let s: String = t.chars().take(width.saturating_sub(1)).collect();
        format!("{s}…")
    }
}

pub fn safe_read_dirs(path: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(path) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with('.') || n.starts_with('$'))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by_key(|p| {
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
