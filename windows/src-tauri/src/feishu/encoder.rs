//! `FeishuBlockEncoder.swift` 的移植 — 飞书 docx 块 API 的 wire
//! 编解码器。
//!
//! 编码方向(`FeishuBlock` → JSON)是 `POST …/blocks/{parent_id}/
//! descendant` 端点消费的形状:
//!
//! ```json
//! { "index": -1,
//!   "children_id": [<页块的顶层合成 ID>],
//!   "descendants": [<每个非页块,见 encode_block_envelope>] }
//! ```
//!
//! 解码方向(JSON → `FeishuBlock`)逐块处理 GET `/blocks` 列表返回。
//! 未知 `block_type` 回落 [`Payload::Divider`],拉取路径不会因飞书
//! schema 新增而硬失败(日志可诊断)。
//!
//! 线上事实(均在真机核实,注释逐条对应 Swift 来源):
//! - 顶层 descendants 必须剥掉 `parent_id`(1770001,见
//!   [`encode_descendant_body_at`]);
//! - 空 elements 合成单个空内容 textRun(1770001,见 [`encode_elements`]);
//! - 5 个样式布尔全部显式写出(99992402,见 [`encode_style`]);
//! - callout 背景色必须是 int 1-15(99992402),经
//!   [`callout::light_color_number_for_name`] 中转;
//! - page PATCH 载荷无 `text_element_style` 的例外由 F2 的
//!   update_document_title 处理(复用 [`encode_elements`])。

use std::collections::HashSet;
use std::fmt;

use serde_json::{json, Map, Value};

use crate::feishu::block::{
    CodePayload, FeishuBlock, ImagePayload, PagePayload, Payload, PlaceholderPayload,
    PlaceholderSubtype, TablePayload, TextElement, TextElementStyle, TextPayload, TextRun,
};
use crate::feishu::callout;

/// 编解码错误(Swift `FeishuBlockEncodingError`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockEncodingError {
    /// 输入没有页(根)块 — converter 恒产出页块,这是程序员错误,
    /// 不是可恢复的 wire 错误。
    MissingPageRoot,
    /// 块信封缺 `block_id` / `block_type` 或类型不对。
    MalformedEnvelope,
}

impl fmt::Display for BlockEncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockEncodingError::MissingPageRoot => {
                write!(f, "块列表缺少页根块(converter 应恒产出)")
            }
            BlockEncodingError::MalformedEnvelope => {
                write!(f, "飞书块信封格式非法(缺 block_id / block_type)")
            }
        }
    }
}

impl std::error::Error for BlockEncodingError {}

/// 编码 descendant 请求体,`index = -1`(追加,段式推送之外的标准形态)。
pub fn encode_descendant_body(blocks: &[FeishuBlock]) -> Result<Value, BlockEncodingError> {
    encode_descendant_body_at(blocks, -1)
}

/// 编码 descendant 请求体;`index` 非负时是段式推送用的显式插入位置。
///
/// **顶层 parent_id 剥离**(#50 切片改写,1770001 真机事实):顶层
/// descendant 的 `parent_id` 指向 converter 合成的页块 ID
/// (如 "blk_000001"),飞书侧不存在该块,严格校验器以 1770001 拒收
/// 整个 body。端点已把真实父块 ID 作为路径参数收到,根级 parent_id
/// 纯属冗余 — 顶层剥掉;嵌套 descendant(表格 cell → text 等)的
/// `parent_id` 是 descendants 数组内部引用,校验器确实会查一致性,
/// 保留。(2026-05-24 含表格块的真机推送暴露;单块载荷常带着无意义
/// 根 parent_id 侥幸通过,表格因嵌套让校验器突然开始在意。)
pub fn encode_descendant_body_at(
    blocks: &[FeishuBlock],
    index: i64,
) -> Result<Value, BlockEncodingError> {
    let page = blocks
        .iter()
        .find(|b| matches!(b.payload, Payload::Page(_)))
        .ok_or(BlockEncodingError::MissingPageRoot)?;
    let top_level_ids: HashSet<&str> = page
        .children
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();

    let mut descendants: Vec<Value> = Vec::new();
    for block in blocks {
        if matches!(block.payload, Payload::Page(_)) {
            continue;
        }
        for mut envelope in encode_block_to_wire(block) {
            let is_top_level = envelope["block_id"]
                .as_str()
                .is_some_and(|id| top_level_ids.contains(id));
            if is_top_level {
                if let Some(obj) = envelope.as_object_mut() {
                    obj.remove("parent_id");
                }
            }
            descendants.push(envelope);
        }
    }
    Ok(json!({
        "index": index,
        "children_id": page.children.clone().unwrap_or_default(),
        "descendants": descendants,
    }))
}

/// 一个类型化块 → 一个或多个 wire 对象。多数块发单个信封;`.quote`
/// 展开成两个 — 飞书把引用建模为 quote_container(block_type 34,空
/// 载荷),可见文字在子文本块(block_type 2)上;把文字内联到容器上
/// 会触发 99992402 字段校验失败。
fn encode_block_to_wire(block: &FeishuBlock) -> Vec<Value> {
    if let Payload::Quote(text) = &block.payload {
        return encode_quote_container(block, text);
    }
    vec![encode_block_envelope(block)]
}

