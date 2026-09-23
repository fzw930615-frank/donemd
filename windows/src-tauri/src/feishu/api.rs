//! `FeishuAPIClient.swift` 前半的移植 — API 面与纯构造层。
//!
//! 本文件零 I/O:`FeishuApi` trait(协调器/F3 依赖的抽象)、
//! `FeishuApiError` 错误枚举、退避策略、请求构造(`PreparedRequest`
//! 风格,镜像 `ai/clients.rs` 的既有惯例)、multipart 字节流拼接、
//! 响应信封分类与解析。reqwest 实现与重试/刷新编排见
//! [`super::http_client`]。
//!
//! 错误码事实(注释逐条对齐 Swift 侧实机验证记录):
//! - 99991663 = token invalid → 一次静默刷新重试后仍失败才升级
//!   `Unauthorized`;
//! - 99991664 = 资源无权限(403 族);
//! - 99991679 = **应用** OAuth scope 不足,线上是 HTTP 400(不是
//!   403!),须在 badRequest 兜底前截获,否则用户看到的是「请求体
//!   畸形」这种误导性文案 —— 唯一解法是去开放平台控制台补 scope;
//! - 页块 PATCH 带 `text_element_style` 会被 1770001 拒收(#57
//!   step4,2026-05-30 真机),正文块的 99992402 契约恰相反。

use crate::feishu::block::FeishuBlock;
use crate::feishu::encoder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

/// Token 端点宿主 —— 所有 docx + drive 调用都在这下面(authorize
/// URL 在 `accounts.feishu.cn`,归 OAuth 客户端管)。
pub const DEFAULT_BASE_URL: &str = "https://open.feishu.cn";

/// 飞书 OpenAPI 业务错误码(判据见模块头注释)。
pub const CODE_TOKEN_INVALID: i64 = 99991663;
pub const CODE_FORBIDDEN: i64 = 99991664;
pub const CODE_SCOPE_INSUFFICIENT: i64 = 99991679;

/// 高层 docx OpenAPI 面 — Done.md 双向同步所需(F2 各端点注释对齐
/// Swift protocol 文档)。协调器(F3)以此抽象注入 `MockApi`。
pub trait FeishuApi: Send + Sync {
    /// 读整篇 docx 的块 + 当前 revision。revision 是飞书给每次成功
    /// 写入盖的单调版本号;拉取协调器存进
    /// `feishu.last_pulled_revision`,推送预检(F3)靠它发现「飞书
    /// 侧已领先于本地」。分页聚合在实现内(page_size=500)。
    async fn pull_document(&self, document_id: &str) -> Result<(Vec<FeishuBlock>, i64), FeishuApiError>;
    /// 用 `blocks` 整体覆盖 docx 正文。飞书没有 markdown 覆写端点,
    /// 唯一现实写路径是 docx 块 API 的 delete-then-create 编排(实
    /// 现内部:拉现有根子块 → 范围删 → 一次 POST descendants)。
    async fn push_document(&self, document_id: &str, blocks: &[FeishuBlock]) -> Result<(), FeishuApiError>;
    /// 范围删除 `[start_index, end_index)` 的子块 — 段式推送(#57)
    /// 清单个非占位段而不伤两侧占位块。协调器负责从后往前删。
    async fn delete_children_range(
        &self,
        document_id: &str,
        parent_block_id: &str,
        start_index: usize,
        end_index: usize,
    ) -> Result<(), FeishuApiError>;
    /// 在 `parent_block_id` 下 `index` 处插入块(只发页块的
    /// descendants;`index = -1` 追加)。空段(page-only)为 no-op —
    /// 两个占位块相邻时本地正文恰好为空。
    async fn insert_children_at(
        &self,
        document_id: &str,
        parent_block_id: &str,
        index: i64,
        blocks: &[FeishuBlock],
    ) -> Result<(), FeishuApiError>;
    /// 新建 docx(可挂父文件夹),返回 document_id。
    async fn create_document(&self, title: &str, parent_token: Option<&str>) -> Result<String, FeishuApiError>;
    /// 改文档标题 = 改页块文本(页块 block_id 等于 document_id,
    /// PATCH 路径折叠成 `/documents/{id}/blocks/{id}`)。已绑定文档
    /// 的首个 H1 变更走这里。
    async fn update_document_title(&self, document_id: &str, title: &str) -> Result<(), FeishuApiError>;
    /// 上传图片返回 `image_token`。`document_id` 用于
    /// `parent_node` + `extra.drive_route_token` — docx_image 上传
    /// 缺任一即 403/1061004(2026-05-30 真机确认)。
    async fn upload_image(
        &self,
        data: &[u8],
        mime_type: &str,
        file_name: &str,
        document_id: &str,
    ) -> Result<String, FeishuApiError>;
    /// 按 token 下载飞书 drive 媒体(图片/文件)。拉取侧用它把
    /// `feishu://image/<token>` 落成字节,返回 (bytes, mime)。
    async fn download_image(&self, token: &str) -> Result<(Vec<u8>, String), FeishuApiError>;
    /// wiki 节点 token → 其包裹的对象(docx/sheet/…)。wiki 页只是
    /// 壳,真正驱动拉取管线的是 `obj_token`。scope `wiki:wiki`。
    async fn resolve_wiki_node(&self, token: &str) -> Result<WikiNodeResolution, FeishuApiError>;
    /// 单 GET 只读当前 `revision_id` — v2-9b 推送预检用。部分租户
    /// 把它序列化成 String,实现须双容忍。
    async fn get_document_revision(&self, document_id: &str) -> Result<i64, FeishuApiError>;
}

/// `wiki/v2/spaces/get_node` 的产物 — 只带导入命令真正用到的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiNodeResolution {
    /// 被包裹对象的 token(docx 场景即 `doxc_…`)。
    pub obj_token: String,
    /// `docx` / `sheet` / `bitable` / … 原样字符串。Done.md 只会拉
    /// `docx`,其他类型应弹「暂不支持」。
    pub obj_type: String,
    /// wiki 树显示名(不一定等于 docx 内标题;飞书可返回空)。
    pub title: Option<String>,
}

