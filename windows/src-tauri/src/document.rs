//! Document lifecycle: new / open / save / save-as, dirty-close
//! confirmation, and file-association launch.
//!
//! Tauri commands can't evaluateJavaScript, so saving is a two-step
//! handshake: native emits `requestDocumentJSON {requestId}`, the web side
//! answers `documentJSON {requestId, doc}` (additive handler in main.ts),
//! and the response handler serializes + writes the file. `state.pending_save`
//! carries the intent across the round-trip and doubles as a re-entrancy
//! guard (menu accelerator + JS key fallback can otherwise double-fire).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};

use crate::assets;
use crate::bridge;
use crate::markdown::{self, frontmatter::Frontmatter};
use crate::state::{empty_tiptap_doc, fresh_staging_dir, AppState, PendingSave};

// MARK: - menu command entry points

/// `menuCommand` 信封 —— `web/src/main.ts` 的按键兜底通道。
///
/// WebView2 聚焦时会吞掉原生菜单加速键,所以前端把这些键镜像过来。
/// 分发的目标与菜单项完全一致(同一函数),不另开一套逻辑。
pub fn on_menu_command(app: &AppHandle, payload: &Value) {
    match payload.get("command").and_then(Value::as_str) {
        Some("new") => new_document(app),
        Some("open") => open_dialog(app),
        Some("save") => save(app),
        Some("saveAs") => save_as(app),
        Some("toggleOutline") => crate::outline::toggle(app),
        Some("feishuPull") => crate::feishu::pull_command::invoke(app),
        Some("feishuPush") => crate::feishu::push_command::invoke(app),
        other => eprintln!("[document] unknown menuCommand: {other:?}"),
    }
}

pub fn new_document(app: &AppHandle) {
    if !confirm_discard_if_dirty(app) {
        return;
    }
    let state = app.state::<AppState>();
    let source_markdown;
    {
        let mut doc = state.doc.lock().unwrap();
        doc.file_path = None;
        doc.dirty = false;
        doc.staging_dir = fresh_staging_dir();
        doc.collapsed_headings.clear();
        doc.outline.clear();
        doc.active_heading = None;
        doc.tiptap_doc = empty_tiptap_doc();
        doc.frontmatter = Frontmatter::default();
        doc.pending_save = None;
        // Invalidate any in-flight source-sync reply (it holds the OLD doc).
        doc.epoch += 1;
        doc.source_sync_pending = false;
        source_markdown = serialize_body(&doc);
        bridge::update_title(app, &doc);
    }
    bridge::send_to_editor(app, "loadDocument", empty_tiptap_doc());
    bridge::send_to_editor(app, "applyFold", json!({ "collapsed": [] }));
    push_source_snapshot(app, source_markdown);
}

/// Source-mirror doc-switch snapshot (M3.6): covers the pane when it is
/// already loaded; a still-loading pane catches up via its `editorReady`
/// snapshot instead (`bridge::on_source_ready`).
fn push_source_snapshot(app: &AppHandle, markdown_text: String) {
    bridge::send_to_source(app, "setMarkdownSource", json!({ "text": markdown_text }));
    bridge::send_to_source(app, "applyFold", json!({ "collapsed": [] }));
}

pub fn open_dialog(app: &AppHandle) {
    if !confirm_discard_if_dirty(app) {
        return;
    }
    let picked = app
        .dialog()
        .file()
        .add_filter("Markdown", &["md", "markdown", "mdown", "mkd"])
        .blocking_pick_file();
    let Some(picked) = picked else { return };
    let Ok(path) = picked.into_path() else { return };
    open_path(app, &path, true);
}

