//! `FeishuOAuthClient.swift` 的移植 — 登录 / 刷新 / 登出全生命周期。
//!
//! wire 事实(2026-05-23 用户拉开放平台文档核实):
//! - authorize:`https://accounts.feishu.cn/open-apis/authen/v1/authorize`
//!   (GET,浏览器),query 带 `app_id` / `redirect_uri` / `state` /
//!   `scope`(空格连接);
//! - token:`https://open.feishu.cn/open-apis/authen/v2/oauth/token`
//!   (POST `application/json; charset=utf-8`),响应是**扁平**的 —
//!   字段全在顶层(`access_token` / `refresh_token` / `expires_in` /
//!   …),哨兵 `code` 字段成功时为 `0`。
//!
//! CSRF:`state` 是每次 login 新铸的 128bit 随机 hex,回环接收器
//! 内部校验后才把 URL 交给调用方(见 oauth_receiver 的
//! StateMismatch 路径)。

use crate::feishu::api::{encode_query_component, FeishuTransport, PreparedRequest};
use crate::feishu::app_config::FeishuAppConfig;
use crate::feishu::credentials::{CredentialStore, FeishuCredentials};
use crate::feishu::oauth_receiver::{url_query_param, ReceiverError};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// authorize 端点(浏览器域,不是 open.feishu.cn)。
pub const AUTHORIZE_ENDPOINT: &str = "https://accounts.feishu.cn/open-apis/authen/v1/authorize";
/// token 交换端点。
pub const TOKEN_ENDPOINT: &str = "https://open.feishu.cn/open-apis/authen/v2/oauth/token";

/// 发给飞书的 scope 全集 — 必须先在开放平台「权限管理」页开通,
/// 否则 authorize 页直接拒。
///
/// `offline_access` 负责发 refresh_token,少了它每 2 小时就得重新
/// 登录。**token 的授权面是用户授权那一刻此列表的快照** — 只在
/// 权限管理页勾选新 scope 不会赋予已有 token,必须出现在这里且
/// 用户登出重登。2026-05-30 真机教训:`docs:document.media:download`
/// 在权限管理页勾了但此列表没带,/drive/v1/medias/{token}/download
/// 一直 99991679。
///
/// 覆盖:docx:document(块推拉)、wiki:wiki(节点解析,兼容保留)、
/// docs:document.media:download / upload(图片两阶段)、
/// offline_access(refresh token)。
pub const DEFAULT_SCOPES: [&str; 5] = [
    "docx:document",
    "wiki:wiki",
    "docs:document.media:download",
    "docs:document.media:upload",
    "offline_access",
];

/// OAuth 流程错误(Swift `OAuthError`)。
#[derive(Debug, Clone, PartialEq)]
pub enum OAuthError {
    /// 应用凭证未配置 — UI 应路由到引导页(贴 App ID / Secret),
    /// 而不是尝试登录。
    NotConfigured,
    /// 无已存凭据(从未登录 / 已登出)。
    NotAuthenticated,
    /// HTTP 非 2xx,或飞书扁平响应 `code ≠ 0`。code/msg 原样透传,
    /// UI 能展示飞书自己的错误串。
    TokenExchangeFailed {
        http_status: u16,
        code: Option<i64>,
        message: Option<String>,
    },
    /// JSON 形状与预期不符 — wire 格式变了,报 bug。
    DecodeFailed(String),
    /// 拉起系统浏览器失败。
    BrowserOpenFailed(String),
    /// token 端点传输失败(断网等)。
    NetworkUnreachable(String),
    /// 回环接收器错误(超时 / state 不匹配 / 端口占用)。
    Callback(ReceiverError),
    /// 凭据存储读写失败(keyring)。
    Store(String),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthError::NotConfigured => write!(f, "飞书应用凭证未配置"),
            OAuthError::NotAuthenticated => write!(f, "尚未登录飞书"),
            OAuthError::TokenExchangeFailed { http_status, code, message } => write!(
                f,
                "token 交换失败(http {http_status}, code {code:?}):{}",
                message.as_deref().unwrap_or("")
            ),
            OAuthError::DecodeFailed(detail) => write!(f, "响应解析失败:{detail}"),
            OAuthError::BrowserOpenFailed(detail) => write!(f, "打开浏览器失败:{detail}"),
            OAuthError::NetworkUnreachable(detail) => write!(f, "网络不可达:{detail}"),
            OAuthError::Callback(err) => write!(f, "{err}"),
            OAuthError::Store(detail) => write!(f, "凭据存储失败:{detail}"),
        }
    }
}

