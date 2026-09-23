//! `FeishuStructuralConverter.swift` 的移植 — 飞书扁平块树
//! (`[FeishuBlock]`,镜像 docx OpenAPI 的 `raw_content`)与本地
//! Markdown 之间的双向、纯函数转换器。
//!
//! 公共入口(与 Swift 同名约定):
//! - [`to_markdown`] / [`to_markdown_with_warnings`] — 飞书 → 本地;
//! - [`to_feishu_blocks`] / [`tiptap_to_blocks`] — 本地 → 飞书。
//!
//! 实现全部经 `markdown` 引擎路由,凡既有解析器/序列化器已按规范形
//! 处理的块类型(ATX 标题、`-` 列表、围栏代码等,见 ADR-0002)免费
//! 获得往返;这里的飞书特有翻译层只是 `[FeishuBlock] ↔ Tiptap JSON`
//! 桥。推送协调器(F3)走 [`tiptap_to_blocks`] 直转活 Tiptap 文档,
//! 绕过 markdown 往返 — 图片上传阶段重写过的块级 `image` 节点
//! (`feishu://image/<token>`)得以原样上 wire,不被解析器的行内图片
//! 扁平化吞掉。
//!
//! 覆盖范围(v2-4c,#46):8 类基础块(标题 ×6 / 段落 / 列表 ×2 /
//! 待办 / 引用 / 代码 / 分割线 / 图片)+ 富格式(callout ×5、GFM
//! 表格、mermaid 走代码块路径)+ 占位块(七类飞书原生块,经
//! `feishu_placeholder_block` Tiptap 节点摆渡)。

use std::collections::HashMap;

use serde_json::{json, Value};

use super::block::{
    CalloutPayload, CodePayload, FeishuBlock, ImagePayload, PagePayload, Payload,
    PlaceholderPayload, PlaceholderSubtype, TablePayload, TextElement, TextElementStyle,
    TextPayload, TextRun,
};
use super::callout::FeishuCalloutType;
use crate::markdown::convert::feishu_placeholder_block;
use crate::markdown::{self, placeholder, tiptap};

// MARK: - 结果与警告

/// 一次「块 → Markdown」转换的结果(Swift `ConversionResult<T>`;
/// Rust 侧值类型只有 String,不做泛型)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionResult {
    pub value: String,
    pub warnings: Vec<ConversionWarning>,
}

/// 转换警告。三类都是「内容有意丢失,用户应被告知」的信号,拉取
/// 结果浮层逐条渲染(Swift `ConversionWarning`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionWarning {
    /// 飞书占位块带了嵌套子块。占位块本身存活;子块被丢弃 —
    /// `feishu_placeholder_block` 是 Tiptap 原子节点(无行内体、无
    /// 子块,ADR-0007 § 已知限制 #1)。携带飞书块 ID + 被丢直接
    /// 子块数,元数据卡徽章订阅此信号。
    NestedContentDroppedInPlaceholder {
        block_id: String,
        dropped_child_count: usize,
    },
    /// 拉取侧:飞书行内 `text_color` / `background_color` 被 Done.md
    /// 的 schema 暂时表达不了(GH #59)。文字内容保留;颜色不保留。
    /// `run_count` 全文档聚合成单条 — 拉取结果浮层报一行
    /// 「N 处文字颜色未保留」,不做逐 run 噪音。
    FeishuInlineColorStripped { run_count: usize },
    /// 拉取侧:表格单元格里装了块级内容(callout / 列表 / 标题 /
    /// 嵌套表 / 图片 …),GFM 表格语法只装得下行内内容。每个受影响
    /// 单元格塌缩为其首个文本块(无文本则空),`cell_count` 全文档
    /// 聚合。PRD 的规避:块级内容放在表格外,或接受损失。
    TableCellBlockContentDropped { cell_count: usize },
}

/// `blocks_to_tiptap` 的结果 — F3 拉取协调器直接消费 Tiptap body
/// (frontmatter 经 `frontmatter::merge` 拼装成 `ParsedDocument`),
/// 警告与 Markdown 序列化路径同源同聚合。
#[derive(Debug, Clone, PartialEq)]
pub struct TiptapConversion {
    pub body: Value,
    pub warnings: Vec<ConversionWarning>,
}

// MARK: - 飞书 → Markdown

/// 把扁平飞书块列表渲染为规范形 Markdown。列表必须恰含一个
/// `page`(block_type=1)根 — 无页块的输入返回空文档的序列化
/// (与 `parse_document("")` 同形)。
///
/// 丢弃转换警告;要浮层展示的调用方(元数据卡「嵌套块丢弃」徽章、
/// 拉取结果浮层)改用 [`to_markdown_with_warnings`]。
pub fn to_markdown(blocks: &[FeishuBlock]) -> String {
    to_markdown_with_warnings(blocks).value
}

/// 同 [`to_markdown`],但一并返回转换警告(已聚合,见
/// [`aggregate_warnings`])。
pub fn to_markdown_with_warnings(blocks: &[FeishuBlock]) -> ConversionResult {
    let conversion = blocks_to_tiptap(blocks);
    let doc = markdown::ParsedDocument {
        frontmatter: Default::default(),
        body: conversion.body,
    };
    ConversionResult {
        value: markdown::serialize(&doc),
        warnings: conversion.warnings,
    }
}

/// 飞书块树 → Tiptap 文档(body)。警告产出顺序与 Swift
/// `toMarkdownWithWarnings` 一致:遍历警告(walk 序)→ 颜色聚合 →
/// 单元格丢块聚合(滚到末尾)。
pub fn blocks_to_tiptap(blocks: &[FeishuBlock]) -> TiptapConversion {
    let mut ctx = ConversionContext::default();
    let body = blocks_to_tiptap_inner(blocks, &mut ctx);
    // 行内颜色剥离在解码边界检出、打在每个 TextRun 的
    // `had_stripped_feishu_color` 上。这里聚合一次成单条警告 —
    // 逐 run 噪音会淹没浮层。放在遍历后,捕获所有块种类
    // (text / heading / 列表 / 表格单元格 / …)的颜色。
    let stripped = count_stripped_color_runs(blocks);
    if stripped > 0 {
        ctx.emit(ConversionWarning::FeishuInlineColorStripped { run_count: stripped });
    }
    TiptapConversion {
        body,
        warnings: aggregate_warnings(ctx.warnings),
    }
}

/// 把同类逐次警告滚成单条汇总。`tableCellBlockContentDropped` 在
/// 遍历中每个受影响单元格发一次;浮层要的是「M 处」而不是 M 行。
/// 其余警告在产出时已各自聚合,原样透传。
fn aggregate_warnings(warnings: Vec<ConversionWarning>) -> Vec<ConversionWarning> {
    let mut rolled: Vec<ConversionWarning> = Vec::new();
    let mut dropped_cell_total = 0;
    for warning in warnings {
        match warning {
            ConversionWarning::TableCellBlockContentDropped { cell_count } => {
                dropped_cell_total += cell_count;
            }
            other => rolled.push(other),
        }
    }
    if dropped_cell_total > 0 {
        rolled.push(ConversionWarning::TableCellBlockContentDropped {
            cell_count: dropped_cell_total,
        });
    }
    rolled
}

/// 数解码器标记过「线上带过 text_color / background_color」的文本
/// run。遍历每个携带 `[TextElement]` 的载荷 — text / heading /
/// quote / 列表项 / 代码围栏 / 表格单元格。占位块无行内;page 载荷
/// 的标题元素**要**数 — 拉取时标题前置为正文 H1。
fn count_stripped_color_runs(blocks: &[FeishuBlock]) -> usize {
    let mut count = 0;
    for block in blocks {
        for element in elements_of(&block.payload) {
            let TextElement::TextRun(run) = element;
            if run.style.had_stripped_feishu_color {
                count += 1;
            }
        }
    }
    count
}

/// 载荷直接携带的行内元素(无行内的载荷返回空切片)。callout /
/// 表格单元格的文字在**子块**上,其行内 run 在遍历别处出现。
fn elements_of(payload: &Payload) -> &[TextElement] {
    match payload {
        Payload::Page(p) => &p.title.elements,
        Payload::Text(t)
        | Payload::Heading { text: t, .. }
        | Payload::Bullet(t)
        | Payload::Ordered(t)
        | Payload::Quote(t) => &t.elements,
        Payload::Todo { text, .. } => &text.elements,
        Payload::Code(c) => &c.elements,
        Payload::Callout(_)
        | Payload::Divider
        | Payload::Image(_)
        | Payload::Table(_)
        | Payload::TableCell
        | Payload::Placeholder(_) => &[],
    }
}

// MARK: - Markdown → 飞书

/// 解析 Markdown 源并发射以合成页块为根的扁平飞书块列表。多数块拿
/// 确定性合成 ID(`blk_000001`,…);`feishu_placeholder_block` 节点
/// 例外 — `block_id` 属性原样保留,它必须与飞书侧块对上,
/// `preserve_existing` 推送才解析得了。生产调用方可在上传成功后
/// 覆写合成 ID;占位块 ID 粘住不动。
pub fn to_feishu_blocks(markdown_text: &str) -> Vec<FeishuBlock> {
    let doc = markdown::parse_document(markdown_text);
    tiptap_to_blocks(&doc.body)
}

/// 直接把 Tiptap 文档转成飞书块,跳过 markdown 往返。推送协调器用
/// 这个入口(理由见模块文档)。
pub fn tiptap_to_blocks(doc: &Value) -> Vec<FeishuBlock> {
    let mut ctx = EmissionContext::default();
    let page_id = ctx.next_id();
    let top_children: Vec<String> = tiptap::content(doc)
        .iter()
        .flat_map(|node| emit(node, &page_id, &mut ctx))
        .collect();
    let page = FeishuBlock {
        block_id: page_id,
        parent_id: None,
        children: if top_children.is_empty() {
            None
        } else {
            Some(top_children)
        },
        payload: Payload::Page(PagePayload::default()),
    };
    let mut out = vec![page];
    out.extend(ctx.emitted);
    out
}

// MARK: - [FeishuBlock] → Tiptap

/// 一次 `[FeishuBlock] → Tiptap` 遍历的可变记账。经 `blocks_to_tiptap`
/// 及其辅助函数一路携带,树深处发的警告不用穿过每层签名冒泡。
#[derive(Default)]
struct ConversionContext {
    warnings: Vec<ConversionWarning>,
}

impl ConversionContext {
    fn emit(&mut self, warning: ConversionWarning) {
        self.warnings.push(warning);
    }
}

fn blocks_to_tiptap_inner(blocks: &[FeishuBlock], ctx: &mut ConversionContext) -> Value {
    if blocks.is_empty() {
        return empty_doc();
    }
    let by_id: HashMap<&str, &FeishuBlock> = blocks
        .iter()
        .map(|b| (b.block_id.as_str(), b))
        .collect();
    let Some(page) = blocks
        .iter()
        .find(|b| matches!(b.payload, Payload::Page(_)))
    else {
        return empty_doc();
    };
    let top_level = children_of(page, &by_id);
    let mut content = render_siblings(&top_level, &by_id, ctx);

    // Notion 式标题绑定(v2-9b step1.5):页块标题是飞书侧文档的权威
    // 标题。拉取时前置为 H1,让它在正文里可编辑;配套的推送侧把首个
    // H1 抽回页块标题(F3 协调器)。空标题不前置 — 不给无标题文档
    // 添一个孤零零的「# 」。
    if let Payload::Page(page_payload) = &page.payload {
        let title_inlines = inline_from(&page_payload.title);
        if !title_inlines.is_empty() {
            let mut heading = tiptap::node_with_content("heading", title_inlines);
            heading["attrs"] = json!({ "level": 1 });
            content.insert(0, heading);
        }
    }

    if content.is_empty() {
        content.push(tiptap::node("paragraph"));
    }
    tiptap::node_with_content("doc", content)
}

fn empty_doc() -> Value {
    tiptap::node_with_content("doc", vec![tiptap::node("paragraph")])
}

/// 解出块的子块(按 `children` ID 顺序,经 by_id 找回;悬空引用
/// 静默跳过 — 测试夹具与容错拉取都会出现)。
fn children_of<'a>(
    block: &'a FeishuBlock,
    by_id: &HashMap<&str, &'a FeishuBlock>,
) -> Vec<&'a FeishuBlock> {
    block
        .children
        .iter()
        .flatten()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .collect()
}

/// 遍历同级块序列,把连续的列表类块(bullet / ordered / todo)归组
/// 进一个 Tiptap 列表容器。
fn render_siblings(
    siblings: &[&FeishuBlock],
    by_id: &HashMap<&str, &FeishuBlock>,
    ctx: &mut ConversionContext,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < siblings.len() {
        if let Some(kind) = list_kind(&siblings[i].payload) {
            let mut run: Vec<&FeishuBlock> = Vec::new();
            while i < siblings.len() && list_kind(&siblings[i].payload) == Some(kind) {
                run.push(siblings[i]);
                i += 1;
            }
            out.push(render_list_container(kind, &run, by_id, ctx));
        } else {
            if let Some(node) = render_single(siblings[i], by_id, ctx) {
                out.push(node);
            }
            i += 1;
        }
    }
    out
}

