//! 「从飞书拉取」用户命令 — `FeishuPullCommand.swift` 的移植(F4)。
//!
//! 把 F3-b 的 [`super::pull::PullCoordinator`] 接到菜单上:前置条件把关 →
//! 脏文档处置 → 跑协调器 → 应用结果。
//!
//! **线程划分(与 Swift 的结构差异,有意为之)**:阻塞式对话框只在同步的
//! [`preflight`] 里弹,那是菜单事件的主线程上下文 —— 与 `document.rs` 已
//! 验证可行的路径一致。异步的 [`run`] 里**一律不弹对话框**,只发 toast:
//! 从 async 任务调 `blocking_show` 会占住运行时工作线程,而且进度/结果
//! 这类信息本来就更适合非阻塞反馈。
//!
//! 未移植(仍属 Swift 侧的后续 issue 或本批之外):撤销快照
//! (`FeishuPullSnapshotStore`)、URL 导入新建文件、模态进度条。

use serde_json::json;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};

use super::api::{FeishuApi, FeishuApiError};
use super::image_download::AssetsImageWriter;
use super::pull::{Progress, PullCoordinator, PullError};
use crate::bridge;
use crate::markdown::frontmatter::Frontmatter;
use crate::state::AppState;

/// 前置检查通过后要带进异步阶段的东西。
pub struct PullPlan {
    doc_token: String,
    path: std::path::PathBuf,
    existing: Frontmatter,
}

/// toast 种类 —— 必须与 web 侧 `renderToast` 的 kind 联合类型逐字对齐
/// (`'progress' | 'done' | 'error'`),写错的 kind 只会得到无样式的浮层。
const TOAST_PROGRESS: &str = "progress";
const TOAST_DONE: &str = "done";
const TOAST_ERROR: &str = "error";

fn toast(app: &AppHandle, message: impl Into<String>, kind: &str) {
    bridge::send_to_editor(
        app,
        "feishuSyncToast",
        json!({ "message": message.into(), "kind": kind }),
    );
}

fn alert(app: &AppHandle, title: &str, message: &str) {
    app.dialog()
        .message(message)
        .title(title)
        .buttons(MessageDialogButtons::Ok)
        .blocking_show();
}

/// 同步前置检查(主线程)。顺序对齐 Swift:未绑定 → 未保存 → 未配置凭证
/// → 未登录 → 脏文档处置。任一不过返回 `None`,并且**已经**把原因告诉
/// 用户了。
pub fn preflight(app: &AppHandle) -> Option<PullPlan> {
    let state = app.state::<AppState>();

    let (file_path, frontmatter, dirty) = {
        let doc = state.doc.lock().unwrap();
        (
            doc.file_path.clone(),
            doc.frontmatter.clone(),
            doc.dirty,
        )
    };

    // ① 未绑定飞书
    let Some(doc_token) = frontmatter
        .feishu
        .as_ref()
        .and_then(|f| f.doc_token.clone())
        .filter(|t| !t.is_empty())
    else {
        alert(
            app,
            "当前文档未绑定飞书",
            "这份文档还没有和任何飞书文档建立关联，所以不知道要从哪里拉取。\n\n\
             想把一篇飞书文档拿到本地：用菜单「飞书 → 从飞书链接新建…」，\
             粘贴文档链接即可，会自动建立关联。\n\n\
             （已有关联的文档，其 frontmatter 里会有 feishu.doc_token 字段。）",
        );
        return None;
    };

    // ② 未保存 —— 拉取会覆盖磁盘文件,所以得先有落点
    let Some(path) = file_path else {
        alert(
            app,
            "请先保存当前文档",
            "拉取会覆盖磁盘上的文件，所以当前文档需要先有一个保存路径（按 Ctrl+S 保存）。",
        );
        return None;
    };

    // ③ 未配置应用凭证
    if state.feishu.current_app_config().is_none() {
        alert(
            app,
            "未配置飞书应用凭证",
            "请到「设置 → 飞书同步」填好 App ID / App Secret / 重定向 URL。",
        );
        return None;
    }

    // ④ 未登录 —— 令牌供给不会自动拉起浏览器登录(它把「未认证」映射成
    // Unauthorized),所以这里提前说清楚,而不是让用户吃一个 401。
    state.feishu.refresh_auth_state();
    if !matches!(
        state.feishu.auth_state(),
        super::manager::AuthState::LoggedIn { .. }
    ) {
        alert(
            app,
            "尚未登录飞书",
            "请到「设置 → 飞书同步」点「登录飞书」完成授权后再拉取。",
        );
        return None;
    }

    // ⑤ 脏文档处置 —— 拉取会覆盖正文,未保存的编辑必须先有个交代。
    if dirty && !resolve_unsaved_changes(app, &path) {
        return None;
    }

    Some(PullPlan {
        doc_token,
        path,
        existing: frontmatter,
    })
}

