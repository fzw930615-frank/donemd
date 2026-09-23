//! 推送协调器 — `FeishuPushCoordinator.swift` 的移植(F3-c 第一批)。
//!
//! 编排「把本地 Done.md 文档推到飞书」:
//!   1. 占位块闸门 —— 含占位块直接拒绝(见下)
//!   2. 已绑定则比对 revision;未绑定则 `create_document` 新建远端文档
//!   3. 图片上传 stage:本地资源 → `feishu://image/<token>`
//!   4. `tiptap_to_blocks` 转成飞书块 → `push_document` 覆盖正文
//!   5. 回写 frontmatter:`doc_token`(新建时)、`last_pushed_at`、
//!      **以及推送后重读的 `last_pulled_revision`**
//!
//! # 为什么推送比拉取危险
//!
//! 飞书没有「整体覆盖正文」的端点,唯一写路径是 delete-then-create(先删
//! 根子块再整棵重建)。含占位块(sheet/board/bitable/mindnote —— Done.md
//! 表达不了、只能以占位节点承载的飞书原生块)的文档若走这条路,会**连带
//! 重建那些块**,摧毁其他用户在其上的实时协作数据。
//!
//! 所以第一批**拒绝**推送含占位块的文档([`PushError::ContainsPlaceholderBlocks`]),
//! 与 Swift 的 v2-9a-step3 lite 同样取舍:宁可不能用,不可摧毁远端数据。
//! 解药是段式推送(按占位块切段、只删改非占位段、占位块以
//! `preserve_existing` 引用留住),属第二批。
//!
//! # 两条必须守住的契约
//!
//! **孤儿文档**:`create_document` 成功而 `push_document` 失败时,远端已
//! 存在一篇空文档。此时返回 [`PushError::PartialSuccess`] 并携带
//! `orphaned_doc_token` —— 调用方**必须在任何重试之前**把它写回
//! frontmatter,否则下次推送会再建一篇,双重孤儿。
//!
//! **revision 自增陷阱**:推送成功后远端 revision 会增长。若不把新值回写
//! 到 `last_pulled_revision`,下一次推送的冲突预检会把「自己上次推送造成
//! 的增长」误判为「远端被别人改了」。故推送成功后重读 revision 并回写。

use serde_json::Value;

use super::api::{FeishuApi, FeishuApiError};
use super::cancel::CancellationSignal;
use super::converter;
use super::image_upload::{ImageReader, ImageUploadStage, Report as ImageReport};
use crate::markdown::frontmatter::{self, Frontmatter};
use crate::markdown::tiptap;

/// 推送失败的归因。
#[derive(Debug, Clone, PartialEq)]
pub enum PushError {
    /// `create_document` 失败,或对已绑定文档 `push_document` 失败
    /// (无孤儿需要善后)。
    ApiFailed(FeishuApiError),
    /// 新建远端文档成功 → 推正文失败。远端多了一篇空文档。
    /// **调用方必须先把 token 写回 frontmatter 再重试**。
    PartialSuccess {
        orphaned_doc_token: String,
        underlying: FeishuApiError,
    },
    /// 正文含占位块,拒绝推送(段式推送未实现,delete-then-create 会
    /// 摧毁远端协作数据)。`block_ids` 按文档顺序去重,供对话框文案用。
    ContainsPlaceholderBlocks { block_ids: Vec<String> },
    /// 远端 revision 已领先本地记录 —— 本地编辑期间别人也改了远端,
    /// 直接覆盖会丢他们的改动。调用方应让用户选择(强推 / 先拉取)。
    RemoteAhead {
        local_revision: i64,
        remote_revision: i64,
    },
    /// 用户取消。**在正文写入之前**取消才是干净的;本协调器只在写入前
    /// 的检查点判定,所以取消永远不留半完成状态(段式推送引入后这条
    /// 不再成立,届时需携带已完成段数)。
    Cancelled,
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::ApiFailed(e) => write!(f, "{e}"),
            PushError::PartialSuccess { underlying, .. } => {
                write!(f, "远端文档已创建但正文推送失败:{underlying}")
            }
            PushError::ContainsPlaceholderBlocks { block_ids } => write!(
                f,
                "文档含 {} 个飞书专有块,暂不支持推送",
                block_ids.len()
            ),
            PushError::RemoteAhead {
                local_revision,
                remote_revision,
            } => write!(
                f,
                "飞书侧已更新(本地 revision {local_revision} → 远端 {remote_revision})"
            ),
            PushError::Cancelled => write!(f, "已取消"),
        }
    }
}

