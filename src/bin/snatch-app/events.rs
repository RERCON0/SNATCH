use super::*;

/// Messages handled per frame. A long playlist can queue thousands of lines
/// between two frames; working through all of them at once would stall the
/// window, so the rest waits for the next frame.
pub(super) const DRAIN_BUDGET: usize = 512;

impl SnatchApp {
    pub(super) fn drain(&mut self, ctx: &egui::Context) {
        let mut handled = 0;
        while handled < DRAIN_BUDGET {
            let Ok(msg) = self.rx.try_recv() else {
                break;
            };
            handled += 1;
            match msg {
                Msg::LinkResolved(url, info) => {
                    if self.link_in_flight.as_ref() == Some(&url) {
                        self.link_in_flight = None;
                    }
                    if url == engines::clean_url(&self.url) {
                        self.link_info = Some((url, info));
                    }
                }
                Msg::Log(id, line) => {
                    let trimmed = line.trim_end().to_string();
                    // Exact yt-dlp prefix, not a substring search anywhere in
                    // the line: an uploader-chosen title containing
                    // "Resuming download" (arriving inside a Destination line)
                    // used to arm the automatic from-scratch retry for any
                    // subsequent failure of that download.
                    if trimmed.starts_with("[download] Resuming download") {
                        if let Some(j) = self.job_mut(id) {
                            j.resumed_seen = true;
                        }
                    }
                    if let Some(title) = engines::ytdlp_title_from_line(&trimmed) {
                        if let Some(j) = self.job_mut(id) {
                            j.label = title;
                        }
                    }
                    if let Some(j) = self.job_mut(id) {
                        j.subs_watch.observe(&trimmed);
                        if engines::looks_like_corrupt_partial(&trimmed) {
                            j.corrupt_part_seen = true;
                        }
                        if engines::looks_like_aria2_refusal(&trimmed) {
                            j.host_refused = true;
                        }
                    }
                    if !trimmed.is_empty() {
                        // yt-dlp's own speed/ETA estimator occasionally comes
                        // up empty for one tick - especially under several
                        // concurrent downloads competing for I/O - and prints
                        // that tick as literally "Unknown B/s ETA Unknown"
                        // (confirmed in the CLI too, so this is yt-dlp's own
                        // output, not something this app generates or can
                        // fix). Just not updating the status line for that
                        // one tick keeps the last real speed/ETA on screen
                        // instead of flashing "Unknown" every second or two;
                        // the percentage keeps moving regardless (Progress is
                        // sent unconditionally, separately from this line).
                        if !trimmed.contains("Unknown B/s") {
                            if let Some(j) = self.job_mut(id) {
                                j.set_status(StatusKind::None, clip(&trimmed, 96));
                            }
                        }
                        self.push_job_log(id, trimmed);
                    }
                }
                Msg::ErrLine(id, line) => {
                    if looks_like_auth(&line) {
                        if let Some(j) = self.job_mut(id) {
                            j.auth_seen = true;
                        }
                    }
                    if engines::is_drm_protected(&line) {
                        if let Some(j) = self.job_mut(id) {
                            j.drm_seen = true;
                        }
                    }
                    if engines::looks_like_corrupt_partial(&line) {
                        if let Some(j) = self.job_mut(id) {
                            j.corrupt_part_seen = true;
                        }
                    }
                    if engines::looks_like_aria2_refusal(&line) {
                        if let Some(j) = self.job_mut(id) {
                            j.host_refused = true;
                        }
                    }
                    let trimmed = line.trim_end().to_string();
                    if engines::aria2_missing_control(&trimmed) {
                        let already = self.job_mut(id).is_some_and(|j| j.missing_control_file);
                        if !already {
                            if let Some(j) = self.job_mut(id) {
                                j.missing_control_file = true;
                            }
                            self.push_job_log(id, engines::aria2_missing_control_hint().to_owned());
                        }
                    } else if !trimmed.is_empty() {
                        let aria2_exception = trimmed.contains("Exception caught")
                            && self
                                .job_mut(id)
                                .is_some_and(|j| j.job.engine == Engine::Aria2);
                        if !aria2_exception {
                            self.push_job_log(id, trimmed);
                        }
                    }
                }
                Msg::Progress(id, p) => {
                    if let Some(j) = self.job_mut(id) {
                        j.progress = Some(p);
                        j.metadata_wait = false;
                    }
                }
                Msg::DirListed(requested, resolved, result) => {
                    self.browser.in_flight = None;
                    if self.browser.open && self.browser.current == requested {
                        self.browser.current = resolved.clone();
                        self.browser.listed_for = Some(resolved);
                        match result {
                            Ok((entries, truncated)) => {
                                self.browser.entries = entries;
                                self.browser.truncated = truncated;
                                self.browser.error = None;
                            }
                            Err(e) => {
                                self.browser.entries.clear();
                                self.browser.error = Some(e);
                            }
                        }
                    }
                }
                Msg::TorrentStatus(id, p, status) => {
                    if let Some(j) = self.job_mut(id) {
                        j.metadata_wait = false;
                        j.progress = Some(p);
                        j.set_status(StatusKind::None, status);
                    }
                }
                Msg::TorrentFiles(id, result) => {
                    let is_current = self.torrent_pick.as_ref().is_some_and(|p| p.id == id);
                    if is_current {
                        match result {
                            Ok((listing, tree)) => {
                                let single =
                                    listing.info.files.len() <= 1 && self.phase != Phase::Setup;
                                if let Some(pick) = self.torrent_pick.as_mut() {
                                    pick.selected = vec![true; listing.info.files.len()];
                                    pick.tree = tree;
                                    pick.view = torrent::TreeView::default();
                                    pick.info = Some(listing.info);
                                    pick.meta = listing.meta;
                                }
                                if single {
                                    self.finish_torrent_pick(ctx, None);
                                }
                            }
                            Err(e) => {
                                // The modal shows why and lets the user pick
                                // "скачать всё" or cancel - no silent switch.
                                if let Some(pick) = self.torrent_pick.as_mut() {
                                    pick.error = Some(e);
                                }
                            }
                        }
                    }
                }
                Msg::TorrentMeta(id, name) => {
                    if let Some(j) = self.job_mut(id) {
                        j.progress = None;
                        j.metadata_wait = true;
                    }
                    if !name.is_empty() {
                        self.push_job_log(id, tr_format!("Torrent: {name}", "Торрент: {name}"));
                        if let Some(j) = self.job_mut(id) {
                            j.label = name;
                        }
                    }
                    if let Some(j) = self.job_mut(id) {
                        j.set_status(
                            StatusKind::None,
                            tr!("Fetching torrent metadata…", "Получаю метаданные торрента…"),
                        );
                    }
                }
                Msg::TorrentName(id, name) => {
                    if let Some(j) = self.job_mut(id) {
                        if !engines::is_bittorrent_input(&j.job.url) {
                            j.label = name;
                        }
                    }
                }
                Msg::TorrentLog(id, line) => self.push_job_log(id, line),
                Msg::Done(id, code) => self.finish_job(id, code, ctx),
                Msg::SetupLog(line) => self.push_log(line),
                Msg::SetupProgress(progress) => {
                    self.setup_progress = Some(progress);
                }
                Msg::ToolsReady(tc) => {
                    apply_initial_discovery(&mut self.tc, self.setup_started, tc)
                }
                Msg::SetupDone(ok, tc) => {
                    self.setup_progress = None;
                    self.tc = tc;
                    // Only leave Setup from Setup: an unconditional Idle here
                    // could re-enable the CTA on top of an already-running
                    // job (the retry button used to make that reachable).
                    if self.phase == Phase::Setup {
                        self.phase = Phase::Idle;
                    }
                    if ok {
                        self.set_status(
                            StatusKind::Ok,
                            tr!("Tools installed", "Загрузчики установлены"),
                        );
                    } else {
                        self.set_status(
                            StatusKind::Err,
                            tr!(
                                "Some tools could not be installed — see log",
                                "Не всё удалось установить — см. журнал"
                            ),
                        );
                        self.show_log_window = true;
                    }
                    self.fill_free_slots(ctx);
                }
            }
        }
        if handled == DRAIN_BUDGET {
            // There may be more: draw this frame, then continue straight away.
            ctx.request_repaint();
        }
        if self.phase == Phase::Setup || !self.jobs.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        // The window close was vetoed while jobs were being killed; now that
        // everything's actually finished, close for real.
        if self.close_requested && self.phase == Phase::Idle && self.jobs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// One specific job (by id) finished - success, failure or cancelled.
    /// Concurrent siblings in `self.jobs` are untouched.
    pub(super) fn finish_job(&mut self, id: u64, code: i32, ctx: &egui::Context) {
        let Some(pos) = self.jobs.iter().position(|j| j.id == id) else {
            // Already removed (e.g. a duplicate Done somehow) - nothing to do.
            return;
        };
        let aj = self.jobs.remove(pos);
        let label = clip(&aj.label, 60);

        // Cancel that lost the race with a completed download still gets the
        // "Готово" branch: the file IS complete, claiming "отменено, можно
        // докачать" would send the user to re-download a finished file.
        if aj.cancelled && code != 0 {
            let text = tr_format!(
                "“{label}” canceled (download can be resumed)",
                "«{label}» отменено (файл можно докачать)"
            );
            self.set_status(StatusKind::Warn, text);
            self.fill_free_slots(ctx);
            return;
        }
        // Same "still finished on its own" race as cancel above: if it hit
        // code 0 right as pause was requested, treat it as done, not paused.
        if aj.pausing && code != 0 {
            self.set_status(
                StatusKind::None,
                tr_format!("“{label}” paused", "«{label}» на паузе"),
            );
            self.paused.push(PausedJob {
                job: aj.job.clone(),
                label,
                progress: aj.progress,
                status: aj.status,
                select: aj.select,
                // A job restarted from scratch stays that way when resumed.
                no_continue: aj.no_continue,
            });
            self.fill_free_slots(ctx);
            return;
        }
        if code == 13 && aj.missing_control_file {
            self.set_status(
                StatusKind::Err,
                tr_format!(
                    "“{label}”: files exist without .aria2 — choose a new empty folder",
                    "«{label}»: файлы уже есть без .aria2 — выберите новую пустую папку"
                ),
            );
            self.show_log_window = true;
            self.fill_free_slots(ctx);
            return;
        }
        match code {
            0 if aj.subs_watch.nothing_written(aj.job.fmt, 0) => {
                // yt-dlp reports "no subtitles for the requested languages"
                // as success; for the subtitles-only format an empty folder
                // is a failure, and the link must not enter history as done.
                self.set_status(
                    StatusKind::Err,
                    tr_format!(
                        "“{label}”: no subtitles for the requested languages — nothing was written",
                        "«{label}»: подходящих субтитров нет — ничего не записано"
                    ),
                );
                self.push_job_log(
                    aj.id,
                    tr!(
                        "yt-dlp found no subtitles for the requested languages; the video itself is never downloaded in subs mode.",
                        "yt-dlp не нашёл субтитров на запрошенных языках; само видео в режиме «только субтитры» не скачивается."
                    )
                    .to_string(),
                );
                self.show_log_window = true;
            }
            0 => {
                self.cfg.remember_url(&aj.job.url);
                self.cfg.remember_dir(&aj.job.out_dir.to_string_lossy());
                let persisted = self.save_config();
                let mut text = tr_format!(
                    "“{label}” done: {}",
                    "«{label}» готово: {}",
                    aj.job.out_dir.display()
                );
                if aj.select.files.is_some() {
                    // aria2 writes whole pieces: unselected neighbours of the
                    // chosen files can exist yet be incomplete.
                    text.push_str(tr!(
                        " · unselected neighboring files may be incomplete",
                        " · соседние невыбранные файлы могут быть неполными"
                    ));
                }
                if !persisted {
                    text.push_str(tr!(
                        " · history was not saved — see log",
                        " · история не сохранена — см. журнал"
                    ));
                }
                self.set_status(
                    if persisted {
                        StatusKind::Ok
                    } else {
                        StatusKind::Warn
                    },
                    text,
                );
            }
            127 => self.set_status(
                StatusKind::Err,
                tr_format!(
                    "“{label}”: could not start downloader (executable is missing or cannot run)",
                    "«{label}»: не удалось запустить загрузчик (бинарник пропал или не исполняем)"
                ),
            ),
            130 => self.set_status(
                StatusKind::Warn,
                tr_format!(
                    "“{label}” interrupted (download can be resumed)",
                    "«{label}» прервано (файл можно докачать)"
                ),
            ),
            c => {
                if self.retry_with_ytdlp_after_refusal(ctx, &aj, &label) {
                    self.fill_free_slots(ctx);
                    return;
                }
                if aj.drm_seen {
                    // A DRM failure reads "error (code 1)" like any other, but
                    // cookies cannot fix encryption - say what it is instead.
                    self.set_status(
                        StatusKind::Err,
                        tr_format!(
                            "“{label}”: {} — see log",
                            "«{label}»: {} — см. журнал",
                            engines::drm_protected_short()
                        ),
                    );
                    self.push_job_log(aj.id, engines::drm_protected_note().to_string());
                } else {
                    self.set_status(
                        StatusKind::Err,
                        tr_format!(
                            "“{label}”: error (code {c}) — see log",
                            "«{label}»: ошибка (код {c}) — см. журнал"
                        ),
                    );
                }
                self.show_log_window = true;
                let offer_retry = c == 1 && aj.auth_seen && aj.job.engine == Engine::YtDlp;
                // The run resumed a .part and still failed: the old part is
                // unusable (stale range / corrupt bytes), so retry the same
                // job once from scratch instead of making the user click.
                let retry_from_scratch = aj.resumed_seen
                    && aj.corrupt_part_seen
                    && !aj.no_continue
                    && !offer_retry
                    && aj.job.engine == Engine::YtDlp;
                if offer_retry {
                    // Only the most recent auth failure offers a retry - a
                    // stacked "3 jobs want cookies" list would need its own
                    // per-job UI for one-shot, rarely-needed friction.
                    self.retry_offer = Some(aj.job.clone());
                } else if retry_from_scratch {
                    if self.spawn_job(ctx, aj.job.clone(), true, aj.select.clone()) {
                        self.set_status(
                            StatusKind::Warn,
                            tr_format!(
                                "“{label}”: resumed file is corrupt — restarting from scratch",
                                "«{label}»: докачка дала битый файл — повторяю с начала"
                            ),
                        );
                    } else {
                        self.retain_failed_start(aj.job, aj.select, true);
                    }
                }
            }
        }
        self.fill_free_slots(ctx);
    }

    /// Some hosts turn aria2c away: its Windows TLS client cannot finish a
    /// handshake with them (Meta's CDN among them) or its fingerprint is
    /// answered with HTTP 403/429; yt-dlp downloads the same direct link with
    /// OpenSSL. One automatic retry, only for a plain link and only when yt-dlp
    /// is installed. Returns whether the retry started.
    pub(super) fn retry_with_ytdlp_after_refusal(
        &mut self,
        ctx: &egui::Context,
        aj: &ActiveJob,
        label: &str,
    ) -> bool {
        if aj.job.engine != Engine::Aria2
            || !aj.host_refused
            || self.tc.yt_dlp.is_none()
            || engines::is_bittorrent_input(&aj.job.url)
            || !engines::discard_failed_aria2_attempt(&aj.job, aj.started_at)
        {
            return false;
        }
        // The host is remembered so yt-dlp downloads the file itself instead of
        // passing it back to aria2c.
        links::remember_aria2_refused(&aj.job.url);
        // The resolved file name stays: it keeps the pinned `-o`, the foreign-pair
        // check and the Internet mark on the file the aria2 attempt was aiming at.
        let job = Job {
            engine: Engine::YtDlp,
            fmt: Format::Best,
            ..aj.job.clone()
        };
        if !self.spawn_job(ctx, job, false, torrent::Choice::default()) {
            return false;
        }
        self.set_status(
            StatusKind::Warn,
            tr_format!(
                "“{label}”: the host turned aria2c away — retrying with yt-dlp",
                "«{label}»: сайт отклонил aria2c — повторяю через yt-dlp"
            ),
        );
        true
    }

    pub(super) fn retain_failed_start(
        &mut self,
        job: Job,
        select: torrent::Choice,
        no_continue: bool,
    ) {
        self.paused.push(PausedJob {
            label: engines::download_name(&job.url, &select),
            job,
            progress: None,
            status: self.status.clone(),
            select,
            no_continue,
        });
    }

    pub(super) fn retire_offers(&mut self, url: &str) {
        if self.retry_offer.as_ref().is_some_and(|job| job.url == url) {
            self.retry_offer = None;
        }
    }

    pub(super) fn retry_with_cookies(&mut self, ctx: &egui::Context) {
        let Some(mut job) = self.retry_offer.clone() else {
            return;
        };
        job.cookies_browser = Some(self.cookies_browser.clone());
        if self.dispatch_job(ctx, job, torrent::Choice::default()) {
            self.retry_offer = None;
        }
    }

    /// Starts queued jobs until MAX_CONCURRENT is reached or the queue runs
    /// out. Called after any job finishes, freeing a slot.
    pub(super) fn fill_free_slots(&mut self, ctx: &egui::Context) {
        if self.close_requested || self.phase == Phase::Setup {
            return;
        }
        while self.jobs.len() < MAX_CONCURRENT {
            let Some(queued) = self.queue.pop_front() else {
                break;
            };
            if !self.spawn_job(
                ctx,
                queued.job.clone(),
                queued.no_continue,
                queued.select.clone(),
            ) {
                self.retain_failed_start(queued.job, queued.select, queued.no_continue);
            }
        }
    }
}
