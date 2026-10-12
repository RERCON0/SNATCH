use super::*;

/// "Terminal Native" theme (the approved mockup):
/// monospace everywhere, thin 1px hairlines instead of thick borders, ghost
/// buttons (border only, no fill), and one accent color used sparingly.
///
/// The mockup's stack (`ui-monospace,"Cascadia Code","Consolas",monospace`)
/// resolves to Cascadia Mono in the browser, and ClearType stem-snaps its
/// Regular stems to ~1px, which is the thin look that got approved. egui has
/// no hinting/subpixel AA: the same Regular (wght 400, the default instance
/// of the variable ttf) rasterizes with 2px blurry stems and reads bold next
/// to the HTML. Cascadia Mono is a variable font (wght 200-700), so
/// fonts/CascadiaMono-Light.ttf is the wght=300 instance baked to a
/// static ttf with fontTools - its thinner stems land at the mockup's visual
/// weight once egui's AA spreads them. Same family, same metrics, so nothing
/// else about the layout moves.
///
/// A system mono as the *only* font (Proportional and Monospace both) is what
/// caused the earlier "text sits above center" bug: its ascent/descent ratio
/// differs from the bundled font egui's centering math assumes. Fixed via
/// `FontTweak::y_offset_factor` instead of giving up on a monospace UI - it
/// nudges glyph *rendering* down without touching the row-height egui uses
/// to center the widget, so layout stays correct. MONO_Y_OFFSET is a guess
/// (a fraction of the font size); if text still reads high/low after
/// building, adjust it up/down and rebuild - nothing else needs to change.
pub(super) const MONO_Y_OFFSET: f32 = 0.15;

// SIL OFL 1.1 (c) Microsoft Corporation, see fonts/OFL-notice.txt
pub(super) const BUNDLED_MONO: &[u8] = include_bytes!("../../../fonts/CascadiaMono-Light.ttf");

