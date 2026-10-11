use super::*;

#[derive(PartialEq, Clone, Copy)]
pub(super) enum EngineMode {
    Auto,
    YtDlp,
    Aria2,
}

/// Only tracks the installer now - "is a download running" is
/// `!self.jobs.is_empty()`, since there can be several at once.
#[derive(PartialEq, Clone, Copy)]
pub(super) enum Phase {
    Idle,
    Setup,
}

#[derive(Clone, Copy)]
pub(super) enum StatusKind {
    None,
    Ok,
    Warn,
    Err,
}

/// Every per-job variant is tagged with that job's id so drain() can route
/// it to the right entry in `self.jobs` - concurrent downloads share the one
/// channel. Setup* /ToolsReady stay untagged: the installer is a single
/// global operation, not a job.
pub(super) enum Msg {
    LinkResolved(String, LinkInfo),
    Log(u64, String),
    ErrLine(u64, String),
    Progress(u64, f32),
    TorrentStatus(u64, f32, String),
    /// File list resolved for a torrent job (metadata fetched off-thread).
    /// The tree is built on the listing thread: a huge/hostile torrent must
    /// not stall (or overflow) the UI thread.
    TorrentFiles(
        u64,
        Result<(torrent::Listing, Vec<torrent::TorrNode>), String>,
    ),
    DirListed(PathBuf, PathBuf, Result<(Vec<PathBuf>, bool), String>),
    TorrentMeta(u64, String),
    TorrentName(u64, String),
    TorrentLog(u64, String),
    Done(u64, i32),
    SetupLog(String),
    /// Live installer progress: asset label, bytes done, total if known.
    SetupProgress(SetupProgress),
    /// (успех, обновлённая цепочка инструментов - discover делается в том же
    /// фоновом потоке, а не на UI-потоке)
    SetupDone(bool, Toolchain),
    ToolsReady(Toolchain),
}

/// One running or just-finished-and-being-retried download. Everything that
/// used to be a scalar field on SnatchApp (progress, torrent_name, per-run
/// auth_seen/resumed_seen/...) now lives here instead, one per concurrent job.
pub(super) struct ActiveJob {
    pub(super) id: u64,
    pub(super) job: Job,
    /// Torrent name once known, else the URL clipped - used for the row
    /// header and to prefix this job's lines in the shared log.
    pub(super) label: String,
    pub(super) progress: Option<f32>,
    pub(super) status: String,
    pub(super) status_kind: StatusKind,
    pub(super) warns: Vec<String>,
    pub(super) cancel_flag: Arc<AtomicBool>,
    pub(super) cancelled: bool,
    /// Set by the "пауза" button - kills the process the same way `cancelled`
    /// does, but finish_job() moves the job into `SnatchApp::paused` instead
    /// of dropping it, so one click on "продолжить" respawns it. Works
    /// because yt-dlp/aria2c already continue an interrupted download from
    /// its own partial file (see resumed_seen/no_continue below); pausing
    /// doesn't need any extra IPC to the child, just an ordinary kill+restart.
    pub(super) pausing: bool,
    pub(super) auth_seen: bool,
    /// The run failed on DRM-protected streams ([`engines::is_drm_protected`]):
    /// the failure gets its own explanation, and no cookies retry is offered
    /// because cookies cannot remove DRM.
    pub(super) drm_seen: bool,
    /// Subtitles-only runs: did any subtitle file appear, and did yt-dlp
    /// say none of the requested languages exist (it still exits 0)?
    pub(super) subs_watch: engines::SubtitleWatch,
    pub(super) missing_control_file: bool,
    pub(super) resumed_seen: bool,
    /// yt-dlp/ffmpeg said the partial file it resumed is unusable
    /// ([`engines::looks_like_corrupt_partial`]). Only that justifies the
    /// automatic restart from scratch; any other failure keeps the partial.
    pub(super) corrupt_part_seen: bool,
    /// Waiting for a torrent's metadata (drives the "Torrent metadata…"
    /// caption). A flag, not a comparison of the localized status text.
    pub(super) metadata_wait: bool,
    /// aria2c reported a TLS handshake failure: its Windows TLS client cannot
    /// talk to this host, and yt-dlp usually can.
    pub(super) host_refused: bool,
    /// When this run was started: what it left in the folder (and only that)
    /// may be cleared if the host turns aria2c away.
    pub(super) started_at: std::time::SystemTime,
    /// Whether *this* run already used --no-continue (a from-scratch retry
    /// of a corrupt resume) - distinct from resumed_seen, which is whether
    /// yt-dlp printed "Resuming download" during this run.
    pub(super) no_continue: bool,
    /// Torrent file selection + fetched metadata, kept so a paused job
    /// resumes with the same selection (default for every other job).
    pub(super) select: torrent::Choice,
}