/// 三选对话框:放弃未保存的编辑 / 先存一份副本 / 取消。
/// 返回 `true` 表示可以继续拉取。
///
/// 「副本」把当前磁盘文件复制成 `<名字>.local.md` 放在旁边 —— 对齐 Swift
/// 的 `~filename.local.md` 意图(别让用户丢东西),但存的是**磁盘上的**版本
/// 而非内存版本:内存版本要经 `requestDocumentJSON` 握手才拿得到,而这里
/// 是同步上下文。差异写在对话框文案里,不含糊其辞。
fn resolve_unsaved_changes(app: &AppHandle, path: &std::path::Path) -> bool {
    let result = app
        .dialog()
        .message(
            "当前文档有未保存的更改，拉取会用飞书的内容覆盖正文。\n\n\
             「保存副本」会把磁盘上的当前版本另存为一份 .local.md 备份（内存中尚未保存的编辑不在其中）。",
        )
        .title("拉取会覆盖未保存的更改")
        .buttons(MessageDialogButtons::YesNoCancelCustom(
            "放弃更改并拉取".to_string(),
            "保存副本后拉取".to_string(),
            "取消".to_string(),
        ))
        .blocking_show_with_result();

    match result {
        MessageDialogResult::Custom(label) if label == "放弃更改并拉取" => true,
        MessageDialogResult::Custom(label) if label == "保存副本后拉取" => {
            match backup_path(path) {
                Some(backup) => match std::fs::copy(path, &backup) {
                    Ok(_) => {
                        eprintln!("[feishu] 拉取前备份到 {}", backup.display());
                        true
                    }
                    Err(e) => {
                        alert(app, "备份失败", &format!("无法写入备份文件：{e}\n\n已取消拉取。"));
                        false
                    }
                },
                None => {
                    alert(app, "备份失败", "无法推导备份文件名，已取消拉取。");
                    false
                }
            }
        }
        _ => false,
    }
}

/// `foo.md` → `foo.local.md`(同目录)。
fn backup_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    Some(path.with_file_name(format!("{stem}.local.md")))
}

/// 这个错误形状是否意味着「传进去的不是一个 docx 文档 id」?
///
/// 飞书对「拿 wiki 节点 token 调 docx 端点」回的是 400 + code 1770001
/// `invalid param`(2026-09-23 真机确认),不存在的文档回 404。两者都说明
/// 「换个解释方式再试」,而 401/403/scope/网络/限流与 token 形态无关,
/// 绝不能被 wiki 回退掩盖 —— 否则用户看到的是「wiki 解析失败」,真因
/// 却是没登录或缺 scope。
fn looks_like_wrong_id_kind(error: &FeishuApiError) -> bool {
    matches!(
        error,
        FeishuApiError::BadRequest { .. } | FeishuApiError::NotFound { .. }
    )
}

