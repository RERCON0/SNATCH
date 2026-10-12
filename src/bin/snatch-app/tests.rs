use super::events::DRAIN_BUDGET;
use super::launch::extras_from_fields;
use super::torrent_ui::{
    torrent_pick_window, torrent_row_height, torrent_tree_row, torrent_visible_rows,
};
use super::*;

fn test_app() -> SnatchApp {
    let (tx, rx) = channel();
    SnatchApp::from_config(
        snatch_rs::config::sanitize(&serde_json::Value::Null),
        tx,
        rx,
    )
}

fn test_job(url: &str) -> Job {
    Job {
        engine: Engine::YtDlp,
        url: url.into(),
        out_dir: std::env::temp_dir(),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    }
}

#[test]
fn closing_a_hidden_window_cancels_and_waits_for_its_downloads() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    let cancel = Arc::new(AtomicBool::new(false));
    app.jobs.push(ActiveJob {
        id: 1,
        job: test_job("https://example.test/video"),
        label: "video".into(),
        progress: None,
        status: String::new(),
        status_kind: StatusKind::None,
        warns: vec![],
        cancel_flag: cancel.clone(),
        cancelled: false,
        pausing: false,
        auth_seen: false,
        drm_seen: false,
        subs_watch: engines::SubtitleWatch::default(),
        missing_control_file: false,
        resumed_seen: false,
        corrupt_part_seen: false,
        metadata_wait: false,
        host_refused: false,
        started_at: std::time::SystemTime::now(),
        no_continue: false,
        select: torrent::Choice::default(),
    });
    let mut input = egui::RawInput::default();
    let viewport = input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap();
    viewport.minimized = Some(true);
    viewport.events.push(egui::ViewportEvent::Close);
    let output = ctx.run_logic(&input, |ctx| {
        eframe::App::logic(&mut app, ctx, &mut eframe::Frame::_new_kittest());
    });
    assert!(app.close_requested && app.jobs[0].cancelled);
    assert!(cancel.load(Ordering::SeqCst));
    assert!(output.viewport_commands[&egui::ViewportId::ROOT]
        .iter()
        .any(|command| matches!(command, egui::ViewportCommand::CancelClose)));
}

#[test]
fn setup_progress_text_shows_bytes_percent_and_a_fallback() {
    let p = SetupProgress {
        label: "ffmpeg".to_string(),
        done: 50 * 1024 * 1024,
        total: Some(100 * 1024 * 1024),
        stage: SetupStage::Downloading,
    };
    let t = setup_progress_text(Some(&p));
    assert!(t.contains("ffmpeg") && t.contains("50%"), "{t}");
    let p = SetupProgress {
        label: "yt-dlp.exe".to_string(),
        done: 1024,
        total: None,
        stage: SetupStage::Downloading,
    };
    assert!(setup_progress_text(Some(&p)).contains("yt-dlp.exe"));
    for stage in [
        SetupStage::Verifying,
        SetupStage::Extracting,
        SetupStage::Installing,
    ] {
        let p = SetupProgress {
            stage,
            done: 100,
            total: Some(100),
            ..p.clone()
        };
        let text = setup_progress_text(Some(&p));
        assert!(text.contains(stage.text()), "{text}");
        assert!(
            !text.contains("100%"),
            "processing still looks like a completed download: {text}"
        );
    }
    assert_eq!(
        setup_progress_text(None),
        tr!(
            "downloading yt-dlp, aria2c, ffmpeg and Deno…",
            "скачиваю yt-dlp, aria2c, ffmpeg и Deno…"
        )
    );
}

#[test]
fn gui_extras_split_args_and_reject_a_bad_quote() {
    let e = extras_from_fields(true, "ru,en", true, "--limit-rate 5M", "").unwrap();
    assert!(e.subs && e.playlist);
    assert_eq!(e.sub_langs, "ru,en");
    assert_eq!(e.extra_ytdlp, ["--limit-rate", "5M"]);
    assert!(e.extra_aria2.is_empty());
    assert!(extras_from_fields(false, "", false, "\"oops", "").is_err());
}