/// 列表种类(Swift `ListKind`)。`renderSiblings` 只对列表类载荷
/// 查询,恒得 `Some`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Bullet,
    Ordered,
    Todo,
}

fn list_kind(payload: &Payload) -> Option<ListKind> {
    match payload {
        Payload::Bullet(_) => Some(ListKind::Bullet),
        Payload::Ordered(_) => Some(ListKind::Ordered),
        Payload::Todo { .. } => Some(ListKind::Todo),
        _ => None,
    }
}

/// Swift 的 `kind` 参数是 `ListKind?`(nil 兜底 bulletList)— 那是
/// 死分支,调用方恒传非 nil;Rust 直接收非可选。
fn render_list_container(
    kind: ListKind,
    items: &[&FeishuBlock],
    by_id: &HashMap<&str, &FeishuBlock>,
    ctx: &mut ConversionContext,
) -> Value {
    let container_type = match kind {
        ListKind::Bullet => "bulletList",
        ListKind::Ordered => "orderedList",
        ListKind::Todo => "taskList",
    };
    let item_nodes: Vec<Value> = items
        .iter()
        .map(|block| render_list_item(block, by_id, ctx))
        .collect();
    tiptap::node_with_content(container_type, item_nodes)
}

fn render_list_item(
    block: &FeishuBlock,
    by_id: &HashMap<&str, &FeishuBlock>,
    ctx: &mut ConversionContext,
) -> Value {
    // 壳:列表项段落(行内内容)+ 可选 checked;子块(嵌套列表 /
    // 引用等)作为嵌套兄弟追加。
    let (item_type, paragraph, checked) = match &block.payload {
        Payload::Bullet(payload) => ("listItem", paragraph_from(payload), None),
        Payload::Ordered(payload) => ("listItem", paragraph_from(payload), None),
        Payload::Todo { text, done } => {
            ("taskItem", paragraph_from(text), Some(*done))
        }
        // 不可达:调用方已过滤列表类载荷。
        _ => ("listItem", tiptap::node("paragraph"), None),
    };
    let mut content = vec![paragraph];
    let nested = children_of(block, by_id);
    if !nested.is_empty() {
        content.extend(render_siblings(&nested, by_id, ctx));
    }
    let mut item = tiptap::node_with_content(item_type, content);
    if let Some(done) = checked {
        item["attrs"] = json!({ "checked": done });
    }
    item
}

fn render_single(
    block: &FeishuBlock,
    by_id: &HashMap<&str, &FeishuBlock>,
    ctx: &mut ConversionContext,
) -> Option<Value> {
    match &block.payload {
        // 页块只做根;作为子块出现不合规范,丢弃。
        Payload::Page(_) => None,
        Payload::Text(payload) => Some(paragraph_from(payload)),
        Payload::Heading { level, text } => {
            let mut node = tiptap::node_with_content("heading", inline_from(text));
            node["attrs"] = json!({ "level": (*level).clamp(1, 6) });
            Some(node)
        }
        Payload::Quote(payload) => {
            // 飞书引用 = 单块载荷;children 是少见多段落情形的嵌套
            // 引用内容。
            let mut blocks = vec![paragraph_from(payload)];
            let nested = children_of(block, by_id);
            if !nested.is_empty() {
                blocks.extend(render_siblings(&nested, by_id, ctx));
            }
            Some(tiptap::node_with_content("blockquote", blocks))
        }
        Payload::Code(payload) => {
            let mut node = tiptap::node_with_content(
                "codeBlock",
                vec![tiptap::text(&plain_text(&payload.elements), None)],
            );
            if let Some(lang) = payload.language.as_deref().filter(|l| !l.is_empty()) {
                node["attrs"] = json!({ "language": lang });
            }
            Some(node)
        }
        Payload::Divider => Some(tiptap::node("horizontalRule")),
        Payload::Image(payload) => {
            // 优先显式 src;退回 image_token 的 `feishu://` 引用,
            // 让规范形 Markdown 在上传/下载接线前就有意义。
            let src = if let Some(s) = payload.src.as_deref().filter(|s| !s.is_empty()) {
                s.to_string()
            } else if let Some(token) = payload.token.as_deref().filter(|t| !t.is_empty()) {
                format!("feishu://image/{token}")
            } else {
                String::new()
            };
            let mut node = tiptap::node("image");
            node["attrs"] = json!({
                "src": src,
                "alt": payload.alt.clone().unwrap_or_default(),
            });
            Some(node)
        }
        Payload::Callout(payload) => {
            let callout_type =
                FeishuCalloutType::from(payload.emoji.as_deref(), payload.background_color.as_deref());
            // 子块:段落 / 标题 / 列表 / 嵌套引用。禁入子块(代码块 /
            // 表格 / 分割线 / 图片)在此丢弃,Tiptap callout schema 才
            // 保持合法;与 ASTConverter 的过滤同表。
            let disallowed = ["codeBlock", "table", "horizontalRule", "image"];
            let rendered: Vec<Value> = render_siblings(&children_of(block, by_id), by_id, ctx)
                .into_iter()
                .filter(|node| !disallowed.contains(&tiptap::node_type(node)))
                .collect();
            // Tiptap callout schema 要求 `(paragraph | …)+`。空体播一个
            // 空段落(与 ASTConverter 一致)。
            let content = if rendered.is_empty() {
                vec![tiptap::node("paragraph")]
            } else {
                rendered
            };
            let mut node = tiptap::node_with_content("callout", content);
            node["attrs"] = json!({ "type": callout_type.as_str() });
            Some(node)
        }
        Payload::Table(payload) => Some(render_table(block, payload, by_id, ctx)),
        // 单元格只在表格子块语境下有意义,单出即丢。
        Payload::TableCell => None,
        Payload::Placeholder(payload) if payload.subtype == PlaceholderSubtype::Video => {
            // 视频:`view` 块(block_type 33)包着一个携带真实 token +
            // 文件名的 `file` 子块(block_type 23,见解码器 case 33)。
            // 在这里 — by_id 可用 — 吸收子块,视频才能以单张飞书视频
            // 卡活过拉取。被吸收的 file 子块**不是**内容损失,有意
            // 跳过 nestedContentDroppedInPlaceholder 警告。
            let file_child = children_of(block, by_id).into_iter().find(|child| {
                matches!(
                    &child.payload,
                    Payload::Placeholder(cp) if cp.subtype == PlaceholderSubtype::Attachment
                )
            });
            if let Some(Payload::Placeholder(file_payload)) = file_child.map(|c| &c.payload) {
                let enriched = PlaceholderPayload {
                    subtype: PlaceholderSubtype::Video,
                    block_token: file_payload.block_token.clone(),
                    title: file_payload.title.clone(),
                    summary: payload.summary.clone(),
                    url: file_payload
                        .block_token
                        .as_deref()
                        .map(|token| format!("feishu://video/{token}"))
                        .unwrap_or_else(|| payload.url.clone()),
                    // Swift init 默认值:created nil、unknownFields 空。
                    created_in_feishu_at: None,
                    unknown_fields: Vec::new(),
                };
                return Some(placeholder_node(&block.block_id, &enriched));
            }
            // 无 file 子块(意外形状)— 发裸视频卡,而不是对并没有
            // 丢内容的子块误报「丢弃」警告。
            Some(placeholder_node(&block.block_id, payload))
        }
        Payload::Placeholder(payload) => {
            // 原子节点 — 无行内内容、无子块。飞书偶尔(且合法地)在
            // sheet / mindnote / bitable 里嵌块;ADR-0007 § 已知限制
            // #1 明确接受此处丢弃,并发警告(元数据卡「⚠️ 该文档含
            // 嵌套块,部分内容仅在飞书可见」徽章)。
            let dropped_child_count = block.children.as_ref().map_or(0, Vec::len);
            if dropped_child_count > 0 {
                ctx.emit(ConversionWarning::NestedContentDroppedInPlaceholder {
                    block_id: block.block_id.clone(),
                    dropped_child_count,
                });
            }
            Some(placeholder_node(&block.block_id, payload))
        }
        // renderSiblings 已提前过滤;不可达。
        Payload::Bullet(_) | Payload::Ordered(_) | Payload::Todo { .. } => None,
    }
}

/// 从飞书载荷构建 `feishu_placeholder_block` Tiptap 节点。字段集合 +
/// 顺序复用 `markdown/convert.rs` 的 `feishu_placeholder_block()` —
/// 魔法注释输入与飞书 API 输入两条路径落在同一节点形状上,round
/// trip 经过完全相同的 Tiptap 状态。
fn placeholder_node(block_id: &str, payload: &PlaceholderPayload) -> Value {
    let adapted = placeholder::FeishuPlaceholder {
        block_type: payload.subtype.as_str().to_string(),
        block_id: block_id.to_string(),
        block_token: payload.block_token.clone(),
        title: payload.title.clone(),
        summary: payload.summary.clone(),
        url: payload.url.clone(),
        created_in_feishu_at: payload.created_in_feishu_at.clone(),
        unknown_fields: payload.unknown_fields.clone(),
    };
    feishu_placeholder_block(&adapted)
}

/// 飞书表格块 → Tiptap `table` 节点。`header_row` 为真时首行渲染成
/// `tableHeader`(v2 恒真 — GFM 必须有表头)。单元格子块塌缩为单个
/// `paragraph`,GFM 表格单元格装不下块级内容;富单元格是已知限制。
fn render_table(
    block: &FeishuBlock,
    payload: &TablePayload,
    by_id: &HashMap<&str, &FeishuBlock>,
    ctx: &mut ConversionContext,
) -> Value {
    let cell_ids: Vec<&str> = block
        .children
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let expected = payload.row_size.saturating_mul(payload.column_size);
    // 防御:飞书声称 rowSize × columnSize 但实际少发单元格(损坏输入)
    // 时补齐/截断,绝不越界读。
    let mut cells: Vec<Value> = Vec::with_capacity(expected);
    for i in 0..expected {
        let cell_block = cell_ids
            .get(i)
            .and_then(|id| by_id.get(*id).copied());
        cells.push(render_table_cell(cell_block, by_id, ctx));
    }

    let cols = payload.column_size.max(1);
    let mut rows: Vec<Value> = Vec::new();
    for r in 0..payload.row_size.max(1) {
        let start = (r * cols).min(cells.len());
        let end = ((r + 1) * cols).min(cells.len());
        let row_slice = &cells[start..end];
        let is_header_row = payload.header_row && r == 0;
        let cell_type = if is_header_row {
            "tableHeader"
        } else {
            "tableCell"
        };
        let typed_cells: Vec<Value> = row_slice
            .iter()
            .map(|cell| {
                let mut node = tiptap::node(cell_type);
                if let Some(content) = cell.get("content") {
                    node["content"] = content.clone();
                }
                node
            })
            .collect();
        rows.push(tiptap::node_with_content("tableRow", typed_cells));
    }
    tiptap::node_with_content("table", rows)
}

fn render_table_cell(
    block: Option<&FeishuBlock>,
    by_id: &HashMap<&str, &FeishuBlock>,
    ctx: &mut ConversionContext,
) -> Value {
    let Some(block) = block else {
        return tiptap::node_with_content("tableCell", vec![tiptap::node("paragraph")]);
    };
    // GFM 表格单元格只装行内内容:塌缩到首个段落(实际是首个文本
    // 块)的行内内容。单元格带非 `.text` 块级子块(callout / 列表 /
    // 标题 / 嵌套表 / 图片)时那些被丢弃 — 每个受影响单元格发一条
    // 警告(不是每个被丢子块一条,一个单元格可能有很多)。
    let child_blocks = children_of(block, by_id);
    let has_non_text_block = child_blocks
        .iter()
        .any(|b| !matches!(b.payload, Payload::Text(_)));
    if has_non_text_block {
        ctx.emit(ConversionWarning::TableCellBlockContentDropped { cell_count: 1 });
    }
    let first_text = child_blocks
        .iter()
        .find(|b| matches!(b.payload, Payload::Text(_)));
    if let Some(Payload::Text(payload)) = first_text.map(|b| &b.payload) {
        let inlines = inline_from(payload);
        let paragraph = if inlines.is_empty() {
            tiptap::node("paragraph")
        } else {
            tiptap::node_with_content("paragraph", inlines)
        };
        return tiptap::node_with_content("tableCell", vec![paragraph]);
    }
    tiptap::node_with_content("tableCell", vec![tiptap::node("paragraph")])
}

fn paragraph_from(payload: &TextPayload) -> Value {
    let inlines = inline_from(payload);
    if inlines.is_empty() {
        // 空行内 → 无 content 键(None ⇒ 键缺失,从不是 null)。
        tiptap::node("paragraph")
    } else {
        tiptap::node_with_content("paragraph", inlines)
    }
}

