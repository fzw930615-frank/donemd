//! `FeishuBlock.swift` 的移植 — 飞书 docx OpenAPI(`raw_content` / 块树)
//! 返回的 JSON 形状的忠实 Rust 镜像,只建模 Done.md 经
//! `converter::FeishuStructuralConverter` 往返的子集。
//!
//! 飞书块 schema 全集有几十种载荷变体;这里覆盖八类基础块
//! (标题 ×6 / 段落 / 列表 ×2 / 待办 / 引用 / 代码 / 分割线 / 图片)、
//! 富格式扩展(callout 19、表格 31/32)与占位块(ADR-0007 的七类
//! 飞书原生块,无 Markdown 等价物,经 `feishu_placeholder` Tiptap 节点 +
//! `<!-- feishu-placeholder … -->` 魔法注释摆渡)。Mermaid 留在
//! `.code`(language = "mermaid")里共用代码块路径,不需要新载荷变体。
//!
//! 形状是**扁平**的:每个块自带 `parent_id` 与子块 ID 列表,与飞书
//! 在线上序列化文档的方式一致;树的组装发生在 converter,不在这里。

use serde_json::{json, Value};

/// 一个飞书块。`block_type` 整数从 `payload` 派生(见 [`Payload::block_type`]),
/// 不单独存储 — 真源保持在类型化枚举上,两者不可能漂移失配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuBlock {
    pub block_id: String,
    pub parent_id: Option<String>,
    /// 子块 ID,按文档序 — 顺序对渲染与往返等价都重要。
    pub children: Option<Vec<String>>,
    pub payload: Payload,
}

impl FeishuBlock {
    pub fn new(block_id: impl Into<String>, payload: Payload) -> Self {
        FeishuBlock {
            block_id: block_id.into(),
            parent_id: None,
            children: None,
            payload,
        }
    }

    /// `block_type` 整数,派生自载荷。
    pub fn block_type(&self) -> i64 {
        self.payload.block_type()
    }

    /// 占位块的 preserve_existing 引用形状
    /// (`{block_id, block_type, _done_md_directive: "preserve_existing"}`),
    /// 推送时告诉飞书 OpenAPI「引用既有块,不要重建」。非占位块返回
    /// `None` — 调用方以此区分「preserve-existing 引用」与「完整内容」
    /// (Swift `preserveExistingReference`;字段名由 ADR-0007
    /// § 推送时如何还原飞书侧原块 钉死)。
    pub fn preserve_existing_reference(&self) -> Option<Value> {
        let Payload::Placeholder(p) = &self.payload else {
            return None;
        };
        Some(json!({
            "block_id": self.block_id,
            "block_type": p.subtype.as_str(),
            "_done_md_directive": "preserve_existing",
        }))
    }
}

/// 块载荷。枚举即扩展缝:后续新增块类型(mention / equation 等)加
/// 变体即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Page(PagePayload),          // 1
    Text(TextPayload),          // 2
    /// 标题,level 1..6(block_type 3..8)。
    Heading { level: u8, text: TextPayload },
    Bullet(TextPayload),        // 12
    Ordered(TextPayload),       // 13
    Code(CodePayload),          // 14
    /// 引用容器(quote_container,34)。可见文字在子文本块上
    /// (`<quoteId>_qtxt`);编码器负责展开,解码器负责重组。
    Quote(TextPayload),
    Todo { text: TextPayload, done: bool }, // 17
    Callout(CalloutPayload),    // 19
    Divider,                    // 22
    Image(ImagePayload),        // 27
    Table(TablePayload),        // 31
    TableCell,                  // 32
    Placeholder(PlaceholderPayload), // 23/24/25/26/28/33/43(冻结枚举值)
}

impl Payload {
    /// 该载荷对应的 `block_type` 整数(与 Swift `Payload.blockType` 同表)。
    pub fn block_type(&self) -> i64 {
        match self {
            Payload::Page(_) => 1,
            Payload::Text(_) => 2,
            Payload::Heading { level, .. } => 2 + (*level).clamp(1, 6) as i64,
            Payload::Bullet(_) => 12,
            Payload::Ordered(_) => 13,
            Payload::Code(_) => 14,
            Payload::Quote(_) => 34,
            Payload::Todo { .. } => 17,
            Payload::Callout(_) => 19,
            Payload::Divider => 22,
            Payload::Image(_) => 27,
            Payload::Table(_) => 31,
            Payload::TableCell => 32,
            Payload::Placeholder(p) => p.subtype.block_type(),
        }
    }
}