pub(super) fn setup_theme(ctx: &egui::Context, dark: bool) {
    let mut fonts = egui::FontDefinitions::default();
    let tweak = egui::FontTweak {
        y_offset_factor: MONO_Y_OFFSET,
        ..Default::default()
    };
    let data = egui::FontData::from_static(BUNDLED_MONO).tweak(tweak);
    fonts
        .font_data
        .insert("mono-system".to_owned(), data.into());
    for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.insert(0, "mono-system".to_owned());
        }
    }
    // Buttons and title bars need a higher baseline than the body text. Keep
    // identical font metrics; only shift painted glyphs so borders and input
    // rows remain aligned while popup captions clear the separator below.
    fonts.font_data.insert(
        "mono-button".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO)
            .tweak(egui::FontTweak {
                y_offset_factor: 0.0,
                ..Default::default()
            })
            .into(),
    );
    fonts.families.insert(
        egui::FontFamily::Name("button".into()),
        vec!["mono-button".to_owned()],
    );
    // TextEdit's inner text: the boxed controls' no-nudge rendering. Body's
    // +0.15em glyph shift (below) reads fine in free-standing labels, but
    // inside an input's tight margins it sits visibly below the box centre;
    // boxed controls should also share one baseline with each other.
    fonts.font_data.insert(
        "mono-field".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO)
            .tweak(egui::FontTweak {
                y_offset_factor: 0.0,
                ..Default::default()
            })
            .into(),
    );
    fonts.families.insert(
        egui::FontFamily::Name("field".into()),
        vec!["mono-field".to_owned()],
    );
    fonts.font_data.insert(
        "mono-title".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO)
            .tweak(egui::FontTweak {
                y_offset_factor: -0.08,
                ..Default::default()
            })
            .into(),
    );
    fonts.families.insert(
        egui::FontFamily::Name("title".into()),
        vec!["mono-title".to_owned()],
    );
    ctx.set_fonts(fonts);
    // Visuals must go into BOTH style buckets too (see the all_styles_mut
    // comment below): ctx.set_visuals writes only the currently active
    // bucket, which at startup is egui's Dark fallback, while the first
    // rendered frame may already use the Light bucket on a light-themed OS.
    ctx.all_styles_mut(|style| {
        style.visuals = terminal_visuals(dark);
    });
    // item_spacing.y is the *tight* rhythm (label to its own field, mockup's
    // ~6-7px); the *loose* rhythm between field groups (mockup's ~16-18px)
    // is added explicitly via ui.add_space() between groups - one uniform
    // spacing value can't express both.
    //
    // all_styles_mut, not style_mut: egui 0.29 keeps a *separate* Style per
    // Theme (dark_style/light_style) and both style_mut/set_visuals act on
    // whichever one ctx.theme() currently resolves to - which tracks the OS
    // preference independently of our own `dark` toggle. setup_theme() only
    // runs once at startup, so if ctx.theme() later settles on the other
    // bucket (e.g. once eframe reports the real OS theme a frame or two in),
    // this spacing config would silently apply to a style nothing renders
    // with - that's what left the scroll bar on its unstyled default despite
    // the fix below. Writing both buckets up front sidesteps the whole
    // class of bug regardless of which one ends up active.
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        // TextEdit's 9px vertical margin made its row ~6px taller than the
        // neighbouring buttons. Raise their vertical padding to match.
        style.spacing.button_padding = egui::vec2(12.0, 10.0);
        style.spacing.interact_size.y = 28.0;
        // Title bar height = title row + window_margin.top/bottom (egui
        // window.rs), and the bar's bottom stroke is the line the title
        // glyphs were sitting on. Roomier vertical window margin pushes that
        // line away from the text in every popup; the horizontal part just
        // gives popup content some air.
        style.spacing.window_margin = egui::Margin::symmetric(10, 15);
        // Default egui scrollbars are a floating, translucent overlay that
        // pops up over content and hides when idle - fine for a generic app,
        // but it breaks this app's whole premise that nothing overlaps and
        // every element always reserves its own flat, bordered space. Use a
        // solid bar (own space, always opaque, square corners already come
        // from the widgets.* rounding=ZERO below) with a hairline-colored
        // handle (foreground_color -> fg_stroke) instead of bg_fill, which
        // here is the same color as the track and would be invisible.
        style.spacing.scroll = egui::style::ScrollStyle {
            foreground_color: true,
            ..egui::style::ScrollStyle::solid()
        };
        // egui's defaults (Body/Button 14, Small 10) are all a step above the
        // mockup, which is why every string in the app read larger than its
        // HTML twin: mockup sizes are input/select/checkbox 13, .btn 12.5,
        // label.tag/.hint 12, footer/banner 11.
        let mut ts = style.text_styles.clone();
        ts.insert(
            egui::TextStyle::Body,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Button,
            egui::FontId::new(12.5, egui::FontFamily::Name("button".into())),
        );
        ts.insert(
            egui::TextStyle::Small,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        );
        style.text_styles = ts;
    });
}

