//! 飞书文档同步 — `donemd/Feishu/`(Swift)的 Rust 移植(M8)。
//!
//! 分五阶段落地(F1 转换层 → F2 网络+身份 → F3 同步引擎 → F4 接线 →
//! F5 后置可选),每个文件对应一个 Swift 源文件,doc 注释标注来源。
//! 日志统一 `eprintln!("[feishu] …")`;离线测试用纯函数 + 泛型注入
//! Mock,真网用例 `#[ignore]`。
//!
//! F1 ✅(转换层,零网络)/ F2 🔄(网络+身份,F2.1–F2.3 网络与凭据面
//! 已落,设置页命令在本文件尾部):
//! - [`block`]          ← `FeishuBlock.swift`
//! - [`callout`]        ← `FeishuCalloutType.swift`
//! - [`encoder`]        ← `FeishuBlockEncoder.swift`
//! - [`converter`]      ← `FeishuStructuralConverter.swift`
//! - [`api`]            ← `FeishuAPIClient.swift`(前半:trait + 错误 +
//!                        纯请求构造/分类/解析;reqwest 编排在 http_client)
//! - [`http_client`]    ← `FeishuAPIClient.swift`(后半:reqwest 传输 +
//!                        401 刷新重试 + 退避 + 分页聚合 + 图片 SHA-256 缓存)
//! - [`oauth`]          ← `FeishuOAuthClient.swift`(authorize URL /
//!                        token 交换 / refresh;60s skew 过期判定)
//! - [`oauth_receiver`] ← `FeishuOAuthCallbackReceiver.swift`
//!                        (127.0.0.1:18127 手写回环接收器)
//! - [`credentials`]    ← `FeishuCredentials.swift` +
//!                        `KeychainCredentialStore.swift`(模型 + 凭据
//!                        管理器存储)
//! - [`app_config`]     ← `FeishuAppConfig.swift` +
//!                        `FeishuAppConfigStore.swift`(模型 + 存储 +
//!                        keyring > env > JSON 三级解析链)
//! - [`manager`]        ← `Settings/FeishuSyncManager.swift`(AuthState
//!                        状态机 + 非机密镜像 + OAuthTokenProvider)
//!
//! 注意:模块分步接入期间 `cargo check` 会出现少量 dead_code 警告
//! (pub 编解码入口在 F3/F4 接线后消除),与
//! `markdown/frontmatter.rs` 预留接口同款情形。当前过渡面:
//! `manager::http_api` 由 F4 推拉命令消费。

pub mod api;
pub mod app_config;
pub mod block;
pub mod callout;
pub mod converter;
pub mod credentials;
pub mod encoder;
pub mod http_client;
pub mod manager;
pub mod oauth;
pub mod oauth_receiver;

// MARK: - 设置页命令(设置窗 invoke;secret 永不回传 webview)

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// 设置页打开时拉全量状态:实时重算(Windows CredMan 读取静默且快,
/// 不需要 Mac 侧「启动只读镜像」的让步 — 镜像只服务主窗启动徽标)。
/// 返回体只含非机密字段;secret 由「留空 = 沿用已存」的保存语义
/// 覆盖(对齐 ai 设置不回填 Key 的惯例)。
#[tauri::command]
pub fn feishu_settings_load(app: AppHandle) -> Value {
    let manager = &app.state::<AppState>().feishu;
    manager.refresh_auth_state();

    let auth_state = manager.auth_state();
    let (state_name, tenant_key) = match &auth_state {
        manager::AuthState::LoggedIn { tenant_key } => ("loggedIn", tenant_key.clone()),
        manager::AuthState::LoggedOut => ("loggedOut", None),
        manager::AuthState::NotConfigured => ("notConfigured", None),
    };
    // 徽标三态:Settings 已配置(keyring 有条目)/ env·文件(链上有
    // 但不是设置页管的)/ 未配置。
    let keyring_config = manager.keyring_app_config();
    let active = manager.current_app_config();
    let source = if keyring_config.is_some() {
        "keyring"
    } else if active.is_some() {
        "envOrFile"
    } else {
        "none"
    };
    let active_client_id = active.as_ref().map(|c| c.client_id.clone());
    json!({
        "authState": state_name,
        "tenantKey": tenant_key,
        "isLoggingIn": manager.is_logging_in(),
        "lastError": manager.last_error(),
        "source": source,
        // 非机密标识,表单预填用(secret 恒不回传)。
        "clientId": active_client_id,
        "redirectUri": active.as_ref().map(|c| c.redirect_uri.clone()),
        // 表单默认预填:回环接收器注册的固定回调地址。
        "defaultRedirectUri": oauth_receiver::FeishuLoopbackReceiver::new().redirect_uri(),
    })
}

/// 保存应用凭证到凭据管理器。secret 留空 = 沿用已存值。
#[tauri::command]
pub async fn feishu_save_app_config(
    app: AppHandle,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
) -> Value {
    let manager = &app.state::<AppState>().feishu;
    match manager.save_app_config(&client_id, &client_secret, &redirect_uri) {
        Ok(()) => json!({ "ok": true }),
        Err(app_config::SaveAppConfigError::MissingField) => {
            json!({ "ok": false, "error": "三个字段都不能为空。" })
        }
        Err(app_config::SaveAppConfigError::PersistFailed(detail)) => {
            json!({ "ok": false, "error": format!("写入凭据管理器失败:{detail}") })
        }
    }
}

/// 清掉凭据管理器里的应用凭证(链回落到 env / 文件;不动用户令牌)。
#[tauri::command]
pub fn feishu_clear_app_config(app: AppHandle) -> Value {
    app.state::<AppState>().feishu.clear_app_config();
    json!({ "ok": true })
}

/// 拉起浏览器走完整授权码流程(最长 5 分钟;进度态走
/// `isLoggingIn`,前端按钮转「正在等待浏览器授权…」)。
#[tauri::command]
pub async fn feishu_login(app: AppHandle) -> Value {
    let manager = &app.state::<AppState>().feishu;
    match manager.login().await {
        Ok(_) => json!({ "ok": true }),
        Err(_) => json!({ "ok": false, "error": manager.last_error().unwrap_or_default() }),
    }
}

/// 登出:无条件清本地令牌(飞书无对应 revoke 端点可用 — 要租户级
/// token,客户端没有)。
#[tauri::command]
pub async fn feishu_logout(app: AppHandle) -> Value {
    app.state::<AppState>().feishu.logout().await;
    json!({ "ok": true })
}