#[test]
fn refused_starts_keep_typed_urls_offers_and_queued_selections() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    let job = test_job("https://example.test/watch");
    app.url = job.url.clone();
    app.out_dir = job.out_dir.to_string_lossy().into_owned();
    app.retry_offer = Some(job.clone());
    app.start_download(&ctx);
    assert_eq!(app.url, job.url);
    assert!(app.retry_offer.is_some());
    app.retry_with_cookies(&ctx);
    assert!(app.retry_offer.is_some());
    app.queue.push_back(QueuedJob {
        job: job.clone(),
        select: torrent::Choice {
            name: None,
            files: Some(vec![1, 3]),
            meta: None,
        },
        no_continue: true,
    });
    app.fill_free_slots(&ctx);
    assert_eq!(app.paused.len(), 1);
    assert_eq!(app.paused[0].job.url, job.url);
    assert_eq!(
        app.paused[0].select.files.as_deref(),
        Some([1, 3].as_slice())
    );
    assert!(app.paused[0].no_continue);
    assert!(!app.paused[0].status.is_empty());
}

fn running(id: u64, job: Job) -> ActiveJob {
    ActiveJob {
        id,
        job,
        label: "video".into(),
        progress: Some(0.8),
        status: String::new(),
        status_kind: StatusKind::None,
        warns: vec![],
        cancel_flag: Arc::new(AtomicBool::new(false)),
        cancelled: false,
        pausing: false,
        auth_seen: false,
        drm_seen: false,
        subs_watch: engines::SubtitleWatch::default(),
        missing_control_file: false,
        resumed_seen: true,
        corrupt_part_seen: false,
        metadata_wait: false,
        host_refused: false,
        started_at: std::time::SystemTime::now(),
        no_continue: false,
        select: torrent::Choice::default(),
    }
}

#[test]
fn a_resumed_download_that_fails_for_any_other_reason_keeps_its_partial_file() {
    // A dropped connection on a resumed download must not discard the
    // partial: only a message that blames the partial itself justifies
    // restarting from scratch.
    let ctx = egui::Context::default();
    let mut app = test_app();
    app.jobs
        .push(running(1, test_job("https://example.test/watch")));
    app.finish_job(1, 1, &ctx);
    assert!(app.jobs.is_empty());
    assert!(
        app.paused.is_empty(),
        "no automatic from-scratch restart was attempted"
    );
    assert!(matches!(app.status_kind, StatusKind::Err));
}

#[test]
fn corrupt_partial_diagnostics_arm_the_restart_from_both_pipes() {
    let mut app = test_app();
    app.jobs
        .push(running(1, test_job("https://example.test/watch")));
    app.jobs
        .push(running(2, test_job("https://example.test/other")));
    let tx = app.tx.clone();
    tx.send(Msg::ErrLine(
        1,
        "ERROR: Postprocessing: Error opening input files: Invalid data found when processing input".into(),
    ))
    .unwrap();
    tx.send(Msg::Log(
        2,
        "[download] Destination: a title with Invalid data found in it".into(),
    ))
    .unwrap();
    app.drain(&egui::Context::default());
    assert!(app.jobs[0].corrupt_part_seen, "stderr diagnostic arms it");
    assert!(
        !app.jobs[1].corrupt_part_seen,
        "remote text without a diagnostics prefix must not"
    );
}

#[test]
fn pausing_a_from_scratch_restart_keeps_it_from_scratch() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    let mut job = running(1, test_job("https://example.test/watch"));
    job.no_continue = true;
    job.pausing = true;
    app.jobs.push(job);
    app.finish_job(1, 1, &ctx);
    assert_eq!(app.paused.len(), 1);
    assert!(app.paused[0].no_continue);
}

