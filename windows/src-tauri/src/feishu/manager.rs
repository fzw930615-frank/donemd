//! `Settings/FeishuSyncManager.swift` 的移植 — 设置页与推送/拉取命令
//! 共用的常驻门面。
//!
//! 三件事:
//! - **AuthState 状态机**:notConfigured(链上无凭证)→ loggedOut
//!   (有凭证无令牌)→ loggedIn(令牌在库),login/logout/save 换轨;
//! - **非机密镜像** `feishu-state.json`:启动渲染只读镜像不碰凭据
//!   管理器(Mac 侧 Keychain 读会弹访问授权框;Windows CredMan 虽
//!   静默,镜像仍省一次 IO 且给主窗徽标即时状态)。镜像只存粗状态
//!   + 租户标签 + 非机密的 client_id/redirect_uri — secret 永不落盘
//!   明文文件;
//! - **令牌适配器** `OAuthTokenProvider`:FeishuHttpApi 的
//!   AccessTokenProvider 面 — access = 过期检查 + 刷新,force =
//!   401 强刷(F3/F4 推拉命令消费)。
//!
//! Swift 侧的 SyncRootStore/Scanner(同步根目录)按计划后置到 F5,
//! 不在本文件。

use crate::feishu::api::{
    AccessTokenProvider, FeishuApiError, FeishuTransport, DEFAULT_BASE_URL,
};
use crate::feishu::app_config::{
    AppConfigStore, FeishuAppConfig, KeyringAppConfigStore, SaveAppConfigError,
};
use crate::feishu::credentials::{CredentialStore, FeishuCredentials, KeyringCredentialStore};
use crate::feishu::http_client::{FeishuHttpApi, ReqwestTransport, TokioSleeper};
use crate::feishu::oauth::{FeishuOAuthClient, OAuthError, SystemBrowserOpener};
use crate::feishu::oauth_receiver::FeishuLoopbackReceiver;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 登录等待浏览器的时限(对齐 Swift 的 300s)。
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// 认证状态(Swift `AuthState`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// 应用凭证链全空 — UI 引导去填 App ID / Secret。
    NotConfigured,
    /// 有凭证、无令牌(从未登录 / 已登出)。
    LoggedOut,
    /// 令牌在库;tenant_key 单租户自建应用可能缺省。
    LoggedIn { tenant_key: Option<String> },
}

/// 镜像文件名(`%APPDATA%\com.shampoo.donemd\feishu-state.json`)。
const MIRROR_FILE: &str = "feishu-state.json";

/// 镜像体 — 只有非机密字段。「已登录」快照 + UI 渲染要用的两个
/// 标识符;secret / access_token / refresh_token 永不进这里。
#[derive(Debug, Default, Serialize, Deserialize)]
struct FeishuStateMirror {
    /// "loggedIn" | "loggedOut" | "notConfigured"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenant_key: Option<String>,
    /// 当前生效配置的 App ID(非机密标识符,设置页徽标用)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    redirect_uri: Option<String>,
}

struct ManagerInner {
    auth_state: AuthState,
    is_logging_in: bool,
    last_error: Option<String>,
}

/// 同步门面。全方法 `&self`(内部互斥),登录等长操作不占锁跨
/// await — AppState 直接持有,tauri 命令并发安全。
pub struct FeishuSyncManager {
    credential_store: Arc<dyn CredentialStore>,
    app_config_store: Arc<dyn AppConfigStore>,
    /// 镜像路径(测试注入临时目录)。
    state_path: PathBuf,
    /// 环境变量源(测试注入;生产取真进程环境)。
    environment: HashMap<String, String>,
    /// 手写配置文件源(测试注入;生产 default_config_path)。
    config_file_path: PathBuf,
    inner: Mutex<ManagerInner>,
}

impl FeishuSyncManager {
    /// 生产装配。
    pub fn new() -> Self {
        Self::with_parts(
            Arc::new(KeyringCredentialStore::new()),
            Arc::new(KeyringAppConfigStore::new()),
            default_state_path(),
        )
    }

