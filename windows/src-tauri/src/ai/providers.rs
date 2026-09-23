//! Port of `AIProvider.swift` + `ProviderRegistry.swift` — the provider table
//! plus user-config persistence. UserDefaults on macOS maps to a JSON file at
//! `%APPDATA%/com.shampoo.donemd/ai-config.json` here; API keys never touch
//! this file (they live in Windows Credential Manager via `credentials.rs`).
//! The `configured` presence mirror survives as a boolean per provider, same
//! "no plaintext key on disk" invariant.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The five user-facing provider brands. Three wire protocols underneath:
/// DeepSeek / OpenAI / MiMo share the OpenAI-compatible client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Deepseek,
    Gemini,
    Openai,
    Claude,
    Mimo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolFamily {
    OpenAiCompatible,
    Anthropic,
    Google,
}

pub const ALL_PROVIDERS: [Provider; 5] = [
    Provider::Deepseek,
    Provider::Gemini,
    Provider::Openai,
    Provider::Claude,
    Provider::Mimo,
];

impl Provider {
    pub fn from_id(id: &str) -> Option<Provider> {
        ALL_PROVIDERS.iter().copied().find(|p| p.id() == id)
    }

    pub fn id(self) -> &'static str {
        match self {
            Provider::Deepseek => "deepseek",
            Provider::Gemini => "gemini",
            Provider::Openai => "openai",
            Provider::Claude => "claude",
            Provider::Mimo => "mimo",
        }
    }

    pub fn family(self) -> ProtocolFamily {
        match self {
            Provider::Deepseek | Provider::Openai | Provider::Mimo => ProtocolFamily::OpenAiCompatible,
            Provider::Claude => ProtocolFamily::Anthropic,
            Provider::Gemini => ProtocolFamily::Google,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Deepseek => "DeepSeek",
            Provider::Gemini => "Gemini",
            Provider::Openai => "OpenAI",
            Provider::Claude => "Claude",
            Provider::Mimo => "MiMo",
        }
    }

    /// Official default endpoint; the user's override (Settings 高级) wins.
    pub fn default_endpoint(self) -> &'static str {
        match self {
            Provider::Deepseek => "https://api.deepseek.com",
            Provider::Gemini => "https://generativelanguage.googleapis.com",
            Provider::Openai => "https://api.openai.com",
            Provider::Claude => "https://api.anthropic.com",
            Provider::Mimo => "https://api.xiaomimimo.com",
        }
    }

    /// Fallback model when the user hasn't picked one and the live list
    /// fetch hasn't succeeded (mirrors `AIProvider.fallbackModel`).
    pub fn fallback_model(self) -> &'static str {
        match self {
            Provider::Deepseek => "deepseek-v4-flash",
            Provider::Gemini => "gemini-2.5-flash",
            Provider::Openai => "gpt-4o-mini",
            Provider::Claude => "claude-haiku-4-5",
            Provider::Mimo => "MiMo-V2.5-Pro",
        }
    }

    pub fn is_recommended_default(self) -> bool {
        self == Provider::Deepseek
    }
}

/// Per-provider non-secret config (the secret half is in Credential Manager).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Selected model id; empty → fallback_model.
    #[serde(default)]
    pub model_id: String,
    /// Endpoint override; empty → official default.
    #[serde(default)]
    pub endpoint: String,
    /// Non-secret presence mirror for "has a key" (mirrors macOS
    /// `ai.providers.<id>.configured`) so the UI can render without touching
    /// Credential Manager.
    #[serde(default)]
    pub configured: bool,
    /// Last successfully fetched model list (dropdown source).
    #[serde(default)]
    pub model_list: Vec<String>,
}

/// The whole non-secret AI config — one JSON file, written atomically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(default = "default_provider")]
    pub default_provider: String,
    /// 0..=3, paragraphs of context each side for 改写/转换 commands.
    #[serde(default = "default_context_range")]
    pub context_range: i64,
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    /// Path on disk; skipped in the file itself.
    #[serde(skip)]
    path: PathBuf,
}

fn default_provider() -> String {
    Provider::Deepseek.id().to_string()
}
fn default_context_range() -> i64 {
    1
}