/// 单块的 descendant 信封:`{ block_id, block_type, parent_id?,
/// children?, <载荷键>: <载荷体> }`。
fn encode_block_envelope(block: &FeishuBlock) -> Value {
    let mut dict = json!({
        "block_id": block.block_id,
        "block_type": block.block_type(),
    });
    if let Some(parent) = &block.parent_id {
        dict["parent_id"] = json!(parent);
    }
    if let Some(children) = &block.children {
        if !children.is_empty() {
            dict["children"] = json!(children);
        }
    }
    let (key, payload) = encode_payload(&block.payload);
    if let Some(payload) = payload {
        dict[key] = payload;
    }
    dict
}

/// 引用展开:容器 + 合成子文本块。子文本块 ID 确定性派生
/// (`<quoteId>_qtxt`),同一输入再编码产出相同 wire ID — 真机联调
/// 时 diff 稳定。
fn encode_quote_container(block: &FeishuBlock, text: &TextPayload) -> Vec<Value> {
    let text_child_id = format!("{}_qtxt", block.block_id);
    let mut children = vec![text_child_id.clone()];
    children.extend(block.children.clone().unwrap_or_default());
    let mut container = json!({
        "block_id": block.block_id,
        "block_type": 34,
        "children": children,
        "quote_container": {},
    });
    if let Some(parent) = &block.parent_id {
        container["parent_id"] = json!(parent);
    }
    let text_child = json!({
        "block_id": text_child_id,
        "block_type": 2,
        "parent_id": block.block_id,
        "text": { "elements": encode_elements(&text.elements) },
    });
    vec![container, text_child]
}

// MARK: - 载荷编码

/// 返回 `(字段名, 载荷体或 None)`。`divider` 带空对象 — 飞书要求
/// 字段在场,即使没有体。
fn encode_payload(payload: &Payload) -> (String, Option<Value>) {
    match payload {
        Payload::Page(p) => (
            "page".into(),
            Some(json!({ "elements": encode_elements(&p.title.elements) })),
        ),
        Payload::Text(t) => ("text".into(), Some(encode_text_body(t))),
        Payload::Heading { level, text } => {
            // 飞书 block_type 3..8 对应标题 1..6,载荷字段名
            // heading1 … heading6。
            (
                format!("heading{level}"),
                Some(encode_text_body(text)),
            )
        }
        Payload::Bullet(t) => ("bullet".into(), Some(encode_text_body(t))),
        Payload::Ordered(t) => ("ordered".into(), Some(encode_text_body(t))),
        Payload::Quote(_) => {
            // 正常路径在 encode_quote_container 展开;此分支只在调用方
            // 直接对 .quote 调 encode_block_envelope 时兜底 — 发空容器
            // 形状,至少过校验。
            ("quote_container".into(), Some(json!({})))
        }
        Payload::Todo { text, done } => {
            // done 挂在块级 style 子对象里。
            let mut body = encode_text_body(text);
            body["style"] = json!({ "done": done });
            ("todo".into(), Some(body))
        }
        Payload::Code(c) => {
            // 飞书 wire 要求 style.language 是 Int 枚举(1..73 映射
            // Swift / Python 等)— 字符串触发 99992402(2026-05-24
            // 真机)。语言表未接线前发无语言块,先让正文落上飞书。
            // converter 的 c.language 在类型模型上保留,markdown 往返
            // 靠它 — 只有线上输出丢弃。`wrap: false` 对齐飞书回显默认。
            let _ = &c.language;
            (
                "code".into(),
                Some(json!({
                    "elements": encode_elements(&c.elements),
                    "style": { "wrap": false },
                })),
            )
        }
        Payload::Divider => ("divider".into(), Some(json!({}))),
        Payload::Image(img) => {
            // `src` 有意不发:飞书只收 image_token(上传后才有)。
            // 无 token 的图片到不了这里 — converter 已剥离。
            let mut body = Map::new();
            if let Some(token) = &img.token {
                body.insert("token".into(), json!(token));
            }
            if let Some(width) = img.width {
                body.insert("width".into(), json!(width));
            }
            if let Some(height) = img.height {
                body.insert("height".into(), json!(height));
            }
            ("image".into(), Some(Value::Object(body)))
        }
        Payload::Callout(c) => {
            let mut body = Map::new();
            if let Some(emoji) = &c.emoji {
                body.insert("emoji_id".into(), json!(emoji));
            }
            if let Some(bg) = &c.background_color {
                // 线上要求 int 1-15,发 "light-blue" 字符串触发
                // 99992402(2026-05-30 真机)。未知名(调色板新增 /
                // 手误)静默省略 — 与 nil 同效,绝不阻塞推送。
                if let Some(num) = callout::light_color_number_for_name(bg) {
                    body.insert("background_color".into(), json!(num));
                }
            }
            ("callout".into(), Some(Value::Object(body)))
        }
        Payload::Table(t) => (
            "table".into(),
            Some(json!({
                "property": {
                    "row_size": t.row_size,
                    "column_size": t.column_size,
                    "header_row": t.header_row,
                },
            })),
        ),
        Payload::TableCell => ("table_cell".into(), Some(json!({}))),
        Payload::Placeholder(_) => {
            // preserve_existing 引用形状由推送协调器(F3)发,占位块
            // 不进 wire 编码器。真到了这里,干净丢载荷(nil),不发
            // 飞书物化不了的假 block_type。
            ("placeholder".into(), None)
        }
    }
}

