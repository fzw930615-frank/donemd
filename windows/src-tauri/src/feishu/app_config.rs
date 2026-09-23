//! `FeishuAppConfig.swift` + `FeishuAppConfigStore.swift` 的移植 —
//! 自建应用凭证三件套:模型、凭据管理器存储、解析链。
//!
//! 解析优先级(`load`,对齐 Swift):
//!   1. 凭据管理器(`com.shampoo.donemd.feishu-app-config`)— 设置页
//!      「飞书应用凭证」表单写入的,用户刻意设置恒胜过环境变量;
//!   2. 环境变量 `DONEMD_FEISHU_APP_ID` / `DONEMD_FEISHU_APP_SECRET` /
//!      `DONEMD_FEISHU_REDIRECT_URI`(注意 redirect 无 `APP_` 前缀,
//!      与 Swift 侧同名对齐);
//!   3. `%APPDATA%\com.shampoo.donemd\feishu-config.json`(开发流
//!      手写的;Mac 侧是 plist,Windows 用 JSON)。
//!
//! 三字段缺一即整源无效 — 返回 `None` 路由到引导态,不产半成品配置
//! (Mac 无 bundle 内置兜底;Windows 发行版同样不内嵌凭证)。
//!
//! App Secret 只进凭据管理器与请求体,**永不回传 webview**(对齐
//! ai/credentials.rs 的「设置页不回填 Key」惯例)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 应用凭证条目的服务名。
pub const APP_CONFIG_SERVICE: &str = "com.shampoo.donemd.feishu-app-config";
/// 条目账户标签。
pub const APP_CONFIG_ACCOUNT: &str = "default";

/// 环境变量名(与 Swift 侧逐字一致 — 文档与用户肌肉记忆共用)。
pub const ENV_APP_ID: &str = "DONEMD_FEISHU_APP_ID";
pub const ENV_APP_SECRET: &str = "DONEMD_FEISHU_APP_SECRET";
pub const ENV_REDIRECT_URI: &str = "DONEMD_FEISHU_REDIRECT_URI";

/// 飞书开放平台自建应用凭证。三字段必须齐活 — 少一个整个 OAuth
/// 流程都跑不起来,load 链据此返回 `None` 路由到引导页。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuAppConfig {
    /// 开放平台「凭证与基础信息」页的 App ID(`cli_` 开头)。
    pub client_id: String,
    /// App Secret — 只进 keyring/请求体,永不回传 webview。
    pub client_secret: String,
    /// 重定向 URL,须与开放平台「重定向 URL」列表逐字一致
    /// (`http://localhost:18127/oauth/callback`)。
    pub redirect_uri: String,
}

impl FeishuAppConfig {
    pub fn new(client_id: &str, client_secret: &str, redirect_uri: &str) -> Self {
        Self {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            redirect_uri: redirect_uri.to_string(),
        }
    }

    /// 用户手写配置文件的默认位置:
    /// `%APPDATA%\com.shampoo.donemd\feishu-config.json`(APPDATA
    /// 缺席时回退临时目录,与 ai-config 同款兜底)。
    pub fn default_config_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("com.shampoo.donemd").join("feishu-config.json")
    }

    /// 走标准优先级链解析。任一源不完整就落到下一源;全空返回
    /// `None`。keyring 读失败**不毒化链**(锁屏 / 服务不可用时视作
    /// 无条目继续走 env / 文件 — 关死在这里用户就没法登录了)。
    pub fn load(
        keyring_store: Option<&dyn AppConfigStore>,
        environment: &HashMap<String, String>,
        config_path: &Path,
    ) -> Option<FeishuAppConfig> {
        if let Some(store) = keyring_store {
            if let Ok(Some(from_keyring)) = store.load() {
                return Some(from_keyring);
            }
        }
        if let Some(from_env) = load_from_environment(environment) {
            return Some(from_env);
        }
        load_from_json_file(config_path)
    }

    /// 生产装配:真 keyring + 真 env + 默认路径。
    pub fn load_default() -> Option<FeishuAppConfig> {
        let environment: HashMap<String, String> = std::env::vars().collect();
        let store = KeyringAppConfigStore::new();
        Self::load(Some(&store), &environment, &Self::default_config_path())
    }
}

