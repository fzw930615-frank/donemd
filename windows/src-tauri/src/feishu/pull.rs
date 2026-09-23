//! 拉取协调器 — `FeishuPullCoordinator.swift` 的移植。
//!
//! 编排「把一篇飞书文档拉成本地 Done.md 文档」:
//!   1. `api.pull_document` 取块 + revision
//!   2. `converter::blocks_to_tiptap` 转成 Tiptap body(带转换警告)
//!   3. 若接了 writer,走图片阶段把 `feishu://image/<token>` 落成本地资源
//!   4. 扫新 body 收集占位块索引
//!   5. 与既有 frontmatter 合并(用户字段逐字保留,盖 doc_token /
//!      last_pulled_revision / placeholder_blocks)
//!
//! **与 Swift 的一处有意偏离**:Swift 走 blocks → Markdown 文本 →
//! `parseDocument` → body 的往返;这里直接用 `blocks_to_tiptap`。Rust 侧
//! 那个函数的存在就是为本协调器准备的(见其文档注释),而
//! `to_markdown_with_warnings` 本身就是建在它之上的薄壳。少一次
//! 序列化/反解析意味着不会被 M2 记录的 Markdown 规范化(`*`/`+`→`-`、
//! setext→ATX、`_`→`*`、空行折叠)碰到 —— 那些规范化对 Tiptap 结构是
//! 幂等的,但往返还会丢掉「Markdown 语法表达不了的节点」,直达路径
//! 严格地丢得更少。警告集合两条路同源同聚合,故行为一致。
//!
//! 不在本模块内(属 F4):撤销快照、对话框、前置条件检查、菜单接线。

use std::collections::HashSet;

use serde_json::Value;

use super::api::{FeishuApi, FeishuApiError};
use super::cancel::CancellationSignal;
use super::converter::{self, ConversionWarning};
use super::image_download::{ImageDownloadStage, ImageWriter, Report as ImageReport};
use crate::markdown::frontmatter::{self, Frontmatter, PlaceholderBlockRef};
use crate::markdown::tiptap;
use crate::markdown::ParsedDocument;

/// 拉取失败的两种归因。
///
/// 只 `PartialEq` 不 `Eq` —— `FeishuApiError` 本身没有 `Eq`(错误体里带
/// 浮点/不可比字段),跟着它走即可,测试断言用不到全序。
#[derive(Debug, Clone, PartialEq)]
pub enum PullError {
    /// `pull_document` 失败。原样携带 API 错误,好让 UI 层分流
    /// (401 → 重新登录、404 → 「文档已不存在」)。
    ApiFailed(FeishuApiError),
    /// 用户取消。拉取在飞书侧是只读的,取消永远安全 —— 没有半完成
    /// 状态要收拾,本地文档也没被碰(抛在写回之前)。
    Cancelled,
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullError::ApiFailed(e) => write!(f, "{e}"),
            PullError::Cancelled => write!(f, "已取消"),
        }
    }
}

/// 粗粒度进度事件。词汇与推送侧对齐,UI 在两个方向上保持一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// 即将调 `pull_document` 读文档树。
    PullingDocument,
    /// 图片阶段开始。`total == 0` 表示正文里没有飞书图片 —— UI 可以
    /// 整行跳过。**无图片时也会发**这一条(Swift 同此),UI 靠它决定
    /// 是否渲染该行,漏发会让界面缺状态。
    ImageStageStarted { total: usize },
    /// 一张图下载并落盘成功。`index` 从 1 起。
    ImageDownloaded { index: usize, total: usize },
    ImageStageFinished,
    /// 全部完成,`PullResult` 即将返回。
    Done,
}

/// 一次拉取的产物。
#[derive(Debug, Clone, PartialEq)]
pub struct PullResult {
    pub updated_document: ParsedDocument,
    pub warnings: Vec<ConversionWarning>,
    /// `None` 表示没接图片阶段(不关心图片的测试,或 writer 尚不可用的
    /// 路径)。接了则带回下载/失败计数供结果浮层显示。
    pub image_report: Option<ImageReport>,
}

