//! 推送侧图片上传 — `FeishuImageUploadStage.swift` 的移植。
//!
//! [`super::image_download`] 的镜像:遍历待推送的 Tiptap body,把每个**本地
//! 资源**图片的字节上传到飞书 drive,再把 `src` 改写成
//! `feishu://image/<image_token>`。
//!
//! 两侧用同一种 `feishu://image/<token>` 形态,于是拉取 → 编辑 → 推送 →
//! 再拉取构成对称往返:推送后 frontmatter/正文里存的就是飞书 token,下次
//! 拉取时下载 stage 认得它。
//!
//! 结构与下载版一致(预扫 → 顺序上传 → 同步改写),理由同样是避开 async
//! 递归装箱,并让上传顺序与进度回调可预测。
//!
//! **软失败**:单张图上传失败只记账,该节点保留本地 `src`。后果要说清
//! 楚 —— 那张图在飞书侧会缺失(本地文件仍在),所以调用方必须把
//! `failed` 计数展示给用户,不能静默。这与下载侧「保留 feishu:// 引用
//! 等用户重试」不同:推送侧的残留是**本地** URL,飞书那边解不了。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::api::FeishuApi;
use crate::markdown::tiptap;

/// 读取本地资源字节。生产实现按文件名在文档 assets 目录里找
/// (见 [`AssetsImageReader`]);测试注内存实现。
pub trait ImageReader: Send + Sync {
    /// 返回 `(bytes, mime_type, file_name)`。`file_name` 要交给飞书的
    /// 上传接口(它要求 multipart 里带文件名)。
    fn read_asset(&self, src: &str) -> Result<(Vec<u8>, String, String), String>;
}

/// 一次上传阶段的战果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub uploaded_count: usize,
    /// 上传或读盘失败的 src。这些节点仍指向本地资源 —— 飞书侧解不了,
    /// 那张图在远端会缺失,必须告知用户。
    pub failed_srcs: Vec<String>,
}

/// 这个 src 指向本地资源吗?
///
/// 运行期资源 URL 有两种形态(见 `assets::asset_url`):Windows 下是
/// `http://donemd-asset.localhost/<file>`,其他平台是
/// `donemd-asset://<file>`。两种都认,好让同一份文档在两端可互操作。
/// 返回资源文件名。
pub fn local_asset_name(src: &str) -> Option<&str> {
    let scheme = crate::assets::SCHEME;
    // donemd-asset://<file>
    if let Some(rest) = src.strip_prefix(&format!("{scheme}://")) {
        return non_empty(rest);
    }
    // http://donemd-asset.localhost/<file>
    for prefix in [
        format!("http://{scheme}.localhost/"),
        format!("https://{scheme}.localhost/"),
    ] {
        if let Some(rest) = src.strip_prefix(&prefix) {
            return non_empty(rest);
        }
    }
    None
}

