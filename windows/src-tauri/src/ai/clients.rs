//! Port of `OpenAIClient` / `AnthropicClient` / `GoogleClient` (Swift) —
//! three protocol families behind one `Client` enum. Request building and
//! SSE line parsing are pure functions (unit-tested without network); the
//! async `stream_completion` / `list_models` are thin reqwest wrappers.

use futures_util::StreamExt;
use serde_json::{json, Value};

use super::prompt::AiMessage;
use super::providers::{ProtocolFamily, Provider};

/// Domain errors mirroring `AIProviderError.swift` — the coordinator maps
/// them to toast copy; keep the variants aligned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    Unauthorized,
    Forbidden(String),
    InsufficientBalance,
    RateLimited,
    BadRequest(u16, String),
    ServerError(u16, String),
    NetworkUnreachable(String),
    DecodeFailed(String),
}

/// Map an HTTP error status + body to the typed error (shared shape across
/// families: `{ "error": { "message": ... } }`).
pub fn error_for_status(status: u16, body: &[u8]) -> ProviderError {
    let msg = extract_error_message(body).unwrap_or_default();
    match status {
        401 => ProviderError::Unauthorized,
        402 => ProviderError::InsufficientBalance,
        403 => ProviderError::Forbidden(msg),
        429 => ProviderError::RateLimited,
        400..=499 => ProviderError::BadRequest(status, msg),
        _ => ProviderError::ServerError(status, msg),
    }
}

/// Pull the human-readable reason out of an error body. Gateways diverge:
/// `{"error":{"message":…}}` (OpenAI), `{"error":"…"}` (some proxies),
/// `{"message":…}` top-level (MiMo &co) — try all three so the toast shows
/// the server's reason instead of a bare "请求错误 400".
fn extract_error_message(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if let Some(e) = v.get("error") {
        if let Some(m) = e.get("message").and_then(Value::as_str) {
            return Some(m.to_string());
        }
        if let Some(m) = e.as_str() {
            return Some(m.to_string());
        }
    }
    v.get("message").and_then(Value::as_str).map(str::to_string)
}

/// Outcome of parsing one SSE line (pure — unit-tested without network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseLine {
    Token(String),
    Done,
    Ignore,
}

/// OpenAI-compatible: `data: {"choices":[{"delta":{"content":"…"}}]}`,
/// terminated by `data: [DONE]`.
pub fn parse_openai_line(line: &str) -> SseLine {
    let payload = line.strip_prefix("data:").unwrap_or(line).trim();
    if payload.is_empty() {
        return SseLine::Ignore;
    }
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return SseLine::Ignore;
    };
    let content = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if content.is_empty() {
        SseLine::Ignore
    } else {
        SseLine::Token(content.to_string())
    }
}

/// Anthropic Messages: `content_block_delta` → `delta.text_delta`,
/// terminated by a `message_stop` event.
pub fn parse_anthropic_line(line: &str) -> SseLine {
    let Some(payload) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let payload = payload.trim();
    if payload.is_empty() {
        return SseLine::Ignore;
    }
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return SseLine::Ignore;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("message_stop") => SseLine::Done,
        Some("content_block_delta") => {
            let text = v
                .get("delta")
                .filter(|d| d.get("type").and_then(Value::as_str) == Some("text_delta"))
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if text.is_empty() {
                SseLine::Ignore
            } else {
                SseLine::Token(text.to_string())
            }
        }
        // message_start / content_block_start / ping / … — nothing to yield.
        _ => SseLine::Ignore,
    }
}