/// 粗粒度进度事件,与拉取侧词汇对称。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// 读远端 revision 做冲突预检(仅已绑定文档)。
    CheckingRemote,
    /// 未绑定 → 在飞书侧新建文档。
    CreatingDocument,
    ImageStageStarted { total: usize },
    ImageUploaded { index: usize, total: usize },
    ImageStageFinished,
    /// 正在写正文(delete-then-create)。
    PushingBody,
    Done,
}

/// 一次推送的产物。
#[derive(Debug, Clone, PartialEq)]
pub struct PushResult {
    /// 已盖好 doc_token / last_pushed_at / last_pulled_revision 的
    /// frontmatter,调用方负责连同正文落盘。
    pub updated_frontmatter: Frontmatter,
    /// 本次是否在飞书侧新建了文档(UI 文案要区分「已创建」与「已更新」)。
    pub created_new_document: bool,
    pub image_report: Option<ImageReport>,
}

/// 推送协调器。对 `A: FeishuApi` 泛型化(AFIT 无 dyn 兼容面);
/// reader 走 `Option<&dyn ImageReader>` —— 不接就跳过图片阶段。
pub struct PushCoordinator<'a, A: FeishuApi> {
    api: &'a A,
    image_reader: Option<&'a dyn ImageReader>,
}

impl<'a, A: FeishuApi> PushCoordinator<'a, A> {
    pub fn new(api: &'a A) -> Self {
        Self {
            api,
            image_reader: None,
        }
    }

    pub fn with_image_reader(api: &'a A, reader: &'a dyn ImageReader) -> Self {
        Self {
            api,
            image_reader: Some(reader),
        }
    }