/// 页(根)块,携带标题元素;只往返标题文字。真实飞书页还有很多
/// 标题式属性(封面图等)— 超出范围。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PagePayload {
    pub title: TextPayload,
}

/// 块的行内内容:有序的样式化文本运行(v2-4a 只有 textRun;
/// mention / equation 留作后续扩展)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextPayload {
    pub elements: Vec<TextElement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextElement {
    TextRun(TextRun),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    pub content: String,
    pub style: TextElementStyle,
}

impl TextRun {
    pub fn new(content: impl Into<String>) -> Self {
        TextRun {
            content: content.into(),
            style: TextElementStyle::PLAIN,
        }
    }
}

/// 文本运行的行内样式。拉取侧提示:`had_stripped_feishu_color` 标记
/// 该运行在线上载荷里带过 `text_color` / `background_color`,而
/// Done.md 的 Tiptap schema 尚无颜色/高亮 mark(#59),拉取时剥离、
/// 推送时省略 — 单向诚实,converter 借此向用户报「N 处文字颜色未保留」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextElementStyle {
    pub bold: bool,
    pub italic: bool,
    pub inline_code: bool,
    pub strikethrough: bool,
    pub link: Option<String>,
    pub had_stripped_feishu_color: bool,
}

impl Default for TextElementStyle {
    fn default() -> Self {
        TextElementStyle::PLAIN
    }
}

impl TextElementStyle {
    pub const PLAIN: TextElementStyle = TextElementStyle {
        bold: false,
        italic: false,
        inline_code: false,
        strikethrough: false,
        link: None,
        had_stripped_feishu_color: false,
    };
}

/// 代码块载荷。`language` 是规范化小写语言名(`swift`、`python` 等);
/// 飞书线上用 Int 枚举,但转换发生在解码边界,converter 拿到的已是
/// 字符串,不需要内置完整语言表。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodePayload {
    pub elements: Vec<TextElement>,
    pub language: Option<String>,
}

/// 高亮块(callout,block_type 19)。携带飞书渲染的视觉样式
/// (`emoji` + `background_color`);实际子块(段落/列表/标题/嵌套引用)
/// 在常规 `children` ID 列表里。按飞书硬性限制(见 CONTEXT.md
/// § 高亮块),callout 内禁止代码块/表格/图片/分割线 — converter 负责把关。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CalloutPayload {
    /// 飞书显示的 emoji(`💡`、`⚠️` 等)。线上格式可选,部分存量
    /// callout 不带。**推送时这里放的是 wire 命名 ID**(`bulb`、
    /// `warning` 等,见 [`crate::feishu::callout`] — 发 Unicode 字形
    /// 会被飞书以 1770006 拒收)。
    pub emoji: Option<String>,
    /// `light-blue` / `light-green` / `light-purple` / `light-yellow` /
    /// `light-red` 等(飞书 MD 语法参考)。
    pub background_color: Option<String>,
}

/// 表格块(block_type 31)。子块是行主序的 `table_cell`(长度必须等于
/// `row_size × column_size`)。Done.md 不支持单元格合并;GFM 表格必须有
/// 表头行,所以本地侧 `header_row` 恒为 `true`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePayload {
    pub row_size: usize,
    pub column_size: usize,
    pub header_row: bool,
}

impl Default for TablePayload {
    fn default() -> Self {
        TablePayload {
            row_size: 0,
            column_size: 0,
            header_row: true,
        }
    }
}

/// ADR-0007 冻结的七类飞书原生块子类型 — Done.md 以不透明引用摆渡,
/// 内容始终留在飞书侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaceholderSubtype {
    Attachment, // 23
    Sheet,      // 24
    Mindnote,   // 25
    Video,      // 26
    Bitable,    // 28
    /// 第三方 iframe / jira 等。
    Embed,      // 33
    /// 飞书原生画板(≠ mermaid)。
    Board,      // 43
}

