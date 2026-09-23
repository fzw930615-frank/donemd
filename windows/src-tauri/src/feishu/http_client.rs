//! `FeishuAPIClient.swift` 后半的移植 — FeishuHttpApi 编排层。
//!
//! 在可注入传输([`FeishuTransport`],生产为 reqwest)之上实现
//! [`FeishuApi`] 的十个方法:401 静默刷新重试一次、429/5xx 退避
//! 重试、错误全表分类(api.rs 的 [`classify_response`] 是单一真
//! 源)、块列表分页聚合、图片 SHA-256 缓存、下载旁路管线。
//!
//! token 供给经 [`AccessTokenProvider`]:F2.3 的 manager 会用
//! oauth 客户端实现它(access_token = refresh_if_needed,
//! force_refresh = 强制刷新)。

use crate::feishu::api::{
    batch_delete_request, classify_response, create_document_request, document_meta_request,
    download_image_request, envelope_metadata, insert_descendants_request, jitter_noise,
    parse_blocks_page, parse_created_document_id, parse_revision, parse_uploaded_file_token,
    parse_wiki_node, pull_blocks_request, sha256_hex, truncate_detail, update_title_request,
    upload_image_request, wiki_get_node_request, AccessTokenProvider, BackoffSleeper, FeishuApi,
    FeishuApiError, FeishuBackoffPolicy, FeishuImageCache, FeishuTransport,
    InMemoryFeishuImageCache, PreparedRequest, ResponseVerdict, TransportResponse,
    WikiNodeResolution, CODE_SCOPE_INSUFFICIENT,
};
use crate::feishu::block::{FeishuBlock, Payload};
use crate::feishu::encoder;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// 生产睡眠器 — tokio 定时(零延迟也调,调用方可观察让出)。
pub struct TokioSleeper;

impl BackoffSleeper for TokioSleeper {
    async fn sleep(&self, seconds: f64) {
        tokio::time::sleep(std::time::Duration::from_secs_f64(seconds)).await;
    }
}

/// reqwest 生产传输 — 把 [`PreparedRequest`] 变成真实 HTTP 往返。
/// 请求构造的断言全在 api.rs 纯函数层,这里只做 I/O。
pub struct ReqwestTransport {
    http: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl FeishuTransport for ReqwestTransport {
    async fn execute(&self, request: PreparedRequest) -> Result<TransportResponse, FeishuApiError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| FeishuApiError::DecodeFailed(format!("invalid method: {e}")))?;
        let url: reqwest::Url = request
            .url
            .parse()
            .map_err(|e| FeishuApiError::DecodeFailed(format!("invalid url: {e}")))?;
        let mut builder = self.http.request(method, url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        builder = builder.body(request.body);
        let response = builder
            .send()
            .await
            .map_err(|e| FeishuApiError::NetworkUnreachable(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|e| FeishuApiError::NetworkUnreachable(e.to_string()))?
            .to_vec();
        Ok(TransportResponse { status, headers, body })
    }
}

/// 编排客户端 — 泛型注入传输 / token 供给 / 睡眠器,静态分发
/// (AFIT,无 async-trait)。测试三件套全换 Mock。
pub struct FeishuHttpApi<T, P, S> {
    transport: T,
    tokens: P,
    sleeper: S,
    backoff: FeishuBackoffPolicy,
    image_cache: Arc<dyn FeishuImageCache>,
    base_url: String,
}

impl<T: FeishuTransport, P: AccessTokenProvider, S: BackoffSleeper> FeishuHttpApi<T, P, S> {
    /// 全件注入 — 测试用(退避注 immediate、睡眠器注计数器)。
    pub fn with_parts(
        base_url: impl Into<String>,
        transport: T,
        tokens: P,
        sleeper: S,
        backoff: FeishuBackoffPolicy,
        image_cache: Arc<dyn FeishuImageCache>,
    ) -> Self {
        Self {
            transport,
            tokens,
            sleeper,
            backoff,
            image_cache,
            base_url: base_url.into(),
        }
    }

    /// 取 bearer。Swift 语义:token 端点自身的网络故障 =
    /// NetworkUnreachable(token 端点也是 HTTP 调用,断网对用户是
    /// 同一个问题);其余(未登录 / 登录取消 / keyring 不可用 /
    /// 交换解码失败)一律 Unauthorized — 路由到「请重新登录」文案。
    async fn bearer(&self) -> Result<String, FeishuApiError> {
        match self.tokens.access_token().await {
            Ok(token) => Ok(token),
            Err(FeishuApiError::NetworkUnreachable(detail)) => {
                Err(FeishuApiError::NetworkUnreachable(detail))
            }
            Err(_) => Err(FeishuApiError::Unauthorized),
        }
    }

