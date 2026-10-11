use super::*;

/// `tc: None` discovers lazily, AFTER `validate_url` succeeds - mirrors
/// Python's `_download(plan, cfg, tc: Toolchain | None = None)`, which only
/// resolved `Toolchain.discover()` past that same validation. The `-y`
/// single-shot path relies on this: a bad URL fails fast without paying for
/// a PATH/winget scan it's about to throw away.
pub(super) fn download(plan: &Plan, cfg: &mut Config, tc: Option<&Toolchain>) -> RunResult {
    download_with_discovery(plan, cfg, tc, Toolchain::discover)
}

pub(super) fn download_with_discovery(
    plan: &Plan,
    cfg: &mut Config,
    tc: Option<&Toolchain>,
    discover: impl FnOnce() -> Toolchain,
) -> RunResult {
    let url = match validate_url(&plan.url) {
        Ok(u) => u,
        Err(e) => {
            errln(format!("✘ {e}"));
            return RunResult {
                code: 2,
                auth_hint: false,
            };
        }
    };
    let discovered;
    let tc: &Toolchain = match tc {
        Some(t) => t,
        None => {
            discovered = discover();
            &discovered
        }
    };

    let clean_plan = Plan {
        url,
        ..plan.clone()
    };
    run_batch(std::slice::from_ref(&clean_plan), cfg, tc, 1)
        .into_iter()
        .next()
        .expect("one queued job has one result")
}

pub(super) fn read_batch_pipe(
    reader: impl std::io::Read,
    id: usize,
    aria2: bool,
    tx: &mpsc::Sender<BatchEvent>,
    auth: &AtomicBool,
    tls: &AtomicBool,
    subs: &engines::SubtitleWatch,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut last_progress = None::<Instant>;
    let mut last_file = None::<String>;
    loop {
        buf.clear();
        match engines::read_line_capped(&mut reader, &mut buf, engines::MAX_CHILD_LINE) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let decoded_bytes = engines::decode_child_bytes(&buf);
        let decoded = clean_loader_text(&decoded_bytes);
        let mut latest = None;
        for line in decoded.lines().map(str::trim).filter(|s| !s.is_empty()) {
            if engines::looks_like_auth(line) {
                auth.store(true, Ordering::Relaxed);
            }
            if engines::looks_like_aria2_refusal(line) {
                tls.store(true, Ordering::Relaxed);
            }
            subs.observe(line);
            if line.contains("Download Progress Summary as of")
                || line.chars().all(|c| c == '-' || c == '=')
            {
                continue;
            }
            if let Some(name) = engines::aria2_metadata_name(line) {
                let _ = tx.send(BatchEvent::TorrentName(id, name));
                latest = Some(
                    tr!(
                        "fetching torrent metadata…",
                        "получение метаданных торрента…"
                    )
                    .into(),
                );
            } else if let Some(name) = engines::aria2_name_from_file(line) {
                if last_file.as_deref() != Some(&name) {
                    last_file = Some(name.clone());
                    let _ = tx.send(BatchEvent::Name(id, name));
                }
            } else if let Some(title) = engines::ytdlp_title_from_line(line) {
                // The GUI does the same: the real title replaces the guess
                // taken from the URL (a video id, or nothing for /reel/ID/).
                if last_file.as_deref() != Some(&title) {
                    last_file = Some(title.clone());
                    let _ = tx.send(BatchEvent::Name(id, title));
                }
            } else if let Some(stat) = engines::aria2_stat(line) {
                latest = Some(stat);
            } else if engines::parse_progress(line).is_some() {
                latest = Some(clip(line, 90));
            } else if line.contains("Allocating disk space") {
                let _ = tx.send(BatchEvent::Progress(
                    id,
                    tr!("allocating disk space…", "выделение места на диске…").into(),
                ));
            } else if !line.starts_with("[#") && !line.starts_with("[FileAlloc:") {
                // Нижний регистр не считаем заранее: для yt-dlp он не нужен, а
                // для aria2 большинство строк ни одного из слов не содержит.
                let worth_reporting = !aria2
                    || ["error", "warning", "failed", "aborted", "exception"]
                        .iter()
                        .any(|hint| engines::contains_ignore_ascii_case(line, hint));
                if worth_reporting {
                    let _ = tx.send(BatchEvent::Line(id, line.to_string()));
                }
            }
        }
        if let Some(stat) = latest {
            if last_progress.is_none_or(|t| t.elapsed() >= Duration::from_millis(400)) {
                last_progress = Some(Instant::now());
                let _ = tx.send(BatchEvent::Progress(id, stat));
            }
        }
    }
}