/// Same "Terminal Native" structure (square corners, hairline ghost buttons,
/// one accent used sparingly) in both modes - only the palette changes. The
/// dark accent/OK/error/warn hues are bright enough to pop on near-black but
/// read as washed-out and low-contrast as text/strokes on a light background,
/// so light mode uses deepened variants (values close to GitHub's light-theme
/// semantic colors, already accessibility-tuned) rather than reusing them.
pub(super) fn terminal_visuals(dark: bool) -> egui::Visuals {
    let rgb = |r: u8, g: u8, b: u8| egui::Color32::from_rgb(r, g, b);
    let (bg, bg_lift, field, line, line_bright, text, text_dim, accent, warn, err) = if dark {
        (
            rgb(0x0b, 0x0b, 0x0e),
            rgb(0x0e, 0x0e, 0x11),
            rgb(0x11, 0x11, 0x14),
            rgb(0x1e, 0x1e, 0x24),
            rgb(0x33, 0x33, 0x3c),
            rgb(0xd6, 0xd6, 0xda),
            rgb(0x83, 0x83, 0x8c),
            rgb(0x59, 0xd6, 0x8c),
            rgb(0xe0, 0xb3, 0x4d),
            rgb(0xe0, 0x65, 0x5c),
        )
    } else {
        (
            rgb(0xf3, 0xf3, 0xf1),
            rgb(0xff, 0xff, 0xff),
            rgb(0xfb, 0xfb, 0xfa),
            rgb(0xda, 0xda, 0xd6),
            rgb(0xb5, 0xb5, 0xb0),
            rgb(0x1c, 0x1c, 0x1e),
            rgb(0x5c, 0x5c, 0x62),
            rgb(0x1a, 0x7f, 0x37),
            rgb(0x9a, 0x67, 0x00),
            rgb(0xcf, 0x22, 0x2e),
        )
    };

    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.window_corner_radius = egui::CornerRadius::ZERO;
    v.menu_corner_radius = egui::CornerRadius::ZERO;
    v.window_fill = bg_lift;
    v.window_stroke = egui::Stroke::new(1.0_f32, line);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.panel_fill = bg;
    v.extreme_bg_color = field; // TextEdit background - the one filled surface
    v.faint_bg_color = field;
    v.code_bg_color = field;
    v.hyperlink_color = accent;
    v.warn_fg_color = warn;
    v.error_fg_color = err;
    // Used for the text-selection highlight inside a TextEdit, and (via
    // interact_selectable) as a focus-outline color - not for "this choice
    // is picked" chrome, that's custom-drawn now (see `engine_choice`).
    v.selection.bg_fill =
        egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 45);
    v.selection.stroke = egui::Stroke::new(1.0_f32, accent);

    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::ZERO;
        // bg_fill backs checkboxes/sliders and must stay visible; weak_bg_fill
        // backs plain buttons and is what makes them "ghost" (border only).
        w.bg_fill = field;
    }
    v.widgets.noninteractive.weak_bg_fill = bg;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, line);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, text_dim);

    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, line);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, text_dim);

    v.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, line_bright);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, text);
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.weak_bg_fill = field;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, accent);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, text);
    v.widgets.active.expansion = 0.0;

    v.widgets.open.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, line_bright);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, text);

    v
}

/// The primary action in a view (Скачать, Выбрать эту папку): an outlined
/// accent "ghost" button rather than a filled block - matches the mockup's
/// CTA treatment and reads as *the* action without a heavy solid fill.
/// Takes the accent color explicitly (from `ui.visuals().hyperlink_color`)
/// since it has to work in both themes and builds the `Button` before it has
/// a `Ui` to read visuals from itself.
pub(super) fn accent_button(
    accent: egui::Color32,
    text: impl Into<String>,
) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(accent))
        .stroke(egui::Stroke::new(1.0_f32, accent))
        .fill(egui::Color32::TRANSPARENT)
}

/// `egui::Spinner` with a bounded repaint cadence. The stock widget calls
/// `Context::request_repaint()` ("because it is animated") on every paint,
/// which pins eframe/winit to the full refresh rate for as long as the
/// spinner is visible - minutes during torrent metadata resolution or a
/// slow folder listing. This draws the same arc (same size, color, 20
/// points, 240° sweep) but asks for the next frame ~66 ms out (~15 fps);
/// status updates and input still repaint immediately.
/// One line of installer feedback: asset, downloaded bytes and a percent
/// when the server declared a size. The old static "скачиваю…" gave no idea
/// whether to wait a minute or ten.
pub(super) fn setup_progress_text(p: Option<&SetupProgress>) -> String {
    let Some(p) = p else {
        return tr!(
            "downloading yt-dlp, aria2c, ffmpeg and Deno…",
            "скачиваю yt-dlp, aria2c, ffmpeg и Deno…"
        )
        .to_string();
    };
    if p.stage != SetupStage::Downloading {
        return format!("{}: {}", p.label, p.stage.text());
    }
    match p.total {
        Some(total) if total > 0 => tr_format!(
            "{}: {} of {} ({:.0}%)",
            "{}: {} из {} ({:.0}%)",
            p.label,
            torrent::human_size(p.done),
            torrent::human_size(total),
            p.done as f64 * 100.0 / total as f64,
        ),
        _ if p.done > 0 => format!("{}: {}", p.label, torrent::human_size(p.done)),
        _ => format!("{}: {}", p.label, p.stage.text()),
    }
}