impl std::error::Error for OAuthError {}

// MARK: - 纯请求构造 / 响应解析

/// authorize URL(query 值逐个百分号编码,scope 空格连接)。
pub fn build_authorize_url(config: &FeishuAppConfig, state: &str, scopes: &[&str]) -> String {
    let query = [
        ("app_id", config.client_id.as_str()),
        ("redirect_uri", config.redirect_uri.as_str()),
        ("state", state),
        ("scope", &scopes.join(" ")),
    ];
    let joined = query
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_query_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_ENDPOINT}?{joined}")
}

/// token 端点请求的手工构造 — **不带** Authorization 头(它不是
/// Bearer 保护的 docx 端点,build_request 的 Bearer 语义不适用)。
fn token_request(body: Value) -> PreparedRequest {
    PreparedRequest {
        method: "POST".to_string(),
        url: TOKEN_ENDPOINT.to_string(),
        headers: vec![
            ("Content-Type".to_string(), "application/json; charset=utf-8".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

/// 授权码交换请求体。
pub fn exchange_request(config: &FeishuAppConfig, code: &str) -> PreparedRequest {
    token_request(json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "client_secret": config.client_secret,
        "code": code,
        "redirect_uri": config.redirect_uri,
    }))
}

/// 刷新请求体 — **重申 scope**,把刷新后的 token 收窄到用户最初
/// 同意的那一组。
pub fn refresh_request(config: &FeishuAppConfig, refresh_token: &str, scopes: &[&str]) -> PreparedRequest {
    token_request(json!({
        "grant_type": "refresh_token",
        "client_id": config.client_id,
        "client_secret": config.client_secret,
        "refresh_token": refresh_token,
        "scope": scopes.join(" "),
    }))
}

/// token 端点扁平响应 → 凭据(纯函数;`now_secs` 是注入的当前
/// unix 秒,`expires_at = now + expires_in`)。
///
/// 非 JSON 体 → TokenExchangeFailed(code: None, "non-JSON
/// response");非 2xx 或 `code ≠ 0` → TokenExchangeFailed 原样透传;
/// 缺 `access_token` / `expires_in` → DecodeFailed。`refresh_token`
/// 缺省存空串 — 只有授权面含 `offline_access` 时飞书才回,缺它不
/// 算硬失败(代价是 ~2h 后重登),后续刷新会以干净的
/// TokenExchangeFailed 收场。
pub fn parse_token_response(
    http_status: u16,
    body: &[u8],
    now_secs: f64,
) -> Result<FeishuCredentials, OAuthError> {
    let payload: Value = serde_json::from_slice(body).map_err(|_| OAuthError::TokenExchangeFailed {
        http_status,
        code: None,
        message: Some("non-JSON response".into()),
    })?;
    let code = payload.get("code").and_then(Value::as_i64);
    let msg = payload.get("msg").and_then(Value::as_str).map(String::from);
    if !(200..=299).contains(&http_status) || code != Some(0) {
        return Err(OAuthError::TokenExchangeFailed {
            http_status,
            code,
            message: msg,
        });
    }
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let expires_in = payload.get("expires_in").and_then(Value::as_i64);
    let (access_token, expires_in) = match (access_token, expires_in) {
        (Some(a), Some(e)) => (a, e),
        _ => {
            return Err(OAuthError::DecodeFailed(
                "token response missing access_token / expires_in".into(),
            ))
        }
    };
    Ok(FeishuCredentials {
        access_token: access_token.to_string(),
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        expires_at: now_secs + expires_in as f64,
        tenant_key: payload.get("tenant_key").and_then(Value::as_str).map(String::from),
    })
}

/// 128bit 随机 hex — 单次用 CSRF token 足够,长度又不至于把重定向
/// URL 顶破常见长度上限。熵源 = uuid v4(RFC 4122,OS CSPRNG)。
pub fn make_random_state() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

// MARK: - 抽象(测试替身挂点)

/// 回调接收器抽象(Swift `FeishuOAuthCallbackReceiver` 协议)。
///
/// `await_callback` 返回**手装箱 future** — AFIT 方法没有 dyn 兼容
/// 面,而客户端要把接收器存成 `Arc<dyn …>` 单一装配(F2.3 manager)。
/// 就地 `Box::pin`,不引 async-trait:每条回调一次装箱,登录流程
/// 毫秒级,无谓优化点不存在。
pub trait OAuthCallbackReceiver: Send + Sync {
    /// 嵌进 authorize URL 的重定向 URI — 须与开放平台注册逐字一致。
    fn redirect_uri(&self) -> String;
    /// 等浏览器打到重定向 URI,返回完整 URL(state 已在内部校验)。
    fn await_callback<'a>(
        &'a self,
        expected_state: &'a str,
        timeout: Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, ReceiverError>> + Send + 'a>,
    >;
}

/// 生产接收器 — 委托给 oauth_receiver 的固有方法(方法解析固有
/// 优先,`self.redirect_uri()` 落到固有 impl,无递归)。
impl OAuthCallbackReceiver for crate::feishu::oauth_receiver::FeishuLoopbackReceiver {
    fn redirect_uri(&self) -> String {
        self.redirect_uri()
    }
    fn await_callback<'a>(
        &'a self,
        expected_state: &'a str,
        timeout: Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, ReceiverError>> + Send + 'a>,
    > {
        Box::pin(self.await_callback(expected_state, timeout))
    }
}

/// 浏览器拉起抽象(Swift `FeishuAuthorizeURLOpener`)。
pub trait AuthorizeUrlOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// 生产 opener — 系统默认浏览器。
pub struct SystemBrowserOpener;

impl AuthorizeUrlOpener for SystemBrowserOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        open::that(url).map_err(|e| e.to_string())
    }
}