fn non_empty(s: &str) -> Option<&str> {
    // 剥掉 query/fragment(资源 URL 通常没有,但别被一个 `?v=2` 绊倒)。
    let clean = s.split(['?', '#']).next().unwrap_or(s);
    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

/// 预扫:按文档顺序收集去重后的本地资源 src。
fn collect_local_srcs(body: &Value) -> Vec<String> {
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    walk_collect(body, &mut ordered, &mut seen);
    ordered
}

fn walk_collect(node: &Value, ordered: &mut Vec<String>, seen: &mut HashSet<String>) {
    if tiptap::node_type(node) == "image" {
        if let Some(src) = tiptap::attr_str(node, "src") {
            if local_asset_name(src).is_some() && seen.insert(src.to_string()) {
                ordered.push(src.to_string());
            }
        }
    }
    for child in tiptap::content(node) {
        walk_collect(child, ordered, seen);
    }
}

/// 同步改写:把 src → `feishu://image/<token>` 的映射应用到 body。
fn rewrite(node: &mut Value, resolved: &HashMap<String, String>) {
    if tiptap::node_type(node) == "image" {
        let replacement = tiptap::attr_str(node, "src")
            .and_then(|src| resolved.get(src))
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

/// 图片上传阶段。对 `A: FeishuApi` 泛型化(AFIT 无 dyn 兼容面);
/// reader 走 `&dyn`(同步 trait,对象安全)。
pub struct ImageUploadStage<'a, A: FeishuApi> {
    api: &'a A,
    reader: &'a dyn ImageReader,
    /// 上传接口要求带 `document_id`(用于 parent_node + drive_route_token,
    /// 缺任一即 403/1061004 —— 真机确认过的硬要求)。
    document_id: String,
}

impl<'a, A: FeishuApi> ImageUploadStage<'a, A> {
    pub fn new(api: &'a A, reader: &'a dyn ImageReader, document_id: impl Into<String>) -> Self {
        Self {
            api,
            reader,
            document_id: document_id.into(),
        }
    }

    /// 走完一次上传阶段。`on_progress(index, total)` 的 `index` 是 1-based
    /// 的成功计数。
    pub async fn process(
        &self,
        body: Value,
        mut on_progress: impl FnMut(usize, usize),
    ) -> (Value, Report) {
        let srcs = collect_local_srcs(&body);
        let total = srcs.len();
        let mut report = Report::default();
        let mut resolved: HashMap<String, String> = HashMap::new();

        for src in srcs {
            let (bytes, mime, file_name) = match self.reader.read_asset(&src) {
                Ok(triple) => triple,
                Err(e) => {
                    eprintln!("[feishu] 读取本地图片失败 src={src}:{e}");
                    report.failed_srcs.push(src);
                    continue;
                }
            };
            match self
                .api
                .upload_image(&bytes, &mime, &file_name, &self.document_id)
                .await
            {
                Ok(token) => {
                    report.uploaded_count += 1;
                    on_progress(report.uploaded_count, total);
                    resolved.insert(src, format!("feishu://image/{token}"));
                }
                Err(e) => {
                    eprintln!("[feishu] 图片上传失败 src={src}:{e}");
                    report.failed_srcs.push(src);
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

/// 生产实现:按资源文件名在当前文档的 assets 目录(或未保存文档的暂存
/// 目录)里找文件。
pub struct AssetsImageReader<'a> {
    pub state: &'a crate::state::AppState,
}

impl ImageReader for AssetsImageReader<'_> {
    fn read_asset(&self, src: &str) -> Result<(Vec<u8>, String, String), String> {
        let name = local_asset_name(src).ok_or_else(|| format!("不是本地资源 URL:{src}"))?;
        let path = crate::assets::stored_file_path(self.state, name)
            .ok_or_else(|| format!("找不到资源文件:{name}"))?;
        let bytes = std::fs::read(&path).map_err(|e| format!("读取 {}: {e}", path.display()))?;
        let mime = crate::assets::mime_type_for_filename(name).to_string();
        Ok((bytes, mime, name.to_string()))
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

    struct MockApi {
        /// file_name → Ok(image_token) / Err
        responses: HashMap<String, Result<String, FeishuApiError>>,
        calls: Mutex<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn with_upload(mut self, file_name: &str, token: &str) -> Self {
            self.responses
                .insert(file_name.to_string(), Ok(token.to_string()));
            self
        }
        fn with_failure(mut self, file_name: &str, err: FeishuApiError) -> Self {
            self.responses.insert(file_name.to_string(), Err(err));
            self
        }
        fn calls_in_order(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn documents_seen(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl FeishuApi for MockApi {
        async fn upload_image(
            &self,
            _data: &[u8],
            _mime_type: &str,
            file_name: &str,
            document_id: &str,
        ) -> Result<String, FeishuApiError> {
            // document_id 必须被带上 —— 缺它飞书回 403/1061004。
            assert!(!document_id.is_empty(), "上传必须带 document_id");
            self.calls.lock().unwrap().push(file_name.to_string());
            match self.responses.get(file_name) {
                Some(Ok(t)) => Ok(t.clone()),
                Some(Err(e)) => Err(e.clone()),
                None => Err(FeishuApiError::NotFound {
                    resource: file_name.to_string(),
                }),
            }
        }
        async fn pull_document(
            &self,
            _d: &str,
        ) -> Result<(Vec<FeishuBlock>, i64), FeishuApiError> {
            unimplemented!("上传阶段不该拉文档")
        }
        async fn push_document(&self, _d: &str, _b: &[FeishuBlock]) -> Result<(), FeishuApiError> {
            unimplemented!()
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
        async fn get_document_revision(&self, _d: &str) -> Result<i64, FeishuApiError> {
            unimplemented!()
        }
    }

    /// 内存 reader:所有 src 都读得到,字节是文件名本身。
    #[derive(Default)]
    struct MemReader {
        missing: Vec<String>,
    }

    impl MemReader {
        fn missing(name: &str) -> Self {
            Self {
                missing: vec![name.to_string()],
            }
        }
    }

    impl ImageReader for MemReader {
        fn read_asset(&self, src: &str) -> Result<(Vec<u8>, String, String), String> {
            let name = local_asset_name(src).ok_or("不是本地资源")?;
            if self.missing.iter().any(|m| m == name) {
                return Err("文件不存在".into());
            }
            Ok((name.as_bytes().to_vec(), "image/png".into(), name.to_string()))
        }
    }

    fn image(src: &str) -> Value {
        json!({ "type": "image", "attrs": { "src": src } })
    }

    fn doc(children: Vec<Value>) -> Value {
        json!({ "type": "doc", "content": children })
    }

    fn local(name: &str) -> String {
        format!("http://donemd-asset.localhost/{name}")
    }

    async fn run(
        body: Value,
        api: &MockApi,
        reader: &MemReader,
    ) -> (Value, Report, Vec<(usize, usize)>) {
        let progress = RefCell::new(Vec::new());
        let stage = ImageUploadStage::new(api, reader, "doxcnDOC");
        let (out, report) = stage
            .process(body, |i, t| progress.borrow_mut().push((i, t)))
            .await;
        (out, report, progress.into_inner())
    }

    // MARK: - 本地资源 URL 判别

    #[test]
    fn recognizes_both_asset_url_forms() {
        // Windows 运行期形态
        assert_eq!(
            local_asset_name("http://donemd-asset.localhost/abc.png"),
            Some("abc.png")
        );
        // 自定义 scheme 形态(macOS / 文档互操作)
        assert_eq!(local_asset_name("donemd-asset://abc.png"), Some("abc.png"));
        // 带 query 也认
        assert_eq!(
            local_asset_name("http://donemd-asset.localhost/abc.png?v=2"),
            Some("abc.png")
        );
    }

    #[test]
    fn rejects_non_local_srcs() {
        // 已经是飞书引用 —— 不该重复上传
        assert_eq!(local_asset_name("feishu://image/tok"), None);
        assert_eq!(local_asset_name("https://example.com/a.png"), None);
        assert_eq!(local_asset_name("http://donemd-asset.localhost/"), None);
        assert_eq!(local_asset_name(""), None);
    }

    // MARK: - 主路径

    #[tokio::test]
    async fn no_local_images_leaves_body_untouched() {
        let body = doc(vec![
            json!({ "type": "paragraph" }),
            // 已是飞书引用,跳过
            image("feishu://image/existing"),
        ]);
        let api = MockApi::new();
        let reader = MemReader::default();
        let (out, report, progress) = run(body.clone(), &api, &reader).await;
        assert_eq!(out, body);
        assert_eq!(report, Report::default());
        assert!(progress.is_empty());
        assert_eq!(api.documents_seen(), 0, "不该发起任何上传");
    }

    #[tokio::test]
    async fn uploads_and_rewrites_to_feishu_reference() {
        let body = doc(vec![image(&local("a.png"))]);
        let api = MockApi::new().with_upload("a.png", "imgTOK1");
        let reader = MemReader::default();
        let (out, report, progress) = run(body, &api, &reader).await;

        assert_eq!(
            tiptap::attr_str(&out["content"][0], "src"),
            Some("feishu://image/imgTOK1"),
            "改写成飞书引用,与拉取侧形态对称"
        );
        assert_eq!(report.uploaded_count, 1);
        assert!(report.failed_srcs.is_empty());
        assert_eq!(progress, vec![(1, 1)]);
    }

    /// 同一张图出现两次只上传一次,但两个节点都要改写。
    #[tokio::test]
    async fn duplicate_src_uploads_once() {
        let body = doc(vec![
            image(&local("dup.png")),
            json!({ "type": "paragraph" }),
            image(&local("dup.png")),
        ]);
        let api = MockApi::new().with_upload("dup.png", "TOKDUP");
        let reader = MemReader::default();
        let (out, report, progress) = run(body, &api, &reader).await;

        assert_eq!(api.calls_in_order(), vec!["dup.png".to_string()]);
        assert_eq!(report.uploaded_count, 1);
        assert_eq!(progress, vec![(1, 1)]);
        assert_eq!(
            tiptap::attr_str(&out["content"][0], "src"),
            Some("feishu://image/TOKDUP")
        );
        assert_eq!(
            tiptap::attr_str(&out["content"][2], "src"),
            Some("feishu://image/TOKDUP")
        );
    }

    #[tokio::test]
    async fn nested_images_are_found() {
        let body = doc(vec![json!({
            "type": "callout",
            "content": [image(&local("deep.png"))]
        })]);
        let api = MockApi::new().with_upload("deep.png", "TOKDEEP");
        let reader = MemReader::default();
        let (out, report, _) = run(body, &api, &reader).await;
        assert_eq!(report.uploaded_count, 1);
        assert_eq!(
            tiptap::attr_str(&out["content"][0]["content"][0], "src"),
            Some("feishu://image/TOKDEEP")
        );
    }

    // MARK: - 软失败

    /// 上传失败保留本地 src 并记账 —— 后果是那张图在飞书侧缺失,
    /// 调用方必须把计数展示给用户。
    #[tokio::test]
    async fn upload_failure_keeps_local_src_and_keeps_going() {
        let body = doc(vec![image(&local("bad.png")), image(&local("good.png"))]);
        let api = MockApi::new()
            .with_failure("bad.png", FeishuApiError::RateLimited)
            .with_upload("good.png", "TOKGOOD");
        let reader = MemReader::default();
        let (out, report, progress) = run(body, &api, &reader).await;

        assert_eq!(
            tiptap::attr_str(&out["content"][0], "src"),
            Some(local("bad.png").as_str()),
            "失败的保留本地 src"
        );
        assert_eq!(
            tiptap::attr_str(&out["content"][1], "src"),
            Some("feishu://image/TOKGOOD"),
            "一张失败不影响其余"
        );
        assert_eq!(report.uploaded_count, 1);
        assert_eq!(report.failed_srcs, vec![local("bad.png")]);
        assert_eq!(progress, vec![(1, 2)]);
    }

    #[tokio::test]
    async fn read_failure_is_also_soft() {
        let body = doc(vec![image(&local("gone.png"))]);
        let api = MockApi::new();
        let reader = MemReader::missing("gone.png");
        let (out, report, _) = run(body.clone(), &api, &reader).await;
        assert_eq!(out, body, "读不到就原样留着");
        assert_eq!(report.uploaded_count, 0);
        assert_eq!(report.failed_srcs, vec![local("gone.png")]);
        assert_eq!(api.documents_seen(), 0, "读盘失败不该发起上传");
    }

    #[tokio::test]
    async fn upload_order_follows_document_order() {
        let body = doc(vec![
            image(&local("1.png")),
            image(&local("2.png")),
            image(&local("3.png")),
        ]);
        let api = MockApi::new()
            .with_upload("1.png", "T1")
            .with_upload("2.png", "T2")
            .with_upload("3.png", "T3");
        let reader = MemReader::default();
        let (_, report, progress) = run(body, &api, &reader).await;
        assert_eq!(report.uploaded_count, 3);
        assert_eq!(progress, vec![(1, 3), (2, 3), (3, 3)]);
        assert_eq!(
            api.calls_in_order(),
            vec!["1.png".to_string(), "2.png".to_string(), "3.png".to_string()]
        );
    }
}
