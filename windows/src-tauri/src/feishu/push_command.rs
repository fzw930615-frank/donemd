//! 「推送到飞书」用户命令 — `FeishuPushCommand.swift` 的移植(F3-c 第一批)。
//!
//! 覆盖两个场景:
//!   A. **本地新建文档**(未绑定)→ 在飞书侧 `create_document` 建一篇 →
//!      推正文 → 把 doc_token 写回 frontmatter
//!   B. **已绑定文档**(拉取过或推送过)→ 冲突预检 → 覆盖远端正文
//!
//! 线程划分同拉取侧:阻塞对话框只在同步的 [`preflight`] 与结果处置里弹
//! (主线程),异步阶段只发 toast。
//!
//! # 孤儿文档:本文件最重要的一段
//!
//! 场景 A 里 `create_document` 成功而推正文失败时,远端已经多了一篇空文档。
//! 此时**必须先把 doc_token 落盘**再让用户重试 —— 否则下次推送又走「未绑定
//! → 新建」,再建一篇,孤儿越攒越多。所以 `PartialSuccess` 分支里,写盘发生
//! 在告知用户之前,且即使写盘也失败,提示里也要把 token 原文给出来。

use serde_json::json;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

use super::image_upload::AssetsImageReader;
use super::push::{Progress, PushCoordinator, PushError, PushResult};
use crate::bridge;
use crate::markdown::frontmatter::Frontmatter;
use crate::state::AppState;

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

/// 前置检查通过后带进异步阶段的东西。
pub struct PushPlan {
    path: std::path::PathBuf,
    frontmatter: Frontmatter,
    body: serde_json::Value,
    /// 新建远端文档时的标题 —— 取正文首个标题,退回文件名。
    title: String,
    /// 用户已在冲突对话框里选择「仍然覆盖」。
    force: bool,
}

/// 同步前置检查(主线程)。
///
/// 与拉取不同,推送**不要求**文档已绑定 —— 未绑定正是场景 A。但要求
/// 文档已保存:推送要写回 frontmatter,没有落点就无处可写;而且正文
/// 也得先落盘,否则「推上去的」和「本地文件里的」会不一致。
pub fn preflight(app: &AppHandle) -> Option<PushPlan> {
    let state = app.state::<AppState>();
    let (file_path, frontmatter, body, dirty) = {
        let doc = state.doc.lock().unwrap();
        (
            doc.file_path.clone(),
            doc.frontmatter.clone(),
            doc.tiptap_doc.clone(),
            doc.dirty,
        )
    };
    // 每个返回点都留一行日志 —— 这些闸门原先只弹对话框不记日志,
    // 用户报「推送没反应」时无从判断是哪道拦下的(2026-09-23 实测教训)。
    eprintln!(
        "[feishu] 推送前置检查:已保存={} 脏={} 已绑定={}",
        file_path.is_some(),
        dirty,
        frontmatter
            .feishu
            .as_ref()
            .and_then(|f| f.doc_token.as_deref())
            .filter(|t| !t.is_empty())
            .is_some()
    );

    let Some(path) = file_path else {
        eprintln!("[feishu] 推送中止:文档未保存");
        alert(
            app,
            "请先保存当前文档",
            "推送会把绑定信息写回文件的 frontmatter，所以文档需要先有一个保存路径（按 Ctrl+S 保存）。",
        );
        return None;
    };

    // 脏文档:先保存再推送。推送要把绑定信息写回磁盘,若不先保存,用户
    // 未保存的编辑会在写回时被覆盖掉。
    //
    // 保存是 `requestDocumentJSON` 异步握手,同步的前置检查里等不到它完成,
    // 所以置 `push_after_save` 标志,由 `document::on_document_json` 在保存
    // 落盘后续跑推送 —— 用户只需点一次。(早先的实现只触发保存就返回、
    // 要求再点一次推送;实测中用户据此以为推送已发生。)
    if dirty {
        let proceed = app
            .dialog()
            .message(
                "当前文档有未保存的更改。推送会先保存文档，再把内容推到飞书。",
            )
            .title("保存并推送")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "保存并推送".to_string(),
                "取消".to_string(),
            ))
            .blocking_show();
        if !proceed {
            eprintln!("[feishu] 推送中止:用户在保存确认框里取消");
            return None;
        }
        eprintln!("[feishu] 脏文档:已置 push_after_save,触发保存握手");
        state.doc.lock().unwrap().push_after_save = true;
        crate::document::save(app);
        // 真正的推送在保存握手回来后发生;这里不继续。
        return None;
    }

    if state.feishu.current_app_config().is_none() {
        eprintln!("[feishu] 推送中止:未配置应用凭证");
        alert(
            app,
            "未配置飞书应用凭证",
            "请到「设置 → 飞书同步」填好 App ID / App Secret / 重定向 URL。",
        );
        return None;
    }
    state.feishu.refresh_auth_state();
    let auth = state.feishu.auth_state();
    if !matches!(auth, super::manager::AuthState::LoggedIn { .. }) {
        eprintln!("[feishu] 推送中止:未登录(当前状态 {auth:?})");
        alert(
            app,
            "尚未登录飞书",
            "请到「设置 → 飞书同步」点「登录飞书」完成授权后再推送。",
        );
        return None;
    }

    let title = document_title(&body, &path);
    eprintln!("[feishu] 推送前置检查通过,远端标题将用:{title}");
    Some(PushPlan {
        path,
        frontmatter,
        body,
        title,
        force: false,
    })
}