/// 把 frontmatter 里的 token 解析成一个**确定可拉取的 docx id**。
///
/// **不靠猜**。早先的实现按 URL 形态 + `wik` 前缀启发式判别,两头都漏:
/// 飞书新版 wiki 节点 token 常常不透明、不带前缀;而一旦解析成功把
/// `doc_token` 写回成 docx token,残留的 wiki `doc_url` 又会让下一次拉取
/// 误判成「还需解析」,拿 docx token 去调 wiki 端点,再吃一个 400。
///
/// 改为确定性的两段式:先用最便宜的探针(单 GET revision)试 docx,
/// 成功即确认;只有在错误形状明确是「这不是 docx id」时才回退去解析
/// wiki 节点。代价是未绑定过的 wiki 文档首次拉取多一个失败请求 —— 而
/// 解析结果会写回 `doc_token`,所以这笔开销只付一次。
async fn resolve_docx_token<A: FeishuApi>(
    app: &AppHandle,
    api: &A,
    doc_token: &str,
) -> Option<String> {
    match api.get_document_revision(doc_token).await {
        Ok(_) => return Some(doc_token.to_string()),
        Err(e) if looks_like_wrong_id_kind(&e) => {
            eprintln!("[feishu] docx 探针未通过（{e}），改按 wiki 节点解析");
        }
        Err(e) => {
            // 与 token 形态无关的失败,原样上报,别让 wiki 回退掩盖真因。
            eprintln!("[feishu] 拉取前探针失败:{e}");
            toast(app, format!("拉取失败：{e}"), TOAST_ERROR);
            return None;
        }
    }

    toast(app, "正在解析 wiki 节点…", TOAST_PROGRESS);
    match api.resolve_wiki_node(doc_token).await {
        Ok(resolution) => {
            if resolution.obj_type != "docx" {
                eprintln!(
                    "[feishu] wiki 节点包裹的是 {},非 docx",
                    resolution.obj_type
                );
                toast(
                    app,
                    format!(
                        "这个 wiki 节点里是「{}」，Done.md 目前只能同步文档（docx）",
                        resolution.obj_type
                    ),
                    TOAST_ERROR,
                );
                return None;
            }
            eprintln!("[feishu] wiki 解析得到 docx {}", resolution.obj_token);
            Some(resolution.obj_token)
        }
        Err(e) => {
            eprintln!("[feishu] wiki 节点解析失败:{e}");
            toast(
                app,
                format!("这个 token 既不是文档也不是可解析的 wiki 节点：{e}"),
                TOAST_ERROR,
            );
            None
        }
    }
}

