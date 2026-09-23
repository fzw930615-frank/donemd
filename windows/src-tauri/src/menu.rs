//! Native menu bar, mirroring the macOS app's command structure
//! (`donemdApp.swift` CommandGroups / CommandMenus).
//!
//! Accelerator reality check: with the webview focused, WebView2 usually
//! swallows key events before the menu's accelerator table sees them, so
//! `web/src/main.ts` has a JS key fallback for the file commands that MUST
//! work (Ctrl+N/O/S/Shift+S → `menuCommand` envelopes). Format keys are
//! covered by Tiptap's own Mod-* bindings in the editor; the menu entries
//! remain for discoverability and mouse access.

use serde_json::json;
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuEvent, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Manager, WindowEvent};

use crate::bridge;
use crate::document::{self, CloseVerdict};

pub fn build(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let item = |id: &str, text: &str, accel: Option<&str>| {
        let b = MenuItemBuilder::with_id(id, text);
        let b = match accel {
            Some(a) => b.accelerator(a),
            None => b,
        };
        b.build(app)
    };

    let file = SubmenuBuilder::new(app, "文件")
        .item(&item("new", "新建", Some("CmdOrCtrl+N"))?)
        .item(&item("open", "打开…", Some("CmdOrCtrl+O"))?)
        .separator()
        .item(&item("save", "存储", Some("CmdOrCtrl+S"))?)
        .item(&item("saveAs", "另存为…", Some("CmdOrCtrl+Shift+S"))?)
        .separator()
        .item(&item("aiSettings", "设置…", Some("CmdOrCtrl+,"))?)
        .separator()
        .item(&item("quit", "退出", None)?)
        .build()?;

    // macOS toggles the outline with ⌃⌘S (`显示文档大纲` under the 显示
    // command group). ⌃ has no Windows counterpart and Ctrl+Shift+S is
    // already 另存为, so the sidebar gets Ctrl+Shift+O (O = outline). The
    // WebView2 accelerator caveat in the header applies — main.ts mirrors
    // this key through `menuCommand {command:"toggleOutline"}`.
    let view = SubmenuBuilder::new(app, "视图")
        .item(
            &CheckMenuItemBuilder::with_id("view:outline", "文档大纲")
                .accelerator("CmdOrCtrl+Shift+O")
                .checked(false)
                .build(app)?,
        )
        .build()?;

    // CmdOrCtrl = Ctrl on Windows. Keys mirror donemdApp.swift's 格式 menu
    // (⌘→Ctrl, ⌥→Alt, ⇧→Shift).
    let format = SubmenuBuilder::new(app, "格式")
        .item(&item("fmt:bold", "加粗", Some("CmdOrCtrl+B"))?)
        .item(&item("fmt:italic", "斜体", Some("CmdOrCtrl+I"))?)
        .item(&item("fmt:strike", "删除线", Some("CmdOrCtrl+Shift+X"))?)
        .item(&item("fmt:code", "行内代码", Some("CmdOrCtrl+E"))?)
        .item(&item("fmt:link", "链接…", Some("CmdOrCtrl+K"))?)
        .item(&item("fmt:clear", "清除格式", Some(r"CmdOrCtrl+\"))?)
        .separator()
        // Heading levels (Windows convention Ctrl+Alt+N; the editor binds
        // these in Tiptap's keymap — the accelerators here are for menu
        // display/discoverability, see the header caveat).
        .item(&item("fmt:paragraph", "正文", Some("CmdOrCtrl+Alt+0"))?)
        .item(&item("fmt:heading1", "一级标题", Some("CmdOrCtrl+Alt+1"))?)
        .item(&item("fmt:heading2", "二级标题", Some("CmdOrCtrl+Alt+2"))?)
        .item(&item("fmt:heading3", "三级标题", Some("CmdOrCtrl+Alt+3"))?)
        .item(&item("fmt:heading4", "四级标题", Some("CmdOrCtrl+Alt+4"))?)
        .item(&item("fmt:heading5", "五级标题", Some("CmdOrCtrl+Alt+5"))?)
        .item(&item("fmt:heading6", "六级标题", Some("CmdOrCtrl+Alt+6"))?)
        .separator()
        .item(&item("fmt:blockquote", "引用", Some("CmdOrCtrl+Shift+B"))?)
        .item(&item("fmt:codeBlock", "代码块", Some("CmdOrCtrl+Alt+C"))?)
        .item(&item("fmt:bulletList", "无序列表", Some("CmdOrCtrl+Shift+8"))?)
        .item(&item("fmt:orderedList", "有序列表", Some("CmdOrCtrl+Shift+7"))?)
        .item(&item("fmt:taskList", "任务列表", None)?)
        .item(&item("fmt:callout", "高亮块", None)?)
        .build()?;

    let insert = SubmenuBuilder::new(app, "插入")
        .item(&item("ins:image", "图片…", Some("CmdOrCtrl+Shift+I"))?)
        .item(&item("ins:video", "视频…", None)?)
        .item(&item("ins:table", "表格", Some("CmdOrCtrl+Alt+T"))?)
        .build()?;

    // 飞书同步菜单(F4)。macOS 侧是 `CommandMenu("飞书")`。Windows 现已接
    // 拉取(F4-a)、URL 新建(F4-b)与推送(F3-c 第一批)。仍缺:撤销上次
    // 拉取、段式推送(含飞书专有块的文档目前会被推送命令拒绝)。
    //
    // 快捷键:macOS 拉取用 ⌘⌥O、推送用 ⌘⌥S;Windows 沿用同形。
    // 菜单加速键在 WebView2 聚焦时常被吞(见本文件头注),所以这里主要是
    // 可发现性与鼠标可达性。
    let feishu = SubmenuBuilder::new(app, "飞书")
        .item(&item("feishu:import", "从飞书链接新建…", None)?)
        .separator()
        .item(&item("feishu:pull", "从飞书拉取", Some("CmdOrCtrl+Alt+O"))?)
        .item(&item("feishu:push", "推送到飞书", Some("CmdOrCtrl+Alt+S"))?)
        .separator()
        .item(&item("feishu:settings", "飞书同步设置…", None)?)
        .build()?;

    MenuBuilder::new(app)
        .items(&[&file, &view, &format, &insert, &feishu])
        .build()
}

pub fn on_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().0.as_str();
    match id {
        "new" => document::new_document(app),
        "open" => document::open_dialog(app),
        "save" => document::save(app),
        "saveAs" => document::save_as(app),
        "quit" => {
            // Route through the close handler so the dirty check runs.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.close();
            }
        }
        "aiSettings" => crate::ai::open_settings(app),
        "view:outline" => crate::outline::toggle(app),
        "feishu:pull" => crate::feishu::pull_command::invoke(app),
        "feishu:import" => crate::feishu::pull_command::open_import_modal(app),
        "feishu:push" => crate::feishu::push_command::invoke(app),
        // 飞书同步与 AI 共用同一个设置窗(两个 tab),消息名沿用历史契约。
        "feishu:settings" => crate::ai::open_settings(app),
        "ins:image" => document::insert_image_dialog(app),
        "ins:video" => document::insert_video_dialog(app),
        "ins:table" => bridge::send_to_editor(app, "insertTable", json!(null)),
        _ => {
            if let Some(cmd) = id.strip_prefix("fmt:") {
                bridge::send_to_editor(app, "formatCommand", json!({ "cmd": cmd }));
            }
        }
    }
}

/// Window-event hub. Dirty-close confirmation: intercept the MAIN window
/// close while the document has unsaved changes (`保存` writes then closes;
/// `不保存` closes; `取消` aborts). Secondary windows (AI 设置) close freely —
/// the handler is keyed on the window label so the settings window never
/// triggers the document save flow. OS theme flips repaint the native canvas
/// behind the transparent page (see `theme.rs`), and main-window resizes
/// re-dock the outline sidebar (`outline.rs`).
pub fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    if let WindowEvent::ThemeChanged(t) = event {
        crate::theme::on_theme_changed(window, *t);
        return;
    }
    if window.label() != "main" {
        return;
    }
    if let WindowEvent::Resized(_) = event {
        // Re-dock the outline sidebar + source mirror panes.
        crate::layout::relayout(&window.app_handle());
        return;
    }
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let app = window.app_handle();
    match document::confirm_close(app) {
        CloseVerdict::Close => {}
        CloseVerdict::Abort => api.prevent_close(),
        CloseVerdict::SaveThenClose => {
            api.prevent_close();
            document::save_and_close(app);
        }
    }
}
