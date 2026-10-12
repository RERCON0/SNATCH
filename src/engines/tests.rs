use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn tmp_out() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("snatch-rs-test-{}-{}", std::process::id(), n))
}

fn tc_both() -> Toolchain {
    Toolchain {
        yt_dlp: Some(PathBuf::from("yt-dlp")),
        aria2c: Some(PathBuf::from("aria2c")),
    }
}

fn tc_ytdlp() -> Toolchain {
    Toolchain {
        yt_dlp: Some(PathBuf::from("yt-dlp")),
        aria2c: None,
    }
}

fn tc_aria2() -> Toolchain {
    Toolchain {
        yt_dlp: None,
        aria2c: Some(PathBuf::from("aria2c")),
    }
}

const MAGNET_OK: &str = "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4";

#[test]
fn validate_accepts_supported() {
    assert_eq!(
        validate_url("https://example.com/v.mp4").unwrap(),
        "https://example.com/v.mp4"
    );
    assert_eq!(validate_url(&format!("  {MAGNET_OK} ")).unwrap(), MAGNET_OK);
    assert_eq!(
        validate_url("ftp://host/file.zip").unwrap(),
        "ftp://host/file.zip"
    );
}

#[test]
fn validate_accepts_magnet_with_tracker_and_name() {
    let magnet = format!("{MAGNET_OK}&tr=http%3A%2F%2Fbt.example.org%2Fann&dn=Some%20Name");
    assert_eq!(validate_url(&magnet).unwrap(), magnet);
}

#[test]
fn validate_rejects_corrupted_magnet() {
    let corrupted = "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4Btr=\
         http%3A%2F%2Fbt.example.org%2Fanndn=Name";
    assert!(validate_url(corrupted).is_err());
}

#[test]
fn validate_rejects_magnet_without_xt() {
    assert!(validate_url("magnet:?dn=NoHashHere").is_err());
}

#[test]
fn validate_magnet_xt_case_insensitive() {
    let m = "magnet:?XT=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4B";
    assert_eq!(validate_url(m).unwrap(), m);
}

#[test]
fn validate_magnet_percent_encoded_xt() {
    let m = "magnet:?xt=urn%3Abtih%3A73510898AF9039563184FAFE9CB0F186DE6AAA4B";
    assert!(validate_url(m).is_ok());
}

