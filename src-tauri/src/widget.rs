//! Widget mode: an opt-in way to keep the dashboard on screen as an
//! always-on-top widget that can be dragged anywhere and collapsed to a
//! slim usage bar. Enabled in Settings → General and persisted as `widgetMode`
//! (plus `widgetCollapsed`). While it is off, lib.rs behaves exactly as
//! before: it only consults `widget_mode()` to skip the blur auto-hide and
//! the re-anchor at the tray click.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::Manager;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetWindow, GetWindowRect, IsWindowVisible, SetWindowPos, GW_HWNDPREV,
    HWND_TOPMOST, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
};

static WIDGET_MODE: AtomicBool = AtomicBool::new(false);
static WIDGET_COLLAPSED: AtomicBool = AtomicBool::new(false);

/// Window size from tauri.conf.json; only the height changes when collapsed.
const WIDTH: f64 = 380.0;
const EXPANDED_HEIGHT: f64 = 600.0;
/// Must match the `#widget-bar` height in styles.css.
const COLLAPSED_HEIGHT: f64 = 40.0;

pub fn widget_mode() -> bool {
    WIDGET_MODE.load(Ordering::Relaxed)
}

/// (mode, collapsed) from config.json. Missing or non-bool keys mean off,
/// and collapsed never applies without the mode.
fn flags_from_config(cfg: &serde_json::Value) -> (bool, bool) {
    let flag = |key| cfg.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false);
    let mode = flag("widgetMode");
    (mode, mode && flag("widgetCollapsed"))
}

/// Restore the persisted state at boot, before the window is first shown.
pub fn init_from_config(cfg: &serde_json::Value, app: &tauri::AppHandle) {
    let (mode, collapsed) = flags_from_config(cfg);
    apply(app, mode, collapsed);
}

fn apply(app: &tauri::AppHandle, mode: bool, collapsed: bool) {
    let collapsed = mode && collapsed;
    WIDGET_MODE.store(mode, Ordering::Relaxed);
    if WIDGET_COLLAPSED.swap(collapsed, Ordering::Relaxed) == collapsed {
        return;
    }
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let height = if collapsed { COLLAPSED_HEIGHT } else { EXPANDED_HEIGHT };
    // The window is `resizable: false`, which makes set_size a no-op on
    // Windows — lift it just for the programmatic resize.
    let _ = window.set_resizable(true);
    let _ = window.set_size(tauri::LogicalSize::new(WIDTH, height));
    let _ = window.set_resizable(false);
    if !collapsed {
        keep_on_screen(&window);
    }
}

/// A bar dragged near the bottom edge would expand off-screen: shift the
/// expanded window back into its monitor's work area.
fn keep_on_screen(window: &tauri::WebviewWindow) {
    let (Ok(Some(monitor)), Ok(pos)) = (window.current_monitor(), window.outer_position()) else {
        return;
    };
    let size = tauri::LogicalSize::new(WIDTH, EXPANDED_HEIGHT).to_physical::<i32>(monitor.scale_factor());
    let area = monitor.work_area();
    let (left, top) = (area.position.x, area.position.y);
    let right = left + area.size.width as i32 - size.width;
    let bottom = top + area.size.height as i32 - size.height;
    let x = pos.x.min(right).max(left);
    let y = pos.y.min(bottom).max(top);
    if (x, y) != (pos.x, pos.y) {
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

/// Frontend → Rust sync after every widget setting change. Persistence
/// stays in the frontend (`set_config`), like every other setting.
#[tauri::command]
pub fn widget_apply(app: tauri::AppHandle, enabled: bool, collapsed: bool) {
    apply(&app, enabled, collapsed);
}

/// Start the native window drag from the widget bar. A command instead of
/// `data-tauri-drag-region` so the webview needs no extra window capability.
#[tauri::command]
pub fn widget_start_drag(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.start_dragging();
    }
}

/// Keeps the widget above the taskbar. The taskbar is topmost too, so
/// clicking it covers the widget — and when it already has focus, our
/// window sees no event at all. So a light loop checks every 25 ms whether
/// a taskbar sits above the widget and overlaps it, and only then
/// re-inserts the widget at the top of the topmost band. (tao's
/// `set_always_on_top(true)` is a no-op on a window that has the flag.)
pub fn spawn_taskbar_keeper(app: &tauri::AppHandle) {
    let Some(hwnd) = app.get_webview_window("main").and_then(|w| w.hwnd().ok()) else {
        return;
    };
    let hwnd = hwnd.0 as isize; // HWND is not Send
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(25));
        let hwnd = HWND(hwnd as _);
        unsafe {
            if widget_mode() && IsWindowVisible(hwnd).as_bool() && under_taskbar(hwnd) {
                let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            }
        }
    });
}

/// Whether a visible taskbar (`Shell_TrayWnd`, or `Shell_SecondaryTrayWnd`
/// on other monitors) is above `hwnd` in the Z order and overlaps it.
unsafe fn under_taskbar(hwnd: HWND) -> bool {
    let mut ours = RECT::default();
    if GetWindowRect(hwnd, &mut ours).is_err() {
        return false;
    }
    let mut above = GetWindow(hwnd, GW_HWNDPREV);
    while let Ok(w) = above {
        let mut class = [0u16; 32];
        let len = GetClassNameW(w, &mut class) as usize;
        let mut r = RECT::default();
        if String::from_utf16_lossy(&class[..len]).ends_with("TrayWnd")
            && IsWindowVisible(w).as_bool()
            && GetWindowRect(w, &mut r).is_ok()
            && r.left < ours.right
            && ours.left < r.right
            && r.top < ours.bottom
            && ours.top < r.bottom
        {
            return true;
        }
        above = GetWindow(w, GW_HWNDPREV);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::flags_from_config;
    use serde_json::json;

    #[test]
    fn widget_flags_follow_config() {
        assert_eq!(flags_from_config(&json!({})), (false, false));
        assert_eq!(flags_from_config(&json!({ "widgetMode": true })), (true, false));
        assert_eq!(
            flags_from_config(&json!({ "widgetMode": true, "widgetCollapsed": true })),
            (true, true)
        );
        // A stale collapsed flag without the mode is ignored.
        assert_eq!(flags_from_config(&json!({ "widgetCollapsed": true })), (false, false));
        // Hand-edited junk must not crash the boot.
        assert_eq!(
            flags_from_config(&json!({ "widgetMode": "yes", "widgetCollapsed": 1 })),
            (false, false)
        );
    }
}