// MARK: - 客户端

/// OAuth 客户端 — 全依赖可注入(传输 / 存储 / 接收器 / opener /
/// 时钟 / state 生成器),测试不碰真浏览器、真端口、真 keyring。
pub struct FeishuOAuthClient<T: FeishuTransport> {
    config: FeishuAppConfig,
    transport: T,
    store: Arc<dyn CredentialStore>,
    receiver: Arc<dyn OAuthCallbackReceiver>,
    opener: Arc<dyn AuthorizeUrlOpener>,
    scopes: Vec<String>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
    state_generator: Arc<dyn Fn() -> String + Send + Sync>,
}

impl<T: FeishuTransport> FeishuOAuthClient<T> {
    pub fn new(
        config: FeishuAppConfig,
        transport: T,
        store: Arc<dyn CredentialStore>,
        receiver: Arc<dyn OAuthCallbackReceiver>,
        opener: Arc<dyn AuthorizeUrlOpener>,
    ) -> Self {
        Self::with_overrides(config, transport, store, receiver, opener, None, None, None)
    }

    /// 测试注入点:scopes / 时钟 / state 生成器(生产用默认)。
    #[allow(clippy::too_many_arguments)]
    pub fn with_overrides(
        config: FeishuAppConfig,
        transport: T,
        store: Arc<dyn CredentialStore>,
        receiver: Arc<dyn OAuthCallbackReceiver>,
        opener: Arc<dyn AuthorizeUrlOpener>,
        scopes: Option<Vec<String>>,
        now: Option<Arc<dyn Fn() -> f64 + Send + Sync>>,
        state_generator: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    ) -> Self {
        Self {
            config,
            transport,
            store,
            receiver,
            opener,
            scopes: scopes.unwrap_or_else(|| DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect()),
            now: now.unwrap_or_else(|| Arc::new(system_now_secs)),
            state_generator: state_generator.unwrap_or_else(|| Arc::new(make_random_state)),
        }
    }

