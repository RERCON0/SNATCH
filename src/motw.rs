//! Mark-of-the-Web for files SNATCH downloads itself.
//!
//! A browser stamps every download with an NTFS alternate data stream,
//! `Zone.Identifier`, and that stamp is what makes SmartScreen, Office
//! Protected View and the "this file came from the Internet" prompt appear.
//! aria2 writes plain files, so an `.exe`, `.msi`, `.iso`, `.zip` or `.docx`
//! fetched through SNATCH would open with none of those protections. This
//! module adds the stamp once a download has finished.
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Text of the stream: zone 3 is "Internet". `HostUrl` is the download URL
/// without credentials, query string or fragment (a signed link's query is a
/// secret and must not be copied into a file next to the download).
pub fn zone_identifier(source_url: &str) -> String {
    let mut text = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Ok(mut url) = reqwest::Url::parse(source_url) {
        if matches!(url.scheme(), "http" | "https" | "ftp") {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            text.push_str(&format!("HostUrl={url}\r\n"));
        }
    }
    text
}

/// Stamps `path`. Returns whether the stream was written: a volume without
/// alternate data streams (FAT, exFAT, many network shares) simply cannot
/// carry the mark, and that is not an error worth surfacing.
pub fn mark(path: &Path, source_url: &str) -> bool {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        std::fs::write(
            std::path::PathBuf::from(stream),
            zone_identifier(source_url),
        )
        .is_ok()
    }
    #[cfg(not(windows))]
    {
        let _ = (path, source_url);
        false
    }
}

/// How many auto-renamed siblings (`name.1.ext` .. `name.99.ext`) are looked for.
const MAX_RENAMED_SIBLINGS: u32 = 99;

/// `name` and every name aria2 may have given the file instead because
/// `name` was taken.
pub fn possible_names(name: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(name.to_string())
        .chain((1..=MAX_RENAMED_SIBLINGS).map(move |counter| auto_renamed(name, counter)))
}

/// aria2 never overwrites: a taken name becomes `name.1.ext`, `name.2.ext`...
/// (the counter goes before the LAST extension).
fn auto_renamed(name: &str, counter: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{}.{counter}{}", &name[..dot], &name[dot..]),
        _ => format!("{name}.{counter}"),
    }
}

/// Stamps what one finished download wrote into `dir`: the file called `name`,
/// or the auto-renamed sibling aria2 used because `name` was taken. Only a
/// file written during the run (modified at or after `started`) qualifies, so
/// a same-named file that was already there is never touched. Returns how many
/// files were stamped.
pub fn mark_new_files(dir: &Path, name: &str, source_url: &str, started: SystemTime) -> usize {
    // Clock granularity and a start time taken just before the process spawns.
    let floor = started
        .checked_sub(Duration::from_secs(2))
        .unwrap_or(started);
    possible_names(name)
        .map(|candidate| dir.join(candidate))
        .filter(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified().map(|modified| (meta.is_file(), modified)))
                .is_ok_and(|(is_file, modified)| is_file && modified >= floor)
        })
        .filter(|path| mark(path, source_url))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_names_the_zone_and_hides_secrets() {
        let text = zone_identifier("https://user:pw@cdn.example/dl/setup.exe?token=abc#frag");
        assert_eq!(
            text,
            "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://cdn.example/dl/setup.exe\r\n"
        );
        // A magnet or a local path has no host to record.
        assert_eq!(
            zone_identifier("magnet:?xt=urn:btih:abc"),
            "[ZoneTransfer]\r\nZoneId=3\r\n"
        );
    }

    #[test]
    fn auto_renamed_siblings_follow_aria2s_pattern() {
        assert_eq!(auto_renamed("setup.exe", 1), "setup.1.exe");
        assert_eq!(auto_renamed("a.tar.gz", 2), "a.tar.2.gz");
        assert_eq!(auto_renamed("README", 3), "README.3");
        assert_eq!(auto_renamed(".hidden", 1), ".hidden.1");
    }

    #[test]
    fn only_files_written_by_this_run_are_stamped() {
        let dir = std::env::temp_dir().join(format!("snatch-motw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("setup.exe");
        std::fs::write(&old, b"was already here").unwrap();
        // Make the pre-existing file clearly older than the run.
        let long_ago = SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let started = SystemTime::now();
        std::fs::write(dir.join("setup.1.exe"), b"fresh").unwrap();

        let stamped = mark_new_files(&dir, "setup.exe", "https://h/setup.exe", started);
        #[cfg(windows)]
        {
            assert_eq!(stamped, 1, "only the renamed, fresh file");
            let stream = std::fs::read_to_string(dir.join("setup.1.exe:Zone.Identifier")).unwrap();
            assert!(stream.contains("ZoneId=3"));
            assert!(std::fs::read_to_string(dir.join("setup.exe:Zone.Identifier")).is_err());
        }
        #[cfg(not(windows))]
        assert_eq!(stamped, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn a_download_renamed_past_nine_is_still_stamped() {
        let dir = std::env::temp_dir().join(format!("snatch-motw-many-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let started = SystemTime::now();
        // Eleven copies already there: aria2 picked `setup.11.exe`.
        std::fs::write(dir.join("setup.11.exe"), b"fresh").unwrap();
        let stamped = mark_new_files(&dir, "setup.exe", "https://h/setup.exe", started);
        assert_eq!(stamped, 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