#[test]
fn the_metadata_caption_follows_a_flag_not_the_localized_status_text() {
    let mut app = test_app();
    app.jobs
        .push(running(1, test_job("magnet:?xt=urn:btih:abc")));
    let ctx = egui::Context::default();
    app.tx.send(Msg::TorrentMeta(1, String::new())).unwrap();
    app.drain(&ctx);
    assert!(app.jobs[0].metadata_wait);
    app.tx
        .send(Msg::TorrentStatus(1, 0.1, "10%".into()))
        .unwrap();
    app.drain(&ctx);
    assert!(!app.jobs[0].metadata_wait, "progress ends the wait");
}

#[test]
fn a_refused_automatic_restart_stays_retryable_without_a_false_status() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    let job = test_job("https://example.test/watch");
    app.jobs.push(ActiveJob {
        id: 1,
        job: job.clone(),
        label: "video".into(),
        progress: Some(0.8),
        status: String::new(),
        status_kind: StatusKind::None,
        warns: vec![],
        cancel_flag: Arc::new(AtomicBool::new(false)),
        cancelled: false,
        pausing: false,
        auth_seen: false,
        drm_seen: false,
        subs_watch: engines::SubtitleWatch::default(),
        missing_control_file: false,
        resumed_seen: true,
        corrupt_part_seen: true,
        metadata_wait: false,
        host_refused: false,
        started_at: std::time::SystemTime::now(),
        no_continue: false,
        select: torrent::Choice::default(),
    });
    app.finish_job(1, 1, &ctx);
    assert!(app.jobs.is_empty());
    assert_eq!(app.paused.len(), 1);
    assert_eq!(app.paused[0].job.url, job.url);
    assert!(app.paused[0].no_continue);
    assert!(!app.status.contains("restarting from scratch"));
    assert!(matches!(app.status_kind, StatusKind::Err));
}

#[test]
fn closing_refuses_picks_and_dispatch_before_any_side_effect() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    app.close_requested = true;
    let job = test_job("https://example.test/watch");
    assert!(!app.dispatch_job(&ctx, job.clone(), torrent::Choice::default()));
    app.begin_torrent_pick(&ctx, job);
    assert!(app.torrent_pick.is_none());
    assert!(app.queue.is_empty());
    assert!(app.jobs.is_empty());
    assert_eq!(app.next_pick_id, 0);
}

#[test]
fn an_open_pick_blocks_duplicate_downloads_and_directory_popups() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    let job = test_job("https://example.test/watch");
    app.torrent_pick = Some(TorrentPick {
        id: 1,
        job: job.clone(),
        cancel: Arc::new(AtomicBool::new(false)),
        info: None,
        meta: None,
        selected: vec![],
        tree: vec![],
        view: torrent::TreeView::default(),
        error: None,
        hint: None,
    });
    assert!(app.url_in_flight(&job.url));
    app.retry_offer = Some(job);
    app.retry_with_cookies(&ctx);
    assert!(app.retry_offer.is_some());
    assert!(app.jobs.is_empty());
    app.open_dir_browser();
    assert!(!app.browser.open);
}

#[test]
fn viewport_sizes_are_in_egui_points_without_a_second_scale() {
    for native in [1.0, 1.25, 2.0] {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .native_pixels_per_point = Some(native);
        let output = test_ui(&ctx, input, |ui| pin_pixel_scale(ui.ctx(), true));
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::InnerSize(size) if *size == egui::vec2(460.0, 720.0))));
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::MinInnerSize(size) if *size == egui::vec2(460.0, 600.0))));
    }
}

#[test]
fn directory_errors_are_visible_and_stale_scans_do_not_replace_navigation() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    let current = std::env::temp_dir().join("wanted");
    let previous = std::env::temp_dir().join("previous");
    app.browser.open = true;
    app.browser.current = current.clone();
    app.browser.in_flight = Some(previous.clone());
    app.tx
        .send(Msg::DirListed(
            previous.clone(),
            previous,
            Ok((vec!["stale".into()], false)),
        ))
        .unwrap();
    app.drain(&ctx);
    assert_eq!(app.browser.current, current);
    assert!(app.browser.entries.is_empty());
    app.tx
        .send(Msg::DirListed(
            current.clone(),
            current.clone(),
            Err("test access denied".into()),
        ))
        .unwrap();
    app.drain(&ctx);
    assert_eq!(app.browser.listed_for, Some(current));
    assert_eq!(app.browser.error.as_deref(), Some("test access denied"));
}