pub(super) fn run_batch_job(
    id: usize,
    plan: &Plan,
    tc: &Toolchain,
    tx: &mpsc::Sender<BatchEvent>,
) -> RunResult {
    let url = match validate_url(&plan.url) {
        Ok(url) => url,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(id, format!("✘ {e}")));
            return RunResult {
                code: 2,
                auth_hint: false,
            };
        }
    };
    let job = Job {
        engine: plan.engine,
        url,
        out_dir: PathBuf::from(&plan.out_dir),
        fmt: plan.fmt,
        cookies_browser: plan.cookies_browser.clone(),
        download_name: plan.download_name.clone(),
    };
    for warn in preflight_warning(&job) {
        let _ = tx.send(BatchEvent::Line(id, format!("⚠ {warn}")));
    }
    // Hold the same-name OS lock through child.wait(): both this process and
    // the GUI/public edition must decide ownership before building -c flags.
    let _output_lock = match engines::lock_aria2_target(&job, true) {
        Ok(lock) => lock,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(id, format!("✘ {e}")));
            return RunResult {
                code: 2,
                auth_hint: false,
            };
        }
    };
    let cmd = match cli_command(
        &job,
        tc,
        plan.no_continue,
        true,
        &plan.torrent,
        &plan.extras,
    ) {
        Ok(cmd) => cmd,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(id, format!("✘ {e}")));
            return RunResult {
                code: 2,
                auth_hint: false,
            };
        }
    };
    let install_guard = match snatch_rs::tools::lock_for_spawn(Path::new(&cmd[0])) {
        Ok(guard) => guard,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(id, format!("✘ {e}")));
            return RunResult {
                code: 127,
                auth_hint: false,
            };
        }
    };
    let started = std::time::SystemTime::now();
    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    snatch_rs::engines::prepend_bootstrap_path(&mut command);
    let (mut child, proc_job) = match snatch_rs::job_object::spawn(&mut command, false) {
        Ok(result) => result,
        Err(e) => {
            let _ = tx.send(BatchEvent::Line(
                id,
                tr_format!(
                    "✘ Could not start downloader: {e}",
                    "✘ Не удалось запустить загрузчик: {e}"
                ),
            ));
            return RunResult {
                code: 127,
                auth_hint: false,
            };
        }
    };
    drop(install_guard);
    let auth = Arc::new(AtomicBool::new(false));
    let tls = Arc::new(AtomicBool::new(false));
    let subs = Arc::new(engines::SubtitleWatch::default());
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let err_tx = tx.clone();
    let err_auth = auth.clone();
    let err_tls = tls.clone();
    let err_subs = subs.clone();
    let aria2 = job.engine == Engine::Aria2;
    let err_reader = std::thread::spawn(move || {
        read_batch_pipe(stderr, id, aria2, &err_tx, &err_auth, &err_tls, &err_subs)
    });
    let out_tx = tx.clone();
    let out_auth = auth.clone();
    let out_tls = tls.clone();
    let out_subs = subs.clone();
    let out_reader = std::thread::spawn(move || {
        read_batch_pipe(stdout, id, aria2, &out_tx, &out_auth, &out_tls, &out_subs)
    });
    let status = child.wait();
    if let Some(job) = &proc_job {
        job.terminate();
    }
    let _ = out_reader.join();
    let _ = err_reader.join();
    let code = status.map(|s| s.code().unwrap_or(130)).unwrap_or(127);
    if code == 0 {
        // Windows' "downloaded from the Internet" mark, as a browser adds it.
        engines::mark_direct_download(&job, started);
    } else if aria2
        && tls.load(Ordering::Relaxed)
        && tc.yt_dlp.is_some()
        && !engines::is_bittorrent_input(&job.url)
        && engines::discard_failed_aria2_attempt(&job, started)
    {
        // The host turned aria2c away - its Windows TLS client cannot talk to
        // some hosts, and others answer its fingerprint with 403/429 - while
        // yt-dlp downloads a direct link with OpenSSL. One automatic retry.
        let _ = tx.send(BatchEvent::Line(
            id,
            tr!(
                "⚠ This host turned aria2c away (TLS or HTTP 403/429) — retrying with yt-dlp",
                "⚠ Сайт отклонил aria2c (TLS или HTTP 403/429) — повторяю через yt-dlp"
            )
            .into(),
        ));
        // The resolved file name stays (see the GUI's equivalent retry). The
        // host is remembered so yt-dlp downloads the file itself instead of
        // passing it back to aria2c.
        links::remember_aria2_refused(&job.url);
        let retry = Plan {
            engine: Engine::YtDlp,
            fmt: Format::Best,
            ..plan.clone()
        };
        return run_batch_job(id, &retry, tc, tx);
    }
    if subs.nothing_written(plan.fmt, code) {
        // yt-dlp treats "no subtitles for the requested languages" as
        // success; for the subtitles-only format that is an empty folder,
        // not a finished request - report it instead of claiming "Done".
        let langs = plan.extras.effective_sub_langs();
        let _ = tx.send(BatchEvent::Line(
            id,
            tr_format!(
                "✘ No subtitles in {langs}: nothing was written (the video itself is never downloaded in subs mode).",
                "✘ Субтитров на языках {langs} нет: ничего не записано (в режиме «только субтитры» само видео не скачивается)."
            ),
        ));
        return RunResult {
            code: RETRY_CODE,
            auth_hint: false,
        };
    }
    RunResult {
        code,
        auth_hint: auth.load(Ordering::Relaxed),
    }
}

