//! Port of `WebViewBridge.swift` — decode the JS → native envelope, dispatch,
//! and emit native → JS envelopes as Tauri events.
//!
//! The JS side is unchanged: `web/src/bridge.ts` routes `send()` through
//! `invoke('bridge_dispatch', …)` when running inside Tauri, and forwards the
//! `bridge` event into `window.donemdBridge.receive`. See
//! `tasks/bridge-contract.md` for the full message inventory.

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Webview, WebviewWindow};

use crate::assets;
use crate::outline;
use crate::source;
use crate::state::{AppState, OutlineHeading};

/// `#[tauri::command]` entry — the JS shim invokes this with the envelope
/// object (`{version, type, payload}`) exactly as it would have posted it to
/// `window.webkit.messageHandlers.donemd`.
///
/// `webview` is the SENDER (Tauri resolves the calling webview, child
/// webviews included): the main editor and the outline sidebar both speak
/// this protocol, and `editorReady` means a different thing for each.
#[tauri::command]
pub fn bridge_dispatch(app: AppHandle, webview: Webview, envelope: Value) {
    let version = envelope.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version != 1 {
        eprintln!("[bridge] unsupported envelope version: {version}");
        return;
    }
    let kind = envelope
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let payload = envelope.get("payload").cloned().unwrap_or(Value::Null);

    match kind.as_str() {
        "editorReady" => {
            match webview.label() {
                // The sidebar announcing itself — answer with the stored snapshot
                // (vs. the main editor, whose ready triggers the document load).
                outline::LABEL => on_outline_ready(&app),
                // The read-only source mirror — answer with the serialized doc.
                source::LABEL => on_source_ready(&app),
                "main" => on_editor_ready(&app),
                other => eprintln!("[bridge] editorReady from unexpected webview: {other}"),
            }
        }
        "documentChanged" => on_document_changed(&app),
        "importImage" => on_import_image(&app, &payload),
        "previewImage" => on_preview_image(&app, &payload),
        "openLink" => on_open_link(&app, &payload),
        "outlineChanged" => on_outline_changed(&app, &payload),
        "activeHeadingChanged" => on_active_heading_changed(&app, &payload),
        "outlineJump" => on_outline_jump(&app, &payload),
        // The source pane red-flags the matching LaTeX ranges itself.
        "badMathFormulas" => send_to_source(&app, "badMathFormulas", payload.clone()),
        "foldToggled" => on_fold_toggled(&app, &payload),
        "foldReplace" => on_fold_replace(&app, &payload),
        "documentJSON" => crate::document::on_document_json(&app, &payload),
        "menuCommand" => crate::document::on_menu_command(&app, &payload),
        "aiCommand" => crate::ai::handle_command(&app, &payload),
        "aiCancel" => crate::ai::handle_cancel(&app),
        "aiRetry" => crate::ai::handle_retry(&app, &payload),
        "aiOpenSettings" => crate::ai::open_settings(&app),
        "aiProvidersQuery" => crate::ai::reply_providers(&app, &payload),
        other => eprintln!("[bridge] unhandled type: {other}"),
    }
}

/// Emit one envelope to one page (`window.donemdBridge.receive`) by label.
///
/// NB: `Emitter::emit` called ON a webview object still broadcasts app-wide
/// (the default trait method ignores the receiver and goes through the
/// manager). Harmless today — unknown types are dropped JS-side — but with
/// three pages in play (main / settings / outline) the spray would make
/// debugging miserable, so everything goes through targeted `emit_to`.
pub fn emit(app: &AppHandle, label: &str, kind: &str, payload: Value) {
    let envelope = json!({ "version": 1, "type": kind, "payload": payload });
    if let Err(e) = app.emit_to(label, "bridge", envelope) {
        eprintln!("[bridge] emit {kind} → {label} failed: {e}");
    }
}

/// Emit to the main window's editor pane (menu/native → editor direction).
pub fn send_to_editor(app: &AppHandle, kind: &str, payload: Value) {
    emit(app, "main", kind, payload);
}

/// Emit to the outline sidebar once it exists — it is created on first
/// toggle and then stays resident (hide/show, see outline.rs), so relays
/// keep the hidden page fresh and re-show is instant. Before first creation
/// the webview doesn't exist; skip silently rather than log an error per
/// 300ms-debounced edit.
fn send_to_outline(app: &AppHandle, kind: &str, payload: Value) {
    if app.get_webview(outline::LABEL).is_some() {
        emit(app, outline::LABEL, kind, payload);
    }
}

/// Emit to the Markdown source mirror (docked since startup, so it exists in
/// every healthy session — still checked to keep a creation failure quiet).
pub(crate) fn send_to_source(app: &AppHandle, kind: &str, payload: Value) {
    if app.get_webview(source::LABEL).is_some() {
        emit(app, source::LABEL, kind, payload);
    }
}