/// Open a .md file into the editor. `skip_confirm` for paths that arrive
/// after the dirty check already ran (dialog flow, argv launch).
pub fn open_path(app: &AppHandle, path: &Path, skip_confirm: bool) {
    if !skip_confirm && !confirm_discard_if_dirty(app) {
        return;
    }
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            app.dialog()
                .message(format!("无法打开文件：{e}"))
                .title("Done.md")
                .buttons(MessageDialogButtons::Ok)
                .blocking_show();
            return;
        }
    };
    let parsed = markdown::parse_document(&source);
    let state = app.state::<AppState>();
    let source_markdown;
    {
        let mut doc = state.doc.lock().unwrap();
        doc.file_path = Some(path.to_path_buf());
        doc.dirty = false;
        doc.tiptap_doc = parsed.body.clone();
        doc.frontmatter = parsed.frontmatter;
        doc.collapsed_headings.clear();
        doc.outline.clear();
        doc.active_heading = None;
        doc.pending_save = None;
        // Invalidate any in-flight source-sync reply (it holds the OLD doc).
        doc.epoch += 1;
        doc.source_sync_pending = false;
        source_markdown = serialize_body(&doc);
        bridge::update_title(app, &doc);
    }
    bridge::send_to_editor(app, "loadDocument", parsed.body);
    bridge::send_to_editor(app, "applyFold", json!({ "collapsed": [] }));
    push_source_snapshot(app, source_markdown);
}

/// Serialize the in-memory Tiptap body to canonical Markdown (no frontmatter
/// — the Mac source pane's frontmatter toggle is not ported; M3.6 keeps the
/// body-only default).
pub(crate) fn serialize_body(doc: &crate::state::DocumentState) -> String {
    markdown::serialize(&markdown::ParsedDocument {
        frontmatter: Frontmatter::default(),
        body: doc.tiptap_doc.clone(),
    })
}

/// `source-sync-<epoch>` reply: serialize and push to the source mirror.
/// Replies from before a new/open (older epoch) carry the previous
/// document's content and are dropped.
/// Returns true when the payload was a source-sync reply (consumed).
fn route_source_sync(app: &AppHandle, payload: &Value) -> bool {
    let Some(id) = payload.get("requestId").and_then(Value::as_str) else {
        return false;
    };
    let Some(epoch) = id
        .strip_prefix("source-sync-")
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return false;
    };
    let state = app.state::<AppState>();
    let mut doc = state.doc.lock().unwrap();
    if epoch != doc.epoch {
        return true; // stale reply — swallow
    }
    doc.source_sync_pending = false;
    let body = payload.get("doc").cloned().unwrap_or(Value::Null);
    drop(doc);
    if !body.is_object() {
        return true;
    }
    let text = markdown::serialize(&markdown::ParsedDocument {
        frontmatter: Frontmatter::default(),
        body,
    });
    bridge::send_to_source(app, "setMarkdownSource", json!({ "text": text }));
    true
}

// MARK: - save

pub fn save(app: &AppHandle) {
    request_save(app, false, false);
}

pub fn save_as(app: &AppHandle) {
    request_save(app, true, false);
}

/// Close-dialog "保存" — write, then close the window.
pub fn save_and_close(app: &AppHandle) {
    request_save(app, false, true);
}

fn request_save(app: &AppHandle, force_dialog: bool, close_after: bool) {
    let state = app.state::<AppState>();
    let (path, migrate_assets) = {
        let mut doc = state.doc.lock().unwrap();
        // A save handshake is already waiting on the webview; drop the dupe.
        if doc.pending_save.is_some() {
            return;
        }
        if !force_dialog && !doc.dirty {
            return;
        }
        let first_save = doc.file_path.is_none();
        let path = if force_dialog || first_save {
            let suggested = doc
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名.md".to_string());
            let picked = app
                .dialog()
                .file()
                .add_filter("Markdown", &["md"])
                .set_file_name(&suggested)
                .blocking_save_file();
            let Some(picked) = picked else { return };
            let Ok(mut path) = picked.into_path() else { return };
            if path.extension().is_none() {
                path.set_extension("md");
            }
            path
        } else {
            doc.file_path.clone().unwrap()
        };
        doc.file_path = Some(path.clone());
        (path, first_save)
    };

    let request_id = format!("save-{}", uuid::Uuid::new_v4());
    state.doc.lock().unwrap().pending_save = Some(PendingSave {
        request_id: request_id.clone(),
        path,
        migrate_assets,
        close_after,
    });
    bridge::send_to_editor(app, "requestDocumentJSON", json!({ "requestId": request_id }));
}