/// 拉取协调器。对 `A: FeishuApi` 泛型化(AFIT 无 dyn 兼容面);
/// writer 走 `Option<&dyn ImageWriter>` —— 同步 trait 对象安全,
/// 「可选图片阶段」因此不必变成第二个类型参数。
pub struct PullCoordinator<'a, A: FeishuApi> {
    api: &'a A,
    image_writer: Option<&'a dyn ImageWriter>,
}

impl<'a, A: FeishuApi> PullCoordinator<'a, A> {
    /// 不接图片阶段(`feishu://image/…` 的 src 原样留着)。
    pub fn new(api: &'a A) -> Self {
        Self {
            api,
            image_writer: None,
        }
    }

    /// 接上图片阶段 —— 生产路径应当走这个,否则拉回来的图片全是碎图。
    pub fn with_image_writer(api: &'a A, writer: &'a dyn ImageWriter) -> Self {
        Self {
            api,
            image_writer: Some(writer),
        }
    }

    /// 拉 `doc_token` 的当前状态并与 `existing` frontmatter 合并。
    ///
    /// 取消只在三个检查点判定:调 API 前、转换前、**图片阶段返回之后**。
    /// 刻意不在图片遍历中途判 —— 半应用的 stage 状态会丢失「哪些已下载、
    /// 哪些还没」的账(Swift 注释明确了这一点,照搬)。
    pub async fn pull(
        &self,
        doc_token: &str,
        existing: Option<&Frontmatter>,
        doc_url: Option<&str>,
        signal: Option<&CancellationSignal>,
        mut on_progress: impl FnMut(Progress),
    ) -> Result<PullResult, PullError> {
        let cancelled = || signal.map(CancellationSignal::is_cancelled).unwrap_or(false);

        // 检查点 1
        if cancelled() {
            return Err(PullError::Cancelled);
        }
        on_progress(Progress::PullingDocument);
        let (blocks, revision_id) = self
            .api
            .pull_document(doc_token)
            .await
            .map_err(PullError::ApiFailed)?;

        // 检查点 2
        if cancelled() {
            return Err(PullError::Cancelled);
        }
        let conversion = converter::blocks_to_tiptap(&blocks);
        let initial_body = conversion.body;

        let (body, image_report) = match self.image_writer {
            Some(writer) => {
                let stage = ImageDownloadStage::new(self.api, writer);
                let mut started = false;
                let (rewritten, report) = {
                    let emit = &mut on_progress;
                    stage
                        .process(initial_body, |index, total| {
                            if !started {
                                emit(Progress::ImageStageStarted { total });
                                started = true;
                            }
                            emit(Progress::ImageDownloaded { index, total });
                        })
                        .await
                };
                // 一张都没成功(含「本来就没图片」)时补发 started,
                // 保证 UI 总能收到阶段开始信号。
                if !started {
                    on_progress(Progress::ImageStageStarted { total: 0 });
                }
                on_progress(Progress::ImageStageFinished);
                // 检查点 3 —— 在 stage 返回之后,不在遍历中途。
                if cancelled() {
                    return Err(PullError::Cancelled);
                }
                (rewritten, Some(report))
            }
            None => (initial_body, None),
        };

        let placeholder_refs = collect_placeholder_refs(&body);
        let merged = merge_frontmatter(existing, doc_token, revision_id, placeholder_refs, doc_url);

        on_progress(Progress::Done);
        Ok(PullResult {
            updated_document: ParsedDocument {
                frontmatter: merged,
                body,
            },
            warnings: conversion.warnings,
            image_report,
        })
    }
}

// MARK: - frontmatter 合并