    /// 全件注入 — 测试驱动(内存存储 + 临时目录 + 假环境)。
    pub fn with_parts(
        credential_store: Arc<dyn CredentialStore>,
        app_config_store: Arc<dyn AppConfigStore>,
        state_path: PathBuf,
    ) -> Self {
        Self {
            credential_store,
            app_config_store,
            state_path,
            environment: std::env::vars().collect(),
            config_file_path: FeishuAppConfig::default_config_path(),
            inner: Mutex::new(ManagerInner {
                auth_state: AuthState::NotConfigured,
                is_logging_in: false,
                last_error: None,
            }),
        }
    }

    /// 启动渲染:**只读镜像**,不碰凭据管理器。镜像缺失/损坏 →
    /// notConfigured(新装/首次动作前的保守态),首次 login/push/
    /// pull 会读真值并回写镜像。
    pub fn boot(&self) {
        let state = match self.read_mirror() {
            Some(mirror) => match mirror.auth_state.as_deref() {
                Some("loggedIn") => AuthState::LoggedIn {
                    tenant_key: mirror.tenant_key.filter(|k| !k.is_empty()),
                },
                Some("loggedOut") => AuthState::LoggedOut,
                _ => AuthState::NotConfigured,
            },
            None => AuthState::NotConfigured,
        };
        self.inner.lock().unwrap().auth_state = state;
    }

    /// 当前认证状态快照。
    pub fn auth_state(&self) -> AuthState {
        self.inner.lock().unwrap().auth_state.clone()
    }

    pub fn is_logging_in(&self) -> bool {
        self.inner.lock().unwrap().is_logging_in
    }

    pub fn last_error(&self) -> Option<String> {
        self.inner.lock().unwrap().last_error.clone()
    }

    /// 重算认证状态并回写镜像:配置链空 → notConfigured;令牌在库
    /// → loggedIn(tenant);否则 loggedOut。凭据读失败按 loggedOut
    /// 处理(锁屏 / 服务不可用 — 下次登录会把真实错误带出来)。
    pub fn refresh_auth_state(&self) {
        let config = self.current_app_config();
        let state = match config {
            None => AuthState::NotConfigured,
            Some(_) => match self.credential_store.load() {
                Ok(Some(credentials)) => AuthState::LoggedIn {
                    tenant_key: credentials.tenant_key,
                },
                Ok(None) => AuthState::LoggedOut,
                Err(detail) => {
                    eprintln!("[feishu] credential store load failed: {detail}");
                    AuthState::LoggedOut
                }
            },
        };
        self.write_mirror(&state, config.as_ref());
        self.inner.lock().unwrap().auth_state = state;
    }

    /// 当前生效的应用凭证(走完整优先级链)。
    pub fn current_app_config(&self) -> Option<FeishuAppConfig> {
        FeishuAppConfig::load(
            Some(self.app_config_store.as_ref()),
            &self.environment,
            &self.config_file_path,
        )
    }

    /// 只读凭据管理器里的那条(设置页「清除」按钮的显隐依据 — env /
    /// 文件来源的配置没有可清除的条目)。
    pub fn keyring_app_config(&self) -> Option<FeishuAppConfig> {
        self.app_config_store.load().unwrap_or(None)
    }

    /// 设置页保存:trim → 三字段非空 → 写凭据管理器 → 状态重算。
    /// Windows 与 ai 设置同款惯例:**secret 不回传 webview**,表单里
    /// secret 留空 = 沿用已存值(App ID 单独改的场景不用重贴 Secret;
    /// 没存过且留空才是缺字段)。
    pub fn save_app_config(
        &self,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
    ) -> Result<(), SaveAppConfigError> {
        let id = client_id.trim();
        let redirect = redirect_uri.trim();
        let secret = {
            let trimmed = client_secret.trim();
            if trimmed.is_empty() {
                match self.app_config_store.load() {
                    Ok(Some(existing)) if !existing.client_secret.is_empty() => {
                        existing.client_secret
                    }
                    _ => return Err(SaveAppConfigError::MissingField),
                }
            } else {
                trimmed.to_string()
            }
        };
        if id.is_empty() || redirect.is_empty() {
            return Err(SaveAppConfigError::MissingField);
        }
        let config = FeishuAppConfig::new(id, &secret, redirect);
        if let Err(detail) = self.app_config_store.save(&config) {
            return Err(SaveAppConfigError::PersistFailed(detail));
        }
        self.refresh_auth_state();
        Ok(())
    }

