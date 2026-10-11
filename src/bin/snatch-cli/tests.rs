use super::*;

#[test]
fn interactive_overrides_are_honored_without_opening_prompts() {
    let tc = Toolchain {
        yt_dlp: None,
        aria2c: None,
    };
    let plan = collect(
        &tc,
        "https://example.test/video".into(),
        "out",
        None,
        Some(Engine::YtDlp),
        Some(Format::Audio),
    )
    .unwrap();
    assert_eq!(plan.engine, Engine::YtDlp);
    assert_eq!(plan.fmt, Format::Audio);
}

#[test]
fn failure_summary_prefers_the_missing_control_and_drm_explanations() {
    let generic = failure_summary(1, false, false);
    assert!(
        generic.contains("(код 1)") || generic.contains("(code 1)"),
        "{generic}"
    );
    assert!(failure_summary(13, true, false).contains(".aria2"));
    assert_eq!(
        failure_summary(1, false, true),
        format!("✘ {}", engines::drm_protected_short())
    );
}

#[test]
fn picker_footer_never_moves_the_cursor_beyond_the_last_line() {
    for height in 6..100 {
        let newlines = 2 + picker_view_rows(height) + 2;
        assert!(newlines < height as usize);
    }
    assert_eq!(picker_view_rows(4), 0);
}
use std::sync::atomic::AtomicUsize;

#[test]
fn banner_preserves_ascii_art_indent() {
    let first = BANNER.trim_matches(['\r', '\n']).lines().next().unwrap();
    assert!(first.starts_with(" ____"), "{first:?}");
    assert!(BANNER.contains("lightweight downloader"));
}

fn base_plan() -> Plan {
    Plan {
        url: "u".to_string(),
        download_name: None,
        engine: Engine::YtDlp,
        fmt: Format::Best,
        out_dir: "d".to_string(),
        cookies_browser: None,
        no_continue: false,
        torrent: torrent::Choice::default(),
        extras: engines::RunExtras::default(),
    }
}

fn empty_cfg() -> Config {
    Config::load_from(&std::env::temp_dir().join("snatch-rs-cli-test-nonexistent.json"))
}

// -- should_offer_cookies (test_offers_only_on_child_auth_failure et al) --

#[test]
fn offers_only_on_child_auth_failure() {
    let plan = base_plan();
    assert!(should_offer_cookies(&plan, 1, true));
    assert!(!should_offer_cookies(&plan, 1, false));
    for code in [0, 2, 3, 127, 130, -9] {
        assert!(!should_offer_cookies(&plan, code, true), "code {code}");
    }
}

#[test]
fn no_offer_for_aria2_engine() {
    let mut plan = base_plan();
    plan.engine = Engine::Aria2;
    assert!(!should_offer_cookies(&plan, 1, true));
}

#[test]
fn offer_repeats_after_failed_cookies_attempt() {
    let mut plan = base_plan();
    plan.cookies_browser = Some("chrome".to_string());
    assert!(should_offer_cookies(&plan, 1, true));
}

// -- plan_from_args (test_plan_from_args_keeps_cookies_value, test_yes_mode_requires_url_and_output) --

fn args_with(url: Option<&str>, output: Option<&str>, cookies_browser: Option<&str>) -> Args {
    Args {
        lang: None,
        help: false,
        version: false,
        urls: url.into_iter().map(String::from).collect(),
        output: output.map(String::from),
        engine: None,
        format: None,
        yes: true,
        jobs: 3,
        cookies_browser: cookies_browser.map(String::from),
        clear_history: false,
        install_tools: false,
        no_continue: false,
        subs: false,
        sub_langs: None,
        playlist: false,
        yt_dlp_args: None,
        aria2_args: None,
    }
}

#[test]
fn plan_from_args_carries_run_extras_and_rejects_a_bad_quote() {
    let mut args = args_with(Some("https://x"), Some("d"), None);
    args.subs = true;
    args.sub_langs = Some("ru,en".into());
    args.playlist = true;
    args.yt_dlp_args = Some("--limit-rate 5M".into());
    let plan = plan_from_args(&args).unwrap();
    assert!(plan.extras.subs);
    assert_eq!(plan.extras.sub_langs, "ru,en");
    assert!(plan.extras.playlist);
    assert_eq!(plan.extras.extra_ytdlp, ["--limit-rate", "5M"]);
    args.yt_dlp_args = Some("\"oops".into());
    assert!(plan_from_args(&args).is_none());
}

