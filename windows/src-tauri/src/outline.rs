//! 文档大纲边栏 — the Windows (multi-webview) counterpart of macOS's SwiftUI
//! `OutlineSidebar` (`NavigationSplitView` column).
//!
//! The sidebar is a child webview (`web/outline.html`, a pure projection)
//! docked at the left edge of the main window; `layout.rs` owns the pane
//! geometry. Data flow is a relay through `bridge.rs`: main.ts pushes
//! `outlineChanged` / `activeHeadingChanged`, native stores the snapshot
//! (`state.rs`) and fans `outlineSet` / `outlineActive` out to this sidebar;
//! row clicks come back as `outlineJump` → `scrollToHeading` to the panes.
//!
//! Toggling hides/shows the child webview — it is created ONCE and kept
//! resident. (The first implementation closed and re-created it per toggle:
//! `Webview::close()` unregisters synchronously but the actual WebView2
//! teardown is a queued event-loop message, and a later `add_child` with the
//! same label silently failed to display. Hide/show is also instant and
//! preserves scroll position — same feel as the macOS sidebar.)

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager};

/// Child-webview label — also the `emit_to` target and the `editorReady`
/// route key in `bridge_dispatch`.
pub const LABEL: &str = "outline";

/// Sidebar width in logical pixels (macOS's sidebar defaults to ≈ 220–260).
pub(crate) const WIDTH: f64 = 260.0;

/// `tauri::Webview` has hide()/show() but no is_visible() getter (2.11), so
/// the docked state is tracked here. Single-window app → one flag suffices.
static VISIBLE: AtomicBool = AtomicBool::new(false);

/// Sidebar currently shown? The webview may exist but be hidden, so track
/// visibility explicitly instead of checking existence.
pub fn is_visible(_app: &AppHandle) -> bool {
    VISIBLE.load(Ordering::Relaxed)
}

/// 视图 → 文档大纲 / Ctrl+Shift+O.
pub fn toggle(app: &AppHandle) {
    if is_visible(app) {
        hide(app);
    } else {
        show(app);
    }
}

fn show(app: &AppHandle) {
    let Some(window) = app.get_window("main") else { return };
    if let Some(existing) = window.get_webview(LABEL) {
        let _ = existing.show();
        VISIBLE.store(true, Ordering::Relaxed);
        crate::layout::relayout(app);
        return;
    }
    let (Ok(scale), Ok(inner)) = (window.scale_factor(), window.inner_size()) else {
        return;
    };
    let logical_h = f64::from(inner.height) / scale;

    let builder = tauri::WebviewBuilder::new(
        LABEL,
        tauri::WebviewUrl::App("outline.html".into()),
    )
    // Not a drop target — dropping a file belongs to the editor pane.
    .disable_drag_drop_handler()
    // Don't steal the editor's caret on open.
    .focused(false);
    let webview = match window.add_child(
        builder,
        LogicalPosition::new(0.0, 0.0),
        LogicalSize::new(WIDTH, logical_h),
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[outline] add_child failed: {e}");
            return;
        }
    };
    crate::theme::paint_child(&window, &webview);
    VISIBLE.store(true, Ordering::Relaxed);
    crate::layout::relayout(app);
}

fn hide(app: &AppHandle) {
    let Some(window) = app.get_window("main") else { return };
    if let Some(webview) = window.get_webview(LABEL) {
        let _ = webview.hide();
    }
    VISIBLE.store(false, Ordering::Relaxed);
    crate::layout::relayout(app);
}
