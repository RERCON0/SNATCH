//! Geometry only: no input text, widget state, paths, or generated secrets.
use eframe::egui;

pub(super) struct WindowState {
    pub size: [f32; 2],
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            size: super::style::WINDOW_SIZE,
            maximized: false,
        }
    }
}

impl WindowState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let Some(storage) = storage else {
            return Self::default();
        };
        let size = eframe::get_value::<[f32; 2]>(storage, "normal_window_size")
            .filter(|size| valid_size(*size))
            .unwrap_or(Self::default().size);
        Self {
            size,
            maximized: eframe::get_value(storage, "window_maximized").unwrap_or(false),
        }
    }

    pub fn observe(&mut self, ctx: &egui::Context) {
        ctx.input(|i| {
            let viewport = i.viewport();
            self.update(
                viewport.inner_rect.map(|r| [r.width(), r.height()]),
                viewport.maximized.unwrap_or(self.maximized),
                viewport.minimized.unwrap_or(false),
            );
        });
    }

    fn update(&mut self, size: Option<[f32; 2]>, maximized: bool, minimized: bool) {
        // A minimized window can report a zero size and change maximized flags.
        if minimized {
            return;
        }
        self.maximized = maximized;
        if !maximized {
            if let Some(size) = size.filter(|size| valid_size(*size)) {
                self.size = size;
            }
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "normal_window_size", &self.size);
        eframe::set_value(storage, "window_maximized", &self.maximized);
    }
}

fn valid_size(size: [f32; 2]) -> bool {
    size.into_iter()
        .all(|v| v.is_finite() && (64.0..=32768.0).contains(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::Storage as _;
    #[derive(Default)]
    struct Memory(std::collections::HashMap<String, String>);
    impl eframe::Storage for Memory {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.into(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }
    #[test]
    fn resize_survives_restart_maximize_and_minimize_without_scaling_drift() {
        let mut storage = Memory::default();
        let mut window = WindowState::default();
        window.update(Some([693.0, 812.0]), false, false);
        window.update(Some([1920.0, 1080.0]), true, false);
        window.update(Some([0.0, 0.0]), false, true);
        window.save(&mut storage);
        for _ in 0..3 {
            window = WindowState::load(Some(&storage));
            assert_eq!(window.size, [693.0, 812.0]);
            assert!(window.maximized);
            window.save(&mut storage);
        }
        window.update(Some([721.0, 835.0]), false, false);
        window.save(&mut storage);
        let restored = WindowState::load(Some(&storage));
        assert_eq!(restored.size, [721.0, 835.0]);
        assert!(!restored.maximized);
        assert_eq!(storage.0.len(), 2); // geometry only
    }
    #[test]
    fn corrupt_or_impossible_sizes_fall_back_to_first_run_defaults() {
        let mut storage = Memory::default();
        for bad in ["broken", "[-2.0, 800.0]", "[0.0, 0.0]", "[999999.0, 900.0]"] {
            storage.set_string("normal_window_size", bad.into());
            assert_eq!(WindowState::load(Some(&storage)).size, [575.0, 900.0]);
        }
    }
}