    /// 把 `body` 推到飞书。`existing` 提供绑定信息(doc_token / revision);
    /// `title` 用于新建远端文档时命名。
    ///
    /// `force` 跳过 revision 冲突预检(用户在对话框里选了「仍然覆盖」)。
    pub async fn push(
        &self,
        body: &Value,
        existing: &Frontmatter,
        title: &str,
        force: bool,
        signal: Option<&CancellationSignal>,
        mut on_progress: impl FnMut(Progress),
    ) -> Result<PushResult, PushError> {
        let cancelled = || signal.map(CancellationSignal::is_cancelled).unwrap_or(false);

        if cancelled() {
            return Err(PushError::Cancelled);
        }

        // ① 占位块闸门 —— 最先判,连一个网络请求都不该发。
        let placeholder_ids = collect_placeholder_ids(body);
        if !placeholder_ids.is_empty() {
            return Err(PushError::ContainsPlaceholderBlocks {
                block_ids: placeholder_ids,
            });
        }

        let existing_token = existing
            .feishu
            .as_ref()
            .and_then(|f| f.doc_token.clone())
            .filter(|t| !t.is_empty());

        // ② 已绑定:冲突预检。未绑定:新建远端文档。
        let (doc_token, created_new) = match existing_token {
            Some(token) => {
                if !force {
                    on_progress(Progress::CheckingRemote);
                    let remote = self
                        .api
                        .get_document_revision(&token)
                        .await
                        .map_err(PushError::ApiFailed)?;
                    let local = existing
                        .feishu
                        .as_ref()
                        .and_then(|f| f.last_pulled_revision);
                    // 本地没有记录过 revision 时不拦 —— 无从比较,
                    // 拦住只会让用户卡死在一个没有出路的对话框里。
                    if let Some(local) = local {
                        if remote > local {
                            return Err(PushError::RemoteAhead {
                                local_revision: local,
                                remote_revision: remote,
                            });
                        }
                    }
                }
                (token, false)
            }
            None => {
                if cancelled() {
                    return Err(PushError::Cancelled);
                }
                on_progress(Progress::CreatingDocument);
                let token = self
                    .api
                    .create_document(title, None)
                    .await
                    .map_err(PushError::ApiFailed)?;
                (token, true)
            }
        };

        // 自此起,新建路径上的失败都必须以 PartialSuccess 上浮 ——
        // 远端那篇文档已经存在了。
        let wrap_err = |e: FeishuApiError| -> PushError {
            if created_new {
                PushError::PartialSuccess {
                    orphaned_doc_token: doc_token.clone(),
                    underlying: e,
                }
            } else {
                PushError::ApiFailed(e)
            }
        };

        if cancelled() {
            // 新建路径下取消也留了孤儿 —— 但没有 API 错误可携带,
            // 如实返回 Cancelled,调用方仍会写回 token(见 push_command)。
            return Err(PushError::Cancelled);
        }

        // ③ 图片上传:本地资源 → feishu://image/<token>
        let (body_for_push, image_report) = match self.image_reader {
            Some(reader) => {
                let stage = ImageUploadStage::new(self.api, reader, &doc_token);
                let mut started = false;
                let (rewritten, report) = {
                    let emit = &mut on_progress;
                    stage
                        .process(body.clone(), |index, total| {
                            if !started {
                                emit(Progress::ImageStageStarted { total });
                                started = true;
                            }
                            emit(Progress::ImageUploaded { index, total });
                        })
                        .await
                };
                if !started {
                    on_progress(Progress::ImageStageStarted { total: 0 });
                }
                on_progress(Progress::ImageStageFinished);
                (rewritten, Some(report))
            }
            None => (body.clone(), None),
        };

        if cancelled() {
            return Err(PushError::Cancelled);
        }

        // ④ 转块 + 写正文
        on_progress(Progress::PushingBody);
        let blocks = converter::tiptap_to_blocks(&body_for_push);
        self.api
            .push_document(&doc_token, &blocks)
            .await
            .map_err(wrap_err)?;

        // ⑤ 回写 frontmatter。**重读 revision** —— 推送自身让它增长了,
        // 不回写的话下次推送的冲突预检会把这次增长误判成「别人改了」。
        // 读失败不算推送失败(正文已经写成功了),只是留个 None 让下次
        // 预检跳过比较。
        let new_revision = match self.api.get_document_revision(&doc_token).await {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("[feishu] 推送后重读 revision 失败(不影响推送结果):{e}");
                None
            }
        };

        let updated_frontmatter = merge_after_push(existing, &doc_token, new_revision);
        on_progress(Progress::Done);
        Ok(PushResult {
            updated_frontmatter,
            created_new_document: created_new,
            image_report,
        })
    }
}

// MARK: - frontmatter 回写

/// 盖 `doc_token`、`last_pushed_at`,以及推送后重读到的
/// `last_pulled_revision`。用户字段与未识别的 `feishu.*` 键逐字保留
/// (复用 `frontmatter::merge` 的语义,与拉取侧同一条路径)。
///
/// `placeholder_blocks` 原样带过:推送不改变占位块集合(含占位块的文档
/// 根本走不到这里)。
fn merge_after_push(
    existing: &Frontmatter,
    doc_token: &str,
    new_revision: Option<i64>,
) -> Frontmatter {
    let mut feishu = existing.feishu.clone().unwrap_or_default();
    feishu.doc_token = Some(doc_token.to_string());
    feishu.last_pushed_at = Some(now_iso8601());
    if let Some(rev) = new_revision {
        feishu.last_pulled_revision = Some(rev);
    }
    frontmatter::merge(
        existing,
        Frontmatter {
            feishu: Some(feishu),
            ..Default::default()
        },
    )
}

