//! Native canvas color behind the transparent web page.
//!
//! `web/src/visual.css` paints `html, body { background: transparent }` so the
//! macOS window canvas (`WritingTheme.backgroundColor` = `NSColor
//! .textBackgroundColor` for the system theme) shows through — the WKWebView
//! there has `drawsBackground = false` to match. WebView2 has no such native
//! canvas behind it: its default background is opaque WHITE, which clashes
//! with the CSS dark palette (`prefers-color-scheme: dark` flips text to
//! light grays) and renders as light-grey text on a white page.
//!
//! This module is the Windows counterpart of that canvas: it paints the
//! WEBVIEW background (WebView2 `DefaultBackgroundColor`) to match the
//! effective system theme, at startup and on every OS theme flip.

use tauri::window::Color;
use tauri::{AppHandle, Manager, Theme, WebviewWindow};

/// macOS `NSColor.textBackgroundColor` equivalents — light is plain white,
/// dark is the familiar near-black — so the Windows canvas reads the same as
/// the Mac system theme the CSS tokens were calibrated against.
const LIGHT_CANVAS: Color = Color(0xFF, 0xFF, 0xFF, 0xFF);
const DARK_CANVAS: Color = Color(0x1E, 0x1E, 0x1E, 0xFF);

fn canvas_color(theme: Theme) -> Color {
    match theme {
        Theme::Dark => DARK_CANVAS,
        _ => LIGHT_CANVAS,
    }
}

fn paint(webview: &WebviewWindow, theme: Theme) {
    if let Err(e) = webview.set_background_color(Some(canvas_color(theme))) {
        eprintln!("[theme] set_background_color failed: {e}");
    }
}

/// Paint the canvas for the current theme at startup.
pub fn apply_system_canvas(app: &AppHandle) {
    let Some(webview) = app.get_webview_window("main") else { return };
    paint_window(&webview);
}

/// Paint one webview with the canvas matching its current theme — used at
/// startup and whenever a secondary window (设置 / 大纲) is created.
pub fn paint_window(webview: &WebviewWindow) {
    let theme = webview.theme().unwrap_or(Theme::Light);
    paint(webview, theme);
}

/// Child webviews (大纲边栏) have no `WebviewWindow` wrapper, so they take
/// the parent window's theme. `web/src/outline.css` paints the page
/// transparent — without this the WebView2 default is opaque white, which
/// clashes with the dark palette the same way as on the main editor.
pub fn paint_child(window: &tauri::Window, webview: &tauri::Webview) {
    let theme = window.theme().unwrap_or(Theme::Light);
    if let Err(e) = webview.set_background_color(Some(canvas_color(theme))) {
        eprintln!("[theme] child set_background_color failed: {e}");
    }
}

/// `WindowEvent::ThemeChanged` — follow OS light/dark flips while running.
/// The webview's `prefers-color-scheme` tracks the same change on its own, so
/// only the native canvas needs the nudge. Child panes (大纲边栏 / 源码镜像)
/// are separate WebView2 instances with their own canvases — repaint them too.
pub fn on_theme_changed(window: &tauri::Window, theme: Theme) {
    let app = window.app_handle();
    let Some(webview) = app.get_webview_window(window.label()) else { return };
    paint(&webview, theme);
    for label in [crate::outline::LABEL, crate::source::LABEL] {
        if let Some(child) = window.get_webview(label) {
            if let Err(e) = child.set_background_color(Some(canvas_color(theme))) {
                eprintln!("[theme] {label} set_background_color failed: {e}");
            }
        }
    }
}