    /// 清掉凭据管理器里的应用凭证(解析链回落到 env / 文件)。不动
    /// 用户令牌 — 那是 `logout` 的事。
    pub fn clear_app_config(&self) {
        if let Err(detail) = self.app_config_store.clear() {
            eprintln!("[feishu] clear app config failed: {detail}");
        }
        self.refresh_auth_state();
    }

    /// 完整授权码流程:拉浏览器 → 等回环 → 交换 → 存库。错误内部
    /// 记入 last_error(UI 直接展示),同时带回给命令层做返回值。
    pub async fn login(&self) -> Result<FeishuCredentials, OAuthError> {
        let Some(config) = self.current_app_config() else {
            let error = OAuthError::NotConfigured;
            self.set_last_error(login_error_text(&error));
            return Err(error);
        };
        let client = self.build_oauth_client(config);
        self.set_logging_in(true);
        let result = client.login(LOGIN_TIMEOUT).await;
        self.set_logging_in(false);
        match result {
            Ok(credentials) => {
                self.inner.lock().unwrap().last_error = None;
                self.refresh_auth_state();
                Ok(credentials)
            }
            Err(error) => {
                let text = login_error_text(&error);
                eprintln!("[feishu] login error: {text}");
                self.set_last_error(text);
                Err(error)
            }
        }
    }

    /// 尽力登出:无条件清令牌库(失败也清 — UI 必须能回到 loggedOut,
    /// 下次登录才走得通),再重算状态。
    pub async fn logout(&self) {
        if let Err(detail) = self.credential_store.clear() {
            eprintln!("[feishu] logout clear failed (ignored): {detail}");
        }
        self.refresh_auth_state();
    }

    /// F3/F4 消费:装好的 API 客户端(生产传输 + OAuth 令牌供给)。
    /// 未配置凭证 → None(调用方走「未配置」文案)。
    pub fn http_api(
        &self,
    ) -> Option<FeishuHttpApi<ReqwestTransport, OAuthTokenProvider<ReqwestTransport>, TokioSleeper>>
    {
        let config = self.current_app_config()?;
        let oauth = Arc::new(self.build_oauth_client(config));
        Some(FeishuHttpApi::new(
            DEFAULT_BASE_URL,
            ReqwestTransport::default(),
            OAuthTokenProvider::new(oauth),
        ))
    }

    fn build_oauth_client(
        &self,
        config: FeishuAppConfig,
    ) -> FeishuOAuthClient<ReqwestTransport> {
        FeishuOAuthClient::new(
            config,
            ReqwestTransport::default(),
            self.credential_store.clone(),
            Arc::new(FeishuLoopbackReceiver::new()),
            Arc::new(SystemBrowserOpener),
        )
    }

    fn set_last_error(&self, text: String) {
        self.inner.lock().unwrap().last_error = Some(text);
    }

    fn set_logging_in(&self, value: bool) {
        self.inner.lock().unwrap().is_logging_in = value;
    }

    // MARK: 镜像读写

    fn read_mirror(&self) -> Option<FeishuStateMirror> {
        let data = std::fs::read(&self.state_path).ok()?;
        // 损坏镜像 = 无镜像(保守 notConfigured),不 panic 不报障。
        serde_json::from_slice(&data).ok()
    }

    fn write_mirror(&self, state: &AuthState, config: Option<&FeishuAppConfig>) {
        let mirror = FeishuStateMirror {
            auth_state: Some(
                match state {
                    AuthState::LoggedIn { .. } => "loggedIn",
                    AuthState::LoggedOut => "loggedOut",
                    AuthState::NotConfigured => "notConfigured",
                }
                .to_string(),
            ),
            tenant_key: match state {
                AuthState::LoggedIn { tenant_key } => tenant_key.clone(),
                _ => None,
            },
            client_id: config.map(|c| c.client_id.clone()),
            redirect_uri: config.map(|c| c.redirect_uri.clone()),
        };
        if let Some(parent) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(detail) = serde_json::to_string_pretty(&mirror)
            .map_err(|e| e.to_string())
            .and_then(|text| std::fs::write(&self.state_path, text).map_err(|e| e.to_string()))
        {
            eprintln!("[feishu] state mirror write failed: {detail}");
        }
    }
}