    /// `sendWithRetryData` 对应物 — 可靠性协议主干:
    /// 挂 Bearer → 401 刷新重试一次 → 429/5xx 退避重试至耗尽 →
    /// 其余错误按 [`classify_response`] 直通。`build` 每轮用新
    /// token 重建请求。
    async fn send_with_retry_data(
        &self,
        resource: &str,
        build: impl Fn(&str) -> PreparedRequest,
    ) -> Result<Vec<u8>, FeishuApiError> {
        let mut did_refresh_after_401 = false;
        let mut retry_attempt = 0usize;
        loop {
            let token = self.bearer().await?;
            let request = build(&token);
            let url = request.url.clone();
            let response = self.transport.execute(request).await?;

            // 非 2xx 或信封 code≠0 打印原文(截断 1000 字),真机
            // 排障要看到飞书自己的错误串(如 "param X invalid")。
            let (envelope_code, _) = envelope_metadata(&response.body);
            if !(200..=299).contains(&response.status) || envelope_code.unwrap_or(0) != 0 {
                let raw = String::from_utf8_lossy(&response.body);
                let raw: String = if raw.chars().count() > 1000 {
                    raw.chars().take(1000).collect::<String>() + "…(truncated)"
                } else {
                    raw.into_owned()
                };
                eprintln!("[feishu] http {} {url} → {raw}", response.status);
            }

            match classify_response(response.status, &response.body) {
                ResponseVerdict::Ok => return Ok(response.body),
                // 401 / 99991663 → 一次静默刷新重试;刷新自身失败
                // (任意错误)直接升级 Unauthorized。
                ResponseVerdict::Unauthorized => {
                    if did_refresh_after_401 {
                        return Err(FeishuApiError::Unauthorized);
                    }
                    did_refresh_after_401 = true;
                    if self.tokens.force_refresh().await.is_err() {
                        return Err(FeishuApiError::Unauthorized);
                    }
                    continue;
                }
                ResponseVerdict::ScopeInsufficient { detail } => {
                    return Err(FeishuApiError::ScopeInsufficient { detail })
                }
                ResponseVerdict::Forbidden { message } => {
                    return Err(FeishuApiError::Forbidden { message })
                }
                ResponseVerdict::NotFound => {
                    return Err(FeishuApiError::NotFound {
                        resource: resource.to_string(),
                    })
                }
                ResponseVerdict::Retryable { rate_limited } => {
                    if retry_attempt >= self.backoff.max_attempts {
                        if rate_limited {
                            return Err(FeishuApiError::RateLimited);
                        }
                        // 耗尽后的 ServerError 携带最后一发响应的信封
                        // code/msg(Swift 同款)。
                        let (code, message) = envelope_metadata(&response.body);
                        return Err(FeishuApiError::ServerError {
                            http_status: response.status,
                            code,
                            message,
                        });
                    }
                    let delay = self
                        .backoff
                        .delay_for_attempt(retry_attempt, jitter_noise(self.backoff.jitter));
                    // 恒调 sleeper(含零延迟),测试借此计数重试事件。
                    self.sleeper.sleep(delay).await;
                    retry_attempt += 1;
                    continue;
                }
                ResponseVerdict::Fatal(err) => return Err(err),
            }
        }
    }

    /// 下载旁路管线 — 不走 [`Self::send_with_retry_data`],因为要读
    /// Content-Type 头,且二进制体会干扰信封探测。重试/刷新语义
    /// 内联重实现(对齐 Swift `downloadImage` 的 bespoke pipeline):
    /// 401 仅按 HTTP status 判定(二进制体无信封可读)、403 不带
    /// 消息、4xx 里单独截获 99991679。
    async fn download_with_retry(&self, media_token: &str) -> Result<TransportResponse, FeishuApiError> {
        let mut did_refresh_after_401 = false;
        let mut retry_attempt = 0usize;
        loop {
            let token = self.bearer().await?;
            let request = download_image_request(&self.base_url, &token, media_token);
            let response = self.transport.execute(request).await?;

            if response.status == 401 {
                if did_refresh_after_401 {
                    return Err(FeishuApiError::Unauthorized);
                }
                did_refresh_after_401 = true;
                if self.tokens.force_refresh().await.is_err() {
                    return Err(FeishuApiError::Unauthorized);
                }
                continue;
            }
            if response.status == 403 {
                return Err(FeishuApiError::Forbidden { message: None });
            }
            if response.status == 404 {
                return Err(FeishuApiError::NotFound {
                    resource: media_token.to_string(),
                });
            }
            if response.status == 429 || (500..=599).contains(&response.status) {
                if retry_attempt >= self.backoff.max_attempts {
                    if response.status == 429 {
                        return Err(FeishuApiError::RateLimited);
                    }
                    // 下载路径的耗尽 ServerError 不带信封(对齐 Swift:
                    // code: nil, message: nil)。
                    return Err(FeishuApiError::ServerError {
                        http_status: response.status,
                        code: None,
                        message: None,
                    });
                }
                let delay = self
                    .backoff
                    .delay_for_attempt(retry_attempt, jitter_noise(self.backoff.jitter));
                self.sleeper.sleep(delay).await;
                retry_attempt += 1;
                continue;
            }
            if !(200..=299).contains(&response.status) {
                if (400..=499).contains(&response.status) {
                    // 4xx 体可能带飞书信封:99991679 必须落专用桶,
                    // 否则 scope 缺失被读成「请求体畸形」。
                    let (code, msg) = envelope_metadata(&response.body);
                    if code == Some(CODE_SCOPE_INSUFFICIENT) {
                        return Err(FeishuApiError::ScopeInsufficient {
                            detail: msg.as_deref().and_then(truncate_detail),
                        });
                    }
                    return Err(FeishuApiError::BadRequest {
                        http_status: response.status,
                        code,
                        message: msg,
                    });
                }
                return Err(FeishuApiError::ServerError {
                    http_status: response.status,
                    code: None,
                    message: None,
                });
            }
            return Ok(response);
        }
    }
}

/// 生产装配的具体类型面 — `new` 固定 ReqwestTransport + TokioSleeper
/// (泛型 impl 内无法把类型参数 S 实例化为具体类型,故单独立块)。
impl<P: AccessTokenProvider> FeishuHttpApi<ReqwestTransport, P, TokioSleeper> {
    /// 生产装配:tokio 睡眠器 + 生产退避表 + 内存图片缓存。
    pub fn new(base_url: impl Into<String>, transport: ReqwestTransport, tokens: P) -> Self {
        Self::with_parts(
            base_url,
            transport,
            tokens,
            TokioSleeper,
            FeishuBackoffPolicy::PRODUCTION,
            Arc::new(InMemoryFeishuImageCache::default()),
        )
    }
}

impl<T: FeishuTransport, P: AccessTokenProvider, S: BackoffSleeper> FeishuApi
    for FeishuHttpApi<T, P, S>
{
    async fn pull_document(
        &self,
        document_id: &str,
    ) -> Result<(Vec<FeishuBlock>, i64), FeishuApiError> {
        let mut collected: Vec<FeishuBlock> = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let next = page_token.clone();
            let body = self
                .send_with_retry_data(document_id, |token| {
                    pull_blocks_request(&self.base_url, token, document_id, next.as_deref())
                })
                .await?;
            let page = parse_blocks_page(&body)?;
            collected.extend(page.blocks);
            // has_more=false 或缺 page_token 都是终止(has_more true 而
            // token 缺失 → 结束,防死循环;对齐 Swift 的 nil 判定)。
            page_token = page.page_token;
            if page_token.is_none() {
                break;
            }
        }
        // 块列表端点不带 revision — 分页完成后单读文档元数据。
        // 失败模式测试(401/403/404/429/5xx 打在 blocks 页)的请求
        // 计数不因这步而漂移。
        let revision = self.get_document_revision(document_id).await?;
        Ok((collected, revision))
    }

