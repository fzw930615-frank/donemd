//! AI 助手 — Windows port of `StreamCoordinator` + the `VisualWebView` AI
//! bridge handlers. Flow: `aiCommand` → prompt build (pure) → provider client
//! SSE stream → `aiStream*` events back to the editor. Single in-flight
//! stream, 60s ceiling, ESC-cancel via task abort, retry replays the last
//! request verbatim.

pub mod clients;
pub mod credentials;
pub mod prompt;
pub mod providers;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::bridge;
use crate::state::{AppState, PendingRequest};

use clients::{Client, ProviderError};
use prompt::{AiCommand, ContextScope, SelectionContext};
use providers::Provider;

// MARK: - bridge entry points

/// `aiCommand` envelope handler (parity with `VisualWebView.handleAICommand`).
pub fn handle_command(app: &AppHandle, payload: &Value) {
    let Some(stream_id) = payload.get("streamId").and_then(Value::as_str).map(str::to_string) else {
        eprintln!("[ai] aiCommand: missing streamId");
        return;
    };
    let str_of = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let arr_of = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default()
    };

    let kind = str_of("command").unwrap_or_else(|| "polish".to_string());
    let arg = str_of("arg");
    let Some(command) = AiCommand::from_kind(&kind, arg.as_deref()) else {
        eprintln!("[ai] aiCommand: unknown command kind '{kind}'");
        return;
    };

    // 生成类 (inputOnly): the typed topic/prompt arrives in `arg`. Everything
    // else transforms an actual selection, which must exist.
    let selection = if command.context_scope() == ContextScope::InputOnly {
        arg.clone().unwrap_or_default()
    } else {
        match str_of("selection") {
            Some(s) => s,
            None => {
                eprintln!("[ai] aiCommand: missing selection for {kind}");
                return;
            }
        }
    };
    if selection.is_empty() {
        eprintln!("[ai] aiCommand: empty input for {kind}");
        return;
    }

    let mut context = SelectionContext {
        selection,
        paragraph: str_of("paragraph").unwrap_or_default(),
        before: arr_of("before"),
        after: arr_of("after"),
        full_document: None,
    };
    if context.paragraph.is_empty() {
        context.paragraph = context.selection.clone();
    }

    let state = app.state::<AppState>();
    let provider = str_of("provider")
        .and_then(|id| Provider::from_id(&id))
        .unwrap_or_else(|| state.ai_config.lock().unwrap().default_provider());

    if !state.ai_config.lock().unwrap().is_configured(provider) {
        bridge::send_to_editor(app, "aiStreamError", json!({
            "streamId": stream_id,
            "message": "尚未配置 AI Provider，请在设置中配置",
            "canOpenSettings": true,
        }));
        return;
    }

    let model = state.ai_config.lock().unwrap().selected_model(provider);
    let request = PendingRequest {
        command,
        context,
        provider,
        model,
    };

    // 续写 needs the whole document — ask the webview for the live Tiptap JSON
    // (Tauri can't evaluateJavaScript), serialize to markdown when it lands.
    if request.command.context_scope() == ContextScope::WholeDocument {
        let request_id = format!("ai-doc-{}", uuid::Uuid::new_v4());
        {
            let mut ai = state.ai.lock().unwrap();
            ai.pending_doc = Some(crate::state::PendingDocFetch { request_id: request_id.clone(), stream_id, request });
        }
        bridge::send_to_editor(app, "requestDocumentJSON", json!({ "requestId": request_id }));
        return;
    }

    start_request(app, stream_id, request);
}