// MARK: - 文本载荷辅助

/// text 载荷字段形状:`{ "elements": [...] }`。
fn encode_text_body(payload: &TextPayload) -> Value {
    json!({ "elements": encode_elements(&payload.elements) })
}

/// 元素数组编码。**空输入合成单个空内容 textRun**,不发空
/// `elements: []` — descendant 端点的严格校验器以 1770001 拒收空
/// elements 列表(2026-05-24 真机:含空表格单元格的文档推送踩中)。
/// 占位 textRun 让 wire 形状合法,飞书侧渲染为空单元格。
///
/// pub:F2 的 `update_document_title` 复用同一 wire 形状组装页块
/// `update_text_elements` 载荷,把严格校验要求的样式默认(见
/// [`encode_style`])收在一处。
pub fn encode_elements(elements: &[TextElement]) -> Vec<Value> {
    if elements.is_empty() {
        return vec![encode_element(&TextElement::TextRun(TextRun::new("")))];
    }
    elements.iter().map(encode_element).collect()
}

fn encode_element(element: &TextElement) -> Value {
    let TextElement::TextRun(run) = element;
    json!({
        "text_run": {
            "content": run.content,
            "text_element_style": encode_style(&run.style),
        }
    })
}

/// 真机响应(2026-05-24)恒回显全部 5 个布尔,含模型未跟踪的
/// `underline`。严格校验器(99992402)拒收缺任一键的样式,所以
/// 每个 text run 全发 — 有 link 时追加。
fn encode_style(style: &TextElementStyle) -> Value {
    let mut dict = json!({
        "bold": style.bold,
        "italic": style.italic,
        "inline_code": style.inline_code,
        "strikethrough": style.strikethrough,
        "underline": false,
    });
    if let Some(link) = &style.link {
        dict["link"] = json!({ "url": link });
    }
    dict
}

// MARK: - 解码

/// 把飞书块 JSON 对象(GET `/blocks` 列表的元素)解码为类型化
/// `FeishuBlock`。未知 block_type 回落 `.divider` — 拉取不因飞书
/// schema 新增硬失败(长期应上升为 converter 警告,目前日志可诊断)。
pub fn decode_block_envelope(dict: &Value) -> Result<FeishuBlock, BlockEncodingError> {
    let block_id = dict
        .get("block_id")
        .and_then(Value::as_str)
        .ok_or(BlockEncodingError::MalformedEnvelope)?;
    let block_type = dict
        .get("block_type")
        .and_then(Value::as_i64)
        .ok_or(BlockEncodingError::MalformedEnvelope)?;
    let parent_id = dict.get("parent_id").and_then(Value::as_str).map(str::to_string);
    // 任一元素不是字符串则整体置 None(对齐 Swift 的 [String] 强转)。
    let children = dict.get("children").and_then(Value::as_array).and_then(|a| {
        a.iter()
            .map(|v| v.as_str())
            .collect::<Option<Vec<_>>>()
            .map(|v| v.into_iter().map(str::to_string).collect())
    });
    Ok(FeishuBlock {
        block_id: block_id.to_string(),
        parent_id,
        children,
        payload: decode_payload(block_type, dict),
    })
}

