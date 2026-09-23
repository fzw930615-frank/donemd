//! `FeishuCredentials.swift` + `KeychainCredentialStore.swift` 的移植 —
//! OAuth 凭据模型 + 存储抽象 + Windows 凭据管理器实现。
//!
//! Mac 存 Keychain;Windows 对应物是凭据管理器(keyring crate 的
//! windows-native 后端,静态加密、按用户隔离)。服务名沿用
//! `<bundleID>.feishu-oauth` 形状,与 ai/credentials.rs 的既有命名
//! 同一审计线索。

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// 凭据条目的服务名(`com.shampoo.donemd.feishu-oauth`)。
pub const CREDENTIAL_SERVICE: &str = "com.shampoo.donemd.feishu-oauth";
/// 条目账户标签 — 单安装单凭据,固定 `default`(多租户前向兼容保留
/// 字段位,对齐 Swift)。
pub const CREDENTIAL_ACCOUNT: &str = "default";

/// 飞书授权交换后的 OAuth 凭据 — 落在凭据管理器(对应 Mac 的
/// Keychain)跨启动存活。
///
/// `expires_at` 是**绝对** unix 秒(交换时由 `expires_in` 换算),
/// 而不是时长 — 长睡眠后的陈旧读取不会把过期 token 误判为有效。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeishuCredentials {
    /// 每次 API 调用随 Bearer 头出示的短命 token。
    pub access_token: String,
    /// 换新 access_token 的长命 token。飞书每次刷新都发**新的**
    /// refresh_token — 最新一枚必须持久化。
    pub refresh_token: String,
    /// `access_token` 的绝对过期时刻(unix 秒)。
    pub expires_at: f64,
    /// 租户键 — 多租户用户靠它圈定 API 调用范围;单租户自建应用
    /// 飞书可能不回,故可缺省。
    pub tenant_key: Option<String>,
}

impl FeishuCredentials {
    /// `access_token` 是否已过期(60s 提前量:请求在途时刚好到期的
    /// token 视为已过期,不是「临界有效」)。
    pub fn is_expired(&self, now_secs: f64) -> bool {
        self.is_expired_with_skew(now_secs, 60.0)
    }

    /// 带自定义提前量的判定(测试注 0 验证边界)。
    pub fn is_expired_with_skew(&self, now_secs: f64, skew: f64) -> bool {
        now_secs + skew >= self.expires_at
    }
}

/// 凭据存储抽象(Swift `CredentialStore` 协议)。生产是 keyring
/// (F2.3),测试注入内存实现 — 凭据管理器不能在 CI 里被污染。
pub trait CredentialStore: Send + Sync {
    fn save(&self, credentials: &FeishuCredentials) -> Result<(), String>;
    fn load(&self) -> Result<Option<FeishuCredentials>, String>;
    fn clear(&self) -> Result<(), String>;
}

/// 内存实现 — oauth 测试与未接线期的占位。
#[derive(Default)]
pub struct InMemoryCredentialStore {
    stored: Mutex<Option<FeishuCredentials>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn save(&self, credentials: &FeishuCredentials) -> Result<(), String> {
        *self.stored.lock().unwrap() = Some(credentials.clone());
        Ok(())
    }
    fn load(&self) -> Result<Option<FeishuCredentials>, String> {
        Ok(self.stored.lock().unwrap().clone())
    }
    fn clear(&self) -> Result<(), String> {
        *self.stored.lock().unwrap() = None;
        Ok(())
    }
}

/// 凭据管理器实现(`KeychainCredentialStore` 对应物)。
///
/// 载荷按 UTF-16 预算分片存多条条目:索引写在规范账户名下,分片写在
/// `<account>.<i>`(keyring 在 Windows 下 `target_name = "{user}.{service}"`,
/// 故变化 account 即得到互不相干的条目)。
pub struct KeyringCredentialStore {
    service: String,
    account: String,
}

// MARK: - 分片存储(绕开凭据管理器的 blob 上限)

/// keyring 的 windows-native 后端按 `password.encode_utf16().count() * 2`
/// 校验 `CRED_MAX_CREDENTIAL_BLOB_SIZE`(**2560 字节**),所以真实预算是
/// 1280 个 UTF-16 码元 —— 它的报错文案写「longer than platform limit of
/// 2560 chars」,那个 2560 是字节数,极易误读成字符数。
///
/// 飞书的 `user_access_token` 与 `refresh_token` 各自数百字符,两枚加 JSON
/// 外壳必然越界 —— 这正是「登录失败:凭据存储失败」的根因。整块载荷因此
/// 按预算切片分存。取 1000 码元留约 22% 余量(飞书 token 是 base64url,
/// 恒 ASCII;非 ASCII 字段按 `len_utf16` 真实计量,不靠字符数估算)。
const MAX_CHUNK_UTF16: usize = 1000;