/// Continuation of `handle_command` for whole-document commands: the webview
/// answered `requestDocumentJSON` with the live doc. Called from
/// `document::on_document_json`. Returns true when the reply was consumed.
pub fn consume_document_json(app: &AppHandle, payload: &Value) -> bool {
    let state = app.state::<AppState>();
    let pending = {
        let mut ai = state.ai.lock().unwrap();
        match ai.pending_doc.as_ref() {
            Some(p)
                if payload.get("requestId").and_then(Value::as_str) == Some(p.request_id.as_str()) =>
            {
                ai.pending_doc.take()
            }
            _ => None,
        }
    };
    let Some(pending) = pending else { return false };

    let doc_json = payload.get("doc").cloned().unwrap_or(Value::Null);
    let markdown = if doc_json.is_object() {
        let parsed = crate::markdown::ParsedDocument {
            frontmatter: crate::markdown::frontmatter::Frontmatter::default(),
            body: doc_json,
        };
        crate::markdown::serialize(&parsed)
    } else {
        String::new()
    };
    let mut request = pending.request;
    request.context.full_document = Some(markdown);
    start_request(app, pending.stream_id, request);
    true
}

/// `aiCancel` — user pressed ESC / typed. Silent teardown, no error (PRD 21).
pub fn handle_cancel(app: &AppHandle) {
    let state = app.state::<AppState>();
    let active = state.ai.lock().unwrap().active.take();
    if let Some(active) = active {
        active.cancel.notify_one();
    }
}

/// `aiRetry` — re-issue the last request verbatim with a new streamId.
pub fn handle_retry(app: &AppHandle, payload: &Value) {
    let Some(stream_id) = payload.get("streamId").and_then(Value::as_str).map(str::to_string) else {
        eprintln!("[ai] aiRetry: missing streamId");
        return;
    };
    let state = app.state::<AppState>();
    let request = state.ai.lock().unwrap().last_request.clone();
    let Some(request) = request else { return };
    start_request(app, stream_id, request);
}

/// `aiProvidersQuery` — reply with configured providers + the default id.
pub fn reply_providers(app: &AppHandle, payload: &Value) {
    let Some(request_id) = payload.get("requestId").and_then(Value::as_str) else { return };
    let state = app.state::<AppState>();
    let cfg = state.ai_config.lock().unwrap();
    let providers: Vec<Value> = cfg
        .configured_list()
        .iter()
        .filter(|(_, configured)| *configured)
        .map(|(p, _)| json!({ "id": p.id(), "name": p.display_name() }))
        .collect();
    let default = cfg.default_provider().id().to_string();
    drop(cfg);
    bridge::send_to_editor(app, "aiProvidersReply", json!({
        "requestId": request_id,
        "providers": providers,
        "default": default,
    }));
}

// MARK: - the stream itself

fn start_request(app: &AppHandle, stream_id: String, request: PendingRequest) {
    let state = app.state::<AppState>();

    // Single in-flight (PRD 27): a concurrent request flashes the busy hint.
    if state.ai.lock().unwrap().active.is_some() {
        bridge::send_to_editor(app, "aiStreamBusy", json!({}));
        return;
    }

    let context_range = state.ai_config.lock().unwrap().context_range();
    let built = prompt::build(&request.command, &request.context, context_range);
    let did_degrade = built.did_degrade;
    let messages = built.messages;

    let (endpoint, api_key) = {
        let cfg = state.ai_config.lock().unwrap();
        let endpoint = cfg.endpoint(request.provider);
        let key = credentials::load_key(request.provider).ok().flatten().unwrap_or_default();
        (endpoint, key)
    };
    if api_key.is_empty() {
        // Configured flag says yes but Credential Manager gave nothing back —
        // fail with an actionable message instead of a doomed 401.
        eprintln!(
            "[ai] {}: configured but no key in Credential Manager (provider={})",
            stream_id,
            request.provider.id()
        );
        bridge::send_to_editor(app, "aiStreamError", json!({
            "streamId": stream_id,
            "message": "凭据读取失败，请在 AI 设置中重新保存 API Key",
            "canOpenSettings": true,
        }));
        return;
    }
    eprintln!(
        "[ai] {} start: provider={} model={} endpoint={} key_len={}",
        stream_id,
        request.provider.id(),
        request.model,
        endpoint,
        api_key.len()
    );
    let client = Client::for_provider(request.provider, endpoint, api_key, state.http.clone());
    let model = request.model.clone();
    let command = request.command.clone();
    state.ai.lock().unwrap().last_request = Some(request);

    bridge::send_to_editor(app, "aiStreamStart", json!({ "streamId": stream_id }));
    if did_degrade {
        bridge::send_to_editor(app, "aiStreamDegrade", json!({ "streamId": stream_id }));
    }

    let app_handle = app.clone();
    let sid = stream_id.clone();
    let task_sid = stream_id.clone();
    let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
    let cancel_in_task = cancel.clone();
    tauri::async_runtime::spawn(async move {
        let run = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            client.stream_completion(&messages, &model, |text| {
                bridge::send_to_editor(&app_handle, "aiStreamToken", json!({
                    "streamId": sid, "text": text,
                }));
            }),
        );
        tokio::select! {
            biased;
            // ESC-cancel: silent — finish_stream is skipped and `active` was
            // already taken by handle_cancel (parity with Swift's silent
            // cancel). Dropping `run` cancels the HTTP request.
            _ = cancel_in_task.notified() => {}
            run_outcome = run => {
                let outcome = match run_outcome {
                    Ok(Ok(accumulated)) => Ok(accumulated),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(ProviderError::NetworkUnreachable("__timeout__".into())),
                };
                finish_stream(&app_handle, &task_sid, &command, outcome);
            }
        }
    });

    state.ai.lock().unwrap().active = Some(crate::state::ActiveStream {
        stream_id,
        cancel,
    });
}