pub(crate) fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("main")
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn payload_i64(payload: &Value, key: &str) -> Option<i64> {
    payload.get(key).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

// MARK: - editor lifecycle

fn on_editor_ready(app: &AppHandle) {
    let state = app.state::<AppState>();
    // File-association launch (argv .md path): open it instead of the empty doc.
    let pending_open = state.doc.lock().unwrap().pending_open.take();
    if let Some(path) = pending_open {
        crate::document::open_path(app, &path, true);
        return;
    }
    let doc = state.doc.lock().unwrap();
    let tiptap = doc.tiptap_doc.clone();
    let collapsed = doc.collapsed_headings.clone();
    drop(doc);
    emit(app, "main", "loadDocument", tiptap);
    // Re-apply fold state after a (re)load, same as resendFoldStateIfNeeded.
    if !collapsed.is_empty() {
        emit(app, "main", "applyFold", json!({ "collapsed": collapsed }));
    }
}

fn on_document_changed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let epoch = {
        let mut doc = state.doc.lock().unwrap();
        if !doc.dirty {
            doc.dirty = true;
            update_title(app, &doc);
        }
        // M3.6: mirror the edit into the source pane. The doc itself comes
        // back through the save handshake channel (no evaluateJavaScript on
        // Tauri). One sync in flight at a time — a skipped beat is fine
        // because the reply carries the doc as of the webview's reply moment,
        // so the freshest edits are never lost.
        if doc.source_sync_pending {
            return;
        }
        doc.source_sync_pending = true;
        doc.epoch
    };
    send_to_editor(
        app,
        "requestDocumentJSON",
        json!({ "requestId": format!("source-sync-{epoch}") }),
    );
}

/// Title-bar text: `文件名 • — Done.md` while dirty, mirroring NSDocument's
/// dirty dot.
pub(crate) fn update_title(app: &AppHandle, doc: &crate::state::DocumentState) {
    let Some(window) = main_window(app) else { return };
    let name = doc
        .file_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未命名".to_string());
    let marker = if doc.dirty { " •" } else { "" };
    let _ = window.set_title(&format!("{name}{marker} — Done.md"));
}

// MARK: - images

fn on_import_image(app: &AppHandle, payload: &Value) {
    let Some(request_id) = payload_str(payload, "requestId").map(str::to_string) else {
        eprintln!("[image] importImage: missing requestId");
        return;
    };
    let reply = |payload: Value| {
        let mut p = payload;
        p["requestId"] = json!(request_id);
        emit(app, "main", "imageImported", p);
    };

    let (Some(mime), Some(b64)) = (
        payload_str(payload, "mime"),
        payload_str(payload, "base64"),
    ) else {
        reply(json!({ "success": false, "error": "invalid importImage payload" }));
        return;
    };
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(_) => {
            reply(json!({ "success": false, "error": "invalid importImage payload" }));
            return;
        }
    };

    let state = app.state::<AppState>();
    match assets::import_asset(&state, &bytes, mime) {
        Ok((_filename, url, markdown_path)) => reply(json!({
            "success": true,
            "assetURL": url,
            "markdownPath": markdown_path,
        })),
        Err(e) => reply(json!({ "success": false, "error": e })),
    }
}

fn on_preview_image(app: &AppHandle, payload: &Value) {
    let Some(src) = payload_str(payload, "src") else { return };
    // src is the runtime asset URL (http://donemd-asset.localhost/<file> on
    // Windows, donemd-asset://<file> elsewhere) — peel the filename.
    let filename = src
        .rsplit('/')
        .next()
        .unwrap_or(src)
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    if filename.is_empty() {
        return;
    }
    let state = app.state::<AppState>();
    if let Some(path) = assets::stored_file_path(&state, &filename) {
        // System default viewer — the Windows counterpart of QuickLook.
        let _ = open::that_detached(path);
    }
}

// MARK: - links

fn on_open_link(app: &AppHandle, payload: &Value) {
    let Some(href) = payload_str(payload, "href") else { return };

    // Dangerous schemes never leave the app (parity with LinkTarget.classify).
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("data:") || lower.starts_with("vbscript:") {
        eprintln!("[link] openLink: rejected href: {href}");
        return;
    }
    // In-page anchors are the webview's own business.
    if href.starts_with('#') {
        return;
    }
    // feishu://<type>/<token> placeholder cards → open the bound Feishu doc.
    if lower.starts_with("feishu://") {
        // Frontmatter parsing lands with the Markdown engine milestone; until
        // then there is no bound doc URL to open.
        eprintln!("[link] openLink: feishu placeholder, no bound doc URL yet");
        return;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
        let _ = open::that_detached(href);
        return;
    }

    // Local path — resolve against the document directory.
    let state = app.state::<AppState>();
    let base = state.doc.lock().unwrap().file_path.clone();
    let resolved = resolve_local_path(href, base.as_deref());
    let Some(path) = resolved else {
        eprintln!("[link] openLink: cannot resolve relative path (unsaved doc): {href}");
        return;
    };
    if path.exists() {
        if should_reveal_rather_than_launch(&path) {
            reveal_in_explorer(&path);
        } else {
            let _ = open::that_detached(&path);
        }
    } else if !href.contains(std::path::MAIN_SEPARATOR) && !href.starts_with('.') {
        // Schemeless token that names no local file (e.g. `example.com`) → web.
        let _ = open::that_detached(format!("https://{href}"));
    }
}