#[test]
fn plan_from_args_carries_no_continue() {
    let mut args = args_with(Some("https://x"), Some("d"), None);
    assert!(!plan_from_args(&args).unwrap().no_continue);
    args.no_continue = true;
    assert!(plan_from_args(&args).unwrap().no_continue);
}

#[test]
fn exit_code_maps_out_of_range_to_failure() {
    // A crashed child arrives as a negative i32 (0xC0000005 ->
    // -1073741819); it must never become exit status 0 ("success").
    assert_eq!(exit_code_u8(0), 0);
    assert_eq!(exit_code_u8(1), 1);
    assert_eq!(exit_code_u8(255), 255);
    assert_eq!(exit_code_u8(-1073741819), 1);
    assert_eq!(exit_code_u8(256), 1);
    assert_eq!(exit_code_u8(-1), 1);
}

#[test]
fn plan_from_args_keeps_cookies_value() {
    let args = args_with(Some("https://x"), Some("d"), Some("Chrome:Profile 1"));
    let plan = plan_from_args(&args).unwrap();
    assert_eq!(plan.cookies_browser.as_deref(), Some("Chrome:Profile 1"));

    let args = args_with(Some("https://x"), Some("d"), None);
    assert_eq!(plan_from_args(&args).unwrap().cookies_browser, None);
}

#[test]
fn plan_from_args_requires_url_and_output() {
    assert!(plan_from_args(&args_with(None, None, None)).is_none());
    assert!(plan_from_args(&args_with(Some("https://x"), None, None)).is_none());
    assert!(plan_from_args(&args_with(None, Some("d"), None)).is_none());
}

#[test]
fn multiple_urls_select_engines_independently() {
    let mut args = args_with(
        Some("https://video.example/watch?id=1"),
        Some("downloads"),
        None,
    );
    args.urls
        .push("magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4".into());
    args.urls.push("https://host/file.iso".into());
    let plans = plans_from_args(&args).unwrap();
    assert_eq!(plans.len(), 3);
    assert_eq!(
        plans.iter().map(|p| p.engine.as_str()).collect::<Vec<_>>(),
        ["yt-dlp", "aria2", "aria2"]
    );
    assert!(plans.iter().all(|p| p.out_dir == "downloads"));
    assert_eq!(
        batch_exit_code(&[
            RunResult {
                code: 0,
                auth_hint: false
            },
            RunResult {
                code: 1,
                auth_hint: false
            }
        ]),
        1
    );
}

#[test]
fn interactive_links_split_preserves_magnets_and_quoted_torrent_paths() {
    let links = split_links("https://host/a.zip magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4&dn=Game%20One \"C:\\My Torrents\\Game 2.torrent\"").unwrap();
    assert_eq!(links.len(), 3);
    assert!(links[1].contains("&dn=Game%20One"));
    assert_eq!(links[2], "C:\\My Torrents\\Game 2.torrent");
    assert!(split_links("'C:\\My Torrents\\Game.torrent").is_err());
}

#[test]
fn interactive_queue_summary_counts_and_lists_each_engine() {
    let mut args = args_with(Some("https://host/a.zip"), Some("Downloads"), None);
    args.urls.extend([
        "https://example.com/watch?v=2".into(),
        "https://host/b.zip".into(),
    ]);
    let plans = plans_from_args(&args).unwrap();
    let text = queue_summary(&plans, 2);
    assert!(text.contains("Jobs: 3 · concurrent: 2"));
    assert!(text.contains("1/3.") && text.contains("2/3.") && text.contains("3/3."));
    assert_eq!(text.matches("aria2c →").count(), 2);
    assert_eq!(text.matches("yt-dlp ·").count(), 1);
}