/// 异步阶段:跑协调器并应用结果。只发 toast,不弹对话框(见模块头注)。
pub async fn run(app: AppHandle, plan: PullPlan) {
    let state = app.state::<AppState>();
    let Some(api) = state.feishu.http_api() else {
        // preflight 已查过配置,走到这里说明期间被清掉了。
        toast(&app, "飞书应用凭证已失效，请重新配置后再试", TOAST_ERROR);
        return;
    };

    let old_revision = plan
        .existing
        .feishu
        .as_ref()
        .and_then(|f| f.last_pulled_revision);

    // 解析出确定可拉取的 docx id(wiki 节点在此被换成它包裹的 obj_token)。
    // 结果作为新的 doc_token 交给协调器,于是会写回 frontmatter;`doc_url`
    // 不动(传 None),人类可导航的 wiki 链接得以保留。
    let Some(doc_token) = resolve_docx_token(&app, &api, &plan.doc_token).await else {
        return; // 失败原因已由 resolve_docx_token 告知用户
    };

    eprintln!("[feishu] 拉取开始 token={doc_token}");
    toast(&app, "正在从飞书拉取…", TOAST_PROGRESS);

    let writer = AssetsImageWriter { state: &state };
    let coordinator = PullCoordinator::with_image_writer(&api, &writer);

    let result = coordinator
        .pull(
            &doc_token,
            Some(&plan.existing),
            None,
            None,
            |event| {
                // 进度事件目前只进日志 + 图片阶段发一条 toast;模态进度条
                // 属 F4 的 UI 后续批次。
                eprintln!("[feishu] 拉取进度 {event:?}");
                if let Progress::ImageStageStarted { total } = event {
                    if total > 0 {
                        // 图片是拉取里最慢的一段,值得单独告知。
                        // 这里不用 toast() 以免和 app 的可变借用打架 —— 直接发。
                        bridge::send_to_editor(
                            &app,
                            "feishuSyncToast",
                            json!({ "message": format!("正在下载 {total} 张图片…"), "kind": TOAST_PROGRESS }),
                        );
                    }
                }
            },
        )
        .await;

    let result = match result {
        Ok(r) => r,
        Err(PullError::Cancelled) => {
            eprintln!("[feishu] 拉取已取消");
            return;
        }
        Err(PullError::ApiFailed(e)) => {
            eprintln!("[feishu] 拉取失败:{e}");
            toast(&app, format!("拉取失败：{e}"), TOAST_ERROR);
            return;
        }
    };

    let new_revision = result
        .updated_document
        .frontmatter
        .feishu
        .as_ref()
        .and_then(|f| f.last_pulled_revision);

    // 空拉取:revision 没动 ⇒ 飞书侧自上次拉取以来没有变化。**不**应用,
    // 免得白翻脏标记、白重载编辑器、白冒覆盖用户在途编辑的风险 ——
    // 如实告诉用户「没有更新」(对齐 Swift 的 no-op 分支)。
    if let (Some(old), Some(new)) = (old_revision, new_revision) {
        if old == new {
            eprintln!("[feishu] 空拉取:revision 仍为 {new},不改动文档");
            toast(&app, format!("飞书暂无更新（revision {new}），文档保持原样"), TOAST_DONE);
            return;
        }
    }

    let warning_count = result.warnings.len();
    let image_note = match &result.image_report {
        Some(r) if !r.failed_tokens.is_empty() => format!(
            "，图片 {} 张成功 / {} 张失败",
            r.downloaded_count,
            r.failed_tokens.len()
        ),
        Some(r) if r.downloaded_count > 0 => format!("，下载 {} 张图片", r.downloaded_count),
        _ => String::new(),
    };

    if let Err(e) = crate::document::apply_pulled_document(&app, &plan.path, result.updated_document)
    {
        eprintln!("[feishu] 应用拉取结果失败:{e}");
        toast(&app, format!("拉取成功但写入失败：{e}"), TOAST_ERROR);
        return;
    }

    let revision_note = new_revision
        .map(|r| format!("revision {r}"))
        .unwrap_or_else(|| "已更新".to_string());
    let warning_note = if warning_count > 0 {
        format!("，{warning_count} 条转换提示", )
    } else {
        String::new()
    };
    eprintln!("[feishu] 拉取完成 {revision_note}{image_note}{warning_note}");
    toast(
        &app,
        format!("已从飞书拉取（{revision_note}）{image_note}{warning_note}"),
        TOAST_DONE,
    );
}

/// 菜单入口:同步把关,通过后把异步阶段丢给运行时。
pub fn invoke(app: &AppHandle) {
    let Some(plan) = preflight(app) else { return };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        run(app, plan).await;
    });
}

// MARK: - 从飞书链接新建(F4-b)

/// 菜单「从飞书链接新建…」:让 web 层打开 URL 录入弹窗。
///
/// 输入框做在前端而非原生对话框,两个原因:① `tauri-plugin-dialog` 没有
/// 文本输入对话框,原生路线要么加剪贴板依赖要么再开一个窗口;② 这个应用
/// 自己的 UI 语言在 web 层(斜杠菜单/气泡菜单/抽屉),原生 Win32 弹框与
/// 那套深色毛玻璃界面割裂。用户提交后经 `feishuImportFromUrl` 回到原生。
pub fn open_import_modal(app: &AppHandle) {
    bridge::send_to_editor(app, "feishuOpenImportModal", json!({}));
}

