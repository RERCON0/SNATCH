use super::*;

pub(super) enum BatchEvent {
    Line(usize, String),
    Name(usize, String),
    TorrentName(usize, String),
    Progress(usize, String),
    Finished(usize, String, String, RunResult),
}

pub(super) fn clean_loader_text(text: &str) -> std::borrow::Cow<'_, str> {
    let cleaned = engines::sanitize_child_output(text);
    if cleaned.contains('\r') {
        std::borrow::Cow::Owned(cleaned.replace('\r', "\n"))
    } else {
        cleaned
    }
}

pub(super) struct BatchView {
    pub(super) name: String,
    pub(super) status: String,
    pub(super) last_printed: Option<Instant>,
}

pub(super) struct BatchDisplay {
    pub(super) multi: Option<MultiProgress>,
    pub(super) bars: Vec<ProgressBar>,
    pub(super) views: Vec<BatchView>,
    pub(super) colors: bool,
    pub(super) ticking: Vec<bool>,
}

pub(super) fn highlight_progress(status: &str) -> String {
    const GREEN: &str = "\x1b[92m";
    const CYAN: &str = "\x1b[96m";
    const RESET: &str = "\x1b[0m";
    if status.starts_with("[download]") {
        let mut out = status.to_string();
        if let Some(at) = out.find(" at ") {
            let start = at + 4;
            let end = out[start..].find(' ').map_or(out.len(), |i| start + i);
            out.insert_str(end, RESET);
            out.insert_str(start, CYAN);
        }
        if let Some(end) = out.find('%') {
            // Advance past the WHOLE whitespace char: for a multi-byte
            // whitespace (e.g. NNBSP) `i + 1` is not a char boundary and
            // insert_str would panic.
            let start = out[..end]
                .rfind(char::is_whitespace)
                .map_or(0, |i| i + out[i..].chars().next().map_or(1, char::len_utf8));
            out.insert_str(end + 1, RESET);
            out.insert_str(start, GREEN);
        }
        return out;
    }
    let mut fields = status.split(" · ");
    let Some(percent) = fields.next() else {
        return status.to_string();
    };
    if !percent.ends_with('%')
        || !percent[..percent.len() - 1]
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
    {
        return status.to_string();
    }
    let mut out = format!("{GREEN}{percent}{RESET}");
    for field in fields {
        out.push_str(" · ");
        if field.starts_with('↓') {
            out.push_str(&format!("{CYAN}{field}{RESET}"));
            continue;
        }
        if let Some((downloaded, total)) = field.split_once('/') {
            if !downloaded.contains(' ') {
                out.push_str(&format!("{GREEN}{downloaded}{RESET}/{total}"));
                continue;
            }
        }
        out.push_str(field);
    }
    out
}

impl BatchDisplay {
    pub(super) fn new(plans: &[Plan]) -> Self {
        let terminal = std::io::stdout().is_terminal();
        let colors = terminal && vt_processing_enabled();
        let multi = terminal.then(|| MultiProgress::with_draw_target(ProgressDrawTarget::stdout()));
        let mut bars = Vec::new();
        let mut views = Vec::new();
        for (id, plan) in plans.iter().enumerate() {
            views.push(BatchView {
                name: engines::download_name(&plan.url, &plan.torrent),
                status: tr!("queued", "в очереди").into(),
                last_printed: None,
            });
            if let Some(multi) = &multi {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_style(
                    ProgressStyle::with_template("{spinner:.green} {wide_msg}")
                        .expect("valid style"),
                );
                bar.set_message(Self::line(id, plans.len(), &views[id], colors));
                bars.push(bar);
            } else {
                outln(Self::line(id, plans.len(), &views[id], false));
            }
        }
        Self {
            multi,
            bars,
            views,
            colors,
            ticking: vec![false; plans.len()],
        }
    }

    pub(super) fn line(id: usize, total: usize, view: &BatchView, colors: bool) -> String {
        let status = if colors {
            highlight_progress(&view.status)
        } else {
            view.status.clone()
        };
        format!(
            "[{} / {}] {} — {}",
            id + 1,
            total,
            clip(&view.name, 38),
            status
        )
    }

    pub(super) fn update(&mut self, id: usize, status: Option<String>, name: Option<String>) {
        let total = self.views.len();
        let view = &mut self.views[id];
        if let Some(name) = name {
            view.name = name;
        }
        if let Some(status) = status {
            // The generic allocation notice can arrive from stderr after a
            // more useful byte counter on stdout; don't overwrite the latter.
            if status != tr!("allocating disk space…", "выделение места на диске…")
                || !view
                    .status
                    .contains(tr!("allocating space ", "выделение места "))
            {
                view.status = status;
            }
        }
        let line = Self::line(id, total, view, self.colors);
        if self.multi.is_some() {
            if !self.ticking[id] {
                self.bars[id].enable_steady_tick(Duration::from_millis(120));
                self.ticking[id] = true;
            }
            self.bars[id].set_message(line);
        } else if view
            .last_printed
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
        {
            outln(line);
            view.last_printed = Some(Instant::now());
        }
    }

    pub(super) fn diagnostic(&self, id: usize, text: &str) {
        // Network-sourced metadata (torrent listings, resolved names) never
        // goes through a child pipe. Never let it inject terminal escapes.
        let safe = engines::sanitize_child_output(text);
        let line = format!(
            "[{} / {}] {}: {safe}",
            id + 1,
            self.views.len(),
            clip(&self.views[id].name, 38)
        );
        if let Some(multi) = &self.multi {
            let _ = multi.println(line);
        } else {
            outln(line);
        }
    }

    pub(super) fn finished(&mut self, id: usize, status: String) {
        self.views[id].status = status;
        let line = Self::line(id, self.views.len(), &self.views[id], self.colors);
        if self.multi.is_some() {
            self.bars[id].disable_steady_tick();
            self.ticking[id] = false;
            self.bars[id].finish_with_message(line);
        } else {
            outln(line);
        }
    }
}