#[test]
fn validate_rejects_unsupported_schemes() {
    for bad in [
        "file:///etc/passwd",
        "ws://host/x",
        "sftp://host/x",
        "ftps://host/x",
    ] {
        assert!(validate_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn validate_rejects_option_like() {
    assert!(validate_url("-x").is_err());
    assert!(validate_url("   ").is_err());
    assert!(validate_url("\"\"").is_err());
}

#[test]
fn validate_rejects_control_chars() {
    assert!(validate_url("https://host.com/pa\nth").is_err());
    assert!(validate_url("https://host.com/pa\u{0}th").is_err());
}

#[test]
fn validate_torrent_file() {
    let dir = tmp_out();
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.torrent");
    std::fs::write(&f, b"d4:infod0:e").unwrap();
    assert_eq!(
        validate_url(f.to_str().unwrap()).unwrap(),
        f.to_str().unwrap()
    );
    let missing = dir.join("missing.torrent");
    assert!(validate_url(missing.to_str().unwrap()).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn validate_rejects_unc_paths() {
    // stat'ing these would leak the user's NetNTLMv2 hash to "evil.com".
    // Assert the *guard's* message, not just is_err(): a missing guard
    // also fails via the unreachable host, so is_err() alone could pass
    // while the SMB stat this test exists to prevent actually happened.
    for bad in [
        r"\\evil.com\share\movie.torrent",
        "//evil.com/share/movie.torrent",
        r"\/evil.com\share\movie.torrent",
        r"/\evil.com\share\movie.torrent",
        r"\\?\UNC\evil.com\share\movie.torrent",
        // NT prefix: Win32 passes "\??\" straight through, so this
        // reaches \\evil.com\share without a leading double separator.
        r"\??\UNC\evil.com\share\movie.torrent",
        "/??/UNC/evil.com/share/movie.torrent",
    ] {
        assert!(is_unc_path(Path::new(bad)), "{bad}");
        let err = validate_url(bad).unwrap_err();
        assert!(err.contains("UNC"), "{bad}: {err}");
    }
}

#[test]
fn ntfs_key_does_not_depend_on_final_sigma_context() {
    assert_eq!(ntfs_case_key("ΚΑΛΟΣ"), ntfs_case_key("καλοσ"));
    assert_eq!(ntfs_case_key("ΚΑΛΟΣ"), "καλοσ");
    #[cfg(windows)]
    {
        // An absolute directory avoids other parallel tests temporarily
        // changing the process-wide current working directory.
        let out = std::env::current_exe().unwrap();
        let out = out.parent().unwrap();
        assert_eq!(
            aria2_target_key(out, "ΚΑΛΟΣ"),
            aria2_target_key(out, "καλοσ")
        );
    }
}

#[cfg(windows)]
#[test]
fn ntfs_sigma_aliases_share_a_key_for_the_same_actual_file() {
    let dir = tmp_out();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ΚΑΛΟΣ.mp3"), b"same file").unwrap();
    for name in ["καλοσ.mp3", "ΚΑΛΟΣ.mp3"] {
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"same file");
        assert_eq!(
            aria2_target_key(&dir, name),
            aria2_target_key(&dir, "ΚΑΛΟΣ.mp3")
        );
    }
    if dir.join("καλος.mp3").exists() {
        assert_eq!(
            aria2_target_key(&dir, "καλος.mp3"),
            aria2_target_key(&dir, "ΚΑΛΟΣ.mp3")
        );
    } else {
        // This volume distinguishes final sigma. Do not merge two
        // different files' .origin records merely by linguistic folding.
        std::fs::write(dir.join("καλος.mp3"), b"different file").unwrap();
        assert_ne!(
            aria2_target_key(&dir, "καλος.mp3"),
            aria2_target_key(&dir, "ΚΑΛΟΣ.mp3")
        );
    }
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn preflight_rejects_unc_before_collision_probes() {
    for path in [r"\\127.0.0.1\snatch-test", r"\??\UNC\127.0.0.1\snatch-test"] {
        let job = Job {
            engine: Engine::Aria2,
            url: "https://example.test/video.mp4".into(),
            out_dir: path.into(),
            fmt: Format::Best,
            cookies_browser: None,
            download_name: None,
        };
        let warnings = preflight_warning_with_ffmpeg(&job, true, true);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("UNC"));
    }
}

#[test]
fn build_rejects_unc_out_dir() {
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://youtube.com/watch?v=1".into(),
        out_dir: PathBuf::from(r"\\evil.com\share"),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let err = build(&job, &tc_ytdlp()).unwrap_err();
    assert!(err.contains("UNC"), "{err}");
    let job = Job {
        out_dir: PathBuf::from("//evil.com/share"),
        ..job
    };
    let err = build(&job, &tc_ytdlp()).unwrap_err();
    assert!(err.contains("UNC"), "{err}");
}

#[test]
fn local_path_engine_detection_ignores_hash_and_supports_metalink() {
    // '#' is legal in local filenames; it must not truncate the suffix.
    assert_eq!(detect_engine("S01 #2.torrent"), Engine::Aria2);
    assert_eq!(detect_engine("movie.meta4"), Engine::Aria2);
    assert_eq!(
        detect_engine(r"C:\Torrents\[DL] Cuphead #1.torrent"),
        Engine::Aria2
    );
}

fn args_of(cmd: &[OsString]) -> Vec<String> {
    cmd.iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn aria2_never_resumes_a_foreign_same_name_file() {
    // E-1 (offensive audit): `-c` must not run on a same-name file that
    // has no `.aria2` control data of its own - that would silently
    // overwrite somebody else's file.
    let out = tmp_out();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("file.bin"), b"old data").unwrap();
    let job = Job {
        engine: Engine::Aria2,
        url: "https://host/file.bin".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let s = args_of(&build(&job, &tc_aria2()).unwrap());
    assert!(!s.contains(&"-c".to_string()), "{s:?}");
    assert!(
        s.contains(&"--auto-file-renaming=true".to_string()),
        "{s:?}"
    );
    assert!(s.contains(&"--max-tries=10".to_string()), "{s:?}");
    assert!(s.contains(&"--seed-time=0".to_string()), "{s:?}");
    assert_eq!(std::fs::read(out.join("file.bin")).unwrap(), b"old data");
    assert!(preflight_warning(&job)
        .iter()
        .any(|w| w.contains("will not be overwritten")));

    // A stray .aria2 is not proof of ownership: it could be another URL's.
    // aria2 auto-continues such a file+control pair EVEN WITHOUT -c
    // (audit F1), so the build must refuse rather than let two sources'
    // bytes mix - for the aria2 engine and the external-downloader branch.
    std::fs::write(out.join("file.bin.aria2"), b"ctl").unwrap();
    let err = build(&job, &tc_aria2()).unwrap_err();
    assert!(err.contains("control file"), "{err}");
    assert!(preflight_warning(&job)
        .iter()
        .any(|w| w.contains("will be stopped")));
    let as_ytdlp = Job {
        engine: Engine::YtDlp,
        ..job.clone()
    };
    assert!(build(&as_ytdlp, &tc_both()).is_err());
    std::fs::remove_file(out.join("file.bin.aria2")).unwrap();

    // yt-dlp's external-downloader branch gets the collision-safe args too.
    let s = args_of(&build(&as_ytdlp, &tc_both()).unwrap());
    let i = s
        .iter()
        .position(|a| a == "--external-downloader-args")
        .unwrap();
    let args = &s[i + 1];
    assert!(args.contains("--auto-file-renaming=true"), "{args}");
    assert!(!args.contains("-c "), "{args}");
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn aria2_no_collision_starts_without_continue_even_if_file_appears_after_build() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: "https://host/fresh.bin".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let s = args_of(&build(&job, &tc_aria2()).unwrap());
    assert!(!s.contains(&"-c".to_string()), "{s:?}");
    assert!(
        s.contains(&"--auto-file-renaming=true".to_string()),
        "{s:?}"
    );
    assert!(!preflight_warning(&job)
        .iter()
        .any(|w| w.contains("will not be overwritten")));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn percent_encoded_names_are_decoded_like_aria2() {
    // aria2 decodes the raw last segment once (`%20` -> space) but
    // re-encodes separators (uppercase) and, on Windows, illegal chars.
    assert_eq!(url_basename("https://h/b/My%20File.zip"), "My File.zip");
    assert_eq!(url_basename("https://h/b/a%2fb.bin"), "a%2Fb.bin");
    assert_eq!(url_basename("https://h/b/d%2520.bin"), "d%20.bin");
    assert_eq!(
        url_basename("https://h/b/..%2F..%2Fpwned.bin"),
        "..%2F..%2Fpwned.bin"
    );
    assert_eq!(url_basename("https://h/b/"), "");
    assert_eq!(url_basename("https://h/b/%2E%2E"), "");
    #[cfg(windows)]
    assert_eq!(url_basename("https://h/b/cv%3Aads.bin"), "cv%3Aads.bin");
}

#[test]
fn a_foreign_pair_under_an_encoded_name_is_caught_and_the_name_is_pinned() {
    let out = tmp_out();
    std::fs::create_dir_all(&out).unwrap();
    let url = "https://h/b/My%20File.zip";
    let name = "My File.zip";
    std::fs::write(out.join(name), b"foreign").unwrap();
    std::fs::write(out.join(format!("{name}.aria2")), b"ctl").unwrap();
    let job = Job {
        engine: Engine::Aria2,
        url: url.into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    assert_eq!(aria2_foreign_pair(&job, &out).as_deref(), Some(name));
    assert!(build(&job, &tc_aria2()).is_err());
    assert!(preflight_warning(&job)
        .iter()
        .any(|w| w.contains("will be stopped")));
    std::fs::remove_file(out.join(format!("{name}.aria2"))).unwrap();

    // Without the pair the download may start, but -o must pin the
    // decoded name so lock/origin records and the file on disk agree.
    let s = args_of(&build(&job, &tc_aria2()).unwrap());
    let i = s.iter().position(|a| a == "-o").expect("-o missing");
    assert_eq!(s[i + 1], name);
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn a_url_without_a_file_name_still_gets_a_pinned_aria2_target() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: "https://h/dir/".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    // Without -o, aria2 would follow the server's Content-Disposition
    // and none of the lock/origin/foreign-pair checks could run.
    let s = args_of(&build(&job, &tc_aria2()).unwrap());
    let i = s.iter().position(|a| a == "-o").expect("-o missing");
    assert!(
        s[i + 1].starts_with("snatch-") && s[i + 1].ends_with(".bin"),
        "{}",
        s[i + 1]
    );
    // Deterministic: the same URL always maps to the same target, so
    // retries and origin records agree.
    let again = args_of(&build(&job, &tc_aria2()).unwrap());
    assert_eq!(again[i + 1], s[i + 1]);

    // A URL that has a name keeps it unchanged.
    let named = Job {
        url: "https://h/My%20File.zip".into(),
        ..job
    };
    let s = args_of(&build(&named, &tc_aria2()).unwrap());
    let i = s.iter().position(|a| a == "-o").unwrap();
    assert_eq!(s[i + 1], "My File.zip");
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn an_encoded_url_resumes_only_its_own_decoded_partial() {
    let out = tmp_out();
    let state = std::env::temp_dir().join(format!("snatch-owned-enc-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let url = "https://h/b/My%20File.zip";
    let name = url_basename(url);
    let job = Job {
        engine: Engine::Aria2,
        url: url.into(),
        download_name: None,
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
    };
    std::fs::write(out.join(&name), b"part").unwrap();
    std::fs::write(out.join(format!("{name}.aria2")), b"ctl").unwrap();
    assert!(!aria2_owned_partial_in(&job, &out, &state));
    std::fs::write(aria2_origin_path(&out, &name, &state), aria2_url_hash(url)).unwrap();
    assert!(aria2_owned_partial_in(&job, &out, &state));
    assert!(!aria2_owned_partial_in(
        &Job {
            url: "https://h/b/Other%20File.zip".into(),
            ..job
        },
        &out,
        &state
    ));
    std::fs::remove_dir_all(&out).ok();
    std::fs::remove_dir_all(&state).ok();
}

#[test]
fn audio_artwork_defaults_allow_an_explicit_opt_out_and_leave_video_alone() {
    let out = tmp_out();
    let mut job = Job {
        engine: Engine::YtDlp,
        url: "https://example.test/watch?v=1".into(),
        out_dir: out.clone(),
        fmt: Format::Audio,
        cookies_browser: None,
        download_name: None,
    };
    let extras = RunExtras {
        extra_ytdlp: vec!["--no-embed-thumbnail".into()],
        ..RunExtras::default()
    };
    for fmt in [Format::Audio, Format::AudioSource] {
        job.fmt = fmt;
        let cmd = build_with(&job, &tc_both(), &extras).unwrap();
        let enabled = cmd.iter().position(|a| a == "--embed-thumbnail").unwrap();
        let disabled = cmd
            .iter()
            .position(|a| a == "--no-embed-thumbnail")
            .unwrap();
        assert!(enabled < disabled, "an explicit user option must win");
        assert_eq!(cmd.last().unwrap(), job.url.as_str());
        assert_eq!(cmd[cmd.len() - 2], "--");
    }
    job.fmt = Format::Best;
    let cmd = build(&job, &tc_both()).unwrap();
    assert!(!cmd
        .iter()
        .any(|a| a == "--embed-thumbnail" || a == "--embed-metadata"));
    std::fs::remove_dir_all(out).unwrap();
}

#[test]
fn torrent_title_uses_metadata_then_decoded_magnet_name() {
    let job = Job {
        engine: Engine::Aria2,
        url: "magnet:?xt=urn:btih:C575A7D7522DF7ADEAAFBFB2531264B2C8CF7067&dn=%5BDL%5D%20Hades%20II%20%28Portable%29".into(),
        out_dir: PathBuf::from(r"C:\Users\example\Desktop"),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let mut choice = crate::torrent::Choice::default();
    assert_eq!(download_name(&job.url, &choice), "[DL] Hades II (Portable)");
    choice.name = Some("\x1b[2JActual torrent name".into());
    assert_eq!(download_name(&job.url, &choice), "Actual torrent name");
    assert_eq!(
        aria2_metadata_name("FILE: [MEMORY][METADATA] Example torrent").as_deref(),
        Some("Example torrent")
    );
    assert!(aria2_metadata_name("FILE: C:/Users/example/Desktop (3more)").is_none());
    let name = batch_name("magnet:?xt=urn:btih:abc&dn=%1B%5B2JExample%0Atrack");
    assert_eq!(name, "Example track");
    assert!(!name.chars().any(char::is_control));
}

#[test]
fn formats_cover_caps_and_source_audio() {
    let keys: Vec<Format> = formats().iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, Format::ALL);
    // Every format round-trips through its command-line spelling.
    for format in Format::ALL {
        assert_eq!(Format::parse(format.as_str()), Some(format));
    }
    assert_eq!(Format::parse("4320p"), None);
    for engine in Engine::ALL {
        assert_eq!(Engine::parse(engine.as_str()), Some(engine));
        assert!(!engine.label().is_empty());
    }
    assert_eq!(Engine::parse("wget"), None);
    assert!(format_flags(Format::P720).contains(&"bv*[height<=720]+ba/b[height<=720]"));
    assert_eq!(
        format_flags(Format::AudioSource),
        vec!["-f", "ba/b", "-x", "--audio-format", "best"]
    );
    assert!(format_flags(Format::Audio).contains(&"mp3"));
    assert!(Format::Audio.is_audio() && Format::AudioSource.is_audio());
    assert!(!Format::Best.is_audio() && !Format::Subs.is_audio());
}

#[test]
fn source_presets_keep_all_general_formats_and_skip_unhelpful_instagram_and_audio_choices() {
    for url in [
        "https://www.instagram.com/reel/id/",
        "https://soundcloud.com/artist/track",
        "https://artist.bandcamp.com/track/title",
    ] {
        let keys: Vec<_> = formats_for_url(url)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, [Format::Best, Format::Audio, Format::AudioSource]);
        assert_eq!(ui_format_for_url(url, Format::P1080), Format::Best);
        assert_eq!(ui_format_for_url(url, Format::Subs), Format::Best);
        assert_eq!(
            ui_format_for_url(url, Format::AudioSource),
            Format::AudioSource
        );
        assert!(!crate::links::offers_subtitles(url));
    }
    for url in [
        "https://youtube.com/watch?v=1",
        "https://vimeo.com/123",
        "https://unknown.test/watch/1",
    ] {
        assert_eq!(formats_for_url(url), formats());
        assert_eq!(ui_format_for_url(url, Format::Subs), Format::Subs);
        assert!(crate::links::offers_subtitles(url));
    }
}

#[test]
fn resolved_filename_is_used_for_argv_lock_origin_and_foreign_partial_guards() {
    let out = tmp_out();
    let state = out.join("state");
    let job = Job {
        engine: Engine::Aria2,
        url: "https://host/download/stable/windows-x64-nsis".into(),
        download_name: crate::links::DownloadName::parse("opencode-desktop-win-x64.exe"),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
    };
    let name = job.download_name.as_ref().unwrap().as_str();
    let lock = lock_aria2_target_in(&job, false, &state).unwrap().unwrap();
    let args = args_of(&build(&job, &tc_aria2()).unwrap());
    let i = args.iter().position(|a| a == "-o").unwrap();
    assert_eq!(args[i + 1], name);
    assert_eq!(args.last().unwrap(), &job.url);
    assert_eq!(
        std::fs::read_to_string(aria2_origin_path(&out, name, &state)).unwrap(),
        aria2_url_hash(&job.url)
    );
    assert!(!aria2_origin_path(&out, "windows-x64-nsis", &state).exists());

    // Different URL basenames that resolve to the same name share a lock.
    let other = Job {
        url: "https://other.test/latest".into(),
        ..job.clone()
    };
    assert!(lock_aria2_target_in(&other, false, &state).is_err());
    std::fs::write(out.join(name), b"partial").unwrap();
    std::fs::write(out.join(format!("{name}.aria2")), b"control").unwrap();
    assert!(aria2_owned_partial_in(&job, &out, &state));
    assert!(!aria2_owned_partial_in(&other, &out, &state));
    assert_eq!(aria2_foreign_pair(&other, &out).as_deref(), Some(name));
    assert!(build(&other, &tc_aria2()).is_err());
    assert_eq!(std::fs::read(out.join(name)).unwrap(), b"partial");
    drop(lock);
    std::fs::remove_dir_all(out).unwrap();
}

#[test]
fn extra_args_split_like_a_simple_shell() {
    assert_eq!(
        split_cli_args("--limit-rate 1M --retry-wait=2").unwrap(),
        vec!["--limit-rate", "1M", "--retry-wait=2"]
    );
    assert_eq!(
        split_cli_args("--paths \"D:\\My Video\" ''").unwrap(),
        vec!["--paths", "D:\\My Video", ""]
    );
    assert_eq!(split_cli_args("").unwrap(), Vec::<String>::new());
    assert!(split_cli_args("\"unclosed").is_err());
}

#[test]
fn build_applies_run_extras() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://youtu.be/x".into(),
        out_dir: out.clone(),
        fmt: Format::P720,
        cookies_browser: None,
        download_name: None,
    };
    let extras = RunExtras {
        subs: true,
        sub_langs: "ru,en".into(),
        playlist: true,
        extra_ytdlp: vec!["--limit-rate".into(), "5M".into()],
        extra_aria2: vec![],
    };
    let s = args_of(&build_with(&job, &tc_ytdlp(), &extras).unwrap());
    assert!(s.contains(&"--yes-playlist".to_string()), "{s:?}");
    assert!(!s.contains(&"--no-playlist".to_string()));
    assert!(s.contains(&"--write-subs".to_string()));
    // Alongside-the-video subtitles never skip the media download.
    assert!(!s.contains(&"--skip-download".to_string()));
    let i = s.iter().position(|a| a == "--sub-langs").unwrap();
    assert_eq!(s[i + 1], "ru,en");
    assert!(s.windows(2).any(|w| w == ["--limit-rate", "5M"]));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn subs_format_downloads_subtitles_only_and_converts_to_srt() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://youtu.be/x".into(),
        out_dir: out.clone(),
        fmt: Format::Subs,
        cookies_browser: None,
        download_name: None,
    };
    // No --subs checkbox and no explicit languages: the subtitles-only
    // format itself must request subs, with the documented default.
    let s = args_of(&build_with(&job, &tc_ytdlp(), &RunExtras::default()).unwrap());
    for flag in [
        "--skip-download",
        "--write-subs",
        "--write-auto-subs",
        "--convert-subs",
    ] {
        assert!(s.contains(&flag.to_string()), "{flag} missing: {s:?}");
    }
    let i = s.iter().position(|a| a == "--sub-langs").unwrap();
    assert_eq!(s[i + 1], "ru,en");
    let i = s.iter().position(|a| a == "--convert-subs").unwrap();
    assert_eq!(s[i + 1], "srt");
    // No media format selection and no audio artwork postprocessors.
    assert!(!s.contains(&"-f".to_string()), "{s:?}");
    assert!(!s.contains(&"--embed-thumbnail".to_string()), "{s:?}");

    // An explicit language list still wins.
    let extras = RunExtras {
        sub_langs: "de".into(),
        ..RunExtras::default()
    };
    let s = args_of(&build_with(&job, &tc_ytdlp(), &extras).unwrap());
    let i = s.iter().position(|a| a == "--sub-langs").unwrap();
    assert_eq!(s[i + 1], "de");
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn quotes_do_not_leak_across_each_other() {
    assert_eq!(
        split_cli_args("--paths \"D:\\Bob's Files\"").unwrap(),
        vec!["--paths", "D:\\Bob's Files"]
    );
    assert_eq!(
        split_cli_args("'it \"quotes\" too'").unwrap(),
        vec!["it \"quotes\" too"]
    );
}

#[test]
fn aria2_extras_are_whitelisted_not_blacklisted() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: "https://h/f.bin".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    // aria2c resolves unambiguous prefixes, so abbreviations of
    // identity-changing flags must be refused alongside full names.
    for bad in [
        vec!["-o"],
        vec!["-oX"],
        vec!["--out=evil.bin"],
        vec!["--ou=evil.bin"],
        vec!["-d/tmp"],
        vec!["-c"],
        vec!["--conti=true"],
        vec!["--allow-overw=true"],
        vec!["--conf-path=x"],
        vec!["-i"],
        vec!["--input-file=x"],
        vec!["-T"],
        vec!["-M"],
        vec!["-Z"],
        vec!["--max-o=1"],
        vec!["--save-cookies=x"],
        vec!["https://evil.example/x"],
    ] {
        let extras = RunExtras {
            extra_aria2: bad.iter().map(|s| s.to_string()).collect(),
            ..RunExtras::default()
        };
        assert!(build_with(&job, &tc_aria2(), &extras).is_err(), "{bad:?}");
    }
    // `=`/attached and space-separated values are both accepted.
    for ok in [
        vec!["-x16"],
        vec!["-x", "16"],
        vec!["--max-tries=3"],
        vec!["--max-tries", "3"],
        vec!["--max-download-l=1M"],
        vec!["--user-agent=Foo"],
        vec!["--header", "A: b"],
        vec!["--referer=http://x"],
        vec!["--connect-timeout", "5"],
        vec!["--all-proxy=http://127.0.0.1:8080"],
    ] {
        let extras = RunExtras {
            extra_aria2: ok.iter().map(|s| s.to_string()).collect(),
            ..RunExtras::default()
        };
        assert!(build_with(&job, &tc_aria2(), &extras).is_ok(), "{ok:?}");
    }
    // A flag left without its value must say so, not blame the value.
    let extras = RunExtras {
        extra_aria2: vec!["--max-tries".into()],
        ..RunExtras::default()
    };
    let err = build_with(&job, &tc_aria2(), &extras).unwrap_err();
    assert!(err.contains("value"), "{err}");
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_places_extra_aria2_args_before_the_terminator() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: "https://h/f.bin".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let extras = RunExtras {
        extra_aria2: vec!["--max-connection-per-server=4".into()],
        ..RunExtras::default()
    };
    let s = args_of(&build_with(&job, &tc_aria2(), &extras).unwrap());
    let at = s
        .iter()
        .position(|a| a == "--max-connection-per-server=4")
        .unwrap();
    let term = s.iter().position(|a| a == "--").unwrap();
    assert!(at < term, "{s:?}");
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn aria2_lock_claims_only_an_empty_target_and_requires_the_same_source_to_resume() {
    let out = tmp_out();
    let state = out.join("state");
    let job = Job {
        engine: Engine::Aria2,
        url: "https://one.example/file.bin".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let lock = lock_aria2_target_in(&job, false, &state).unwrap().unwrap();
    assert!(
        lock_aria2_target_in(&job, false, &state).is_err(),
        "concurrent jobs must not share a basename"
    );
    std::fs::write(out.join("file.bin"), b"partial").unwrap();
    std::fs::write(out.join("file.bin.aria2"), b"control").unwrap();
    assert!(aria2_owned_partial_in(&job, &out, &state));
    assert!(aria2_flags(true).contains(&"-c"));
    drop(lock);

    let other = Job {
        url: "https://other.example/file.bin".into(),
        ..job.clone()
    };
    let lock = lock_aria2_target_in(&other, false, &state)
        .unwrap()
        .unwrap();
    assert!(!aria2_owned_partial_in(&other, &out, &state));
    assert!(!aria2_flags(false).contains(&"-c"));
    drop(lock);
    assert!(
        aria2_owned_partial_in(&job, &out, &state),
        "a foreign URL must not re-claim the existing partial"
    );
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn clean_url_strips_quotes() {
    assert_eq!(clean_url("  \"https://x\"  "), "https://x");
    assert_eq!(clean_url("'https://x'"), "https://x");
}

#[test]
fn detect_engine_basics() {
    assert_eq!(detect_engine("magnet:?xt=1"), Engine::Aria2);
    assert_eq!(
        detect_engine("https://youtube.com/watch?v=1"),
        Engine::YtDlp
    );
    assert_eq!(detect_engine("https://host.com/file.zip"), Engine::Aria2);
    assert_eq!(
        detect_engine("https://host.com/file.MP4?x=1"),
        Engine::Aria2
    );
    assert_eq!(detect_engine("https://host/x.deb"), Engine::Aria2);
}

#[test]
fn detect_engine_torrent_goes_to_aria2() {
    assert_eq!(detect_engine("C:\\torrents\\a.torrent"), Engine::Aria2);
    assert_eq!(detect_engine("a.torrent"), Engine::Aria2);
    assert_eq!(detect_engine("https://host/x.torrent"), Engine::Aria2);
    assert_eq!(detect_engine("https://host/x.meta4"), Engine::Aria2);
}

#[test]
fn detect_engine_malformed_url_no_crash() {
    assert_eq!(detect_engine("https://[::1"), Engine::YtDlp);
    assert_eq!(detect_engine("https://[bad"), Engine::YtDlp);
}

#[test]
fn validate_rejects_unparseable_url() {
    assert!(validate_url("https://[::1").is_err());
    assert!(validate_url("https://[bad").is_err());
    assert!(validate_url("https://[::1]/path").is_ok());
}

#[test]
fn is_direct_download_basics() {
    assert!(is_direct_download("https://host/file.zip"));
    assert!(is_direct_download("ftp://host/file.iso"));
    assert!(!is_direct_download("https://youtube.com/watch?v=1"));
    assert!(!is_direct_download("magnet:?xt=1"));
    assert!(!is_direct_download("https://host/page.html"));
}

#[test]
fn build_aria2() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: "magnet:?xt=1".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = build(&job, &tc_aria2()).unwrap();
    let s: Vec<String> = cmd
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    assert_eq!(s[1], "--no-conf");
    assert_eq!(
        s[s.iter().position(|x| x == "-d").unwrap() + 1],
        out.to_string_lossy()
    );
    assert_eq!(s[s.len() - 2], "--");
    assert_eq!(s[s.len() - 1], "magnet:?xt=1");
    assert!(s.contains(&"--max-tries=10".to_string()));
    // without it aria2c seeds until ratio 1.0 and never exits
    assert!(s.contains(&"--seed-time=0".to_string()));
    assert!(s.contains(&"--file-allocation=none".to_string()));
    assert!(s.contains(&"--auto-save-interval=1".to_string()));
    assert!(!s.iter().any(|a| a.starts_with("--allow-overwrite")));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn bittorrent_flags_apply_to_local_file_but_not_direct_http() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: r"C:\Torrents\[DL] Cuphead [RUS + ENG].torrent".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let bt = build(&job, &tc_aria2()).unwrap();
    assert!(bt.iter().any(|a| a == "--auto-save-interval=1"));
    let direct = Job {
        url: "https://host/file.zip".into(),
        ..job
    };
    let http = build(&direct, &tc_aria2()).unwrap();
    assert!(!http.iter().any(|a| a == "--auto-save-interval=1"));
    assert!(!http.iter().any(|a| a == "--file-allocation=none"));
    std::fs::remove_dir_all(out).ok();
}

#[test]
fn build_ytdlp_page_no_external_downloader() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://youtube.com/watch?v=1".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = build(&job, &tc_both()).unwrap();
    let s: Vec<String> = cmd
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    assert!(s.contains(&"--ignore-config".to_string()));
    assert!(s.contains(&"--no-playlist".to_string()));
    assert!(!s.contains(&"--external-downloader".to_string()));
    assert_eq!(s[s.len() - 2..], ["--", "https://youtube.com/watch?v=1"]);
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_ytdlp_direct_link_uses_external_downloader() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://host/file.zip".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = build(&job, &tc_both()).unwrap();
    let s: Vec<String> = cmd
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    let i = s.iter().position(|x| x == "--external-downloader").unwrap();
    assert_eq!(s[i + 1], "aria2c");
    assert!(s[i + 3].contains("--no-conf"));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_ytdlp_no_aria2_tool_no_external() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://host/file.zip".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = build(&job, &tc_ytdlp()).unwrap();
    let s: Vec<String> = cmd
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    assert!(!s.contains(&"--external-downloader".to_string()));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_out_dir_neutralized() {
    // A relative out_dir that's literally an option-shaped string ("-P")
    // must not reach yt-dlp as anything option-shaped: build() resolves
    // it to an absolute path before it's ever placed after `-P`. Exercising
    // the relative branch means changing cwd; no other test in this crate
    // reads current_dir(), and it's restored before the first assert (the
    // first thing that could panic), so this stays safe under `cargo test`'s
    // default parallel threads.
    let base = tmp_out();
    std::fs::create_dir_all(&base).unwrap();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&base).unwrap();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://x".into(),
        out_dir: PathBuf::from("-P"),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let result = build(&job, &tc_ytdlp());
    std::env::set_current_dir(&prev_cwd).unwrap();
    let cmd = result.unwrap();
    let s: Vec<String> = cmd
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    let value = &s[s.iter().position(|x| x == "-P").unwrap() + 1];
    assert!(!value.starts_with('-'), "{value:?}");
    assert!(Path::new(value).is_absolute(), "{value:?}");
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn build_missing_tool() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://x".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    assert!(build(&job, &tc_aria2()).is_err());
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_mkdir_failure() {
    // Make a *file* sit where a path component of out_dir needs to be a
    // directory, so create_dir_all fails for real (no mocking needed).
    let base = tmp_out();
    std::fs::create_dir_all(&base).unwrap();
    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://x".into(),
        out_dir: blocker.join("sub"),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    assert!(build(&job, &tc_ytdlp()).is_err());
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn preflight_aria2_with_cookies_drops_cookies_flag() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: "magnet:?xt=1".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: Some("chrome".into()),
        download_name: None,
    };
    assert!(preflight_warning(&job).iter().any(|w| w.contains("yt-dlp")));
    let cmd = build(&job, &tc_aria2()).unwrap();
    assert!(!cmd.iter().any(|a| a == "--cookies-from-browser"));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_format_flags() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://youtube.com/watch?v=1".into(),
        out_dir: out.clone(),
        fmt: Format::P1080,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = build(&job, &tc_ytdlp()).unwrap();
    let s: Vec<String> = cmd
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    assert!(s.contains(&"bv*[height<=1080]+ba/b[height<=1080]".to_string()));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_cookies_browser_valid() {
    let out = tmp_out();
    for value in [
        "chrome",
        "Chrome",
        "chrome:Profile 1",
        "firefox+keyring:prof::C:\\container",
    ] {
        let job = Job {
            engine: Engine::YtDlp,
            url: "https://youtube.com/watch?v=1".into(),
            out_dir: out.clone(),
            fmt: Format::Best,
            cookies_browser: Some(value.into()),
            download_name: None,
        };
        let cmd = build(&job, &tc_ytdlp()).unwrap();
        let s: Vec<String> = cmd
            .iter()
            .map(|x| x.to_string_lossy().into_owned())
            .collect();
        let i = s
            .iter()
            .position(|x| x == "--cookies-from-browser")
            .unwrap();
        assert_eq!(s[i + 1], value);
    }
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn build_rejects_bad_browser_name() {
    for bad in [
        "--exec=danger",
        "chrome;rm",
        "safari;rm -rf /",
        "",
        "nonsense",
        "somesite:prof",
        " chrome",
        "chrome:..",
        "chrome:../evil",
        "firefox:prof::..\\cnt",
        "chrome:pro\u{0}file",
        "chrome:a\nb",
    ] {
        assert!(validate_cookies_browser(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn preflight_warnings() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::Aria2,
        url: "magnet:?xt=1".into(),
        out_dir: out.clone(),
        fmt: Format::Audio,
        cookies_browser: Some("chrome".into()),
        download_name: None,
    };
    let warns = preflight_warning(&job);
    assert_eq!(warns.len(), 2);
    assert!(warns.iter().all(|w| w.contains("yt-dlp")));
    let job = Job {
        engine: Engine::Aria2,
        url: "magnet:?xt=1".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    assert!(preflight_warning(&job).is_empty());
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn yt_dlp_warns_about_missing_ffmpeg_for_audio_and_video() {
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://video.example/123".into(),
        out_dir: PathBuf::from("out"),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    assert!(preflight_warning_with_ffmpeg(&job, false, true)
        .iter()
        .any(|w| w.contains("merging")));
    assert!(preflight_warning_with_ffmpeg(&job, true, true).is_empty());
    assert!(preflight_warning_with_ffmpeg(&job, true, false)
        .iter()
        .any(|w| w.contains("Deno")));
    let audio = Job {
        fmt: Format::Audio,
        ..job.clone()
    };
    assert!(preflight_warning_with_ffmpeg(&audio, false, true)
        .iter()
        .any(|w| w.contains("mp3")));
    let subs = Job {
        fmt: Format::Subs,
        ..job
    };
    let warns = preflight_warning_with_ffmpeg(&subs, false, true);
    assert!(warns.iter().any(|w| w.contains(".srt")), "{warns:?}");
    // Subtitles-only downloads never mention video/audio merging.
    assert!(!warns.iter().any(|w| w.contains("merging")), "{warns:?}");
    assert!(preflight_warning_with_ffmpeg(&subs, true, true).is_empty());
}

#[test]
fn auth_hint_patterns() {
    for line in [
        "ERROR: [youtube] x: Sign in to confirm you're not a bot",
        "ERROR: [youtube] x: This video is age-restricted",
        "ERROR: Private video. Sign in",
        "WARNING: This video is members-only",
        "ERROR: Login required",
        "ERROR: unable to download video data: HTTP Error 403: Forbidden",
        "ERROR: requires authentication",
        "ERROR: [youtube] x: Join this channel to get access",
        // Rate limiting is treated as a cookie hint: the retry offer is
        // "try cookies from your browser", which is YouTube's own cure.
        "ERROR: Unable to download video subtitles for 'ru': HTTP Error 429: Too Many Requests",
        "ERROR: unable to download video data: HTTP Error 429: Too Many Requests",
        "09/27 03:10:34 [ERROR] CUID#7 - Download aborted. HTTP Error 403: Forbidden",
    ] {
        assert!(looks_like_auth(line), "{line}");
    }
    for line in [
        "ERROR: Unsupported URL: https://x",
        "ERROR: Video unavailable",
        "HTTP Error 404: Not Found",
        "unable to download webpage: timeout",
        // diagnostics context required: a bare filename/URI that happens
        // to contain a hint phrase must not flip the heuristic
        "CUID#7 - Download aborted. URI=`https://host/forbidden-songs.mp3'",
        "Sign in to confirm you're not a bot",
        // A file title containing the phrase, without the "ERROR:" /
        // "WARNING:" diagnostics context, must not arm the offer.
        "/out/How To Handle Too Many Requests.mp4 has already been downloaded",
        // A real 429 line, but a Destination path - the title happens to
        // say "Error 429"; only the loaders' own prefixes count.
        "[download] Destination: C:\\v\\How to Fix HTTP Error 429 - Too Many Requests.mp4",
    ] {
        assert!(!looks_like_auth(line), "{line}");
    }
}

#[test]
fn drm_failures_are_explained_but_never_arm_the_cookie_hint() {
    for line in [
        "ERROR: [soundcloud] 520518291: This video is DRM protected",
        "ERROR: This format is DRM protected; Try selecting another format with --format",
    ] {
        assert!(is_drm_protected(line), "{line}");
        // DRM is encryption, not a login wall: a cookies retry cannot
        // fix it, so it must not offer one.
        assert!(!looks_like_auth(line), "{line}");
    }
    for line in [
        // diagnostics context required, same as the auth hint
        "How to fix This video is DRM protected — tutorial",
        "[download] Destination: C:\\v\\This video is DRM protected.mp4",
        "ERROR: Video unavailable",
    ] {
        assert!(!is_drm_protected(line), "{line}");
    }
}

#[test]
fn parse_progress_ytdlp_and_aria2() {
    let close = |a: Option<f32>, b: f32| {
        assert!(a.is_some_and(|v| (v - b).abs() < 1e-4), "{a:?} != {b}");
    };
    close(
        parse_progress("[download]  42.3% of    9.99MiB at  1.2MiB/s ETA 00:05"),
        0.423,
    );
    close(
        parse_progress("[#50e13e 880KiB/0.9MiB(88%) CN:1 DL:812KiB]"),
        0.88,
    );
    close(parse_progress("[download] 100% of 1MiB"), 1.0);
    assert_eq!(parse_progress("[download] Destination: file"), None);
    assert_eq!(parse_progress("ERROR: Unsupported URL"), None);
    // a percentage inside a title/path is not progress
    assert_eq!(
        parse_progress("[download] Destination: C:\\v\\Progress (100%).mp4"),
        None
    );
    let title_with_aria2_marker =
        "[download] Destination: C:\\v\\Clip [#abc123 1MiB/2MiB(88%)].mp4";
    assert_eq!(parse_progress(title_with_aria2_marker), None);
    assert_eq!(aria2_stat(title_with_aria2_marker), None);
    assert_eq!(
        parse_progress("  [#50e13e 880KiB/0.9MiB(88%) CN:1]"),
        Some(0.88)
    );
    assert_eq!(parse_progress("[#50e13e 880KiB/0.9MiB(oops%) CN:1]"), None);
    assert_eq!(aria2_stat("prefix [#50e13e 880KiB/0.9MiB(88%) CN:1]"), None);
    assert_eq!(parse_progress("[Merger] Merging (50%) something"), None);
}

#[test]
fn aria2_status_distinguishes_allocation_from_download() {
    let allocating =
        "[#bc1f6e 0B/6.8GiB(0%) CN:0 SD:0 DL:0B] [FileAlloc:#bc1f6e 1.1GiB/3.3GiB(33%)]";
    assert_eq!(
        aria2_stat(allocating).as_deref(),
        Some("0% · 0B/6.8GiB · allocating space 1.1GiB/3.3GiB (33%)")
    );
    let downloading = "[#25017a 361MiB/6.8GiB(5%) CN:30 SD:5 DL:4.1MiB ETA:26m46s]";
    assert_eq!(
        aria2_stat(downloading).as_deref(),
        Some("5% · 361MiB/6.8GiB · ↓4.1MiB/s · seeds 5 · connections 30")
    );
    assert_eq!(
        aria2_name_from_file("FILE: C:/Downloads/Cuphead_1.3.9/setup.bin (7more)").as_deref(),
        Some("Cuphead_1.3.9")
    );
}

#[test]
fn aria2_missing_control_error_is_specific_not_every_code_13() {
    let detail = "Exception: [RequestGroup.cc:436] errorCode=13 File C:/Downloads/Cuphead exists, but a control file(*.aria2) does not exist.";
    assert!(aria2_missing_control(detail));
    assert!(!aria2_missing_control("errorCode=13 permission denied"));
    assert!(!aria2_missing_control(
        "control file(*.aria2) does not exist"
    ));
}

#[test]
fn detect_engine_percent_encoded_suffix() {
    assert_eq!(detect_engine("https://host/file%2Ezip"), Engine::Aria2);
    assert!(is_direct_download("https://host/file%2Emp4"));
    assert_eq!(detect_engine("https://host/page%2Ehtml"), Engine::YtDlp);
}

#[test]
fn decode_child_bytes_utf8_passthrough() {
    assert_eq!(
        decode_child_bytes(b"plain ascii\n").as_ref(),
        "plain ascii\n"
    );
    assert_eq!(
        decode_child_bytes("UTF-8 строка — ok".as_bytes()).as_ref(),
        "UTF-8 строка — ok"
    );
}

#[test]
fn decode_codepage_cp1251_table() {
    // "Привет — мир" as the frozen yt-dlp.exe writes it on a cp1251
    // system: Cyrillic in 0xC0-0xFF, em-dash 0x97, curly quote 0x92.
    let cp1251: &[u8] = b"\xCF\xf0\xe8\xe2\xe5\xf2 \x97 \xec\xe8\xf0\x92";
    assert_eq!(decode_codepage(cp1251, 1251), "Привет — мир\u{2019}");
    // Ё/ё live outside the contiguous Cyrillic block
    assert_eq!(decode_codepage(b"\xa8\xb8", 1251), "\u{0401}\u{0451}");
    // undefined 0x98 decodes to the replacement char, not a panic
    assert_eq!(decode_codepage(b"\x98", 1251), "\u{FFFD}");
    // 0xA0-0xBF is not Latin-1 in cp1251: 0xB3="і", 0xBF="ї", 0xB9="№".
    assert_eq!(decode_codepage(b"\xb3\xc0", 1251), "\u{0456}\u{0410}");
    assert_eq!(decode_codepage(b"\xb9", 1251), "\u{2116}");
    assert_eq!(decode_codepage(b"\xbf", 1251), "\u{0457}");
}

#[cfg(windows)]
#[test]
fn decode_codepage_uses_the_os_for_other_ansi_pages() {
    // An English or German Windows writes accents in cp1252; reading them
    // as cp1251 turned "café" into "cafй".
    assert_eq!(
        decode_codepage(b"caf\xe9 \x97 na\xefve", 1252),
        "café — naïve"
    );
    assert_eq!(decode_codepage(b"", 1252), "");
}

#[test]
fn options_go_before_the_terminator_and_the_input_stays_after_it() {
    let mut cmd: Vec<OsString> = ["aria2c", "-d", "out", "--", "https://h/f.bin"]
        .iter()
        .map(OsString::from)
        .collect();
    insert_option(&mut cmd, "--flag");
    insert_option(&mut cmd, String::from("--other=1"));
    set_input(&mut cmd, OsString::from("input.torrent"));
    assert_eq!(
        args_of(&cmd),
        [
            "aria2c",
            "-d",
            "out",
            "--flag",
            "--other=1",
            "--",
            "input.torrent"
        ]
    );
}

#[test]
fn a_cookies_profile_cannot_be_a_network_share() {
    for bad in [
        r"chrome:\\evil\share\Profile",
        "chrome:\\\\evil/share",
        "firefox:\\??\\UNC\\evil\\share",
    ] {
        let err = validate_cookies_browser(bad).unwrap_err();
        assert!(err.contains("UNC"), "{bad}: {err}");
    }
    // A drive path and an ordinary profile name are fine.
    assert!(validate_cookies_browser(r"chrome:C:\Users\me\Profile 1").is_ok());
    assert!(validate_cookies_browser("firefox:default-release").is_ok());
    assert!(validate_cookies_browser("chrome").is_ok());
}

#[test]
fn stale_aria2_records_are_swept_but_held_locks_and_fresh_files_stay() {
    let dir = tmp_out();
    std::fs::create_dir_all(&dir).unwrap();
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(200 * 24 * 3600);
    let write = |name: &str, old: bool| {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        if old {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        path
    };
    let old_origin = write("a.origin", true);
    let old_free_lock = write("b.lock", true);
    let old_held_lock = write("c.lock", true);
    let fresh_origin = write("d.origin", false);
    let unrelated = write("e.txt", true);
    let held = crate::config::open_lock_file(&old_held_lock, false).unwrap();
    FileExt::try_lock(&held).unwrap();

    sweep_aria2_state(&dir, ARIA2_STATE_MAX_AGE);

    assert!(!old_origin.exists(), "an idle origin record goes");
    assert!(!old_free_lock.exists(), "an idle, unheld lock goes");
    assert!(old_held_lock.exists(), "a lock a download holds stays");
    assert!(fresh_origin.exists(), "a recent record stays");
    assert!(unrelated.exists(), "only .origin and .lock files are ours");
    drop(held);
    std::fs::remove_dir_all(&dir).ok();
}

const SIGNED_CDN_LINK: &str = "https://scontent-waw2-1.cdninstagram.com/o1/v/t16/f2/m84/AQMT-W8Y3.mp4?_nc_cat=108&oh=00_AQOZ&oe=6ACDA1C4";

#[test]
fn a_meta_cdn_file_goes_to_yt_dlp_without_the_aria2_downloader() {
    if !cfg!(windows) {
        return;
    }
    // aria2c 1.37 for Windows cannot finish a TLS handshake with these hosts.
    assert_eq!(detect_engine(SIGNED_CDN_LINK), Engine::YtDlp);
    assert_eq!(
        detect_engine("https://www.threads.com/share/H9gVJw3xc/"),
        Engine::YtDlp
    );
    // A torrent stays with aria2, and other hosts are unaffected.
    assert_eq!(
        detect_engine("https://www.facebook.com/x.torrent"),
        Engine::Aria2
    );
    assert_eq!(detect_engine("https://example.com/file.mp4"), Engine::Aria2);

    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: SIGNED_CDN_LINK.into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = args_of(&build(&job, &tc_both()).unwrap());
    assert!(!cmd.iter().any(|a| a == "--external-downloader"), "{cmd:?}");
    // The file is named after the URL's own file name, never its query.
    let at = cmd.iter().position(|a| a == "-o").expect("-o pinned");
    assert_eq!(cmd[at + 1], "AQMT-W8Y3.mp4");
    // Another host keeps the aria2 downloader for a direct link.
    let other = Job {
        url: "https://example.com/file.mp4".into(),
        ..job
    };
    let cmd = args_of(&build(&other, &tc_both()).unwrap());
    assert!(cmd.iter().any(|a| a == "--external-downloader"), "{cmd:?}");
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn long_direct_names_keep_their_extension_and_percent_signs_are_escaped() {
    let long = format!("{}.mp4", "a".repeat(400));
    let limited = limit_name(&long, 150);
    assert_eq!(limited.chars().count(), 150);
    assert!(limited.ends_with(".mp4"));
    assert_eq!(limit_name("short.mp4", 150), "short.mp4");
    assert_eq!(limit_name(&"b".repeat(200), 10), "b".repeat(10));
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://example.com/100%25%20sure.mp4".into(),
        out_dir: PathBuf::from("."),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    // The file name "100% sure.mp4" must reach yt-dlp's -o template with the
    // percent sign doubled, or it would be read as a field.
    assert_eq!(ytdlp_direct_output(&job).as_deref(), Some("100%% sure.mp4"));
    let page = Job {
        url: "https://example.com/watch?v=1".into(),
        ..job
    };
    assert_eq!(ytdlp_direct_output(&page), None);
}

#[test]
fn a_direct_link_downloaded_by_yt_dlp_gets_the_same_internet_mark_as_aria2() {
    let out = tmp_out();
    std::fs::create_dir_all(&out).unwrap();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://example.com/download/My%20File.zip".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    // Exactly the name build_with pins with -o, minus the template escaping.
    let pinned = ytdlp_direct_output(&job).unwrap().replace("%%", "%");
    assert_eq!(pinned, "My File.zip");
    std::fs::write(out.join(&pinned), b"fresh").unwrap();
    mark_direct_download(&job, std::time::SystemTime::now());
    #[cfg(windows)]
    {
        let stream =
            std::fs::read_to_string(out.join(format!("{pinned}:Zone.Identifier"))).unwrap();
        assert!(stream.contains("ZoneId=3"), "{stream}");
        assert!(
            stream.contains("HostUrl=https://example.com/download/My%20File.zip"),
            "{stream}"
        );
    }

    // A yt-dlp media page keeps yt-dlp's own (title-based) file name, so there
    // is no name to stamp under.
    let page = Job {
        url: "https://example.com/watch?v=1".into(),
        ..job.clone()
    };
    std::fs::write(out.join("Title [id].mp4"), b"fresh").unwrap();
    mark_direct_download(&page, std::time::SystemTime::now());
    #[cfg(windows)]
    assert!(std::fs::read_to_string(out.join("Title [id].mp4:Zone.Identifier")).is_err());

    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn yt_dlp_only_ever_loads_snatchs_own_plugin_folder() {
    let out = tmp_out();
    let job = Job {
        engine: Engine::YtDlp,
        url: "https://youtu.be/x".into(),
        out_dir: out.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = args_of(&build(&job, &tc_ytdlp()).unwrap());
    let clear = cmd
        .iter()
        .position(|a| a == "--no-plugin-dirs")
        .expect("defaults off");
    if let Some(add) = cmd.iter().position(|a| a == "--plugin-dirs") {
        assert!(
            add > clear,
            "the folder is added after the defaults are cleared"
        );
    }
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn the_bundled_plugin_is_written_once_and_refreshed_when_it_changes() {
    let root = tmp_out();
    let file = ytdlp_plugin_file(&root);
    install_ytdlp_plugin(&root).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), THREADS_PLUGIN);
    // The layout yt-dlp's --plugin-dirs expects.
    assert!(file.ends_with("snatch/yt_dlp_plugins/extractor/snatch_threads.py"));
    // Identical content is not rewritten.
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    install_ytdlp_plugin(&root).unwrap();
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
    // An older copy (a previous SNATCH) is replaced, with no temp file left.
    std::fs::write(&file, "# old plugin").unwrap();
    install_ytdlp_plugin(&root).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), THREADS_PLUGIN);
    let leftovers = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
        .count();
    assert_eq!(leftovers, 0);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn aria2s_tls_failure_line_is_recognised() {
    assert!(looks_like_aria2_tls_failure(
        "  -> [SocketCore.cc:1019] errorCode=1 SSL/TLS handshake failure: Error: x"
    ));
    assert!(!looks_like_aria2_tls_failure(
        "errorCode=3 Resource not found"
    ));
}

#[test]
fn a_host_that_turns_aria2_away_with_403_or_429_gets_one_try_with_yt_dlp() {
    // The line aria2 prints for Wikimedia's answer to its TLS fingerprint.
    for status in ["429", "403"] {
        assert!(looks_like_aria2_refusal(&format!(
            "  -> [HttpSkipResponseCommand.cc:239] errorCode=22 The response status is not successful. status={status}"
        )));
    }
    assert!(looks_like_aria2_refusal(
        "  -> [SocketCore.cc:1019] errorCode=1 SSL/TLS handshake failure: Error: x"
    ));
    // Other statuses are real answers about the file, not a refusal of aria2.
    for status in ["404", "410", "500", "503"] {
        assert!(!looks_like_aria2_refusal(&format!(
            "  -> [HttpSkipResponseCommand.cc:239] errorCode=22 The response status is not successful. status={status}"
        )));
    }
    // A file or page name that merely contains the text is not aria2's report.
    assert!(!looks_like_aria2_refusal(
        "[download] Destination: status=429 notes.txt"
    ));
}

#[test]
fn only_resume_corruption_arms_the_from_scratch_restart() {
    for line in [
        "ERROR: Postprocessing: Error opening input files: Invalid data found when processing input",
        "ERROR: unable to open for writing: [Errno 22] Invalid argument: 'x.part'",
        "ERROR: unable to download video data: HTTP Error 416: Requested Range Not Satisfiable",
    ] {
        assert!(looks_like_corrupt_partial(line), "{line}");
    }
    for line in [
        "ERROR: unable to download video data: <urlopen error [Errno 11001] getaddrinfo failed>",
        "ERROR: [youtube] x: Connection reset by peer",
        "ERROR: No space left on device",
        // Remote text (a title) must not arm it: no diagnostics prefix.
        "[download] Destination: How to fix Invalid data found.mp4",
    ] {
        assert!(!looks_like_corrupt_partial(line), "{line}");
    }
}

#[test]
fn capped_lines_never_exceed_the_limit_and_keep_the_stream_in_sync() {
    let data = format!("short\n{}\nnext\nlast-without-newline", "x".repeat(50));
    // A 4-byte buffer forces every line across several fill_buf calls.
    let mut reader = std::io::BufReader::with_capacity(4, data.as_bytes());
    let mut buf = Vec::new();
    let mut lines = Vec::new();
    loop {
        buf.clear();
        if read_line_capped(&mut reader, &mut buf, 10).unwrap() == 0 {
            break;
        }
        lines.push(String::from_utf8(buf.clone()).unwrap());
    }
    assert_eq!(lines, ["short\n", "xxxxxxxxxx", "next\n", "last-witho"]);
}

#[test]
fn decode_child_bytes_never_loses_a_non_utf8_line() {
    // Whatever this machine's ANSI page is, valid UTF-8 never takes the
    // fallback and invalid UTF-8 never panics or comes back empty.
    assert!(!decode_child_bytes(b"\xb3\xc0").is_empty());
    assert_eq!(decode_child_bytes(b"ok").as_ref(), "ok");
}

#[test]
fn batch_name_uses_query_id_for_generic_route_segments() {
    assert_eq!(
        batch_name("https://youtube.com/watch?v=dQw4w9WgXcQ"),
        "dQw4w9WgXcQ"
    );
    assert_eq!(batch_name("https://youtu.be/dQw4w9WgXcQ"), "dQw4w9WgXcQ");
    assert_eq!(batch_name("https://example.com/embed?id=abc123"), "abc123");
    assert_eq!(
        batch_name("https://example.com/some-video-title"),
        "some-video-title"
    );
    assert_eq!(batch_name("https://example.com/watch"), "watch");
}

#[test]
fn batch_name_still_handles_magnets() {
    let name = batch_name("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Foo");
    assert_eq!(name, "Foo");
    assert_eq!(
        batch_name("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"),
        "magnet 0123456789a…"
    );
}

#[test]
fn ytdlp_title_from_line_reads_destination_and_merger_lines() {
    assert_eq!(
        ytdlp_title_from_line(
            "[download] Destination: C:\\out\\Rick Astley - Never Gonna Give You Up.mp4"
        ),
        Some("Rick Astley - Never Gonna Give You Up".to_string())
    );
    assert_eq!(
        ytdlp_title_from_line("[Merger] Merging formats into \"Some Title.mkv\""),
        Some("Some Title".to_string())
    );
    assert_eq!(
        ytdlp_title_from_line("/out/Already Downloaded.mp4 has already been downloaded"),
        None
    );
    assert_eq!(
        ytdlp_title_from_line("[download] /out/Already Downloaded.mp4 has already been downloaded"),
        Some("Already Downloaded".to_string())
    );
    assert_eq!(
        ytdlp_title_from_line(
            "[info] Writing video subtitles to: C:\\out\\Rick Astley - Never Gonna Give You Up [dQw4w9WgXcQ].en.vtt"
        ),
        Some("Rick Astley - Never Gonna Give You Up [dQw4w9WgXcQ]".to_string())
    );
    assert_eq!(
        ytdlp_title_from_line("[info] Writing video subtitles to: /out/Клип.zh-Hans.srt"),
        Some("Клип".to_string())
    );
    // The real subtitle download repeats it as a generic "Destination:"
    // line - same stripping there; a media file whose stem merely looks
    // like a language tag must stay intact.
    assert_eq!(
        ytdlp_title_from_line(
            "[download] Destination: C:\\out\\Rick Astley - Never Gonna Give You Up [dQw4w9WgXcQ].en.vtt"
        ),
        Some("Rick Astley - Never Gonna Give You Up [dQw4w9WgXcQ]".to_string())
    );
    assert_eq!(
        ytdlp_title_from_line("[download] Destination: /out/Interview.en.mp4"),
        Some("Interview.en".to_string())
    );
    assert_eq!(ytdlp_title_from_line("[download]  42.0% of 10.00MiB"), None);
    assert_eq!(ytdlp_title_from_line("some unrelated line"), None);
}

#[test]
fn subtitle_language_suffixes_are_stripped_but_real_dots_are_kept() {
    assert_eq!(strip_subtitle_language("Title [id].en"), "Title [id]");
    assert_eq!(strip_subtitle_language("Title [id].ru"), "Title [id]");
    assert_eq!(strip_subtitle_language("Title [id].pt-BR"), "Title [id]");
    assert_eq!(strip_subtitle_language("Title [id].zh-Hans"), "Title [id]");
    assert_eq!(strip_subtitle_language("Title [id].es-419"), "Title [id]");
    // Not language-shaped: kept whole.
    assert_eq!(strip_subtitle_language("Title [id]"), "Title [id]");
    assert_eq!(strip_subtitle_language("Title [id].mp4"), "Title [id].mp4");
    assert_eq!(strip_subtitle_language("Версия 2.0"), "Версия 2.0");
}

#[test]
fn subtitle_watch_separates_written_from_no_subtitles() {
    let watch = SubtitleWatch::default();
    watch.observe("[info] There are no subtitles for the requested languages");
    assert!(watch.missing() && !watch.written());
    assert!(watch.nothing_written(Format::Subs, 0));
    // Only the subtitles-only format and a clean exit turn this into a
    // failure; a failed run already reports its own error.
    assert!(!watch.nothing_written(Format::Best, 0));
    assert!(!watch.nothing_written(Format::Subs, 1));

    // A written (or already present) subtitle file is the success shape.
    let watch = SubtitleWatch::default();
    watch.observe("[info] Writing video subtitles to: C:\\out\\Clip [id].en.vtt");
    assert!(watch.written() && !watch.missing());
    assert!(!watch.nothing_written(Format::Subs, 0));

    let watch = SubtitleWatch::default();
    watch.observe("[info] Video subtitle en.vtt is already present");
    assert!(watch.written());

    // Unrelated lines are ignored entirely, subtitle-ish ones too.
    let watch = SubtitleWatch::default();
    watch.observe("[download] Destination: C:\\out\\Clip.en.mp4");
    watch.observe("[info] Writing video description to: C:\\out\\Clip.en.description");
    assert!(!watch.written() && !watch.missing());
}

#[test]
fn names_yt_dlp_would_expand_are_not_handed_to_its_output_template() {
    // yt-dlp runs `-o` through expanduser/expandvars with no way to escape
    // them: `$TEMP.mp4` would leave the download folder. Such a name is not
    // pinned (yt-dlp then names the file itself), and nothing is stamped for
    // a name SNATCH did not choose.
    let job = |url: &str| Job {
        engine: Engine::YtDlp,
        url: url.into(),
        out_dir: PathBuf::from("."),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    for hostile in [
        "https://example.com/$TEMP.mp4",
        "https://example.com/%24APPDATA.exe",
        "https://example.com/~admin.zip",
        "https://example.com/a$b.bin",
    ] {
        assert_eq!(ytdlp_direct_output(&job(hostile)), None, "{hostile}");
    }
    // A tilde or percent sign elsewhere is harmless and stays pinned.
    assert_eq!(
        ytdlp_direct_output(&job("https://example.com/a~b.mp4")).as_deref(),
        Some("a~b.mp4")
    );
}

#[test]
fn torrent_names_from_aria2_are_cleaned_before_they_become_a_label() {
    let line = "FILE: C:/Downloads/Track \u{202e}3pm.exe\u{1b}]0;x\u{7} (3more)";
    let name = aria2_name_from_file(line).unwrap();
    assert!(
        !name
            .chars()
            .any(|c| c.is_control() || crate::ui::is_disguising_format(c)),
        "{name:?}"
    );
    assert_eq!(aria2_name_from_file("FILE: \u{202e}\u{7}"), None);
}

#[cfg(windows)]
#[test]
fn trailing_dots_and_spaces_are_dropped_like_windows_does() {
    // `clip.mp4.` is created as `clip.mp4`; the guard must look at that file.
    assert_eq!(url_basename("https://h/b/clip.mp4."), "clip.mp4");
    assert_eq!(url_basename("https://h/b/clip.mp4%20"), "clip.mp4");
    assert_eq!(url_basename("https://h/b/..."), "");
}

#[test]
fn the_subtitle_language_default_lives_in_one_place() {
    let mut extras = RunExtras::default();
    assert_eq!(extras.effective_sub_langs(), DEFAULT_SUB_LANGS);
    extras.sub_langs = "  de, fr ".into();
    assert_eq!(extras.effective_sub_langs(), "de, fr");
}

#[test]
fn a_link_to_a_share_is_recognised_without_touching_the_share() {
    // The lexical rules still apply.
    assert!(reaches_unc(Path::new(r"\\host\share\folder")));
    assert!(reaches_unc(Path::new("//host/share/folder")));
    let real = tmp_out();
    std::fs::create_dir_all(real.join("local")).unwrap();
    assert!(!reaches_unc(&real));
    assert!(!reaches_unc(&real.join("local")));
    assert!(!reaches_unc(&real.join("not-there").join("deeper")));
    std::fs::remove_dir_all(&real).ok();

    // Symlinks cannot be created without a privilege on Windows, so the chain
    // logic runs against a small fake filesystem under /d (C:/d on Windows):
    // share-link -> share, chain -> share-link, rel -> ../local,
    // ok-link -> /plain, loop-a <-> loop-b.
    use std::path::PathBuf;
    let base = if cfg!(windows) { "C:/d" } else { "/d" };
    let at = |rel: &str| format!("{base}/{rel}");
    let fake = |path: &Path| -> LinkState {
        let name = path.to_string_lossy().replace('\\', "/");
        let to = |target: String| LinkState::Link(Ok(PathBuf::from(target)));
        if name == at("share-link") {
            to("//192.0.2.1/share".into())
        } else if name == at("chain") {
            to(at("share-link"))
        } else if name == at("rel") {
            to("../local".into())
        } else if name == at("ok-link") {
            to(if cfg!(windows) { "C:/plain" } else { "/plain" }.into())
        } else if name == at("loop-a") {
            to(at("loop-b"))
        } else if name == at("loop-b") {
            to(at("loop-a"))
        } else if name == at("broken") {
            LinkState::Link(Err(std::io::Error::other("unreadable")))
        } else if name == at("gone") || name == at("gone/x") {
            LinkState::Missing
        } else {
            LinkState::Plain
        }
    };
    let reaches = |rel: &str| reaches_unc_via(Path::new(&at(rel)), 0, &fake);
    assert!(reaches("share-link"), "a link straight to a share");
    assert!(reaches("share-link/sub/deeper"), "below such a link");
    assert!(reaches("chain"), "a link to a link to a share");
    assert!(reaches("loop-a"), "a loop never settles");
    assert!(reaches("broken"), "an unreadable link is not trusted");
    assert!(!reaches("rel"), "a relative link to a local folder");
    assert!(!reaches("ok-link/inside"), "a link to a local folder");
    assert!(!reaches("plain/inside"), "no links at all");
    assert!(!reaches("gone/x/y"), "nothing exists below a missing part");
}

#[test]
fn a_link_ending_in_a_slash_is_named_by_its_last_real_segment() {
    // Instagram, Threads and many players end their links with "/".
    assert_eq!(
        batch_name("https://www.instagram.com/reel/DeMCxNgt0EM/?obrf=abc"),
        "DeMCxNgt0EM"
    );
    assert_eq!(
        batch_name("https://www.threads.com/share/H9gVJw3xc/"),
        "H9gVJw3xc"
    );
    assert_eq!(
        batch_name("https://example.com/files/archive.zip"),
        "archive.zip"
    );
    // Percent-encoding is undone for display; control characters never show.
    assert_eq!(
        batch_name("https://example.com/%D0%9C%D0%BE%D1%81%D0%BA%D0%B2%D0%B0%20%281%29.jpg"),
        "Москва (1).jpg"
    );
    assert_eq!(batch_name("https://example.com/a%1B%5B2Jb.zip"), "ab.zip");
    // A local file is named by its file name, whichever separator the path uses.
    assert_eq!(
        batch_name("C:\\Users\\me\\Downloads\\ubuntu.iso.torrent"),
        "ubuntu.iso.torrent"
    );
    assert_eq!(batch_name("/home/me/t.torrent"), "t.torrent");
    // Only an authority: there is no name to show.
    assert_eq!(
        batch_name("https://example.com/"),
        tr!("download", "загрузка")
    );
    assert_eq!(
        batch_name("https://example.com"),
        tr!("download", "загрузка")
    );
}

#[test]
fn a_format_tag_is_not_part_of_the_title_yt_dlp_announces() {
    for (line, title) in [
        (
            "[download] Destination: C:\\dl\\Me at the zoo [jNQXAC9IVRw].f395.mp4",
            "Me at the zoo [jNQXAC9IVRw]",
        ),
        (
            "[download] Destination: D:\\x\\Video by a [DeM].fdash-1047539904997767v.mp4",
            "Video by a [DeM]",
        ),
        (
            "[Merger] Merging formats into \"C:\\dl\\Me at the zoo [jNQXAC9IVRw].webm\"",
            "Me at the zoo [jNQXAC9IVRw]",
        ),
        // A title that merely contains ".f" keeps it.
        ("[download] Destination: C:\\dl\\v1.final.mp4", "v1.final"),
    ] {
        assert_eq!(
            ytdlp_title_from_line(line).as_deref(),
            Some(title),
            "{line}"
        );
    }
}

#[test]
fn yt_dlp_recommending_cookies_arms_the_cookie_hint() {
    assert!(looks_like_auth(
        "ERROR: [vimeo] 56015672: The web client only works when logged-in. Use --cookies, --cookies-from-browser, --username and --password"
    ));
    // Remote text in a Destination line must not arm it.
    assert!(!looks_like_auth(
        "[download] Destination: how to use --cookies-from-browser.mp4"
    ));
}

#[test]
fn a_magnet_name_aria2_reports_still_form_encoded_is_decoded_for_display() {
    assert_eq!(
        aria2_metadata_name("FILE: [MEMORY][METADATA]The+WIRED+CD+-+Rip.+Sample%21").as_deref(),
        Some("The WIRED CD - Rip. Sample!")
    );
    // A real plus sign survives when the magnet spelled it %2B.
    assert_eq!(
        aria2_metadata_name("FILE: [MEMORY][METADATA]C%2B%2B+Primer").as_deref(),
        Some("C++ Primer")
    );
}

fn residue_job(out: &Path) -> Job {
    Job {
        engine: Engine::Aria2,
        url: "https://example.com/files/photo.jpg".into(),
        out_dir: out.to_path_buf(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    }
}

#[test]
fn a_refused_aria2_attempt_leaves_nothing_yt_dlp_could_mistake_for_a_finished_file() {
    let out = tmp_out();
    std::fs::create_dir_all(&out).unwrap();
    let job = residue_job(&out);
    let started = std::time::SystemTime::now();
    // What aria2c leaves after a refusal: the preallocated target (zeros) and
    // its control file - yt-dlp would call that "already downloaded".
    std::fs::write(out.join("photo.jpg"), vec![0u8; 4096]).unwrap();
    std::fs::write(out.join("photo.jpg.aria2"), b"control").unwrap();
    assert!(discard_failed_aria2_attempt(&job, started));
    assert!(!out.join("photo.jpg").exists());
    assert!(!out.join("photo.jpg.aria2").exists());
    // Nothing there is fine too.
    assert!(discard_failed_aria2_attempt(&job, started));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn an_older_file_of_the_same_name_is_never_touched_and_blocks_the_retry() {
    let out = tmp_out();
    std::fs::create_dir_all(&out).unwrap();
    let job = residue_job(&out);
    let old = out.join("photo.jpg");
    std::fs::write(&old, b"the user's own photo").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    let started = std::time::SystemTime::now();
    // aria2c saw the name taken and wrote to the next free one.
    std::fs::write(out.join("photo.1.jpg"), vec![0u8; 64]).unwrap();
    std::fs::write(out.join("photo.1.jpg.aria2"), b"control").unwrap();
    // yt-dlp would skip the download over the old file, so no retry.
    assert!(!discard_failed_aria2_attempt(&job, started));
    assert_eq!(std::fs::read(&old).unwrap(), b"the user's own photo");
    assert!(!out.join("photo.1.jpg").exists());
    assert!(!out.join("photo.1.jpg.aria2").exists());
    std::fs::remove_dir_all(&out).ok();
}

fn ytdlp_direct_job(out: &Path) -> Job {
    Job {
        engine: Engine::YtDlp,
        url: "https://example.com/files/file.zip".into(),
        out_dir: out.to_path_buf(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    }
}

#[test]
fn yt_dlp_never_continues_another_links_part_file() {
    let out = tmp_out();
    let state = out.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let job = ytdlp_direct_job(&out);
    // Nothing there: nothing to guard.
    assert_eq!(ytdlp_foreign_part_in(&job, &out, &state), None);
    assert!(build(&job, &tc_ytdlp()).is_ok());
    // A stranger's partial (no origin record): yt-dlp would continue it.
    let part = out.join("file.zip.part");
    std::fs::write(&part, b"another link's bytes").unwrap();
    assert_eq!(
        ytdlp_foreign_part_in(&job, &out, &state).as_deref(),
        Some("file.zip.part")
    );
    // The command is refused with and without aria2c as the downloader, and
    // the stranger's file is left alone.
    assert!(build(&job, &tc_ytdlp()).is_err());
    assert!(build(&job, &tc_both()).is_err());
    assert_eq!(std::fs::read(&part).unwrap(), b"another link's bytes");
    // The origin record for the SAME link makes the partial ours to continue...
    std::fs::write(
        aria2_origin_path(&out, "file.zip", &state),
        aria2_url_hash(&job.url),
    )
    .unwrap();
    assert_eq!(ytdlp_foreign_part_in(&job, &out, &state), None);
    // ...but not for another link that happens to save under the same name.
    let other = Job {
        url: "https://other.example/dl/file.zip".into(),
        ..job.clone()
    };
    assert!(ytdlp_foreign_part_in(&other, &out, &state).is_some());
    // A media page is not a pinned download: no such guard (yt-dlp names the
    // file itself).
    let page = Job {
        url: "https://youtube.com/watch?v=1".into(),
        ..job
    };
    assert_eq!(ytdlp_foreign_part_in(&page, &out, &state), None);
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn a_pinned_yt_dlp_download_takes_the_same_lock_and_origin_record_as_aria2() {
    let out = tmp_out();
    let state = out.join("state");
    let job = ytdlp_direct_job(&out);
    let lock = lock_aria2_target_in(&job, false, &state)
        .unwrap()
        .expect("a pinned direct download is locked");
    assert!(
        lock_aria2_target_in(&job, false, &state).is_err(),
        "two downloads must not share a name"
    );
    // An empty slot is claimed for this link.
    assert_eq!(
        std::fs::read_to_string(aria2_origin_path(&out, "file.zip", &state)).unwrap(),
        aria2_url_hash(&job.url)
    );
    drop(lock);
    // A media page is not locked.
    let page = Job {
        url: "https://youtube.com/watch?v=1".into(),
        ..job.clone()
    };
    assert!(lock_aria2_target_in(&page, false, &state)
        .unwrap()
        .is_none());
    std::fs::remove_dir_all(&out).ok();

    // A slot that already holds someone's `.part` is NOT claimed: the record
    // must not turn a stranger's partial into ours.
    let busy = tmp_out();
    let state = busy.join("state");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("file.zip.part"), b"stranger").unwrap();
    let job = ytdlp_direct_job(&busy);
    drop(lock_aria2_target_in(&job, false, &state).unwrap());
    assert!(!aria2_origin_path(&busy, "file.zip", &state).exists());
    assert!(ytdlp_foreign_part_in(&job, &busy, &state).is_some());
    std::fs::remove_dir_all(&busy).ok();
}
#[test]
fn sha256_upgrade_preserves_resume_origin_fingerprints() {
    assert_eq!(
        aria2_url_hash("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        aria2_url_hash(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
