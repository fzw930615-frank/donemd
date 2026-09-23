//! 拉取侧图片落地 — `FeishuImageDownloadStage.swift` 的移植。
//!
//! 遍历刚转换出的 Tiptap body,把每个 `feishu://image/<token>` 的字节从
//! 飞书 drive 媒体端点取回、交给 [`ImageWriter`] 落盘,再把 `src` 改写成
//! 本地资源 URL。缺这一步,拉回来的文档里每张飞书图片都是碎图 ——
//! WebView 解不了 `feishu://` scheme(Swift 侧 #21 的原始报告)。
//!
//! **与 Swift 的结构差异(行为等价)**:Swift 在递归遍历中途惰性下载;
//! Rust 拆成「预扫收集 → 顺序下载 → 同步改写」三趟。理由是 async 递归
//! 在 Rust 需要手动装箱 future,而三趟结构既避开装箱,又让下载顺序、
//! 去重集合与进度回调序列与 Swift 逐项一致(预扫保持文档顺序)。
//!
//! 去重分两层:**单次拉取内**按 token 去重(同一 token 出现两次只打一次
//! 网络),**跨拉取**由 writer 负责(生产实现是内容 SHA-256 命名,同一张图
//! 第二次拉取不重复写盘)。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::api::FeishuApi;
use crate::markdown::tiptap;

/// 飞书图片在转换产物里的 src 形状。
const FEISHU_IMAGE_PREFIX: &str = "feishu://image/";

/// 把下载到的字节落成本地资源,返回 body 应当引用的 URL。
///
/// 生产实现见 [`AssetsImageWriter`];测试注内存实现。返回 `Err` 时
/// stage **软跳过**该节点(保留原 `src`),不会中断整次拉取。
///
/// `Send + Sync` 是必须的:整条拉取管线跑在 `tauri::async_runtime::spawn`
/// 里,要求 future 为 `Send`,而 future 跨 await 持有 `&dyn ImageWriter`。
pub trait ImageWriter: Send + Sync {
    fn write_downloaded_image(&self, data: &[u8], mime_type: &str) -> Result<String, String>;
}

/// 一次图片阶段的战果,上浮到拉取结果浮层(「已下载 N 张,失败 M 张」)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub downloaded_count: usize,
    /// 取不到的 token(网络 / 404 / 落盘失败)。这些节点的 `src` 保持
    /// `feishu://image/<token>` 原样 —— 用户可以重新拉取,而不是永久
    /// 丢掉引用。
    pub failed_tokens: Vec<String>,
}

/// 从 `feishu://image/<token>` 里抠出 token;其他 src 形态(https://、
/// 已经是本地资源的 `donemd-asset://`)返回 `None`,原样放过。
pub fn feishu_image_token(src: &str) -> Option<&str> {
    let token = src.strip_prefix(FEISHU_IMAGE_PREFIX)?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// 预扫:按**文档顺序**收集去重后的 token。顺序即下载顺序,也即进度
/// 回调顺序 —— 与 Swift 的惰性遍历一致。
fn collect_tokens(body: &Value) -> Vec<String> {
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    walk_collect(body, &mut ordered, &mut seen);
    ordered
}

fn walk_collect(node: &Value, ordered: &mut Vec<String>, seen: &mut HashSet<String>) {
    if tiptap::node_type(node) == "image" {
        if let Some(token) = tiptap::attr_str(node, "src").and_then(feishu_image_token) {
            if seen.insert(token.to_string()) {
                ordered.push(token.to_string());
            }
        }
    }
    for child in tiptap::content(node) {
        walk_collect(child, ordered, seen);
    }
}

/// 同步改写:把 token → 本地 URL 的映射应用到 body。映射里没有的 token
/// (下载或落盘失败过的)保持原 `src` 不动。
fn rewrite(node: &mut Value, resolved: &HashMap<String, String>) {
    if tiptap::node_type(node) == "image" {
        let replacement = tiptap::attr_str(node, "src")
            .and_then(feishu_image_token)
            .and_then(|token| resolved.get(token))
            .cloned();
        if let Some(url) = replacement {
            node["attrs"]["src"] = Value::String(url);
        }
    }
    if let Some(children) = node.get_mut("content").and_then(Value::as_array_mut) {
        for child in children {
            rewrite(child, resolved);
        }
    }
}

/// 图片下载阶段。对 `A: FeishuApi` 泛型化而非持 `Arc<dyn FeishuApi>`——
/// 该 trait 带 async 方法(AFIT),没有 dyn 兼容面(F2 已踩过 E0038)。
///
/// writer 反过来走 `&dyn`:`ImageWriter` 是同步 trait,对象安全,用 dyn
/// 能让拉取协调器把「可选图片阶段」表达成 `Option<&dyn ImageWriter>`,
/// 不必为一个可能不存在的 writer 多背一个类型参数。
pub struct ImageDownloadStage<'a, A: FeishuApi> {
    api: &'a A,
    writer: &'a dyn ImageWriter,
}