/// 分片数上限。1000 × 16 = 16,000 字符,远超任何可能的 token 组合;越界
/// 说明模型出了问题,明确报错而不是静默写坏半套条目。
const MAX_CHUNKS: usize = 16;

/// 分片格式版本 —— 条目跨版本存活,索引带版本号便于日后演进。
const CHUNK_FORMAT_VERSION: u32 = 2;

/// 索引条目的内容:载荷被切成几片。
///
/// 必填字段(`v` / `chunks`)与 `FeishuCredentials` 完全不相交,所以同一
/// 账户名下的**旧式整存 blob** 能靠「解析成索引失败」干净地区分出来
/// (见 `load` 的兼容腿)。
#[derive(Debug, Serialize, Deserialize)]
struct ChunkManifest {
    v: u32,
    chunks: usize,
}

/// 按 UTF-16 预算切片,**绝不切断任何 char**(切断会产出非法 UTF-8,
/// keyring 的 `set_password` 根本存不进去)。纯函数 —— 测试主战场。
///
/// 代理对字符(emoji 等)按 `len_utf16() == 2` 真实计量,故一个字符恰好
/// 跨预算边界时整体挪到下一片,不会算漏。
fn split_utf16_chunks(payload: &str, max_utf16: usize) -> Vec<String> {
    let budget = max_utf16.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_utf16 = 0usize;
    for ch in payload.chars() {
        let ch_utf16 = ch.len_utf16();
        if current_utf16 + ch_utf16 > budget && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_utf16 = 0;
        }
        current.push(ch);
        current_utf16 += ch_utf16;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

impl KeyringCredentialStore {
    /// 生产装配(固定服务名/账户)。
    pub fn new() -> Self {
        Self {
            service: CREDENTIAL_SERVICE.to_string(),
            account: CREDENTIAL_ACCOUNT.to_string(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, String> {
        self.entry_for(&self.account)
    }

    /// 任意账户名下的条目 —— 索引用规范账户名,分片用 `<account>.<i>`。
    fn entry_for(&self, account: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, account).map_err(|e| e.to_string())
    }

    fn chunk_account(&self, index: usize) -> String {
        format!("{}.{index}", self.account)
    }

    /// 删一条条目,NoEntry 视为成功(幂等 —— clear 与 save 的清理腿都靠它)。
    fn delete_entry(&self, account: &str) -> Result<(), String> {
        match self.entry_for(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// 抹掉索引与**全部**可能的分片。上一次写入的片数未知(且可能多于
    /// 这次),所以按上限全扫一遍,避免残留片被新索引误纳。
    fn purge_all(&self) -> Result<(), String> {
        self.delete_entry(&self.account)?;
        for i in 0..MAX_CHUNKS {
            self.delete_entry(&self.chunk_account(i))?;
        }
        Ok(())
    }
}

impl Default for KeyringCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for KeyringCredentialStore {
    /// 写序:**先抹索引 → 写分片 → 最后写索引**。
    ///
    /// 索引的存在即「分片齐全」的承诺,所以它最后落地。中途崩溃时
    /// `load` 读不到索引,退化成「未登录」让用户重新登录 —— 而不是读到
    /// 一个指向半套分片的索引(那才是真正的脏状态)。
    fn save(&self, credentials: &FeishuCredentials) -> Result<(), String> {
        let payload = serde_json::to_string(credentials).map_err(|e| e.to_string())?;
        let chunks = split_utf16_chunks(&payload, MAX_CHUNK_UTF16);
        if chunks.len() > MAX_CHUNKS {
            return Err(format!(
                "凭据载荷异常:{} 个 UTF-16 码元需 {} 片,超过上限 {}",
                payload.encode_utf16().count(),
                chunks.len(),
                MAX_CHUNKS
            ));
        }
        self.purge_all()?;
        for (i, chunk) in chunks.iter().enumerate() {
            self.entry_for(&self.chunk_account(i))?
                .set_password(chunk)
                .map_err(|e| e.to_string())?;
        }
        let manifest = serde_json::to_string(&ChunkManifest {
            v: CHUNK_FORMAT_VERSION,
            chunks: chunks.len(),
        })
        .map_err(|e| e.to_string())?;
        self.entry()?
            .set_password(&manifest)
            .map_err(|e| e.to_string())
    }

    fn load(&self) -> Result<Option<FeishuCredentials>, String> {
        let head = match self.entry()?.get_password() {
            Ok(payload) => payload,
            // 无条目 = 从未登录 / 已登出。
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };

        // 分片格式:索引 + N 片拼回整块载荷。
        if let Ok(manifest) = serde_json::from_str::<ChunkManifest>(&head) {
            if manifest.chunks > MAX_CHUNKS {
                return Err(format!(
                    "凭据索引异常:声明 {} 片,超过上限 {}",
                    manifest.chunks, MAX_CHUNKS
                ));
            }
            let mut payload = String::new();
            for i in 0..manifest.chunks {
                match self.entry_for(&self.chunk_account(i))?.get_password() {
                    Ok(chunk) => payload.push_str(&chunk),
                    // 索引在但分片缺 —— 半套状态(上次写入被打断)。当作
                    // 未登录让用户重登,比抛错卡死登录入口更可用。
                    Err(keyring::Error::NoEntry) => return Ok(None),
                    Err(e) => return Err(e.to_string()),
                }
            }
            return serde_json::from_str(&payload)
                .map(Some)
                .map_err(|e| format!("凭据条目解码失败:{e}"));
        }

        // 旧式整存 blob(短 token 时代、分片格式之前写入的条目)。
        // 条目在但两种格式都解不出 — 模型改版了,报错原文带回,调用方
        // 提示重新登录(Swift StoreError.decodeFailed 对应路径)。
        serde_json::from_str(&head)
            .map(Some)
            .map_err(|e| format!("凭据条目解码失败:{e}"))
    }

    fn clear(&self) -> Result<(), String> {
        self.purge_all()
    }
}

// MARK: - 测试(InMemory 组在 http_client.rs;这里锁 keyring 形状)

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FeishuCredentials {
        FeishuCredentials {
            access_token: "u-access".into(),
            refresh_token: "u-refresh".into(),
            expires_at: 4102444800.0,
            tenant_key: Some("ten_x".into()),
        }
    }

    /// 序列化形状锁死 — 凭据条目跨版本存活,字段名是持久化契约。
    #[test]
    fn credentials_json_shape_is_stable() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert_eq!(
            json,
            r#"{"access_token":"u-access","refresh_token":"u-refresh","expires_at":4102444800.0,"tenant_key":"ten_x"}"#
        );
        let back: FeishuCredentials = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample());
    }

    /// JSON 字段缺省(tenant_key 可缺)— 旧条目兼容。
    #[test]
    fn credentials_decode_tolerates_missing_tenant_key() {
        let back: FeishuCredentials =
            serde_json::from_str(r#"{"access_token":"a","refresh_token":"r","expires_at":1.5}"#)
                .unwrap();
        assert_eq!(back.tenant_key, None);
        assert_eq!(back.expires_at, 1.5);
    }

    // MARK: - 分片(凭据管理器 blob 上限的根因修复)

    /// 真实机器上的报错复现:两枚飞书 token 整存必然越过
    /// `CRED_MAX_CREDENTIAL_BLOB_SIZE`(2560 字节 = 1280 UTF-16 码元)。
    /// 这条锁住「整存会炸」这个前提 —— 前提不成立了分片就该重新评估。
    #[test]
    fn realistic_feishu_payload_exceeds_platform_limit() {
        let creds = FeishuCredentials {
            access_token: "u-".to_string() + &"A".repeat(700),
            refresh_token: "r-".to_string() + &"B".repeat(700),
            expires_at: 4102444800.0,
            tenant_key: Some("ten_abcdef123456".into()),
        };
        let payload = serde_json::to_string(&creds).unwrap();
        // keyring 的实际判据:encode_utf16().count() * 2 > 2560。
        assert!(
            payload.encode_utf16().count() * 2 > 2560,
            "整存载荷应当越界,否则分片前提失效"
        );
        // 分片后每片都在预算内。
        for chunk in split_utf16_chunks(&payload, MAX_CHUNK_UTF16) {
            assert!(chunk.encode_utf16().count() <= MAX_CHUNK_UTF16);
            assert!(chunk.encode_utf16().count() * 2 <= 2560);
        }
    }

    #[test]
    fn split_rejoins_to_original() {
        let payload = "x".repeat(2500);
        let chunks = split_utf16_chunks(&payload, 1000);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), payload);
    }

    #[test]
    fn split_short_payload_is_single_chunk() {
        let chunks = split_utf16_chunks("short", 1000);
        assert_eq!(chunks, vec!["short".to_string()]);
    }

    #[test]
    fn split_exact_multiple_has_no_trailing_empty_chunk() {
        let payload = "y".repeat(2000);
        let chunks = split_utf16_chunks(&payload, 1000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.encode_utf16().count() == 1000));
        assert_eq!(chunks.concat(), payload);
    }

    #[test]
    fn split_empty_payload_yields_no_chunks() {
        assert!(split_utf16_chunks("", 1000).is_empty());
    }

    /// 绝不切断 char:多字节 UTF-8 与代理对都必须整体搬走,否则产出的
    /// 片不是合法 UTF-8 / 丢字。
    #[test]
    fn split_never_splits_a_char() {
        // '中' = 3 UTF-8 字节但 1 个 UTF-16 码元。
        let payload = "中".repeat(5);
        let chunks = split_utf16_chunks(&payload, 2);
        assert_eq!(chunks, vec!["中中", "中中", "中"]);
        assert_eq!(chunks.concat(), payload);

        // emoji 是代理对 = 2 个 UTF-16 码元;预算 3 时一片只塞得下一个。
        let payload = "🎉🎉🎉".to_string();
        let chunks = split_utf16_chunks(&payload, 3);
        assert_eq!(chunks, vec!["🎉", "🎉", "🎉"]);
        assert_eq!(chunks.concat(), payload);
        for chunk in &chunks {
            assert!(chunk.encode_utf16().count() <= 3);
        }
    }

    /// 预算小于单个字符时也不得丢字或 panic(内部 const 不会走到,
    /// 但边界得是定义好的)。
    #[test]
    fn split_budget_smaller_than_char_still_rejoins() {
        let chunks = split_utf16_chunks("🎉ab", 1);
        assert_eq!(chunks.concat(), "🎉ab");
        let chunks = split_utf16_chunks("abc", 0);
        assert_eq!(chunks.concat(), "abc");
    }

    /// 索引与凭据的 JSON 互不可解 —— `load` 就是靠这个区分分片格式和
    /// 旧式整存 blob 的,两者一旦能互相解析,兼容腿就会走错。
    #[test]
    fn manifest_and_credentials_json_are_mutually_unparseable() {
        let manifest_json = serde_json::to_string(&ChunkManifest {
            v: CHUNK_FORMAT_VERSION,
            chunks: 3,
        })
        .unwrap();
        let creds_json = serde_json::to_string(&sample()).unwrap();

        assert!(serde_json::from_str::<FeishuCredentials>(&manifest_json).is_err());
        assert!(serde_json::from_str::<ChunkManifest>(&creds_json).is_err());
        // 索引自身往返正常。
        let back: ChunkManifest = serde_json::from_str(&manifest_json).unwrap();
        assert_eq!(back.chunks, 3);
        assert_eq!(back.v, CHUNK_FORMAT_VERSION);
    }

    /// 分片账户名形状 —— 条目键是持久化契约(Windows 下
    /// `target_name = "{user}.{service}"`,故账户名后缀即条目区分位)。
    #[test]
    fn chunk_account_naming_is_stable() {
        let store = KeyringCredentialStore::new();
        assert_eq!(store.chunk_account(0), "default.0");
        assert_eq!(store.chunk_account(15), "default.15");
    }

    /// 真·凭据管理器探针(不污染生产条目 — 独立服务名)。手动:
    /// `cargo test feishu::credentials -- --ignored --nocapture`
    #[test]
    #[ignore = "写真实凭据管理器,手动跑"]
    fn keyring_store_roundtrip_probe() {
        let store = KeyringCredentialStore {
            service: "com.shampoo.donemd.probe.feishu-oauth".into(),
            account: "probe".into(),
        };
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None, "清空后应为无条目");

        store.save(&sample()).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.as_ref(), Some(&sample()));

        // save 是先删后写 — 二次保存不产生重复条目也不丢数据。
        let mut second = sample();
        second.access_token = "u-access-2".into();
        store.save(&second).unwrap();
        assert_eq!(store.load().unwrap(), Some(second));

        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        // clear 幂等。
        store.clear().unwrap();
    }

    /// 真·凭据管理器的**超限载荷**往返 —— 这是 2026-09-23 报修的那条路径
    /// (整存时 keyring 回 `TooLong("password encoded as UTF-16", 2560)`)。
    /// 手动:`cargo test feishu::credentials -- --ignored --nocapture`
    #[test]
    #[ignore = "写真实凭据管理器,手动跑"]
    fn keyring_store_oversized_payload_roundtrip_probe() {
        let store = KeyringCredentialStore {
            service: "com.shampoo.donemd.probe.feishu-oauth-big".into(),
            account: "probe".into(),
        };
        store.clear().unwrap();

        // 仿真机 token 长度:整存必越界,分片后必须能原样读回。
        let big = FeishuCredentials {
            access_token: "u-".to_string() + &"A".repeat(900),
            refresh_token: "r-".to_string() + &"B".repeat(900),
            expires_at: 4102444800.0,
            tenant_key: Some("ten_probe".into()),
        };
        let payload = serde_json::to_string(&big).unwrap();
        assert!(payload.encode_utf16().count() * 2 > 2560, "探针载荷应当越界");

        store.save(&big).unwrap();
        assert_eq!(store.load().unwrap(), Some(big.clone()));

        // 再存一份**更短**的:旧的多余分片必须被抹掉,不能被新索引误纳。
        let small = sample();
        store.save(&small).unwrap();
        assert_eq!(store.load().unwrap(), Some(small));

        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }
}