/// 保留用户字段与未识别的 `feishu.*` 键;盖 `doc_token` 与
/// `last_pulled_revision`;把 `placeholder_blocks` 按新 body **整体重写**。
///
/// 为什么是重写而不是合并:body 已经被整篇替换,拉取前的占位块索引已经
/// 失效,索引必须镜像用户编辑器接下来真正会看到的内容。这与其他
/// frontmatter 字段「逐字往返」的处理方式相反,是推送侧完整性校验的
/// 对称面 —— 两边都认定 frontmatter 索引是「哪些块是占位块」的真源,
/// 且必须与 body 一致。
///
/// `last_pushed_at`、`unknown_fields` 原样带过。`doc_url` 只在调用方给出
/// 时写入:飞书文档 URL 落在租户子域上,无法从 token 反推,只有 URL 导入
/// 路径才知道它 —— 所以**永不**把一个已知 URL 覆盖回 `None`。
fn merge_frontmatter(
    existing: Option<&Frontmatter>,
    doc_token: &str,
    revision_id: i64,
    placeholder_refs: Vec<PlaceholderBlockRef>,
    doc_url: Option<&str>,
) -> Frontmatter {
    let mut feishu = existing
        .and_then(|f| f.feishu.clone())
        .unwrap_or_default();
    feishu.doc_token = Some(doc_token.to_string());
    feishu.last_pulled_revision = Some(revision_id);
    feishu.placeholder_blocks = placeholder_refs;
    if let Some(url) = doc_url {
        feishu.doc_url = Some(url.to_string());
    }

    // `frontmatter::merge` 的语义正是「用户字段逐字保留 + feishu 子树整体
    // 替换 + feishu_original_index 缺省取 user_fields.len() + has_fence
    // 置真」,与 Swift 的 mergeFrontmatter 逐项一致,复用而不重写。
    // has_fence 必须为真:拉取后若产出无 fence 的文件,下一次保存就会
    // 丢掉绑定关系。
    let base = existing.cloned().unwrap_or_default();
    frontmatter::merge(
        &base,
        Frontmatter {
            feishu: Some(feishu),
            ..Default::default()
        },
    )
}

// MARK: - 占位块索引提取

/// 遍历新 body,把每个 `feishu_placeholder_block` 收成
/// `PlaceholderBlockRef`(block_id / type / title —— 与 frontmatter 写回
/// YAML 的字段集一致)。
///
/// 按文档顺序返回,按 `block_id` 去重(理论上外部工具可能写出重复 id,
/// 索引仍应是集合)。
fn collect_placeholder_refs(body: &Value) -> Vec<PlaceholderBlockRef> {
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    walk_for_placeholders(body, &mut refs, &mut seen);
    refs
}