impl<'a, A: FeishuApi> ImageDownloadStage<'a, A> {
    pub fn new(api: &'a A, writer: &'a dyn ImageWriter) -> Self {
        Self { api, writer }
    }

    /// 走完一次图片阶段。失败**只记账不抛**——单张坏图不该让整篇拉取
    /// 泡汤,body 照常产出,坏引用留给用户重试。
    ///
    /// `on_progress(index, total)` 的 `index` 是 1-based 的**成功**计数,
    /// `total` 是预扫得到的去重 token 总数(与 Swift 同义)。
    pub async fn process(
        &self,
        body: Value,
        mut on_progress: impl FnMut(usize, usize),
    ) -> (Value, Report) {
        let tokens = collect_tokens(&body);
        let total = tokens.len();
        let mut report = Report::default();
        let mut resolved: HashMap<String, String> = HashMap::new();

        for token in tokens {
            let (bytes, mime) = match self.api.download_image(&token).await {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("[feishu] 图片下载失败 token={token}:{e}");
                    report.failed_tokens.push(token);
                    continue;
                }
            };
            match self.writer.write_downloaded_image(&bytes, &mime) {
                Ok(url) => {
                    report.downloaded_count += 1;
                    on_progress(report.downloaded_count, total);
                    resolved.insert(token, url);
                }
                Err(e) => {
                    eprintln!("[feishu] 图片落盘失败 token={token}:{e}");
                    report.failed_tokens.push(token);
                }
            }
        }

        let mut out = body;
        if !resolved.is_empty() {
            rewrite(&mut out, &resolved);
        }
        (out, report)
    }
}

/// 生产实现:落进当前文档的 assets 目录。`assets::import_asset` 按内容
/// SHA-256 命名,所以跨拉取去重是内容哈希天然带来的 —— 同一张图第二次
/// 拉取命中已存在的文件,不重复写盘。
pub struct AssetsImageWriter<'a> {
    pub state: &'a crate::state::AppState,
}

