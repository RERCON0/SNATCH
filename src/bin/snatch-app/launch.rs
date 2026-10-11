use super::*;

/// Turns the GUI's option fields into the argv extras build() understands.
/// A bad quote is reported instead of silently running with different flags.
pub(super) fn extras_from_fields(
    subs: bool,
    sub_langs: &str,
    playlist: bool,
    ytdlp: &str,
    aria2: &str,
) -> Result<engines::RunExtras, String> {
    Ok(engines::RunExtras {
        subs,
        sub_langs: sub_langs.to_string(),
        playlist,
        extra_ytdlp: engines::split_cli_args(ytdlp)?,
        extra_aria2: engines::split_cli_args(aria2)?,
    })
}

impl SnatchApp {
    /// The "▶ СКАЧАТЬ" / "+ В ОЧЕРЕДЬ" button always calls this. Starts right
    /// away while there's a free concurrent slot (MAX_CONCURRENT, the same
    /// default as the CLI's `-j`); once full, queues instead of
    /// locking the form - engine is auto-detected per link right now
    /// (whatever the current движок/формат/папка choice resolves to),
    /// matching the CLI's `-j` batch where each URL picks its own engine but
    /// shares format/folder.
    pub(super) fn start_download(&mut self, ctx: &egui::Context) {
        if self.close_requested {
            self.set_status(
                StatusKind::Warn,
                tr!("Application is closing", "Приложение закрывается"),
            );
            return;
        }
        if self.phase == Phase::Setup {
            self.set_status(
                StatusKind::Err,
                tr!(
                    "Wait for tool installation to finish",
                    "Дождитесь установки загрузчиков"
                ),
            );
            return;
        }
        let engine = self.effective_engine();
        let url = match validate_url(&self.url) {
            Ok(u) => u,
            Err(e) => {
                self.set_status(StatusKind::Err, e);
                return;
            }
        };
        if self.out_dir.trim().is_empty() {
            self.set_status(
                StatusKind::Err,
                tr!("Choose a download folder", "Укажите папку сохранения"),
            );
            return;
        }
        if !self.link_ready() {
            self.set_status(
                StatusKind::Warn,
                tr!(
                    "Checking the download link…",
                    "Проверяю ссылку на загрузку…"
                ),
            );
            return;
        }
        let fmt = if engine == Engine::YtDlp {
            engines::ui_format_for_url(&url, self.fmt)
        } else {
            Format::Best
        };
        let cookies = if !self.use_cookies {
            None
        } else {
            match engine {
                Engine::YtDlp => Some(self.cookies_browser.clone()),
                Engine::Aria2 => None,
            }
        };
        if self.url_in_flight(&url) {
            self.set_status(
                StatusKind::Err,
                tr!(
                    "This link is already downloading, queued or paused",
                    "Эта ссылка уже скачивается, в очереди или на паузе"
                ),
            );
            return;
        }
        let download_name = if engine == Engine::Aria2 {
            self.resolved_link().and_then(|info| info.name.clone())
        } else {
            None
        };
        let job = Job {
            engine,
            url,
            out_dir: PathBuf::from(&self.out_dir),
            fmt,
            cookies_browser: cookies,
            download_name,
        };
        // Torrents resolve their file list first: the user ticks files in a
        // modal before the actual download starts. URL and stale offers are
        // left untouched until the pick ends (start_picked / cancel).
        if job.engine == Engine::Aria2 && engines::is_bittorrent_input(&job.url) {
            // The modal disables the form, so this is only a safety net: a
            // second pick must never replace the open one and its job.
            if self.torrent_pick.is_some() {
                self.set_status(
                    StatusKind::Err,
                    tr!(
                        "Finish selecting files for the open torrent first",
                        "Сначала завершите выбор файлов открытого торрента"
                    ),
                );
                return;
            }
            self.begin_torrent_pick(ctx, job);
            return;
        }
        // A failed-job offer is not an active job. If the user starts that
        // link manually, retire the stale offer; otherwise clicking its retry
        // button afterward could start a second writer on the same file.
        // Cleared once the job is valid and either starting or queued - not
        // on a validation error above, so a typo doesn't lose what's typed.
        let url = job.url.clone();
        if self.dispatch_job(ctx, job, torrent::Choice::default()) {
            self.retire_offers(&url);
            self.url.clear();
        }
    }

