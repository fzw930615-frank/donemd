//! Markdown 源码镜像栏 — the Windows counterpart of macOS's
//! `MarkdownSourceWebView`: a read-only CodeMirror pane docked right of the
//! Visual editor, fed by `setMarkdownSource` (see bridge.rs / document.rs).
//!
//! macOS parity: the pane is ALWAYS docked — no visibility toggle — so it is
//! created once at startup and stays resident.
//!
//! Sync flow (Windows has no evaluateJavaScript): Visual's `documentChanged`
//! (rAF-coalesced ~60Hz) → native asks the editor for the live doc via the
//! save handshake channel (`requestDocumentJSON {requestId:
//! "source-sync-<epoch>"}`) → the reply is serialized by the M2 engine and
//! pushed back as `setMarkdownSource {text}`. The `<epoch>` suffix guards
//! the async reply against a new/open landing mid-flight: a stale reply
//! carries the PREVIOUS document and must be dropped (document.rs).

use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager};

/// Child-webview label — `emit_to` target and `editorReady` route key.
pub const LABEL: &str = "source";

/// Create the pane at startup. Initial bounds are a guess; `layout::relayout`
/// (called right after, and on every window resize) owns the real geometry.
pub fn setup(app: &AppHandle) {
    let Some(window) = app.get_window("main") else { return };
    if window.get_webview(LABEL).is_some() {
        return;
    }
    let (Ok(scale), Ok(inner)) = (window.scale_factor(), window.inner_size()) else {
        return;
    };
    let w = f64::from(inner.width) / scale;
    let h = f64::from(inner.height) / scale;

    let builder = tauri::WebviewBuilder::new(
        LABEL,
        tauri::WebviewUrl::App("markdown-source.html".into()),
    )
    // Not a drop target — dropping a file belongs to the editor pane.
    .disable_drag_drop_handler()
    // Don't steal the editor's caret at startup.
    .focused(false);
    match window.add_child(
        builder,
        LogicalPosition::new(w / 2.0, 0.0),
        LogicalSize::new(w / 2.0, h),
    ) {
        Ok(webview) => {
            crate::theme::paint_child(&window, &webview);
        }
        Err(e) => eprintln!("[source] add_child failed: {e}"),
    }
    crate::layout::relayout(app);
}