pub(super) fn slow_spinner(ui: &mut egui::Ui) {
    let size = ui.style().spacing.interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::ProgressIndicator));
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.ctx().request_repaint_after(Duration::from_millis(66));
    let radius = (rect.height() / 2.0) - 2.0;
    let n_points = 20;
    let time = ui.input(|i| i.time);
    let start_angle = time * std::f64::consts::TAU;
    let end_angle = start_angle + 240f64.to_radians() * time.sin();
    let points: Vec<egui::Pos2> = (0..n_points)
        .map(|i| {
            let angle = egui::lerp(start_angle..=end_angle, i as f64 / n_points as f64);
            let (sin, cos) = angle.sin_cos();
            rect.center() + radius * egui::vec2(cos as f32, sin as f32)
        })
        .collect();
    let color = ui.visuals().strong_text_color();
    ui.painter()
        .add(egui::Shape::line(points, egui::Stroke::new(3.0_f32, color)));
}

/// A fixed-width bar slot and two fixed-height buttons. `allocate_ui` cannot
/// reserve the empty/spinner slot here: it advances by the *used* child width,
/// shifting both buttons left when progress is not known yet. The usual 10px
/// vertical button padding also makes an add_sized([…, 28]) button 35px tall;
/// the second button then drifts down relative to the first and to the bar.
pub(super) fn job_controls(
    ui: &mut egui::Ui,
    progress: Option<f32>,
    fill: egui::Color32,
    paused: bool,
    waiting_text: &str,
    first: (&str, f32),
    second: (&str, f32),
) -> (bool, bool) {
    const HEIGHT: f32 = 28.0;
    ui.scope(|ui| {
        ui.spacing_mut().button_padding.y = 5.0;
        ui.horizontal(|ui| {
            let gap = ui.spacing().item_spacing.x;
            let bar_w = (ui.available_width() - first.1 - second.1 - 2.0 * gap).max(40.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, HEIGHT), egui::Sense::hover());
            let mut bar_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            if let Some(p) = progress {
                bar_ui.add(
                    egui::ProgressBar::new(p)
                        .corner_radius(egui::CornerRadius::ZERO)
                        .fill(fill)
                        .desired_width(bar_w)
                        .desired_height(HEIGHT),
                );
            } else if paused {
                bar_ui.add(
                    egui::ProgressBar::new(0.0)
                        .corner_radius(egui::CornerRadius::ZERO)
                        .fill(fill)
                        .desired_width(bar_w)
                        .desired_height(HEIGHT),
                );
            } else {
                bar_ui.horizontal(|ui| {
                    slow_spinner(ui);
                    ui.weak(waiting_text);
                });
            }
            if paused {
                ui.painter().rect_stroke(
                    rect,
                    egui::CornerRadius::ZERO,
                    egui::Stroke::new(1.0_f32, fill),
                    egui::StrokeKind::Middle,
                );
            }
            (
                ui.add_sized([first.1, HEIGHT], egui::Button::new(first.0))
                    .clicked(),
                ui.add_sized([second.1, HEIGHT], egui::Button::new(second.0))
                    .clicked(),
            )
        })
        .inner
    })
    .inner
}

/// Flat "● label" / "○ label" choice, no box at all (not radio_value's
/// circle-in-a-different-style-from-everything-else, not selectable_value's
/// filled pill) - this is what the approved mockup's `.choices` is.
pub(super) fn engine_choice(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let dot = if selected { "●" } else { "○" };
    let accent = ui.visuals().hyperlink_color;
    let color = if selected {
        accent
    } else {
        ui.visuals().widgets.inactive.fg_stroke.color
    };
    // Mockup's `.choices` is 13px text with an 11px ○/● in `::before` - two
    // sizes in one label, so a single RichText can't express it; a LayoutJob
    // (Button accepts it via WidgetText) can.
    // Inactive dot is dimmer than its label in the mockup
    // (`::before{color:--text-mute}` vs `span{color:--text-dim}`).
    let dot_color = if selected {
        accent
    } else {
        ui.visuals().weak_text_color()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &format!("{dot} "),
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(11.0),
            color: dot_color,
            ..Default::default()
        },
    );
    job.append(
        label,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(13.0),
            color,
            ..Default::default()
        },
    );
    let text: egui::WidgetText = job.into();
    // `Button::fill()`/`stroke()` each silently force `frame = true` (it's
    // documented on the methods themselves) - calling either after
    // `.frame(false)` re-enables the frame it just turned off, which is what
    // was drawing a border here (bright accent on the focused choice via
    // the `active` visuals, near-invisible dark-gray on the other two). With
    // frame genuinely off, fill/stroke are never painted, so just don't set
    // them.
    let button = egui::Button::new(text).frame(false);
    ui.add(button)
}

