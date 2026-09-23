//! Main-window pane layout: `[大纲?][Visual][Markdown 源]`.
//!
//! The Visual editor is the window's primary webview ("main"); the outline
//! sidebar (`outline.rs`, on-demand) and the Markdown source mirror
//! (`source.rs`, always docked) are child webviews. Bounds are re-asserted
//! here on every show/hide/resize — this module is the single layout source
//! of truth.
//!
//! Why manual bounds: the primary webview's auto-resize would snap it back
//! over the whole window on every resize, so once any sibling is docked it
//! stays OFF for "main" and this function owns the geometry.

use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Rect};

use crate::{outline, source};

pub fn relayout(app: &AppHandle) {
    let Some(window) = app.get_window("main") else { return };
    let (Ok(scale), Ok(inner)) = (window.scale_factor(), window.inner_size()) else {
        return;
    };
    let w = f64::from(inner.width) / scale;
    let h = f64::from(inner.height) / scale;
    // Minimized windows report a 0×0 client area on Windows — keep the last
    // good bounds instead of squashing every pane to nothing.
    if w < 1.0 || h < 1.0 {
        return;
    }
    let Some(visual) = window.get_webview("main") else { return };

    let mut x = 0.0;
    if outline::is_visible(app) {
        if let Some(bar) = window.get_webview(outline::LABEL) {
            let _ = bar.set_bounds(Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(outline::WIDTH, h).into(),
            });
        }
        x = outline::WIDTH;
    }

    let source_pane = window.get_webview(source::LABEL);
    // Visual gets the left half of the remaining strip, source the right;
    // if the source pane failed to create, Visual takes everything.
    let remaining = (w - x).max(200.0);
    let visual_w = if source_pane.is_some() { remaining / 2.0 } else { remaining };

    let _ = visual.set_auto_resize(false);
    let _ = visual.set_bounds(Rect {
        position: LogicalPosition::new(x, 0.0).into(),
        size: LogicalSize::new(visual_w, h).into(),
    });
    if let Some(src) = source_pane {
        let _ = src.set_bounds(Rect {
            position: LogicalPosition::new(x + visual_w, 0.0).into(),
            size: LogicalSize::new(remaining - visual_w, h).into(),
        });
    }
}