impl Default for FeishuSyncManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 镜像默认位置:`%APPDATA%\com.shampoo.donemd\feishu-state.json`。
pub fn default_state_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("com.shampoo.donemd").join(MIRROR_FILE)
}

/// 登录失败的用户文案(Swift `登录失败:\(error)` + 未配置引导)。
fn login_error_text(error: &OAuthError) -> String {
    match error {
        OAuthError::NotConfigured => {
            "未配置飞书应用凭证。请到「设置 → 飞书同步」填好 App ID / App Secret 并保存。".into()
        }
        other => format!("登录失败:{other}"),
    }
}

// MARK: - 令牌适配器(F3/F4 推拉命令消费)

/// `AccessTokenProvider` 的 OAuth 实现 — 把 FeishuHttpApi 的令牌需求
/// 接到 OAuth 客户端上:
/// - `access_token`:过期检查 + 按需刷新(`refresh_if_needed`);
/// - `force_refresh`:无视过期的强刷 — 401 重试腿,服务端已拒,
///   本地时钟判断作废。
///
/// 错误映射沿 http_client `bearer()` 语义:token 端点网络故障 =
/// NetworkUnreachable(断网对用户是同一个问题);其余(未登录 /
/// 交换失败 / 存储故障)一律 Unauthorized — 路由到「请重新登录」。
pub struct OAuthTokenProvider<T: FeishuTransport> {
    client: Arc<FeishuOAuthClient<T>>,
}

impl<T: FeishuTransport> OAuthTokenProvider<T> {
    pub fn new(client: Arc<FeishuOAuthClient<T>>) -> Self {
        Self { client }
    }
}

/// OAuthError → FeishuApiError(见 `OAuthTokenProvider` 文档)。
pub fn map_oauth_error(error: OAuthError) -> FeishuApiError {
    match error {
        OAuthError::NetworkUnreachable(detail) => FeishuApiError::NetworkUnreachable(detail),
        _ => FeishuApiError::Unauthorized,
    }
}

impl<T: FeishuTransport> AccessTokenProvider for OAuthTokenProvider<T> {
    async fn access_token(&self) -> Result<String, FeishuApiError> {
        self.client
            .refresh_if_needed()
            .await
            .map(|credentials| credentials.access_token)
            .map_err(map_oauth_error)
    }

    async fn force_refresh(&self) -> Result<String, FeishuApiError> {
        self.client
            .force_refresh()
            .await
            .map(|credentials| credentials.access_token)
            .map_err(map_oauth_error)
    }
}