/// 协调器解码并上浮的域错误 — 每种飞书 HTTP 失败各归一格,没有
/// `genericFailure` 兜底,逼着每条新错误路径想清楚重试策略与用户
/// 文案(文案映射在 F4 命令层)。
#[derive(Debug, Clone, PartialEq)]
pub enum FeishuApiError {
    /// 401 / 99991663。客户端已试过一次静默刷新;协调器应路由到
    /// 重新登录。
    Unauthorized,
    /// 403 / 99991664 — 用户对该资源无权限。
    Forbidden { message: Option<String> },
    /// 99991679 — access_token 有效但应用缺该端点的 OAuth scope
    /// (线上是 HTTP 400)。携带飞书原文(常列出所需 scope 名)供
    /// 对话框展示。
    ScopeInsufficient { detail: Option<String> },
    /// 404 — 文档/文件夹/wiki 不存在或不可见。携带资源 id 供日志。
    NotFound { resource: String },
    /// 429 — 自动重试耗尽后仍被限流。
    RateLimited,
    /// 其余 4xx — 通常是 400 + 飞书码指明哪个参数错了。原样透传
    /// status/code/message。
    BadRequest { http_status: u16, code: Option<i64>, message: Option<String> },
    /// 5xx — 重试耗尽后的服务端错误。
    ServerError { http_status: u16, code: Option<i64>, message: Option<String> },
    /// 传输层抛异常 — DNS / TLS / 断网。
    NetworkUnreachable(String),
    /// 响应 JSON 与预期形状不符 — 应报 bug,用户无法自愈。
    DecodeFailed(String),
}

impl std::fmt::Display for FeishuApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeishuApiError::Unauthorized => write!(f, "unauthorized"),
            FeishuApiError::Forbidden { message } => {
                write!(f, "forbidden: {}", message.as_deref().unwrap_or(""))
            }
            FeishuApiError::ScopeInsufficient { detail } => {
                write!(f, "scope insufficient: {}", detail.as_deref().unwrap_or(""))
            }
            FeishuApiError::NotFound { resource } => write!(f, "not found: {resource}"),
            FeishuApiError::RateLimited => write!(f, "rate limited"),
            FeishuApiError::BadRequest { http_status, code, message } => write!(
                f,
                "bad request {http_status} (code {code:?}): {}",
                message.as_deref().unwrap_or("")
            ),
            FeishuApiError::ServerError { http_status, code, message } => write!(
                f,
                "server error {http_status} (code {code:?}): {}",
                message.as_deref().unwrap_or("")
            ),
            FeishuApiError::NetworkUnreachable(detail) => write!(f, "network unreachable: {detail}"),
            FeishuApiError::DecodeFailed(detail) => write!(f, "decode failed: {detail}"),
        }
    }
}

impl std::error::Error for FeishuApiError {}

/// hash → `image_token` 内存缓存 — 同图重推不重传。进程生命周期
/// 足够(Done.md 典型图集 <100);持久化变体可经 trait 接入。
pub trait FeishuImageCache: Send + Sync {
    fn token_for_sha256(&self, hash: &str) -> Option<String>;
    fn remember_token(&self, token: &str, sha256: &str);
}

/// `InMemoryFeishuImageCache` — v2 Slice 5 默认实现。
#[derive(Default)]
pub struct InMemoryFeishuImageCache {
    map: Mutex<HashMap<String, String>>,
}

impl FeishuImageCache for InMemoryFeishuImageCache {
    fn token_for_sha256(&self, hash: &str) -> Option<String> {
        self.map.lock().unwrap().get(hash).cloned()
    }
    fn remember_token(&self, token: &str, sha256: &str) {
        self.map.lock().unwrap().insert(sha256.to_string(), token.to_string());
    }
}

/// 429/5xx 重试的退避表 — 结构体暴露,测试可注零延迟表。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeishuBackoffPolicy {
    pub initial_delay: f64,
    pub multiplier: f64,
    pub max_delay: f64,
    pub max_attempts: usize,
    pub jitter: f64,
}

impl FeishuBackoffPolicy {
    /// 生产表:1s → 8s ×2,4 次,jitter 0.5s。
    pub const PRODUCTION: FeishuBackoffPolicy = FeishuBackoffPolicy {
        initial_delay: 1.0,
        multiplier: 2.0,
        max_delay: 8.0,
        max_attempts: 4,
        jitter: 0.5,
    };

    /// 测试表:零延迟(每次重试仍走 sleeper,便于计数)。
    pub const IMMEDIATE: FeishuBackoffPolicy = FeishuBackoffPolicy {
        initial_delay: 0.0,
        multiplier: 1.0,
        max_delay: 0.0,
        max_attempts: 4,
        jitter: 0.0,
    };

    /// `attempt` 从 0 起(首次失败后的第一次重试)。指数递增封顶
    /// `max_delay`;`noise` 是调用方注入的 [0, jitter) 抖动(生产
    /// 取 [`jitter_noise`],测试传 0)—— 拆成参数是因为 Swift 的
    /// `Double.random` 在纯函数里没法断言。
    pub fn delay_for_attempt(&self, attempt: usize, noise: f64) -> f64 {
        let raw = self
            .max_delay
            .min(self.initial_delay * self.multiplier.powi(attempt as i32));
        if self.jitter <= 0.0 {
            return raw;
        }
        raw + noise.clamp(0.0, self.jitter)
    }
}