    /// Whether this link is already running, queued or paused.
    pub(super) fn url_in_flight(&self, url: &str) -> bool {
        self.jobs.iter().any(|j| j.job.url == url)
            || self.queue.iter().any(|j| j.job.url == url)
            || self.paused.iter().any(|p| p.job.url == url)
            || self.torrent_pick.as_ref().is_some_and(|p| p.job.url == url)
    }

    /// Final step of a torrent pick: start (or queue) the job with what the
    /// user chose. The pick can stay open for minutes, so duplicates are
    /// checked again here, and only now are the URL field and stale offers
    /// for this link consumed. Returns whether the job is running or queued.
    pub(super) fn start_picked(
        &mut self,
        ctx: &egui::Context,
        job: Job,
        choice: torrent::Choice,
    ) -> bool {
        if self.url_in_flight(&job.url) {
            self.set_status(
                StatusKind::Err,
                tr!(
                    "This link is already downloading, queued or paused",
                    "Эта ссылка уже скачивается, в очереди или на паузе"
                ),
            );
            return false;
        }
        // Before dispatch: a queued or failed start overrides it.
        self.set_status(
            StatusKind::None,
            tr!("Downloading torrent…", "Скачиваю торрент…"),
        );
        let url = job.url.clone();
        let started = self.dispatch_job(ctx, job, choice);
        if started {
            self.retire_offers(&url);
            if self.url.trim() == url {
                self.url.clear();
            }
        }
        started
    }

    /// Retain the fetched metadata and selection when a lock/spawn refuses
    /// this attempt, so retrying the modal never repeats a slow magnet fetch.
    pub(super) fn finish_torrent_pick(&mut self, ctx: &egui::Context, files: Option<Vec<usize>>) {
        if let Some(mut pick) = self.torrent_pick.take() {
            let choice = torrent::Choice {
                name: pick.info.as_ref().map(|info| info.name.clone()),
                files,
                meta: pick.meta.clone(),
            };
            if !self.start_picked(ctx, pick.job.clone(), choice) {
                pick.hint = Some(self.status.clone());
                self.torrent_pick = Some(pick);
            }
        }
    }

    /// Closes the pick modal without starting anything. A listing still in
    /// flight is told to stop, which kills its aria2c right away.
    pub(super) fn cancel_torrent_pick(&mut self) -> Option<Job> {
        let pick = self.torrent_pick.take()?;
        pick.cancel.store(true, Ordering::Relaxed);
        Some(pick.job)
    }

    /// Queue-or-start for jobs that are ready to run. Returns whether the job
    /// is now running or queued (false: spawn_job refused it).
    pub(super) fn dispatch_job(
        &mut self,
        ctx: &egui::Context,
        job: Job,
        select: torrent::Choice,
    ) -> bool {
        if self.close_requested {
            self.set_status(
                StatusKind::Warn,
                tr!("Application is closing", "Приложение закрывается"),
            );
            return false;
        }
        if self.url_in_flight(&job.url) {
            self.set_status(
                StatusKind::Warn,
                tr!(
                    "This link is already downloading, queued or awaiting file selection",
                    "Эта ссылка уже скачивается, ожидает или находится в выборе файлов"
                ),
            );
            return false;
        }
        if self.jobs.len() < MAX_CONCURRENT {
            self.spawn_job(ctx, job, false, select)
        } else {
            self.queue.push_back(QueuedJob {
                job,
                select,
                no_continue: false,
            });
            self.set_status(
                StatusKind::None,
                tr_format!(
                    "Added to queue ({})",
                    "Добавлено в очередь ({})",
                    self.queue.len()
                ),
            );
            true
        }
    }