/// 整数 wire 型号与冻结枚举的分歧(均已实机核实,见
/// `PlaceholderSubtype::block_type` 注释):sheet 30 / bitable 18 /
/// mindnote 29 / 视频 view 33 / iframe 26 / file 23 / board 43。
/// 拉取往返经由子类型名保真,不经整数。
fn decode_payload(block_type: i64, dict: &Value) -> Payload {
    match block_type {
        1 => Payload::Page(PagePayload {
            title: decode_text_payload(&dict["page"]),
        }),
        2 => Payload::Text(decode_text_payload(&dict["text"])),
        3..=8 => {
            let level = (block_type - 2) as u8;
            let body = &dict[format!("heading{level}")];
            Payload::Heading {
                level,
                text: decode_text_payload(body),
            }
        }
        12 => Payload::Bullet(decode_text_payload(&dict["bullet"])),
        13 => Payload::Ordered(decode_text_payload(&dict["ordered"])),
        14 => {
            let body = &dict["code"];
            let style = &body["style"];
            let language = style
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| style.get("language").and_then(Value::as_i64).map(|n| n.to_string()));
            Payload::Code(CodePayload {
                elements: decode_elements(&body["elements"]),
                language,
            })
        }
        // quote_container 本体不带行内文字 — 飞书把引用文字挂在子块
        // (block_type 2)上。此处发空载荷;converter 在树组装时把
        // 容器 + 子块重组为单个 .quote。
        34 => Payload::Quote(TextPayload::default()),
        // 存量 quote 形状,读到为止(不能确定线上还有没有旧文档)。
        15 => Payload::Quote(decode_text_payload(&dict["quote"])),
        17 => {
            let body = &dict["todo"];
            let done = body["style"]["done"].as_bool().unwrap_or(false);
            Payload::Todo {
                text: decode_text_payload(body),
                done,
            }
        }
        19 => {
            let body = &dict["callout"];
            // background_color 线上是 int 1-15(与推送同形),经调色板
            // 表映回规范 "light-*" 名,Done.md 其余代码保持字符串面。
            let bg_name = body
                .get("background_color")
                .and_then(Value::as_i64)
                .and_then(callout::light_color_name_for_number)
                .map(str::to_string)
                // 容忍老/伪响应发字符串;字段缺失则 None。
                .or_else(|| {
                    body.get("background_color")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let emoji = body.get("emoji_id").and_then(Value::as_str).map(str::to_string);
            Payload::Callout(crate::feishu::block::CalloutPayload {
                emoji,
                background_color: bg_name,
            })
        }
        22 => Payload::Divider,
        27 => {
            let body = &dict["image"];
            Payload::Image(ImagePayload {
                token: body.get("token").and_then(Value::as_str).map(str::to_string),
                src: None,
                alt: body.get("alt").and_then(Value::as_str).map(str::to_string),
                width: body.get("width").and_then(Value::as_i64),
                height: body.get("height").and_then(Value::as_i64),
            })
        }
        31 => {
            let body = &dict["table"];
            let prop = &body["property"];
            Payload::Table(TablePayload {
                row_size: prop.get("row_size").and_then(Value::as_u64).unwrap_or(0) as usize,
                column_size: prop.get("column_size").and_then(Value::as_u64).unwrap_or(0) as usize,
                header_row: prop.get("header_row").and_then(Value::as_bool).unwrap_or(true),
            })
        }
        32 => Payload::TableCell,
        // 画板(43):wire 形状 { board: { token } },2026-05-30 与
        // feishu-mcp-pro 核对。
        43 => {
            let token = dict["board"]["token"].as_str().unwrap_or("");
            native_placeholder(
                PlaceholderSubtype::Board,
                non_empty(token),
                "画板",
                None,
                token_url("board", token),
            )
        }
        // 电子表格(30):{ sheet: { token, row_size?, column_size? } }。
        // 注意线上是 30,不是冻结枚举的 24。
        30 => {
            let body = &dict["sheet"];
            let token = body["token"].as_str().unwrap_or("");
            let summary = match (
                body.get("row_size").and_then(Value::as_i64),
                body.get("column_size").and_then(Value::as_i64),
            ) {
                (Some(r), Some(c)) => Some(format!("{r} × {c}")),
                _ => None,
            };
            native_placeholder(
                PlaceholderSubtype::Sheet,
                non_empty(token),
                "电子表格",
                summary,
                token_url("sheet", token),
            )
        }
        // 多维表格(18):{ bitable: { token, view_type? } }。
        18 => {
            let body = &dict["bitable"];
            let token = body["token"].as_str().unwrap_or("");
            let view_label = match body.get("view_type").and_then(Value::as_i64) {
                Some(1) => Some("table".to_string()),
                Some(2) => Some("kanban".to_string()),
                _ => None,
            };
            native_placeholder(
                PlaceholderSubtype::Bitable,
                non_empty(token),
                "多维表格",
                view_label,
                token_url("bitable", token),
            )
        }
        // 思维笔记(29):{ mindnote: { token } }。
        29 => {
            let token = dict["mindnote"]["token"].as_str().unwrap_or("");
            native_placeholder(
                PlaceholderSubtype::Mindnote,
                non_empty(token),
                "思维笔记",
                None,
                token_url("mindnote", token),
            )
        }
        // 嵌入资源(26):{ iframe: { component: { url, iframe_type } } }。
        26 => {
            let url = dict["iframe"]["component"]["url"].as_str().unwrap_or("");
            native_placeholder(
                PlaceholderSubtype::Embed,
                None,
                "嵌入资源",
                None,
                url.to_string(),
            )
        }
        // 附件(23):{ file: { token, name? } }。name 有则作标题。
        23 => {
            let body = &dict["file"];
            let token = body["token"].as_str().unwrap_or("");
            let title = body.get("name").and_then(Value::as_str).unwrap_or("附件");
            native_placeholder(
                PlaceholderSubtype::Attachment,
                non_empty(token),
                title,
                None,
                token_url("file", token),
            )
        }
        // 视图(33):飞书把上传视频(及部分其他行内媒体)包在 view
        // 块里,真实 token + 文件名在其子 file 块(23)上。逐块解码
        // 够不着子块,此处发裸 .video 占位,converter 在树组装时
        // (byId 可用)吸收子块的 token + 名字。2026-08-16 真机核实:
        // 视频 = view(33, view_type 2) → file(23, *.mp4)。没有这个
        // case 时 view 落进 .divider,视频静默丢失(违反 ADR-0007
        // 「不丢信息」)。
        33 => native_placeholder(PlaceholderSubtype::Video, None, "视频", None, String::new()),
        // 未知/未支持型号(chat-card、equation、grid / grid-column、
        // OKR 家族、AddOns、JiraIssue、SyncedBlock …)。
        _ => {
            eprintln!("[feishu] pull: unknown block_type {block_type} → divider fallback");
            Payload::Divider
        }
    }
}

fn decode_text_payload(body: &Value) -> TextPayload {
    TextPayload {
        elements: decode_elements(&body["elements"]),
    }
}

fn decode_elements(raw: &Value) -> Vec<TextElement> {
    raw.as_array()
        .map(|arr| arr.iter().filter_map(decode_element).collect())
        .unwrap_or_default()
}

fn decode_element(raw: &Value) -> Option<TextElement> {
    let run = &raw["text_run"];
    let content = run.get("content").and_then(Value::as_str)?;
    let style_dict = &run["text_element_style"];
    // Done.md 的 schema 尚无颜色/高亮 mark,飞书侧 text_color /
    // background_color 进站即失。值不保留(没有槽位),但打标记,
    // converter 借此向用户报「N 处文字颜色未保留」(#59)。
    let had_stripped_color = {
        let text_color = &style_dict["text_color"];
        let background_color = &style_dict["background_color"];
        !text_color.is_null() || !background_color.is_null()
    };
    let style = TextElementStyle {
        bold: style_dict["bold"].as_bool().unwrap_or(false),
        italic: style_dict["italic"].as_bool().unwrap_or(false),
        inline_code: style_dict["inline_code"].as_bool().unwrap_or(false),
        strikethrough: style_dict["strikethrough"].as_bool().unwrap_or(false),
        link: style_dict["link"]["url"].as_str().map(str::to_string),
        had_stripped_feishu_color: had_stripped_color,
    };
    Some(TextElement::TextRun(TextRun {
        content: content.to_string(),
        style,
    }))
}

/// 占位块解码装配器:`created_in_feishu_at` / `unknown_fields` 拉取侧
/// 恒空(它们只经魔法注释往返)。
fn native_placeholder(
    subtype: PlaceholderSubtype,
    block_token: Option<String>,
    title: &str,
    summary: Option<String>,
    url: String,
) -> Payload {
    Payload::Placeholder(PlaceholderPayload {
        subtype,
        block_token,
        title: title.to_string(),
        summary,
        url,
        created_in_feishu_at: None,
        unknown_fields: Vec::new(),
    })
}

/// token → `feishu://<segment>/<token>`;空 token 给空 url(可推导性
/// 由魔法注释层的 canonical_url 兜底)。
fn token_url(segment: &str, token: &str) -> String {
    if token.is_empty() {
        String::new()
    } else {
        format!("feishu://{segment}/{token}")
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    //! Swift `FeishuBlockEncoderTests` 的移植(除依赖 converter 的
    //! 全链路视频用例 — 该例随 F1.4 的 markdown 入口移植)。
    //! 飞书严格校验器只回 1770001「invalid param」不说哪个键错,
    //! 这些用例就是每一条 wire 事实的回归锚。

    use super::*;

    fn page(id: &str, children: &[&str]) -> FeishuBlock {
        FeishuBlock {
            block_id: id.into(),
            parent_id: None,
            children: Some(children.iter().map(|s| s.to_string()).collect()),
            payload: Payload::Page(PagePayload::default()),
        }
    }

    fn text_block(id: &str, parent: &str, content: &str) -> FeishuBlock {
        FeishuBlock {
            block_id: id.into(),
            parent_id: Some(parent.into()),
            children: None,
            payload: Payload::Text(TextPayload {
                elements: vec![TextElement::TextRun(TextRun::new(content))],
            }),
        }
    }

    fn empty_text_block(id: &str, parent: &str) -> FeishuBlock {
        FeishuBlock {
            block_id: id.into(),
            parent_id: Some(parent.into()),
            children: None,
            payload: Payload::Text(TextPayload::default()),
        }
    }

    fn descendants(body: &Value) -> &Vec<Value> {
        body["descendants"].as_array().unwrap()
    }

    fn by_id(body: &Value) -> std::collections::HashMap<&str, &Value> {
        descendants(body)
            .iter()
            .map(|env| (env["block_id"].as_str().unwrap(), env))
            .collect()
    }

    /// 顶层 descendants 不得带 `parent_id` — 1770001 的直接回归。
    #[test]
    fn top_level_descendants_have_no_parent_id() {
        let blocks = vec![
            page("blk_001", &["blk_002"]),
            text_block("blk_002", "blk_001", "x"),
        ];
        let body = encode_descendant_body(&blocks).unwrap();
        let descs = descendants(&body);
        assert_eq!(descs.len(), 1);
        assert!(
            descs[0].get("parent_id").is_none(),
            "顶层 descendant 必须剥 parent_id — 根级 parent_id 与路径参数不符时校验器拒收"
        );
    }

    /// 嵌套 descendant(表格 cell 指向表格父、cell 内文字)保留
    /// `parent_id` — 校验器查 descendants 数组内部一致性。
    #[test]
    fn nested_descendants_keep_parent_id() {
        let blocks = vec![
            page("blk_001", &["blk_table"]),
            FeishuBlock {
                block_id: "blk_table".into(),
                parent_id: Some("blk_001".into()),
                children: Some(vec!["blk_cell".into()]),
                payload: Payload::Table(TablePayload {
                    row_size: 1,
                    column_size: 1,
                    header_row: true,
                }),
            },
            FeishuBlock {
                block_id: "blk_cell".into(),
                parent_id: Some("blk_table".into()),
                children: Some(vec!["blk_text".into()]),
                payload: Payload::TableCell,
            },
            text_block("blk_text", "blk_cell", "x"),
        ];
        let body = encode_descendant_body(&blocks).unwrap();
        let map = by_id(&body);
        // 顶层表格:parent_id 已剥。
        assert!(map["blk_table"].get("parent_id").is_none());
        // 嵌套 cell:parent_id == 表格(数组内部引用)。
        assert_eq!(map["blk_cell"]["parent_id"].as_str(), Some("blk_table"));
        // cell 内文字:parent_id == cell。
        assert_eq!(map["blk_text"]["parent_id"].as_str(), Some("blk_cell"));
    }

    /// children_id 来自页块 children,按文档序;多个顶层块全部剥
    /// parent_id,不止第一个。
    #[test]
    fn all_top_level_descendants_have_parent_id_stripped() {
        let blocks = vec![
            page("blk_001", &["blk_a", "blk_b", "blk_c"]),
            text_block("blk_a", "blk_001", "a"),
            FeishuBlock {
                block_id: "blk_b".into(),
                parent_id: Some("blk_001".into()),
                children: None,
                payload: Payload::Heading {
                    level: 1,
                    text: TextPayload {
                        elements: vec![TextElement::TextRun(TextRun::new("b"))],
                    },
                },
            },
            FeishuBlock {
                block_id: "blk_c".into(),
                parent_id: Some("blk_001".into()),
                children: None,
                payload: Payload::Divider,
            },
        ];
        let body = encode_descendant_body(&blocks).unwrap();
        for env in descendants(&body) {
            assert!(
                env.get("parent_id").is_none(),
                "所有顶层 descendant 都剥 parent_id — {} 带了",
                env["block_id"]
            );
        }
        assert_eq!(body["children_id"], json!(["blk_a", "blk_b", "blk_c"]));
    }

    /// `index` 往返:-1 追加(默认),非负是段式推送的显式插入位。
    #[test]
    fn index_round_trips() {
        let blocks = vec![page("blk_001", &["blk_a"]), text_block("blk_a", "blk_001", "x")];
        let append_body = encode_descendant_body(&blocks).unwrap();
        assert_eq!(append_body["index"].as_i64(), Some(-1), "默认 = 追加");
        let insert_body = encode_descendant_body_at(&blocks, 3).unwrap();
        assert_eq!(insert_body["index"].as_i64(), Some(3));
    }

    /// 空文本载荷(空表格 cell 等)必须编码为单个空内容 textRun,
    /// 不能是空 elements 数组 — 真机 1770001。
    #[test]
    fn empty_text_payload_encodes_empty_text_run_not_empty_array() {
        let blocks = vec![
            page("blk_001", &["blk_empty"]),
            empty_text_block("blk_empty", "blk_001"),
        ];
        let body = encode_descendant_body(&blocks).unwrap();
        let elements = descendants(&body)[0]["text"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1, "空文本载荷 = 单个空内容 textRun");
        assert_eq!(elements[0]["text_run"]["content"].as_str(), Some(""));
    }

    /// 表格空单元格 — 真机 1770001 案例的直接回归。
    #[test]
    fn empty_table_cell_text_encodes_non_empty_elements_array() {
        let blocks = vec![
            page("blk_001", &["blk_table"]),
            FeishuBlock {
                block_id: "blk_table".into(),
                parent_id: Some("blk_001".into()),
                children: Some(vec!["blk_cell".into()]),
                payload: Payload::Table(TablePayload {
                    row_size: 1,
                    column_size: 1,
                    header_row: true,
                }),
            },
            FeishuBlock {
                block_id: "blk_cell".into(),
                parent_id: Some("blk_table".into()),
                children: Some(vec!["blk_emptyText".into()]),
                payload: Payload::TableCell,
            },
            empty_text_block("blk_emptyText", "blk_cell"),
        ];
        let body = encode_descendant_body(&blocks).unwrap();
        let map = by_id(&body);
        let elements = map["blk_emptyText"]["text"]["elements"].as_array().unwrap();
        assert!(
            !elements.is_empty(),
            "空表格 cell 文字绝不能序列化为 elements:[] — 飞书 1770001 拒收"
        );
    }

    // MARK: - callout 颜色往返(真机 99992402)

    /// 推送体里 callout.background_color 必须是 int 1-15 — 发
    /// "light-blue" 字符串真机回 99992402(2026-05-30)。编码器对
    /// model 的 emoji 透传(命名 ID 由 converter 落进模型)。
    #[test]
    fn callout_background_color_encodes_as_int() {
        let blocks = vec![
            page("p", &["c1"]),
            FeishuBlock {
                block_id: "c1".into(),
                parent_id: Some("p".into()),
                children: None,
                payload: Payload::Callout(crate::feishu::block::CalloutPayload {
                    emoji: Some("💡".into()),
                    background_color: Some("light-blue".into()),
                }),
            },
        ];
        let body = encode_descendant_body(&blocks).unwrap();
        let callout = &descendants(&body)[0]["callout"];
        assert_eq!(
            callout["background_color"].as_i64(),
            Some(5),
            "light-blue 必须编码为 5;字符串形回 99992402"
        );
        assert_eq!(callout["emoji_id"].as_str(), Some("💡"));
    }

    /// 拉取侧:int 4 解回规范名 "light-green"(FeishuCalloutType 按
    /// 字符串分发)。
    #[test]
    fn callout_background_color_decodes_from_int() {
        let dict = json!({
            "block_id": "c1",
            "block_type": 19,
            "parent_id": "p",
            "callout": { "emoji_id": "✨", "background_color": 4 },
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Callout(payload) = block.payload else {
            panic!("期望 callout 载荷");
        };
        assert_eq!(payload.background_color.as_deref(), Some("light-green"));
        assert_eq!(payload.emoji.as_deref(), Some("✨"));
    }

    /// 往返:规范名 → int → 规范名,钉死 Done.md 关心的全部 7 色。
    #[test]
    fn callout_color_round_trips_across_all_canonical_colors() {
        let canonical = [
            ("light-red", 1i64),
            ("light-orange", 2),
            ("light-yellow", 3),
            ("light-green", 4),
            ("light-blue", 5),
            ("light-purple", 6),
            ("light-gray", 7),
        ];
        for (name, expected) in canonical {
            let blocks = vec![
                page("p", &["c"]),
                FeishuBlock {
                    block_id: "c".into(),
                    parent_id: Some("p".into()),
                    children: None,
                    payload: Payload::Callout(crate::feishu::block::CalloutPayload {
                        emoji: None,
                        background_color: Some(name.into()),
                    }),
                },
            ];
            let body = encode_descendant_body(&blocks).unwrap();
            let callout = &descendants(&body)[0]["callout"];
            assert_eq!(callout["background_color"].as_i64(), Some(expected), "{name}");
        }
    }

    // MARK: - 占位块解码(#19)

    /// 画板(43)真机 wire 形状(mcp doc_list_blocks 抓取)。必须
    /// 解出 .placeholder(.board),不能落 .divider(v2-9a-step1'
    /// 的 default case 曾把所有画板静默丢掉)。
    #[test]
    fn decode_board_block_produces_board_placeholder() {
        let dict = json!({
            "block_id": "doxc_BOARD",
            "block_type": 43,
            "parent_id": "page_X",
            "board": { "token": "BOARD_TOKEN_xyz" },
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Placeholder(payload) = block.payload else {
            panic!("期望占位载荷,得到 {:?}", block.payload);
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Board);
        assert_eq!(payload.block_token.as_deref(), Some("BOARD_TOKEN_xyz"));
        assert_eq!(payload.title, "画板", "飞书侧无名时,默认标题给出本地化提示");
        assert_eq!(payload.url, "feishu://board/BOARD_TOKEN_xyz");
    }

    /// 电子表格(30 — 与冻结枚举的 24 分歧,推送侧保持 24 直到
    /// ADR 协调)。
    #[test]
    fn decode_sheet_block_produces_sheet_placeholder() {
        let dict = json!({
            "block_id": "doxc_SHEET",
            "block_type": 30,
            "parent_id": "page_X",
            "sheet": { "token": "SHEET_TOKEN_xyz", "row_size": 10, "column_size": 5 },
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Placeholder(payload) = block.payload else {
            panic!("期望占位载荷");
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Sheet);
        assert_eq!(payload.block_token.as_deref(), Some("SHEET_TOKEN_xyz"));
        assert_eq!(payload.summary.as_deref(), Some("10 × 5"), "行列数进占位摘要");
    }

    /// 多维表格(18)。
    #[test]
    fn decode_bitable_block_produces_bitable_placeholder() {
        let dict = json!({
            "block_id": "doxc_BITABLE",
            "block_type": 18,
            "parent_id": "page_X",
            "bitable": { "token": "BITABLE_TOKEN", "view_type": 2 },
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Placeholder(payload) = block.payload else {
            panic!("期望占位载荷");
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Bitable);
        assert_eq!(payload.block_token.as_deref(), Some("BITABLE_TOKEN"));
        assert_eq!(payload.summary.as_deref(), Some("kanban"), "view_type 2 → 摘要 kanban");
    }

    /// 思维笔记(29)。
    #[test]
    fn decode_mindnote_block_produces_mindnote_placeholder() {
        let dict = json!({
            "block_id": "doxc_MINDNOTE",
            "block_type": 29,
            "parent_id": "page_X",
            "mindnote": { "token": "MINDNOTE_TOKEN" },
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Placeholder(payload) = block.payload else {
            panic!("期望占位载荷");
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Mindnote);
        assert_eq!(payload.block_token.as_deref(), Some("MINDNOTE_TOKEN"));
    }

    /// 嵌入资源(26):url 嵌在 iframe.component.url,不在顶层。
    #[test]
    fn decode_iframe_block_produces_embed_placeholder() {
        let dict = json!({
            "block_id": "doxc_IFRAME",
            "block_type": 26,
            "parent_id": "page_X",
            "iframe": {
                "component": { "url": "https://example.com/embed", "iframe_type": 1 },
            },
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Placeholder(payload) = block.payload else {
            panic!("期望占位载荷");
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Embed);
        assert_eq!(payload.url, "https://example.com/embed");
    }

    /// 附件(23):有 name 用 name 作标题,否则本地化兜底。
    #[test]
    fn decode_file_block_produces_attachment_placeholder() {
        let dict = json!({
            "block_id": "doxc_FILE",
            "block_type": 23,
            "parent_id": "page_X",
            "file": { "token": "FILE_TOKEN", "name": "report.pdf" },
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Placeholder(payload) = block.payload else {
            panic!("期望占位载荷");
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Attachment);
        assert_eq!(payload.block_token.as_deref(), Some("FILE_TOKEN"));
        assert_eq!(payload.title, "report.pdf", "文件名有则作占位标题");
    }

    /// 视图(33)— 飞书包上传视频的行内容器。此处发裸 .video
    /// (token + 文件名由 converter 吸收子 file 块);没有这个 case
    /// 时 33 落 .divider,视频静默丢失(ADR-0007「不丢信息」)。
    #[test]
    fn decode_view_block_produces_bare_video_placeholder() {
        let dict = json!({
            "block_id": "doxc_VIEW",
            "block_type": 33,
            "parent_id": "page_X",
            "view": { "view_type": 2 },
            "children": ["doxc_FILE_child"],
        });
        let block = decode_block_envelope(&dict).unwrap();
        let Payload::Placeholder(payload) = block.payload else {
            panic!("期望 .placeholder(.video),得到 {:?}", block.payload);
        };
        assert_eq!(payload.subtype, PlaceholderSubtype::Video);
        assert!(payload.block_token.is_none(), "token 在子 file 块上 — converter 吸收");
        assert_eq!(payload.title, "视频");
        assert_eq!(block.children.as_deref(), Some(&["doxc_FILE_child".to_string()][..]));
    }

    /// 未知 block_type 回落 .divider(安全网 — OKR / Synced 等
    /// schema 新增不硬失败)。
    #[test]
    fn decode_truly_unknown_block_type_falls_through_to_divider() {
        let dict = json!({
            "block_id": "doxc_FUTURE",
            "block_type": 999,
            "parent_id": "page_X",
        });
        let block = decode_block_envelope(&dict).unwrap();
        assert!(matches!(block.payload, Payload::Divider), "未知型号回落 divider");
    }

    /// 引用容器:合成文本子块的 parent 是引用块 — 嵌套关系永在,
    /// 内部 parent_id 必须保留。
    #[test]
    fn quote_container_nested_text_keeps_parent_id() {
        let blocks = vec![
            page("blk_001", &["blk_quote"]),
            FeishuBlock {
                block_id: "blk_quote".into(),
                parent_id: Some("blk_001".into()),
                children: None,
                payload: Payload::Quote(TextPayload {
                    elements: vec![TextElement::TextRun(TextRun::new("q"))],
                }),
            },
        ];
        let body = encode_descendant_body(&blocks).unwrap();
        let map = by_id(&body);
        // 顶层 quote_container:parent_id 已剥。
        assert!(map["blk_quote"].get("parent_id").is_none());
        // 合成文本子块(blk_quote_qtxt):parent_id 保留。
        assert_eq!(
            map["blk_quote_qtxt"]["parent_id"].as_str(),
            Some("blk_quote"),
            "合成的引用文本子块指向其容器"
        );
    }

    /// 缺 block_id / block_type → MalformedEnvelope(对齐 Swift
    /// decodeBlockEnvelope 的 guard)。
    #[test]
    fn decode_rejects_malformed_envelope() {
        let missing_id = json!({ "block_type": 2 });
        assert_eq!(
            decode_block_envelope(&missing_id).unwrap_err(),
            BlockEncodingError::MalformedEnvelope
        );
        let missing_type = json!({ "block_id": "x" });
        assert_eq!(
            decode_block_envelope(&missing_type).unwrap_err(),
            BlockEncodingError::MalformedEnvelope
        );
        // block_type 非整数(字符串)同样拒。
        let string_type = json!({ "block_id": "x", "block_type": "2" });
        assert_eq!(
            decode_block_envelope(&string_type).unwrap_err(),
            BlockEncodingError::MalformedEnvelope
        );
    }

    /// 没有页根块 → MissingPageRoot(converter 恒产出页块,此为
    /// 程序员错误)。
    #[test]
    fn encode_without_page_root_is_missing_page_root() {
        let blocks = vec![text_block("blk_002", "blk_001", "x")];
        assert_eq!(
            encode_descendant_body(&blocks).unwrap_err(),
            BlockEncodingError::MissingPageRoot
        );
    }
}