/// Keep at most `jobs` independent loader processes alive, and serialize all
/// terminal output and config writes on this (main) thread. Each worker owns
/// a different Job and pipe pair; one noisy torrent cannot overwrite another
/// torrent's progress or cookie hint.
pub(super) fn run_batch(
    plans: &[Plan],
    cfg: &mut Config,
    tc: &Toolchain,
    jobs: usize,
) -> Vec<RunResult> {
    run_batch_with(plans, cfg, tc, jobs, run_batch_job)
}

pub(super) fn run_batch_with(
    plans: &[Plan],
    cfg: &mut Config,
    tc: &Toolchain,
    jobs: usize,
    run: impl Fn(usize, &Plan, &Toolchain, &mpsc::Sender<BatchEvent>) -> RunResult + Sync,
) -> Vec<RunResult> {
    let queue = Arc::new(Mutex::new((0..plans.len()).collect::<VecDeque<_>>()));
    let (tx, rx) = mpsc::channel();
    let mut results: Vec<Option<RunResult>> = (0..plans.len()).map(|_| None).collect();
    let mut missing_control = vec![false; plans.len()];
    let mut drm = vec![false; plans.len()];
    let mut display = BatchDisplay::new(plans);
    std::thread::scope(|scope| {
        let run = &run;
        for _ in 0..jobs.min(plans.len()) {
            let queue = queue.clone();
            let tx = tx.clone();
            scope.spawn(move || loop {
                let Some(id) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                let plan = &plans[id];
                let _ = tx.send(BatchEvent::Progress(
                    id,
                    tr!("connecting…", "подключение…").to_string(),
                ));
                let result = run(id, plan, tc, &tx);
                let _ = tx.send(BatchEvent::Finished(
                    id,
                    plan.url.clone(),
                    plan.out_dir.clone(),
                    result,
                ));
            });
        }
        drop(tx);
        for event in rx {
            match event {
                BatchEvent::Line(id, line) => {
                    if engines::is_drm_protected(&line) {
                        drm[id] = true;
                    }
                    if plans[id].engine == Engine::Aria2 && engines::aria2_missing_control(&line) {
                        if !missing_control[id] {
                            missing_control[id] = true;
                            display.diagnostic(id, engines::aria2_missing_control_hint());
                        }
                    } else if plans[id].engine != Engine::Aria2
                        || !(line.contains("Exception caught")
                            || line.starts_with("(OK):")
                            || line.starts_with("If there are any errors"))
                    {
                        display.diagnostic(id, &line);
                    }
                }
                BatchEvent::Name(id, name) => {
                    if !engines::is_bittorrent_input(&plans[id].url) {
                        display.update(id, None, Some(name));
                    }
                }
                BatchEvent::TorrentName(id, name) => display.update(id, None, Some(name)),
                BatchEvent::Progress(id, line) => display.update(id, Some(line), None),
                BatchEvent::Finished(id, url, dir, result) => {
                    if result.code == 0 {
                        cfg.remember_url(&url);
                        cfg.remember_dir(&dir);
                        cfg.save();
                        let dir = clip(&engines::sanitize_child_output(&dir), 90);
                        display.finished(id, tr_format!("✔ Done: {dir}", "✔ Готово: {dir}"));
                        if plans[id].torrent.files.is_some() {
                            display.diagnostic(id, partial_neighbours_note());
                        }
                    } else {
                        display.finished(
                            id,
                            failure_summary(result.code, missing_control[id], drm[id]),
                        );
                        if drm[id] {
                            display.diagnostic(id, engines::drm_protected_note());
                        }
                    }
                    results[id] = Some(result);
                }
            }
        }
    });
    results
        .into_iter()
        .map(|r| {
            r.unwrap_or(RunResult {
                code: 127,
                auth_hint: false,
            })
        })
        .collect()
}

pub(super) fn batch_exit_code(results: &[RunResult]) -> i32 {
    results.iter().find(|r| r.code != 0).map_or(0, |r| r.code)
}