/// "› label" on its own line, above its control - the approved mockup's
/// `label.tag` (`display:block`, so label and field never share a row). The
/// first port kept several labels inline with their control instead
/// (`ui.label("Движок"); <choices on the same horizontal>`), which is
/// exactly the "old architecture wearing new colors" complaint.
pub(super) fn tag_label(ui: &mut egui::Ui, text: &str) {
    // mockup's label.tag is 12px, a step under the fields it captions
    ui.label(
        egui::RichText::new(format!("› {text}"))
            .color(ui.visuals().weak_text_color())
            .size(12.0),
    );
}

/// `egui::Window::new(title)` renders the title with `TextStyle::Heading`
/// (18pt, unstyled) unless the title already carries its own font - a
/// plain `&str` doesn't, so every popup title rendered noticeably larger
/// and in the un-tweaked fallback font, standing out from the rest of the
/// monospace UI around it. Forcing FontId::monospace here matches everything
/// else instead of falling back to Heading's default.
pub(super) fn window_title(text: &str) -> egui::RichText {
    // 15pt made the title taller than the title bar's own line box, so the
    // glyphs rode on top of the window's top stroke; 13 sits inside it. The
    // "title" family is the un-tweaked face (see setup_theme).
    egui::RichText::new(text).font(egui::FontId::new(
        13.0,
        egui::FontFamily::Name("title".into()),
    ))
}

/// Input-field text: body size, but rendered with the boxed controls' font
/// ("field" family, no glyph nudge - see setup_theme) so the text sits
/// centred inside the field's margins.
pub(super) fn field_font() -> egui::FontSelection {
    egui::FontSelection::FontId(egui::FontId::new(
        13.0,
        egui::FontFamily::Name("field".into()),
    ))
}

pub(super) fn clicked_outside(ctx: &egui::Context, rect: egui::Rect) -> bool {
    ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|pos| !rect.contains(pos))
    })
}

/// Dashed 1px rule across the available width - the approved mockup uses
/// this under the status line instead of a solid `ui.separator()`.
pub(super) fn dashed_separator(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let y = rect.center().y;
    let (dash, gap) = (4.0, 3.0);
    let mut x = rect.left();
    while x < rect.right() {
        let x_end = (x + dash).min(rect.right());
        ui.painter().line_segment(
            [egui::pos2(x, y), egui::pos2(x_end, y)],
            egui::Stroke::new(1.0_f32, color),
        );
        x += dash + gap;
    }
}

/// Fractional OS scaling (125/150%) is where the design fell apart: egui
/// rasterized 13pt as 16.25 blurry physical px and every measurement taken
/// from the mockup grew by the scale factor, so the app never matched the
/// HTML side by side. Pinning pixels_per_point to a constant makes one
/// design px equal UI_SCALE physical px on any display, whatever the OS
/// reports. UI_SCALE is larger than 1.0 for readability. Viewport commands
/// take egui points and winit applies the effective scale itself; multiplying
/// those dimensions again would scale the window twice.
pub(super) const UI_SCALE: f32 = 1.18;

pub(super) fn pin_pixel_scale(ctx: &egui::Context, resize_viewport: bool) {
    if ctx.pixels_per_point() == UI_SCALE && !resize_viewport {
        return;
    }
    ctx.set_pixels_per_point(UI_SCALE);
    if resize_viewport {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(460.0, 720.0)));
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
        460.0, 600.0,
    )));
}

/// Drive letter (`C` etc.) of a rooted local path, uppercased for
/// comparison. `None` for UNC/device/relative paths - those aren't letters
/// `drive_roots()` can ever report, so callers treat them as unknown roots.
#[cfg(windows)]
pub(super) fn path_drive_letter(p: &Path) -> Option<u8> {
    match p.components().next()? {
        std::path::Component::Prefix(pre) => match pre.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => {
                Some(letter.to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}