#[test]
fn torrent_file_summaries_cannot_replace_the_download_title() {
    let ctx = egui::Context::default();
    let mut app = test_app();
    let mut job =
        test_job("magnet:?xt=urn:btih:C575A7D7522DF7ADEAAFBFB2531264B2C8CF7067&dn=Hades%20II");
    job.engine = Engine::Aria2;
    app.jobs.push(ActiveJob {
        id: 1,
        label: engines::download_name(&job.url, &torrent::Choice::default()),
        job,
        progress: Some(0.01),
        status: String::new(),
        status_kind: StatusKind::None,
        warns: vec![],
        cancel_flag: Arc::new(AtomicBool::new(false)),
        cancelled: false,
        pausing: false,
        auth_seen: false,
        drm_seen: false,
        subs_watch: engines::SubtitleWatch::default(),
        missing_control_file: false,
        resumed_seen: false,
        corrupt_part_seen: false,
        metadata_wait: false,
        host_refused: false,
        started_at: std::time::SystemTime::now(),
        no_continue: false,
        select: torrent::Choice::default(),
    });
    for line in [
        "FILE: C:/Users/example/Desktop/file.bin (3more)",
        "FILE: C:/Users/example/Desktop/.egstore/data.bin (2more)",
    ] {
        app.tx.send(torrent_console_line(1, line).unwrap()).unwrap();
    }
    app.drain(&ctx);
    assert_eq!(app.jobs[0].label, "Hades II");
    app.tx
        .send(torrent_console_line(1, "FILE: [MEMORY][METADATA] Actual torrent title").unwrap())
        .unwrap();
    app.drain(&ctx);
    assert_eq!(app.jobs[0].label, "Actual torrent title");
    app.tx
        .send(torrent_console_line(1, "FILE: C:/Users/example/Desktop/file.bin (3more)").unwrap())
        .unwrap();
    app.drain(&ctx);
    assert_eq!(app.jobs[0].label, "Actual torrent title");
}

#[test]
fn torrent_console_summary_cannot_replace_gui_status_with_separator() {
    assert!(torrent_console_line(1, "*** Download Progress Summary as of now ***").is_none());
    assert!(torrent_console_line(1, "============================").is_none());
    assert!(
        matches!(torrent_console_line(1, "[#25017a 361MiB/6.8GiB(5%) CN:30 SD:5 DL:4.1MiB]"),
        Some(Msg::TorrentStatus(1, p, status)) if (p - 0.05).abs() < 0.001 && status.contains("361MiB/6.8GiB"))
    );
    assert!(
        matches!(torrent_console_line(1, "FILE: [MEMORY][METADATA][DL] Cuphead"),
        Some(Msg::TorrentMeta(1, name)) if name == "Cuphead")
    );
    assert!(
        matches!(torrent_console_line(1, "Exception: errorCode=13 File x exists, but a control file(*.aria2) does not exist."),
        Some(Msg::ErrLine(1, line)) if engines::aria2_missing_control(&line))
    );
}