/// Terminal path for a stream: decline / complete / error, then clear state.
/// Cancelled tasks are aborted before their future completes, so a cancel
/// never reaches here (matching Swift's silent cancel).
fn finish_stream(
    app: &AppHandle,
    stream_id: &str,
    command: &AiCommand,
    outcome: Result<String, ProviderError>,
) {
    let state = app.state::<AppState>();
    {
        let mut ai = state.ai.lock().unwrap();
        if ai.active.as_ref().map(|a| a.stream_id.as_str()) != Some(stream_id) {
            return; // superseded / cancelled
        }
        ai.active = None;
    }

    match outcome {
        Ok(accumulated) => {
            // 转表格 may decline (不适合) — never overwrite the selection with
            // the decline text.
            if let Some(decline) = decline_message(command, &accumulated) {
                bridge::send_to_editor(app, "aiStreamNotApplicable", json!({
                    "streamId": stream_id, "message": decline,
                }));
                return;
            }
            let parsed = crate::markdown::parse_document(&accumulated);
            bridge::send_to_editor(app, "aiStreamComplete", json!({
                "streamId": stream_id, "node": parsed.body,
            }));
        }
        Err(e) => {
            let (message, can_open_settings) = stream_error_copy(&e);
            bridge::send_to_editor(app, "aiStreamError", json!({
                "streamId": stream_id,
                "message": message,
                "canOpenSettings": can_open_settings,
            }));
        }
    }
}

/// 转表格's prompt lets the model reply 「不适合」; a real markdown table always
/// has a pipe, so no `|` ⇒ declined (parity with StreamCoordinator.declineMessage).
fn decline_message(command: &AiCommand, output: &str) -> Option<&'static str> {
    if *command == AiCommand::ToTable && !output.trim().contains('|') {
        Some("该内容不适合转换为表格")
    } else {
        None
    }
}

/// Provider error → toast copy (parity with `StreamCoordinator.streamError`).
fn stream_error_copy(error: &ProviderError) -> (String, bool) {
    match error {
        ProviderError::Unauthorized => ("AI 调用失败：未授权，请检查 Provider 配置".into(), true),
        ProviderError::Forbidden(_) => ("AI 调用失败：无权限访问该模型".into(), true),
        ProviderError::InsufficientBalance => {
            ("AI 调用失败：账户余额不足，请到 Provider 官网充值".into(), true)
        }
        ProviderError::RateLimited => {
            ("AI 调用失败：请求过于频繁或已用尽额度，请稍后重试".into(), false)
        }
        ProviderError::BadRequest(status, msg) => {
            let suffix = if msg.is_empty() { String::new() } else { format!("：{msg}") };
            (format!("AI 调用失败：请求错误 {status}{suffix}"), false)
        }
        ProviderError::ServerError(status, _) => {
            (format!("AI 调用失败：服务繁忙（{status}），请稍后重试"), false)
        }
        ProviderError::NetworkUnreachable(detail) if detail == "__timeout__" => {
            ("AI 响应超时（60s 无响应）".into(), false)
        }
        ProviderError::NetworkUnreachable(_) => ("AI 调用失败：网络中断".into(), false),
        ProviderError::DecodeFailed(_) => ("AI 调用失败：响应解析失败".into(), false),
    }
}