fn inline_from(payload: &TextPayload) -> Vec<Value> {
    payload
        .elements
        .iter()
        .filter_map(|element| match element {
            TextElement::TextRun(run) => {
                if run.content.is_empty() {
                    None
                } else {
                    Some(tiptap::text(&run.content, marks_from(&run.style)))
                }
            }
        })
        .collect()
}

/// mark 顺序:bold / italic / strike / code / link(href)。
fn marks_from(style: &TextElementStyle) -> Option<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    if style.bold {
        out.push(tiptap::mark("bold", None));
    }
    if style.italic {
        out.push(tiptap::mark("italic", None));
    }
    if style.strikethrough {
        out.push(tiptap::mark("strike", None));
    }
    if style.inline_code {
        out.push(tiptap::mark("code", None));
    }
    if let Some(href) = style.link.as_deref().filter(|h| !h.is_empty()) {
        let mut attrs = serde_json::Map::new();
        attrs.insert("href".into(), Value::from(href));
        out.push(tiptap::mark("link", Some(attrs)));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn plain_text(elements: &[TextElement]) -> String {
    elements
        .iter()
        .map(|element| match element {
            TextElement::TextRun(run) => run.content.as_str(),
        })
        .collect()
}

// MARK: - Tiptap → [FeishuBlock]

/// 一次 `to_feishu_blocks` 调用的可变记账。
#[derive(Default)]
struct EmissionContext {
    counter: usize,
    /// 已发射块,按文档序(父块先于子孙;`insert_block` 保证)。
    emitted: Vec<FeishuBlock>,
}

impl EmissionContext {
    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("blk_{:06}", self.counter)
    }

    fn push(&mut self, block: FeishuBlock) {
        self.emitted.push(block);
    }
}

/// 把一个 Tiptap 块级节点转成一个或多个飞书块(容器类如 `bulletList`
/// 展开成多个同级项)。返回顶层发射块的 ID(按序)— 调用方拿它当
/// 父块的 `children`。
fn emit(node: &Value, parent_id: &str, ctx: &mut EmissionContext) -> Vec<String> {
    match tiptap::node_type(node) {
        "paragraph" => {
            let id = ctx.next_id();
            ctx.push(FeishuBlock {
                block_id: id.clone(),
                parent_id: Some(parent_id.to_string()),
                children: None,
                payload: Payload::Text(text_payload(tiptap::content(node))),
            });
            vec![id]
        }
        "heading" => {
            let level = tiptap::attr_i64(node, "level")
                .unwrap_or(1)
                .clamp(1, 6) as u8;
            let id = ctx.next_id();
            ctx.push(FeishuBlock {
                block_id: id.clone(),
                parent_id: Some(parent_id.to_string()),
                children: None,
                payload: Payload::Heading {
                    level,
                    text: text_payload(tiptap::content(node)),
                },
            });
            vec![id]
        }
        "blockquote" => {
            // 引用载荷取*首个*段落;其余子块成为嵌套子块。这是多段
            // 引用的有损路径,但占绝对多数的单段引用往返是精确的。
            //
            // Swift 源在 case 开头有一个从未使用的 `let id = nextId()`
            // (死代码,每个 blockquote 白烧一个编号);Rust 移植只分
            // 配一次(quoteId)。编号与 Swift 不逐位对齐,测试不钉死
            // 具体编号。
            let children = tiptap::content(node);
            let (first_inlines, rest) = split_leading_paragraph(children);
            // 先于子块保留引用 ID,子块的 parentId / 页 children 排序
            // 才与文档序一致。
            let quote_id = ctx.next_id();
            let nested_ids: Vec<String> = rest
                .iter()
                .flat_map(|child| emit(child, &quote_id, ctx))
                .collect();
            let block = FeishuBlock {
                block_id: quote_id.clone(),
                parent_id: Some(parent_id.to_string()),
                children: children_opt(&nested_ids),
                payload: Payload::Quote(text_payload(first_inlines)),
            };
            // 把引用拼接到子块前面,保持文档序。
            insert_block(block, &nested_ids, ctx);
            vec![quote_id]
        }
        "codeBlock" => {
            let lang = tiptap::attr_str(node, "language").filter(|s| !s.is_empty());
            let id = ctx.next_id();
            let raw: String = tiptap::content(node)
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect();
            ctx.push(FeishuBlock {
                block_id: id.clone(),
                parent_id: Some(parent_id.to_string()),
                children: None,
                payload: Payload::Code(CodePayload {
                    elements: if raw.is_empty() {
                        Vec::new()
                    } else {
                        vec![TextElement::TextRun(TextRun::new(raw))]
                    },
                    language: lang.map(str::to_string),
                }),
            });
            vec![id]
        }
        "horizontalRule" => {
            let id = ctx.next_id();
            ctx.push(FeishuBlock {
                block_id: id.clone(),
                parent_id: Some(parent_id.to_string()),
                children: None,
                payload: Payload::Divider,
            });
            vec![id]
        }
        "image" => {
            let src = tiptap::attr_str(node, "src");
            let alt = tiptap::attr_str(node, "alt");
            const PREFIX: &str = "feishu://image/";
            let payload = if let Some(src) = src.filter(|s| s.starts_with(PREFIX)) {
                ImagePayload {
                    token: Some(src[PREFIX.len()..].to_string()),
                    src: None,
                    alt: alt.map(str::to_string),
                    ..ImagePayload::default()
                }
            } else {
                ImagePayload {
                    token: None,
                    src: src.map(str::to_string),
                    alt: alt.map(str::to_string),
                    ..ImagePayload::default()
                }
            };
            let id = ctx.next_id();
            ctx.push(FeishuBlock {
                block_id: id.clone(),
                parent_id: Some(parent_id.to_string()),
                children: None,
                payload: Payload::Image(payload),
            });
            vec![id]
        }
        "bulletList" => emit_list_items(tiptap::content(node), ListKind::Bullet, parent_id, ctx),
        "orderedList" => emit_list_items(tiptap::content(node), ListKind::Ordered, parent_id, ctx),
        "taskList" => emit_list_items(tiptap::content(node), ListKind::Todo, parent_id, ctx),
        "callout" => emit_callout(node, parent_id, ctx),
        "table" => emit_table(node, parent_id, ctx),
        "feishu_placeholder_block" => emit_placeholder(node, parent_id, ctx)
            .into_iter()
            .collect(),
        // 仅行内;不该进块发射路径 — 静默返回而不是污染输出。
        "text" | "hardBreak" => Vec::new(),
        // 本地视频(#88):v1 的飞书没有本地视频块,什么都不发射。
        // FeishuImageUploadStage 已把该节点记进 Report.skippedVideos
        // (→ 软警告),本地资源 + 磁盘 <video> 行原样保留。显式 case,
        // 视频绝不无名落入静默 default。
        "video" => Vec::new(),
        // 未知块(raw_markdown_block 等)— 静默丢弃,后续切片可以
        // 无冲突地叠上支持。
        _ => Vec::new(),
    }
}

/// 首个段落拆分:规范形下引用 / 列表项的第一个子节点是承载行内内容
/// 的段落,其余是嵌套块。无首段落时行内为空、全部子块按嵌套处理。
fn split_leading_paragraph(children: &[Value]) -> (&[Value], &[Value]) {
    match children.split_first() {
        Some((first, rest)) if tiptap::node_type(first) == "paragraph" => {
            (tiptap::content(first), rest)
        }
        _ => (&[], children),
    }
}

/// 把 `feishu_placeholder_block` Tiptap 节点转回携带 `.placeholder`
/// 载荷的 `FeishuBlock`。与其余发射路径的两点差异:
///
/// 1. **`block_id` 从节点 attrs 原样保留**,不取 `ctx.next_id()`。
///    占位块往返的全部意义就是把原飞书块 ID 交回推送协调器(经
///    `preserve_existing_reference`),让它说「引用既有块,别重建」。
/// 2. **不发射子块。** 占位块是原子节点;嵌套在进站边界
///    (`render_single`)已丢,到这里 Tiptap 树里占位块下不会有子块。
///
/// 节点畸形(缺必填属性)返回 `None` — 调用方静默丢弃,不污染输出。
fn emit_placeholder(
    node: &Value,
    parent_id: &str,
    ctx: &mut EmissionContext,
) -> Option<String> {
    let subtype = tiptap::attr_str(node, "type").and_then(PlaceholderSubtype::from_name)?;
    let block_id = tiptap::attr_str(node, "block_id").filter(|id| !id.is_empty())?;
    let title = tiptap::attr_str(node, "title")?;
    let url = tiptap::attr_str(node, "url")?;
    let block_token = tiptap::attr_str(node, "block_token").filter(|s| !s.is_empty());
    let summary = tiptap::attr_str(node, "summary");
    let created = tiptap::attr_str(node, "created_in_feishu_at").filter(|s| !s.is_empty());
    let unknown = extract_unknown_fields(node.get("attrs").and_then(|a| a.get("unknown_fields")));
    ctx.push(FeishuBlock {
        block_id: block_id.to_string(),
        parent_id: Some(parent_id.to_string()),
        children: None,
        payload: Payload::Placeholder(PlaceholderPayload {
            subtype,
            block_token: block_token.map(str::to_string),
            title: title.to_string(),
            summary: summary.map(str::to_string),
            url: url.to_string(),
            created_in_feishu_at: created.map(str::to_string),
            unknown_fields: unknown,
        }),
    });
    Some(block_id.to_string())
}

fn extract_unknown_fields(raw: Option<&Value>) -> Vec<(String, String)> {
    let Some(items) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let key = item.get("key")?.as_str()?;
            let value = item.get("value")?.as_str()?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn emit_callout(node: &Value, parent_id: &str, ctx: &mut EmissionContext) -> Vec<String> {
    let type_raw = tiptap::attr_str(node, "type")
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "note".to_string());
    let callout_type = FeishuCalloutType::from_raw_name(&type_raw).unwrap_or(FeishuCalloutType::Note);
    let callout_id = ctx.next_id();

    // 发射边界也过滤禁入子块(纵深防御 — ASTConverter 做了同样的事,
    // 但来自 JS 的 Tiptap 状态可能违 schema)。禁入:代码块 / 表格 /
    // 分割线 / 图片。
    let disallowed = ["codeBlock", "table", "horizontalRule", "image"];
    let nested_ids: Vec<String> = tiptap::content(node)
        .iter()
        .filter(|child| !disallowed.contains(&tiptap::node_type(child)))
        .flat_map(|child| emit(child, &callout_id, ctx))
        .collect();

    let block = FeishuBlock {
        block_id: callout_id.clone(),
        parent_id: Some(parent_id.to_string()),
        children: children_opt(&nested_ids),
        payload: Payload::Callout(CalloutPayload {
            // wire 命名 ID(如 "bulb"),**不是** Unicode 字形 — 飞书
            // callout 端点吃码点会回 1770006 schema mismatch。Unicode
            // 形只用于本地 Tiptap 渲染。
            emoji: Some(callout_type.wire_emoji_id().to_string()),
            background_color: Some(callout_type.background_color().to_string()),
        }),
    };
    insert_block(block, &nested_ids, ctx);
    vec![callout_id]
}

fn emit_table(node: &Value, parent_id: &str, ctx: &mut EmissionContext) -> Vec<String> {
    let row_nodes = tiptap::content(node);
    let row_size = row_nodes.len();
    // 列数取各行单元格数的最大值;短行补空单元格,飞书
    // `children.count == rowSize × columnSize` 的不变量恒成立。
    let column_size = row_nodes
        .iter()
        .map(|row| tiptap::content(row).len())
        .max()
        .unwrap_or(0);
    if row_size == 0 || column_size == 0 {
        return Vec::new();
    }

    // 表头检测:任一行的首单元格是 `tableHeader` 即有表头。
    // Tiptap 解析器只把表头放第 0 行。
    let header_row = row_nodes
        .first()
        .and_then(|row| tiptap::content(row).first())
        .is_some_and(|cell| tiptap::node_type(cell) == "tableHeader");

    let table_id = ctx.next_id();

    // 逐 (r,c) 预留单元格 ID 并发射单元格 + 内层文本块。
    let mut cell_ids: Vec<String> = Vec::new();
    for row in row_nodes {
        let cells = tiptap::content(row);
        for c in 0..column_size {
            let cell_node = cells.get(c);
            let cell_id = ctx.next_id();
            cell_ids.push(cell_id.clone());

            // 内层文本块:单元格首个段落塌缩为飞书文本块;空单元格
            // 得空文本块。
            let inlines: &[Value] = cell_node
                .and_then(|n| {
                    tiptap::content(n)
                        .iter()
                        .find(|child| tiptap::node_type(child) == "paragraph")
                })
                .map(tiptap::content)
                .unwrap_or(&[]);
            let text_id = ctx.next_id();
            ctx.push(FeishuBlock {
                block_id: text_id.clone(),
                parent_id: Some(cell_id.clone()),
                children: None,
                payload: Payload::Text(text_payload(inlines)),
            });
            // 单元格拼到其内层文本块前面。
            let cell_block = FeishuBlock {
                block_id: cell_id.clone(),
                parent_id: Some(table_id.clone()),
                children: Some(vec![text_id.clone()]),
                payload: Payload::TableCell,
            };
            insert_block(cell_block, &[text_id.clone()], ctx);
        }
    }

    let table_block = FeishuBlock {
        block_id: table_id.clone(),
        parent_id: Some(parent_id.to_string()),
        children: Some(cell_ids.clone()),
        payload: Payload::Table(TablePayload {
            row_size,
            column_size,
            header_row,
        }),
    };
    insert_block(table_block, &cell_ids, ctx);
    vec![table_id]
}