/// 环境变量源 — 三变量全非空白才有效;值防御性 trim(Xcode 式
/// 环境变量面板带尾换行是真实场景,redirect URI 带杂白会被飞书拒)。
pub fn load_from_environment(env: &HashMap<String, String>) -> Option<FeishuAppConfig> {
    let field = |name: &str| -> Option<String> {
        env.get(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some(FeishuAppConfig {
        client_id: field(ENV_APP_ID)?,
        client_secret: field(ENV_APP_SECRET)?,
        redirect_uri: field(ENV_REDIRECT_URI)?,
    })
}

/// JSON 文件源(读写都走它 — Mac 侧用户手写 plist 的对应物)。
pub fn load_from_json_file(path: &Path) -> Option<FeishuAppConfig> {
    let data = std::fs::read(path).ok()?;
    decode_config_json(&data)
}

/// 配置 JSON 的形状解码(纯函数,测试主战场)。三键全在且非空白
/// 才有效;缺任一键 → `None`,无半成品。
pub fn decode_config_json(data: &[u8]) -> Option<FeishuAppConfig> {
    let raw: HashMap<String, String> = serde_json::from_slice(data).ok()?;
    let field = |key: &str| -> Option<String> {
        raw.get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some(FeishuAppConfig {
        client_id: field("client_id")?,
        client_secret: field("client_secret")?,
        redirect_uri: field("redirect_uri")?,
    })
}

/// 存储抽象(Swift `AppConfigStore` 协议)。生产 keyring,测试注入
/// stub(StubAppConfigStore 对应物)— CI 不碰真凭据管理器。
pub trait AppConfigStore: Send + Sync {
    fn save(&self, config: &FeishuAppConfig) -> Result<(), String>;
    fn load(&self) -> Result<Option<FeishuAppConfig>, String>;
    fn clear(&self) -> Result<(), String>;
}

/// 凭据管理器实现(`FeishuKeychainAppConfigStore` 对应物)。与用户
/// 凭据(`credentials::KeyringCredentialStore`)分立两条目 — 两个
/// 载荷语义不同(应用级 vs 用户级)、轮换节奏不同(换 App Secret
/// vs 登出)、UI 面也不同。
///
/// 条目体是 `{"client_id","client_secret","redirect_uri"}` JSON;
/// 读出后按 load 同款 trim/非空校验,不完整视作无条目。
pub struct KeyringAppConfigStore {
    service: String,
    account: String,
}

impl KeyringAppConfigStore {
    pub fn new() -> Self {
        Self {
            service: APP_CONFIG_SERVICE.to_string(),
            account: APP_CONFIG_ACCOUNT.to_string(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, &self.account).map_err(|e| e.to_string())
    }
}

impl Default for KeyringAppConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AppConfigStore for KeyringAppConfigStore {
    fn save(&self, config: &FeishuAppConfig) -> Result<(), String> {
        let entry = self.entry()?;
        let payload = serde_json::json!({
            "client_id": config.client_id,
            "client_secret": config.client_secret,
            "redirect_uri": config.redirect_uri,
        })
        .to_string();
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(e.to_string()),
        }
        entry.set_password(&payload).map_err(|e| e.to_string())
    }

    fn load(&self) -> Result<Option<FeishuAppConfig>, String> {
        let payload = match self.entry()?.get_password() {
            Ok(payload) => payload,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        Ok(decode_config_json(payload.as_bytes()))
    }

    fn clear(&self) -> Result<(), String> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// 设置页保存的校验错误(Swift `SaveAppConfigError`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveAppConfigError {
    /// 三字段有空白 — 表单侧本就该禁用保存,这里是最后防线。
    MissingField,
    /// 凭据管理器写失败。
    PersistFailed(String),
}

// MARK: - 测试(FeishuAppConfigTests.swift,16 例;plist→JSON,bundle 分支无对应物)

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // MARK: 环境变量源

    fn env(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn load_from_environment_happy_path() {
        let config = load_from_environment(&env(&[
            (ENV_APP_ID, "cli_aaa"),
            (ENV_APP_SECRET, "secret-bbb"),
            (ENV_REDIRECT_URI, "http://localhost:18127/oauth/callback"),
        ]))
        .unwrap();
        assert_eq!(config.client_id, "cli_aaa");
        assert_eq!(config.client_secret, "secret-bbb");
        assert_eq!(config.redirect_uri, "http://localhost:18127/oauth/callback");
    }

    #[test]
    fn load_from_environment_missing_field_returns_none() {
        assert!(load_from_environment(&env(&[
            (ENV_APP_ID, "cli_aaa"),
            (ENV_APP_SECRET, ""),
            (ENV_REDIRECT_URI, "http://localhost:18127/oauth/callback"),
        ]))
        .is_none());
    }

    #[test]
    fn load_from_environment_whitespace_only_treated_as_missing() {
        assert!(load_from_environment(&env(&[
            (ENV_APP_ID, "  "),
            (ENV_APP_SECRET, "secret"),
            (ENV_REDIRECT_URI, "http://localhost:18127/oauth/callback"),
        ]))
        .is_none());
    }

    #[test]
    fn load_from_environment_trims_whitespace() {
        // 环境变量面板带尾换行不能毒化值 — 飞书拒带杂白的
        // redirect URI,防御性 trim。
        let config = load_from_environment(&env(&[
            (ENV_APP_ID, "  cli_aaa\n"),
            (ENV_APP_SECRET, "secret-bbb"),
            (ENV_REDIRECT_URI, "http://localhost:18127/oauth/callback "),
        ]))
        .unwrap();
        assert_eq!(config.client_id, "cli_aaa");
        assert_eq!(config.redirect_uri, "http://localhost:18127/oauth/callback");
    }

    // MARK: JSON 文件源

    #[test]
    fn decode_config_json_happy_path() {
        let config = decode_config_json(
            br#"{"client_id":"cli_xxx","client_secret":"secret-yyy","redirect_uri":"http://localhost:18127/oauth/callback"}"#,
        )
        .unwrap();
        assert_eq!(config.client_id, "cli_xxx");
        assert_eq!(config.client_secret, "secret-yyy");
        assert_eq!(config.redirect_uri, "http://localhost:18127/oauth/callback");
    }

    #[test]
    fn decode_config_json_missing_field_returns_none() {
        // 空串 secret = 缺字段。
        assert!(decode_config_json(
            br#"{"client_id":"cli_xxx","client_secret":"","redirect_uri":"http://x/cb"}"#
        )
        .is_none());
    }

    #[test]
    fn decode_config_json_malformed_returns_none() {
        assert!(decode_config_json(b"this is not json").is_none());
    }

    #[test]
    fn load_from_json_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("donemd-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feishu-config.json");
        std::fs::write(
            &path,
            r#"{"client_id":"cli_zzz","client_secret":"secret-aaa","redirect_uri":"http://localhost:18127/oauth/callback"}"#,
        )
        .unwrap();
        let config = load_from_json_file(&path).unwrap();
        assert_eq!(config.client_id, "cli_zzz");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_from_json_file_missing_file_returns_none() {
        let nowhere = std::env::temp_dir()
            .join(format!("does-not-exist-{}.json", uuid::Uuid::new_v4()));
        assert!(load_from_json_file(&nowhere).is_none());
    }

    // MARK: 优先级链

    /// 测试 stub(Swift StubAppConfigStore 对应物)— 可注入 load 失败。
    struct StubAppConfigStore {
        stored: Mutex<Option<FeishuAppConfig>>,
        load_error: bool,
    }

    impl StubAppConfigStore {
        fn new() -> Self {
            Self {
                stored: Mutex::new(None),
                load_error: false,
            }
        }
        fn with(config: FeishuAppConfig) -> Self {
            Self {
                stored: Mutex::new(Some(config)),
                load_error: false,
            }
        }
    }

    impl AppConfigStore for StubAppConfigStore {
        fn save(&self, config: &FeishuAppConfig) -> Result<(), String> {
            *self.stored.lock().unwrap() = Some(config.clone());
            Ok(())
        }
        fn load(&self) -> Result<Option<FeishuAppConfig>, String> {
            if self.load_error {
                return Err("locked".into());
            }
            Ok(self.stored.lock().unwrap().clone())
        }
        fn clear(&self) -> Result<(), String> {
            *self.stored.lock().unwrap() = None;
            Ok(())
        }
    }

    fn env_full(id: &str) -> HashMap<String, String> {
        env(&[
            (ENV_APP_ID, id),
            (ENV_APP_SECRET, "e-secret"),
            (ENV_REDIRECT_URI, "http://localhost:18127/oauth/callback"),
        ])
    }

    #[test]
    fn keyring_wins_over_env_and_file() {
        let keyring = StubAppConfigStore::with(FeishuAppConfig::new(
            "from-keyring",
            "k-secret",
            "http://127.0.0.1:9876/callback",
        ));
        let config = FeishuAppConfig::load(
            Some(&keyring),
            &env_full("from-env"),
            Path::new("nonexistent.json"),
        )
        .unwrap();
        assert_eq!(config.client_id, "from-keyring", "设置页存的凭证必须压过 env/文件");
    }

    #[test]
    fn keyring_empty_falls_through_to_env() {
        let keyring = StubAppConfigStore::new();
        let config = FeishuAppConfig::load(
            Some(&keyring),
            &env_full("from-env"),
            Path::new("nonexistent.json"),
        )
        .unwrap();
        assert_eq!(config.client_id, "from-env", "keyring 空须落到 env,不能返回 None");
    }

    #[test]
    fn keyring_error_does_not_poison_chain() {
        // keyring 读失败(锁屏/服务不可用)视作无条目继续走链 —
        // 关死在这里用户就没法登录了。
        let keyring = StubAppConfigStore {
            stored: Mutex::new(None),
            load_error: true,
        };
        let config = FeishuAppConfig::load(
            Some(&keyring),
            &env_full("from-env"),
            Path::new("nonexistent.json"),
        )
        .unwrap();
        assert_eq!(config.client_id, "from-env");
    }

    #[test]
    fn env_wins_over_file() {
        let dir = std::env::temp_dir().join(format!("donemd-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feishu-config.json");
        std::fs::write(
            &path,
            r#"{"client_id":"from-file","client_secret":"p-secret","redirect_uri":"http://localhost:18127/oauth/callback"}"#,
        )
        .unwrap();
        let config =
            FeishuAppConfig::load(None, &env_full("from-env"), &path).unwrap();
        assert_eq!(config.client_id, "from-env", "env 必须压过手写文件");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_used_when_env_incomplete() {
        let dir = std::env::temp_dir().join(format!("donemd-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feishu-config.json");
        std::fs::write(
            &path,
            r#"{"client_id":"from-file","client_secret":"p-secret","redirect_uri":"http://localhost:18127/oauth/callback"}"#,
        )
        .unwrap();
        // 只设了一个 env 变量 — 不完整,须落穿到文件。
        let config =
            FeishuAppConfig::load(None, &env(&[(ENV_APP_ID, "from-env")]), &path).unwrap();
        assert_eq!(config.client_id, "from-file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn returns_none_when_all_sources_empty() {
        assert!(FeishuAppConfig::load(
            None,
            &HashMap::new(),
            Path::new("nonexistent.json"),
        )
        .is_none());
    }

    #[test]
    fn stub_store_save_load_roundtrip() {
        let store = StubAppConfigStore::new();
        let config =
            FeishuAppConfig::new("cli_save", "secret-save", "http://127.0.0.1:9999/cb");
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), Some(config));
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None, "clear() 必须清条目");
    }

    /// 真·凭据管理器探针(独立服务名,不碰生产条目)。手动:
    /// `cargo test feishu::app_config -- --ignored --nocapture`
    #[test]
    #[ignore = "写真实凭据管理器,手动跑"]
    fn keyring_app_config_store_roundtrip_probe() {
        let store = KeyringAppConfigStore {
            service: "com.shampoo.donemd.probe.feishu-app-config".into(),
            account: "probe".into(),
        };
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);

        let config = FeishuAppConfig::new(
            "cli_probe",
            "secret-probe",
            "http://localhost:18127/oauth/callback",
        );
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), Some(config));

        // 半成品条目(缺 secret)读出为 None,不当有效配置用。
        let entry = keyring::Entry::new(
            "com.shampoo.donemd.probe.feishu-app-config",
            "probe",
        )
        .unwrap();
        entry.set_password(r#"{"client_id":"only-id"}"#).unwrap();
        assert_eq!(store.load().unwrap(), None);

        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }
}