/// Bridge handler for the webview's `documentJSON {requestId, doc}` reply.
pub fn on_document_json(app: &AppHandle, payload: &Value) {
    // Source-mirror sync (M3.6) and whole-document AI commands (续写) share
    // this handshake channel — route by requestId before the save flow.
    if route_source_sync(app, payload) {
        return;
    }
    if crate::ai::consume_document_json(app, payload) {
        return;
    }
    let state = app.state::<AppState>();
    let pending = {
        let mut doc = state.doc.lock().unwrap();
        let matches = doc
            .pending_save
            .as_ref()
            .map(|p| {
                payload.get("requestId").and_then(Value::as_str) == Some(p.request_id.as_str())
            })
            .unwrap_or(false);
        if matches {
            doc.pending_save.take()
        } else {
            None
        }
    };
    let Some(pending) = pending else { return };

    let doc_json = payload.get("doc").cloned().unwrap_or(Value::Null);
    if !doc_json.is_object() {
        eprintln!("[save] documentJSON: missing/invalid doc payload");
        return;
    }

    let frontmatter = state.doc.lock().unwrap().frontmatter.clone();
    let parsed = markdown::ParsedDocument {
        frontmatter,
        body: doc_json,
    };
    let markdown_text = markdown::serialize(&parsed);

    if let Err(e) = write_atomic(&pending.path, markdown_text.as_bytes()) {
        // 保存失败 ⇒ 撤掉「保存并推送」的意图,否则标志会留在装填状态,
        // 下一次无关的保存会意外触发一次推送。
        state.doc.lock().unwrap().push_after_save = false;
        app.dialog()
            .message(format!("保存失败：{e}"))
            .title("Done.md")
            .buttons(MessageDialogButtons::Ok)
            .blocking_show();
        return;
    }
    if pending.migrate_assets {
        if let Err(e) = assets::migrate_assets_to_document(&state) {
            eprintln!("[save] asset migration failed: {e}");
        }
    }
    let continue_push = {
        let mut doc = state.doc.lock().unwrap();
        doc.dirty = false;
        bridge::update_title(app, &doc);
        // 「保存并推送」的续跑点 —— 取出即清零,避免任何后续保存
        // (比如关闭时的保存)意外触发一次推送。
        std::mem::take(&mut doc.push_after_save)
    };
    if pending.close_after {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.destroy();
        }
        return;
    }
    if continue_push {
        eprintln!("[feishu] 保存完成,续跑推送");
        crate::feishu::push_command::invoke(app);
    }
}

/// Write-temp-then-rename so a crash mid-save can't truncate the document.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("md.donemd-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Apply a document produced by the 飞书 pull coordinator: write it to disk,
/// adopt it as the in-memory state, and reload both panes.
///
/// Unlike `save`, this needs NO `requestDocumentJSON` handshake — the pulled
/// document IS the source of truth here, not the webview, so we serialize what
/// the coordinator returned and push it INTO the editor (the reverse direction
/// of a save). Mirrors the macOS `applyUpdatedDocumentAndSave`: the file on
/// disk is overwritten, so the dirty flag lands clean.
///
/// The caller has already gated on the dirty flag (pull overwrites the body,
/// so unsaved edits must be dealt with first — see `pull_command`).
pub(crate) fn apply_pulled_document(
    app: &AppHandle,
    path: &Path,
    parsed: markdown::ParsedDocument,
) -> Result<(), String> {
    let markdown_text = markdown::serialize(&parsed);
    write_atomic(path, markdown_text.as_bytes()).map_err(|e| format!("写入失败：{e}"))?;

    let state = app.state::<AppState>();
    let source_markdown;
    {
        let mut doc = state.doc.lock().unwrap();
        doc.file_path = Some(path.to_path_buf());
        doc.dirty = false;
        doc.tiptap_doc = parsed.body.clone();
        doc.frontmatter = parsed.frontmatter;
        // 折叠态与大纲跟随新正文重建(旧序数对新文档无意义)。
        doc.collapsed_headings.clear();
        doc.outline.clear();
        doc.active_heading = None;
        doc.pending_save = None;
        // 作废在途的 source-sync 回包 —— 它携带的是拉取前的旧正文。
        doc.epoch += 1;
        doc.source_sync_pending = false;
        source_markdown = serialize_body(&doc);
        bridge::update_title(app, &doc);
    }
    bridge::send_to_editor(app, "loadDocument", parsed.body);
    bridge::send_to_editor(app, "applyFold", json!({ "collapsed": [] }));
    push_source_snapshot(app, source_markdown);
    Ok(())
}