#[test]
fn cli_progress_options_do_not_change_shared_gui_builder() {
    let dir = std::env::temp_dir().join(format!("snatch-cli-opts-{}", std::process::id()));
    let tc = Toolchain {
        yt_dlp: None,
        aria2c: Some(PathBuf::from("aria2c")),
    };
    let job = Job {
        engine: Engine::Aria2,
        url: "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4".into(),
        out_dir: dir.clone(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let cmd = cli_command(
        &job,
        &tc,
        false,
        true,
        &torrent::Choice::default(),
        &engines::RunExtras::default(),
    )
    .unwrap();
    assert_eq!(cmd[cmd.len() - 2], "--");
    assert!(cmd.iter().any(|arg| arg == "--summary-interval=2"));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn batch_torrent_output_keeps_names_stats_and_allocation_without_terminal_noise() {
    let (tx, rx) = mpsc::channel();
    let auth = AtomicBool::new(false);
    let output = b"09/27 03:10 [\x1b[1;32mNOTICE\x1b[0m] Allocating disk space. See --file-allocation option\n\
                   FILE: C:/Downloads/Cuphead_1.3.9/setup-1.bin (7more)\n\
                   *** Download Progress Summary as of now ***\n\
                   [#bc1f6e 0B/6.8GiB(0%) CN:0 SD:0 DL:0B] [FileAlloc:#bc1f6e 1.1GiB/3.3GiB(33%)]\n";
    read_batch_pipe(
        &output[..],
        0,
        true,
        &tx,
        &auth,
        &AtomicBool::new(false),
        &engines::SubtitleWatch::default(),
    );
    let events: Vec<_> = rx.try_iter().collect();
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[1], BatchEvent::Name(0, name) if name == "Cuphead_1.3.9"));
    assert!(
        matches!(&events[2], BatchEvent::Progress(0, line) if line.contains("allocating space 1.1GiB/3.3GiB (33%)"))
    );
    assert!(!events.iter().any(|e| matches!(e, BatchEvent::Line(..))));
    assert!(!clean_loader_text("\x1b[1;32mNOTICE\x1b[0m").contains("[1;32m"));
    let stat =
        engines::aria2_stat("[#50e13e 880KiB/0.9MiB(88%) CN:1 SD:4 DL:812KiB UL:14KiB]").unwrap();
    assert!(
        stat.contains("88% · 880KiB/0.9MiB · ↓812KiB/s · seeds 4 · connections 1"),
        "{stat}"
    );
}

#[test]
fn batch_pipe_flags_rate_limited_subtitles_as_a_cookies_hint() {
    // The exact shape a real run produced: subtitles, HTTP 429. The pipe
    // reader must arm the cookies retry ("try cookies from a browser?")
    // and still report the line to the user.
    let (tx, rx) = mpsc::channel();
    let auth = AtomicBool::new(false);
    let output =
        b"ERROR: Unable to download video subtitles for 'ru': HTTP Error 429: Too Many Requests\n";
    read_batch_pipe(
        &output[..],
        0,
        false,
        &tx,
        &auth,
        &AtomicBool::new(false),
        &engines::SubtitleWatch::default(),
    );
    assert!(auth.load(Ordering::Relaxed));
    let events: Vec<_> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BatchEvent::Line(0, l) if l.contains("429"))),
        "no 429 line among {} events",
        events.len()
    );
}

#[test]
fn batch_pipe_feeds_the_subtitle_watch() {
    let (tx, _rx) = mpsc::channel();
    let auth = AtomicBool::new(false);
    let subs = engines::SubtitleWatch::default();
    let output =
        b"[info] There are no subtitles for the requested languages\n[info] Downloading 1 format(s)\n";
    read_batch_pipe(
        &output[..],
        0,
        false,
        &tx,
        &auth,
        &AtomicBool::new(false),
        &subs,
    );
    assert!(subs.missing() && !subs.written());
    assert!(subs.nothing_written(Format::Subs, 0));
    // The same line must not arm the cookie offer.
    assert!(!auth.load(Ordering::Relaxed));
}

#[test]
fn batch_colors_only_tty_progress_numbers() {
    let view = BatchView {
        name: "movie.mkv".into(),
        status: "42% · 420MiB/1GiB · ↓3MiB/s · seeds 8".into(),
        last_printed: None,
    };
    let colored = BatchDisplay::line(0, 2, &view, true);
    assert!(colored.contains("\x1b[92m42%\x1b[0m"));
    assert!(colored.contains("\x1b[92m420MiB\x1b[0m/1GiB"));
    assert!(colored.contains("\x1b[96m↓3MiB/s\x1b[0m"));
    let plain = BatchDisplay::line(0, 2, &view, false);
    assert!(!plain.contains('\x1b'));
    assert!(plain.contains("42% · 420MiB/1GiB · ↓3MiB/s"));
}

#[test]
fn batch_reports_each_failure_in_original_url_order() {
    let mut args = args_with(Some("not-a-url"), Some("unused"), None);
    args.urls.push("also-not-a-url".into());
    let plans = plans_from_args(&args).unwrap();
    let mut cfg = empty_cfg();
    let tc = Toolchain {
        yt_dlp: None,
        aria2c: None,
    };
    let results = run_batch(&plans, &mut cfg, &tc, 2);
    assert_eq!(results.iter().map(|r| r.code).collect::<Vec<_>>(), [2, 2]);
    assert_eq!(batch_exit_code(&results), 2);
    assert!(cfg.urls.is_empty());
}