// MARK: - settings window + tauri commands

/// `aiOpenSettings` — open (or focus) the settings window (AI + 飞书同步
/// 两个 tab 共用一个窗口;消息名沿用 aiOpenSettings 历史契约)。
///
/// The build is deferred to a spawned thread: `aiOpenSettings` also arrives
/// through the bridge envelope, i.e. from inside a WebView2
/// `WebResourceReceived` callback on the main thread. Building a webview
/// window there nests WebView2's async controller creation inside that
/// callback and deadlocks (the window never navigates past about:blank and
/// the invoke never resolves). From a plain thread the create request goes
/// through the event-loop proxy, which is free to process it.
pub fn open_settings(app: &AppHandle) {
    if let Some(existing) = app.get_webview_window("settings") {
        let _ = existing.set_focus();
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        match tauri::WebviewWindowBuilder::new(&app, "settings", tauri::WebviewUrl::App("settings.html".into()))
            .title("设置 — Done.md")
            .inner_size(560.0, 680.0)
            .min_inner_size(480.0, 520.0)
            .build()
        {
            Ok(window) => {
                crate::theme::paint_window(&window);
                // The app menu bar (文件/格式/插入) belongs to the editor.
                let _ = window.remove_menu();
            }
            Err(e) => eprintln!("[ai] open settings window failed: {e}"),
        }
    });
}

#[derive(serde::Serialize)]
struct SettingsProviderView {
    id: &'static str,
    name: &'static str,
    recommended: bool,
    configured: bool,
    model_id: String,
    endpoint: String,
    model_list: Vec<String>,
}

#[tauri::command]
pub fn ai_settings_load(app: AppHandle) -> Value {
    let state = app.state::<AppState>();
    let cfg = state.ai_config.lock().unwrap();
    let providers: Vec<SettingsProviderView> = providers::ALL_PROVIDERS
        .iter()
        .map(|p| {
            let pc = cfg.provider_cfg(*p);
            SettingsProviderView {
                id: p.id(),
                name: p.display_name(),
                recommended: p.is_recommended_default(),
                configured: pc.configured,
                model_id: cfg.selected_model(*p),
                endpoint: cfg.endpoint(*p),
                model_list: pc.model_list,
            }
        })
        .collect();
    json!({
        "defaultProvider": cfg.default_provider().id(),
        "contextRange": cfg.context_range(),
        "providers": providers,
    })
}

/// Pick the model to pin after a list refresh: exact fallback match, else a
/// case-insensitive one — MiMo's list spells `mimo-v2.5-pro` while the
/// fallback constant is display-cased `MiMo-V2.5-Pro`; a case-sensitive
/// `contains` misses and wrongly pins `models[0]` (an ASR/TTS id can sort
/// first, and audio ids 400 on chat/completions). Last resort: first entry.
fn pin_model(models: &[String], fallback: &str) -> String {
    if let Some(m) = models.iter().find(|m| m.as_str() == fallback) {
        return m.clone();
    }
    if let Some(m) = models.iter().find(|m| m.eq_ignore_ascii_case(fallback)) {
        return m.clone();
    }
    models[0].clone()
}