/// Gemini `streamGenerateContent?alt=sse`: text in
/// `candidates[].content.parts[].text`; no done sentinel — EOF ends it.
pub fn parse_gemini_line(line: &str) -> SseLine {
    let Some(payload) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let payload = payload.trim();
    if payload.is_empty() {
        return SseLine::Ignore;
    }
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return SseLine::Ignore;
    };
    let mut text = String::new();
    if let Some(candidates) = v.get("candidates").and_then(Value::as_array) {
        for c in candidates {
            if let Some(parts) = c.get("content").and_then(|c| c.get("parts")).and_then(Value::as_array) {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    if text.is_empty() {
        SseLine::Ignore
    } else {
        SseLine::Token(text)
    }
}

/// A ready-to-call provider client: resolved endpoint + key + family, plus a
/// shared `reqwest::Client` (connection-pool + schannel trust store are reused
/// across every stream / model-list call instead of rebuilt per request).
pub struct Client {
    pub family: ProtocolFamily,
    pub base_url: String,
    pub api_key: String,
    http: reqwest::Client,
}

/// A prepared HTTP request (pure — unit-tested without network).
pub struct PreparedRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

/// Join an endpoint base with an API path, deduping the version segment.
/// Users routinely paste the OpenAI-SDK-style base that already ends in
/// `/v1` (provider consoles show it that way); naïvely appending produces
/// `/v1/v1/models` → 404.
pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    for seg in ["v1", "v1beta"] {
        if base.ends_with(&format!("/{seg}")) && path.starts_with(&format!("{seg}/")) {
            return format!("{base}/{}", &path[seg.len() + 1..]);
        }
    }
    format!("{base}/{path}")
}

impl Client {
    pub fn for_provider(
        provider: Provider,
        base_url: String,
        api_key: String,
        http: reqwest::Client,
    ) -> Client {
        Client { family: provider.family(), base_url, api_key, http }
    }

    /// POST body + URL + headers for a streaming completion.
    pub fn prepare_stream_request(&self, messages: &[AiMessage], model: &str) -> PreparedRequest {
        let base = self.base_url.trim_end_matches('/');
        match self.family {
            ProtocolFamily::OpenAiCompatible => PreparedRequest {
                url: join_url(base, "v1/chat/completions"),
                headers: vec![
                    ("Authorization".into(), format!("Bearer {}", self.api_key)),
                    ("Content-Type".into(), "application/json".into()),
                    ("Accept".into(), "text/event-stream".into()),
                ],
                body: json!({
                    "model": model,
                    "stream": true,
                    "messages": messages.iter().map(|m| json!({"role": m.role, "content": m.content})).collect::<Vec<_>>(),
                }),
            },
            ProtocolFamily::Anthropic => {
                // Anthropic carries `system` top-level, not as a message.
                let system_text = messages
                    .iter()
                    .filter(|m| m.role == "system")
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let wire: Vec<Value> = messages
                    .iter()
                    .filter(|m| m.role != "system")
                    .map(|m| json!({"role": m.role, "content": m.content}))
                    .collect();
                let mut body = json!({
                    "model": model,
                    "max_tokens": 4096,
                    "stream": true,
                    "messages": wire,
                });
                if !system_text.is_empty() {
                    body["system"] = json!(system_text);
                }
                let mut headers = vec![
                    ("anthropic-version".into(), "2023-06-01".into()),
                    ("Content-Type".into(), "application/json".into()),
                    ("Accept".into(), "text/event-stream".into()),
                ];
                if !self.api_key.is_empty() {
                    headers.insert(0, ("x-api-key".into(), self.api_key.clone()));
                }
                PreparedRequest { url: join_url(base, "v1/messages"), headers, body }
            }
            ProtocolFamily::Google => {
                // Endpoint embeds the model id + SSE flag; system message lifts
                // into `systemInstruction`; assistant role spells `model`.
                let system_text = messages
                    .iter()
                    .filter(|m| m.role == "system")
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let contents: Vec<Value> = messages
                    .iter()
                    .filter(|m| m.role != "system")
                    .map(|m| {
                        let role = if m.role == "assistant" { "model" } else { "user" };
                        json!({"role": role, "parts": [{"text": m.content}]})
                    })
                    .collect();
                let mut body = json!({ "contents": contents });
                if !system_text.is_empty() {
                    body["systemInstruction"] = json!({ "parts": [{"text": system_text}] });
                }
                PreparedRequest {
                    url: join_url(base, &format!("v1beta/models/{model}:streamGenerateContent?alt=sse")),
                    headers: vec![
                        ("x-goog-api-key".into(), self.api_key.clone()),
                        ("Content-Type".into(), "application/json".into()),
                        ("Accept".into(), "text/event-stream".into()),
                    ],
                    body,
                }
            }
        }
    }

    fn parse_line(&self, line: &str) -> SseLine {
        match self.family {
            ProtocolFamily::OpenAiCompatible => parse_openai_line(line),
            ProtocolFamily::Anthropic => parse_anthropic_line(line),
            ProtocolFamily::Google => parse_gemini_line(line),
        }
    }

    /// The exact URL `list_models` hits — surfaced in settings status text so
    /// a 404 (wrong endpoint override) is debuggable from the UI.
    pub fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.family {
            ProtocolFamily::Google => join_url(base, "v1beta/models"),
            _ => join_url(base, "v1/models"),
        }
    }

    /// GET the model list (settings "保存 → 拉真列表" chain).
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = self.models_url();
        let http = &self.http;
        let mut req = http.get(&url);
        match self.family {
            ProtocolFamily::Anthropic => {
                req = req
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01");
            }
            ProtocolFamily::Google => {
                req = req.header("x-goog-api-key", &self.api_key);
            }
            ProtocolFamily::OpenAiCompatible => {
                req = req.header("Authorization", format!("Bearer {}", self.api_key));
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::NetworkUnreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| ProviderError::NetworkUnreachable(e.to_string()))?;
        if !(200..=299).contains(&status) {
            return Err(error_for_status(status, &body));
        }
        let v: Value = serde_json::from_slice(&body)
            .map_err(|e| ProviderError::DecodeFailed(e.to_string()))?;
        // OpenAI/Anthropic: {data:[{id}]}; Gemini: {models:[{name:"models/x"}]}.
        let ids: Vec<String> = if let Some(data) = v.get("data").and_then(Value::as_array) {
            data.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        } else if let Some(models) = v.get("models").and_then(Value::as_array) {
            models
                .iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                .collect()
        } else {
            return Err(ProviderError::DecodeFailed("empty model list".into()));
        };
        if ids.is_empty() {
            return Err(ProviderError::DecodeFailed("empty model list".into()));
        }
        Ok(ids)
    }

    /// Stream a completion, invoking `on_token` per text delta. Returns the
    /// accumulated text. Dropping the future cancels the HTTP request (the
    /// coordinator aborts the spawned task for ESC-cancel).
    pub async fn stream_completion(
        &self,
        messages: &[AiMessage],
        model: &str,
        mut on_token: impl FnMut(String),
    ) -> Result<String, ProviderError> {
        let prepared = self.prepare_stream_request(messages, model);
        let http = &self.http;
        let mut req = http.post(&prepared.url);
        for (k, v) in &prepared.headers {
            req = req.header(k, v);
        }
        let resp = req
            .json(&prepared.body)
            .send()
            .await
            .map_err(|e| ProviderError::NetworkUnreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..=299).contains(&status) {
            let body = resp
                .bytes()
                .await
                .map_err(|e| ProviderError::NetworkUnreachable(e.to_string()))?;
            return Err(error_for_status(status, &body));
        }

        let mut accumulated = String::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(|e| ProviderError::NetworkUnreachable(e.to_string()))?;
            buf.extend_from_slice(&chunk);
            // Drain complete lines; SSE events are line-delimited.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                match self.parse_line(line) {
                    SseLine::Token(text) => {
                        accumulated.push_str(&text);
                        on_token(text);
                    }
                    SseLine::Done => return Ok(accumulated),
                    SseLine::Ignore => {}
                }
            }
        }
        // EOF (Gemini has no done sentinel) — flush a trailing line if any.
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).trim().to_string();
            if let SseLine::Token(text) = self.parse_line(&line) {
                accumulated.push_str(&text);
                on_token(text);
            }
        }
        Ok(accumulated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<AiMessage> {
        vec![
            AiMessage { role: "system", content: "系统指令".into() },
            AiMessage { role: "user", content: "用户文本".into() },
        ]
    }

    // MARK: OpenAI-compatible SSE

    #[test]
    fn openai_parses_content_delta() {
        let line = r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert_eq!(parse_openai_line(line), SseLine::Token("Hello".into()));
    }

    #[test]
    fn openai_done_sentinel() {
        assert_eq!(parse_openai_line("data: [DONE]"), SseLine::Done);
    }

    #[test]
    fn openai_ignores_empty_delta_and_blank() {
        assert_eq!(parse_openai_line(""), SseLine::Ignore);
        assert_eq!(parse_openai_line(r#"data: {"choices":[{"delta":{}}]}"#), SseLine::Ignore);
        assert_eq!(parse_openai_line(r#"data: {"choices":[{"delta":{"content":""}}]}"#), SseLine::Ignore);
    }

    #[test]
    fn openai_request_shape() {
        let c = Client { family: ProtocolFamily::OpenAiCompatible, base_url: "https://api.deepseek.com".into(), api_key: "k".into(), http: reqwest::Client::new() };
        let r = c.prepare_stream_request(&msgs(), "deepseek-v4-flash");
        assert_eq!(r.url, "https://api.deepseek.com/v1/chat/completions");
        assert!(r.headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer k"));
        assert_eq!(r.body["model"], "deepseek-v4-flash");
        assert_eq!(r.body["stream"], true);
        // System stays a message in the OpenAI family.
        assert_eq!(r.body["messages"][0]["role"], "system");
    }

    // MARK: Anthropic SSE

    #[test]
    fn anthropic_parses_text_delta() {
        let line = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"好"}}"#;
        assert_eq!(parse_anthropic_line(line), SseLine::Token("好".into()));
    }

    #[test]
    fn anthropic_message_stop_is_done() {
        assert_eq!(parse_anthropic_line(r#"data: {"type":"message_stop"}"#), SseLine::Done);
    }

    #[test]
    fn anthropic_ignores_non_text_events() {
        assert_eq!(parse_anthropic_line("event: content_block_delta"), SseLine::Ignore);
        assert_eq!(parse_anthropic_line(r#"data: {"type":"ping"}"#), SseLine::Ignore);
        assert_eq!(parse_anthropic_line(r#"data: {"type":"content_block_start"}"#), SseLine::Ignore);
    }

    #[test]
    fn anthropic_request_splits_system_out() {
        let c = Client { family: ProtocolFamily::Anthropic, base_url: "https://api.anthropic.com".into(), api_key: "k".into(), http: reqwest::Client::new() };
        let r = c.prepare_stream_request(&msgs(), "claude-haiku-4-5");
        assert_eq!(r.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(r.body["system"], "系统指令");
        // system must NOT appear in messages[].
        assert_eq!(r.body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(r.body["messages"][0]["role"], "user");
        assert_eq!(r.body["max_tokens"], 4096);
        assert!(r.headers.iter().any(|(k, v)| k == "x-api-key" && v == "k"));
        assert!(r.headers.iter().any(|(k, _)| k == "anthropic-version"));
    }

    // MARK: Gemini SSE

    #[test]
    fn gemini_parses_text_parts() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"He"},{"text":"llo"}],"role":"model"}}]}"#;
        assert_eq!(parse_gemini_line(line), SseLine::Token("Hello".into()));
    }

    #[test]
    fn gemini_ignores_non_data_lines() {
        assert_eq!(parse_gemini_line(""), SseLine::Ignore);
        assert_eq!(parse_gemini_line(": comment"), SseLine::Ignore);
    }

    #[test]
    fn gemini_request_uses_model_in_url_and_system_instruction() {
        let c = Client { family: ProtocolFamily::Google, base_url: "https://generativelanguage.googleapis.com".into(), api_key: "k".into(), http: reqwest::Client::new() };
        let r = c.prepare_stream_request(&msgs(), "gemini-2.5-flash");
        assert_eq!(
            r.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(r.body["systemInstruction"]["parts"][0]["text"], "系统指令");
        assert_eq!(r.body["contents"][0]["role"], "user");
        assert!(r.headers.iter().any(|(k, v)| k == "x-goog-api-key" && v == "k"));
    }

    // MARK: URL joining

    #[test]
    fn join_url_appends_path_to_plain_base() {
        assert_eq!(
            join_url("https://api.deepseek.com", "v1/models"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            join_url("https://api.deepseek.com/", "v1/models"),
            "https://api.deepseek.com/v1/models"
        );
    }

    #[test]
    fn join_url_dedupes_version_segment() {
        // Users paste the SDK-style base ending in /v1 — must not double it.
        assert_eq!(
            join_url("https://api.deepseek.com/v1", "v1/models"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            join_url("https://api.deepseek.com/v1/", "v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://proxy.example.com/v1beta", "v1beta/models"),
            "https://proxy.example.com/v1beta/models"
        );
    }

    #[test]
    fn join_url_leaves_other_segments_alone() {
        // A /v1 base with a non-v1 path (or vice versa) still joins plainly.
        assert_eq!(
            join_url("https://api.example.com/v2", "v1/models"),
            "https://api.example.com/v2/v1/models"
        );
    }

    #[test]
    fn extract_error_message_handles_gateway_shapes() {
        assert_eq!(
            extract_error_message(br#"{"error":{"message":"model mimo-v2.5-asr not supported"}}"#),
            Some("model mimo-v2.5-asr not supported".into())
        );
        assert_eq!(
            extract_error_message(br#"{"error":"bad model"}"#),
            Some("bad model".into())
        );
        assert_eq!(
            extract_error_message(br#"{"message":"Model Not Exist"}"#),
            Some("Model Not Exist".into())
        );
        assert_eq!(extract_error_message(b"not json"), None);
        assert_eq!(extract_error_message(br#"{"code":400}"#), None);
    }

    // MARK: status mapping

    #[test]
    fn status_mapping_matches_swift_vocabulary() {
        assert_eq!(error_for_status(401, b""), ProviderError::Unauthorized);
        assert_eq!(error_for_status(402, b""), ProviderError::InsufficientBalance);
        assert_eq!(error_for_status(403, br#"{"error":{"message":"no access"}}"#), ProviderError::Forbidden("no access".into()));
        assert_eq!(error_for_status(429, b""), ProviderError::RateLimited);
        assert_eq!(error_for_status(400, br#"{"error":{"message":"bad"}}"#), ProviderError::BadRequest(400, "bad".into()));
        assert_eq!(error_for_status(500, b""), ProviderError::ServerError(500, String::new()));
    }
}
