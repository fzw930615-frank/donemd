// Windows GUI 子系统:双击启动不附带控制台窗口(Tauri 模板标配)。
// debug 构建保留控制台 — eprintln! 的 [feishu]/[ai] 日志印在那里,
// 联调时是唯一的无调试器可见性;release/安装包静默。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Done.md for Windows — Tauri shell around the shared `web/` editor.
//!
//! The macOS app (SwiftUI + WKWebView) is untouched; this binary is the
//! Windows counterpart, reusing the same single-file web bundle and the same
//! versioned bridge envelope (`tasks/bridge-contract.md`).

mod ai;
mod assets;
mod bridge;
mod document;
mod feishu;
mod layout;
mod markdown;
mod menu;
mod outline;
mod source;
mod state;
mod theme;

use state::AppState;
use tauri::Manager;

fn main() {
    // File-association launch: Windows passes the opened file as argv[1].
    let pending_open = document::pending_open_from_argv();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::with_pending_open(pending_open))
        .register_uri_scheme_protocol(assets::SCHEME, assets::handle_asset_request)
        .invoke_handler(tauri::generate_handler![
            bridge::bridge_dispatch,
            ai::ai_settings_load,
            ai::ai_settings_save_key,
            ai::ai_settings_clear_key,
            ai::ai_settings_refresh_models,
            ai::ai_settings_update,
            feishu::feishu_settings_load,
            feishu::feishu_save_app_config,
            feishu::feishu_clear_app_config,
            feishu::feishu_login,
            feishu::feishu_logout,
        ])
        .setup(|app| {
            let m = menu::build(app.handle())?;
            app.set_menu(m)?;
            theme::apply_system_canvas(app.handle());
            // M8: 飞书同步启动渲染 — 只读非机密镜像 `feishu-state.json`,
            // 不碰凭据管理器(读取静默且快,镜像只服务主窗启动徽标)。
            app.state::<AppState>().feishu.boot();
            // M3.6: dock the read-only Markdown source mirror (macOS parity:
            // always visible, no toggle). The outline sidebar is created
            // on-demand instead (视图 → 文档大纲).
            source::setup(app.handle());
            Ok(())
        })
        .on_menu_event(menu::on_event)
        .on_window_event(menu::on_window_event)
        .run(tauri::generate_context!())
        .expect("error while running Done.md");
}