fn walk_for_placeholders(
    node: &Value,
    refs: &mut Vec<PlaceholderBlockRef>,
    seen: &mut HashSet<String>,
) {
    if tiptap::node_type(node) == "feishu_placeholder_block" {
        // 占位块是 Tiptap 原子节点,无论收下还是跳过都不再下探子节点
        // (Swift 两条分支都 return,照搬)。
        let Some(id) = tiptap::attr_str(node, "block_id").filter(|s| !s.is_empty()) else {
            return;
        };
        if !seen.insert(id.to_string()) {
            return;
        }
        refs.push(PlaceholderBlockRef {
            block_id: id.to_string(),
            block_type: tiptap::attr_str(node, "type").unwrap_or_default().to_string(),
            title: tiptap::attr_str(node, "title")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
        return;
    }
    for child in tiptap::content(node) {
        walk_for_placeholders(child, refs, seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::api::WikiNodeResolution;
    use crate::feishu::block::{
        FeishuBlock, ImagePayload, PagePayload, Payload, TextElement, TextPayload, TextRun,
    };
    use crate::markdown::frontmatter::FeishuFrontmatter;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // MARK: - mocks

    /// 内部状态用 `Mutex` 而非 `RefCell`:`FeishuApi` / `ImageWriter` 都要求
    /// `Send + Sync`(整条管线跑在 `async_runtime::spawn` 下),`RefCell` 不是
    /// `Sync`,用 Mutex 就不必写 `unsafe impl Sync`。
    struct MockApi {
        pull: Mutex<Option<Result<(Vec<FeishuBlock>, i64), FeishuApiError>>>,
        images: HashMap<String, Result<(Vec<u8>, String), FeishuApiError>>,
        pull_calls: Mutex<usize>,
    }

    impl MockApi {
        fn pulling(blocks: Vec<FeishuBlock>, revision: i64) -> Self {
            Self {
                pull: Mutex::new(Some(Ok((blocks, revision)))),
                images: HashMap::new(),
                pull_calls: Mutex::new(0),
            }
        }
        fn failing(err: FeishuApiError) -> Self {
            Self {
                pull: Mutex::new(Some(Err(err))),
                images: HashMap::new(),
                pull_calls: Mutex::new(0),
            }
        }
        fn with_image(mut self, token: &str, bytes: &[u8], mime: &str) -> Self {
            self.images
                .insert(token.to_string(), Ok((bytes.to_vec(), mime.to_string())));
            self
        }
        fn pull_call_count(&self) -> usize {
            *self.pull_calls.lock().unwrap()
        }
    }

    impl FeishuApi for MockApi {
        async fn pull_document(
            &self,
            _document_id: &str,
        ) -> Result<(Vec<FeishuBlock>, i64), FeishuApiError> {
            *self.pull_calls.lock().unwrap() += 1;
            self.pull
                .lock()
                .unwrap()
                .clone()
                .expect("MockApi 未配置 pull 响应")
        }
        async fn download_image(
            &self,
            token: &str,
        ) -> Result<(Vec<u8>, String), FeishuApiError> {
            match self.images.get(token) {
                Some(r) => r.clone(),
                None => Err(FeishuApiError::NotFound {
                    resource: token.to_string(),
                }),
            }
        }
        async fn push_document(
            &self,
            _d: &str,
            _b: &[FeishuBlock],
        ) -> Result<(), FeishuApiError> {
            unimplemented!("拉取不该推送")
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
        async fn create_document(
            &self,
            _t: &str,
            _p: Option<&str>,
        ) -> Result<String, FeishuApiError> {
            unimplemented!()
        }
        async fn update_document_title(
            &self,
            _d: &str,
            _t: &str,
        ) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn upload_image(
            &self,
            _d: &[u8],
            _m: &str,
            _f: &str,
            _doc: &str,
        ) -> Result<String, FeishuApiError> {
            unimplemented!()
        }
        async fn resolve_wiki_node(
            &self,
            _t: &str,
        ) -> Result<WikiNodeResolution, FeishuApiError> {
            unimplemented!()
        }
        async fn get_document_revision(&self, _d: &str) -> Result<i64, FeishuApiError> {
            unimplemented!()
        }
    }

    #[derive(Default)]
    struct MemWriter {
        count: Mutex<usize>,
    }

    impl ImageWriter for MemWriter {
        fn write_downloaded_image(&self, _data: &[u8], _mime: &str) -> Result<String, String> {
            let mut c = self.count.lock().unwrap();
            *c += 1;
            Ok(format!("asset://img{c}.png"))
        }
    }

    // MARK: - 夹具

    /// 最小可用块列表:一个 page 根(飞书要求恰有一个)+ 一段文本。
    /// 构造方式对齐 converter.rs 测试里的既有夹具写法。
    fn page_with_text(text: &str) -> Vec<FeishuBlock> {
        vec![
            FeishuBlock {
                block_id: "p".into(),
                parent_id: None,
                children: Some(vec!["blk1".into()]),
                payload: Payload::Page(PagePayload::default()),
            },
            FeishuBlock {
                block_id: "blk1".into(),
                parent_id: Some("p".into()),
                children: None,
                payload: Payload::Text(TextPayload {
                    elements: vec![TextElement::TextRun(TextRun::new(text))],
                }),
            },
        ]
    }

    async fn pull_with(
        api: &MockApi,
        existing: Option<&Frontmatter>,
        doc_url: Option<&str>,
        signal: Option<&CancellationSignal>,
    ) -> (Result<PullResult, PullError>, Vec<Progress>) {
        let events = RefCell::new(Vec::new());
        let coordinator = PullCoordinator::new(api);
        let result = coordinator
            .pull("doxcnTOKEN", existing, doc_url, signal, |p| {
                events.borrow_mut().push(p)
            })
            .await;
        (result, events.into_inner())
    }

    // MARK: - 主路径与错误分流

    #[tokio::test]
    async fn happy_path_stamps_token_and_revision() {
        let api = MockApi::pulling(page_with_text("你好"), 142);
        let (result, events) = pull_with(&api, None, None, None).await;
        let result = result.expect("拉取应成功");

        let feishu = result
            .updated_document
            .frontmatter
            .feishu
            .as_ref()
            .expect("必须盖上 feishu 子树");
        assert_eq!(feishu.doc_token.as_deref(), Some("doxcnTOKEN"));
        assert_eq!(feishu.last_pulled_revision, Some(142));
        // 没接 writer 时不产出 image_report
        assert!(result.image_report.is_none());
        // 进度:无图片阶段
        assert_eq!(events, vec![Progress::PullingDocument, Progress::Done]);
    }

    /// 契约:API 错误原样上浮,好让 UI 分流 401/404。
    #[tokio::test]
    async fn api_failure_surfaces_verbatim() {
        let api = MockApi::failing(FeishuApiError::Unauthorized);
        let (result, events) = pull_with(&api, None, None, None).await;
        assert_eq!(result.unwrap_err(), PullError::ApiFailed(FeishuApiError::Unauthorized));
        // 失败前已经发过 PullingDocument,不该发 Done
        assert_eq!(events, vec![Progress::PullingDocument]);
    }

    // MARK: - 契约 ①:取消只在三个检查点

    #[tokio::test]
    async fn cancel_before_api_call_skips_the_request() {
        let api = MockApi::pulling(page_with_text("x"), 1);
        let signal = CancellationSignal::new();
        signal.cancel();
        let (result, events) = pull_with(&api, None, None, Some(&signal)).await;
        assert_eq!(result.unwrap_err(), PullError::Cancelled);
        assert_eq!(api.pull_call_count(), 0, "检查点 1 应当在调 API 之前");
        assert!(events.is_empty(), "取消在 PullingDocument 之前");
    }

    /// 取消是只读侧的,所以本地文档绝不该被改动 —— 抛在构造
    /// PullResult 之前即满足(没有 Ok 返回就没有东西可写回)。
    #[tokio::test]
    async fn cancel_after_pull_aborts_before_producing_document() {
        let api = MockApi::pulling(page_with_text("x"), 7);
        let signal = CancellationSignal::new();
        // 用一个在 PullingDocument 事件时就翻转的信号模拟「用户在网络
        // 往返期间点了取消」。
        let events = RefCell::new(Vec::new());
        let coordinator = PullCoordinator::new(&api);
        let result = coordinator
            .pull("doxcnTOKEN", None, None, Some(&signal), |p| {
                if p == Progress::PullingDocument {
                    signal.cancel();
                }
                events.borrow_mut().push(p);
            })
            .await;
        assert_eq!(result.unwrap_err(), PullError::Cancelled);
        assert_eq!(api.pull_call_count(), 1, "API 已调用,取消发生在其后");
        assert_eq!(events.into_inner(), vec![Progress::PullingDocument]);
    }

    // MARK: - 契约 ②③:图片阶段事件与软失败

    /// 契约 ③:无图片时**仍要**发 ImageStageStarted{total:0}。
    #[tokio::test]
    async fn image_stage_emits_started_zero_when_no_images() {
        let api = MockApi::pulling(page_with_text("没有图"), 3);
        let writer = MemWriter::default();
        let events = RefCell::new(Vec::new());
        let coordinator = PullCoordinator::with_image_writer(&api, &writer);
        let result = coordinator
            .pull("doxcnTOKEN", None, None, None, |p| {
                events.borrow_mut().push(p)
            })
            .await
            .expect("应成功");

        assert_eq!(
            events.into_inner(),
            vec![
                Progress::PullingDocument,
                Progress::ImageStageStarted { total: 0 },
                Progress::ImageStageFinished,
                Progress::Done,
            ]
        );
        let report = result.image_report.expect("接了 writer 就该有报告");
        assert_eq!(report, ImageReport::default());
    }

    /// 端到端:带一张飞书图片走完整条路 —— 转换出 `feishu://image/<token>`
    /// → 图片阶段下载落盘 → body 里的 src 被改写 → report 与进度事件齐全。
    /// 这条覆盖的是协调器与 stage 的**接线面**(进度事件交织、report 传递),
    /// 单测两边各自正确并不保证接线正确。
    #[tokio::test]
    async fn image_stage_downloads_and_rewrites_end_to_end() {
        let blocks = vec![
            FeishuBlock {
                block_id: "p".into(),
                parent_id: None,
                children: Some(vec!["img1".into()]),
                payload: Payload::Page(PagePayload::default()),
            },
            FeishuBlock {
                block_id: "img1".into(),
                parent_id: Some("p".into()),
                children: None,
                payload: Payload::Image(ImagePayload {
                    token: Some("imgTOK".into()),
                    ..Default::default()
                }),
            },
        ];
        let api = MockApi::pulling(blocks, 11).with_image("imgTOK", b"PNG", "image/png");
        let writer = MemWriter::default();
        let events = RefCell::new(Vec::new());
        let coordinator = PullCoordinator::with_image_writer(&api, &writer);
        let result = coordinator
            .pull("doxcnTOKEN", None, None, None, |p| {
                events.borrow_mut().push(p)
            })
            .await
            .expect("应成功");

        assert_eq!(
            events.into_inner(),
            vec![
                Progress::PullingDocument,
                Progress::ImageStageStarted { total: 1 },
                Progress::ImageDownloaded { index: 1, total: 1 },
                Progress::ImageStageFinished,
                Progress::Done,
            ],
            "阶段开始必须先于第一张下载事件"
        );
        let report = result.image_report.expect("应有报告");
        assert_eq!(report.downloaded_count, 1);
        assert!(report.failed_tokens.is_empty());

        // body 里的 src 已经指向本地资源,不再是 feishu:// 引用。
        let body = &result.updated_document.body;
        let src = find_first_image_src(body).expect("body 里应有 image 节点");
        assert_eq!(src, "asset://img1.png");
    }

    /// 图片下载失败时,拉取仍然成功(契约:单张坏图不中断),坏引用保留。
    #[tokio::test]
    async fn image_failure_does_not_fail_the_pull() {
        let blocks = vec![
            FeishuBlock {
                block_id: "p".into(),
                parent_id: None,
                children: Some(vec!["img1".into()]),
                payload: Payload::Page(PagePayload::default()),
            },
            FeishuBlock {
                block_id: "img1".into(),
                parent_id: Some("p".into()),
                children: None,
                payload: Payload::Image(ImagePayload {
                    token: Some("missing".into()),
                    ..Default::default()
                }),
            },
        ];
        // MockApi 对未配置的 token 回 NotFound
        let api = MockApi::pulling(blocks, 12);
        let writer = MemWriter::default();
        let coordinator = PullCoordinator::with_image_writer(&api, &writer);
        let result = coordinator
            .pull("doxcnTOKEN", None, None, None, |_| {})
            .await
            .expect("单张坏图不该让整次拉取失败");

        let report = result.image_report.expect("应有报告");
        assert_eq!(report.downloaded_count, 0);
        assert_eq!(report.failed_tokens, vec!["missing".to_string()]);
        // 原引用保留,供用户重试
        let src = find_first_image_src(&result.updated_document.body).unwrap();
        assert_eq!(src, "feishu://image/missing");
    }

    /// 递归找第一个 image 节点的 src。
    fn find_first_image_src(node: &Value) -> Option<String> {
        if tiptap::node_type(node) == "image" {
            return tiptap::attr_str(node, "src").map(str::to_string);
        }
        tiptap::content(node).iter().find_map(find_first_image_src)
    }

    // MARK: - 契约 ④:doc_url 永不被覆盖成 None

    #[tokio::test]
    async fn doc_url_is_written_when_supplied() {
        let api = MockApi::pulling(page_with_text("x"), 1);
        let (result, _) = pull_with(&api, None, Some("https://t.feishu.cn/docx/abc"), None).await;
        assert_eq!(
            result.unwrap().updated_document.frontmatter.feishu.unwrap().doc_url.as_deref(),
            Some("https://t.feishu.cn/docx/abc")
        );
    }

    #[tokio::test]
    async fn doc_url_is_never_clobbered_to_none() {
        let existing = Frontmatter {
            feishu: Some(FeishuFrontmatter {
                doc_url: Some("https://t.feishu.cn/docx/known".into()),
                ..Default::default()
            }),
            has_fence: true,
            ..Default::default()
        };
        let api = MockApi::pulling(page_with_text("x"), 9);
        // doc_url = None(普通 token 拉取,不是 URL 导入)
        let (result, _) = pull_with(&api, Some(&existing), None, None).await;
        assert_eq!(
            result.unwrap().updated_document.frontmatter.feishu.unwrap().doc_url.as_deref(),
            Some("https://t.feishu.cn/docx/known"),
            "已知 URL 必须原样带过,不能被 None 覆盖"
        );
    }

    // MARK: - 契约 ⑤:placeholder_blocks 整体重写而非合并

    #[tokio::test]
    async fn placeholder_index_is_rewritten_not_merged() {
        // 拉取前索引里有一条陈旧记录,新 body 里没有它 —— 必须消失。
        let existing = Frontmatter {
            feishu: Some(FeishuFrontmatter {
                placeholder_blocks: vec![PlaceholderBlockRef {
                    block_id: "STALE".into(),
                    block_type: "sheet".into(),
                    title: Some("旧表".into()),
                }],
                ..Default::default()
            }),
            has_fence: true,
            ..Default::default()
        };
        let api = MockApi::pulling(page_with_text("新正文无占位块"), 5);
        let (result, _) = pull_with(&api, Some(&existing), None, None).await;
        let blocks = result.unwrap().updated_document.frontmatter.feishu.unwrap().placeholder_blocks;
        assert!(
            blocks.is_empty(),
            "body 已替换,陈旧索引必须被整体重写掉而非保留"
        );
    }

    // MARK: - 契约 ⑥⑦:has_fence 与 feishu_original_index

    #[tokio::test]
    async fn has_fence_is_always_true_after_pull() {
        let api = MockApi::pulling(page_with_text("x"), 1);
        let (result, _) = pull_with(&api, None, None, None).await;
        assert!(
            result.unwrap().updated_document.frontmatter.has_fence,
            "拉取后必须有 fence,否则下次保存会丢绑定"
        );
    }

    #[tokio::test]
    async fn feishu_index_defaults_to_user_field_count() {
        let existing = Frontmatter {
            user_fields: vec![
                ("title".into(), "title: 甲\n".into()),
                ("tags".into(), "tags: [a]\n".into()),
            ],
            feishu: None,
            feishu_original_index: None,
            has_fence: true,
        };
        let api = MockApi::pulling(page_with_text("x"), 1);
        let (result, _) = pull_with(&api, Some(&existing), None, None).await;
        let fm = result.unwrap().updated_document.frontmatter;
        assert_eq!(fm.feishu_original_index, Some(2), "缺省应取 user_fields 数量");
        // 用户字段逐字保留(契约 ⑨)
        assert_eq!(fm.user_fields.len(), 2);
        assert_eq!(fm.user_fields[0].1, "title: 甲\n");
    }

    // MARK: - 契约 ⑨:用户字段与 unknown 键逐字往返

    #[tokio::test]
    async fn user_fields_and_unknown_feishu_keys_round_trip() {
        let existing = Frontmatter {
            user_fields: vec![("author".into(), "author: 乙\n".into())],
            feishu: Some(FeishuFrontmatter {
                last_pushed_at: Some("2026-05-23T10:23:45+08:00".into()),
                unknown_fields: vec!["future_key: 42\n".into()],
                ..Default::default()
            }),
            feishu_original_index: Some(1),
            has_fence: true,
        };
        let api = MockApi::pulling(page_with_text("x"), 77);
        let (result, _) = pull_with(&api, Some(&existing), None, None).await;
        let fm = result.unwrap().updated_document.frontmatter;

        assert_eq!(fm.user_fields[0].1, "author: 乙\n", "用户字段逐字");
        assert_eq!(fm.feishu_original_index, Some(1), "既有位置不被改写");
        let feishu = fm.feishu.unwrap();
        assert_eq!(
            feishu.last_pushed_at.as_deref(),
            Some("2026-05-23T10:23:45+08:00"),
            "last_pushed_at 是本地上次推送时刻,拉取不该动它"
        );
        assert_eq!(feishu.unknown_fields, vec!["future_key: 42\n".to_string()]);
        // 同时新值已盖上
        assert_eq!(feishu.last_pulled_revision, Some(77));
    }

    // MARK: - 占位块收集(纯函数,直接测)

    fn placeholder(id: &str, kind: &str, title: Option<&str>) -> Value {
        let mut attrs = serde_json::Map::new();
        attrs.insert("block_id".into(), json!(id));
        attrs.insert("type".into(), json!(kind));
        if let Some(t) = title {
            attrs.insert("title".into(), json!(t));
        }
        json!({ "type": "feishu_placeholder_block", "attrs": attrs })
    }

    #[test]
    fn collects_placeholders_in_document_order() {
        let body = json!({ "type": "doc", "content": [
            placeholder("b1", "sheet", Some("Q2 OKR")),
            { "type": "paragraph" },
            placeholder("b2", "board", None),
        ]});
        let refs = collect_placeholder_refs(&body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].block_id, "b1");
        assert_eq!(refs[0].block_type, "sheet");
        assert_eq!(refs[0].title.as_deref(), Some("Q2 OKR"));
        assert_eq!(refs[1].block_id, "b2");
        assert_eq!(refs[1].title, None, "空 title 归一为 None");
    }

    /// 契约 ⑧:按 block_id 去重(外部工具可能写出重复 id)。
    #[test]
    fn dedupes_placeholders_by_block_id() {
        let body = json!({ "type": "doc", "content": [
            placeholder("same", "sheet", Some("首次")),
            placeholder("same", "sheet", Some("重复")),
        ]});
        let refs = collect_placeholder_refs(&body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].title.as_deref(), Some("首次"), "保留首次出现的那条");
    }

    #[test]
    fn skips_placeholders_without_block_id() {
        let body = json!({ "type": "doc", "content": [
            { "type": "feishu_placeholder_block", "attrs": { "type": "sheet" } },
            { "type": "feishu_placeholder_block", "attrs": { "block_id": "", "type": "sheet" } },
        ]});
        assert!(collect_placeholder_refs(&body).is_empty());
    }

    #[test]
    fn finds_nested_placeholders() {
        let body = json!({ "type": "doc", "content": [{
            "type": "callout",
            "content": [placeholder("deep", "bitable", None)]
        }]});
        let refs = collect_placeholder_refs(&body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].block_id, "deep");
    }

    #[test]
    fn pull_error_display_is_user_readable() {
        assert_eq!(PullError::Cancelled.to_string(), "已取消");
        // API 错误透传其自身文案
        let inner = FeishuApiError::Unauthorized;
        assert_eq!(
            PullError::ApiFailed(inner.clone()).to_string(),
            inner.to_string()
        );
    }
}