#[test]
fn batch_respects_worker_limit_and_keeps_results_by_job() {
    let mut args = args_with(Some("https://host/one.zip"), Some("unused"), None);
    args.urls.extend([
        "https://host/two.zip".into(),
        "https://host/three.zip".into(),
    ]);
    let plans = plans_from_args(&args).unwrap();
    let mut cfg = empty_cfg();
    let tc = Toolchain {
        yt_dlp: None,
        aria2c: None,
    };
    let active = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let results = run_batch_with(&plans, &mut cfg, &tc, 2, |id, _, _, _| {
        let n = active.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(n, Ordering::SeqCst);
        let start = Instant::now();
        while peak.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(20));
        active.fetch_sub(1, Ordering::SeqCst);
        RunResult {
            code: id as i32 + 1,
            auth_hint: false,
        }
    });
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        results.iter().map(|r| r.code).collect::<Vec<_>>(),
        [1, 2, 3]
    );
}

#[test]
fn jobs_flag_rejects_zero_and_more_than_sixteen() {
    assert!(Args::try_parse_from(["snatch", "--jobs", "0"]).is_err());
    assert!(Args::try_parse_from(["snatch", "--jobs", "17"]).is_err());
    assert_eq!(Args::try_parse_from(["snatch", "-j", "2"]).unwrap().jobs, 2);
}

// Bad URLs must fail before the potentially slow PATH/winget discovery.
#[test]
fn download_bad_url_fails_without_valid_toolchain() {
    let mut plan = base_plan();
    plan.url = "https://[::1".to_string();
    let mut cfg = empty_cfg();
    assert_eq!(
        download_with_discovery(&plan, &mut cfg, None, || panic!(
            "bad URL must not discover tools"
        ))
        .code,
        2
    );
}

// -- retry_with_cookies_inner (test_retry_with_cookies_*, test_no_retry_without_auth_hint, test_retry_gives_up_after_second_failure) --

#[test]
fn retry_with_cookies_retries_and_succeeds() {
    let mut cfg = empty_cfg();
    let mut seen: Vec<Option<String>> = Vec::new();
    let out = retry_with_cookies_inner(
        base_plan(),
        RunResult {
            code: 1,
            auth_hint: true,
        },
        &mut cfg,
        |_q, _default| Some(true),
        || Some("chrome".to_string()),
        |plan, _cfg| {
            seen.push(plan.cookies_browser.clone());
            RunResult {
                code: 0,
                auth_hint: false,
            }
        },
    );
    assert_eq!(out.code, 0);
    assert_eq!(seen, vec![Some("chrome".to_string())]);
}

#[test]
fn retry_with_cookies_declined_returns_original() {
    let mut cfg = empty_cfg();
    let out = retry_with_cookies_inner(
        base_plan(),
        RunResult {
            code: 1,
            auth_hint: true,
        },
        &mut cfg,
        |_q, _default| Some(false),
        || panic!("select must not be called"),
        |_p, _c| panic!("download must not be called"),
    );
    assert_eq!(out.code, 1);
}

#[test]
fn retry_with_cookies_no_browser_selected() {
    let mut cfg = empty_cfg();
    let out = retry_with_cookies_inner(
        base_plan(),
        RunResult {
            code: 1,
            auth_hint: true,
        },
        &mut cfg,
        |_q, _default| Some(true),
        || None,
        |_p, _c| panic!("download must not be called"),
    );
    assert_eq!(out.code, 1);
}

#[test]
fn no_retry_without_auth_hint() {
    let mut cfg = empty_cfg();
    let out = retry_with_cookies_inner(
        base_plan(),
        RunResult {
            code: 1,
            auth_hint: false,
        },
        &mut cfg,
        |_q, _default| panic!("confirm must not be called"),
        || panic!("select must not be called"),
        |_p, _c| panic!("download must not be called"),
    );
    assert_eq!(out.code, 1);
}