#[test]
fn long_local_torrent_filename_is_a_valid_aria2_job() {
    let dir = std::env::temp_dir().join(format!("snatch-gui-torrent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("[DL] Cuphead [L] [RUS + ENG + 7] (2017, Arcade) (1.3.9 + 1 DLC) [GOG] [rutracker-5768660].torrent");
    std::fs::write(&file, b"d4:infod0:e").unwrap();
    let url = local_torrent_path(&file).unwrap();
    assert_eq!(url, file.to_string_lossy());
    assert_eq!(detect_engine(&url), Engine::Aria2);
    let job = Job {
        engine: detect_engine(&url),
        url,
        out_dir: dir.join("Downloads"),
        fmt: Format::Best,
        cookies_browser: None,
        download_name: None,
    };
    let tc = Toolchain {
        yt_dlp: None,
        aria2c: Some(PathBuf::from("aria2c")),
    };
    let cmd = engines::build(&job, &tc).unwrap();
    assert_eq!(cmd[cmd.len() - 2], "--");
    assert_eq!(cmd.last().unwrap(), file.as_os_str());
    assert!(local_torrent_path(&dir.join("not-a-torrent.txt")).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn job_controls_keep_same_height_with_and_without_progress() {
    let ctx = egui::Context::default();
    setup_theme(&ctx, true);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(460.0, 600.0),
        )),
        ..Default::default()
    };
    let mut heights = Vec::new();
    let _ = test_ui(&ctx, input, |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            let green = ui.visuals().hyperlink_color;
            let yellow = ui.visuals().warn_fg_color;
            for (progress, paused, fill, labels) in [
                (
                    Some(0.3),
                    false,
                    green,
                    (
                        (tr!("pause", "пауза"), 86.0),
                        (tr!("cancel", "отмена"), 86.0),
                    ),
                ),
                (
                    None,
                    false,
                    green,
                    (
                        (tr!("pause", "пауза"), 86.0),
                        (tr!("cancel", "отмена"), 86.0),
                    ),
                ),
                (
                    Some(0.3),
                    true,
                    yellow,
                    (
                        (tr!("resume", "продолжить"), 100.0),
                        (tr!("remove", "убрать"), 72.0),
                    ),
                ),
            ] {
                let top = ui.cursor().top();
                let _ = job_controls(
                    ui,
                    progress,
                    fill,
                    paused,
                    tr!("Connecting…", "Подключение…"),
                    labels.0,
                    labels.1,
                );
                heights.push(ui.cursor().top() - top);
            }
        });
    });
    for height in heights {
        assert!(
            (height - 34.0).abs() < 1.0,
            "row + spacing must be 28 + 6, got {height}"
        );
    }
}

#[test]
fn torrent_pick_rows_have_the_height_show_rows_assumes() {
    let ctx = egui::Context::default();
    setup_theme(&ctx, true);
    let files = [
        torrent::FileEntry {
            index: 1,
            path: "folder/a.bin".into(),
            size: 10,
        },
        torrent::FileEntry {
            index: 2,
            path: "folder/sub/b.bin".into(),
            size: 20,
        },
        torrent::FileEntry {
            index: 3,
            path: "top.txt".into(),
            size: 5,
        },
    ];
    let mut tree = torrent::build_torrent_tree(&files);
    tree[0].expanded = true;
    let mut selected = vec![true; files.len()];
    let mut rows = Vec::new();
    torrent_visible_rows(&tree, 0, &mut Vec::new(), &mut rows);
    // folder, folder/sub, folder/a.bin, top.txt: two folder rows (▶ toggle
    // + checkbox) and two file rows (checkbox only).
    assert_eq!(rows.len(), 4);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(600.0, 600.0),
        )),
        ..Default::default()
    };
    let (mut heights, mut pitch) = (Vec::new(), 0.0);
    let _ = test_ui(&ctx, input, |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            let row_h = torrent_row_height(ui);
            pitch = row_h + ui.spacing().item_spacing.y;
            for row in &rows {
                let top = ui.cursor().top();
                torrent_tree_row(ui, &mut tree, &mut selected, row, row_h);
                heights.push(ui.cursor().top() - top);
            }
        });
    });
    for height in heights {
        assert!(
            (height - pitch).abs() < 0.5,
            "every row must advance {pitch}, got {height}"
        );
    }
}