fn emit_list_items(
    items: &[Value],
    kind: ListKind,
    parent_id: &str,
    ctx: &mut EmissionContext,
) -> Vec<String> {
    items
        .iter()
        .map(|item| emit_list_item(item, kind, parent_id, ctx))
        .collect()
}

fn emit_list_item(
    item: &Value,
    kind: ListKind,
    parent_id: &str,
    ctx: &mut EmissionContext,
) -> String {
    let children = tiptap::content(item);
    // Tiptap 列表项恒以段落开头(规范形);嵌套列表 / 引用作为后续
    // 兄弟节点。
    let (first_inlines, nested) = split_leading_paragraph(children);
    let item_id = ctx.next_id();
    let nested_ids: Vec<String> = nested
        .iter()
        .flat_map(|child| emit(child, &item_id, ctx))
        .collect();
    let payload = text_payload(first_inlines);
    let block = match kind {
        ListKind::Bullet => FeishuBlock {
            block_id: item_id.clone(),
            parent_id: Some(parent_id.to_string()),
            children: children_opt(&nested_ids),
            payload: Payload::Bullet(payload),
        },
        ListKind::Ordered => FeishuBlock {
            block_id: item_id.clone(),
            parent_id: Some(parent_id.to_string()),
            children: children_opt(&nested_ids),
            payload: Payload::Ordered(payload),
        },
        ListKind::Todo => {
            let done = tiptap::attr_bool(item, "checked").unwrap_or(false);
            FeishuBlock {
                block_id: item_id.clone(),
                parent_id: Some(parent_id.to_string()),
                children: children_opt(&nested_ids),
                payload: Payload::Todo {
                    text: payload,
                    done,
                },
            }
        }
    };
    insert_block(block, &nested_ids, ctx);
    item_id
}

/// 把父块拼到它已发射的子块前面,`emitted` 保持文档序(父先于子孙)。
/// 没有这步,blockquote / 列表项这类预分配 ID 的块会排在其子块之后。
fn insert_block(block: FeishuBlock, child_ids: &[String], ctx: &mut EmissionContext) {
    if child_ids.is_empty() {
        ctx.push(block);
        return;
    }
    if let Some(first_child) = child_ids.first() {
        if let Some(index) = ctx
            .emitted
            .iter()
            .position(|b| b.block_id == *first_child)
        {
            ctx.emitted.insert(index, block);
            return;
        }
    }
    ctx.push(block);
}

/// Swift `nestedIds.isEmpty ? nil : nestedIds` — 空子块列表存 None,
/// 不存 `Some([])`。
fn children_opt(ids: &[String]) -> Option<Vec<String>> {
    if ids.is_empty() {
        None
    } else {
        Some(ids.to_vec())
    }
}

fn text_payload(inlines: &[Value]) -> TextPayload {
    let mut out: Vec<TextElement> = Vec::new();
    for node in inlines {
        match tiptap::node_type(node) {
            "text" => {
                let style = text_element_style(node.get("marks").and_then(Value::as_array));
                let content = node.get("text").and_then(Value::as_str).unwrap_or("");
                if !content.is_empty() {
                    out.push(TextElement::TextRun(TextRun {
                        content: content.to_string(),
                        style,
                    }));
                }
            }
            "hardBreak" => {
                match out.last_mut() {
                    Some(TextElement::TextRun(run)) => run.content.push('\n'),
                    _ => out.push(TextElement::TextRun(TextRun::new("\\n"))),
                }
            }
            // 图片 / 行内扩展等,v2-4a 行内范围之外。
            _ => {}
        }
    }
    TextPayload { elements: out }
}