fn resolve_local_path(path: &str, doc_path: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        dirs_home().map(|h| h.join(rest))
    } else {
        let p = std::path::PathBuf::from(path);
        if p.is_absolute() {
            Some(p)
        } else {
            doc_path?.parent().map(|dir| dir.join(p))
        }
    }?;
    Some(expanded)
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
}

/// Executable-ish files are revealed in Explorer instead of launched — a link
/// in an untrusted document must not run a program on a single click.
fn should_reveal_rather_than_launch(path: &std::path::Path) -> bool {
    const DANGEROUS: &[&str] = &[
        "exe", "bat", "cmd", "com", "ps1", "psm1", "vbs", "vbe", "js", "jse",
        "wsf", "wsh", "msi", "msp", "scr", "pif", "lnk", "hta", "cpl", "dll",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| DANGEROUS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[cfg(windows)]
fn reveal_in_explorer(path: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer")
        .arg("/select,")
        .arg(path)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn();
}

#[cfg(not(windows))]
fn reveal_in_explorer(path: &std::path::Path) {
    let _ = open::that_detached(path);
}

// MARK: - outline & fold

/// The sidebar webview signals readiness: answer with the stored snapshot so
/// it renders immediately instead of waiting for the user's next edit.
fn on_outline_ready(app: &AppHandle) {
    let state = app.state::<AppState>();
    let (headings, active) = {
        let doc = state.doc.lock().unwrap();
        (doc.outline.clone(), doc.active_heading)
    };
    send_to_outline(app, "outlineSet", json!({ "headings": headings }));
    send_to_outline(app, "outlineActive", json!({ "index": active }));
}

fn on_outline_changed(app: &AppHandle, payload: &Value) {
    let headings: Vec<OutlineHeading> = payload
        .get("headings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|h| {
                    Some(OutlineHeading {
                        index: h.get("index")?.as_i64().or_else(|| h.get("index")?.as_f64().map(|f| f as i64))?,
                        level: h.get("level").and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(1),
                        text: h.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let state = app.state::<AppState>();
    state.doc.lock().unwrap().outline = headings.clone();
    // Relay to the sidebar (single consumer; no-op while it's hidden).
    send_to_outline(app, "outlineSet", json!({ "headings": headings }));
}

fn on_active_heading_changed(app: &AppHandle, payload: &Value) {
    let index = payload_i64(payload, "index");
    let state = app.state::<AppState>();
    state.doc.lock().unwrap().active_heading = index;
    send_to_outline(app, "outlineActive", json!({ "index": index }));
}

/// Sidebar row click → scroll both panes to the Nth heading (the Visual pane
/// smooth-scrolls; the source mirror finds the same heading-ordinal in the
/// serialized text, same as the macOS fan-out).
fn on_outline_jump(app: &AppHandle, payload: &Value) {
    if let Some(index) = payload_i64(payload, "index") {
        send_to_editor(app, "scrollToHeading", json!({ "index": index }));
        send_to_source(app, "scrollToHeading", json!({ "index": index }));
    }
}

/// The source mirror announcing itself: push the current doc (serialized
/// body-only — the frontmatter display toggle stays a macOS-only top-bar
/// feature for now) plus the fold snapshot, so a late-loading pane catches
/// up without waiting for the next edit.
fn on_source_ready(app: &AppHandle) {
    let state = app.state::<AppState>();
    let (markdown, collapsed) = {
        let doc = state.doc.lock().unwrap();
        (crate::document::serialize_body(&doc), doc.collapsed_headings.clone())
    };
    send_to_source(app, "setMarkdownSource", json!({ "text": markdown }));
    send_to_source(app, "applyFold", json!({ "collapsed": collapsed }));
}

fn on_fold_toggled(app: &AppHandle, payload: &Value) {
    let (Some(ordinal), Some(collapse)) = (
        payload_i64(payload, "ordinal"),
        payload.get("collapse").and_then(Value::as_bool),
    ) else {
        return;
    };
    let state = app.state::<AppState>();
    let collapsed = {
        let mut doc = state.doc.lock().unwrap();
        if collapse {
            if !doc.collapsed_headings.contains(&ordinal) {
                doc.collapsed_headings.push(ordinal);
            }
        } else {
            doc.collapsed_headings.retain(|&o| o != ordinal);
        }
        doc.collapsed_headings.clone()
    };
    // Fold state is authoritative here and re-broadcast to BOTH panes —
    // the chevron click may have come from either (foldToggled), and the
    // other pane must follow (same as macOS).
    emit(app, "main", "applyFold", json!({ "collapsed": collapsed }));
    send_to_source(app, "applyFold", json!({ "collapsed": collapsed }));
}

fn on_fold_replace(app: &AppHandle, payload: &Value) {
    let ordinals: Vec<i64> = payload
        .get("collapsed")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                .collect()
        })
        .unwrap_or_default();
    let state = app.state::<AppState>();
    state.doc.lock().unwrap().collapsed_headings = ordinals.clone();
    emit(app, "main", "applyFold", json!({ "collapsed": ordinals }));
    send_to_source(app, "applyFold", json!({ "collapsed": ordinals }));
}