/// `feishuImportFromUrl` 信封处理:解析 URL → 拉取 → 让用户选保存位置。
///
/// **先拉取再问路径**:URL 写错或没权限时,用户不该已经白选过一次保存
/// 位置。代价是成功路径上文件对话框来得稍晚一点,值得。
pub fn handle_import_from_url(app: &AppHandle, payload: &serde_json::Value) {
    let raw = payload
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    let Some(token) = super::url::extract_doc_token(&raw) else {
        alert(
            app,
            "无法识别这个链接",
            "请粘贴飞书云文档的链接，形如：\n\n\
             https://你的租户.feishu.cn/wiki/xxxxxxxx\n\
             https://你的租户.feishu.cn/docx/xxxxxxxx\n\n\
             也可以直接粘贴文档 token。",
        );
        return;
    };

    // 凭证与登录态把关 —— 与 preflight 同样的两道,复用文案。
    let state = app.state::<AppState>();
    if state.feishu.current_app_config().is_none() {
        alert(
            app,
            "未配置飞书应用凭证",
            "请到「设置 → 飞书同步」填好 App ID / App Secret / 重定向 URL。",
        );
        return;
    }
    state.feishu.refresh_auth_state();
    if !matches!(
        state.feishu.auth_state(),
        super::manager::AuthState::LoggedIn { .. }
    ) {
        alert(
            app,
            "尚未登录飞书",
            "请到「设置 → 飞书同步」点「登录飞书」完成授权后再导入。",
        );
        return;
    }

    let app = app.clone();
    // 原始输入若是 URL 就存进 frontmatter 的 doc_url —— 飞书文档 URL 落在
    // 租户子域上,无法从 token 反推,这是唯一知道它的时机(对齐 Swift 的
    // URL 导入路径)。裸 token 输入则没有 URL 可存。
    let doc_url = raw.contains("://").then(|| raw.trim().to_string());
    tauri::async_runtime::spawn(async move {
        run_import(app, token, doc_url).await;
    });
}

async fn run_import(app: AppHandle, token: String, doc_url: Option<String>) {
    let state = app.state::<AppState>();
    let Some(api) = state.feishu.http_api() else {
        toast(&app, "飞书应用凭证已失效，请重新配置后再试", TOAST_ERROR);
        return;
    };

    let Some(doc_token) = resolve_docx_token(&app, &api, &token).await else {
        return; // 失败原因已告知
    };

    eprintln!("[feishu] 导入开始 token={doc_token}");
    toast(&app, "正在从飞书拉取…", TOAST_PROGRESS);

    let writer = AssetsImageWriter { state: &state };
    let coordinator = PullCoordinator::with_image_writer(&api, &writer);
    // 新建路径没有既有 frontmatter;doc_url 在此刻写入。
    let result = coordinator
        .pull(&doc_token, None, doc_url.as_deref(), None, |event| {
            eprintln!("[feishu] 导入进度 {event:?}");
            if let Progress::ImageStageStarted { total } = event {
                if total > 0 {
                    bridge::send_to_editor(
                        &app,
                        "feishuSyncToast",
                        json!({ "message": format!("正在下载 {total} 张图片…"), "kind": TOAST_PROGRESS }),
                    );
                }
            }
        })
        .await;

    let result = match result {
        Ok(r) => r,
        Err(PullError::Cancelled) => return,
        Err(PullError::ApiFailed(e)) => {
            eprintln!("[feishu] 导入失败:{e}");
            toast(&app, format!("拉取失败：{e}"), TOAST_ERROR);
            return;
        }
    };

    let image_note = match &result.image_report {
        Some(r) if !r.failed_tokens.is_empty() => format!(
            "，图片 {} 张成功 / {} 张失败",
            r.downloaded_count,
            r.failed_tokens.len()
        ),
        Some(r) if r.downloaded_count > 0 => format!("，下载 {} 张图片", r.downloaded_count),
        _ => String::new(),
    };
    let suggested = suggest_file_name(&result.updated_document.body);

    // 非阻塞的保存对话框(回调版)。阻塞版会占住 async 运行时的工作线程;
    // 写盘与状态采纳都是同步操作,在回调里做完即可。
    let app_for_save = app.clone();
    let parsed = result.updated_document;
    app.dialog()
        .file()
        .add_filter("Markdown", &["md"])
        .set_file_name(&suggested)
        .save_file(move |picked| {
            let Some(picked) = picked else {
                eprintln!("[feishu] 导入已取消(未选保存位置)");
                return;
            };
            let Ok(mut path) = picked.into_path() else { return };
            if path.extension().is_none() {
                path.set_extension("md");
            }
            match crate::document::apply_pulled_document(&app_for_save, &path, parsed) {
                Ok(()) => {
                    eprintln!("[feishu] 导入完成 → {}", path.display());
                    bridge::send_to_editor(
                        &app_for_save,
                        "feishuSyncToast",
                        json!({
                            "message": format!("已从飞书新建文档{image_note}"),
                            "kind": TOAST_DONE,
                        }),
                    );
                }
                Err(e) => {
                    eprintln!("[feishu] 导入写盘失败:{e}");
                    bridge::send_to_editor(
                        &app_for_save,
                        "feishuSyncToast",
                        json!({ "message": format!("写入失败：{e}"), "kind": TOAST_ERROR }),
                    );
                }
            }
        });
}

