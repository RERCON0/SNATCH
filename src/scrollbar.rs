use eframe::egui;

/// Give only the scrollbar a subdued palette. Content keeps its original
/// widget/text colors in both the full form and nested scroll areas.
fn styled<R>(
    ui: &mut egui::Ui,
    render: impl FnOnce(&mut egui::Ui, std::sync::Arc<egui::Style>) -> R,
) -> R {
    ui.scope(|ui| {
        let content_style = ui.style().clone();
        if ui.visuals().dark_mode {
            let widgets = &mut ui.visuals_mut().widgets;
            widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(0x34, 0x34, 0x3e);
            widgets.hovered.fg_stroke.color = egui::Color32::from_rgb(0x4d, 0x4d, 0x59);
            widgets.active.fg_stroke.color = egui::Color32::from_rgb(0x67, 0x67, 0x75);
        }
        render(ui, content_style)
    })
    .inner
}

pub trait ScrollAreaExt {
    fn show_terminal<R>(
        self,
        ui: &mut egui::Ui,
        content: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R>;

    fn show_terminal_rows<R>(
        self,
        ui: &mut egui::Ui,
        height: f32,
        rows: usize,
        content: impl FnOnce(&mut egui::Ui, std::ops::Range<usize>) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R>;
}

impl ScrollAreaExt for egui::ScrollArea {
    fn show_terminal<R>(
        self,
        ui: &mut egui::Ui,
        content: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R> {
        styled(ui, |ui, original| {
            self.show(ui, |inner| {
                inner.set_style(original);
                content(inner)
            })
        })
    }

    fn show_terminal_rows<R>(
        self,
        ui: &mut egui::Ui,
        height: f32,
        rows: usize,
        content: impl FnOnce(&mut egui::Ui, std::ops::Range<usize>) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R> {
        styled(ui, |ui, original| {
            self.show_rows(ui, height, rows, |inner, range| {
                inner.set_style(original);
                content(inner, range)
            })
        })
    }
}