    async fn push_document(&self, document_id: &str, blocks: &[FeishuBlock]) -> Result<(), FeishuApiError> {
        // 块编排「先删后建」:列现有根子块 → 范围删 → 一次 POST
        // descendants。内部拉取只为找页块 + 根子块,revision 丢弃
        // (但 meta 调用照发 — 对齐 Swift,请求序列可断言)。
        let (existing, _revision) = self.pull_document(document_id).await?;
        let page = existing
            .iter()
            .find(|b| matches!(b.payload, Payload::Page(_)))
            .ok_or_else(|| FeishuApiError::DecodeFailed("pulled document has no page block".into()))?;
        let page_block_id = page.block_id.clone();
        let root_child_count = page.children.as_ref().map_or(0, |ids| ids.len());

        if root_child_count > 0 {
            self.delete_children_range(document_id, &page_block_id, 0, root_child_count)
                .await?;
        }

        let body = encoder::encode_descendant_body_at(blocks, -1)
            .map_err(|e| FeishuApiError::DecodeFailed(e.to_string()))?;
        log_wire_body("descendant POST", &body);
        let parent = page_block_id.clone();
        self.send_with_retry_data(document_id, |token| {
            insert_descendants_request(&self.base_url, token, document_id, &parent, &body)
        })
        .await?;
        Ok(())
    }

    async fn delete_children_range(
        &self,
        document_id: &str,
        parent_block_id: &str,
        start_index: usize,
        end_index: usize,
    ) -> Result<(), FeishuApiError> {
        self.send_with_retry_data(document_id, |token| {
            batch_delete_request(
                &self.base_url,
                token,
                document_id,
                parent_block_id,
                start_index,
                end_index,
            )
        })
        .await?;
        Ok(())
    }

    async fn insert_children_at(
        &self,
        document_id: &str,
        parent_block_id: &str,
        index: i64,
        blocks: &[FeishuBlock],
    ) -> Result<(), FeishuApiError> {
        // 页块是 converter 预置的合成根;按「非页块数」判空 —
        // page-only 段 = 无内容,直接 no-op(两个占位块相邻时)。
        let non_page_count = blocks
            .iter()
            .filter(|b| !matches!(b.payload, Payload::Page(_)))
            .count();
        if non_page_count == 0 {
            return Ok(());
        }
        let body = encoder::encode_descendant_body_at(blocks, index)
            .map_err(|e| FeishuApiError::DecodeFailed(e.to_string()))?;
        log_wire_body("descendant POST", &body);
        self.send_with_retry_data(document_id, |token| {
            insert_descendants_request(&self.base_url, token, document_id, parent_block_id, &body)
        })
        .await?;
        Ok(())
    }

    async fn create_document(
        &self,
        title: &str,
        parent_token: Option<&str>,
    ) -> Result<String, FeishuApiError> {
        let body = self
            .send_with_retry_data(title, |token| {
                create_document_request(&self.base_url, token, title, parent_token)
            })
            .await?;
        parse_created_document_id(&body)
    }

    async fn update_document_title(&self, document_id: &str, title: &str) -> Result<(), FeishuApiError> {
        self.send_with_retry_data(document_id, |token| {
            update_title_request(&self.base_url, token, document_id, title)
        })
        .await?;
        Ok(())
    }

    async fn upload_image(
        &self,
        data: &[u8],
        mime_type: &str,
        file_name: &str,
        document_id: &str,
    ) -> Result<String, FeishuApiError> {
        // 缓存键 = 内容 SHA-256(不是文件名 — 同图改名不重传)。
        let hash = sha256_hex(data);
        if let Some(cached) = self.image_cache.token_for_sha256(&hash) {
            return Ok(cached);
        }
        let boundary = format!("Boundary-{}", Uuid::new_v4());
        let body = self
            .send_with_retry_data(file_name, |token| {
                upload_image_request(
                    &self.base_url,
                    token,
                    data,
                    mime_type,
                    file_name,
                    document_id,
                    &boundary,
                )
            })
            .await?;
        let token = parse_uploaded_file_token(&body)?;
        self.image_cache.remember_token(&token, &hash);
        Ok(token)
    }

    async fn download_image(&self, token: &str) -> Result<(Vec<u8>, String), FeishuApiError> {
        let response = self.download_with_retry(token).await?;
        // Content-Type 缺省按 image/png(上传侧文件名启发式同款防御)。
        let mime = response
            .header("Content-Type")
            .unwrap_or("image/png")
            .to_string();
        Ok((response.body, mime))
    }

    async fn resolve_wiki_node(&self, token: &str) -> Result<WikiNodeResolution, FeishuApiError> {
        let body = self
            .send_with_retry_data(token, |bearer| {
                wiki_get_node_request(&self.base_url, bearer, token)
            })
            .await?;
        parse_wiki_node(&body, token)
    }