impl PlaceholderSubtype {
    /// `CaseIterable` 对应物。
    pub const ALL: [PlaceholderSubtype; 7] = [
        PlaceholderSubtype::Attachment,
        PlaceholderSubtype::Sheet,
        PlaceholderSubtype::Mindnote,
        PlaceholderSubtype::Video,
        PlaceholderSubtype::Bitable,
        PlaceholderSubtype::Embed,
        PlaceholderSubtype::Board,
    ];

    /// Swift 的 `rawValue` — 魔法注释 `type:` 字段与 frontmatter
    /// `placeholder_blocks[].type` 用的就是这个名字;拉取往返经由
    /// 子类型名保真,不经过整数。
    pub fn as_str(&self) -> &'static str {
        match self {
            PlaceholderSubtype::Attachment => "attachment",
            PlaceholderSubtype::Sheet => "sheet",
            PlaceholderSubtype::Mindnote => "mindnote",
            PlaceholderSubtype::Video => "video",
            PlaceholderSubtype::Bitable => "bitable",
            PlaceholderSubtype::Embed => "embed",
            PlaceholderSubtype::Board => "board",
        }
    }

    pub fn from_name(name: &str) -> Option<PlaceholderSubtype> {
        PlaceholderSubtype::ALL
            .into_iter()
            .find(|s| s.as_str() == name)
    }

    /// 冻结枚举的 `block_type` 整数(ADR-0007)。
    ///
    /// **与真实线上值存在已核实的分歧**(2026-05-30 / 08-16 实测,
    /// 见 `FeishuBlockEncoder.swift` 各 case):拉取解码时 sheet=30、
    /// bitable=18、mindnote=29、视频包在 view(33)里、附件 file=23、
    /// iframe=26、board=43。分歧可接受 — 拉取从不经由这个整数往返
    /// (块 → 魔法注释 → 块走子类型名),推送侧占位块也不进 wire
    /// 编码器(coordinator 过滤后发 preserve_existing 引用)。
    pub fn block_type(&self) -> i64 {
        match self {
            PlaceholderSubtype::Attachment => 23,
            PlaceholderSubtype::Sheet => 24,
            PlaceholderSubtype::Mindnote => 25,
            PlaceholderSubtype::Video => 26,
            PlaceholderSubtype::Bitable => 28,
            PlaceholderSubtype::Embed => 33,
            PlaceholderSubtype::Board => 43,
        }
    }
}

/// 占位块载荷。携带 ADR-0007 承诺往返的全部字段,外加 `unknown_fields`
/// 前向兼容槽(未来飞书 schema 新增的元数据原样往返,顺序不变;
/// 形状同 `markdown::placeholder::FeishuPlaceholder`)。
///
/// 所属 `FeishuBlock::block_id` 是飞书文档里**真实的块 ID**,从不被
/// converter 分配的合成 ID(`blk_%06d`)覆盖 — 推送协调器靠它发
/// preserve_existing 引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderPayload {
    pub subtype: PlaceholderSubtype,
    pub block_token: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub url: String,
    pub created_in_feishu_at: Option<String>,
    /// 未识别的 `key: value` 字段,按原序保留。
    pub unknown_fields: Vec<(String, String)>,
}