// MARK: - 测试(Swift 侧 FeishuSyncManager 无单测 — 浏览器/Keychain
// 不可注入;Windows 侧全依赖可注入,补上状态机与镜像组)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::app_config::AppConfigStore;
    use crate::feishu::credentials::InMemoryCredentialStore;
    use crate::feishu::oauth::OAuthCallbackReceiver;
    use crate::feishu::oauth_receiver::ReceiverError;

    fn temp_state_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("donemd-test-{}", uuid::Uuid::new_v4()))
            .join(MIRROR_FILE)
    }

    /// 测试装配:内存凭据存储 + 内存应用配置存储 + 临时镜像路径 +
    /// 空环境 + 不存在的配置文件。
    fn make_manager() -> (FeishuSyncManager, Arc<InMemoryCredentialStore>, Arc<InMemoryAppConfigStore>) {
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let config_store = Arc::new(InMemoryAppConfigStore::new());
        let mut manager = FeishuSyncManager::with_parts(
            credentials.clone(),
            config_store.clone(),
            temp_state_path(),
        );
        manager.environment.clear();
        manager.config_file_path = std::env::temp_dir()
            .join(format!("nonexistent-{}.json", uuid::Uuid::new_v4()));
        (manager, credentials, config_store)
    }

    /// AppConfigStore 的内存实现(app_config 测试里的 Stub 对应物)。
    struct InMemoryAppConfigStore {
        stored: Mutex<Option<FeishuAppConfig>>,
    }

    impl InMemoryAppConfigStore {
        fn new() -> Self {
            Self {
                stored: Mutex::new(None),
            }
        }
    }

    impl AppConfigStore for InMemoryAppConfigStore {
        fn save(&self, config: &FeishuAppConfig) -> Result<(), String> {
            *self.stored.lock().unwrap() = Some(config.clone());
            Ok(())
        }
        fn load(&self) -> Result<Option<FeishuAppConfig>, String> {
            Ok(self.stored.lock().unwrap().clone())
        }
        fn clear(&self) -> Result<(), String> {
            *self.stored.lock().unwrap() = None;
            Ok(())
        }
    }

    fn sample_config() -> FeishuAppConfig {
        FeishuAppConfig::new(
            "cli_test",
            "secret-test",
            "http://localhost:18127/oauth/callback",
        )
    }

    fn sample_credentials() -> FeishuCredentials {
        FeishuCredentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 4102444800.0,
            tenant_key: Some("ten_abc".into()),
        }
    }

    // MARK: 状态机

    #[test]
    fn refresh_auth_state_not_configured_when_chain_empty() {
        let (manager, _, _) = make_manager();
        manager.refresh_auth_state();
        assert_eq!(manager.auth_state(), AuthState::NotConfigured);
    }

    #[test]
    fn refresh_auth_state_logged_out_with_config_only() {
        let (manager, _, config_store) = make_manager();
        config_store.save(&sample_config()).unwrap();
        manager.refresh_auth_state();
        assert_eq!(manager.auth_state(), AuthState::LoggedOut);
    }

    #[test]
    fn refresh_auth_state_logged_in_with_tenant() {
        let (manager, credentials, config_store) = make_manager();
        config_store.save(&sample_config()).unwrap();
        credentials.save(&sample_credentials()).unwrap();
        manager.refresh_auth_state();
        assert_eq!(
            manager.auth_state(),
            AuthState::LoggedIn {
                tenant_key: Some("ten_abc".into())
            }
        );
    }

    #[test]
    fn refresh_auth_state_store_failure_treated_as_logged_out() {
        // 凭据读失败(锁屏/服务不可用)→ loggedOut,不让设置页僵死。
        let (mut manager, _, config_store) = make_manager();
        config_store.save(&sample_config()).unwrap();
        // InMemory 不支持注错 — 用一个恒失败的包装。
        let failing = Arc::new(FailingCredentialStore);
        manager.credential_store = failing;
        manager.refresh_auth_state();
        assert_eq!(manager.auth_state(), AuthState::LoggedOut);
    }

    struct FailingCredentialStore;

    impl CredentialStore for FailingCredentialStore {
        fn save(&self, _: &FeishuCredentials) -> Result<(), String> {
            Err("locked".into())
        }
        fn load(&self) -> Result<Option<FeishuCredentials>, String> {
            Err("locked".into())
        }
        fn clear(&self) -> Result<(), String> {
            Err("locked".into())
        }
    }

    // MARK: 应用凭证保存

    #[test]
    fn save_app_config_trims_and_persists() {
        let (manager, _, config_store) = make_manager();
        manager
            .save_app_config("  cli_x \n", "  secret-y  ", " http://localhost:18127/oauth/callback ")
            .unwrap();
        assert_eq!(config_store.load().unwrap(), Some(sample_like_config()));
        // 保存后状态从 notConfigured 变 loggedOut。
        assert_eq!(manager.auth_state(), AuthState::LoggedOut);
    }

    fn sample_like_config() -> FeishuAppConfig {
        FeishuAppConfig::new(
            "cli_x",
            "secret-y",
            "http://localhost:18127/oauth/callback",
        )
    }

    #[test]
    fn save_app_config_missing_field_reuses_stored_secret_or_fails() {
        let (manager, _, _) = make_manager();
        // 什么都没存过 — secret 空即缺字段。
        assert_eq!(
            manager.save_app_config("cli_x", "", "http://localhost:18127/oauth/callback"),
            Err(SaveAppConfigError::MissingField)
        );
        // id / redirect 空同样缺字段。
        assert_eq!(
            manager.save_app_config("", "s", "http://localhost:18127/oauth/callback"),
            Err(SaveAppConfigError::MissingField)
        );

        // 存过之后 secret 留空 = 沿用已存 secret(改 App ID 不重贴)。
        let (manager, _, _) = make_manager();
        manager
            .save_app_config("cli_first", "secret-keep", "http://localhost:18127/oauth/callback")
            .unwrap();
        manager
            .save_app_config("cli_second", "", "http://localhost:18127/oauth/callback")
            .unwrap();
        assert_eq!(manager.keyring_app_config().unwrap().client_id, "cli_second");
        assert_eq!(
            manager.keyring_app_config().unwrap().client_secret,
            "secret-keep"
        );
    }

    #[test]
    fn clear_app_config_falls_back_and_recomputes() {
        let (manager, _, config_store) = make_manager();
        config_store.save(&sample_config()).unwrap();
        manager.refresh_auth_state();
        assert_eq!(manager.auth_state(), AuthState::LoggedOut);

        manager.clear_app_config();
        assert_eq!(config_store.load().unwrap(), None);
        assert_eq!(manager.auth_state(), AuthState::NotConfigured);
    }

    // MARK: 镜像

    #[test]
    fn mirror_roundtrip_boot_from_another_manager() {
        let path = temp_state_path();
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let config_store = Arc::new(InMemoryAppConfigStore::new());
        let mut first = FeishuSyncManager::with_parts(
            credentials.clone(),
            config_store.clone(),
            path.clone(),
        );
        first.environment.clear();
        first.config_file_path =
            std::env::temp_dir().join(format!("nonexistent-{}.json", uuid::Uuid::new_v4()));

        config_store.save(&sample_config()).unwrap();
        credentials.save(&sample_credentials()).unwrap();
        first.refresh_auth_state();

        // 同路径的第二个实例(= 重启)只读镜像就还原状态。
        let second = FeishuSyncManager::with_parts(
            Arc::new(InMemoryCredentialStore::new()),
            Arc::new(InMemoryAppConfigStore::new()),
            path,
        );
        second.boot();
        assert_eq!(
            second.auth_state(),
            AuthState::LoggedIn {
                tenant_key: Some("ten_abc".into())
            }
        );

        // 镜像里的 client_id 是非机密标识(渲染徽标用)。
        let mirror: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&second.state_path).unwrap()).unwrap();
        assert_eq!(mirror["client_id"], "cli_test");
        assert_eq!(mirror["redirect_uri"], "http://localhost:18127/oauth/callback");
        // secret / token 永不进镜像。
        let text = mirror.to_string();
        assert!(!text.contains("secret"));
        assert!(!text.contains("access_token"));
        std::fs::remove_file(&second.state_path).ok();
    }

    #[test]
    fn boot_without_mirror_is_not_configured() {
        let (manager, _, _) = make_manager();
        manager.boot();
        assert_eq!(manager.auth_state(), AuthState::NotConfigured);
    }

    #[test]
    fn boot_tolerates_corrupt_mirror() {
        let path = temp_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        let manager = FeishuSyncManager::with_parts(
            Arc::new(InMemoryCredentialStore::new()),
            Arc::new(InMemoryAppConfigStore::new()),
            path.clone(),
        );
        manager.boot();
        assert_eq!(manager.auth_state(), AuthState::NotConfigured);
        std::fs::remove_file(&path).ok();
    }

    // MARK: 登录前置 / 登出

    #[tokio::test]
    async fn login_without_config_sets_last_error_and_returns_not_configured() {
        let (manager, _, _) = make_manager();
        let error = manager.login().await.unwrap_err();
        assert_eq!(error, OAuthError::NotConfigured);
        assert!(manager.last_error().unwrap().contains("未配置飞书应用凭证"));
        assert!(!manager.is_logging_in());
    }

    #[tokio::test]
    async fn logout_clears_credentials_and_state() {
        let (manager, credentials, config_store) = make_manager();
        config_store.save(&sample_config()).unwrap();
        credentials.save(&sample_credentials()).unwrap();
        manager.refresh_auth_state();

        manager.logout().await;
        assert_eq!(credentials.load().unwrap(), None);
        assert_eq!(manager.auth_state(), AuthState::LoggedOut);
    }

    // MARK: 令牌适配器

    use crate::feishu::api::TransportResponse;
    use crate::feishu::http_client::MockTransport;
    use crate::feishu::oauth_receiver::DEFAULT_PORT;

    fn ok_transport(body: &str) -> Result<TransportResponse, FeishuApiError> {
        Ok(TransportResponse {
            status: 200,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        })
    }

    fn refresh_ok_body(new_access: &str) -> String {
        format!(
            r#"{{"code":0,"access_token":"{new_access}","refresh_token":"r2","expires_in":7200,"tenant_key":"ten_abc"}}"#
        )
    }

    /// 该路径不该有回调(token 供给不走浏览器)— 碰到即炸。
    struct PanickingReceiver;

    impl OAuthCallbackReceiver for PanickingReceiver {
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
            Box::pin(async { panic!("receiver must not be invoked on token paths") })
        }
    }

    /// OAuthTokenProvider 需要 FeishuOAuthClient<T> — 注入 MockTransport
    /// 与固定时钟,刷新腿全程离线可测。
    fn provider_client(
        transport: MockTransport,
        credentials: FeishuCredentials,
        now: f64,
    ) -> Arc<FeishuOAuthClient<MockTransport>> {
        let store = Arc::new(InMemoryCredentialStore::new());
        store.save(&credentials).unwrap();
        Arc::new(FeishuOAuthClient::with_overrides(
            sample_config(),
            transport,
            store,
            Arc::new(PanickingReceiver),
            Arc::new(SystemBrowserOpener),
            None,
            Some(Arc::new(move || now)),
            None,
        ))
    }

    #[tokio::test]
    async fn token_provider_access_returns_fresh_token_without_http() {
        // 未过期 + 时钟不动 → 不碰网络(空队列 mock,任何请求都会炸)。
        let provider = OAuthTokenProvider::new(provider_client(
            MockTransport::new(Vec::new()),
            sample_credentials(),
            1000.0, // expires_at 4102444800 远未到
        ));
        assert_eq!(provider.access_token().await.unwrap(), "a");
    }

    #[tokio::test]
    async fn token_provider_access_refreshes_when_expired() {
        let provider = OAuthTokenProvider::new(provider_client(
            MockTransport::new(vec![ok_transport(&refresh_ok_body("NEW_ACCESS"))]),
            FeishuCredentials {
                expires_at: 500.0,
                ..sample_credentials()
            },
            1000.0, // 已过期
        ));
        assert_eq!(provider.access_token().await.unwrap(), "NEW_ACCESS");
    }

    #[tokio::test]
    async fn token_provider_force_refresh_ignores_expiry() {
        // 凭据远未过期,但 force 腿必须仍然真换(401 场景)。
        let provider = OAuthTokenProvider::new(provider_client(
            MockTransport::new(vec![ok_transport(&refresh_ok_body("FORCED"))]),
            sample_credentials(),
            1000.0,
        ));
        assert_eq!(provider.force_refresh().await.unwrap(), "FORCED");
    }

    #[tokio::test]
    async fn token_provider_maps_not_authenticated_to_unauthorized() {
        // 空库 → NotAuthenticated → Unauthorized(路由「请重新登录」)。
        let client = FeishuOAuthClient::with_overrides(
            sample_config(),
            MockTransport::new(Vec::new()),
            Arc::new(InMemoryCredentialStore::new()),
            Arc::new(PanickingReceiver),
            Arc::new(SystemBrowserOpener),
            None,
            Some(Arc::new(|| 1000.0)),
            None,
        );
        let provider = OAuthTokenProvider::new(Arc::new(client));
        assert_eq!(provider.access_token().await.unwrap_err(), FeishuApiError::Unauthorized);
    }

    #[test]
    fn login_error_text_matches_swift_shapes() {
        assert!(login_error_text(&OAuthError::NotConfigured).contains("未配置"));
        assert_eq!(
            login_error_text(&OAuthError::NotAuthenticated),
            "登录失败:尚未登录飞书"
        );
    }

    /// redirect 默认值就是回环接收器注册的那条(设置页预填用)。
    #[test]
    fn receiver_redirect_uri_is_default_prefill() {
        let receiver = FeishuLoopbackReceiver::new();
        assert_eq!(
            receiver.redirect_uri(),
            format!("http://localhost:{DEFAULT_PORT}/oauth/callback")
        );
    }
}
