//! Application state shared across the bridge command and the asset protocol.
//!
//! Mirrors the parts of `DonemdDocument` the Windows port needs: the current
//! file location, dirty flag, staged assets for untitled documents, the
//! authoritative heading-fold set, and the outline snapshot. All mutation
//! funnels through the bridge dispatcher on the main thread path.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::ai::prompt::{AiCommand, SelectionContext};
use crate::ai::providers::{AiConfig, Provider};
use crate::markdown::frontmatter::Frontmatter;

/// One heading entry reported by the JS outline extractor (#78).
/// Serialize feeds the `outlineSet` relay to the sidebar webview; the field
/// names match the contract (`{level, text, index}`) — don't rename.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OutlineHeading {
    pub index: i64,
    pub level: i64,
    pub text: String,
}

/// A save that has asked the webview for the live Tiptap JSON and is waiting
/// for the `documentJSON` reply (`requestDocumentJSON` handshake — Tauri
/// commands can't evaluateJavaScript, so the doc comes back as an envelope).
pub struct PendingSave {
    pub request_id: String,
    pub path: PathBuf,
    /// First save of an untitled doc: move staged assets next to the file.
    pub migrate_assets: bool,
    /// Close the window once the write lands (save-in-close-dialog flow).
    pub close_after: bool,
}

pub struct DocumentState {
    /// On-disk path of the open document; `None` while untitled.
    pub file_path: Option<PathBuf>,
    /// Edits since last save — drives the title-bar "•" marker.
    pub dirty: bool,
    /// Staging directory for an untitled document's assets. Migrated next to
    /// the .md file on first save (mirrors `AssetsManager.migrateAssets`).
    pub staging_dir: PathBuf,
    /// Authoritative collapsed-heading ordinal set (single source of truth,
    /// same role as on macOS). Re-broadcast as `applyFold` on every change.
    pub collapsed_headings: Vec<i64>,
    pub outline: Vec<OutlineHeading>,
    pub active_heading: Option<i64>,
    /// Current Tiptap document JSON — what `loadDocument` pushes on
    /// `editorReady`. An empty doc until a file is loaded.
    pub tiptap_doc: serde_json::Value,
    /// Parsed YAML frontmatter of the open document (user keys survive a
    /// save round-trip verbatim even though the editor UI never shows them).
    pub frontmatter: Frontmatter,
    /// In-flight save handshake, if any. A second save request while one is
    /// pending is dropped (menu accelerator + JS fallback can double-fire).
    pub pending_save: Option<PendingSave>,
    /// File passed on the command line (Windows file-association launch),
    /// opened as soon as the editor signals `editorReady`.
    pub pending_open: Option<PathBuf>,
    /// Bumped on every new/open. The source-mirror sync handshake
    /// (`source-sync-<epoch>`) drops replies whose epoch has moved on —
    /// they carry the PREVIOUS document's content (M3.6).
    pub epoch: u64,
    /// A `source-sync` doc fetch is in flight; further `documentChanged`
    /// beats are skipped until the reply lands (it carries latest state).
    pub source_sync_pending: bool,
}

pub struct AppState {
    pub doc: Mutex<DocumentState>,
    /// Non-secret AI config (`%APPDATA%\com.shampoo.donemd\ai-config.json`).
    pub ai_config: Mutex<AiConfig>,
    /// In-flight AI stream + retry state (single in-flight, PRD 27).
    pub ai: Mutex<AiRuntime>,
    /// 飞书同步管理器(M8)— AuthState 状态机 + OAuth 客户端装配。
    /// 全 interior Mutex,自身可跨线程共享,无需再包 Mutex。
    pub feishu: crate::feishu::manager::FeishuSyncManager,
    /// Shared HTTP client for AI provider calls — reused across every stream
    /// and model-list fetch so the connection pool and the schannel trust
    /// store are built once, not per request (M9-B). `reqwest::Client` is an
    /// `Arc` internally, so cloning it out to a `Client` is cheap.
    pub http: reqwest::Client,
}

// MARK: - AI runtime

/// A fully-resolved AI request — everything `start_request` needs. Stored as
/// `last_request` so `aiRetry` replays it verbatim with a fresh streamId.
#[derive(Clone)]
pub struct PendingRequest {
    pub command: AiCommand,
    pub context: SelectionContext,
    pub provider: Provider,
    pub model: String,
}

/// The one in-flight stream. `cancel` fires on ESC (silent teardown); the
/// spawned task selects on it alongside the HTTP stream.
pub struct ActiveStream {
    pub stream_id: String,
    pub cancel: Arc<tokio::sync::Notify>,
}

/// A WholeDocument command waiting on the `documentJSON` handshake (the
/// `ai-doc-`-prefixed requestId routes the reply back to the AI pipeline
/// instead of the save flow).
pub struct PendingDocFetch {
    pub request_id: String,
    pub stream_id: String,
    pub request: PendingRequest,
}

#[derive(Default)]
pub struct AiRuntime {
    pub active: Option<ActiveStream>,
    pub last_request: Option<PendingRequest>,
    pub pending_doc: Option<PendingDocFetch>,
}

/// Fresh per-document staging dir for untitled assets (`%TEMP%/com.shampoo.donemd/Untitled-<uuid>`).
pub fn fresh_staging_dir() -> PathBuf {
    std::env::temp_dir()
        .join("com.shampoo.donemd")
        .join(format!("Untitled-{}", uuid::Uuid::new_v4()))
}

impl AppState {
    pub fn new() -> Self {
        Self::with_pending_open(None)
    }

    pub fn with_pending_open(pending_open: Option<PathBuf>) -> Self {
        Self {
            doc: Mutex::new(DocumentState {
                file_path: None,
                dirty: false,
                staging_dir: fresh_staging_dir(),
                collapsed_headings: Vec::new(),
                outline: Vec::new(),
                active_heading: None,
                tiptap_doc: empty_tiptap_doc(),
                frontmatter: Frontmatter::default(),
                pending_save: None,
                pending_open,
                epoch: 0,
                source_sync_pending: false,
            }),
            ai_config: Mutex::new(AiConfig::load(
                crate::ai::providers::default_config_path(),
            )),
            ai: Mutex::new(AiRuntime::default()),
            feishu: crate::feishu::manager::FeishuSyncManager::new(),
            http: reqwest::Client::new(),
        }
    }
}

/// Tiptap's schema requires `block+`; an empty document is one empty paragraph.
pub fn empty_tiptap_doc() -> serde_json::Value {
    serde_json::json!({
        "type": "doc",
        "content": [{ "type": "paragraph" }]
    })
}