#[test]
fn retry_second_decline_returns_failure() {
    // The loop is user-driven (no retry cap): the first offer is accepted
    // (login) and the retried download fails again; declining the second
    // offer must return that failure as-is.
    let mut cfg = empty_cfg();
    let mut confirms = vec![false, true]; // .pop() yields true first, then false
    let mut calls = 0;
    let out = retry_with_cookies_inner(
        base_plan(),
        RunResult {
            code: 1,
            auth_hint: true,
        },
        &mut cfg,
        move |_q, _default| confirms.pop(),
        || Some("chrome".to_string()),
        |_p, _c| {
            calls += 1;
            RunResult {
                code: 1,
                auth_hint: true,
            }
        },
    );
    assert_eq!(out.code, 1);
    assert_eq!(calls, 1);
}

// -- confirm_message (test_confirm_mentions_cookies, test_confirm_aria2_has_no_cookies_line) --

#[test]
fn confirm_mentions_cookies() {
    let msg = confirm_message(
        Engine::YtDlp,
        "https://youtube.com/watch?v=1",
        Format::Best,
        "d",
        Some("chrome"),
    );
    assert!(msg.contains("chrome"));
    let msg = confirm_message(
        Engine::YtDlp,
        "https://youtube.com/watch?v=1",
        Format::Best,
        "d",
        None,
    );
    assert!(!msg.contains("chrome"));
}

#[test]
fn confirm_aria2_has_no_cookies_line() {
    let msg = confirm_message(
        Engine::Aria2,
        "https://host/file.zip",
        Format::Best,
        "d",
        None,
    );
    assert!(msg.contains("aria2c"));
    assert!(!msg.contains("chrome"));
}

#[test]
fn confirm_line_uses_the_same_source_aware_label_as_the_format_menu() {
    // The plan summary must repeat what the user picked (the SoundCloud
    // preset names "best" after its audio behaviour, not "Best quality").
    let msg = confirm_message(
        Engine::YtDlp,
        "https://soundcloud.com/artist/track",
        Format::Best,
        "d",
        None,
    );
    assert!(
        msg.contains(tr!(
            "Audio — as-is (no conversion)",
            "Аудио — как есть (без перекодирования)"
        )),
        "{msg}"
    );
    let msg = confirm_message(
        Engine::YtDlp,
        "https://www.youtube.com/watch?v=1",
        Format::Best,
        "d",
        None,
    );
    assert!(
        msg.contains(tr!("Best quality", "Лучшее качество")),
        "{msg}"
    );
}

#[test]
fn picker_rows_are_clipped_by_terminal_cells() {
    assert_eq!(clip_cells("abc", 3), "abc");
    assert_eq!(clip_cells("abcdef", 4), "abc…");
    // Each CJK char is two cells: 3 of them + the ellipsis fill 7 cells.
    assert_eq!(clip_cells("東京都の写真集", 8), "東京都…");
    assert_eq!(clip_cells("東京", 4), "東京");
}

#[test]
fn a_long_list_of_links_is_prepared_in_order_by_a_few_workers() {
    // Invalid links are never probed, so this stays off the network.
    let plans: Vec<Plan> = (0..64)
        .map(|i| Plan {
            url: format!("not a link {i}"),
            ..base_plan()
        })
        .collect();
    let urls: Vec<String> = plans.iter().map(|p| p.url.clone()).collect();
    let prepared = prepare_plans(plans, true);
    assert_eq!(
        prepared.iter().map(|p| p.url.clone()).collect::<Vec<_>>(),
        urls
    );
}

// -- engine_offer / ask_engine --

#[test]
fn engine_offer_puts_guess_first() {
    let tc = Toolchain {
        yt_dlp: Some(PathBuf::from("y")),
        aria2c: Some(PathBuf::from("a")),
    };
    assert_eq!(
        engine_offer(&tc, "https://youtube.com/watch?v=1"),
        [Engine::YtDlp, Engine::Aria2]
    );
    assert_eq!(
        engine_offer(&tc, "https://host/f.zip"),
        [Engine::Aria2, Engine::YtDlp]
    );
    let none = Toolchain {
        yt_dlp: None,
        aria2c: None,
    };
    assert!(engine_offer(&none, "https://x").is_empty());
    let one = Toolchain {
        yt_dlp: Some(PathBuf::from("y")),
        aria2c: None,
    };
    assert_eq!(
        engine_offer(&one, "https://youtube.com/watch?v=1"),
        [Engine::YtDlp]
    );
}

#[test]
fn ask_engine_single_option_needs_no_terminal() {
    let tc = Toolchain {
        yt_dlp: Some(PathBuf::from("y")),
        aria2c: None,
    };
    assert_eq!(
        ask_engine("https://youtube.com/watch?v=1", &tc, &LinkInfo::default()),
        Some(Engine::YtDlp)
    );
}