// MARK: - dirty confirmation

/// True when it's safe to discard the current document (not dirty, or the
/// user explicitly chose to discard).
pub fn confirm_discard_if_dirty(app: &AppHandle) -> bool {
    let dirty = app.state::<AppState>().doc.lock().unwrap().dirty;
    if !dirty {
        return true;
    }
    app.dialog()
        .message("当前文档有未保存的更改，确定要放弃吗？")
        .title("Done.md")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "放弃更改".to_string(),
            "取消".to_string(),
        ))
        .blocking_show()
}

/// Window close handler flow. Returns true if the close may proceed.
pub fn confirm_close(app: &AppHandle) -> CloseVerdict {
    let dirty = app.state::<AppState>().doc.lock().unwrap().dirty;
    if !dirty {
        return CloseVerdict::Close;
    }
    let result = app
        .dialog()
        .message("保存对当前文档的更改吗？")
        .title("Done.md")
        .buttons(MessageDialogButtons::YesNoCancelCustom(
            "保存".to_string(),
            "不保存".to_string(),
            "取消".to_string(),
        ))
        .blocking_show_with_result();
    match result {
        MessageDialogResult::Custom(label) if label == "保存" => CloseVerdict::SaveThenClose,
        MessageDialogResult::Custom(label) if label == "不保存" => CloseVerdict::Close,
        _ => CloseVerdict::Abort,
    }
}

pub enum CloseVerdict {
    Close,
    Abort,
    SaveThenClose,
}

// MARK: - 插入 menu

pub fn insert_image_dialog(app: &AppHandle) {
    let picked = app
        .dialog()
        .file()
        .add_filter("图片", &["png", "jpg", "jpeg", "gif", "webp", "heic", "svg", "bmp", "tiff"])
        .blocking_pick_file();
    insert_media(app, picked, "insertImage");
}

pub fn insert_video_dialog(app: &AppHandle) {
    let picked = app
        .dialog()
        .file()
        .add_filter("视频", &["mp4", "mov", "m4v", "webm"])
        .blocking_pick_file();
    insert_media(app, picked, "insertVideo");
}

/// Shared tail of 插入图片/视频: read bytes → assets dir → `insert* {src}`.
fn insert_media(app: &AppHandle, picked: Option<tauri_plugin_dialog::FilePath>, kind: &str) {
    let Some(picked) = picked else { return };
    let Ok(path) = picked.into_path() else { return };
    insert_media_from_path(app, &path, kind);
}

fn insert_media_from_path(app: &AppHandle, path: &Path, kind: &str) {
    let Some(filename) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[insert] read {} failed: {e}", path.display());
            return;
        }
    };
    let mime = assets::mime_type_for_filename(&filename);
    let state = app.state::<AppState>();
    match assets::import_asset(&state, &bytes, mime) {
        Ok((_filename, url, _markdown_path)) => {
            bridge::send_to_editor(app, kind, json!({ "src": url }));
        }
        Err(e) => eprintln!("[insert] import failed: {e}"),
    }
}

/// Parse the process arguments for a file-association launch path
/// (Windows passes the opened file as argv[1]; macOS uses OpenURLs events).
pub fn pending_open_from_argv() -> Option<PathBuf> {
    let arg = std::env::args().nth(1)?;
    let path = PathBuf::from(&arg);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    if matches!(ext.as_str(), "md" | "markdown" | "mdown" | "mkd") && path.is_file() {
        Some(path)
    } else {
        None
    }
}