    /// 完整授权码流程:铸 state → 拉浏览器 → 等回环重定向(接收器
    /// 内部校验 state)→ 交换 token → 存 keyring。返回新铸凭据
    /// (已持久化,下次启动直接取)。
    pub async fn login(&self, timeout: Duration) -> Result<FeishuCredentials, OAuthError> {
        let state = (self.state_generator)();
        let scopes: Vec<&str> = self.scopes.iter().map(String::as_str).collect();
        let authorize_url = build_authorize_url(&self.config, &state, &scopes);

        self.opener
            .open(&authorize_url)
            .map_err(OAuthError::BrowserOpenFailed)?;

        let callback_url = self
            .receiver
            .await_callback(&state, timeout)
            .await
            .map_err(OAuthError::Callback)?;

        // state 已由接收器校验过;这里只抠 code。飞书偶发不回 code
        // (配置侧错误页也会打到重定向),干净上报。
        let code = url_query_param(&callback_url, "code")
            .filter(|c| !c.is_empty())
            .ok_or_else(|| OAuthError::DecodeFailed("authorization callback missing 'code'".into()))?;

        let credentials = self.exchange(&code).await?;
        self.store
            .save(&credentials)
            .map_err(OAuthError::Store)?;
        Ok(credentials)
    }

    /// 取未过期的 access token,过期则刷新一次。无凭据 →
    /// NotAuthenticated(调用方路由到 login)。
    pub async fn refresh_if_needed(&self) -> Result<FeishuCredentials, OAuthError> {
        let stored = self
            .store
            .load()
            .map_err(OAuthError::Store)?
            .ok_or(OAuthError::NotAuthenticated)?;
        if !stored.is_expired((self.now)()) {
            return Ok(stored);
        }
        let refreshed = self.refresh(&stored.refresh_token).await?;
        self.store
            .save(&refreshed)
            .map_err(OAuthError::Store)?;
        Ok(refreshed)
    }

    /// 无视过期判定的强制刷新 — 401 后的静默重试腿用:服务端已拒,
    /// 本地「还没过期」的时钟判断作废,必须真换一枚。
    pub async fn force_refresh(&self) -> Result<FeishuCredentials, OAuthError> {
        let stored = self
            .store
            .load()
            .map_err(OAuthError::Store)?
            .ok_or(OAuthError::NotAuthenticated)?;
        let refreshed = self.refresh(&stored.refresh_token).await?;
        self.store
            .save(&refreshed)
            .map_err(OAuthError::Store)?;
        Ok(refreshed)
    }

    /// 尽力登出:无条件清本地存储。飞书有 revoke 端点但要租户级
    /// token(客户端没有),不从这里调。
    pub async fn logout(&self) -> Result<(), OAuthError> {
        self.store.clear().map_err(OAuthError::Store)
    }

    async fn exchange(&self, code: &str) -> Result<FeishuCredentials, OAuthError> {
        let response = self
            .transport
            .execute(exchange_request(&self.config, code))
            .await
            .map_err(|e| OAuthError::NetworkUnreachable(e.to_string()))?;
        parse_token_response(response.status, &response.body, (self.now)())
    }

    async fn refresh(&self, refresh_token: &str) -> Result<FeishuCredentials, OAuthError> {
        let scopes: Vec<&str> = self.scopes.iter().map(String::as_str).collect();
        let response = self
            .transport
            .execute(refresh_request(&self.config, refresh_token, &scopes))
            .await
            .map_err(|e| OAuthError::NetworkUnreachable(e.to_string()))?;
        parse_token_response(response.status, &response.body, (self.now)())
    }
}