    async fn get_document_revision(&self, document_id: &str) -> Result<i64, FeishuApiError> {
        let body = self
            .send_with_retry_data(document_id, |token| {
                document_meta_request(&self.base_url, token, document_id)
            })
            .await?;
        parse_revision(&body, document_id)
    }
}

/// descendant 请求体日志(截断 4000 字)— 飞书严格校验器回
/// 1770001 不说是哪个字段,用户报障时把手头 JSON 给出来就能定位。
fn log_wire_body(desc: &str, body: &Value) {
    let raw = body.to_string();
    let truncated: String = if raw.chars().count() > 4000 {
        raw.chars().take(4000).collect::<String>() + "…(truncated)"
    } else {
        raw
    };
    eprintln!("[feishu] {desc} body={truncated}");
}

// MARK: - 测试替身(跨文件共享:oauth.rs 的测试也用它)

/// FIFO 响应队列 + 请求记录 — Swift `APIMockProtocol` 对应物。
/// 队列空时再收到请求直接 panic(测试编排错了)。`Clone` 共享同一
/// 内部状态:句柄留在测试手里,本体装进客户端,事后仍能断言。
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct MockTransport {
    inner: std::sync::Arc<MockTransportInner>,
}

#[cfg(test)]
struct MockTransportInner {
    responses: std::sync::Mutex<std::collections::VecDeque<Result<TransportResponse, FeishuApiError>>>,
    requests: std::sync::Mutex<Vec<RecordedRequest>>,
}