/// 图片块载荷。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImagePayload {
    /// 飞书媒体上传接口返回的 `image_token`。本地 authored 的图片
    /// (Markdown `![alt](url)`)在上传前只有 `src`;上传后 `token` 填充。
    pub token: Option<String>,
    pub src: Option<String>,
    pub alt: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_block_types_match_wire_numbers() {
        let cases: Vec<(Payload, i64)> = vec![
            (Payload::Page(PagePayload::default()), 1),
            (Payload::Text(TextPayload::default()), 2),
            (
                Payload::Heading { level: 1, text: TextPayload::default() },
                3,
            ),
            (
                Payload::Heading { level: 6, text: TextPayload::default() },
                8,
            ),
            (Payload::Bullet(TextPayload::default()), 12),
            (Payload::Ordered(TextPayload::default()), 13),
            (Payload::Code(CodePayload::default()), 14),
            (
                Payload::Todo { text: TextPayload::default(), done: false },
                17,
            ),
            (Payload::Callout(CalloutPayload::default()), 19),
            (Payload::Divider, 22),
            (Payload::Image(ImagePayload::default()), 27),
            (
                Payload::Table(TablePayload { row_size: 2, column_size: 3, header_row: true }),
                31,
            ),
            (Payload::TableCell, 32),
            (Payload::Quote(TextPayload::default()), 34),
        ];
        for (payload, expected) in cases {
            let block = FeishuBlock::new("blk_000001", payload);
            assert_eq!(block.block_type(), expected, "payload: {:?}", block.payload);
        }
    }

    #[test]
    fn heading_level_clamps_to_one_through_six() {
        // Swift: 2 + max(1, min(level, 6)) — 越界值钳回 1..6。
        for (level, expected) in [(0u8, 3i64), (1, 3), (6, 8), (7, 8), (255, 8)] {
            let block = FeishuBlock::new(
                "b",
                Payload::Heading { level, text: TextPayload::default() },
            );
            assert_eq!(block.block_type(), expected, "level: {level}");
        }
    }

    #[test]
    fn placeholder_subtypes_use_frozen_adr_numbers() {
        for (subtype, expected) in [
            (PlaceholderSubtype::Attachment, 23i64),
            (PlaceholderSubtype::Sheet, 24),
            (PlaceholderSubtype::Mindnote, 25),
            (PlaceholderSubtype::Video, 26),
            (PlaceholderSubtype::Bitable, 28),
            (PlaceholderSubtype::Embed, 33),
            (PlaceholderSubtype::Board, 43),
        ] {
            assert_eq!(subtype.block_type(), expected, "{subtype:?}");
        }
    }

    #[test]
    fn placeholder_subtype_name_roundtrip() {
        for subtype in PlaceholderSubtype::ALL {
            assert_eq!(PlaceholderSubtype::from_name(subtype.as_str()), Some(subtype));
        }
    }

    #[test]
    fn placeholder_subtype_rejects_unknown_names() {
        assert_eq!(PlaceholderSubtype::from_name("mermaid"), None);
        assert_eq!(PlaceholderSubtype::from_name(""), None);
        // 前后空格不宽容 — 名字来自受控来源(魔法注释/frontmatter)。
        assert_eq!(PlaceholderSubtype::from_name("attachment "), None);
    }

    #[test]
    fn placeholder_payload_reports_frozen_block_type() {
        let payload = PlaceholderPayload {
            subtype: PlaceholderSubtype::Board,
            block_token: Some("bd1".into()),
            title: "画板".into(),
            summary: None,
            url: "feishu://board/bd1".into(),
            created_in_feishu_at: None,
            unknown_fields: Vec::new(),
        };
        let block = FeishuBlock::new("realFeishuId", Payload::Placeholder(payload));
        assert_eq!(block.block_type(), 43);
    }

    #[test]
    fn preserve_existing_reference_only_for_placeholder() {
        let text = FeishuBlock::new("blk_000001", Payload::Text(TextPayload::default()));
        assert!(text.preserve_existing_reference().is_none());

        let payload = PlaceholderPayload {
            subtype: PlaceholderSubtype::Board,
            block_token: Some("bd1".into()),
            title: "画板".into(),
            summary: None,
            url: "feishu://board/bd1".into(),
            created_in_feishu_at: None,
            unknown_fields: Vec::new(),
        };
        let block = FeishuBlock::new("realFeishuId", Payload::Placeholder(payload));
        let reference = block.preserve_existing_reference().unwrap();
        assert_eq!(reference["block_id"], "realFeishuId");
        assert_eq!(reference["block_type"], "board");
        assert_eq!(reference["_done_md_directive"], "preserve_existing");
    }

    #[test]
    fn text_run_new_defaults_to_plain_style() {
        let run = TextRun::new("hello");
        assert_eq!(run.style, TextElementStyle::PLAIN);
        assert_eq!(run.style.bold, false);
        assert_eq!(run.style.link, None);
        assert_eq!(run.style.had_stripped_feishu_color, false);
    }
}