/// 系统时钟(unix 秒)。
fn system_now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// MARK: - 测试(FeishuOAuthClientTests.swift,9 例)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::api::{FeishuApiError, TransportResponse};
    use crate::feishu::http_client::MockTransport;
    use crate::feishu::oauth_receiver::DEFAULT_PORT;
    use std::sync::Mutex;

    fn test_config() -> FeishuAppConfig {
        FeishuAppConfig::new(
            "cli_test123",
            "secret-test",
            "http://localhost:18127/oauth/callback",
        )
    }

    fn ok(body: &str) -> Result<TransportResponse, FeishuApiError> {
        Ok(TransportResponse {
            status: 200,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        })
    }

    fn raw(status: u16, body: &str) -> Result<TransportResponse, FeishuApiError> {
        Ok(TransportResponse {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        })
    }

    fn success_token_response(access_token: &str, refresh_token: &str, expires_in: i64) -> String {
        format!(
            r#"{{"code":0,"msg":"success","access_token":"{access_token}","refresh_token":"{refresh_token}","expires_in":{expires_in},"refresh_token_expires_in":604800,"scope":"docx:document offline_access","token_type":"Bearer","tenant_key":"ten_abc"}}"#
        )
    }

    /// 立即回传预设回调 URL 的假接收器(不绑端口)。
    struct StubReceiver {
        callback_url: String,
        last_expected_state: Mutex<Option<String>>,
    }

    impl StubReceiver {
        fn new(callback_url: &str) -> Self {
            Self {
                callback_url: callback_url.to_string(),
                last_expected_state: Mutex::new(None),
            }
        }
    }

    impl OAuthCallbackReceiver for StubReceiver {
        fn redirect_uri(&self) -> String {
            format!("http://localhost:{DEFAULT_PORT}/oauth/callback")
        }
        fn await_callback<'a>(
            &'a self,
            expected_state: &'a str,
            _timeout: Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, ReceiverError>> + Send + 'a>,
        > {
            *self.last_expected_state.lock().unwrap() = Some(expected_state.to_string());
            Box::pin(async move { Ok(self.callback_url.clone()) })
        }
    }

    /// 不该被碰到的路径(refresh / logout / URL 构造)用的哨兵。
    struct NeverReceiver;

    impl OAuthCallbackReceiver for NeverReceiver {
        fn redirect_uri(&self) -> String {
            format!("http://localhost:{DEFAULT_PORT}/oauth/callback")
        }
        fn await_callback<'a>(
            &'a self,
            _expected_state: &'a str,
            _timeout: Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, ReceiverError>> + Send + 'a>,
        > {
            Box::pin(async { panic!("receiver must not be invoked on this path") })
        }
    }

    /// 记录打开过的 URL。
    struct RecordingOpener {
        opened: Mutex<Vec<String>>,
    }

    impl AuthorizeUrlOpener for RecordingOpener {
        fn open(&self, url: &str) -> Result<(), String> {
            self.opened.lock().unwrap().push(url.to_string());
            Ok(())
        }
    }

    fn recording_opener() -> Arc<RecordingOpener> {
        Arc::new(RecordingOpener {
            opened: Mutex::new(Vec::new()),
        })
    }

    fn make_client(
        transport: MockTransport,
        store: Arc<dyn CredentialStore>,
        receiver: Arc<dyn OAuthCallbackReceiver>,
        opener: Arc<RecordingOpener>,
        now: Option<f64>,
        state: Option<&'static str>,
    ) -> FeishuOAuthClient<MockTransport> {
        let now_fn: Option<Arc<dyn Fn() -> f64 + Send + Sync>> =
            now.map(|fixed| Arc::new(move || fixed) as Arc<dyn Fn() -> f64 + Send + Sync>);
        let state_fn: Option<Arc<dyn Fn() -> String + Send + Sync>> = state
            .map(|fixed| Arc::new(move || fixed.to_string()) as Arc<dyn Fn() -> String + Send + Sync>);
        FeishuOAuthClient::with_overrides(
            test_config(),
            transport,
            store,
            receiver,
            opener as Arc<dyn AuthorizeUrlOpener>,
            None,
            now_fn,
            state_fn,
        )
    }

    #[tokio::test]
    async fn login_exchanges_code_and_persists_credentials() {
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        let receiver = Arc::new(StubReceiver::new(
            "http://localhost:18127/oauth/callback?code=auth_code_xyz&state=STATE_FIXED",
        ));
        let transport = MockTransport::new(vec![ok(&success_token_response(
            "access-abc", "refresh-def", 7200,
        ))]);
        let opener = Arc::new(RecordingOpener {
            opened: Mutex::new(Vec::new()),
        });
        let fixed_now = 1_700_000_000.0f64;
        let client = make_client(
            transport,
            store.clone(),
            receiver.clone(),
            opener.clone(),
            Some(fixed_now),
            Some("STATE_FIXED"),
        );

        let credentials = client.login(Duration::from_secs(5)).await.unwrap();

        assert_eq!(credentials.access_token, "access-abc");
        assert_eq!(credentials.refresh_token, "refresh-def");
        assert_eq!(credentials.expires_at, fixed_now + 7200.0);
        assert_eq!(credentials.tenant_key.as_deref(), Some("ten_abc"));
        assert_eq!(
            store.load().unwrap(),
            Some(credentials),
            "successful login must persist to the store"
        );
        // 客户端把生成的 state 交给接收器做 CSRF 校验。
        assert_eq!(
            *receiver.last_expected_state.lock().unwrap(),
            Some("STATE_FIXED".to_string())
        );
        // 浏览器只开一次,authorize URL 带全四参。
        let opened = opener.opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1);
        assert_eq!(
            opened[0],
            "https://accounts.feishu.cn/open-apis/authen/v1/authorize\
             ?app_id=cli_test123\
             &redirect_uri=http%3A%2F%2Flocalhost%3A18127%2Foauth%2Fcallback\
             &state=STATE_FIXED\
             &scope=docx%3Adocument%20wiki%3Awiki%20docs%3Adocument.media%3Adownload%20docs%3Adocument.media%3Aupload%20offline_access"
        );
    }

    #[tokio::test]
    async fn login_sends_expected_exchange_body() {
        // 独立用例锁定交换请求形状(方法 / URL / 头 / 体字段)。
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        let receiver = Arc::new(StubReceiver::new(
            "http://localhost:18127/oauth/callback?code=CODE42&state=S",
        ));
        let transport = MockTransport::new(vec![ok(&success_token_response("a", "r", 60))]);
        let opener = Arc::new(RecordingOpener {
            opened: Mutex::new(Vec::new()),
        });
        let client = make_client(
            transport.clone(),
            store,
            receiver,
            opener,
            Some(1.0),
            Some("S"),
        );

        client.login(Duration::from_secs(1)).await.unwrap();

        let recorded = transport.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].url, TOKEN_ENDPOINT);
        assert_eq!(
            recorded[0].header("Content-Type"),
            Some("application/json; charset=utf-8")
        );
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["client_id"], "cli_test123");
        assert_eq!(body["client_secret"], "secret-test");
        assert_eq!(body["code"], "CODE42");
        assert_eq!(body["redirect_uri"], "http://localhost:18127/oauth/callback");
    }

    #[tokio::test]
    async fn login_propagates_feishu_error_code() {
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        let receiver = Arc::new(StubReceiver::new(
            "http://localhost:18127/oauth/callback?code=bad&state=S",
        ));
        // 飞书错误信封:HTTP 200 但 code ≠ 0 — 必须按失败处理,不能
        // 把无 token 的凭据对象静默递出去。
        let transport = MockTransport::new(vec![raw(
            200,
            r#"{"code": 20007, "msg": "code invalid"}"#,
        )]);
        let client = make_client(
            transport,
            store.clone(),
            receiver,
            recording_opener(),
            None,
            Some("S"),
        );

        let err = client.login(Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(
            err,
            OAuthError::TokenExchangeFailed {
                http_status: 200,
                code: Some(20007),
                message: Some("code invalid".into()),
            }
        );
        assert_eq!(store.load().unwrap(), None, "failed login must not persist anything");
    }

    #[tokio::test]
    async fn login_rejects_callback_missing_code() {
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        // state 匹配(接收器侧已过),但飞书没回 code — 大概率配置
        // 侧错误页仍打到了重定向。干净上报。
        let receiver = Arc::new(StubReceiver::new("http://localhost:18127/oauth/callback?state=S"));
        let transport = MockTransport::new(vec![ok("")]);
        let client = make_client(
            transport,
            store,
            receiver,
            recording_opener(),
            None,
            Some("S"),
        );

        let err = client.login(Duration::from_secs(1)).await.unwrap_err();
        assert!(matches!(err, OAuthError::DecodeFailed(_)));
    }

    #[tokio::test]
    async fn refresh_if_needed_short_circuits_when_token_fresh() {
        let fixed_now = 1_700_000_000.0f64;
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        let fresh = FeishuCredentials {
            access_token: "still-good".into(),
            refresh_token: "rt".into(),
            expires_at: fixed_now + 3600.0,
            tenant_key: None,
        };
        store.save(&fresh).unwrap();

        // 空响应队列:任何 HTTP 调用都会 panic mock — 新鲜 token
        // 不得触发刷新请求。
        let transport = MockTransport::new(Vec::new());
        let client = make_client(
            transport,
            store.clone(),
            Arc::new(NeverReceiver),
            recording_opener(),
            Some(fixed_now),
            None,
        );

        let result = client.refresh_if_needed().await.unwrap();
        assert_eq!(result, fresh);
    }

    #[tokio::test]
    async fn refresh_if_needed_exchanges_and_persists_when_expired() {
        let fixed_now = 1_700_000_000.0f64;
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        let stale = FeishuCredentials {
            access_token: "old-token".into(),
            refresh_token: "old-refresh".into(),
            expires_at: fixed_now - 10.0,
            tenant_key: None,
        };
        store.save(&stale).unwrap();

        let transport = MockTransport::new(vec![ok(&success_token_response(
            "new-token", "new-refresh", 7200,
        ))]);
        let client = make_client(
            transport,
            store.clone(),
            Arc::new(NeverReceiver),
            recording_opener(),
            Some(fixed_now),
            None,
        );

        let refreshed = client.refresh_if_needed().await.unwrap();
        assert_eq!(refreshed.access_token, "new-token");
        assert_eq!(
            refreshed.refresh_token, "new-refresh",
            "refreshed credentials must use Feishu's NEW refresh token"
        );
        assert_eq!(store.load().unwrap(), Some(refreshed));
    }

    #[test]
    fn refresh_request_reasserts_scope() {
        // 刷新请求体:grant_type / refresh_token / scope 重申。
        let config = test_config();
        let request = refresh_request(&config, "old-refresh", &DEFAULT_SCOPES);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "old-refresh");
        assert_eq!(
            body["scope"],
            "docx:document wiki:wiki docs:document.media:download docs:document.media:upload offline_access"
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, TOKEN_ENDPOINT);
    }

    #[tokio::test]
    async fn refresh_if_needed_with_empty_store_throws_not_authenticated() {
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        let transport = MockTransport::new(Vec::new());
        let client = make_client(
            transport,
            store,
            Arc::new(NeverReceiver),
            recording_opener(),
            None,
            None,
        );

        let err = client.refresh_if_needed().await.unwrap_err();
        assert_eq!(err, OAuthError::NotAuthenticated);
    }

    #[tokio::test]
    async fn logout_clears_store() {
        let store = Arc::new(crate::feishu::credentials::InMemoryCredentialStore::new());
        store
            .save(&FeishuCredentials {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at: 100.0,
                tenant_key: None,
            })
            .unwrap();
        assert!(store.load().unwrap().is_some());
        let transport = MockTransport::new(Vec::new());
        let client = make_client(
            transport,
            store.clone(),
            Arc::new(NeverReceiver),
            recording_opener(),
            None,
            None,
        );

        client.logout().await.unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn build_authorize_url_encodes_query_params() {
        let url = build_authorize_url(&test_config(), "abc", &DEFAULT_SCOPES);
        assert_eq!(
            url,
            "https://accounts.feishu.cn/open-apis/authen/v1/authorize\
             ?app_id=cli_test123\
             &redirect_uri=http%3A%2F%2Flocalhost%3A18127%2Foauth%2Fcallback\
             &state=abc\
             &scope=docx%3Adocument%20wiki%3Awiki%20docs%3Adocument.media%3Adownload%20docs%3Adocument.media%3Aupload%20offline_access"
        );
    }

    #[test]
    fn random_state_looks_random() {
        let a = make_random_state();
        let b = make_random_state();
        // 16 字节随机 → 32 个 hex 字符。
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "state generator must not return constants");
    }

    #[test]
    fn parse_token_response_maps_error_shapes() {
        // 非 JSON 体。
        assert_eq!(
            parse_token_response(200, b"not json", 0.0),
            Err(OAuthError::TokenExchangeFailed {
                http_status: 200,
                code: None,
                message: Some("non-JSON response".into()),
            })
        );
        // HTTP 200 但 code ≠ 0。
        assert_eq!(
            parse_token_response(200, br#"{"code": 20007, "msg": "code invalid"}"#, 0.0),
            Err(OAuthError::TokenExchangeFailed {
                http_status: 200,
                code: Some(20007),
                message: Some("code invalid".into()),
            })
        );
        // 成功体缺 refresh_token → 空串(offline_access 未授权)。
        let body = br#"{"code":0,"access_token":"a","expires_in":100}"#;
        let credentials = parse_token_response(200, body, 10.0).unwrap();
        assert_eq!(credentials.refresh_token, "");
        assert_eq!(credentials.expires_at, 110.0);
        // 缺 expires_in → DecodeFailed。
        let broken = br#"{"code":0,"access_token":"a"}"#;
        assert!(matches!(
            parse_token_response(200, broken, 0.0),
            Err(OAuthError::DecodeFailed(_))
        ));
    }
}