/// Save an API key → Credential Manager, flip the configured mirror, then
/// refresh the model list (success pins the selection to the fallback model
/// when present, failure degrades gracefully — saving is never blocked).
/// An empty key reuses the stored one (endpoint-only edits on an already
/// configured provider don't force re-pasting).
#[tauri::command]
pub async fn ai_settings_save_key(app: AppHandle, provider: String, key: String) -> Value {
    let Some(p) = Provider::from_id(&provider) else {
        return json!({ "ok": false, "error": "unknown provider" });
    };
    let key_to_use: String = {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            match credentials::load_key(p) {
                Ok(Some(stored)) if !stored.is_empty() => stored,
                _ => return json!({ "ok": false, "error": "API Key 不能为空" }),
            }
        } else {
            trimmed.to_string()
        }
    };
    if let Err(e) = credentials::save_key(p, &key_to_use) {
        return json!({ "ok": false, "error": format!("凭据保存失败：{e}") });
    }
    let state = app.state::<AppState>();
    let (endpoint, fallback) = {
        let mut cfg = state.ai_config.lock().unwrap();
        cfg.provider_cfg_mut(p).configured = true;
        let _ = cfg.save();
        (cfg.endpoint(p), p.fallback_model().to_string())
    };

    let client = Client::for_provider(p, endpoint, key_to_use, state.http.clone());
    match client.list_models().await {
        Ok(models) => {
            let mut cfg = state.ai_config.lock().unwrap();
            let pc = cfg.provider_cfg_mut(p);
            pc.model_list = models.clone();
            pc.model_id = pin_model(&models, &fallback);
            let _ = cfg.save();
            json!({ "ok": true, "models": models, "degraded": false })
        }
        Err(e) => {
            let mut cfg = state.ai_config.lock().unwrap();
            let pc = cfg.provider_cfg_mut(p);
            pc.model_id = fallback.clone();
            let _ = cfg.save();
            json!({ "ok": true, "models": [fallback.clone()], "degraded": true,
                    "fallback": fallback, "error": describe_error(&e),
                    "url": client.models_url() })
        }
    }
}

/// Re-fetch the model list with the stored key (settings「重新拉模型列表」).
/// Failure keeps the cached list — the error is only surfaced in the UI.
#[tauri::command]
pub async fn ai_settings_refresh_models(app: AppHandle, provider: String) -> Value {
    let Some(p) = Provider::from_id(&provider) else {
        return json!({ "ok": false, "error": "unknown provider" });
    };
    let state = app.state::<AppState>();
    let endpoint = state.ai_config.lock().unwrap().endpoint(p);
    let key = credentials::load_key(p).ok().flatten().unwrap_or_default();
    let client = Client::for_provider(p, endpoint, key, state.http.clone());
    match client.list_models().await {
        Ok(models) => {
            let fallback = p.fallback_model().to_string();
            let mut cfg = state.ai_config.lock().unwrap();
            let pc = cfg.provider_cfg_mut(p);
            pc.model_list = models.clone();
            if !models.contains(&pc.model_id) {
                pc.model_id = pin_model(&models, &fallback);
            }
            let _ = cfg.save();
            json!({ "ok": true, "models": models })
        }
        Err(e) => json!({ "ok": false, "error": describe_error(&e), "url": client.models_url() }),
    }
}

/// Short Chinese reason for a model-list fetch failure — the settings panel
/// embeds it in「已保存，但无法拉模型列表（…）」(parity with the Swift
/// `describe(error)` helper in AIProviderSettingsView).
fn describe_error(error: &ProviderError) -> String {
    match error {
        ProviderError::Unauthorized => "未授权 / key 无效".into(),
        ProviderError::InsufficientBalance => "账户余额不足".into(),
        ProviderError::Forbidden(_) => "无权限".into(),
        ProviderError::RateLimited => "限流".into(),
        ProviderError::BadRequest(status, _) => format!("请求错误 {status}"),
        ProviderError::ServerError(status, _) => format!("服务端错误 {status}"),
        ProviderError::NetworkUnreachable(_) => "网络不可达".into(),
        ProviderError::DecodeFailed(_) => "响应解析失败".into(),
    }
}