/// 抖动噪声 [0, jitter):以系统时钟纳秒做伪随机源 — 只求打散齐步
/// 重试,不是密码学随机。
pub fn jitter_noise(jitter: f64) -> f64 {
    if jitter <= 0.0 {
        return 0.0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    jitter * (f64::from(nanos) / f64::from(u32::MAX))
}

// MARK: - 请求构造(纯函数,PreparedRequest 风格)

/// 一个就绪待发的 HTTP 请求(纯数据 — 不经网络即可单测)。
/// `body` 为原始字节:JSON 端点装 UTF-8 JSON,multipart 端点装
/// 拼好的字节流。
pub struct PreparedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// `FeishuHTTPAPIClient.buildRequest` — 拼 URL(手工 query 编码)、
/// 挂 `Authorization: Bearer`、可选 Content-Type、恒带
/// `Accept: application/json`。Swift 侧特意绕开
/// `appendingPathComponent`(它会给字面 `/` 做百分号编码),Rust
/// 对等做法:直接字符串拼接 base + path。
pub fn build_request(
    base_url: &str,
    method: &str,
    path: &str,
    query: &[(&str, String)],
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
    token: &str,
) -> PreparedRequest {
    let base = base_url.trim_end_matches('/');
    let mut url = format!("{base}{path}");
    if !query.is_empty() {
        let joined = query
            .iter()
            .map(|(k, v)| format!("{}={}", k, encode_query_component(v)))
            .collect::<Vec<_>>()
            .join("&");
        url.push('?');
        url.push_str(&joined);
    }
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    if let Some(ct) = content_type {
        headers.push(("Content-Type".to_string(), ct.to_string()));
    }
    PreparedRequest {
        method: method.to_string(),
        url,
        headers,
        body: body.unwrap_or_default(),
    }
}

/// query 值的百分号编码(unreserved = `A-Za-z0-9-_.~`)。page_token
/// 是 base64 变体,可能带 `+` `/` `=`,必须编码。oauth 模块拼
/// authorize URL 也走这里(scope 里的 `:` 与空格)。
pub(crate) fn encode_query_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// GET `…/documents/{id}/blocks?page_size=500[&page_token=…]` —
/// 500 是飞书声明的块列表每页上限。
pub fn pull_blocks_request(
    base_url: &str,
    token: &str,
    document_id: &str,
    page_token: Option<&str>,
) -> PreparedRequest {
    let mut query = vec![("page_size", "500".to_string())];
    if let Some(pt) = page_token {
        query.push(("page_token", pt.to_string()));
    }
    build_request(
        base_url,
        "GET",
        &format!("/open-apis/docx/v1/documents/{document_id}/blocks"),
        &query,
        None,
        None,
        token,
    )
}

/// GET `…/documents/{id}` — 块列表不带 revision,得单读文档元数据。
pub fn document_meta_request(base_url: &str, token: &str, document_id: &str) -> PreparedRequest {
    build_request(
        base_url,
        "GET",
        &format!("/open-apis/docx/v1/documents/{document_id}"),
        &[],
        None,
        None,
        token,
    )
}

/// DELETE `…/blocks/{parent}/children/batch_delete`,体
/// `{start_index, end_index}`。
pub fn batch_delete_request(
    base_url: &str,
    token: &str,
    document_id: &str,
    parent_block_id: &str,
    start_index: usize,
    end_index: usize,
) -> PreparedRequest {
    let body = json!({ "start_index": start_index, "end_index": end_index });
    build_request(
        base_url,
        "DELETE",
        &format!(
            "/open-apis/docx/v1/documents/{document_id}/blocks/{parent_block_id}/children/batch_delete"
        ),
        &[],
        Some("application/json; charset=utf-8"),
        Some(serde_json::to_vec(&body).unwrap_or_default()),
        token,
    )
}

/// POST `…/blocks/{parent}/descendant`,体为
/// [`encoder::encode_descendant_body_at`] 的产物
/// `{index, children_id, descendants}`。
pub fn insert_descendants_request(
    base_url: &str,
    token: &str,
    document_id: &str,
    parent_block_id: &str,
    body: &Value,
) -> PreparedRequest {
    build_request(
        base_url,
        "POST",
        &format!(
            "/open-apis/docx/v1/documents/{document_id}/blocks/{parent_block_id}/descendant"
        ),
        &[],
        Some("application/json; charset=utf-8"),
        Some(serde_json::to_vec(body).unwrap_or_default()),
        token,
    )
}

/// POST `/open-apis/docx/v1/documents`,体 `{title[, folder_token]}` —
/// parent 为 `None` 时省略 `folder_token`(不能发 null)。
pub fn create_document_request(
    base_url: &str,
    token: &str,
    title: &str,
    parent_token: Option<&str>,
) -> PreparedRequest {
    let mut body = json!({ "title": title });
    if let Some(parent) = parent_token {
        body["folder_token"] = json!(parent);
    }
    build_request(
        base_url,
        "POST",
        "/open-apis/docx/v1/documents",
        &[],
        Some("application/json; charset=utf-8"),
        Some(serde_json::to_vec(&body).unwrap_or_default()),
        token,
    )
}

/// PATCH `…/documents/{id}/blocks/{id}`(页块 block_id ==
/// document_id,路径折叠)。体只带单个 `text_run.content` —
/// **不含** `text_element_style`:页块 PATCH 的校验器对它整块拒收
/// (1770001,布尔全 false 也拒;2026-05-30 真机 + feishu-mcp-pro
/// renameDoc 交叉核对)。正文块的 99992402 契约(全布尔显式)恰
/// 相反,那条在 encoder 里。
pub fn update_title_request(base_url: &str, token: &str, document_id: &str, title: &str) -> PreparedRequest {
    let body = json!({
        "update_text_elements": {
            "elements": [ { "text_run": { "content": title } } ],
        },
    });
    build_request(
        base_url,
        "PATCH",
        &format!("/open-apis/docx/v1/documents/{document_id}/blocks/{document_id}"),
        &[],
        Some("application/json; charset=utf-8"),
        Some(serde_json::to_vec(&body).unwrap_or_default()),
        token,
    )
}

/// GET `/open-apis/wiki/v2/spaces/get_node?token=…`(scope
/// `wiki:wiki`;wiki 节点原子,无分页)。
pub fn wiki_get_node_request(base_url: &str, token: &str, wiki_token: &str) -> PreparedRequest {
    build_request(
        base_url,
        "GET",
        "/open-apis/wiki/v2/spaces/get_node",
        &[("token", wiki_token.to_string())],
        None,
        None,
        token,
    )
}

/// GET `/open-apis/drive/v1/medias/{token}/download` — 与上传同属
/// drive 媒体面,方向相反。
pub fn download_image_request(base_url: &str, token: &str, media_token: &str) -> PreparedRequest {
    build_request(
        base_url,
        "GET",
        &format!("/open-apis/drive/v1/medias/{media_token}/download"),
        &[],
        None,
        None,
        token,
    )
}

/// multipart 体的字段段(供 [`build_upload_multipart`] 与测试拼装)。
fn multipart_field(boundary: &str, name: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

/// 手工拼 `upload_all` 的 multipart 字节流(reqwest 生态同样没有
/// 内建 multipart 助手,Swift 侧也是手拼)。字段序对齐 Swift:
/// file_name → parent_type=docx_image → parent_node → extra → size →
/// file。`parent_node` + `extra.drive_route_token` 缺一即 403/
/// 1061004(2026-05-30 真机)。
pub fn build_upload_multipart(
    data: &[u8],
    mime_type: &str,
    file_name: &str,
    document_id: &str,
    boundary: &str,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&multipart_field(boundary, "file_name", file_name));
    body.extend_from_slice(&multipart_field(boundary, "parent_type", "docx_image"));
    body.extend_from_slice(&multipart_field(boundary, "parent_node", document_id));
    let extra = format!("{{\"drive_route_token\":\"{document_id}\"}}");
    body.extend_from_slice(&multipart_field(boundary, "extra", &extra));
    body.extend_from_slice(&multipart_field(boundary, "size", &data.len().to_string()));
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// POST `/open-apis/drive/v1/medias/upload_all`(multipart 全量上
/// 传;boundary 由调用方生成)。
pub fn upload_image_request(
    base_url: &str,
    token: &str,
    data: &[u8],
    mime_type: &str,
    file_name: &str,
    document_id: &str,
    boundary: &str,
) -> PreparedRequest {
    let body = build_upload_multipart(data, mime_type, file_name, document_id, boundary);
    build_request(
        base_url,
        "POST",
        "/open-apis/drive/v1/medias/upload_all",
        &[],
        Some(&format!("multipart/form-data; boundary={boundary}")),
        Some(body),
        token,
    )
}

/// `sha256_hex` — 图片去重缓存的键。
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// MARK: - 响应分类与解析(纯函数)

/// `sendWithRetryData` 错误分支的分类结果 — 编排(刷新/重试)在
/// http_client,判据在这里。分支顺序与 Swift 一致:unauthorized →
/// scope → forbidden → notFound → 可重试 → OK 兜底。
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseVerdict {
    /// 2xx 且信封 code == 0 / 缺省。
    Ok,
    /// 401 或 99991663 → 静默刷新后重试一次。
    Unauthorized,
    /// 99991679(应用 scope 不足,detail 截断至 600 字)。
    ScopeInsufficient { detail: Option<String> },
    /// 403 或 99991664。
    Forbidden { message: Option<String> },
    /// 404。
    NotFound,
    /// 429 / 5xx → 退避重试;`rate_limited` 区分 429(耗尽后错误
    /// 文案不同)。
    Retryable { rate_limited: bool },
    /// 其余一律致命(不重试):4xx → BadRequest,否则 ServerError
    /// —— 含「2xx 但信封 code ≠ 0」的畸形响应。
    Fatal(FeishuApiError),
}

/// 信封 `{code, msg}` 预解 — 错误分支要在整包有效前就能读到飞书
/// 的判定。体不是 JSON 时返回 (None, None)。
pub fn envelope_metadata(body: &[u8]) -> (Option<i64>, Option<String>) {
    serde_json::from_slice::<Value>(body)
        .ok()
        .map(|v| {
            (
                v.get("code").and_then(Value::as_i64),
                v.get("msg").and_then(Value::as_str).map(String::from),
            )
        })
        .unwrap_or((None, None))
}

/// 600 字截断 — 飞书 scope 不足的 msg 是一面墙的 scope 名,对话框
/// 摘要比原文可读(Swift `msg.prefix(600) + "…"`)。http_client 的
/// 下载旁路管线同样引用。
pub(crate) fn truncate_detail(msg: &str) -> Option<String> {
    if msg.chars().count() > 600 {
        Some(msg.chars().take(600).collect::<String>() + "…")
    } else {
        Some(msg.to_string())
    }
}

/// 响应分类(错误全表的单一真源)。`status` 传非 HTTP 值(如 0)
/// 时落到 Fatal/ServerError — 与 Swift `?? -1` 行为一致。
pub fn classify_response(status: u16, body: &[u8]) -> ResponseVerdict {
    let (code, msg) = envelope_metadata(body);
    // 401 / 401 等价码 → 一次刷新重试。
    if status == 401 || code == Some(CODE_TOKEN_INVALID) {
        return ResponseVerdict::Unauthorized;
    }
    // 99991679 必须先于 403/404/兜底截获:它线上是 400,掉进
    // badRequest 会读成「请求体畸形」。
    if code == Some(CODE_SCOPE_INSUFFICIENT) {
        return ResponseVerdict::ScopeInsufficient {
            detail: msg.as_deref().and_then(truncate_detail),
        };
    }
    if status == 403 || code == Some(CODE_FORBIDDEN) {
        return ResponseVerdict::Forbidden { message: msg };
    }
    if status == 404 {
        return ResponseVerdict::NotFound;
    }
    // 429 + 5xx → 退避循环,耗尽才升级错误。
    if status == 429 || (500..=599).contains(&status) {
        return ResponseVerdict::Retryable { rate_limited: status == 429 };
    }
    let envelope_ok = code == Some(0) || code.is_none();
    if (200..=299).contains(&status) && envelope_ok {
        return ResponseVerdict::Ok;
    }
    // 4xx 是客户端侧(参数格式问题),与 5xx 分开 — 曾经混进
    // serverError,把无效参数谎报成瞬时故障误导用户重试。
    if (400..=499).contains(&status) {
        return ResponseVerdict::Fatal(FeishuApiError::BadRequest {
            http_status: status,
            code,
            message: msg,
        });
    }
    ResponseVerdict::Fatal(FeishuApiError::ServerError {
        http_status: status,
        code,
        message: msg,
    })
}

/// 传输层产出 — status + 响应头(下载要读 Content-Type)+ 原始体。
pub struct TransportResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl TransportResponse {
    /// 大小写不敏取响应头(下载路径的 Content-Type)。
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }
}

/// 请求执行抽象 — 生产是 reqwest(`http_client::ReqwestTransport`),
/// 测试注入响应队列 Mock。泛型静态分发(AFIT)。
pub trait FeishuTransport: Send + Sync {
    async fn execute(&self, request: PreparedRequest) -> Result<TransportResponse, FeishuApiError>;
}

/// access token 供给 — 每次请求前取一次;401 后强制刷新。
/// `force_refresh` 默认回落 `access_token`(OAuth 客户端自己对新鲜
/// token 短路,与 Swift `onUnauthorized ?? tokenProvider` 同语义)。
pub trait AccessTokenProvider: Send + Sync {
    async fn access_token(&self) -> Result<String, FeishuApiError>;
    async fn force_refresh(&self) -> Result<String, FeishuApiError> {
        self.access_token().await
    }
}

/// 退避睡眠 — 生产是 tokio 定时;测试记录秒数并立即返回。
pub trait BackoffSleeper: Send + Sync {
    async fn sleep(&self, seconds: f64);
}

// MARK: - 响应体解析(纯函数)

/// 块列表单页:items 经 [`encoder::decode_block_envelope`] 逐块解码
/// (载荷按 block_type 动态分派,不是静态 Decodable)。
pub struct BlocksPage {
    pub blocks: Vec<FeishuBlock>,
    pub has_more: bool,
    pub page_token: Option<String>,
}

pub fn parse_blocks_page(body: &[u8]) -> Result<BlocksPage, FeishuApiError> {
    let json: Value = serde_json::from_slice(body)
        .map_err(|e| FeishuApiError::DecodeFailed(format!("blocks response: {e}")))?;
    if !json.is_object() {
        return Err(FeishuApiError::DecodeFailed(
            "blocks response root is not a JSON object".into(),
        ));
    }
    let data = json.get("data").cloned().unwrap_or_else(|| json!({}));
    let items = data.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut blocks = Vec::with_capacity(items.len());
    for item in &items {
        let block = encoder::decode_block_envelope(item)
            .map_err(|e| FeishuApiError::DecodeFailed(e.to_string()))?;
        blocks.push(block);
    }
    let has_more = data.get("has_more").and_then(Value::as_bool) == Some(true);
    let page_token = if has_more {
        data.get("page_token").and_then(Value::as_str).map(String::from)
    } else {
        None
    };
    Ok(BlocksPage { blocks, has_more, page_token })
}

/// 文档元数据的 `revision_id` — Int / String 双容忍(部分租户按
/// 字符串序列化),缺字段报 DecodeFailed。
pub fn parse_revision(body: &[u8], document_id: &str) -> Result<i64, FeishuApiError> {
    let json: Value = serde_json::from_slice(body)
        .map_err(|e| FeishuApiError::DecodeFailed(format!("document meta response: {e}")))?;
    let document = json
        .get("data")
        .and_then(|d| d.get("document"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(rev) = document.get("revision_id").and_then(Value::as_i64) {
        return Ok(rev);
    }
    if let Some(rev) = document
        .get("revision_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
    {
        return Ok(rev);
    }
    Err(FeishuApiError::DecodeFailed(format!(
        "document meta response missing revision_id ({document_id})"
    )))
}

/// createDocument 的 `data.document.document_id`。
pub fn parse_created_document_id(body: &[u8]) -> Result<String, FeishuApiError> {
    let json: Value = serde_json::from_slice(body)
        .map_err(|e| FeishuApiError::DecodeFailed(format!("create response: {e}")))?;
    json.get("data")
        .and_then(|d| d.get("document"))
        .and_then(|d| d.get("document_id"))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| FeishuApiError::DecodeFailed("create response missing document_id".into()))
}

/// upload_all 的 `data.file_token`。
pub fn parse_uploaded_file_token(body: &[u8]) -> Result<String, FeishuApiError> {
    let json: Value = serde_json::from_slice(body)
        .map_err(|e| FeishuApiError::DecodeFailed(format!("upload response: {e}")))?;
    json.get("data")
        .and_then(|d| d.get("file_token"))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| FeishuApiError::DecodeFailed("upload response missing file_token".into()))
}

/// wiki get_node 的 `data.node.{obj_token, obj_type[, title]}` —
/// obj_token / obj_type 缺失或为空按 DecodeFailed 处理(实际几乎
/// 不发生:wiki 节点总包着点什么)。
pub fn parse_wiki_node(body: &[u8], wiki_token: &str) -> Result<WikiNodeResolution, FeishuApiError> {
    let json: Value = serde_json::from_slice(body)
        .map_err(|e| FeishuApiError::DecodeFailed(format!("wiki get_node response: {e}")))?;
    let node = json
        .get("data")
        .and_then(|d| d.get("node"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let obj_token = node.get("obj_token").and_then(Value::as_str).unwrap_or("");
    let obj_type = node.get("obj_type").and_then(Value::as_str).unwrap_or("");
    if obj_token.is_empty() || obj_type.is_empty() {
        return Err(FeishuApiError::DecodeFailed(format!(
            "wiki get_node response missing obj_token / obj_type ({wiki_token})"
        )));
    }
    Ok(WikiNodeResolution {
        obj_token: obj_token.to_string(),
        obj_type: obj_type.to_string(),
        title: node.get("title").and_then(Value::as_str).map(String::from),
    })
}

// MARK: - 测试(FeishuAPIClientTests.swift 的纯函数面;网络编排
// 组在 http_client.rs)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::block::Payload;
    use serde_json::json;

    fn json_body(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    fn header<'a>(req: &'a PreparedRequest, name: &str) -> Option<&'a str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    // MARK: - build_request / query 编码

    #[test]
    fn build_request_sets_bearer_and_accept() {
        let req = build_request(
            DEFAULT_BASE_URL,
            "GET",
            "/open-apis/docx/v1/documents/d1",
            &[],
            None,
            None,
            "TOK123",
        );
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/docx/v1/documents/d1"
        );
        assert_eq!(header(&req, "Authorization"), Some("Bearer TOK123"));
        assert_eq!(header(&req, "Accept"), Some("application/json"));
        assert_eq!(header(&req, "Content-Type"), None);
        assert!(req.body.is_empty());
    }

    #[test]
    fn build_request_adds_content_type_and_trims_base_slash() {
        // base 尾斜杠不产生双斜杠(Swift baseURL 声明带不带斜杠都行)。
        let req = build_request(
            "https://open.feishu.cn/",
            "POST",
            "/p",
            &[],
            Some("application/json; charset=utf-8"),
            Some(b"{}".to_vec()),
            "T",
        );
        assert_eq!(req.url, "https://open.feishu.cn/p");
        assert_eq!(
            header(&req, "Content-Type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(req.body, b"{}".to_vec());
    }

    #[test]
    fn encode_query_component_percent_encodes_reserved() {
        assert_eq!(encode_query_component("abcXYZ09-_.~"), "abcXYZ09-_.~");
        // page_token 是 base64 变体,`+` `/` `=` 必须编码。
        assert_eq!(encode_query_component("a+b/c=d"), "a%2Bb%2Fc%3Dd");
        // 非 ASCII 按 UTF-8 字节逐个转义。
        assert_eq!(encode_query_component("中"), "%E4%B8%AD");
    }

    // MARK: - 端点请求构造

    #[test]
    fn pull_blocks_request_carries_page_size_and_optional_token() {
        let req = pull_blocks_request(DEFAULT_BASE_URL, "T", "doxcnA", None);
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/docx/v1/documents/doxcnA/blocks?page_size=500"
        );
        let req = pull_blocks_request(DEFAULT_BASE_URL, "T", "doxcnA", Some("Pt+1/2="));
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/docx/v1/documents/doxcnA/blocks\
             ?page_size=500&page_token=Pt%2B1%2F2%3D"
        );
    }

    #[test]
    fn document_meta_request_targets_document_endpoint() {
        let req = document_meta_request(DEFAULT_BASE_URL, "T", "doxcnRev");
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/docx/v1/documents/doxcnRev"
        );
    }

    #[test]
    fn batch_delete_request_body_is_start_end_range() {
        let req = batch_delete_request(DEFAULT_BASE_URL, "T", "doxcnA", "doxcnA", 0, 1);
        assert_eq!(req.method, "DELETE");
        assert!(req
            .url
            .ends_with("/documents/doxcnA/blocks/doxcnA/children/batch_delete"));
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body, json!({ "start_index": 0, "end_index": 1 }));
        assert_eq!(
            header(&req, "Content-Type"),
            Some("application/json; charset=utf-8")
        );
    }

    #[test]
    fn insert_descendants_request_passes_encoded_body() {
        let body = json!({ "index": -1, "children_id": ["b1"], "descendants": [] });
        let req = insert_descendants_request(DEFAULT_BASE_URL, "T", "doxcnA", "doxcnA", &body);
        assert_eq!(req.method, "POST");
        assert!(req.url.ends_with("/documents/doxcnA/blocks/doxcnA/descendant"));
        assert_eq!(serde_json::from_slice::<Value>(&req.body).unwrap(), body);
    }

    #[test]
    fn create_document_request_omits_folder_token_when_absent() {
        let req = create_document_request(DEFAULT_BASE_URL, "T", "新建文档", None);
        assert_eq!(req.method, "POST");
        assert!(req.url.ends_with("/open-apis/docx/v1/documents"));
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body, json!({ "title": "新建文档" }));
    }

    #[test]
    fn create_document_request_includes_folder_token_when_present() {
        let req = create_document_request(DEFAULT_BASE_URL, "T", "t", Some("fldcnX"));
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body, json!({ "title": "t", "folder_token": "fldcnX" }));
    }

    #[test]
    fn update_title_request_omits_text_element_style() {
        // 页块 PATCH 带 text_element_style 被 1770001 拒收(#57 step4)。
        let req = update_title_request(DEFAULT_BASE_URL, "T", "doxcnA", "新标题");
        assert_eq!(req.method, "PATCH");
        // 页块 block_id == document_id,路径折叠。
        assert!(req.url.ends_with("/documents/doxcnA/blocks/doxcnA"));
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let elements = body["update_text_elements"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0]["text_run"]["content"], "新标题");
        assert!(elements[0]["text_run"].get("text_element_style").is_none());
    }

    #[test]
    fn wiki_get_node_request_targets_spaces_endpoint() {
        let req = wiki_get_node_request(DEFAULT_BASE_URL, "T", "wikcnABC");
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/wiki/v2/spaces/get_node?token=wikcnABC"
        );
    }

    #[test]
    fn download_image_request_targets_media_download() {
        let req = download_image_request(DEFAULT_BASE_URL, "T", "IMGTOKEN");
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/drive/v1/medias/IMGTOKEN/download"
        );
    }

    // MARK: - multipart / 摘要

    #[test]
    fn build_upload_multipart_is_byte_exact() {
        let body = build_upload_multipart(b"PNGDATA", "image/png", "shot.png", "doxcnABC", "BOUND");
        let expected = concat!(
            "--BOUND\r\n",
            "Content-Disposition: form-data; name=\"file_name\"\r\n\r\nshot.png\r\n",
            "--BOUND\r\n",
            "Content-Disposition: form-data; name=\"parent_type\"\r\n\r\ndocx_image\r\n",
            "--BOUND\r\n",
            "Content-Disposition: form-data; name=\"parent_node\"\r\n\r\ndoxcnABC\r\n",
            "--BOUND\r\n",
            "Content-Disposition: form-data; name=\"extra\"\r\n\r\n",
            "{\"drive_route_token\":\"doxcnABC\"}\r\n",
            "--BOUND\r\n",
            "Content-Disposition: form-data; name=\"size\"\r\n\r\n7\r\n",
            "--BOUND\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"shot.png\"\r\n",
            "Content-Type: image/png\r\n\r\nPNGDATA\r\n",
            "--BOUND--\r\n",
        );
        assert_eq!(body, expected.as_bytes());
    }

    #[test]
    fn upload_image_request_sets_multipart_content_type() {
        let req = upload_image_request(
            DEFAULT_BASE_URL,
            "T",
            b"xx",
            "image/png",
            "a.png",
            "doxcnA",
            "BNDRY",
        );
        assert_eq!(req.method, "POST");
        assert!(req.url.ends_with("/open-apis/drive/v1/medias/upload_all"));
        assert_eq!(
            header(&req, "Content-Type"),
            Some("multipart/form-data; boundary=BNDRY")
        );
        let text = String::from_utf8(req.body).unwrap();
        assert!(text.starts_with("--BNDRY\r\n"));
        assert!(text.ends_with("--BNDRY--\r\n"));
    }

    #[test]
    fn sha256_hex_known_vectors() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // MARK: - 退避

    #[test]
    fn production_backoff_sequence_clamps_at_max() {
        let policy = FeishuBackoffPolicy::PRODUCTION;
        // 1s → 2s → 4s → 8s(封顶)→ 8s;噪声 0 时严格等于表值。
        let delays: Vec<f64> = (0..5).map(|a| policy.delay_for_attempt(a, 0.0)).collect();
        assert_eq!(delays, vec![1.0, 2.0, 4.0, 8.0, 8.0]);
    }

    #[test]
    fn immediate_backoff_is_all_zero() {
        let policy = FeishuBackoffPolicy::IMMEDIATE;
        for attempt in 0..4 {
            assert_eq!(policy.delay_for_attempt(attempt, 0.0), 0.0);
        }
    }

    #[test]
    fn backoff_noise_is_clamped_to_jitter() {
        let policy = FeishuBackoffPolicy::PRODUCTION;
        // 噪声超上限被钳到 jitter(0.5s)。
        assert_eq!(policy.delay_for_attempt(0, 99.0), 1.5);
        // jitter ≤ 0 的表忽略噪声(测试注零表仍零延迟)。
        assert_eq!(FeishuBackoffPolicy::IMMEDIATE.delay_for_attempt(0, 0.7), 0.0);
    }

    #[test]
    fn jitter_noise_stays_within_range() {
        for _ in 0..32 {
            let n = jitter_noise(0.5);
            assert!((0.0..0.5).contains(&n), "noise out of range: {n}");
        }
    }

    // MARK: - classify_response(错误全表;分支顺序与 Swift 一致)

    #[test]
    fn classify_401_is_unauthorized() {
        assert_eq!(classify_response(401, b"{}"), ResponseVerdict::Unauthorized);
    }

    #[test]
    fn classify_token_invalid_code_is_unauthorized_even_on_200() {
        let body = json_body(json!({ "code": 99991663, "msg": "invalid access token" }));
        assert_eq!(classify_response(200, &body), ResponseVerdict::Unauthorized);
    }

    #[test]
    fn classify_429_with_token_invalid_code_still_unauthorized() {
        // 分支顺序:unauthorized 先于可重试。
        let body = json_body(json!({ "code": 99991663 }));
        assert_eq!(classify_response(429, &body), ResponseVerdict::Unauthorized);
    }

    #[test]
    fn classify_scope_insufficient_wins_over_bad_request() {
        // 99991679 线上是 HTTP 400 — 必须先截获,不能落进 badRequest
        // 把 scope 缺失读成「请求体畸形」。
        let body = json_body(json!({ "code": 99991679, "msg": "缺少 docx:document scope" }));
        assert_eq!(
            classify_response(400, &body),
            ResponseVerdict::ScopeInsufficient {
                detail: Some("缺少 docx:document scope".into())
            }
        );
    }

    #[test]
    fn classify_scope_detail_truncates_at_600_chars() {
        let body = json_body(json!({ "code": 99991679, "msg": "x".repeat(700) }));
        match classify_response(400, &body) {
            ResponseVerdict::ScopeInsufficient { detail } => {
                let detail = detail.unwrap();
                // 600 字符 + 省略号。
                assert_eq!(detail.chars().count(), 601);
                assert!(detail.ends_with('…'));
            }
            other => panic!("expected ScopeInsufficient, got {other:?}"),
        }
    }

    #[test]
    fn classify_403_and_forbidden_code_are_forbidden() {
        let body = json_body(json!({ "code": 99991664, "msg": "no permission" }));
        assert_eq!(
            classify_response(200, &body),
            ResponseVerdict::Forbidden {
                message: Some("no permission".into())
            }
        );
        assert_eq!(
            classify_response(403, b"{}"),
            ResponseVerdict::Forbidden { message: None }
        );
    }

    #[test]
    fn classify_404_is_not_found() {
        assert_eq!(classify_response(404, b"{}"), ResponseVerdict::NotFound);
    }

    #[test]
    fn classify_429_and_5xx_are_retryable() {
        assert_eq!(
            classify_response(429, b"{}"),
            ResponseVerdict::Retryable { rate_limited: true }
        );
        assert_eq!(
            classify_response(500, b"{}"),
            ResponseVerdict::Retryable { rate_limited: false }
        );
        assert_eq!(
            classify_response(503, b"{}"),
            ResponseVerdict::Retryable { rate_limited: false }
        );
    }

    #[test]
    fn classify_2xx_with_zero_or_missing_code_is_ok() {
        assert_eq!(
            classify_response(200, &json_body(json!({ "code": 0 }))),
            ResponseVerdict::Ok
        );
        // 下载端点等 2xx 无信封响应(空体)同样 OK。
        assert_eq!(classify_response(204, b""), ResponseVerdict::Ok);
    }

    #[test]
    fn classify_2xx_with_nonzero_code_is_server_error() {
        // 畸形响应:HTTP 200 但信封 code ≠ 0 — 归服务端错误。
        let body = json_body(json!({ "code": 5, "msg": "weird" }));
        assert_eq!(
            classify_response(200, &body),
            ResponseVerdict::Fatal(FeishuApiError::ServerError {
                http_status: 200,
                code: Some(5),
                message: Some("weird".into()),
            })
        );
    }

    #[test]
    fn classify_400_is_bad_request_not_server_error() {
        let body = json_body(json!({ "code": 1770001, "msg": "param error" }));
        assert_eq!(
            classify_response(400, &body),
            ResponseVerdict::Fatal(FeishuApiError::BadRequest {
                http_status: 400,
                code: Some(1770001),
                message: Some("param error".into()),
            })
        );
    }

    #[test]
    fn classify_malformed_body_still_classifies_by_status() {
        // 非 JSON 体退化为 (None, None) 信封,分类只看 status。
        assert_eq!(classify_response(200, b"<html>oops"), ResponseVerdict::Ok);
        assert_eq!(
            classify_response(400, b"<html>oops"),
            ResponseVerdict::Fatal(FeishuApiError::BadRequest {
                http_status: 400,
                code: None,
                message: None,
            })
        );
    }

    #[test]
    fn envelope_metadata_reads_code_and_msg() {
        let body = json_body(json!({ "code": 99991679, "msg": "scope" }));
        assert_eq!(envelope_metadata(&body), (Some(99991679), Some("scope".into())));
        assert_eq!(envelope_metadata(b"not json"), (None, None));
    }

    // MARK: - 响应体解析

    #[test]
    fn parse_blocks_page_decodes_items_and_pagination() {
        let body = json_body(json!({
            "code": 0,
            "data": {
                "items": [
                    { "block_id": "PAGE", "block_type": 1, "children": ["B1"] },
                    { "block_id": "B1", "block_type": 2,
                      "text": { "elements": [ { "text_run": { "content": "hi" } } ] } }
                ],
                "has_more": true,
                "page_token": "PAGE2"
            }
        }));
        let page = parse_blocks_page(&body).unwrap();
        assert_eq!(page.blocks.len(), 2);
        assert_eq!(page.blocks[0].block_id, "PAGE");
        assert!(matches!(page.blocks[0].payload, Payload::Page(_)));
        assert_eq!(page.blocks[0].children.as_deref(), Some(&["B1".to_string()][..]));
        assert!(matches!(page.blocks[1].payload, Payload::Text(_)));
        assert!(page.has_more);
        assert_eq!(page.page_token.as_deref(), Some("PAGE2"));

        // 末页:has_more=false 不再携带 page_token。
        let tail = json_body(json!({
            "code": 0,
            "data": { "items": [ { "block_id": "B2", "block_type": 2 } ], "has_more": false }
        }));
        let page = parse_blocks_page(&tail).unwrap();
        assert_eq!(page.blocks.len(), 1);
        assert!(!page.has_more);
        assert_eq!(page.page_token, None);
    }

    #[test]
    fn parse_blocks_page_malformed_block_is_decode_failed() {
        let body = json_body(json!({
            "code": 0,
            "data": { "items": [ { "block_type": 2 } ] }
        }));
        assert!(matches!(parse_blocks_page(&body), Err(FeishuApiError::DecodeFailed(_))));
    }

    #[test]
    fn parse_blocks_page_tolerates_missing_data() {
        let body = json_body(json!({ "code": 0 }));
        let page = parse_blocks_page(&body).unwrap();
        assert!(page.blocks.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn parse_revision_accepts_int_and_string() {
        let int_body =
            json_body(json!({ "data": { "document": { "document_id": "d", "revision_id": 42 } } }));
        assert_eq!(parse_revision(&int_body, "d").unwrap(), 42);
        let str_body =
            json_body(json!({ "data": { "document": { "document_id": "d", "revision_id": "43" } } }));
        assert_eq!(parse_revision(&str_body, "d").unwrap(), 43);
    }

    #[test]
    fn parse_revision_missing_field_is_decode_failed() {
        let body = json_body(json!({ "data": { "document": { "document_id": "d" } } }));
        assert!(matches!(parse_revision(&body, "d"), Err(FeishuApiError::DecodeFailed(_))));
    }

    #[test]
    fn parse_created_document_id_reads_nested_document() {
        let body = json_body(json!({ "data": { "document": { "document_id": "doxcnNEW" } } }));
        assert_eq!(parse_created_document_id(&body).unwrap(), "doxcnNEW");
        let missing = json_body(json!({ "data": {} }));
        assert!(matches!(
            parse_created_document_id(&missing),
            Err(FeishuApiError::DecodeFailed(_))
        ));
    }

    #[test]
    fn parse_uploaded_file_token_reads_data() {
        let body = json_body(json!({ "data": { "file_token": "IMGTOKEN" } }));
        assert_eq!(parse_uploaded_file_token(&body).unwrap(), "IMGTOKEN");
        let missing = json_body(json!({ "data": {} }));
        assert!(matches!(
            parse_uploaded_file_token(&missing),
            Err(FeishuApiError::DecodeFailed(_))
        ));
    }

    #[test]
    fn parse_wiki_node_reads_obj_fields() {
        let body = json_body(json!({
            "data": { "node": { "obj_token": "doxcnOBJ", "obj_type": "docx", "title": "Q2 计划" } }
        }));
        let node = parse_wiki_node(&body, "wikcnA").unwrap();
        assert_eq!(
            node,
            WikiNodeResolution {
                obj_token: "doxcnOBJ".into(),
                obj_type: "docx".into(),
                title: Some("Q2 计划".into()),
            }
        );
        // title 可缺省。
        let no_title =
            json_body(json!({ "data": { "node": { "obj_token": "t", "obj_type": "sheet" } } }));
        assert_eq!(parse_wiki_node(&no_title, "w").unwrap().title, None);
        // obj_token 缺失 → DecodeFailed。
        let broken = json_body(json!({ "data": { "node": { "obj_type": "docx" } } }));
        assert!(matches!(parse_wiki_node(&broken, "w"), Err(FeishuApiError::DecodeFailed(_))));
    }

    // MARK: - 缓存 / trait 默认实现

    #[test]
    fn in_memory_image_cache_round_trips_by_hash() {
        let cache = InMemoryFeishuImageCache::default();
        let hash = sha256_hex(b"same-bytes");
        assert_eq!(cache.token_for_sha256(&hash), None);
        cache.remember_token("IMG1", &hash);
        assert_eq!(cache.token_for_sha256(&hash), Some("IMG1".into()));
        // 不同 hash 互不影响。
        assert_eq!(cache.token_for_sha256(sha256_hex(b"other").as_str()), None);
    }

    #[test]
    fn transport_response_header_lookup_is_case_insensitive() {
        let resp = TransportResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "image/png".into()),
                ("x-request-id".into(), "42".into()),
            ],
            body: Vec::new(),
        };
        assert_eq!(resp.header("content-type"), Some("image/png"));
        assert_eq!(resp.header("CONTENT-TYPE"), Some("image/png"));
        assert_eq!(resp.header("X-Request-Id"), Some("42"));
        assert_eq!(resp.header("missing"), None);
    }

    struct StaticToken;

    impl AccessTokenProvider for StaticToken {
        async fn access_token(&self) -> Result<String, FeishuApiError> {
            Ok("TOK".into())
        }
    }

    #[tokio::test]
    async fn access_token_provider_default_refresh_falls_back() {
        let provider = StaticToken;
        assert_eq!(provider.access_token().await.unwrap(), "TOK");
        // 默认 force_refresh 回落 access_token — OAuth 客户端自己对
        // 新鲜 token 短路(与 Swift `onUnauthorized ?? tokenProvider`
        // 同语义)。
        assert_eq!(provider.force_refresh().await.unwrap(), "TOK");
    }

    #[test]
    fn api_error_display_carries_status_and_message() {
        let err = FeishuApiError::BadRequest {
            http_status: 400,
            code: Some(1770001),
            message: Some("param error".into()),
        };
        assert_eq!(
            err.to_string(),
            "bad request 400 (code Some(1770001)): param error"
        );
        assert_eq!(FeishuApiError::Unauthorized.to_string(), "unauthorized");
    }
}