    /// Fetch the torrent file list off-thread, then show the pick modal.
    pub(super) fn begin_torrent_pick(&mut self, ctx: &egui::Context, job: Job) {
        if self.close_requested || self.phase == Phase::Setup {
            self.set_status(
                StatusKind::Warn,
                tr!(
                    "Cannot start a download while closing or installing tools",
                    "Новая загрузка недоступна во время закрытия или установки"
                ),
            );
            return;
        }
        self.browser.open = false;
        self.show_dir_history = false;
        self.show_url_history = false;
        let id = self.next_pick_id;
        self.next_pick_id += 1;
        let aria2c = self.tc.aria2c.clone();
        let url = job.url.clone();
        let tx = self.tx.clone();
        let ctx_bg = ctx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_bg = cancel.clone();
        std::thread::spawn(move || {
            let result = match aria2c {
                Some(path) => {
                    torrent::show_files(&path, &url, torrent::METADATA_TIMEOUT, &cancel_bg).map(
                        |listing| {
                            let tree = torrent::build_torrent_tree(&listing.info.files);
                            (listing, tree)
                        },
                    )
                }
                None => Err(tr!(
                    "aria2c not found — install tools",
                    "aria2c не найден — установите загрузчики"
                )
                .to_string()),
            };
            let _ = tx.send(Msg::TorrentFiles(id, result));
            ctx_bg.request_repaint();
        });
        self.torrent_pick = Some(TorrentPick {
            id,
            job,
            cancel,
            info: None,
            meta: None,
            selected: Vec::new(),
            tree: Vec::new(),
            view: torrent::TreeView::default(),
            error: None,
            hint: None,
        });
        self.set_status(
            StatusKind::None,
            tr!(
                "Fetching torrent file list…",
                "Получаю список файлов торрента…"
            ),
        );
    }