/// 新建远端文档时的标题:正文首个标题优先,退回文件名(去扩展名)。
fn document_title(body: &serde_json::Value, path: &std::path::Path) -> String {
    if let Some(t) = first_heading_text(body) {
        let t = t.trim();
        if !t.is_empty() {
            return t.chars().take(100).collect();
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未命名文档".to_string())
}

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

/// 异步阶段:跑协调器并处置结果。
///
/// 冲突后的「仍然覆盖」用**循环**重试而非递归调用自身:`run` 递归会让
/// 编译器无法证明 future 是 `Send`(判定自身是否 Send 需要先知道自身是否
/// Send,循环依赖),而 `async_runtime::spawn` 要求 Send。循环也更直白。
pub async fn run(app: AppHandle, plan: PushPlan) {
    let mut plan = plan;
    loop {
        let state = app.state::<AppState>();
        let Some(api) = state.feishu.http_api() else {
            toast(&app, "飞书应用凭证已失效，请重新配置后再试", TOAST_ERROR);
            return;
        };

        let is_new = plan
            .frontmatter
            .feishu
            .as_ref()
            .and_then(|f| f.doc_token.as_deref())
            .filter(|t| !t.is_empty())
            .is_none();
        eprintln!(
            "[feishu] 推送开始({}{})",
            if is_new { "新建远端" } else { "覆盖远端" },
            if plan.force { "，已确认覆盖" } else { "" }
        );
        toast(&app, "正在推送到飞书…", TOAST_PROGRESS);

        let reader = AssetsImageReader { state: &state };
        let coordinator = PushCoordinator::with_image_reader(&api, &reader);
        let result = coordinator
            .push(
                &plan.body,
                &plan.frontmatter,
                &plan.title,
                plan.force,
                None,
                |event| {
                    eprintln!("[feishu] 推送进度 {event:?}");
                    if let Progress::ImageStageStarted { total } = event {
                        if total > 0 {
                            bridge::send_to_editor(
                                &app,
                                "feishuSyncToast",
                                json!({ "message": format!("正在上传 {total} 张图片…"), "kind": TOAST_PROGRESS }),
                            );
                        }
                    }
                },
            )
            .await;

        match result {
            Ok(push_result) => {
                finish_success(&app, &plan, push_result);
                return;
            }
            Err(PushError::Cancelled) => {
                eprintln!("[feishu] 推送已取消");
                return;
            }
            Err(PushError::ContainsPlaceholderBlocks { block_ids }) => {
                eprintln!("[feishu] 拒绝推送:含 {} 个占位块", block_ids.len());
                // 用对话框而非 toast —— 这条需要解释,且用户必须知道为什么
                // 不能推,否则会反复尝试。
                alert(
                    &app,
                    "暂不支持推送这篇文档",
                    &format!(
                        "文档里有 {} 个飞书专有块（表格 / 画板 / 多维表格 / 脑图这类 Done.md 无法编辑、\
                         只能原样承载的块）。\n\n\
                         飞书没有「整体覆盖正文」的接口，推送必须先删再重建整棵内容树 —— \
                         那会连带重建这些块，摧毁其他人在上面的实时协作数据。所以这里选择拒绝，\
                         而不是冒险覆盖。\n\n\
                         保住这些块的「分段推送」能力尚未实现。当前可行的做法：\
                         把这些块移出文档，或改在飞书侧直接编辑。",
                        block_ids.len()
                    ),
                );
                return;
            }
            Err(PushError::RemoteAhead {
                local_revision,
                remote_revision,
            }) => {
                eprintln!(
                    "[feishu] 远端已领先:本地 {local_revision} → 远端 {remote_revision}"
                );
                let overwrite = app
                    .dialog()
                    .message(format!(
                        "飞书侧的文档在你本地编辑期间也发生了变化\
                         （你拉取时是版本 {local_revision}，现在远端是 {remote_revision}）。\n\n\
                         直接推送会用你的本地内容覆盖掉远端那些改动。",
                    ))
                    .title("飞书侧已更新")
                    .buttons(MessageDialogButtons::OkCancelCustom(
                        "仍然覆盖".to_string(),
                        "取消".to_string(),
                    ))
                    .blocking_show();
                if !overwrite {
                    return;
                }
                plan.force = true;
                continue; // 重跑一轮,这次跳过预检
            }
            Err(PushError::PartialSuccess {
                orphaned_doc_token,
                underlying,
            }) => {
                eprintln!(
                    "[feishu] 部分成功:已建远端文档 {orphaned_doc_token},但推正文失败:{underlying}"
                );
                // **先写盘再告知** —— 不写回的话,用户重试会再建一篇文档。
                let persisted = persist_token_only(&app, &plan, &orphaned_doc_token);
                let tail = if persisted {
                    "已把这篇远端文档的 token 记入本地文件，所以重试会更新它、不会再建一篇。"
                        .to_string()
                } else {
                    format!(
                        "⚠️ 未能把 token 写回本地文件。请手动在 frontmatter 的 feishu.doc_token \
                         填入：{orphaned_doc_token}\n否则下次推送会在飞书侧再新建一篇文档。"
                    )
                };
                alert(
                    &app,
                    "远端文档已创建，但正文推送失败",
                    &format!("失败原因：{underlying}\n\n{tail}"),
                );
                return;
            }
            Err(PushError::ApiFailed(e)) => {
                eprintln!("[feishu] 推送失败:{e}");
                toast(&app, format!("推送失败：{e}"), TOAST_ERROR);
                return;
            }
        }
    }
}

/// 成功路径:把回写后的 frontmatter 连同正文落盘,再报结果。
fn finish_success(app: &AppHandle, plan: &PushPlan, result: PushResult) {
    let image_note = match &result.image_report {
        Some(r) if !r.failed_srcs.is_empty() => format!(
            "，图片 {} 张成功 / {} 张失败（失败的图在飞书侧会缺失）",
            r.uploaded_count,
            r.failed_srcs.len()
        ),
        Some(r) if r.uploaded_count > 0 => format!("，上传 {} 张图片", r.uploaded_count),
        _ => String::new(),
    };

    let parsed = crate::markdown::ParsedDocument {
        frontmatter: result.updated_frontmatter,
        body: plan.body.clone(),
    };
    if let Err(e) = crate::document::apply_pulled_document(app, &plan.path, parsed) {
        eprintln!("[feishu] 推送成功但写回本地失败:{e}");
        toast(
            app,
            format!("已推送到飞书，但本地写回失败：{e}"),
            TOAST_ERROR,
        );
        return;
    }

    let verb = if result.created_new_document {
        "已在飞书新建文档"
    } else {
        "已更新飞书文档"
    };
    eprintln!("[feishu] 推送完成:{verb}{image_note}");
    toast(app, format!("{verb}{image_note}"), TOAST_DONE);
}

/// 只把 doc_token 写回本地(孤儿善后)。正文保持原样。
/// 返回是否写成功 —— 失败时调用方要把 token 原文告诉用户。
fn persist_token_only(app: &AppHandle, plan: &PushPlan, doc_token: &str) -> bool {
    let mut feishu = plan.frontmatter.feishu.clone().unwrap_or_default();
    feishu.doc_token = Some(doc_token.to_string());
    let merged = crate::markdown::frontmatter::merge(
        &plan.frontmatter,
        Frontmatter {
            feishu: Some(feishu),
            ..Default::default()
        },
    );
    let parsed = crate::markdown::ParsedDocument {
        frontmatter: merged,
        body: plan.body.clone(),
    };
    match crate::document::apply_pulled_document(app, &plan.path, parsed) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[feishu] 孤儿 token 写回失败:{e}");
            false
        }
    }
}