/// 一条已记录的请求(字段克隆自 PreparedRequest)。
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct RecordedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[cfg(test)]
impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
impl MockTransport {
    pub(crate) fn new(
        responses: Vec<Result<TransportResponse, FeishuApiError>>,
    ) -> Self {
        Self {
            inner: std::sync::Arc::new(MockTransportInner {
                responses: std::sync::Mutex::new(responses.into_iter().collect()),
                requests: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    pub(crate) fn recorded(&self) -> Vec<RecordedRequest> {
        self.inner.requests.lock().unwrap().clone()
    }

    pub(crate) fn request_count(&self) -> usize {
        self.inner.requests.lock().unwrap().len()
    }
}

#[cfg(test)]
impl FeishuTransport for MockTransport {
    async fn execute(&self, request: PreparedRequest) -> Result<TransportResponse, FeishuApiError> {
        self.inner.requests.lock().unwrap().push(RecordedRequest {
            method: request.method.clone(),
            url: request.url.clone(),
            headers: request.headers.clone(),
            body: request.body.clone(),
        });
        self.inner
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("mock transport received an unexpected request")
    }
}

// MARK: - 测试(FeishuAPIClientTests.swift 的编排组)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::api::{
        FeishuApi, FeishuApiError, FeishuBackoffPolicy, TransportResponse,
    };
    use crate::feishu::credentials::{CredentialStore, InMemoryCredentialStore};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // MARK: 测试替身

    /// 固定 token 的供给器。
    struct StaticTokens(&'static str);

    impl AccessTokenProvider for StaticTokens {
        async fn access_token(&self) -> Result<String, FeishuApiError> {
            Ok(self.0.to_string())
        }
    }

    /// OLD→NEW:401 后 force_refresh 换 NEW(计数)。Arc 内芯 +
    /// Clone — 测试留一份句柄读计数,另一份随客户端走。
    #[derive(Clone)]
    struct RotatingTokens {
        inner: std::sync::Arc<RotatingTokensInner>,
    }

    struct RotatingTokensInner {
        current: Mutex<String>,
        refreshes: AtomicUsize,
    }

    impl RotatingTokens {
        fn old_new() -> Self {
            Self {
                inner: std::sync::Arc::new(RotatingTokensInner {
                    current: Mutex::new("OLD".to_string()),
                    refreshes: AtomicUsize::new(0),
                }),
            }
        }
        fn refreshes(&self) -> usize {
            self.inner.refreshes.load(Ordering::SeqCst)
        }
    }

    impl AccessTokenProvider for RotatingTokens {
        async fn access_token(&self) -> Result<String, FeishuApiError> {
            Ok(self.inner.current.lock().unwrap().clone())
        }
        async fn force_refresh(&self) -> Result<String, FeishuApiError> {
            self.inner.refreshes.fetch_add(1, Ordering::SeqCst);
            *self.inner.current.lock().unwrap() = "NEW".to_string();
            Ok("NEW".to_string())
        }
    }

    /// 恒失败的供给器(bearer 阶段错误映射用)。
    struct FailingTokens(FeishuApiError);

    impl AccessTokenProvider for FailingTokens {
        async fn access_token(&self) -> Result<String, FeishuApiError> {
            Err(self.0.clone())
        }
    }

    /// 计数睡眠器(零等待)。Arc 内芯 + Clone — 同上,测试留句柄。
    #[derive(Clone)]
    struct CountingSleeper {
        inner: std::sync::Arc<Mutex<Vec<f64>>>,
    }

    impl CountingSleeper {
        fn new() -> Self {
            Self {
                inner: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn count(&self) -> usize {
            self.inner.lock().unwrap().len()
        }
    }

    impl BackoffSleeper for CountingSleeper {
        async fn sleep(&self, seconds: f64) {
            self.inner.lock().unwrap().push(seconds);
        }
    }

    // MARK: 响应构造

    fn resp(status: u16, body: &str) -> Result<TransportResponse, FeishuApiError> {
        Ok(TransportResponse {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        })
    }

    fn ok(body: &str) -> Result<TransportResponse, FeishuApiError> {
        resp(200, body)
    }

    fn err(err: FeishuApiError) -> Result<TransportResponse, FeishuApiError> {
        Err(err)
    }

    /// 块列表页(items / has_more / page_token)。
    fn blocks_page(items: &str, has_more: bool, page_token: Option<&str>) -> String {
        match page_token {
            Some(token) => format!(
                r#"{{"code":0,"data":{{"items":[{items}],"page_token":"{token}","has_more":{has_more}}}}}"#
            ),
            None => format!(
                r#"{{"code":0,"data":{{"items":[{items}],"page_token":null,"has_more":{has_more}}}}}"#
            ),
        }
    }

    /// 文档元数据信封(pull 尾声的 revision 单读)。
    fn meta_doc(revision: i64) -> String {
        format!(r#"{{"code":0,"data":{{"document":{{"document_id":"d","revision_id":{revision}}}}}}}"#)
    }

    fn make_api(
        transport: MockTransport,
        tokens: StaticTokens,
    ) -> FeishuHttpApi<MockTransport, StaticTokens, CountingSleeper> {
        FeishuHttpApi::with_parts(
            "https://open.feishu.cn",
            transport,
            tokens,
            CountingSleeper::new(),
            FeishuBackoffPolicy::IMMEDIATE,
            std::sync::Arc::new(crate::feishu::api::InMemoryFeishuImageCache::default()),
        )
    }

    // MARK: - Bearer 头 + 401 刷新

    #[tokio::test]
    async fn requests_carry_authorization_bearer_header() {
        let transport = MockTransport::new(vec![
            ok(&blocks_page("", false, None)),
            ok(&meta_doc(1)),
        ]);
        let api = make_api(transport.clone(), StaticTokens("AAA111"));
        api.pull_document("doc_xyz").await.unwrap();
        let recorded = transport.recorded();
        assert_eq!(recorded[0].header("Authorization"), Some("Bearer AAA111"));
    }

    #[tokio::test]
    async fn unauthorized_401_triggers_one_refresh_and_retries() {
        let transport = MockTransport::new(vec![
            resp(401, r#"{"code":99991663,"msg":"token invalid"}"#),
            ok(&blocks_page("", false, None)),
            ok(&meta_doc(1)),
        ]);
        let tokens = RotatingTokens::old_new();
        let api = FeishuHttpApi::with_parts(
            "https://open.feishu.cn",
            transport.clone(),
            tokens.clone(),
            CountingSleeper::new(),
            FeishuBackoffPolicy::IMMEDIATE,
            std::sync::Arc::new(crate::feishu::api::InMemoryFeishuImageCache::default()),
        );

        api.pull_document("doc_xyz").await.unwrap();

        assert_eq!(tokens.refreshes(), 1);
        let recorded = transport.recorded();
        // blocks 401 + 重试 + meta = 3 请求。
        assert_eq!(recorded.len(), 3, "blocks 401 + retry + meta = 3 requests");
        assert_eq!(recorded[0].header("Authorization"), Some("Bearer OLD"));
        assert_eq!(recorded[1].header("Authorization"), Some("Bearer NEW"));
    }

    #[tokio::test]
    async fn unauthorized_401_after_refresh_surfaces_unauthorized() {
        let transport = MockTransport::new(vec![
            resp(401, r#"{"code":99991663,"msg":"token invalid"}"#),
            resp(401, r#"{"code":99991663,"msg":"token still invalid"}"#),
        ]);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        assert_eq!(err, FeishuApiError::Unauthorized);
        // 刷新后第二次 401 直接升级,不再刷新。
        assert_eq!(transport.request_count(), 2);
    }

    // MARK: - 403 / 404 / scope

    #[tokio::test]
    async fn forbidden_surfaces_scope_message() {
        let transport = MockTransport::new(vec![resp(
            403,
            r#"{"code":99991664,"msg":"scope insufficient: docx:document"}"#,
        )]);
        let api = make_api(transport, StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        assert_eq!(
            err,
            FeishuApiError::Forbidden {
                message: Some("scope insufficient: docx:document".into())
            }
        );
    }

    /// 99991679(应用 scope 不足)线上是 HTTP 400 — 必须落专用桶,
    /// 不能进 badRequest(读起来像「请求体畸形」,信号全错)。
    #[tokio::test]
    async fn scope_insufficient_routes_away_from_bad_request() {
        let transport = MockTransport::new(vec![resp(
            400,
            r#"{"code":99991679,"msg":"Unauthorized. ... required: [docs:document.media:download]"}"#,
        )]);
        let api = make_api(transport, StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        match err {
            FeishuApiError::ScopeInsufficient { detail } => {
                // detail 携带原文,排障能认出缺的 scope 名。
                assert!(detail.unwrap().contains("docs:document.media:download"));
            }
            other => panic!("expected scopeInsufficient, got {other:?}"),
        }
    }

    /// 下载旁路管线没有 JSON 信封可读,99991679 检测是单独实现的 —
    /// 锁死。
    #[tokio::test]
    async fn download_image_scope_insufficient() {
        let transport = MockTransport::new(vec![resp(
            400,
            r#"{"code":99991679,"msg":"required: [docs:document.media:download]"}"#,
        )]);
        let api = make_api(transport, StaticTokens("T"));
        let err = api.download_image("IMG_TOKEN").await.unwrap_err();
        match err {
            FeishuApiError::ScopeInsufficient { detail } => {
                assert!(detail.unwrap().contains("docs:document.media:download"));
            }
            other => panic!("expected scopeInsufficient, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_found_carries_resource() {
        let transport = MockTransport::new(vec![resp(404, "{}")]);
        let api = make_api(transport, StaticTokens("T"));
        let err = api.pull_document("missing-doc").await.unwrap_err();
        assert_eq!(err, FeishuApiError::NotFound { resource: "missing-doc".into() });
    }

    // MARK: - 429 / 5xx 重试

    #[tokio::test]
    async fn rate_limited_429_backs_off_then_succeeds() {
        let transport = MockTransport::new(vec![
            resp(429, r#"{"code":99991400,"msg":"rate limited"}"#),
            resp(429, r#"{"code":99991400,"msg":"rate limited"}"#),
            ok(&blocks_page("", false, None)),
            ok(&meta_doc(1)),
        ]);
        let sleeper = CountingSleeper::new();
        let api = FeishuHttpApi::with_parts(
            "https://open.feishu.cn",
            transport.clone(),
            StaticTokens("T"),
            sleeper.clone(),
            FeishuBackoffPolicy::IMMEDIATE,
            std::sync::Arc::new(crate::feishu::api::InMemoryFeishuImageCache::default()),
        );
        api.pull_document("d").await.unwrap();
        // 两次被限流的重试 + 成功的 blocks + meta = 4 请求。
        assert_eq!(transport.request_count(), 4);
        // 每次重试恰一次 sleep,成功腿不多睡。
        assert_eq!(sleeper.count(), 2);
    }

    #[tokio::test]
    async fn rate_limited_429_exhausts_retries() {
        let responses = (0..5)
            .map(|_| resp(429, r#"{"code":99991400,"msg":"rate limited"}"#))
            .collect();
        let transport = MockTransport::new(responses);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        assert_eq!(err, FeishuApiError::RateLimited);
        // 首发 + maxAttempts(4) 次重试 = 5 调用。
        assert_eq!(transport.request_count(), 5);
    }

    #[tokio::test]
    async fn server_error_5xx_retries_then_throws_with_envelope() {
        let responses = (0..5)
            .map(|_| resp(503, r#"{"code":99991500,"msg":"server overloaded"}"#))
            .collect();
        let transport = MockTransport::new(responses);
        let api = make_api(transport, StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        // 耗尽后的 ServerError 携带最后一发响应的信封 code/msg。
        assert_eq!(
            err,
            FeishuApiError::ServerError {
                http_status: 503,
                code: Some(99991500),
                message: Some("server overloaded".into()),
            }
        );
    }

    // MARK: - 4xx 不重试

    #[tokio::test]
    async fn bad_request_400_surfaces_and_does_not_retry() {
        // 真机验证期的教训:400 曾被映射成 serverError,把参数错误谎报
        // 成瞬时故障。4xx 是客户端侧,一发即止。
        let transport = MockTransport::new(vec![resp(
            400,
            r#"{"code":99991678,"msg":"invalid param"}"#,
        )]);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        assert_eq!(
            err,
            FeishuApiError::BadRequest {
                http_status: 400,
                code: Some(99991678),
                message: Some("invalid param".into()),
            }
        );
        assert_eq!(transport.request_count(), 1, "4xx must not be retried");
    }

    // MARK: - 传输层 / token 供给

    #[tokio::test]
    async fn transport_error_surfaces_network_unreachable() {
        let transport = MockTransport::new(vec![err(FeishuApiError::NetworkUnreachable(
            "dns failure".into(),
        ))]);
        let api = make_api(transport, StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        assert_eq!(
            err,
            FeishuApiError::NetworkUnreachable("dns failure".into())
        );
    }

    #[tokio::test]
    async fn token_provider_failure_maps_to_unauthorized() {
        // Swift 语义:供给器除 NetworkUnreachable 外的失败(未登录 /
        // 取消 / keyring)一律 Unauthorized — 路由到「请重新登录」。
        let transport = MockTransport::new(Vec::new());
        let api = FeishuHttpApi::with_parts(
            "https://open.feishu.cn",
            transport,
            FailingTokens(FeishuApiError::DecodeFailed("not logged in".into())),
            CountingSleeper::new(),
            FeishuBackoffPolicy::IMMEDIATE,
            std::sync::Arc::new(crate::feishu::api::InMemoryFeishuImageCache::default()),
        );
        let err = api.pull_document("d").await.unwrap_err();
        assert_eq!(err, FeishuApiError::Unauthorized);
    }

    #[tokio::test]
    async fn token_provider_network_failure_propagates_verbatim() {
        let transport = MockTransport::new(Vec::new());
        let api = FeishuHttpApi::with_parts(
            "https://open.feishu.cn",
            transport,
            FailingTokens(FeishuApiError::NetworkUnreachable("offline".into())),
            CountingSleeper::new(),
            FeishuBackoffPolicy::IMMEDIATE,
            std::sync::Arc::new(crate::feishu::api::InMemoryFeishuImageCache::default()),
        );
        let err = api.pull_document("d").await.unwrap_err();
        assert_eq!(err, FeishuApiError::NetworkUnreachable("offline".into()));
    }

    #[tokio::test]
    async fn malformed_json_surfaces_decode_failed() {
        // 200 + 非 JSON 体:分类层(信封读不出)放行,解析层拦截 —
        // 契约是「不把垃圾递给调用方」。
        let transport = MockTransport::new(vec![ok("not valid json")]);
        let api = make_api(transport, StaticTokens("T"));
        let err = api.pull_document("d").await.unwrap_err();
        assert!(matches!(err, FeishuApiError::DecodeFailed(_)));
    }

    // MARK: - pullDocument 分页

    #[tokio::test]
    async fn pull_document_follows_page_token() {
        let transport = MockTransport::new(vec![
            ok(&blocks_page(
                r#"{"block_id":"b1","parent_id":null,"block_type":22}"#,
                true,
                Some("PAGE2"),
            )),
            ok(&blocks_page(
                r#"{"block_id":"b2","parent_id":null,"block_type":22}"#,
                false,
                None,
            )),
            ok(&meta_doc(1)),
        ]);
        let api = make_api(transport.clone(), StaticTokens("T"));

        let (blocks, _revision) = api.pull_document("doc_paged").await.unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].block_id, "b1");
        assert_eq!(blocks[1].block_id, "b2");
        // 第二发必须带 page_token。
        let second = &transport.recorded()[1];
        assert!(second.url.contains("page_token=PAGE2"), "{}", second.url);
    }

    #[tokio::test]
    async fn pull_document_returns_revision_from_meta() {
        let transport = MockTransport::new(vec![
            ok(&blocks_page(
                r#"{"block_id":"b1","parent_id":null,"block_type":22}"#,
                false,
                None,
            )),
            ok(&meta_doc(7)),
        ]);
        let api = make_api(transport.clone(), StaticTokens("T"));

        let (blocks, revision) = api.pull_document("doc_rev").await.unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(revision, 7, "revision_id 从 data.document 流下来");
        // meta 调用打裸文档端点。
        let second = &transport.recorded()[1];
        let path = second.url.split('?').next().unwrap();
        assert_eq!(path, "https://open.feishu.cn/open-apis/docx/v1/documents/doc_rev");
    }

    // MARK: - pushDocument(先删后建编排)

    #[tokio::test]
    async fn push_document_deletes_existing_then_posts_descendant() {
        // 一个现存根子块:push 依序发 pull blocks → meta →
        // batch_delete → descendant。(内部拉取丢弃 revision,但
        // meta 调用照发 — 请求序列可断言。)
        let transport = MockTransport::new(vec![
            ok(&blocks_page(
                r#"{"block_id":"doc_target","block_type":1,"children":["bx_old"],"page":{"elements":[]}}"#,
                false,
                None,
            )),
            ok(&meta_doc(1)),
            ok(r#"{"code":0}"#),
            ok(r#"{"code":0}"#),
        ]);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let blocks = crate::feishu::converter::to_feishu_blocks("# Hello\n");

        api.push_document("doc_target", &blocks).await.unwrap();

        let recorded = transport.recorded();
        assert_eq!(recorded.len(), 4);
        let path = |i: usize| recorded[i].url.split('?').next().unwrap().to_string();
        assert_eq!(path(0), "https://open.feishu.cn/open-apis/docx/v1/documents/doc_target/blocks");
        assert_eq!(recorded[0].method, "GET");
        assert_eq!(path(1), "https://open.feishu.cn/open-apis/docx/v1/documents/doc_target");
        assert_eq!(recorded[1].method, "GET");
        assert_eq!(
            path(2),
            "https://open.feishu.cn/open-apis/docx/v1/documents/doc_target/blocks/doc_target/children/batch_delete"
        );
        assert_eq!(recorded[2].method, "DELETE");
        assert_eq!(
            path(3),
            "https://open.feishu.cn/open-apis/docx/v1/documents/doc_target/blocks/doc_target/descendant"
        );
        assert_eq!(recorded[3].method, "POST");

        let delete_body: Value = serde_json::from_slice(&recorded[2].body).unwrap();
        assert_eq!(delete_body["start_index"], 0);
        assert_eq!(
            delete_body["end_index"], 1,
            "end_index 必须等于现存根子块数"
        );

        let desc_body: Value = serde_json::from_slice(&recorded[3].body).unwrap();
        assert_eq!(desc_body["index"], -1);
        assert!(desc_body["children_id"].is_array());
        assert!(desc_body["descendants"].is_array());
    }

    #[tokio::test]
    async fn push_document_skips_delete_when_page_has_no_children() {
        // 空文档:无可删,blocks + meta + descendant 三发。
        let transport = MockTransport::new(vec![
            ok(&blocks_page(
                r#"{"block_id":"doc_target","block_type":1,"children":[],"page":{"elements":[]}}"#,
                false,
                None,
            )),
            ok(&meta_doc(1)),
            ok(r#"{"code":0}"#),
        ]);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let blocks = crate::feishu::converter::to_feishu_blocks("# Hello\n");

        api.push_document("doc_target", &blocks).await.unwrap();

        let recorded = transport.recorded();
        assert_eq!(recorded.len(), 3, "no children means no batch_delete leg");
        assert_eq!(
            recorded[2].url.split('?').next().unwrap(),
            "https://open.feishu.cn/open-apis/docx/v1/documents/doc_target/blocks/doc_target/descendant"
        );
    }

    #[tokio::test]
    async fn insert_children_at_empty_segment_is_noop() {
        // page-only 段(两个占位块相邻时的空切面)= 无内容,零网络
        // 调用。converter 对空 markdown 会合成空文本段,所以这里
        // 手工构造纯页块,单测 no-op 判定本身。
        let transport = MockTransport::new(Vec::new());
        let api = make_api(transport.clone(), StaticTokens("T"));
        let page_only = vec![crate::feishu::block::FeishuBlock::new(
            "blk_page",
            Payload::Page(crate::feishu::block::PagePayload::default()),
        )];
        api.insert_children_at("d", "p", -1, &page_only).await.unwrap();
        assert_eq!(transport.request_count(), 0);
    }

    // MARK: - createDocument / 上传缓存 / 下载

    #[tokio::test]
    async fn create_document_returns_document_id() {
        let transport =
            MockTransport::new(vec![ok(r#"{"code":0,"data":{"document":{"document_id":"doxc_NEW"}}}"#)]);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let id = api.create_document("Hello", Some("fldr_PARENT")).await.unwrap();
        assert_eq!(id, "doxc_NEW");
        // 体同时带 title 与 folder_token(形状细则见 api.rs 构造组)。
        let body: Value = serde_json::from_slice(&transport.recorded()[0].body).unwrap();
        assert_eq!(body["title"], "Hello");
        assert_eq!(body["folder_token"], "fldr_PARENT");
    }

    #[tokio::test]
    async fn upload_image_returns_token_and_caches_by_sha256() {
        let transport = MockTransport::new(vec![ok(r#"{"code":0,"data":{"file_token":"img_AAA"}}"#)]);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let payload = b"fake-png-bytes".to_vec();

        let token = api
            .upload_image(&payload, "image/png", "a.png", "doxc_TEST")
            .await
            .unwrap();
        assert_eq!(token, "img_AAA");
        assert_eq!(transport.request_count(), 1);

        // 同字节改名再传 — 必须走缓存,不碰网络(空队列:任何调用
        // 都会 panic mock)。
        let cached = api
            .upload_image(&payload, "image/png", "renamed.png", "doxc_TEST")
            .await
            .unwrap();
        assert_eq!(cached, "img_AAA", "缓存键是内容哈希,不是文件名");
        assert_eq!(transport.request_count(), 1);
    }

    #[tokio::test]
    async fn upload_image_different_bytes_reuploads() {
        let transport = MockTransport::new(vec![
            ok(r#"{"code":0,"data":{"file_token":"img_AAA"}}"#),
            ok(r#"{"code":0,"data":{"file_token":"img_BBB"}}"#),
        ]);
        let api = make_api(transport, StaticTokens("T"));
        let t1 = api.upload_image(&[0x01], "image/png", "a.png", "doxc_TEST").await.unwrap();
        let t2 = api.upload_image(&[0x02], "image/png", "a.png", "doxc_TEST").await.unwrap();
        assert_ne!(t1, t2);
    }

    /// Rust 补充:下载的 Content-Type — 有头用头,缺省 image/png。
    #[tokio::test]
    async fn download_image_uses_content_type_with_png_default() {
        let with_header = Ok(TransportResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "image/jpeg".to_string())],
            body: b"JPGDATA".to_vec(),
        });
        let transport = MockTransport::new(vec![with_header]);
        let api = make_api(transport, StaticTokens("T"));
        let (bytes, mime) = api.download_image("IMG").await.unwrap();
        assert_eq!(bytes, b"JPGDATA".to_vec());
        assert_eq!(mime, "image/jpeg");

        let no_header = Ok(TransportResponse {
            status: 200,
            headers: Vec::new(),
            body: b"PNGDATA".to_vec(),
        });
        let transport = MockTransport::new(vec![no_header]);
        let api = make_api(transport, StaticTokens("T"));
        let (_, mime) = api.download_image("IMG").await.unwrap();
        assert_eq!(mime, "image/png");
    }

    /// Rust 补充:下载路径的 401 也走一次静默刷新(旁路管线独立
    /// 实现的重试语义)。
    #[tokio::test]
    async fn download_image_401_refreshes_once() {
        let transport = MockTransport::new(vec![
            resp(401, "unauthorized"),
            Ok(TransportResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "image/png".to_string())],
                body: b"PNG".to_vec(),
            }),
        ]);
        let tokens = RotatingTokens::old_new();
        let api = FeishuHttpApi::with_parts(
            "https://open.feishu.cn",
            transport.clone(),
            tokens.clone(),
            CountingSleeper::new(),
            FeishuBackoffPolicy::IMMEDIATE,
            std::sync::Arc::new(crate::feishu::api::InMemoryFeishuImageCache::default()),
        );
        let (bytes, _) = api.download_image("IMG").await.unwrap();
        assert_eq!(bytes, b"PNG".to_vec());
        assert_eq!(tokens.refreshes(), 1);
        assert_eq!(transport.request_count(), 2);
    }

    /// Rust 补充:段式插入走 encode_descendant_body_at(index),
    /// 体里带显式 index。
    #[tokio::test]
    async fn insert_children_at_carries_explicit_index() {
        let transport = MockTransport::new(vec![ok(r#"{"code":0}"#)]);
        let api = make_api(transport.clone(), StaticTokens("T"));
        let blocks = crate::feishu::converter::to_feishu_blocks("- 列表项\n");

        api.insert_children_at("doc_i", "blk_root", 3, &blocks).await.unwrap();

        let recorded = transport.recorded();
        assert!(recorded[0]
            .url
            .ends_with("/documents/doc_i/blocks/blk_root/descendant"));
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(body["index"], 3);
    }

    // MARK: - 凭据模型补充

    /// Rust 补充:60s 提前量边界(credential 模型在 credentials.rs,
    /// 顺带在这里锁语义)。
    #[test]
    fn credentials_expiry_skew_boundary() {
        let credentials = crate::feishu::credentials::FeishuCredentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1000.0,
            tenant_key: None,
        };
        // now + 60 == expires_at → 已过期(临界视作过期)。
        assert!(credentials.is_expired(940.0));
        // now + 60 < expires_at → 有效。
        assert!(!credentials.is_expired(939.9));
        assert!(!credentials.is_expired_with_skew(900.0, 0.0));
        assert!(credentials.is_expired_with_skew(1000.0, 0.0));
    }

    /// InMemoryCredentialStore 往返(oauth 测试依赖它,这里锁语义)。
    #[test]
    fn in_memory_store_round_trip() {
        let store = InMemoryCredentialStore::new();
        assert_eq!(store.load().unwrap(), None);
        let credentials = crate::feishu::credentials::FeishuCredentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1.0,
            tenant_key: Some("t".into()),
        };
        store.save(&credentials).unwrap();
        assert_eq!(store.load().unwrap(), Some(credentials));
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    /// 编译期证明 ReqwestTransport 满足传输抽象(不真正发请求)。
    #[test]
    fn reqwest_transport_implements_trait() {
        fn assert_transport<T: FeishuTransport>() {}
        assert_transport::<ReqwestTransport>();
        fn assert_sleeper<S: BackoffSleeper>() {}
        assert_sleeper::<TokioSleeper>();
    }
}