/// 从正文首个标题推荐文件名 —— 飞书页标题会被转换器放成首个 H1,
/// 所以这通常就是文档名。取不到则退回通用名。
fn suggest_file_name(body: &serde_json::Value) -> String {
    let title = first_heading_text(body).unwrap_or_default();
    let cleaned: String = title
        .chars()
        // Windows 文件名非法字符 + 控制字符一律换成下划线。
        .map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        "飞书文档.md".to_string()
    } else {
        // 留出扩展名与路径余量,别构造出超长文件名。
        let truncated: String = cleaned.chars().take(60).collect();
        format!("{truncated}.md")
    }
}

/// 深度优先找首个 heading 节点的纯文本。
fn first_heading_text(node: &serde_json::Value) -> Option<String> {
    if crate::markdown::tiptap::node_type(node) == "heading" {
        let text = collect_text(node);
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    crate::markdown::tiptap::content(node)
        .iter()
        .find_map(first_heading_text)
}

fn collect_text(node: &serde_json::Value) -> String {
    if crate::markdown::tiptap::node_type(node) == "text" {
        return node
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    crate::markdown::tiptap::content(node)
        .iter()
        .map(collect_text)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_sits_next_to_the_document() {
        let p = std::path::Path::new("D:\\docs\\note.md");
        assert_eq!(
            backup_path(p).unwrap(),
            std::path::PathBuf::from("D:\\docs\\note.local.md")
        );
    }

    #[test]
    fn backup_path_handles_dotted_names() {
        let p = std::path::Path::new("/tmp/my.notes.md");
        assert_eq!(
            backup_path(p).unwrap(),
            std::path::PathBuf::from("/tmp/my.notes.local.md")
        );
    }

    // MARK: - 「这不是 docx id」的错误形状判别
    //
    // 这是回退去解析 wiki 节点的唯一闸门。判错的代价是不对称的:
    // 把 401 误判成「换个解释方式」会让用户看到「既不是文档也不是 wiki
    // 节点」,而真因是没登录 —— 所以这组用例逐个变体钉死。

    #[test]
    fn bad_request_and_not_found_trigger_wiki_fallback() {
        // 真机实测:拿 wiki 节点 token 调 docx 端点 → 400 / code 1770001。
        assert!(looks_like_wrong_id_kind(&FeishuApiError::BadRequest {
            http_status: 400,
            code: Some(1770001),
            message: Some("invalid param".into()),
        }));
        assert!(looks_like_wrong_id_kind(&FeishuApiError::NotFound {
            resource: "doxcnX".into(),
        }));
    }

    #[test]
    fn auth_and_transport_failures_never_trigger_wiki_fallback() {
        // 这些与 token 形态无关,必须原样上报,否则真因被掩盖。
        assert!(!looks_like_wrong_id_kind(&FeishuApiError::Unauthorized));
        assert!(!looks_like_wrong_id_kind(&FeishuApiError::Forbidden {
            message: None
        }));
        assert!(!looks_like_wrong_id_kind(
            &FeishuApiError::ScopeInsufficient { detail: None }
        ));
        assert!(!looks_like_wrong_id_kind(&FeishuApiError::RateLimited));
        assert!(!looks_like_wrong_id_kind(&FeishuApiError::NetworkUnreachable(
            "断网".into()
        )));
        assert!(!looks_like_wrong_id_kind(&FeishuApiError::ServerError {
            http_status: 500,
            code: None,
            message: None,
        }));
        assert!(!looks_like_wrong_id_kind(&FeishuApiError::DecodeFailed(
            "形状不符".into()
        )));
    }
}