/// 菜单入口。
pub fn invoke(app: &AppHandle) {
    eprintln!("[feishu] 菜单「推送到飞书」已触发");
    let Some(plan) = preflight(app) else { return };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        run(app, plan).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc_with_heading(text: &str) -> serde_json::Value {
        json!({ "type": "doc", "content": [
            { "type": "heading", "attrs": { "level": 1 },
              "content": [{ "type": "text", "text": text }] },
        ]})
    }

    #[test]
    fn title_prefers_first_heading() {
        let p = std::path::Path::new("D:\\docs\\note.md");
        assert_eq!(document_title(&doc_with_heading("交付说明 v2"), p), "交付说明 v2");
    }

    #[test]
    fn title_falls_back_to_file_stem() {
        let p = std::path::Path::new("D:\\docs\\my-report.md");
        let body = json!({ "type": "doc", "content": [{ "type": "paragraph" }] });
        assert_eq!(document_title(&body, p), "my-report");
    }

    /// 空标题(只有标记没有文字)也要退回文件名,不能推一个空标题上去。
    #[test]
    fn blank_heading_falls_back_to_file_stem() {
        let p = std::path::Path::new("D:\\docs\\fallback.md");
        let body = json!({ "type": "doc", "content": [
            { "type": "heading", "attrs": { "level": 1 }, "content": [] },
        ]});
        assert_eq!(document_title(&body, p), "fallback");
    }

    #[test]
    fn title_is_length_capped() {
        let p = std::path::Path::new("D:\\docs\\x.md");
        let long = "标".repeat(300);
        assert_eq!(document_title(&doc_with_heading(&long), p).chars().count(), 100);
    }

    /// 嵌套结构里的首个标题也要找到(比如标题被包在 callout 里)。
    #[test]
    fn finds_heading_nested() {
        let body = json!({ "type": "doc", "content": [
            { "type": "callout", "content": [
                { "type": "heading", "attrs": { "level": 2 },
                  "content": [{ "type": "text", "text": "内层标题" }] },
            ]},
        ]});
        let p = std::path::Path::new("D:\\docs\\x.md");
        assert_eq!(document_title(&body, p), "内层标题");
    }

    /// 标题由多个 text 片段组成(带加粗等 marks)时要拼全。
    #[test]
    fn concatenates_split_heading_text() {
        let body = json!({ "type": "doc", "content": [
            { "type": "heading", "attrs": { "level": 1 }, "content": [
                { "type": "text", "text": "交付" },
                { "type": "text", "text": "说明", "marks": [{ "type": "bold" }] },
            ]},
        ]});
        let p = std::path::Path::new("D:\\docs\\x.md");
        assert_eq!(document_title(&body, p), "交付说明");
    }
}