fn text_element_style(marks: Option<&Vec<Value>>) -> TextElementStyle {
    let mut style = TextElementStyle::default();
    for mark in marks.map(Vec::as_slice).unwrap_or(&[]) {
        match tiptap::node_type(mark) {
            "bold" => style.bold = true,
            "italic" => style.italic = true,
            "strike" => style.strikethrough = true,
            "code" => style.inline_code = true,
            "link" => {
                if let Some(href) = mark
                    .get("attrs")
                    .and_then(|a| a.get("href"))
                    .and_then(Value::as_str)
                {
                    style.link = Some(href.to_string());
                }
            }
            _ => {}
        }
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::encoder;
    use serde_json::json;

    // MARK: - 夹具辅助(Swift 测试同款)

    /// Swift `payloadShapes`:只比载荷 — 往返会合法重写 block_id /
    /// parent_id / children ID;树形由载荷发射顺序(父先于子)保持。
    fn payload_shapes(blocks: &[FeishuBlock]) -> Vec<Payload> {
        blocks.iter().map(|b| b.payload.clone()).collect()
    }

    /// Swift `body`:剔掉 page 根再数正文块。
    fn body(blocks: &[FeishuBlock]) -> Vec<FeishuBlock> {
        blocks
            .iter()
            .filter(|b| !matches!(b.payload, Payload::Page(_)))
            .cloned()
            .collect()
    }

    fn mk(
        id: &str,
        parent: Option<&str>,
        children: Option<Vec<&str>>,
        payload: Payload,
    ) -> FeishuBlock {
        FeishuBlock {
            block_id: id.to_string(),
            parent_id: parent.map(str::to_string),
            children: children.map(|c| c.into_iter().map(String::from).collect()),
            payload,
        }
    }

    fn page(children: &[&str]) -> FeishuBlock {
        mk(
            "p",
            None,
            Some(children.to_vec()),
            Payload::Page(PagePayload::default()),
        )
    }

    fn page_ids(children: Vec<String>) -> FeishuBlock {
        FeishuBlock {
            block_id: "p".into(),
            parent_id: None,
            children: Some(children),
            payload: Payload::Page(PagePayload::default()),
        }
    }

    /// 单个纯文本 run 的 `TextPayload`。
    fn tp(content: &str) -> TextPayload {
        TextPayload {
            elements: vec![TextElement::TextRun(TextRun::new(content))],
        }
    }

    fn textb(id: &str, parent: &str, content: &str) -> FeishuBlock {
        mk(id, Some(parent), None, Payload::Text(tp(content)))
    }

    fn headingb(id: &str, parent: &str, level: u8, content: &str) -> FeishuBlock {
        mk(
            id,
            Some(parent),
            None,
            Payload::Heading {
                level,
                text: tp(content),
            },
        )
    }

    fn bulletb(id: &str, parent: &str, content: &str) -> FeishuBlock {
        mk(id, Some(parent), None, Payload::Bullet(tp(content)))
    }

    fn orderedb(id: &str, parent: &str, content: &str) -> FeishuBlock {
        mk(id, Some(parent), None, Payload::Ordered(tp(content)))
    }

    fn todob(id: &str, parent: &str, content: &str, done: bool) -> FeishuBlock {
        mk(
            id,
            Some(parent),
            None,
            Payload::Todo {
                text: tp(content),
                done,
            },
        )
    }

    fn codeb(id: &str, parent: &str, language: Option<&str>, content: &str) -> FeishuBlock {
        mk(
            id,
            Some(parent),
            None,
            Payload::Code(CodePayload {
                elements: vec![TextElement::TextRun(TextRun::new(content))],
                language: language.map(str::to_string),
            }),
        )
    }

    fn cellb(id: &str, parent: &str, text_id: &str) -> FeishuBlock {
        mk(id, Some(parent), Some(vec![text_id]), Payload::TableCell)
    }

    fn calloutb(
        id: &str,
        parent: &str,
        children: &[&str],
        emoji: &str,
        color: &str,
    ) -> FeishuBlock {
        mk(
            id,
            Some(parent),
            Some(children.to_vec()),
            Payload::Callout(CalloutPayload {
                emoji: Some(emoji.to_string()),
                background_color: Some(color.to_string()),
            }),
        )
    }

    /// 最小合法占位块(4 个必填字段,无子块;parent 恒 "p")。
    fn ph(id: &str, subtype: PlaceholderSubtype, title: &str, url: &str) -> FeishuBlock {
        ph_full(id, subtype, title, url, None, None, None, vec![], None)
    }

    #[allow(clippy::too_many_arguments)]
    fn ph_full(
        id: &str,
        subtype: PlaceholderSubtype,
        title: &str,
        url: &str,
        block_token: Option<&str>,
        summary: Option<&str>,
        created: Option<&str>,
        unknown: Vec<(&str, &str)>,
        children: Option<Vec<&str>>,
    ) -> FeishuBlock {
        mk(
            id,
            Some("p"),
            children,
            Payload::Placeholder(PlaceholderPayload {
                subtype,
                block_token: block_token.map(str::to_string),
                title: title.to_string(),
                summary: summary.map(str::to_string),
                url: url.to_string(),
                created_in_feishu_at: created.map(str::to_string),
                unknown_fields: unknown
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }),
        )
    }

    /// 载荷种类名(失败信息里给人看的;Swift `kindName`)。
    fn kind_name(payload: &Payload) -> String {
        match payload {
            Payload::Page(_) => "page".into(),
            Payload::Text(_) => "text".into(),
            Payload::Heading { level, .. } => format!("heading({level})"),
            Payload::Bullet(_) => "bullet".into(),
            Payload::Ordered(_) => "ordered".into(),
            Payload::Code(p) => format!("code({})", p.language.as_deref().unwrap_or("?")),
            Payload::Quote(_) => "quote".into(),
            Payload::Todo { done, .. } => {
                format!("todo({})", if *done { "done" } else { "pending" })
            }
            Payload::Callout(p) => {
                format!("callout({})", p.background_color.as_deref().unwrap_or("?"))
            }
            Payload::Divider => "divider".into(),
            Payload::Image(_) => "image".into(),
            Payload::Table(_) => "table".into(),
            Payload::TableCell => "tableCell".into(),
            Payload::Placeholder(p) => format!("placeholder({})", p.subtype.as_str()),
        }
    }

    fn kind_list(blocks: &[FeishuBlock]) -> String {
        blocks
            .iter()
            .map(|b| kind_name(&b.payload))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 空文档的规范序列化(与 `parse_document("")` 同形)。
    fn empty_md() -> String {
        markdown::serialize(&markdown::parse_document(""))
    }

    // MARK: - Markdown → blocks(FeishuStructuralConverterTests)

    /// Swift `testParagraphBecomesText`。
    #[test]
    fn paragraph_becomes_text() {
        let blocks = to_feishu_blocks("hello world");
        let body_blocks = body(&blocks);
        assert_eq!(body_blocks.len(), 1);
        let Payload::Text(payload) = &body_blocks[0].payload else {
            panic!("expected text, got {:?}", body_blocks[0].payload)
        };
        assert_eq!(payload.elements.len(), 1);
        // TextElement 当前只有 TextRun 一个变体,直接解构。
        let TextElement::TextRun(run) = &payload.elements[0];
        assert_eq!(run.content, "hello world");
        assert_eq!(run.style, TextElementStyle::PLAIN);
    }

    /// Swift `testHeadingLevelsOneThroughSix`。
    #[test]
    fn heading_levels_one_through_six() {
        let md = "# h1\n## h2\n### h3\n#### h4\n##### h5\n###### h6";
        let body_blocks = body(&to_feishu_blocks(md));
        assert_eq!(body_blocks.len(), 6);
        for (idx, expected) in (1..=6u8).enumerate() {
            let Payload::Heading { level, .. } = &body_blocks[idx].payload else {
                panic!("expected heading at {idx}, got {:?}", body_blocks[idx].payload)
            };
            assert_eq!(*level, expected, "index {idx}");
        }
    }

    /// Swift `testBoldItalicMarksRoundTripIntoStyle`。
    #[test]
    fn bold_italic_marks_round_trip_into_style() {
        let body_blocks = body(&to_feishu_blocks("**bold** and *italic*"));
        let Payload::Text(payload) = &body_blocks[0].payload else {
            panic!("expected text, got {:?}", body_blocks[0].payload)
        };
        let runs: Vec<(&str, &TextElementStyle)> = payload
            .elements
            .iter()
            .filter_map(|e| match e {
                TextElement::TextRun(r) => Some((r.content.as_str(), &r.style)),
            })
            .collect();
        assert!(
            runs.iter().any(|&(c, s)| c == "bold" && s.bold),
            "runs: {runs:?}"
        );
        assert!(
            runs.iter().any(|&(c, s)| c == "italic" && s.italic),
            "runs: {runs:?}"
        );
    }

    /// Swift `testInlineCodeMarkRoundTrips`。
    #[test]
    fn inline_code_mark_round_trips() {
        let body_blocks = body(&to_feishu_blocks("call `foo()`"));
        let Payload::Text(payload) = &body_blocks[0].payload else {
            panic!("expected text, got {:?}", body_blocks[0].payload)
        };
        let runs: Vec<(&str, &TextElementStyle)> = payload
            .elements
            .iter()
            .filter_map(|e| match e {
                TextElement::TextRun(r) => Some((r.content.as_str(), &r.style)),
            })
            .collect();
        assert!(
            runs.iter().any(|&(c, s)| c == "foo()" && s.inline_code),
            "runs: {runs:?}"
        );
    }

    /// Swift `testLinkMarkRoundTrips`。
    #[test]
    fn link_mark_round_trips() {
        let body_blocks = body(&to_feishu_blocks("see [docs](https://example.com)"));
        let Payload::Text(payload) = &body_blocks[0].payload else {
            panic!("expected text, got {:?}", body_blocks[0].payload)
        };
        let linked: Vec<(&str, &str)> = payload
            .elements
            .iter()
            .filter_map(|e| match e {
                TextElement::TextRun(r) => {
                    r.style.link.as_deref().map(|href| (r.content.as_str(), href))
                }
            })
            .collect();
        assert_eq!(
            linked.first(),
            Some(&("docs", "https://example.com")),
            "linked: {linked:?}"
        );
    }

    /// Swift `testBulletListBecomesBulletItems`。
    #[test]
    fn bullet_list_becomes_bullet_items() {
        let body_blocks = body(&to_feishu_blocks("- alpha\n- beta"));
        assert_eq!(body_blocks.len(), 2);
        for block in &body_blocks {
            assert!(
                matches!(block.payload, Payload::Bullet(_)),
                "expected bullet, got {:?}",
                block.payload
            );
        }
    }

    /// Swift `testOrderedListBecomesOrderedItems`。
    #[test]
    fn ordered_list_becomes_ordered_items() {
        let body_blocks = body(&to_feishu_blocks("1. one\n2. two"));
        assert_eq!(body_blocks.len(), 2);
        for block in &body_blocks {
            assert!(
                matches!(block.payload, Payload::Ordered(_)),
                "expected ordered, got {:?}",
                block.payload
            );
        }
    }

    /// Swift `testTodoListMapsCheckedState`。
    #[test]
    fn todo_list_maps_checked_state() {
        let body_blocks = body(&to_feishu_blocks("- [ ] open\n- [x] done"));
        assert_eq!(body_blocks.len(), 2);
        let Payload::Todo { done: open_done, .. } = &body_blocks[0].payload else {
            panic!("expected todo, got {:?}", body_blocks[0].payload)
        };
        let Payload::Todo { done: done_done, .. } = &body_blocks[1].payload else {
            panic!("expected todo, got {:?}", body_blocks[1].payload)
        };
        assert!(!open_done);
        assert!(done_done);
    }

    /// Swift `testQuoteBecomesQuote`。
    #[test]
    fn quote_becomes_quote() {
        let body_blocks = body(&to_feishu_blocks("> wisdom"));
        assert_eq!(body_blocks.len(), 1);
        let Payload::Quote(payload) = &body_blocks[0].payload else {
            panic!("expected quote, got {:?}", body_blocks[0].payload)
        };
        let Some(TextElement::TextRun(run)) = payload.elements.first() else {
            panic!("expected textRun")
        };
        assert_eq!(run.content, "wisdom");
    }

    /// Swift `testCodeBlockPreservesLanguage`。
    #[test]
    fn code_block_preserves_language() {
        let md = "```swift\nlet x = 1\n```";
        let body_blocks = body(&to_feishu_blocks(md));
        assert_eq!(body_blocks.len(), 1);
        let Payload::Code(payload) = &body_blocks[0].payload else {
            panic!("expected code, got {:?}", body_blocks[0].payload)
        };
        assert_eq!(payload.language.as_deref(), Some("swift"));
        let Some(TextElement::TextRun(run)) = payload.elements.first() else {
            panic!("expected textRun in code")
        };
        assert!(run.content.contains("let x = 1"), "{}", run.content);
    }

    /// Swift `testDividerBecomesDivider`。
    #[test]
    fn divider_becomes_divider() {
        let body_blocks = body(&to_feishu_blocks("---"));
        assert_eq!(body_blocks.len(), 1);
        assert!(
            matches!(body_blocks[0].payload, Payload::Divider),
            "expected divider, got {:?}",
            body_blocks[0].payload
        );
    }

    /// Swift `testImagePreservesSrcAndAlt` — 弱断言:解析器把独立图片
    /// 包进段落,converter 把该段落投影为行内为空的文本块(图片是
    /// 块级节点,行内路径未桥接;v2-4a 可接受)。块级图片路径由
    /// 手工构造块的方向覆盖(test 16)。此处只验证不崩、有合理产出。
    #[test]
    fn image_preserves_src_and_alt() {
        let blocks = to_feishu_blocks("![logo](https://example.com/logo.png)");
        assert!(!body(&blocks).is_empty());
    }

    /// Swift `testLocalVideoNodeEmitsNoBlock`(#88):`video` 节点不发射
    /// 飞书块 — 必须命中显式 `case "video"`,不是静默 default;用户侧
    /// 跳过警告由 FeishuImageUploadStage 的 `Report.skippedVideos` 出。
    #[test]
    fn local_video_node_emits_no_block() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [{ "type": "text", "text": "before" }] },
                { "type": "video", "attrs": { "src": "donemd-asset://clip.mp4" } },
                { "type": "paragraph", "content": [{ "type": "text", "text": "after" }] },
            ],
        });
        let body_blocks = body(&tiptap_to_blocks(&doc));
        assert_eq!(body_blocks.len(), 2, "video 节点不得发射飞书块");
        for block in &body_blocks {
            assert!(
                matches!(block.payload, Payload::Text(_)),
                "expected only text blocks, got {:?}",
                block.payload
            );
        }
    }

    // MARK: - blocks → Markdown

    /// Swift `testToMarkdownRendersCanonicalHeadings`。
    #[test]
    fn to_markdown_renders_canonical_headings() {
        let blocks = vec![
            page(&["b1", "b2"]),
            headingb("b1", "p", 1, "Title"),
            textb("b2", "p", "Body"),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("# Title"), "{md}");
        assert!(md.contains("Body"), "{md}");
    }

    /// Swift `testToMarkdownRendersTodoChecked`。
    #[test]
    fn to_markdown_renders_todo_checked() {
        let blocks = vec![
            page(&["t1", "t2"]),
            todob("t1", "p", "open", false),
            todob("t2", "p", "done", true),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("- [ ] open"), "{md}");
        assert!(md.contains("- [x] done"), "{md}");
    }

    /// Swift `testToMarkdownRendersImageWithToken`。
    #[test]
    fn to_markdown_renders_image_with_token() {
        let blocks = vec![
            page(&["i1"]),
            mk(
                "i1",
                Some("p"),
                None,
                Payload::Image(ImagePayload {
                    token: Some("boxcnImg123".into()),
                    src: None,
                    alt: Some("diagram".into()),
                    width: None,
                    height: None,
                }),
            ),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("feishu://image/boxcnImg123"), "{md}");
        assert!(md.contains("diagram"), "{md}");
    }

    // MARK: - round-trip

    /// Swift `testMarkdownRoundTripIsCanonicalIdempotent`。
    #[test]
    fn markdown_round_trip_is_canonical_idempotent() {
        let canonical = "# Title\n\nBody paragraph with **bold** and *italic*.\n\n\
- [ ] open task\n- [x] done task\n\n\
> quoted line\n\n\
```swift\nlet x = 1\n```\n\n\
---";
        let blocks = to_feishu_blocks(canonical);
        let regenerated = to_markdown(&blocks);
        // 再解析再发射应与第二遍一致(规范输入上的幂等)。
        let re_blocks = to_feishu_blocks(&regenerated);
        assert_eq!(
            payload_shapes(&blocks),
            payload_shapes(&re_blocks),
            "regenerated markdown:\n{regenerated}"
        );
    }

    /// Swift `testFeishuJSONRoundTripStructuralEquivalence`。夹具排序
    /// 注记:bullet 组与 todo 组之间用段落隔开 — 相邻 `-` 列表混排会
    /// 按 ADR-0002 / ASTConverter 规范形塌缩进单个 taskList(复选框
    /// 出现在 `-` 列表里就把整列表拉进 taskList),那是正确的规范化,
    /// 但不是逐块等价。
    #[test]
    fn feishu_json_round_trip_structural_equivalence() {
        let original = vec![
            page(&["h1", "p1", "li1", "li2", "p2", "td1", "q1", "c1", "d1"]),
            headingb("h1", "p", 2, "Section"),
            textb("p1", "p", "lead"),
            bulletb("li1", "p", "alpha"),
            bulletb("li2", "p", "beta"),
            textb("p2", "p", "tasks below"),
            todob("td1", "p", "task", true),
            mk("q1", Some("p"), None, Payload::Quote(tp("quoted"))),
            codeb("c1", "p", Some("python"), "code\n"),
            mk("d1", Some("p"), None, Payload::Divider),
        ];
        let md = to_markdown(&original);
        let regenerated = to_feishu_blocks(&md);
        assert_eq!(
            payload_shapes(&original),
            payload_shapes(&regenerated),
            "markdown:\n{md}"
        );
    }

    /// Swift `testNestedBulletPreservesHierarchy`。
    #[test]
    fn nested_bullet_preserves_hierarchy() {
        let blocks = to_feishu_blocks("- top\n  - inner");
        let bullets: Vec<&FeishuBlock> = blocks
            .iter()
            .filter(|b| matches!(b.payload, Payload::Bullet(_)))
            .collect();
        assert_eq!(bullets.len(), 2);
        let top = bullets
            .iter()
            .find(|b| {
                matches!(
                    &b.payload,
                    Payload::Bullet(p)
                        if matches!(
                            p.elements.first(),
                            Some(TextElement::TextRun(r)) if r.content == "top"
                        )
                )
            })
            .expect("top bullet");
        assert_eq!(top.children.as_ref().map(Vec::len), Some(1));
    }

    // MARK: - mermaid + callout + 表格(FeishuStructuralConverterRichBlocksTests)

    /// Swift `testMermaidCodeBlockPreservesLanguage`。
    #[test]
    fn mermaid_code_block_preserves_language() {
        let md = "```mermaid\ngraph TD; A-->B;\n```";
        let blocks = body(&to_feishu_blocks(md));
        assert_eq!(blocks.len(), 1);
        let Payload::Code(payload) = &blocks[0].payload else {
            panic!("expected code, got {:?}", blocks[0].payload)
        };
        assert_eq!(payload.language.as_deref(), Some("mermaid"));
        let Some(TextElement::TextRun(run)) = payload.elements.first() else {
            panic!("expected textRun")
        };
        assert!(run.content.contains("graph TD"), "{}", run.content);
    }

    /// Swift `testMermaidRoundTripIsCanonicalIdempotent`。
    #[test]
    fn mermaid_round_trip_is_canonical_idempotent() {
        let canonical = "```mermaid\nsequenceDiagram\n    A->>B: Hi\n```";
        let blocks = to_feishu_blocks(canonical);
        let md = to_markdown(&blocks);
        let re_blocks = to_feishu_blocks(&md);
        assert_eq!(payload_shapes(&blocks), payload_shapes(&re_blocks));
    }

    /// Swift `testCalloutNoteEmitsLightBlue`。
    #[test]
    fn callout_note_emits_light_blue() {
        let blocks = body(&to_feishu_blocks("> [!NOTE]\n> info body"));
        let Payload::Callout(payload) = &blocks[0].payload else {
            panic!("expected callout, got {:?}", blocks[0].payload)
        };
        assert_eq!(payload.background_color.as_deref(), Some("light-blue"));
        // wireEmojiId,**不是** Unicode 字形 — 2026-06-04 真机发 "💡"
        // 回 1770006 schema mismatch,飞书 wire 要命名 ID。
        assert_eq!(payload.emoji.as_deref(), Some("bulb"));
    }

    /// Swift `testCalloutAllFiveTypesMapDistinctColors`。
    #[test]
    fn callout_all_five_types_map_distinct_colors() {
        let cases = [
            ("NOTE", "light-blue", "bulb"),
            ("TIP", "light-green", "sparkles"),
            ("IMPORTANT", "light-purple", "exclamation"),
            ("WARNING", "light-yellow", "warning"),
            ("CAUTION", "light-red", "rotating_light"),
        ];
        for (callout_type, color, wire_emoji_id) in cases {
            let blocks = body(&to_feishu_blocks(&format!(
                "> [!{callout_type}]\n> body"
            )));
            let Payload::Callout(payload) = &blocks[0].payload else {
                panic!("expected callout for {callout_type}, got {:?}", blocks[0].payload)
            };
            assert_eq!(
                payload.background_color.as_deref(),
                Some(color),
                "color for {callout_type}"
            );
            // wire 命名 ID,不是 Unicode 字形(1770006 修复)。
            assert_eq!(
                payload.emoji.as_deref(),
                Some(wire_emoji_id),
                "wireEmojiId for {callout_type}"
            );
        }
    }

    /// Swift `testCalloutWireBodyEmojiIdIsNamedNotUnicode` — 1770006 修复
    /// 的端到端 wire 契约锚点:markdown → to_feishu_blocks →
    /// encode_descendant_body 产出的 `callout.emoji_id` 必须是命名 ID。
    /// 钉在 wire 边界,未来把 Unicode 字形放回路径的回归立刻被抓。
    #[test]
    fn callout_wire_body_emoji_id_is_named_not_unicode() {
        let blocks = to_feishu_blocks("> [!NOTE]\n> info");
        // 编码器要完整块列表(含作为 descendant 根的页块)。
        let wire = encoder::encode_descendant_body(&blocks).unwrap();
        assert_eq!(
            wire["descendants"][0]["callout"]["emoji_id"], "bulb",
            "wire 契约:emoji_id 必须是命名 ID,不是 Unicode(1770006 修复)"
        );
    }

    /// Swift `testCalloutChildrenAreNestedBlocks`。
    #[test]
    fn callout_children_are_nested_blocks() {
        let md = "> [!WARNING]\n> heads up\n>\n> - item one\n> - item two";
        let blocks = body(&to_feishu_blocks(md));
        assert!(
            matches!(blocks[0].payload, Payload::Callout(_)),
            "expected callout, got {:?}",
            blocks[0].payload
        );
        // callout 子块:至少 1 段 + 2 个 bullet。
        assert!(
            blocks[0]
                .children
                .as_ref()
                .is_some_and(|children| children.len() >= 3),
            "children: {:?}",
            blocks[0].children
        );
    }

    /// Swift `testCalloutBlocksToMarkdownEmitsGitHubSyntax`。
    #[test]
    fn callout_blocks_to_markdown_emits_github_syntax() {
        let blocks = vec![
            page(&["c1"]),
            calloutb("c1", "p", &["t1"], "⚠️", "light-yellow"),
            textb("t1", "c1", "heads up"),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("> [!WARNING]"), "{md}");
        assert!(md.contains("> heads up"), "{md}");
    }

    /// Swift `testCalloutDisallowedChildrenAreFiltered` — 飞书硬限制:
    /// callout 内的代码块 / 表格 / 分割线 / 图片在 converter 边界丢弃。
    #[test]
    fn callout_disallowed_children_are_filtered() {
        let blocks = vec![
            page(&["c1"]),
            calloutb("c1", "p", &["t1", "code1", "div1"], "💡", "light-blue"),
            textb("t1", "c1", "ok"),
            codeb("code1", "c1", Some("swift"), "x"),
            mk("div1", Some("c1"), None, Payload::Divider),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("> [!NOTE]"), "{md}");
        assert!(md.contains("> ok"), "{md}");
        // 禁入子块不得出现在 callout 内或外。
        assert!(!md.contains("```swift"), "{md}");
        assert!(!md.contains("---"), "{md}");
    }

    /// Swift `testGfmTableEmitsTableAndCells`。
    #[test]
    fn gfm_table_emits_table_and_cells() {
        let md = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |";
        let blocks = body(&to_feishu_blocks(md));
        // 1 个 table 块 + (3 行 × 2 列 = 6 单元格)+ 6 个内层文本。
        let tables = blocks
            .iter()
            .filter(|b| matches!(b.payload, Payload::Table(_)))
            .count();
        let cells = blocks
            .iter()
            .filter(|b| matches!(b.payload, Payload::TableCell))
            .count();
        assert_eq!(tables, 1);
        assert_eq!(cells, 6);
        let table = blocks
            .iter()
            .find(|b| matches!(b.payload, Payload::Table(_)))
            .unwrap();
        let Payload::Table(payload) = &table.payload else {
            unreachable!()
        };
        assert_eq!(payload.row_size, 3);
        assert_eq!(payload.column_size, 2);
        assert!(payload.header_row);
    }

    /// Swift `testTableBlocksToMarkdownEmitsGfmPipes`。
    #[test]
    fn table_blocks_to_markdown_emits_gfm_pipes() {
        let blocks = vec![
            page(&["tab1"]),
            mk(
                "tab1",
                Some("p"),
                Some(vec!["c00", "c01", "c10", "c11"]),
                Payload::Table(TablePayload {
                    row_size: 2,
                    column_size: 2,
                    header_row: true,
                }),
            ),
            cellb("c00", "tab1", "t00"),
            cellb("c01", "tab1", "t01"),
            cellb("c10", "tab1", "t10"),
            cellb("c11", "tab1", "t11"),
            textb("t00", "c00", "Name"),
            textb("t01", "c01", "Age"),
            textb("t10", "c10", "Alice"),
            textb("t11", "c11", "30"),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("| Name | Age |"), "{md}");
        assert!(md.contains("| --- | --- |"), "{md}");
        assert!(md.contains("| Alice | 30 |"), "{md}");
    }

    /// Swift `testCalloutRoundTripStructuralEquivalence` — 夹具用 wire
    /// 形式命名 ID("warning"),对应真实飞书拉取返回的
    /// `callout.emoji_id`;1770006 修复后 converter 无条件发 wire 格式,
    /// 只有 wire 格式夹具往返恒等。
    #[test]
    fn callout_round_trip_structural_equivalence() {
        let original = vec![
            page(&["c1"]),
            calloutb("c1", "p", &["t1"], "warning", "light-yellow"),
            textb("t1", "c1", "heads up"),
        ];
        let md = to_markdown(&original);
        let regenerated = to_feishu_blocks(&md);
        assert_eq!(payload_shapes(&original), payload_shapes(&regenerated));
    }

    /// Swift `testTableRoundTripStructuralEquivalence` — 规范文档序是
    /// 深度前序:每个 table_cell 紧跟其文本子块,与 converter 发射和
    /// 飞书实际序列化块树的方式一致(父 → 子孙 → 下一兄弟)。
    #[test]
    fn table_round_trip_structural_equivalence() {
        let original = vec![
            page(&["tab1"]),
            mk(
                "tab1",
                Some("p"),
                Some(vec!["c00", "c01", "c10", "c11"]),
                Payload::Table(TablePayload {
                    row_size: 2,
                    column_size: 2,
                    header_row: true,
                }),
            ),
            cellb("c00", "tab1", "t00"),
            textb("t00", "c00", "h1"),
            cellb("c01", "tab1", "t01"),
            textb("t01", "c01", "h2"),
            cellb("c10", "tab1", "t10"),
            textb("t10", "c10", "v1"),
            cellb("c11", "tab1", "t11"),
            textb("t11", "c11", "v2"),
        ];
        let md = to_markdown(&original);
        let regenerated = to_feishu_blocks(&md);
        assert_eq!(payload_shapes(&original), payload_shapes(&regenerated));
    }

    // MARK: - 单元格块级内容(架构边界)

    /// Swift `testTableCellWithCalloutEmitsDroppedWarning` — 2×1 表格:
    /// A 格 [text "intro" + callout],B 格 [text "ok"]。A 丢 callout 保
    /// 文字;警告携带单元格计数。
    #[test]
    fn table_cell_with_callout_emits_dropped_warning() {
        let blocks = vec![
            page(&["t"]),
            mk(
                "t",
                Some("p"),
                Some(vec!["cA", "cB"]),
                Payload::Table(TablePayload {
                    row_size: 1,
                    column_size: 2,
                    header_row: false,
                }),
            ),
            mk(
                "cA",
                Some("t"),
                Some(vec!["txA", "calloutA"]),
                Payload::TableCell,
            ),
            textb("txA", "cA", "intro"),
            calloutb("calloutA", "cA", &[], "💡", "light-yellow"),
            mk("cB", Some("t"), Some(vec!["txB"]), Payload::TableCell),
            textb("txB", "cB", "ok"),
        ];
        let result = to_markdown_with_warnings(&blocks);
        assert!(result.value.contains("intro"), "受影响单元格的前导文字必须存活");
        assert!(result.value.contains("ok"), "未受影响单元格正常往返");
        // 恰一条警告,计数 1(只有 A 格带块级内容)。
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert_eq!(
            result.warnings[0],
            ConversionWarning::TableCellBlockContentDropped { cell_count: 1 },
            "只有 A 格带块级内容;B 格纯文本"
        );
    }

    /// Swift `testTableCellWithBlockChildAggregatesAcrossCells` — 多个
    /// 受影响单元格滚成单条警告携带总数,不是每格一条。
    #[test]
    fn table_cell_with_block_child_aggregates_across_cells() {
        let blocks = vec![
            page(&["t"]),
            mk(
                "t",
                Some("p"),
                Some(vec!["cA", "cB"]),
                Payload::Table(TablePayload {
                    row_size: 1,
                    column_size: 2,
                    header_row: false,
                }),
            ),
            mk("cA", Some("t"), Some(vec!["calloutA"]), Payload::TableCell),
            calloutb("calloutA", "cA", &[], "💡", "light-yellow"),
            mk("cB", Some("t"), Some(vec!["calloutB"]), Payload::TableCell),
            calloutb("calloutB", "cB", &[], "⚠️", "light-red"),
        ];
        let result = to_markdown_with_warnings(&blocks);
        assert_eq!(
            result.warnings.len(),
            1,
            "多个受影响单元格必须滚成单条警告:{:?}",
            result.warnings
        );
        assert_eq!(
            result.warnings[0],
            ConversionWarning::TableCellBlockContentDropped { cell_count: 2 },
            "两个带块级内容的单元格 → cellCount = 2"
        );
    }

    /// Swift `testTableCellTextOnlyEmitsNoWarning` — 纯文本单元格零警告。
    #[test]
    fn table_cell_text_only_emits_no_warning() {
        let blocks = vec![
            page(&["t"]),
            mk(
                "t",
                Some("p"),
                Some(vec!["cA"]),
                Payload::Table(TablePayload {
                    row_size: 1,
                    column_size: 1,
                    header_row: false,
                }),
            ),
            mk("cA", Some("t"), Some(vec!["txA"]), Payload::TableCell),
            textb("txA", "cA", "plain"),
        ];
        let result = to_markdown_with_warnings(&blocks);
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| matches!(w, ConversionWarning::TableCellBlockContentDropped { .. })),
            "纯文本单元格不得触发丢块警告:{:?}",
            result.warnings
        );
    }

    // MARK: - 全景夹具(FeishuStructuralConverterRoundTripFixtureTests)

    /// Swift `makeKitchenSinkFeishuBlocks` — 覆盖 converter 认识的每个
    /// 载荷分支 + 历史上抓到回归的边界(callout 命名 ID、带行内 mark
    /// 的尾段、占位块 7 子类型、GFM 表格)。
    fn kitchen_sink() -> Vec<FeishuBlock> {
        let mut blocks: Vec<FeishuBlock> = Vec::new();
        let mut page_children: Vec<String> = Vec::new();
        let mut add = |block: FeishuBlock| {
            if block.parent_id.as_deref() == Some("p") {
                page_children.push(block.block_id.clone());
            }
            blocks.push(block);
        };

        // 标题 1–6,各前置一段,避免相邻标题(序列化/解析都歧义)。
        for level in 1..=6u8 {
            add(textb(
                &format!("h{level}-intro"),
                "p",
                &format!("Intro for heading {level}"),
            ));
            add(headingb(
                &format!("h{level}"),
                "p",
                level,
                &format!("Heading level {level}"),
            ));
        }

        add(bulletb("b1", "p", "bullet one"));
        add(bulletb("b2", "p", "bullet two"));
        add(orderedb("o1", "p", "step 1"));
        add(orderedb("o2", "p", "step 2"));
        add(todob("td1", "p", "task pending", false));
        add(todob("td2", "p", "task done", true));

        // 引用容器。子 ID "q1-t" 故意悬空(夹具简化),children_of 对
        // 悬空引用静默跳过 — 与容错拉取同一语义。
        add(mk(
            "q1",
            Some("p"),
            Some(vec!["q1-t"]),
            Payload::Quote(tp("a quoted line")),
        ));

        add(codeb("code1", "p", Some("swift"), "let x = 1"));
        add(codeb("code2", "p", Some("mermaid"), "graph TD; A-->B;"));
        add(mk("div1", Some("p"), None, Payload::Divider));

        // Callout ×5 — 用 wire 命名 ID(拉取真实返回的形态,1770006
        // 修复后 converter 无条件发 wire 格式)。
        let callout_specs = [
            ("ca-note", "light-blue", "bulb", "note callout"),
            ("ca-tip", "light-green", "sparkles", "tip callout"),
            ("ca-important", "light-purple", "exclamation", "important callout"),
            ("ca-warning", "light-yellow", "warning", "warning callout"),
            ("ca-caution", "light-red", "rotating_light", "caution callout"),
        ];
        for (id, color, emoji, text) in callout_specs {
            let text_id = format!("{id}-t");
            add(calloutb(id, "p", &[text_id.as_str()], emoji, color));
            add(textb(&text_id, id, text));
        }

        // 2×2 表格(表头 + 一行数据),单元格只放行内内容。
        add(mk(
            "tab1",
            Some("p"),
            Some(vec!["c00", "c01", "c10", "c11"]),
            Payload::Table(TablePayload {
                row_size: 2,
                column_size: 2,
                header_row: true,
            }),
        ));
        for (cell_id, text_id, content) in
            [("c00", "tt00", "Name"), ("c01", "tt01", "Status"), ("c10", "tt10", "Alice"), ("c11", "tt11", "active")]
        {
            add(cellb(cell_id, "tab1", text_id));
            add(textb(text_id, cell_id, content));
        }

        // 占位块 ×7 — converter 解码的全部子类型。url 故意带 `&`;
        // summary 带 `·` 等非 ASCII(注释体原样往返的覆盖面)。
        let placeholder_specs: [(&str, PlaceholderSubtype, &str, &str, Option<&str>); 7] = [
            ("ph-sheet", PlaceholderSubtype::Sheet, "Q2 OKR 表", "https://feishu.cn/sheets/shtcnA?utm=1&x=2", Some("12 行 · 4 列")),
            ("ph-mindnote", PlaceholderSubtype::Mindnote, "架构脑图", "https://feishu.cn/docx/mn_X", None),
            ("ph-board", PlaceholderSubtype::Board, "Sprint 画板", "https://feishu.cn/docx/bd_Y", Some("草图 · DRY-RUN 状态")),
            ("ph-bitable", PlaceholderSubtype::Bitable, "Bug 多维表", "https://feishu.cn/base/bcA", Some("P0 · 12 / P1 · 8")),
            ("ph-attach", PlaceholderSubtype::Attachment, "spec.pdf", "https://feishu.cn/file/atA", Some("1.2 MB")),
            ("ph-video", PlaceholderSubtype::Video, "Demo 录屏", "https://feishu.cn/file/vidA", Some("3 min")),
            ("ph-embed", PlaceholderSubtype::Embed, "Jira 工单", "https://example.atlassian.net/browse/X-1", None),
        ];
        for (id, subtype, title, url, summary) in placeholder_specs {
            add(mk(
                id,
                Some("p"),
                None,
                Payload::Placeholder(PlaceholderPayload {
                    subtype,
                    block_token: None,
                    title: title.to_string(),
                    summary: summary.map(str::to_string),
                    url: url.to_string(),
                    created_in_feishu_at: None,
                    unknown_fields: Vec::new(),
                }),
            ));
        }

        // 尾段行内 mark 拼贴(bold + italic + code + strikethrough + link)。
        add(mk(
            "p-final",
            Some("p"),
            None,
            Payload::Text(TextPayload {
                elements: vec![
                    TextElement::TextRun(TextRun::new("plain ")),
                    TextElement::TextRun(TextRun {
                        content: "bold".into(),
                        style: TextElementStyle { bold: true, ..TextElementStyle::PLAIN },
                    }),
                    TextElement::TextRun(TextRun::new(" ")),
                    TextElement::TextRun(TextRun {
                        content: "italic".into(),
                        style: TextElementStyle { italic: true, ..TextElementStyle::PLAIN },
                    }),
                    TextElement::TextRun(TextRun::new(" ")),
                    TextElement::TextRun(TextRun {
                        content: "code".into(),
                        style: TextElementStyle { inline_code: true, ..TextElementStyle::PLAIN },
                    }),
                    TextElement::TextRun(TextRun::new(" ")),
                    TextElement::TextRun(TextRun {
                        content: "struck".into(),
                        style: TextElementStyle { strikethrough: true, ..TextElementStyle::PLAIN },
                    }),
                    TextElement::TextRun(TextRun::new(" ")),
                    TextElement::TextRun(TextRun {
                        content: "link".into(),
                        style: TextElementStyle {
                            link: Some("https://example.com/?a=1&b=2".into()),
                            ..TextElementStyle::PLAIN
                        },
                    }),
                    TextElement::TextRun(TextRun::new(" end.")),
                ],
            }),
        ));

        let mut out = vec![page_ids(page_children)];
        out.extend(blocks);
        out
    }

    /// Swift `testKitchenSinkFeishuBlocksRoundTripPreservesShapes`(Path A):
    /// 飞书块 → toMarkdown → toFeishuBlocks,载荷形状序列等价。
    #[test]
    fn kitchen_sink_round_trip_preserves_shapes() {
        let original = kitchen_sink();
        let md = to_markdown(&original);
        let regenerated = to_feishu_blocks(&md);
        let original_shapes = payload_shapes(&original);
        let regenerated_shapes = payload_shapes(&regenerated);
        assert_eq!(
            original_shapes.len(),
            regenerated_shapes.len(),
            "块数往返后发散。\n原({}): {}\n再({}): {}\n=== markdown ===\n{md}",
            original_shapes.len(),
            kind_list(&original),
            regenerated_shapes.len(),
            kind_list(&regenerated),
        );
        for (index, (original, regenerated)) in original_shapes
            .iter()
            .zip(&regenerated_shapes)
            .enumerate()
        {
            assert_eq!(
                original, regenerated,
                "第 {index} 块载荷发散。\n=== markdown ===\n{md}"
            );
        }
    }

    /// Swift `testCanonicalMarkdownIsRoundTripIdempotent`(Path B):已
    /// 是规范形的输入往返后逐字节相同(两侧去首尾空白 — toMarkdown
    /// 按文件末行惯例带尾换行,字面量不带)。
    #[test]
    fn canonical_markdown_is_round_trip_idempotent() {
        let canonical = r#"# Heading one

Plain paragraph **bold** *italic* `code` ~~struck~~ [link](https://example.com/).

## Heading two

- bullet a
- bullet b

1. step one
2. step two

- [ ] todo pending
- [x] todo done

> single-line quote stays intact

---

```mermaid
graph TD; A-->B;
```

> [!NOTE]
> note callout body

> [!WARNING]
> warning callout body

| Name | Status |
| --- | --- |
| Alice | active |
| Bob | offline |

<!-- feishu-placeholder
type: sheet
block_id: doxbcXXX_blk001
title: Q2 OKR
summary: 12 行
url: https://example.feishu.cn/sheets/shtcnA
-->

Trailing line."#;
        let expected = canonical.trim();
        let blocks = to_feishu_blocks(canonical);
        let regenerated = to_markdown(&blocks);
        assert_eq!(
            regenerated.trim(),
            expected,
            "规范 markdown 往返非逐字节稳定"
        );
    }

    // MARK: - 占位块(FeishuStructuralConverterPlaceholderTests)

    /// Swift `testToMarkdownEmitsMagicCommentForSheet` — 全部 7 字段。
    #[test]
    fn to_markdown_emits_magic_comment_for_sheet() {
        let blocks = vec![
            page(&["doxbcXXX_blk001"]),
            ph_full(
                "doxbcXXX_blk001",
                PlaceholderSubtype::Sheet,
                "Q2 OKR 进度表",
                "https://example.feishu.cn/sheets/shtcnYYY",
                Some("shtcnYYY"),
                Some("列:目标 / 责任人 / 进度 / 备注 · 共 12 行"),
                Some("2026-04-15T09:00:00+08:00"),
                vec![],
                None,
            ),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("<!-- feishu-placeholder"), "{md}");
        assert!(md.contains("type: sheet"), "{md}");
        assert!(md.contains("block_id: doxbcXXX_blk001"), "{md}");
        assert!(md.contains("block_token: shtcnYYY"), "{md}");
        assert!(md.contains("title: Q2 OKR 进度表"), "{md}");
        assert!(md.contains("summary: 列:目标 / 责任人 / 进度 / 备注 · 共 12 行"), "{md}");
        assert!(md.contains("url: https://example.feishu.cn/sheets/shtcnYYY"), "{md}");
        assert!(md.contains("created_in_feishu_at: 2026-04-15T09:00:00+08:00"), "{md}");
        assert!(md.contains("-->"), "{md}");
    }

    /// Swift `testMinimalMagicCommentOmitsOptionalFields` — 只有 4 个
    /// 必填字段(type/block_id/title/url),可选行整行不出现。
    #[test]
    fn minimal_magic_comment_omits_optional_fields() {
        let blocks = vec![
            page(&["doxbcXXX_blk003"]),
            ph(
                "doxbcXXX_blk003",
                PlaceholderSubtype::Embed,
                "第三方系统嵌入",
                "https://example.com/foo",
            ),
        ];
        let md = to_markdown(&blocks);
        assert!(md.contains("type: embed"), "{md}");
        assert!(md.contains("block_id: doxbcXXX_blk003"), "{md}");
        assert!(md.contains("title: 第三方系统嵌入"), "{md}");
        assert!(md.contains("url: https://example.com/foo"), "{md}");
        assert!(!md.contains("block_token:"), "{md}");
        assert!(!md.contains("summary:"), "{md}");
        assert!(!md.contains("created_in_feishu_at:"), "{md}");
    }

    /// Swift `testParsesAllSevenSubtypes` — 7 个子类型的魔法注释各
    /// 解析出 1 个正文块,block_id 逐字保留(不重编号)。
    #[test]
    fn parses_all_seven_subtypes() {
        let cases = [
            (PlaceholderSubtype::Attachment, "attachment"),
            (PlaceholderSubtype::Sheet, "sheet"),
            (PlaceholderSubtype::Mindnote, "mindnote"),
            (PlaceholderSubtype::Video, "video"),
            (PlaceholderSubtype::Bitable, "bitable"),
            (PlaceholderSubtype::Embed, "embed"),
            (PlaceholderSubtype::Board, "board"),
        ];
        for (subtype, raw) in cases {
            let md = format!(
                "<!-- feishu-placeholder\ntype: {raw}\nblock_id: doxbcXXX_blk_{raw}\ntitle: {raw} sample\nurl: https://example.feishu.cn/{raw}/abc\n-->"
            );
            let blocks = body(&to_feishu_blocks(&md));
            assert_eq!(blocks.len(), 1, "subtype {raw} 产出 {} 块", blocks.len());
            let Payload::Placeholder(payload) = &blocks[0].payload else {
                panic!("subtype {raw} 未产出占位载荷,得 {:?}", blocks[0].payload)
            };
            assert_eq!(payload.subtype, subtype, "{raw}");
            assert_eq!(blocks[0].block_id, format!("doxbcXXX_blk_{raw}"), "{raw}");
            assert_eq!(payload.title, format!("{raw} sample"), "{raw}");
            assert_eq!(
                payload.url,
                format!("https://example.feishu.cn/{raw}/abc"),
                "{raw}"
            );
        }
    }

    /// Swift `testPreservesAllSevenFields` — 全字段魔法注释逐字段回读。
    #[test]
    fn preserves_all_seven_fields() {
        let md = "<!-- feishu-placeholder\ntype: bitable\nblock_id: doxbcXXX_blk008\nblock_token: bascnZZZ\ntitle: 任务跟踪表\nsummary: 字段:负责人 / 状态 / 截止日 · 共 28 行\nurl: https://example.feishu.cn/base/bascnZZZ\ncreated_in_feishu_at: 2026-03-01T14:30:00+08:00\n-->";
        let blocks = body(&to_feishu_blocks(md));
        assert_eq!(blocks.len(), 1);
        let Payload::Placeholder(payload) = &blocks[0].payload else {
            panic!("expected placeholder, got {:?}", blocks[0].payload)
        };
        assert_eq!(blocks[0].block_id, "doxbcXXX_blk008");
        assert_eq!(payload.subtype, PlaceholderSubtype::Bitable);
        assert_eq!(payload.block_token.as_deref(), Some("bascnZZZ"));
        assert_eq!(payload.title, "任务跟踪表");
        assert_eq!(
            payload.summary.as_deref(),
            Some("字段:负责人 / 状态 / 截止日 · 共 28 行")
        );
        assert_eq!(payload.url, "https://example.feishu.cn/base/bascnZZZ");
        assert_eq!(
            payload.created_in_feishu_at.as_deref(),
            Some("2026-03-01T14:30:00+08:00")
        );
        assert!(payload.unknown_fields.is_empty());
    }

    /// Swift `testUnknownFieldsPreservedInOrder` — 未知键按出现顺序
    /// 原样携带(向前兼容:新飞书字段往返不丢)。
    #[test]
    fn unknown_fields_preserved_in_order() {
        let md = "<!-- feishu-placeholder\ntype: board\nblock_id: doxbcXXX_blk010\ntitle: Sprint 画板\nurl: https://example.feishu.cn/board/bd1\nfuture_quota: 42\nfuture_owner: alice\n-->";
        let blocks = body(&to_feishu_blocks(md));
        let Payload::Placeholder(payload) = &blocks[0].payload else {
            panic!("expected placeholder, got {:?}", blocks[0].payload)
        };
        assert_eq!(
            payload.unknown_fields,
            vec![
                ("future_quota".to_string(), "42".to_string()),
                ("future_owner".to_string(), "alice".to_string()),
            ]
        );
    }

    /// Swift `testRoundTripFullPlaceholderByteStable` — 全字段魔法注释
    /// 往返后逐字节重现(含注释头尾与字段顺序)。
    #[test]
    fn round_trip_full_placeholder_byte_stable() {
        let raw = "<!-- feishu-placeholder\ntype: bitable\nblock_id: doxbcXXX_blk008\nblock_token: bascnZZZ\ntitle: 任务跟踪表\nsummary: 字段:负责人 / 状态 / 截止日 · 共 28 行\nurl: https://example.feishu.cn/base/bascnZZZ\ncreated_in_feishu_at: 2026-03-01T14:30:00+08:00\n-->";
        let blocks = to_feishu_blocks(raw);
        let regenerated = to_markdown(&blocks);
        assert!(
            regenerated.contains(raw),
            "再生成 markdown 应含逐字魔法注释:\n{regenerated}"
        );
    }

    /// Swift `testRoundTripWithUnknownFieldsByteStable`。
    #[test]
    fn round_trip_with_unknown_fields_byte_stable() {
        let raw = "<!-- feishu-placeholder\ntype: board\nblock_id: doxbcXXX_blk011\ntitle: 评审看板\nurl: https://example.feishu.cn/board/bd2\nfuture_collab_count: 7\n-->";
        let blocks = to_feishu_blocks(raw);
        let regenerated = to_markdown(&blocks);
        assert!(
            regenerated.contains(raw),
            "未知字段必须按原样逐字节往返:\n{regenerated}"
        );
    }

    /// Swift `testRoundTripStructuralEquivalenceForAllSubtypes` — 7 个
    /// 子类型块 → markdown → 块,载荷等价 + 占位 block_id 逐字存活
    /// (preserve_existing 重绑定语义的立足点)。
    #[test]
    fn round_trip_structural_equivalence_for_all_subtypes() {
        let original = vec![
            page(&["a1", "s1", "m1", "v1", "bt1", "e1", "br1"]),
            ph("a1", PlaceholderSubtype::Attachment, "PDF", "https://x.feishu.cn/file/a1"),
            ph("s1", PlaceholderSubtype::Sheet, "Sheet", "https://x.feishu.cn/sheets/s1"),
            ph("m1", PlaceholderSubtype::Mindnote, "Mind", "https://x.feishu.cn/mindnotes/m1"),
            ph("v1", PlaceholderSubtype::Video, "Video", "https://x.feishu.cn/video/v1"),
            ph("bt1", PlaceholderSubtype::Bitable, "Bitable", "https://x.feishu.cn/base/bt1"),
            ph("e1", PlaceholderSubtype::Embed, "Embed", "https://example.com/e1"),
            ph("br1", PlaceholderSubtype::Board, "Board", "https://x.feishu.cn/board/br1"),
        ];
        let md = to_markdown(&original);
        let regenerated = to_feishu_blocks(&md);
        assert_eq!(
            payload_shapes(&original),
            payload_shapes(&regenerated),
            "markdown:\n{md}"
        );
        let original_body = body(&original);
        let regenerated_body = body(&regenerated);
        let original_ids: Vec<&str> = original_body
            .iter()
            .map(|b| b.block_id.as_str())
            .collect();
        let regenerated_ids: Vec<&str> = regenerated_body
            .iter()
            .map(|b| b.block_id.as_str())
            .collect();
        assert_eq!(
            original_ids, regenerated_ids,
            "占位 block_id 必须逐字存活:\n{md}"
        );
    }

    /// Swift `testNestedChildrenInPlaceholderEmitWarning` — 占位块下的
    /// 子内容在 markdown 里无处安放,发丢块警告。
    #[test]
    fn nested_children_in_placeholder_emit_warning() {
        let blocks = vec![
            page(&["s1"]),
            ph_full(
                "s1",
                PlaceholderSubtype::Sheet,
                "Q2 OKR",
                "https://x.feishu.cn/sheets/s1",
                None,
                None,
                None,
                vec![],
                Some(vec!["nested1", "nested2"]),
            ),
            textb("nested1", "s1", "nested para"),
            textb("nested2", "s1", "another"),
        ];
        let result = to_markdown_with_warnings(&blocks);
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert_eq!(
            result.warnings[0],
            ConversionWarning::NestedContentDroppedInPlaceholder {
                block_id: "s1".into(),
                dropped_child_count: 2,
            }
        );
    }

    /// Swift `testNoNestedChildrenEmitsNoWarning`。
    #[test]
    fn no_nested_children_emits_no_warning() {
        let blocks = vec![
            page(&["s1"]),
            ph(
                "s1",
                PlaceholderSubtype::Sheet,
                "Q2 OKR",
                "https://x.feishu.cn/sheets/s1",
            ),
        ];
        let result = to_markdown_with_warnings(&blocks);
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| matches!(w, ConversionWarning::NestedContentDroppedInPlaceholder { .. })),
            "{:?}",
            result.warnings
        );
    }

    /// Swift `testVideoAbsorbsChildFileTokenAndName`(#87 + #89)— 飞书
    /// 把上传视频表示为 view 容器 + file 子块;converter 把子块的
    /// block_token 和文件名吸收进占位卡,不是丢弃(不发警告)。
    #[test]
    fn video_absorbs_child_file_token_and_name() {
        let blocks = vec![
            page(&["v1"]),
            ph_full(
                "v1",
                PlaceholderSubtype::Video,
                "视频",
                "",
                None,
                None,
                None,
                vec![],
                Some(vec!["f1"]),
            ),
            ph_full(
                "f1",
                PlaceholderSubtype::Attachment,
                "20260728-172558.mp4",
                "feishu://file/MP4TOKEN",
                Some("MP4TOKEN"),
                None,
                None,
                vec![],
                None,
            ),
        ];
        let result = to_markdown_with_warnings(&blocks);
        assert!(result.value.contains("type: video"), "{}", result.value);
        // preserve-existing 语义:拉回时按同一 view 块 id 重绑占位卡。
        assert!(
            result.value.contains("block_id: v1"),
            "往返必须引用 view 块 id:\n{}",
            result.value
        );
        // 真实 token 从被吸收的 file 子块提升到卡上。
        assert!(
            result.value.contains("block_token: MP4TOKEN"),
            "token 应从被吸收的 file 子块提升:\n{}",
            result.value
        );
        // 卡片标题 = 上传文件名(不是飞书默认的「视频」)。
        assert!(
            result.value.contains("title: 20260728-172558.mp4"),
            "标题应为上传文件名:\n{}",
            result.value
        );
        // #89:序列化器省略可从 type + block_token 派生的 url 行;真正
        // 的保证是解析回读时回填派生值 — 断言那边,而不是断言一条
        // 有意不再写的行。
        let reparsed = markdown::parse_document(&result.value);
        let video_node = tiptap::content(&reparsed.body)
            .iter()
            .find(|n| tiptap::node_type(n) == "feishu_placeholder_block")
            .expect("应解析出占位节点");
        assert_eq!(
            tiptap::attr_str(video_node, "url"),
            Some("feishu://video/MP4TOKEN"),
            "url 应在解析时从 type + block_token 派生(#89)"
        );
        assert!(
            result.warnings.is_empty(),
            "file 子块被吸收而非丢弃 — 不发误报损失警告:{:?}",
            result.warnings
        );
    }

    /// Swift `testPlaceholderInsideRegularContentRoundTrips` — 占位块
    /// 混在普通正文里,块序不变、id 逐字保留。
    #[test]
    fn placeholder_inside_regular_content_round_trips() {
        let md = "# 项目状态\n\n正文段落。\n\n<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR 进度表\nurl: https://example.feishu.cn/sheets/shtcnYYY\n-->\n\n- 上方是当前进度\n- 下方是风险点";
        let blocks = body(&to_feishu_blocks(md));
        assert_eq!(blocks.len(), 5);
        assert!(
            matches!(blocks[0].payload, Payload::Heading { .. }),
            "expected heading, got {:?}",
            blocks[0].payload
        );
        assert!(
            matches!(blocks[1].payload, Payload::Text(_)),
            "expected text, got {:?}",
            blocks[1].payload
        );
        let Payload::Placeholder(payload) = &blocks[2].payload else {
            panic!("expected placeholder, got {:?}", blocks[2].payload)
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Sheet);
        assert_eq!(blocks[2].block_id, "doxbcXXX_blk001");
        assert!(
            matches!(blocks[3].payload, Payload::Bullet(_)),
            "expected bullet, got {:?}",
            blocks[3].payload
        );
        assert!(
            matches!(blocks[4].payload, Payload::Bullet(_)),
            "expected bullet, got {:?}",
            blocks[4].payload
        );
        // 反向:再生 markdown 含逐字节魔法注释。
        let regenerated = to_markdown(&to_feishu_blocks(md));
        assert!(regenerated.contains("<!-- feishu-placeholder"));
        assert!(regenerated.contains("block_id: doxbcXXX_blk001"));
    }

    // MARK: - 真机 wire 夹具(FeishuBlockEncoderTests)

    /// Swift `testRealWireVideoSurvivesFullPullConversion` — 2026-07-28
    /// 真机拉取的最小 wire 形态:page(view_type 未带的页块)+ view
    /// (block_type 33)+ file(block_type 23)。解码 → converter 全链
    /// 必须产出视频占位卡,且不把 file 子块误报成丢块。
    #[test]
    fn real_wire_video_survives_full_pull_conversion() {
        let page_wire = json!({
            "block_id": "JS04dEQuRoAd4DxBtplcYwWWn4v",
            "block_type": 1,
            "children": ["UZC5dlS3nojQ2XxkKFfcDF0wnVf"],
            "page": { "elements": [] },
        });
        let view_wire = json!({
            "block_id": "UZC5dlS3nojQ2XxkKFfcDF0wnVf",
            "block_type": 33,
            "parent_id": "JS04dEQuRoAd4DxBtplcYwWWn4v",
            "view": { "view_type": 2 },
            "children": ["WPTld9a5joj1bPxGa5WcD64fnbh"],
        });
        let file_wire = json!({
            "block_id": "WPTld9a5joj1bPxGa5WcD64fnbh",
            "block_type": 23,
            "parent_id": "UZC5dlS3nojQ2XxkKFfcDF0wnVf",
            "file": { "name": "20260728-172558.mp4", "token": "MzMvbKV31opGhoxYpgMcbN98nSg" },
        });
        let wire = [page_wire, view_wire, file_wire];
        let blocks: Vec<FeishuBlock> = wire
            .iter()
            .map(|d| encoder::decode_block_envelope(d).unwrap())
            .collect();
        let result = to_markdown_with_warnings(&blocks);
        assert!(
            result.value.contains("type: video"),
            "拉取 markdown 必须带视频占位:\n---\n{}",
            result.value
        );
        assert!(
            result
                .value
                .contains("block_token: MzMvbKV31opGhoxYpgMcbN98nSg"),
            "真实 token 应从被吸收的 file 子块提升:\n{}",
            result.value
        );
        assert!(
            result.value.contains("title: 20260728-172558.mp4"),
            "卡片标题应为上传文件名:\n{}",
            result.value
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| matches!(w, ConversionWarning::NestedContentDroppedInPlaceholder { .. })),
            "file 子块被吸收而非丢弃 — 不发误报损失警告:{:?}",
            result.warnings
        );
    }

    // MARK: - Rust 侧补充(Swift 未覆盖的入口行为)

    /// 页块标题前置为正文 H1(v2-9b 标题绑定,拉取侧)。空标题不前置。
    /// Swift 侧此行为由 PullCoordinator 测试间接覆盖;Rust 把入口收在
    /// converter 里,直接钉在这里。
    #[test]
    fn page_title_prepends_leading_h1() {
        let blocks = vec![
            mk(
                "p",
                None,
                Some(vec!["t1"]),
                Payload::Page(PagePayload { title: tp("文档标题") }),
            ),
            textb("t1", "p", "body"),
        ];
        let md = to_markdown(&blocks);
        assert!(md.starts_with("# 文档标题"), "{md}");
        assert!(md.contains("body"), "{md}");

        // 空标题:不前置空 H1,正文原样开头。
        let empty_title = vec![
            mk(
                "p",
                None,
                Some(vec!["t1"]),
                Payload::Page(PagePayload { title: TextPayload::default() }),
            ),
            textb("t1", "p", "body"),
        ];
        let md = to_markdown(&empty_title);
        assert!(!md.contains('#'), "空标题不得前置空 H1:{md}");
        assert!(md.contains("body"), "{md}");
    }

    /// 缺页块根 / 空输入:渲染空文档,不 panic、无警告 — 拉取到半截
    /// 数据时用户看到空文档而非崩溃。
    #[test]
    fn missing_page_root_renders_empty_doc() {
        let result = to_markdown_with_warnings(&[textb("t1", "p", "orphan")]);
        assert_eq!(result.value, empty_md());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);

        let result = to_markdown_with_warnings(&[]);
        assert_eq!(result.value, empty_md());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }
}