    /// Spawns the loader for an already-validated job as a new concurrent
    /// entry in `self.jobs`. `no_continue` retries a resumed yt-dlp download
    /// from scratch: a stale/corrupt .part can't be validated by yt-dlp and
    /// poisons the run (Errno 22 mid-download or a merger "Invalid data"
    /// afterwards), so one clean retry heals it. Returns whether the job is
    /// now running (false: refused, with the reason already in the status).
    pub(super) fn spawn_job(
        &mut self,
        ctx: &egui::Context,
        job: Job,
        no_continue: bool,
        select: torrent::Choice,
    ) -> bool {
        // The close flow vetoes the window close until `jobs` is empty. A
        // spawn racing in from a still-clickable button right after × would
        // add an uncancelled process past cancel_all() and defer closing
        // until that download finishes.
        if self.close_requested {
            self.set_status(
                StatusKind::Warn,
                tr!(
                    "Application is closing — download was not started",
                    "Приложение закрывается — загрузка не запущена"
                ),
            );
            return false;
        }
        if self.phase == Phase::Setup {
            self.set_status(
                StatusKind::Err,
                tr!(
                    "Wait for tool installation to finish",
                    "Дождитесь установки загрузчиков"
                ),
            );
            return false;
        }
        if self.url_in_flight(&job.url) {
            self.set_status(
                StatusKind::Warn,
                tr!(
                    "This link is already downloading, queued or awaiting file selection",
                    "Эта ссылка уже скачивается, ожидает или находится в выборе файлов"
                ),
            );
            return false;
        }
        let label = engines::download_name(&job.url, &select);
        let extras = match extras_from_fields(
            self.subs && links::offers_subtitles(&job.url),
            &self.sub_langs,
            self.playlist,
            &self.extra_ytdlp,
            &self.extra_aria2,
        ) {
            Ok(extras) => extras,
            Err(e) => {
                self.set_status(StatusKind::Err, format!("«{label}»: {e}"));
                return false;
            }
        };
        // Everything below touches the download folder (probing for a partial
        // file, creating the folder, the lock records). On a disconnected
        // network drive those calls can block for tens of seconds, and this is
        // the UI thread: give them a few seconds, then report the folder.
        let prepared = {
            let (job, tc, select) = (job.clone(), self.tc.clone(), select.clone());
            snatch_rs::ui::run_bounded(FOLDER_IO_TIMEOUT, move || {
                let warns = preflight_warning(&job);
                let lock = engines::lock_aria2_target(&job, false)?;
                let cmd = engines::build_for_run_with(&job, &tc, &select, &extras)?;
                Ok::<_, String>((warns, lock, cmd))
            })
        };
        let (warns, output_lock, mut cmd) = match prepared {
            Some(Ok(parts)) => parts,
            Some(Err(e)) => {
                self.set_status(StatusKind::Err, format!("«{label}»: {e}"));
                return false;
            }
            None => {
                self.set_status(
                    StatusKind::Err,
                    tr_format!(
                        "“{label}”: the download folder does not respond — check the drive or choose another folder",
                        "«{label}»: папка загрузки не отвечает — проверьте диск или выберите другую папку"
                    ),
                );
                return false;
            }
        };

        if job.engine == Engine::YtDlp {
            engines::insert_option(&mut cmd, "--newline");
        } else {
            engines::insert_option(&mut cmd, "--summary-interval=1");
        }
        if no_continue && job.engine == Engine::YtDlp {
            engines::insert_option(&mut cmd, "--no-continue");
        }

        let install_guard = match snatch_rs::tools::try_lock_for_spawn(Path::new(&cmd[0])) {
            Ok(guard) => guard,
            Err(e) => {
                self.set_status(StatusKind::Err, e);
                return false;
            }
        };
        let mut command = Command::new(&cmd[0]);
        command
            .args(&cmd[1..])
            // Best-effort nudge for a *non-frozen* yt-dlp (e.g. one run as
            // `python -m yt_dlp` via SNATCH_YT_DLP): when its stdout/stderr is
            // piped rather than a real console, Python falls back to the ANSI
            // codepage (cp1251 on a Cyrillic Windows). NOTE: the official
            // yt-dlp.exe is a PyInstaller-frozen exe that IGNORES both of
            // these, so it emits cp1251 regardless - the real safeguard is the
            // lossy byte-reader on the pipes below, which tolerates those
            // non-UTF-8 bytes instead of abandoning (and closing) the pipe.
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // job_object::spawn(.., hidden = true) adds CREATE_NO_WINDOW itself:
        // yt-dlp/aria2c are console-subsystem exes, and this GUI has no
        // console, so without it every download would flash a CMD window.
        engines::prepend_bootstrap_path(&mut command);
        let (mut child, proc_job) = match job_object::spawn(&mut command, true) {
            Ok(result) => result,
            Err(e) => {
                self.set_status(
                    StatusKind::Err,
                    tr_format!(
                        "“{label}”: downloader startup/process protection: {e}",
                        "«{label}»: запуск/защита загрузчика: {e}"
                    ),
                );
                self.push_log(tr_format!(
                    "✘ Downloader stopped: {e}",
                    "✘ Загрузчик остановлен: {e}"
                ));
                return false;
            }
        };
        // Put the loader into a kill-on-close job object before it has
        // spawned children of its own (ffmpeg, aria2c-as-external-downloader)
        // - see mod job_object for the two orphan classes this closes.
        // Named proc_job: `job` is the engines::Job parameter below.
        drop(install_guard);
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let id = self.next_job_id;
        self.next_job_id += 1;
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let aria2 = job.engine == Engine::Aria2;
        self.jobs.push(ActiveJob {
            id,
            job: job.clone(),
            label,
            progress: None,
            status: tr!("Starting…", "Запускаю…").to_string(),
            status_kind: StatusKind::None,
            warns,
            cancel_flag: cancel_flag.clone(),
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
            no_continue,
            select,
        });

        let tx = self.tx.clone();
        let ctx_w = ctx.clone();
        let started = std::time::SystemTime::now();
        let stamped_job = job.clone();
        std::thread::spawn(move || {
            let tx_out = tx.clone();
            let h_out = std::thread::spawn(move || {
                // Read raw bytes and decode lossily - never `BufRead::lines()`.
                // The frozen yt-dlp.exe writes cp1251 (it ignores the
                // PYTHONIOENCODING/PYTHONUTF8 we set), so a title with an
                // em-dash reaches us as 0x97, which is invalid UTF-8. lines()
                // returns Err on it and the old `else { break }` dropped the
                // reader, closing the pipe; yt-dlp's next write then died with
                // `OSError: [Errno 22] Invalid argument` and aborted the
                // download. read_until + from_utf8_lossy keeps draining the
                // pipe no matter what bytes arrive.
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                // yt-dlp with --newline emits a progress line per downloaded
                // chunk - dozens per second on a fast link. Logging ALL of
                // them (a) evicted the meaningful lines ([info], Destination,
                // Merger, warnings) out of the 500-line cap within seconds of
                // a big download, and (b) invalidated the log-window text
                // every frame, forcing epaint to re-layout+re-tessellate the
                // whole 500-line galley on each new line. The progress BAR
                // still gets every tick (cheap, one widget); the text log and
                // status line get at most ~2 progress lines per second.
                // Non-progress lines always pass through immediately.
                let mut last_progress_log: Option<Instant> = None;
                let mut last_torrent_log: Option<Instant> = None;
                loop {
                    buf.clear();
                    match engines::read_line_capped(&mut reader, &mut buf, engines::MAX_CHILD_LINE)
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let decoded = engines::decode_child_bytes(&buf);
                    let line = engines::sanitize_child_output(&decoded);
                    if aria2 {
                        let mut latest = None;
                        // split, not replace: a chunk of aria2 output arrives
                        // with \r-separated fragments, and building a whole
                        // replaced copy of it per read is pure garbage.
                        for fragment in line.split(['\r', '\n']) {
                            if let Some(msg) = torrent_console_line(id, fragment) {
                                match msg {
                                    Msg::TorrentStatus(..) => latest = Some(msg),
                                    other => {
                                        if matches!(other, Msg::TorrentMeta(..)) {
                                            latest = None;
                                        }
                                        let _ = tx_out.send(other);
                                    }
                                }
                            }
                        }
                        if let Some(msg) = latest {
                            let due = last_progress_log
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(200));
                            if due {
                                if let Msg::TorrentStatus(_, _, ref stat) = msg {
                                    if last_torrent_log
                                        .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
                                    {
                                        last_torrent_log = Some(Instant::now());
                                        let _ = tx_out.send(Msg::TorrentLog(id, stat.clone()));
                                    }
                                }
                                last_progress_log = Some(Instant::now());
                                let _ = tx_out.send(msg);
                            }
                        }
                        continue;
                    }
                    match parse_progress(&line) {
                        Some(p) => {
                            let _ = tx_out.send(Msg::Progress(id, p));
                            let due = last_progress_log
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(500));
                            if due {
                                last_progress_log = Some(Instant::now());
                                let _ = tx_out.send(Msg::Log(id, line.to_string()));
                                // No request_repaint per line: drain() already
                                // ticks while jobs run, and a per-line request
                                // redraws the whole window - the stderr reader
                                // next door never did this either.
                            }
                        }
                        None => {
                            let _ = tx_out.send(Msg::Log(id, line.to_string()));
                        }
                    }
                }
            });
            let tx_err = tx.clone();
            let h_err = std::thread::spawn(move || {
                // Same lossy byte-read as stdout: stderr can carry the very
                // same non-UTF-8 cp1251 bytes inside error/warning text.
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match engines::read_line_capped(&mut reader, &mut buf, engines::MAX_CHILD_LINE)
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let line = engines::sanitize_child_output(&engines::decode_child_bytes(&buf))
                        .into_owned();
                    let _ = tx_err.send(Msg::ErrLine(id, line));
                    // Running already schedules repaint every 150ms in drain;
                    // per-line requests here would redraw the entire window
                    // for every noisy warning from aria2c.
                }
            });

