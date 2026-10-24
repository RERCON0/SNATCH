//! Startup placement in one coordinate system, independent of egui zoom/DPI.
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
}

#[link(name = "user32")]
extern "system" {
    fn MonitorFromWindow(window: *mut c_void, flags: u32) -> *mut c_void;
    fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
    fn GetWindowRect(window: *mut c_void, rect: *mut Rect) -> i32;
    fn IsZoomed(window: *mut c_void) -> i32;
    fn SetWindowPos(
        window: *mut c_void,
        after: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
}

fn centered_rect(window: Rect, work: Rect) -> Option<Rect> {
    let width = (window.right - window.left).min(work.right - work.left);
    let height = (window.bottom - window.top).min(work.bottom - work.top);
    if width <= 0 || height <= 0 {
        return None;
    }
    let left = work.left + (work.right - work.left - width) / 2;
    let top = work.top + (work.bottom - work.top - height) / 2;
    Some(Rect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    })
}

pub(super) fn center(frame: &eframe::Frame) {
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let window = handle.hwnd.get() as *mut c_void;
    let mut rect = Rect::default();
    let mut info = MonitorInfo {
        size: std::mem::size_of::<MonitorInfo>() as u32,
        monitor: Rect::default(),
        work: Rect::default(),
        flags: 0,
    };
    // All Win32 calls run on the window's UI thread. Its live handle is borrowed
    // from Frame; both window and monitor rectangles use native screen units.
    unsafe {
        if IsZoomed(window) != 0 || GetWindowRect(window, &mut rect) == 0 {
            return;
        }
        let monitor = MonitorFromWindow(window, 2); // MONITOR_DEFAULTTONEAREST
        if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        if let Some(rect) = centered_rect(rect, info.work) {
            SetWindowPos(
                window,
                std::ptr::null_mut(),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                0x0004 | 0x0010,
            ); // NOZORDER | NOACTIVATE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centers_final_physical_size_without_applying_ui_scale_again() {
        let window = Rect {
            left: 100,
            top: 100,
            right: 643,
            bottom: 950,
        };
        let work = Rect {
            left: 0,
            top: 0,
            right: 2048,
            bottom: 1192,
        };
        let rect = centered_rect(window, work).unwrap();
        assert_eq!((rect.left, rect.top), (752, 171));
        assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (543, 850));
    }

    #[test]
    fn honors_negative_monitor_origin_and_taskbar_work_area() {
        let rect = centered_rect(
            Rect {
                left: 0,
                top: 0,
                right: 600,
                bottom: 900,
            },
            Rect {
                left: -1920,
                top: 40,
                right: 0,
                bottom: 1080,
            },
        )
        .unwrap();
        assert_eq!((rect.left, rect.top), (-1260, 110));
    }

    #[test]
    fn clamps_oversized_window_to_small_monitor() {
        let rect = centered_rect(
            Rect {
                left: 0,
                top: 0,
                right: 614,
                bottom: 991,
            },
            Rect {
                left: 1920,
                top: 0,
                right: 2944,
                bottom: 728,
            },
        )
        .unwrap();
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (2125, 0, 2739, 728)
        );
    }
}