#[test]
fn torrent_pick_window_fits_the_default_window() {
    let ctx = egui::Context::default();
    setup_theme(&ctx, true);
    let files: Vec<torrent::FileEntry> = (1..=30)
        .map(|i| torrent::FileEntry {
            index: i,
            path: format!(
                "Очень длинное имя папки релиза {}/вложенная папка с длинным именем/файл номер {i} с длинным названием.mkv",
                i % 3
            ),
            size: 1 << 30,
        })
        .collect();
    let mut tree = torrent::build_torrent_tree(&files);
    for node in &mut tree {
        node.expanded = true;
    }
    let mut pick = TorrentPick {
        id: 0,
        job: Job {
            engine: Engine::Aria2,
            url: "magnet:?xt=urn:btih:c01abfff06149cb74765dca1b5226003eeb4e877".into(),
            out_dir: PathBuf::from("."),
            fmt: Format::Best,
            cookies_browser: None,
            download_name: None,
        },
        cancel: Arc::new(AtomicBool::new(false)),
        info: Some(torrent::TorrentInfo {
            name: "Очень длинное имя торрента, которое не помещается в одну строку диалога".into(),
            total: 30 << 30,
            files: files.clone(),
        }),
        meta: None,
        selected: vec![true; files.len()],
        tree,
        view: torrent::TreeView::default(),
        error: None,
        hint: None,
    };
    // The app's default (and minimum) window is 460 points wide.
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(460.0, 640.0));
    let frame = |pick: &mut TorrentPick| {
        let (mut rect, mut buttons) = (None, Vec::new());
        // A Window settles its size over the first frames.
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let _ = test_ui(&ctx, input, |ui| {
                let ctx = ui.ctx().clone();
                let ctx = &ctx;
                let (act, shown) = torrent_pick_window(ctx, pick);
                (rect, buttons) = (shown, act.buttons);
            });
        }
        (rect.expect("the pick window is shown"), buttons)
    };
    let check = |what: &str, (dialog, buttons): (egui::Rect, Vec<egui::Rect>), expected: usize| {
        assert!(
            screen.contains_rect(dialog),
            "{what} dialog {dialog:?} must fit {screen:?}"
        );
        assert_eq!(buttons.len(), expected, "{what}: buttons drawn");
        for (i, a) in buttons.iter().enumerate() {
            assert!(
                dialog.contains_rect(*a),
                "{what}: button {a:?} outside {dialog:?}"
            );
            for b in &buttons[i + 1..] {
                let both = a.intersect(*b);
                // An overlap made a click on one button hit the other.
                assert!(
                    both.width() <= 0.0 || both.height() <= 0.0,
                    "{what}: {a:?} overlaps {b:?}"
                );
            }
        }
    };
    check("list", frame(&mut pick), 4);
    pick.info = None;
    pick.error = Some(
        tr!(
            "could not fetch torrent metadata (timeout)",
            "не удалось получить метаданные торрента (таймаут)"
        )
        .into(),
    );
    check("error", frame(&mut pick), 2);
}

#[test]
fn late_startup_discovery_does_not_undo_installed_tools() {
    let installed = PathBuf::from("installed-aria2c.exe");
    let mut tools = Toolchain {
        yt_dlp: None,
        aria2c: Some(installed.clone()),
    };
    apply_initial_discovery(
        &mut tools,
        true,
        Toolchain {
            yt_dlp: None,
            aria2c: None,
        },
    );
    assert_eq!(tools.aria2c, Some(installed));
    let mut not_installed = Toolchain {
        yt_dlp: None,
        aria2c: None,
    };
    apply_initial_discovery(&mut not_installed, false, tools);
    assert!(not_installed.aria2c.is_some());
}

#[test]
fn one_frame_handles_a_bounded_batch_of_messages() {
    let mut app = test_app();
    app.jobs
        .push(running(1, test_job("https://example.test/watch")));
    let tx = app.tx.clone();
    let total = DRAIN_BUDGET * 2 + 10;
    for i in 0..total {
        tx.send(Msg::Log(1, format!("[download] line {i}")))
            .unwrap();
    }
    app.drain(&egui::Context::default());
    // The rest waits for the next frame instead of freezing this one.
    assert_eq!(app.rx.try_iter().count(), total - DRAIN_BUDGET);
}

fn test_ui(
    ctx: &egui::Context,
    input: egui::RawInput,
    draw: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let mut output = ctx.run_ui(input, draw);
    output.textures_delta.clear();
    output
}