#[tauri::command]
pub fn ai_settings_clear_key(app: AppHandle, provider: String) -> Value {
    let Some(p) = Provider::from_id(&provider) else {
        return json!({ "ok": false, "error": "unknown provider" });
    };
    if let Err(e) = credentials::clear_key(p) {
        return json!({ "ok": false, "error": format!("凭据清除失败：{e}") });
    }
    let state = app.state::<AppState>();
    let mut cfg = state.ai_config.lock().unwrap();
    cfg.provider_cfg_mut(p).configured = false;
    let _ = cfg.save();
    json!({ "ok": true })
}

#[tauri::command]
pub fn ai_settings_update(app: AppHandle, field: String, provider: Option<String>, value: Value) -> Value {
    let state = app.state::<AppState>();
    let mut cfg = state.ai_config.lock().unwrap();
    match field.as_str() {
        "defaultProvider" => {
            let Some(id) = provider.as_deref().or(value.as_str()) else {
                return json!({ "ok": false, "error": "missing provider" });
            };
            if Provider::from_id(id).is_none() {
                return json!({ "ok": false, "error": "unknown provider" });
            }
            cfg.default_provider = id.to_string();
        }
        "contextRange" => {
            let n = value.as_i64().unwrap_or(1);
            cfg.context_range = n.clamp(0, 3);
        }
        "modelId" => {
            let (Some(id), Some(model)) = (provider.as_deref(), value.as_str()) else {
                return json!({ "ok": false, "error": "missing provider/model" });
            };
            let Some(p) = Provider::from_id(id) else {
                return json!({ "ok": false, "error": "unknown provider" });
            };
            cfg.provider_cfg_mut(p).model_id = model.trim().to_string();
        }
        "endpoint" => {
            let (Some(id), Some(endpoint)) = (provider.as_deref(), value.as_str()) else {
                return json!({ "ok": false, "error": "missing provider/endpoint" });
            };
            let Some(p) = Provider::from_id(id) else {
                return json!({ "ok": false, "error": "unknown provider" });
            };
            cfg.provider_cfg_mut(p).endpoint = endpoint.trim().to_string();
        }
        _ => return json!({ "ok": false, "error": format!("unknown field: {field}") }),
    }
    if let Err(e) = cfg.save() {
        return json!({ "ok": false, "error": format!("配置写入失败：{e}") });
    }
    json!({ "ok": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_model_matches_fallback_case_insensitively() {
        // MiMo's live list lowercases the display-cased fallback constant —
        // pinning must land on the list's exact spelling, not models[0]
        // (audio ids like `mimo-v2.5-asr` 400 on chat/completions).
        let models = vec![
            "mimo-v2.5".to_string(),
            "mimo-v2.5-asr".to_string(),
            "mimo-v2.5-pro".to_string(),
        ];
        assert_eq!(pin_model(&models, "MiMo-V2.5-Pro"), "mimo-v2.5-pro");
        assert_eq!(pin_model(&models, "mimo-v2.5-pro"), "mimo-v2.5-pro");
        assert_eq!(pin_model(&models, "no-such-model"), "mimo-v2.5");
    }

    #[test]
    fn to_table_decline_detection() {
        assert_eq!(decline_message(&AiCommand::ToTable, "不适合"), Some("该内容不适合转换为表格"));
        assert_eq!(decline_message(&AiCommand::ToTable, "  不适合  "), Some("该内容不适合转换为表格"));
        assert_eq!(decline_message(&AiCommand::ToTable, "| a | b |\n|---|---|"), None);
        // Only toTable declines.
        assert_eq!(decline_message(&AiCommand::Summarize, "不适合"), None);
    }

    #[test]
    fn error_copy_matches_swift() {
        assert_eq!(
            stream_error_copy(&ProviderError::Unauthorized),
            ("AI 调用失败：未授权，请检查 Provider 配置".to_string(), true)
        );
        assert_eq!(
            stream_error_copy(&ProviderError::InsufficientBalance).1,
            true
        );
        assert_eq!(
            stream_error_copy(&ProviderError::NetworkUnreachable("__timeout__".into())).0,
            "AI 响应超时（60s 无响应）"
        );
        assert_eq!(
            stream_error_copy(&ProviderError::RateLimited).1,
            false
        );
    }
}