impl ActiveJob {
    pub(super) fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_kind = kind;
        self.status = text.into();
    }
}

/// A job stopped via "пауза" instead of "отмена" - just enough to redisplay
/// it and respawn it unchanged when the user clicks "продолжить".
#[derive(Clone)]
pub(super) struct PausedJob {
    pub(super) job: Job,
    pub(super) label: String,
    pub(super) progress: Option<f32>,
    pub(super) status: String,
    pub(super) select: torrent::Choice,
    pub(super) no_continue: bool,
}

#[derive(Clone)]
pub(super) struct QueuedJob {
    pub(super) job: Job,
    pub(super) select: torrent::Choice,
    pub(super) no_continue: bool,
}

/// A torrent job waiting in the file-pick modal: first while its file list
/// is fetched off-thread, then while the user ticks the files.
pub(super) struct TorrentPick {
    pub(super) id: u64,
    pub(super) job: Job,
    /// Set on cancel/close: the listing thread kills its aria2c at once
    /// instead of fetching metadata nobody will use.
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) info: Option<torrent::TorrentInfo>,
    /// Metadata fetched for a magnet/http(s) input; the download reuses it.
    pub(super) meta: Option<torrent::Meta>,
    pub(super) selected: Vec<bool>,
    pub(super) tree: Vec<torrent::TorrNode>,
    pub(super) view: torrent::TreeView,
    /// The listing failed: the modal says why and offers the whole torrent
    /// instead of silently switching to it.
    pub(super) error: Option<String>,
    /// Why "скачать" was refused (a selection too scattered to pass).
    pub(super) hint: Option<String>,
}

impl TorrentPick {
    /// Still fetching the file list: the form stays usable for running jobs
    /// and the run row shows a cancel; the modal only opens once there is a
    /// list (or an error) to act on.
    pub(super) fn loading(&self) -> bool {
        self.info.is_none() && self.error.is_none()
    }
}

/// What a frame of the pick modal asked for.
#[derive(Default)]
pub(super) struct PickActions {
    pub(super) confirm: bool,
    pub(super) cancel: bool,
    pub(super) all: bool,
    pub(super) none: bool,
    pub(super) download_all: bool,
    /// Rects of the dialog's buttons this frame - the layout test checks
    /// they stay inside the dialog and never overlap.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) buttons: Vec<egui::Rect>,
}

pub(super) struct DirBrowser {
    pub(super) open: bool,
    pub(super) current: PathBuf,
    pub(super) entries: Vec<PathBuf>,
    /// The last listing hit `DIR_BROWSER_SCAN_CAP`: more entries may exist.
    pub(super) truncated: bool,
    pub(super) listed_for: Option<PathBuf>,
    pub(super) in_flight: Option<PathBuf>,
    pub(super) resolve_initial: bool,
    pub(super) error: Option<String>,
}

pub(super) fn torrent_console_line(id: u64, line: &str) -> Option<Msg> {
    let line = line.trim();
    if line.is_empty()
        || line.contains("Download Progress Summary as of")
        || line.chars().all(|c| matches!(c, '=' | '-'))
    {
        return None;
    }
    if let Some(name) = engines::aria2_metadata_name(line) {
        return Some(Msg::TorrentMeta(id, name));
    }
    if let Some(name) = engines::aria2_name_from_file(line) {
        return Some(Msg::TorrentName(id, name));
    }
    if let (Some(p), Some(stat)) = (parse_progress(line), engines::aria2_stat(line)) {
        return Some(Msg::TorrentStatus(id, p, stat));
    }
    if line.contains("Allocating disk space") {
        return Some(Msg::TorrentLog(
            id,
            tr!("Allocating disk space…", "Выделение места на диске…").into(),
        ));
    }
    let interested = ["error", "warning", "failed", "aborted", "exception"]
        .iter()
        .any(|hint| engines::contains_ignore_ascii_case(line, hint));
    if interested {
        return Some(Msg::ErrLine(id, line.to_string()));
    }
    None
}

pub(super) fn local_torrent_path(path: &Path) -> Result<String, String> {
    let supported = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["torrent", "metalink", "meta4"]
                .iter()
                .any(|allowed| ext.eq_ignore_ascii_case(allowed))
        });
    if !supported {
        return Err(tr!(
            "Choose a local .torrent, .metalink or .meta4 file.",
            "Выберите локальный .torrent, .metalink или .meta4 файл."
        )
        .into());
    }
    // Validation rejects UNC before touching the filesystem; spaces and
    // brackets in a local filename remain ordinary path characters.
    validate_url(&path.to_string_lossy())
}

pub(super) fn apply_initial_discovery(
    current: &mut Toolchain,
    setup_started: bool,
    discovered: Toolchain,
) {
    if !setup_started {
        *current = discovered;
    }
}