impl ImageWriter for AssetsImageWriter<'_> {
    fn write_downloaded_image(&self, data: &[u8], mime_type: &str) -> Result<String, String> {
        crate::assets::import_asset(self.state, data, mime_type).map(|(_name, url, _md)| url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::api::{FeishuApiError, WikiNodeResolution};
    use crate::feishu::block::FeishuBlock;
    use serde_json::json;
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// 只有 `download_image` 有意义的 MockApi;其余端点在图片阶段不该
    /// 被碰到,一旦被调用就 panic(比返回假数据更早暴露接线错误)。
    ///
    /// 内部状态用 `Mutex` 而非 `RefCell`:`FeishuApi` 要求 `Send + Sync`,
    /// `RefCell` 不是 `Sync`,用 Mutex 就不必写 `unsafe impl Sync`。
    struct MockApi {
        /// token → Ok((bytes, mime)) 或 Err(错误)
        responses: HashMap<String, Result<(Vec<u8>, String), FeishuApiError>>,
        calls: Mutex<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn with_image(mut self, token: &str, bytes: &[u8], mime: &str) -> Self {
            self.responses.insert(
                token.to_string(),
                Ok((bytes.to_vec(), mime.to_string())),
            );
            self
        }
        fn with_failure(mut self, token: &str, err: FeishuApiError) -> Self {
            self.responses.insert(token.to_string(), Err(err));
            self
        }
        fn call_count(&self, token: &str) -> usize {
            self.calls.lock().unwrap().iter().filter(|t| *t == token).count()
        }
        fn calls_in_order(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl FeishuApi for MockApi {
        async fn download_image(
            &self,
            token: &str,
        ) -> Result<(Vec<u8>, String), FeishuApiError> {
            self.calls.lock().unwrap().push(token.to_string());
            match self.responses.get(token) {
                Some(Ok(pair)) => Ok(pair.clone()),
                Some(Err(e)) => Err(e.clone()),
                None => Err(FeishuApiError::NotFound {
                    resource: token.to_string(),
                }),
            }
        }

        async fn pull_document(
            &self,
            _document_id: &str,
        ) -> Result<(Vec<FeishuBlock>, i64), FeishuApiError> {
            unimplemented!("图片阶段不该拉文档")
        }
        async fn push_document(
            &self,
            _document_id: &str,
            _blocks: &[FeishuBlock],
        ) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn delete_children_range(
            &self,
            _document_id: &str,
            _parent_block_id: &str,
            _start_index: usize,
            _end_index: usize,
        ) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn insert_children_at(
            &self,
            _document_id: &str,
            _parent_block_id: &str,
            _index: i64,
            _blocks: &[FeishuBlock],
        ) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn create_document(
            &self,
            _title: &str,
            _parent_token: Option<&str>,
        ) -> Result<String, FeishuApiError> {
            unimplemented!()
        }
        async fn update_document_title(
            &self,
            _document_id: &str,
            _title: &str,
        ) -> Result<(), FeishuApiError> {
            unimplemented!()
        }
        async fn upload_image(
            &self,
            _data: &[u8],
            _mime_type: &str,
            _file_name: &str,
            _document_id: &str,
        ) -> Result<String, FeishuApiError> {
            unimplemented!()
        }
        async fn resolve_wiki_node(
            &self,
            _token: &str,
        ) -> Result<WikiNodeResolution, FeishuApiError> {
            unimplemented!()
        }
        async fn get_document_revision(
            &self,
            _document_id: &str,
        ) -> Result<i64, FeishuApiError> {
            unimplemented!()
        }
    }

    /// 内存 writer:记录写入顺序,按序号造假 URL。
    /// 用 `Mutex` 而非 `RefCell` —— `ImageWriter` 要求 `Send + Sync`。
    #[derive(Default)]
    struct MemWriter {
        written: Mutex<Vec<(Vec<u8>, String)>>,
        fail: bool,
    }

    impl MemWriter {
        fn failing() -> Self {
            Self {
                fail: true,
                ..Default::default()
            }
        }
        fn nth(&self, i: usize) -> (Vec<u8>, String) {
            self.written.lock().unwrap()[i].clone()
        }
    }

    impl ImageWriter for MemWriter {
        fn write_downloaded_image(&self, data: &[u8], mime_type: &str) -> Result<String, String> {
            if self.fail {
                return Err("磁盘满".into());
            }
            let mut w = self.written.lock().unwrap();
            w.push((data.to_vec(), mime_type.to_string()));
            Ok(format!("asset://img{}.png", w.len()))
        }
    }

    fn image(src: &str) -> Value {
        json!({ "type": "image", "attrs": { "src": src } })
    }

    fn doc(children: Vec<Value>) -> Value {
        json!({ "type": "doc", "content": children })
    }

    async fn run(body: Value, api: &MockApi, writer: &MemWriter) -> (Value, Report, Vec<(usize, usize)>) {
        let progress = RefCell::new(Vec::new());
        let stage = ImageDownloadStage::new(api, writer);
        let (out, report) = stage
            .process(body, |i, t| progress.borrow_mut().push((i, t)))
            .await;
        (out, report, progress.into_inner())
    }

    // MARK: - token 解析

    #[test]
    fn token_parsing() {
        assert_eq!(feishu_image_token("feishu://image/tok1"), Some("tok1"));
        // 空 token 不算
        assert_eq!(feishu_image_token("feishu://image/"), None);
        // 其他 scheme 原样放过
        assert_eq!(feishu_image_token("https://example.com/a.png"), None);
        assert_eq!(feishu_image_token("asset://abc.png"), None);
        assert_eq!(feishu_image_token(""), None);
    }

    // MARK: - 主路径

    #[tokio::test]
    async fn no_images_leaves_body_untouched_and_reports_zero() {
        let body = doc(vec![json!({ "type": "paragraph" })]);
        let api = MockApi::new();
        let writer = MemWriter::default();
        let (out, report, progress) = run(body.clone(), &api, &writer).await;
        assert_eq!(out, body, "无图片时 body 必须逐字不动");
        assert_eq!(report, Report::default());
        assert!(progress.is_empty(), "无图片不该有进度回调");
    }

    #[tokio::test]
    async fn single_image_is_downloaded_and_rewritten() {
        let body = doc(vec![image("feishu://image/tok1")]);
        let api = MockApi::new().with_image("tok1", b"PNGDATA", "image/png");
        let writer = MemWriter::default();
        let (out, report, progress) = run(body, &api, &writer).await;

        assert_eq!(
            tiptap::attr_str(&out["content"][0], "src"),
            Some("asset://img1.png")
        );
        assert_eq!(report.downloaded_count, 1);
        assert!(report.failed_tokens.is_empty());
        assert_eq!(progress, vec![(1, 1)]);
        assert_eq!(writer.nth(0).0, b"PNGDATA".to_vec());
        assert_eq!(writer.nth(0).1, "image/png");
    }

    /// 单次拉取内去重:同一 token 出现两次只打一次网络,但**两个节点都**
    /// 要改写。
    #[tokio::test]
    async fn duplicate_token_downloads_once_but_rewrites_all_nodes() {
        let body = doc(vec![
            image("feishu://image/dup"),
            json!({ "type": "paragraph" }),
            image("feishu://image/dup"),
        ]);
        let api = MockApi::new().with_image("dup", b"X", "image/png");
        let writer = MemWriter::default();
        let (out, report, progress) = run(body, &api, &writer).await;

        assert_eq!(api.call_count("dup"), 1, "同一 token 只该下载一次");
        assert_eq!(report.downloaded_count, 1);
        assert_eq!(progress, vec![(1, 1)], "total 是去重后的数量");
        assert_eq!(
            tiptap::attr_str(&out["content"][0], "src"),
            Some("asset://img1.png")
        );
        assert_eq!(
            tiptap::attr_str(&out["content"][2], "src"),
            Some("asset://img1.png"),
            "第二个节点也必须改写"
        );
    }

    /// 嵌套结构里的图片(callout / 列表 / 表格单元格内)必须被找到。
    #[tokio::test]
    async fn nested_images_are_found_and_rewritten() {
        let body = doc(vec![json!({
            "type": "callout",
            "content": [{
                "type": "bulletList",
                "content": [{
                    "type": "listItem",
                    "content": [image("feishu://image/deep")]
                }]
            }]
        })]);
        let api = MockApi::new().with_image("deep", b"D", "image/jpeg");
        let writer = MemWriter::default();
        let (out, report, _) = run(body, &api, &writer).await;

        assert_eq!(report.downloaded_count, 1);
        let deep = &out["content"][0]["content"][0]["content"][0]["content"][0];
        assert_eq!(tiptap::attr_str(deep, "src"), Some("asset://img1.png"));
    }

    #[tokio::test]
    async fn non_feishu_srcs_are_left_alone() {
        let body = doc(vec![
            image("https://example.com/remote.png"),
            image("asset://already-local.png"),
        ]);
        let api = MockApi::new();
        let writer = MemWriter::default();
        let (out, report, _) = run(body.clone(), &api, &writer).await;
        assert_eq!(out, body);
        assert_eq!(report, Report::default());
    }

    // MARK: - 软失败(核心契约:单张坏图不中断拉取)

    #[tokio::test]
    async fn download_failure_preserves_src_and_keeps_going() {
        let body = doc(vec![
            image("feishu://image/bad"),
            image("feishu://image/good"),
        ]);
        let api = MockApi::new()
            .with_failure("bad", FeishuApiError::NotFound { resource: "bad".into() })
            .with_image("good", b"G", "image/png");
        let writer = MemWriter::default();
        let (out, report, progress) = run(body, &api, &writer).await;

        // 坏的保留原 src,供用户重新拉取
        assert_eq!(
            tiptap::attr_str(&out["content"][0], "src"),
            Some("feishu://image/bad")
        );
        // 好的照常落地 —— 一张坏图不能让整篇泡汤
        assert_eq!(
            tiptap::attr_str(&out["content"][1], "src"),
            Some("asset://img1.png")
        );
        assert_eq!(report.downloaded_count, 1);
        assert_eq!(report.failed_tokens, vec!["bad".to_string()]);
        // 进度只记成功项,但 total 仍是 2
        assert_eq!(progress, vec![(1, 2)]);
    }

    #[tokio::test]
    async fn write_failure_is_also_soft() {
        let body = doc(vec![image("feishu://image/tok1")]);
        let api = MockApi::new().with_image("tok1", b"X", "image/png");
        let writer = MemWriter::failing();
        let (out, report, progress) = run(body, &api, &writer).await;

        assert_eq!(
            tiptap::attr_str(&out["content"][0], "src"),
            Some("feishu://image/tok1"),
            "落盘失败也要保留原引用"
        );
        assert_eq!(report.downloaded_count, 0);
        assert_eq!(report.failed_tokens, vec!["tok1".to_string()]);
        assert!(progress.is_empty(), "落盘失败不算一次成功进度");
    }

    #[tokio::test]
    async fn all_failures_still_return_body() {
        let body = doc(vec![image("feishu://image/a"), image("feishu://image/b")]);
        let api = MockApi::new()
            .with_failure("a", FeishuApiError::NetworkUnreachable("断网".into()))
            .with_failure("b", FeishuApiError::NetworkUnreachable("断网".into()));
        let writer = MemWriter::default();
        let (out, report, _) = run(body.clone(), &api, &writer).await;
        assert_eq!(out, body, "全失败时 body 原样返回,不是空文档");
        assert_eq!(report.downloaded_count, 0);
        assert_eq!(report.failed_tokens, vec!["a".to_string(), "b".to_string()]);
    }

    /// 下载顺序 = 文档顺序(预扫保序),进度序列因此可预测。
    #[tokio::test]
    async fn download_order_follows_document_order() {
        let body = doc(vec![
            image("feishu://image/first"),
            image("feishu://image/second"),
            image("feishu://image/third"),
        ]);
        let api = MockApi::new()
            .with_image("first", b"1", "image/png")
            .with_image("second", b"2", "image/png")
            .with_image("third", b"3", "image/png");
        let writer = MemWriter::default();
        let (_, report, progress) = run(body, &api, &writer).await;

        assert_eq!(report.downloaded_count, 3);
        assert_eq!(progress, vec![(1, 3), (2, 3), (3, 3)]);
        assert_eq!(
            api.calls_in_order(),
            vec!["first".to_string(), "second".to_string(), "third".to_string()]
        );
    }
}