/// 当前时刻的 ISO-8601(UTC,秒精度)。frontmatter 里 `last_pushed_at` 是
/// 逐字往返的字符串,不做日期运算,所以自己拼即可,不引入 chrono。
fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_utc(now)
}

/// unix 秒 → `YYYY-MM-DDTHH:MM:SSZ`。纯函数,便于测试(不引入时间库)。
fn format_unix_utc(secs: u64) -> String {
    // 1970-01-01 起的天数与当天秒数
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);

    // Howard Hinnant 的 civil_from_days —— 无外部依赖的公历换算。
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// MARK: - 占位块闸门

/// 按文档顺序收集去重的占位块 id。非空即拒绝推送。
fn collect_placeholder_ids(body: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk(body, &mut ids, &mut seen);
    ids
}

fn walk(node: &Value, ids: &mut Vec<String>, seen: &mut std::collections::HashSet<String>) {
    if tiptap::node_type(node) == "feishu_placeholder_block" {
        // 无 id 的占位块同样危险(delete-then-create 照样会重建它),
        // 用一个稳定的占位标记计入,不能因为缺 id 就放行。
        let id = tiptap::attr_str(node, "block_id")
            .filter(|s| !s.is_empty())
            .unwrap_or("<缺少 block_id>");
        if seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
        return;
    }
    for child in tiptap::content(node) {
        walk(child, ids, seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::api::WikiNodeResolution;
    use crate::feishu::block::{FeishuBlock, PagePayload, Payload};
    use crate::markdown::frontmatter::{FeishuFrontmatter, PlaceholderBlockRef};
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // MARK: - mock

    #[derive(Default)]
    struct MockApi {
        /// create_document 的回应
        create: Option<Result<String, FeishuApiError>>,
        /// push_document 的回应
        push: Option<Result<(), FeishuApiError>>,
        /// get_document_revision 依次返回(预检读一次,推送后再读一次)
        revisions: Mutex<Vec<Result<i64, FeishuApiError>>>,
        uploads: HashMap<String, String>,
        log: Mutex<Vec<String>>,
    }

    impl MockApi {
        fn bound(remote_revision: i64, after_push: i64) -> Self {
            Self {
                push: Some(Ok(())),
                revisions: Mutex::new(vec![Ok(remote_revision), Ok(after_push)]),
                ..Default::default()
            }
        }
        fn unbound(new_token: &str, after_push: i64) -> Self {
            Self {
                create: Some(Ok(new_token.to_string())),
                push: Some(Ok(())),
                revisions: Mutex::new(vec![Ok(after_push)]),
                ..Default::default()
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl FeishuApi for MockApi {
        async fn get_document_revision(&self, _d: &str) -> Result<i64, FeishuApiError> {
            self.log.lock().unwrap().push("revision".into());
            let mut q = self.revisions.lock().unwrap();
            if q.is_empty() {
                return Err(FeishuApiError::DecodeFailed("mock 未配置 revision".into()));
            }
            q.remove(0)
        }
        async fn create_document(
            &self,
            title: &str,
            _p: Option<&str>,
        ) -> Result<String, FeishuApiError> {
            self.log.lock().unwrap().push(format!("create:{title}"));
            self.create
                .clone()
                .unwrap_or(Err(FeishuApiError::DecodeFailed("mock 未配置 create".into())))
        }
        async fn push_document(
            &self,
            _d: &str,
            blocks: &[FeishuBlock],
        ) -> Result<(), FeishuApiError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("push:{}blocks", blocks.len()));
            self.push
                .clone()
                .unwrap_or(Err(FeishuApiError::DecodeFailed("mock 未配置 push".into())))
        }
        async fn upload_image(
            &self,
            _data: &[u8],
            _mime: &str,
            file_name: &str,
            _doc: &str,
        ) -> Result<String, FeishuApiError> {
            self.log.lock().unwrap().push(format!("upload:{file_name}"));
            match self.uploads.get(file_name) {
                Some(t) => Ok(t.clone()),
                None => Err(FeishuApiError::NotFound {
                    resource: file_name.into(),
                }),
            }
        }
        async fn pull_document(
            &self,
            _d: &str,
        ) -> Result<(Vec<FeishuBlock>, i64), FeishuApiError> {
            unimplemented!("推送不该拉文档")
        }
        async fn delete_children_range(
            &self,
            _d: &str,
            _p: &str,
            _s: usize,
            _e: usize,
        ) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn insert_children_at(
            &self,
            _d: &str,
            _p: &str,
            _i: i64,
            _b: &[FeishuBlock],
        ) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn update_document_title(&self, _d: &str, _t: &str) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn download_image(&self, _t: &str) -> Result<(Vec<u8>, String), FeishuApiError> {
            unimplemented!()
        }
        async fn resolve_wiki_node(
            &self,
            _t: &str,
        ) -> Result<WikiNodeResolution, FeishuApiError> {
            unimplemented!()
        }
    }

    // MARK: - 夹具

    /// 只有标题与代码块的正文 —— 正是用户的实际文档形态。
    fn simple_body() -> Value {
        json!({ "type": "doc", "content": [
            { "type": "heading", "attrs": { "level": 1 },
              "content": [{ "type": "text", "text": "交付说明" }] },
            { "type": "codeBlock", "attrs": { "language": "rust" },
              "content": [{ "type": "text", "text": "fn main() {}" }] },
        ]})
    }

    fn bound_frontmatter(token: &str, revision: Option<i64>) -> Frontmatter {
        Frontmatter {
            feishu: Some(FeishuFrontmatter {
                doc_token: Some(token.into()),
                last_pulled_revision: revision,
                ..Default::default()
            }),
            has_fence: true,
            ..Default::default()
        }
    }

    async fn push_with(
        api: &MockApi,
        body: &Value,
        fm: &Frontmatter,
        force: bool,
    ) -> (Result<PushResult, PushError>, Vec<Progress>) {
        let events = RefCell::new(Vec::new());
        let coordinator = PushCoordinator::new(api);
        let r = coordinator
            .push(body, fm, "交付说明", force, None, |p| {
                events.borrow_mut().push(p)
            })
            .await;
        (r, events.into_inner())
    }

    // MARK: - 场景 A:本地新建 → 远端新建

    #[tokio::test]
    async fn unbound_document_creates_remote_then_pushes() {
        let api = MockApi::unbound("doxcnNEW", 1);
        let fm = Frontmatter::default();
        let (result, events) = push_with(&api, &simple_body(), &fm, false).await;
        let result = result.expect("应成功");

        assert!(result.created_new_document, "应标记为新建");
        let feishu = result.updated_frontmatter.feishu.unwrap();
        assert_eq!(feishu.doc_token.as_deref(), Some("doxcnNEW"));
        assert_eq!(feishu.last_pulled_revision, Some(1), "回写推送后的 revision");
        assert!(feishu.last_pushed_at.is_some(), "应盖上推送时刻");
        // 未绑定不该做冲突预检
        assert!(!events.contains(&Progress::CheckingRemote));
        assert!(events.contains(&Progress::CreatingDocument));
        assert_eq!(events.last(), Some(&Progress::Done));
        // 标题用于新建远端文档
        assert!(api.log().iter().any(|l| l == "create:交付说明"));
    }

    // MARK: - 场景 B:已绑定 → 覆盖远端

    #[tokio::test]
    async fn bound_document_checks_revision_then_pushes() {
        // 远端 5,本地记录 5 —— 一致,放行
        let api = MockApi::bound(5, 6);
        let fm = bound_frontmatter("doxcnOLD", Some(5));
        let (result, events) = push_with(&api, &simple_body(), &fm, false).await;
        let result = result.expect("应成功");

        assert!(!result.created_new_document);
        assert!(events.contains(&Progress::CheckingRemote));
        assert!(!events.contains(&Progress::CreatingDocument), "已绑定不该新建");
        let feishu = result.updated_frontmatter.feishu.unwrap();
        assert_eq!(feishu.doc_token.as_deref(), Some("doxcnOLD"));
        assert_eq!(feishu.last_pulled_revision, Some(6));
    }

    /// **连带陷阱的回归测试**:推送让远端 revision 从 5 涨到 6。若不回写,
    /// 下次推送预检会把这次自增误判成「别人改了远端」。这条锁死回写。
    #[tokio::test]
    async fn push_writes_back_new_revision_so_next_push_is_not_a_false_conflict() {
        let api = MockApi::bound(5, 6);
        let fm = bound_frontmatter("doxcnOLD", Some(5));
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        let after = result.unwrap().updated_frontmatter;
        assert_eq!(
            after.feishu.as_ref().unwrap().last_pulled_revision,
            Some(6),
            "必须回写推送后的 revision,否则第二次推送必然误报 RemoteAhead"
        );

        // 拿回写后的 frontmatter 再推一次:远端仍是 6,不该误报冲突。
        let api2 = MockApi::bound(6, 7);
        let (result2, _) = push_with(&api2, &simple_body(), &after, false).await;
        assert!(result2.is_ok(), "第二次推送不该被误判为远端领先");
    }

    #[tokio::test]
    async fn remote_ahead_blocks_push() {
        // 远端 9 > 本地记录 5 —— 本地编辑期间别人改了远端
        let api = MockApi::bound(9, 10);
        let fm = bound_frontmatter("doxcnOLD", Some(5));
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        assert_eq!(
            result.unwrap_err(),
            PushError::RemoteAhead {
                local_revision: 5,
                remote_revision: 9
            }
        );
        // 拦下时不该已经写过正文
        assert!(!api.log().iter().any(|l| l.starts_with("push:")));
    }

    #[tokio::test]
    async fn force_skips_conflict_check() {
        let api = MockApi {
            push: Some(Ok(())),
            revisions: Mutex::new(vec![Ok(10)]), // 只剩推送后那次读
            ..Default::default()
        };
        let fm = bound_frontmatter("doxcnOLD", Some(5));
        let (result, events) = push_with(&api, &simple_body(), &fm, true).await;
        assert!(result.is_ok(), "force 应跳过预检直接覆盖");
        assert!(!events.contains(&Progress::CheckingRemote));
    }

    /// 本地从未记录 revision(比如手写 frontmatter 只填了 token)时不拦 ——
    /// 无从比较,拦住只会让用户卡在一个没有出路的对话框里。
    #[tokio::test]
    async fn missing_local_revision_does_not_block() {
        let api = MockApi::bound(7, 8);
        let fm = bound_frontmatter("doxcnOLD", None);
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        assert!(result.is_ok());
    }

    // MARK: - 占位块闸门(数据安全红线)

    #[tokio::test]
    async fn placeholder_blocks_are_refused_before_any_network_call() {
        let body = json!({ "type": "doc", "content": [
            { "type": "feishu_placeholder_block",
              "attrs": { "block_id": "blkSHEET", "type": "sheet" } },
        ]});
        let api = MockApi::bound(1, 2);
        let fm = bound_frontmatter("doxcnOLD", Some(1));
        let (result, events) = push_with(&api, &body, &fm, false).await;

        assert_eq!(
            result.unwrap_err(),
            PushError::ContainsPlaceholderBlocks {
                block_ids: vec!["blkSHEET".to_string()]
            }
        );
        assert!(api.log().is_empty(), "闸门应在任何网络请求之前拦下");
        assert!(events.is_empty(), "拦下时不该发任何进度事件");
    }

    /// force 也**不能**越过占位块闸门 —— 那是数据安全红线,不是用户偏好。
    #[tokio::test]
    async fn force_cannot_bypass_the_placeholder_gate() {
        let body = json!({ "type": "doc", "content": [
            { "type": "feishu_placeholder_block",
              "attrs": { "block_id": "blkBOARD", "type": "board" } },
        ]});
        let api = MockApi::bound(1, 2);
        let fm = bound_frontmatter("doxcnOLD", Some(1));
        let (result, _) = push_with(&api, &body, &fm, true).await;
        assert!(matches!(
            result.unwrap_err(),
            PushError::ContainsPlaceholderBlocks { .. }
        ));
    }

    /// 缺 block_id 的占位块同样危险(delete-then-create 照样重建它),
    /// 不能因为缺 id 就放行。
    #[tokio::test]
    async fn placeholder_without_id_still_blocks() {
        let body = json!({ "type": "doc", "content": [
            { "type": "feishu_placeholder_block", "attrs": { "type": "sheet" } },
        ]});
        let api = MockApi::bound(1, 2);
        let fm = bound_frontmatter("doxcnOLD", Some(1));
        let (result, _) = push_with(&api, &body, &fm, false).await;
        assert!(matches!(
            result.unwrap_err(),
            PushError::ContainsPlaceholderBlocks { .. }
        ));
    }

    #[test]
    fn placeholder_ids_are_deduped_in_document_order() {
        let body = json!({ "type": "doc", "content": [
            { "type": "feishu_placeholder_block", "attrs": { "block_id": "b2" } },
            { "type": "callout", "content": [
                { "type": "feishu_placeholder_block", "attrs": { "block_id": "b1" } },
            ]},
            { "type": "feishu_placeholder_block", "attrs": { "block_id": "b2" } },
        ]});
        assert_eq!(collect_placeholder_ids(&body), vec!["b2", "b1"]);
    }

    // MARK: - 孤儿文档契约

    /// 新建远端成功 → 推正文失败:必须以 PartialSuccess 携带 token 上浮,
    /// 否则调用方无从写回,下次推送会再建一篇。
    #[tokio::test]
    async fn create_then_push_failure_surfaces_orphan_token() {
        let api = MockApi {
            create: Some(Ok("doxcnORPHAN".into())),
            push: Some(Err(FeishuApiError::RateLimited)),
            revisions: Mutex::new(vec![]),
            ..Default::default()
        };
        let fm = Frontmatter::default();
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        assert_eq!(
            result.unwrap_err(),
            PushError::PartialSuccess {
                orphaned_doc_token: "doxcnORPHAN".to_string(),
                underlying: FeishuApiError::RateLimited,
            }
        );
    }

    /// 已绑定文档推正文失败没有孤儿 —— 应是普通 ApiFailed,
    /// 不能误报成 PartialSuccess(那会让 UI 说出「已创建远端文档」的假话)。
    #[tokio::test]
    async fn bound_push_failure_is_plain_api_failure() {
        let api = MockApi {
            push: Some(Err(FeishuApiError::RateLimited)),
            revisions: Mutex::new(vec![Ok(5)]),
            ..Default::default()
        };
        let fm = bound_frontmatter("doxcnOLD", Some(5));
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        assert_eq!(
            result.unwrap_err(),
            PushError::ApiFailed(FeishuApiError::RateLimited)
        );
    }

    /// 推送后重读 revision 失败不该让推送失败 —— 正文已经写成功了。
    #[tokio::test]
    async fn revision_reread_failure_does_not_fail_the_push() {
        let api = MockApi {
            push: Some(Ok(())),
            revisions: Mutex::new(vec![
                Ok(5),
                Err(FeishuApiError::NetworkUnreachable("断网".into())),
            ]),
            ..Default::default()
        };
        let fm = bound_frontmatter("doxcnOLD", Some(5));
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        let result = result.expect("正文已写成功,不该报失败");
        // 读不到就保留原值,下次预检会以旧值比较(保守但不误伤)
        assert_eq!(
            result.updated_frontmatter.feishu.unwrap().last_pulled_revision,
            Some(5)
        );
    }

    // MARK: - frontmatter 回写

    #[tokio::test]
    async fn user_fields_and_unknown_keys_survive_push() {
        let fm = Frontmatter {
            user_fields: vec![("author".into(), "author: 甲\n".into())],
            feishu: Some(FeishuFrontmatter {
                doc_token: Some("doxcnOLD".into()),
                last_pulled_revision: Some(3),
                doc_url: Some("https://mi.feishu.cn/wiki/W123".into()),
                unknown_fields: vec!["future_key: 1\n".into()],
                placeholder_blocks: vec![],
                ..Default::default()
            }),
            feishu_original_index: Some(1),
            has_fence: true,
        };
        let api = MockApi::bound(3, 4);
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        let after = result.unwrap().updated_frontmatter;

        assert_eq!(after.user_fields[0].1, "author: 甲\n");
        assert_eq!(after.feishu_original_index, Some(1));
        let feishu = after.feishu.unwrap();
        assert_eq!(feishu.unknown_fields, vec!["future_key: 1\n".to_string()]);
        assert_eq!(
            feishu.doc_url.as_deref(),
            Some("https://mi.feishu.cn/wiki/W123"),
            "推送不该动 doc_url"
        );
        assert!(after.has_fence);
    }

    /// 占位块索引原样带过(含占位块的文档根本走不到推送,所以这里
    /// 只可能是空表;锁住「不被清掉」的语义)。
    #[tokio::test]
    async fn placeholder_index_is_carried_through_untouched() {
        let mut fm = bound_frontmatter("doxcnOLD", Some(1));
        fm.feishu.as_mut().unwrap().placeholder_blocks = vec![PlaceholderBlockRef {
            block_id: "kept".into(),
            block_type: "sheet".into(),
            title: None,
        }];
        let api = MockApi::bound(1, 2);
        let (result, _) = push_with(&api, &simple_body(), &fm, false).await;
        let feishu = result.unwrap().updated_frontmatter.feishu.unwrap();
        assert_eq!(feishu.placeholder_blocks.len(), 1);
        assert_eq!(feishu.placeholder_blocks[0].block_id, "kept");
    }

    // MARK: - 取消

    #[tokio::test]
    async fn cancel_before_anything_sends_no_request() {
        let api = MockApi::bound(1, 2);
        let fm = bound_frontmatter("doxcnOLD", Some(1));
        let signal = CancellationSignal::new();
        signal.cancel();
        let coordinator = PushCoordinator::new(&api);
        let r = coordinator
            .push(&simple_body(), &fm, "t", false, Some(&signal), |_| {})
            .await;
        assert_eq!(r.unwrap_err(), PushError::Cancelled);
        assert!(api.log().is_empty());
    }

    // MARK: - 时间格式化(纯函数)

    #[test]
    fn unix_to_iso8601_known_vectors() {
        // 向量用独立来源(JS Date.toISOString)核对过。
        assert_eq!(format_unix_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_utc(1_790_661_600), "2026-09-29T06:00:00Z");
        // 闰年边界:2024-02-29
        assert_eq!(format_unix_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        // 世纪边界(2000 是闰年,能抓出 civil_from_days 的 era 处理错误)
        assert_eq!(format_unix_utc(951_868_800), "2000-03-01T00:00:00Z");
    }

    #[test]
    fn push_error_display_is_readable() {
        assert_eq!(PushError::Cancelled.to_string(), "已取消");
        assert!(PushError::ContainsPlaceholderBlocks {
            block_ids: vec!["a".into(), "b".into()]
        }
        .to_string()
        .contains("2 个"));
        assert!(PushError::RemoteAhead {
            local_revision: 1,
            remote_revision: 2
        }
        .to_string()
        .contains("飞书侧已更新"));
    }

    /// 页块载荷存在性的冒烟:确认 tiptap_to_blocks 产出里有 page 根,
    /// 否则 push_document 那边会因缺页块而失败。
    #[test]
    fn converted_blocks_contain_a_page_root() {
        let blocks = converter::tiptap_to_blocks(&simple_body());
        assert!(
            blocks.iter().any(|b| matches!(b.payload, Payload::Page(_))),
            "转换产物必须含唯一 page 根"
        );
        let _ = PagePayload::default();
    }
}