impl AiConfig {
    pub fn load(path: PathBuf) -> AiConfig {
        let cfg = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<AiConfig>(&s).ok())
            .unwrap_or_else(|| AiConfig {
                default_provider: default_provider(),
                context_range: 1,
                providers: Default::default(),
                path: path.clone(),
            });
        AiConfig { path, ..cfg }
    }

    /// Write-temp-then-rename, same crash-safety discipline as document saves.
    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("json.donemd-tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn provider_cfg(&self, p: Provider) -> ProviderConfig {
        self.providers.get(p.id()).cloned().unwrap_or_default()
    }

    pub fn provider_cfg_mut(&mut self, p: Provider) -> &mut ProviderConfig {
        self.providers.entry(p.id().to_string()).or_default()
    }

    pub fn default_provider(&self) -> Provider {
        Provider::from_id(&self.default_provider)
            .unwrap_or(Provider::Deepseek)
    }

    pub fn context_range(&self) -> i64 {
        self.context_range.clamp(0, 3)
    }

    /// Resolved endpoint: user override if set, else official default.
    pub fn endpoint(&self, p: Provider) -> String {
        let o = self.provider_cfg(p).endpoint;
        let o = o.trim();
        if o.is_empty() { p.default_endpoint().to_string() } else { o.to_string() }
    }

    /// Selected model: user's choice if set, else the fallback.
    pub fn selected_model(&self, p: Provider) -> String {
        let m = self.provider_cfg(p).model_id;
        let m = m.trim();
        if m.is_empty() { p.fallback_model().to_string() } else { m.to_string() }
    }

    pub fn is_configured(&self, p: Provider) -> bool {
        self.provider_cfg(p).configured
    }

    /// Configured providers in registry order (the settings + slash-dropdown
    /// listing), plus the default id — the `aiProvidersReply` payload shape.
    pub fn configured_list(&self) -> Vec<(Provider, bool)> {
        ALL_PROVIDERS.iter().map(|p| (*p, self.is_configured(*p))).collect()
    }
}

/// Config file location: `%APPDATA%\com.shampoo.donemd\ai-config.json`
/// (falls back to the temp dir when APPDATA is unset, e.g. tests).
pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("com.shampoo.donemd").join("ai-config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_roundtrip() {
        for p in ALL_PROVIDERS {
            assert_eq!(Provider::from_id(p.id()), Some(p));
        }
        assert_eq!(Provider::from_id("bogus"), None);
    }

    #[test]
    fn defaults_match_swift_fallbacks() {
        let cfg = AiConfig::load(PathBuf::from("nonexistent-dir/cfg.json"));
        assert_eq!(cfg.default_provider(), Provider::Deepseek);
        assert_eq!(cfg.context_range(), 1);
        assert_eq!(cfg.selected_model(Provider::Claude), "claude-haiku-4-5");
        assert_eq!(cfg.endpoint(Provider::Deepseek), "https://api.deepseek.com");
        assert!(!cfg.is_configured(Provider::Openai));
    }

    #[test]
    fn save_and_reload_preserves_fields() {
        let dir = std::env::temp_dir().join(format!("donemd-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("ai-config.json");
        let mut cfg = AiConfig::load(path.clone());
        cfg.default_provider = "claude".into();
        cfg.context_range = 3;
        {
            let p = cfg.provider_cfg_mut(Provider::Deepseek);
            p.configured = true;
            p.model_id = "deepseek-v4-flash".into();
            p.endpoint = "https://proxy.example.com".into();
        }
        cfg.save().unwrap();

        let back = AiConfig::load(path);
        assert_eq!(back.default_provider(), Provider::Claude);
        assert_eq!(back.context_range(), 3);
        assert!(back.is_configured(Provider::Deepseek));
        assert_eq!(back.endpoint(Provider::Deepseek), "https://proxy.example.com");
        // Untouched provider keeps defaults.
        assert_eq!(back.endpoint(Provider::Gemini), "https://generativelanguage.googleapis.com");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_config_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("donemd-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ai-config.json");
        std::fs::write(&path, "{ not json").unwrap();
        let cfg = AiConfig::load(path);
        assert_eq!(cfg.default_provider(), Provider::Deepseek);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_endpoint_and_model_fall_back() {
        let mut cfg = AiConfig::load(PathBuf::from("nonexistent-dir/cfg.json"));
        let p = cfg.provider_cfg_mut(Provider::Mimo);
        p.endpoint = "   ".into();
        p.model_id = String::new();
        assert_eq!(cfg.endpoint(Provider::Mimo), "https://api.xiaomimimo.com");
        assert_eq!(cfg.selected_model(Provider::Mimo), "MiMo-V2.5-Pro");
    }
}