            // Held (not used) until the thread ends: dropping the job closes
            // its last handle, and KILL_ON_JOB_CLOSE then reaps whatever of
            // the process tree is still alive (e.g. an ffmpeg left mid-merge
            // by a cancelled yt-dlp). If this process dies outright, the OS
            // closes the handle for us - same effect, no orphaned seeders.
            let proc_job = proc_job;
            let status = loop {
                if cancel_flag.load(Ordering::SeqCst) {
                    if let Some(j) = &proc_job {
                        j.terminate();
                    }
                    let _ = child.kill();
                    break child.wait();
                }
                match child.try_wait() {
                    Ok(Some(st)) => break Ok(st),
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(e) => break Err(e),
                }
            };
            let code = status.map(|s| s.code().unwrap_or(130)).unwrap_or(127);
            if code == 0 {
                // Windows' "downloaded from the Internet" mark, as a browser
                // adds it, while this download still owns its target name.
                engines::mark_direct_download(&stamped_job, started);
            }
            // F4 (audit 4): reap whatever is left of the tree BEFORE waiting
            // on the pipes. If the direct child died abnormally while a
            // grandchild (ffmpeg / external aria2c) still holds the inherited
            // handles, the joins below would block unbounded - and Done
            // already sent would have freed the GUI slot for a duplicate of
            // the same job. A normal exit leaves an empty tree, so this
            // terminate is a no-op there.
            if let Some(j) = &proc_job {
                j.terminate();
            }
            // aria2 has exited (and its tree is now stopped), so release its
            // basename before Done starts the next GUI queue entry for the
            // same destination.
            drop(output_lock);
            let _ = h_out.join();
            let _ = h_err.join();
            let _ = tx.send(Msg::Done(id, code));
            ctx_w.request_repaint();
        });
        true
    }
}
