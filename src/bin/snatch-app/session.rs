use super::*;

impl SnatchApp {
    pub(super) fn start_setup(&mut self, ctx: &egui::Context) {
        self.setup_started = true;
        self.phase = Phase::Setup;
        self.setup_progress = None;
        // Keep refused jobs/offers available after installation; spawn_job's
        // Setup guard prevents the old overlapping-installer retry race.
        self.clear_log();
        self.set_status(
            StatusKind::None,
            tr!("Installing tools…", "Устанавливаю загрузчики…"),
        );
        let tx = self.tx.clone();
        let ctx_w = ctx.clone();
        std::thread::spawn(move || {
            let tx_prog = tx.clone();
            let mut report = move |p: SetupProgress| {
                let _ = tx_prog.send(Msg::SetupProgress(p));
            };
            let bin = match bootstrap_dir() {
                Ok(bin) => bin,
                Err(e) => {
                    let _ = tx.send(Msg::SetupLog(e));
                    let _ = tx.send(Msg::SetupDone(false, Toolchain::discover()));
                    ctx_w.request_repaint();
                    return;
                }
            };
            let mut ok = std::fs::create_dir_all(&bin).is_ok();
            if ok {
                let _ = tx.send(Msg::SetupLog(tr_format!(
                    "Installing tools in {}",
                    "Устанавливаю загрузчики в {}",
                    bin.display()
                )));
                match install_yt_dlp_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("yt-dlp: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(tr_format!(
                            "yt-dlp: ERROR {e}",
                            "yt-dlp: ОШИБКА {e}"
                        )));
                        ok = false;
                    }
                }
                match install_aria2_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("aria2c: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(tr_format!(
                            "aria2c: ERROR {e}",
                            "aria2c: ОШИБКА {e}"
                        )));
                        ok = false;
                    }
                }
                match install_ffmpeg_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("ffmpeg: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(tr_format!(
                            "ffmpeg: ERROR {e}",
                            "ffmpeg: ОШИБКА {e}"
                        )));
                        ok = false;
                    }
                }
                match install_deno_with(&bin, &mut report) {
                    Ok(p) => {
                        let _ = tx.send(Msg::SetupLog(format!("Deno: {}", p.display())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::SetupLog(tr_format!(
                            "Deno: ERROR {e}",
                            "Deno: ОШИБКА {e}"
                        )));
                        ok = false;
                    }
                }
            } else {
                let _ = tx.send(Msg::SetupLog(
                    tr!(
                        "Could not create the bin folder",
                        "Не удалось создать папку bin"
                    )
                    .to_string(),
                ));
            }
            // Re-discover in THIS thread: Toolchain::discover() walks PATH and
            // the WinGet dirs, and doing that on the UI thread froze the
            // window for as long as the scan took (dead network PATH entries:
            // seconds).
            let tc = Toolchain::discover();
            let _ = tx.send(Msg::SetupDone(ok, tc));
            ctx_w.request_repaint();
        });
    }

    /// Cancels exactly one active job - picking the wrong one among several
    /// running at once was the whole point of moving this off a single
    /// shared flag.
    pub(super) fn cancel_job(&mut self, id: u64) {
        if let Some(j) = self.job_mut(id) {
            j.cancelled = true;
            j.cancel_flag.store(true, Ordering::SeqCst);
            let label = j.label.clone();
            self.set_status(
                StatusKind::Warn,
                tr_format!("“{}”: stopping…", "«{}»: останавливаю…", clip(&label, 60)),
            );
        }
    }

    /// Pauses exactly one active job: kills its process the same way cancel
    /// does, but finish_job() sees `pausing` and keeps the job around in
    /// `self.paused` (with a "продолжить" button) instead of dropping it -
    /// see the field doc on ActiveJob::pausing for why no extra IPC to the
    /// child is needed.
    pub(super) fn pause_job(&mut self, id: u64) {
        if let Some(j) = self.job_mut(id) {
            j.pausing = true;
            j.cancel_flag.store(true, Ordering::SeqCst);
            let label = j.label.clone();
            self.set_status(
                StatusKind::None,
                tr_format!("“{}”: pausing…", "«{}»: ставлю на паузу…", clip(&label, 60)),
            );
        }
    }

    /// Cancels every active job - used when the whole app is closing, not
    /// from the per-job cancel button.
    pub(super) fn cancel_all(&mut self) {
        for j in &mut self.jobs {
            j.cancelled = true;
            j.cancel_flag.store(true, Ordering::SeqCst);
        }
        self.set_status(StatusKind::Warn, tr!("Stopping…", "Останавливаю…"));
    }

    pub(super) fn found_tools_line(&self) -> String {
        let mut found = Vec::new();
        if self.tc.yt_dlp.is_some() {
            found.push("yt-dlp");
        }
        if self.tc.aria2c.is_some() {
            found.push("aria2c");
        }
        if found.is_empty() {
            String::new()
        } else {
            tr_format!("$ found: {}", "$ найдены: {}", found.join(", "))
        }
    }
}
