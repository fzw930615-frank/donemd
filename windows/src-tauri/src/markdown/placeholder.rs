//! Port of `FeishuPlaceholder.swift` — the `<!-- feishu-placeholder … -->`
//! magic comment that persists a Feishu-native block (sheet / mindnote /
//! board / bitable / attachment / video / embed) with no Markdown equivalent.
//!
//! Hand-rolled `key: value` parser, deliberately NOT YAML (ADR-0007 § 解析器):
//! a real YAML parser would coerce `summary: yes` into a boolean, and the
//! parser must recover (return None) on corrupt input so the caller can fall
//! back to a raw markdown block without losing bytes.

/// First line of every placeholder magic comment — exact match.
pub const OPENER: &str = "<!-- feishu-placeholder";
/// Last line of every placeholder magic comment.
pub const CLOSER: &str = "-->";

const KNOWN_FIELDS: &[&str] = &[
    "type",
    "block_id",
    "block_token",
    "title",
    "summary",
    "url",
    "created_in_feishu_at",
];

/// Missing any of these → parse fails → raw-markdown fallback (#89 note:
/// `url` is deliberately NOT required — it's derivable for native blocks).
const REQUIRED_FIELDS: &[&str] = &["type", "block_id", "title"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeishuPlaceholder {
    pub block_type: String,
    pub block_id: String,
    pub block_token: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub url: String,
    pub created_in_feishu_at: Option<String>,
    /// Unrecognized `key: value` lines in original order.
    pub unknown_fields: Vec<(String, String)>,
}

/// Canonical internal-reference url for a feishu-native block, derivable from
/// `type` + `block_token`. `None` ⇒ the url is not derivable and must be
/// persisted explicitly (`embed`, unknown types, missing token).
///
/// The type→segment map mirrors the block encoder: attachment serializes as
/// `feishu://file/<token>`; every other native type uses its own name.
pub fn canonical_url(block_type: &str, block_token: Option<&str>) -> Option<String> {
    let token = block_token.filter(|t| !t.is_empty())?;
    let segment = match block_type {
        "board" | "sheet" | "bitable" | "mindnote" | "video" => block_type,
        "attachment" => "file",
        _ => return None,
    };
    Some(format!("feishu://{segment}/{token}"))
}

/// Parse the literal source of an HTML block. `None` when it isn't ours, is
/// malformed, or misses a required field — the caller then falls back to a
/// raw markdown block so the bytes survive.
pub fn parse(raw_html: &str) -> Option<FeishuPlaceholder> {
    let lines: Vec<&str> = raw_html.split('\n').map(str::trim).collect();
    // Trim empty leading / trailing lines (parser is the forgiving end).
    let mut start = 0;
    while start < lines.len() && lines[start].is_empty() {
        start += 1;
    }
    let mut end = lines.len();
    while end > start && lines[end - 1].is_empty() {
        end -= 1;
    }
    if end - start < 2 || lines[start] != OPENER || lines[end - 1] != CLOSER {
        return None;
    }

    let mut fields: Vec<(&str, &str)> = Vec::new();
    let mut unknown: Vec<(String, String)> = Vec::new();
    for line in &lines[start + 1..end - 1] {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':')?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return None;
        }
        if KNOWN_FIELDS.contains(&key) {
            fields.push((key, value));
        } else {
            unknown.push((key.to_string(), value.to_string()));
        }
    }

    let get = |k: &str| {
        fields
            .iter()
            .rev()
            .find(|(f, _)| *f == k)
            .map(|(_, v)| *v)
    };
    for required in REQUIRED_FIELDS {
        match get(required) {
            Some(v) if !v.is_empty() => {}
            _ => return None,
        }
    }

    let block_type = get("type").unwrap_or_default().to_string();
    let block_token = get("block_token").map(str::to_string);
    // Backfill an omitted url from the canonical form (#89) so in-memory
    // `url` always has a value.
    let url = get("url")
        .map(str::to_string)
        .or_else(|| canonical_url(&block_type, block_token.as_deref()))
        .unwrap_or_default();

    Some(FeishuPlaceholder {
        block_type,
        block_id: get("block_id").unwrap_or_default().to_string(),
        block_token,
        title: get("title").unwrap_or_default().to_string(),
        summary: get("summary").map(str::to_string),
        url,
        created_in_feishu_at: get("created_in_feishu_at").map(str::to_string),
        unknown_fields: unknown,
    })
}

/// Emit the magic-comment text form. Fixed field order (ADR-0007): known
/// fields in canonical order, then unknown fields in original order.
pub fn serialize(p: &FeishuPlaceholder) -> String {
    let mut lines: Vec<String> = vec![OPENER.to_string()];
    let mut push = |key: &str, value: &str| {
        debug_assert!(!value.contains('\n') && !value.contains("--"));
        lines.push(format!("{key}: {value}"));
    };
    push("type", &p.block_type);
    push("block_id", &p.block_id);
    if let Some(t) = &p.block_token {
        push("block_token", t);
    }
    push("title", &p.title);
    if let Some(s) = &p.summary {
        push("summary", s);
    }
    // #89: omit url when it equals the canonical derivable form.
    if Some(p.url.clone()) != canonical_url(&p.block_type, p.block_token.as_deref()) {
        push("url", &p.url);
    }
    if let Some(c) = &p.created_in_feishu_at {
        push("created_in_feishu_at", c);
    }
    for (k, v) in &p.unknown_fields {
        push(k, v);
    }
    lines.push(CLOSER.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::tiptap;
    // 文档级序列化器换名引入,避免遮蔽本模块的 `serialize(&FeishuPlaceholder)`。
    use crate::markdown::{parse_document, serialize as serialize_document};

    // MARK: - parse

    /// Swift `testParseFullPlaceholder` — 全部 7 字段。
    #[test]
    fn parse_full_placeholder() {
        let raw = "<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\nblock_token: shtcnYYY\ntitle: Q2 OKR 进度表\nsummary: 列:目标 / 责任人 / 进度 / 备注 · 共 12 行\nurl: https://example.feishu.cn/sheets/shtcnYYY\ncreated_in_feishu_at: 2026-04-15T09:00:00+08:00\n-->";
        let parsed = parse(raw).expect("合法占位注释必须解析");
        assert_eq!(parsed.block_type, "sheet");
        assert_eq!(parsed.block_id, "doxbcXXX_blk001");
        assert_eq!(parsed.block_token.as_deref(), Some("shtcnYYY"));
        assert_eq!(parsed.title, "Q2 OKR 进度表");
        assert_eq!(
            parsed.summary.as_deref(),
            Some("列:目标 / 责任人 / 进度 / 备注 · 共 12 行")
        );
        assert_eq!(parsed.url, "https://example.feishu.cn/sheets/shtcnYYY");
        assert_eq!(
            parsed.created_in_feishu_at.as_deref(),
            Some("2026-04-15T09:00:00+08:00")
        );
        assert!(parsed.unknown_fields.is_empty());
    }

    /// Swift `testParseMinimalRequiredFieldsOnly`。
    #[test]
    fn parse_minimal_required_fields_only() {
        let raw = "<!-- feishu-placeholder\ntype: embed\nblock_id: doxbcXXX_blk003\ntitle: 第三方系统嵌入\nurl: https://example.com/foo\n-->";
        let parsed = parse(raw).unwrap();
        assert_eq!(parsed.block_type, "embed");
        assert_eq!(parsed.block_id, "doxbcXXX_blk003");
        assert_eq!(parsed.title, "第三方系统嵌入");
        assert_eq!(parsed.url, "https://example.com/foo");
        assert_eq!(parsed.block_token, None);
        assert_eq!(parsed.summary, None);
        assert_eq!(parsed.created_in_feishu_at, None);
    }

    /// Swift `testParseUnknownFieldsPreservedInOrder`。
    #[test]
    fn parse_unknown_fields_preserved_in_order() {
        let raw = "<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR 进度表\nurl: https://example.feishu.cn/sheets/shtcnYYY\nfuture_quota: 42\nfuture_owner: alice\n-->";
        let parsed = parse(raw).unwrap();
        assert_eq!(
            parsed.unknown_fields,
            vec![
                ("future_quota".to_string(), "42".to_string()),
                ("future_owner".to_string(), "alice".to_string()),
            ]
        );
    }

    /// Swift `testParseRejectsWhenNotPlaceholder`。
    #[test]
    fn parse_rejects_when_not_placeholder() {
        assert_eq!(parse("<!-- not a feishu placeholder\ntype: sheet\n-->"), None);
    }

    /// Swift `testParseRejectsWhenOpenerLineHasTrailingTokens` — 单行变体
    /// 显式禁止(ADR-0007 §「首尾分别独占一行」)。
    #[test]
    fn parse_rejects_when_opener_line_has_trailing_tokens() {
        assert_eq!(
            parse("<!-- feishu-placeholder type: sheet block_id: x title: y url: u -->"),
            None
        );
    }

    /// Swift `testParseRejectsMissingRequiredField` — 缺 `block_id`:
    /// 块引用没了 → None → 调用方按原始字节回退。
    #[test]
    fn parse_rejects_missing_required_field() {
        let raw = "<!-- feishu-placeholder\ntype: sheet\ntitle: Q2 OKR\nurl: https://x.feishu.cn/foo\n-->";
        assert_eq!(parse(raw), None);
    }

    /// Swift `testParseBackfillsOmittedURLFromToken`(#89)— url 行省略
    /// 但 type + block_token 可派生,解析成功并回填。
    #[test]
    fn parse_backfills_omitted_url_from_token() {
        let raw = "<!-- feishu-placeholder\ntype: board\nblock_id: doxbcXXX_blk012\nblock_token: bdcnAAA\ntitle: 架构图\n-->";
        let parsed = parse(raw).unwrap();
        assert_eq!(parsed.block_type, "board");
        assert_eq!(parsed.block_token.as_deref(), Some("bdcnAAA"));
        assert_eq!(parsed.url, "feishu://board/bdcnAAA");
    }

    /// Swift `testParseBackfillsAttachmentURLToFileSegment` — attachment
    /// 映射到 `file` url 段(不是 `attachment`)。
    #[test]
    fn parse_backfills_attachment_url_to_file_segment() {
        let raw = "<!-- feishu-placeholder\ntype: attachment\nblock_id: doxbcXXX_blk020\nblock_token: fileTOK\ntitle: 季度报告.pdf\n-->";
        assert_eq!(parse(raw).unwrap().url, "feishu://file/fileTOK");
    }

    /// Swift `testParseOmittedURLWithNoTokenBackfillsEmpty` — 不可派生
    /// (无 block_token)且无 url 行 → url 为空串,仍是合法占位块。
    #[test]
    fn parse_omitted_url_with_no_token_backfills_empty() {
        let raw = "<!-- feishu-placeholder\ntype: embed\nblock_id: doxbcXXX_blk003\ntitle: 第三方系统嵌入\n-->";
        let parsed = parse(raw).expect("embed 无 token 也合法");
        assert_eq!(parsed.url, "");
    }

    /// Swift `testParseRejectsEmptyRequiredField` — `type` 在场但为空。
    #[test]
    fn parse_rejects_empty_required_field() {
        let raw = "<!-- feishu-placeholder\ntype:\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR\nurl: https://x.feishu.cn/foo\n-->";
        assert_eq!(parse(raw), None);
    }

    /// Swift `testParseRejectsLineWithoutColon`。
    #[test]
    fn parse_rejects_line_without_colon() {
        let raw = "<!-- feishu-placeholder\ntype: sheet\nblock_id doxbcXXX_blk001\ntitle: Q2 OKR\nurl: https://x.feishu.cn/foo\n-->";
        assert_eq!(parse(raw), None);
    }

    /// Swift `testParseRejectsMissingCloser`。
    #[test]
    fn parse_rejects_missing_closer() {
        let raw = "<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR\nurl: https://x.feishu.cn/foo\n";
        assert_eq!(parse(raw), None);
    }

    // MARK: - serialize

    /// Swift `testSerializeFullPlaceholder` — 规范字段序。
    #[test]
    fn serialize_full_placeholder() {
        let p = FeishuPlaceholder {
            block_type: "sheet".into(),
            block_id: "doxbcXXX_blk001".into(),
            block_token: Some("shtcnYYY".into()),
            title: "Q2 OKR 进度表".into(),
            summary: Some("列:目标 / 责任人 / 进度 / 备注 · 共 12 行".into()),
            url: "https://example.feishu.cn/sheets/shtcnYYY".into(),
            created_in_feishu_at: Some("2026-04-15T09:00:00+08:00".into()),
            unknown_fields: vec![],
        };
        let expected = "<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\nblock_token: shtcnYYY\ntitle: Q2 OKR 进度表\nsummary: 列:目标 / 责任人 / 进度 / 备注 · 共 12 行\nurl: https://example.feishu.cn/sheets/shtcnYYY\ncreated_in_feishu_at: 2026-04-15T09:00:00+08:00\n-->";
        assert_eq!(serialize(&p), expected);
    }

    /// Swift `testSerializeOmitsOptionalFields`。
    #[test]
    fn serialize_omits_optional_fields() {
        let p = FeishuPlaceholder {
            block_type: "embed".into(),
            block_id: "doxbcXXX_blk003".into(),
            title: "第三方系统嵌入".into(),
            url: "https://example.com/foo".into(),
            ..Default::default()
        };
        let expected = "<!-- feishu-placeholder\ntype: embed\nblock_id: doxbcXXX_blk003\ntitle: 第三方系统嵌入\nurl: https://example.com/foo\n-->";
        assert_eq!(serialize(&p), expected);
    }

    /// Swift `testSerializeUnknownFieldsAfterKnownInOrder` — 已知字段在前,
    /// 未知字段按原序在后。
    #[test]
    fn serialize_unknown_fields_after_known_in_order() {
        let p = FeishuPlaceholder {
            block_type: "sheet".into(),
            block_id: "doxbcXXX_blk001".into(),
            title: "Q2 OKR".into(),
            url: "https://x.feishu.cn/foo".into(),
            unknown_fields: vec![
                ("future_quota".into(), "42".into()),
                ("future_owner".into(), "alice".into()),
            ],
            ..Default::default()
        };
        let expected = "<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR\nurl: https://x.feishu.cn/foo\nfuture_quota: 42\nfuture_owner: alice\n-->";
        assert_eq!(serialize(&p), expected);
    }

    /// Swift `testSerializeOmitsCanonicalURL`(#89)— url == 派生形时
    /// url 行整行省略。
    #[test]
    fn serialize_omits_canonical_url() {
        let p = FeishuPlaceholder {
            block_type: "video".into(),
            block_id: "doxbcXXX_blk001".into(),
            block_token: Some("vidTOK".into()),
            title: "demo.mp4".into(),
            url: "feishu://video/vidTOK".into(),
            ..Default::default()
        };
        let expected = "<!-- feishu-placeholder\ntype: video\nblock_id: doxbcXXX_blk001\nblock_token: vidTOK\ntitle: demo.mp4\n-->";
        assert_eq!(serialize(&p), expected);
    }

    /// Swift `testSerializeKeepsEmbedExternalURL` — embed 带不可派生的
    /// 外链(无 block_token)→ url 行永远写。
    #[test]
    fn serialize_keeps_embed_external_url() {
        let p = FeishuPlaceholder {
            block_type: "embed".into(),
            block_id: "doxbcXXX_blk003".into(),
            title: "第三方系统嵌入".into(),
            url: "https://grafana.example.com/d/abc".into(),
            ..Default::default()
        };
        assert!(serialize(&p).contains("url: https://grafana.example.com/d/abc"));
    }

    /// Swift `testSerializeKeepsNonCanonicalURL` — 原生类型 + token 在场,
    /// 但 url 是真实 http 链接(旧版拉取产物)→ 保留,不省略。
    #[test]
    fn serialize_keeps_non_canonical_url() {
        let p = FeishuPlaceholder {
            block_type: "sheet".into(),
            block_id: "doxbcXXX_blk001".into(),
            block_token: Some("shtcnYYY".into()),
            title: "Q2 OKR".into(),
            url: "https://example.feishu.cn/sheets/shtcnYYY".into(),
            ..Default::default()
        };
        assert!(serialize(&p).contains("url: https://example.feishu.cn/sheets/shtcnYYY"));
    }

    // MARK: - canonical_url

    /// Swift `testCanonicalURLMapping` — 类型 → url 段映射;不可派生
    /// 的组合返回 None。
    #[test]
    fn canonical_url_mapping() {
        assert_eq!(canonical_url("board", Some("t")), Some("feishu://board/t".into()));
        assert_eq!(canonical_url("sheet", Some("t")), Some("feishu://sheet/t".into()));
        assert_eq!(canonical_url("bitable", Some("t")), Some("feishu://bitable/t".into()));
        assert_eq!(canonical_url("mindnote", Some("t")), Some("feishu://mindnote/t".into()));
        assert_eq!(canonical_url("video", Some("t")), Some("feishu://video/t".into()));
        assert_eq!(canonical_url("attachment", Some("t")), Some("feishu://file/t".into()));
        // 不可派生:embed、未知类型、token 缺失或为空。
        assert_eq!(canonical_url("embed", Some("t")), None);
        assert_eq!(canonical_url("board", None), None);
        assert_eq!(canonical_url("board", Some("")), None);
    }

    // MARK: - round-trip

    /// Swift `testRoundTripCanonicalOmittedURLIsByteStable`(#89)— 无
    /// url 行的输入按短形逐字节往返,派生 url 在内存里可用。
    #[test]
    fn round_trip_canonical_omitted_url_is_byte_stable() {
        let raw = "<!-- feishu-placeholder\ntype: bitable\nblock_id: doxbcXXX_blk008\nblock_token: bascnZZZ\ntitle: 任务跟踪表\n-->";
        let parsed = parse(raw).expect("合法输入必须解析");
        assert_eq!(parsed.url, "feishu://bitable/bascnZZZ");
        assert_eq!(serialize(&parsed), raw);
    }

    /// Swift `testRoundTripEmbedExternalURLIsByteStable`。
    #[test]
    fn round_trip_embed_external_url_is_byte_stable() {
        let raw = "<!-- feishu-placeholder\ntype: embed\nblock_id: doxbcXXX_blk003\ntitle: 第三方系统嵌入\nurl: https://grafana.example.com/d/abc\n-->";
        let parsed = parse(raw).unwrap();
        assert_eq!(serialize(&parsed), raw);
    }

    /// Swift `testRoundTripFullPlaceholderIsByteStable`。
    #[test]
    fn round_trip_full_placeholder_is_byte_stable() {
        let raw = "<!-- feishu-placeholder\ntype: bitable\nblock_id: doxbcXXX_blk008\nblock_token: bascnZZZ\ntitle: 任务跟踪表\nsummary: 字段:负责人 / 状态 / 截止日 · 共 28 行\nurl: https://example.feishu.cn/base/bascnZZZ\ncreated_in_feishu_at: 2026-03-01T14:30:00+08:00\n-->";
        let parsed = parse(raw).unwrap();
        assert_eq!(serialize(&parsed), raw);
    }

    /// Swift `testRoundTripWithUnknownFieldsIsByteStable`。
    #[test]
    fn round_trip_with_unknown_fields_is_byte_stable() {
        let raw = "<!-- feishu-placeholder\ntype: board\nblock_id: doxbcXXX_blk012\ntitle: 架构图\nurl: https://example.feishu.cn/board/bdcnAAA\nfuture_collab_count: 7\n-->";
        let parsed = parse(raw).unwrap();
        assert_eq!(serialize(&parsed), raw);
    }

    // MARK: - 经 MarkdownEngine 的端到端

    /// Swift `testMarkdownParseProducesPlaceholderNode` — 占位注释落在
    /// `feishu_placeholder_block` 节点,不是 raw markdown block。
    #[test]
    fn markdown_parse_produces_placeholder_node() {
        let source = "前言段落。\n\n<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR 进度表\nurl: https://example.feishu.cn/sheets/shtcnYYY\n-->\n\n后续段落。";
        let doc = parse_document(source);
        let blocks = tiptap::content(&doc.body);
        assert_eq!(blocks.len(), 3);
        assert_eq!(tiptap::node_type(&blocks[0]), "paragraph");
        assert_eq!(tiptap::node_type(&blocks[1]), "feishu_placeholder_block");
        assert_eq!(tiptap::node_type(&blocks[2]), "paragraph");

        let node = &blocks[1];
        assert_eq!(tiptap::attr_str(node, "type"), Some("sheet"));
        assert_eq!(tiptap::attr_str(node, "block_id"), Some("doxbcXXX_blk001"));
        assert_eq!(tiptap::attr_str(node, "title"), Some("Q2 OKR 进度表"));
        assert_eq!(
            tiptap::attr_str(node, "url"),
            Some("https://example.feishu.cn/sheets/shtcnYYY")
        );
        assert_eq!(tiptap::attr_str(node, "block_token"), None);
        assert_eq!(tiptap::attr_str(node, "summary"), None);
    }

    /// Swift `testCorruptPlaceholderFallsBackToRawBlock` — 缺必填
    /// `block_id`:解析返回 None,字节经 raw_markdown_block 路径往返,
    /// 不丢失。(#89 后 url 不再必填;block_id 仍是。)
    #[test]
    fn corrupt_placeholder_falls_back_to_raw_block() {
        let source = "<!-- feishu-placeholder\ntype: sheet\ntitle: 没有 block_id 的占位块\nurl: https://x.feishu.cn/foo\n-->";
        let doc = parse_document(source);
        let blocks = tiptap::content(&doc.body);
        assert_eq!(blocks.len(), 1);
        assert_eq!(tiptap::node_type(&blocks[0]), "raw_markdown_block");
    }

    /// Swift `testMarkdownParseBackfillsURLAttrWhenOmitted`(#89)— 无
    /// url 行的占位块产出的节点 attrs.url 已回填为规范形,web 端
    /// NodeView 的打开按钮继续可用。
    #[test]
    fn markdown_parse_backfills_url_attr_when_omitted() {
        let source = "<!-- feishu-placeholder\ntype: video\nblock_id: doxbcXXX_blk001\nblock_token: vidTOK\ntitle: demo.mp4\n-->";
        let doc = parse_document(source);
        let blocks = tiptap::content(&doc.body);
        assert_eq!(blocks.len(), 1);
        assert_eq!(tiptap::node_type(&blocks[0]), "feishu_placeholder_block");
        assert_eq!(
            tiptap::attr_str(&blocks[0], "url"),
            Some("feishu://video/vidTOK")
        );
    }

    /// Swift `testNonPlaceholderHTMLCommentStillRoutesToRawBlock` —
    /// 普通HTML注释(不含魔法词)继续走 raw_markdown_block 路径。
    #[test]
    fn non_placeholder_html_comment_still_routes_to_raw_block() {
        let doc = parse_document("<!-- TODO revisit this section -->");
        let blocks = tiptap::content(&doc.body);
        assert_eq!(blocks.len(), 1);
        assert_eq!(tiptap::node_type(&blocks[0]), "raw_markdown_block");
    }

    /// Swift `testFullDocumentRoundTripPreservesPlaceholder` — 全文档
    /// 往返:序列化器补尾换行(规范形)。
    #[test]
    fn full_document_round_trip_preserves_placeholder() {
        let source = "# 标题\n\n<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR 进度表\nurl: https://example.feishu.cn/sheets/shtcnYYY\n-->\n\n正文段落。";
        let doc = parse_document(source);
        let expected = "# 标题\n\n<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR 进度表\nurl: https://example.feishu.cn/sheets/shtcnYYY\n-->\n\n正文段落。\n";
        assert_eq!(serialize_document(&doc), expected);
    }

    /// Swift `testUnknownFieldsRoundTripThroughTiptapAttrs` — 未知字段经
    /// Tiptap 属性往返;顺序:已知字段在前,未知在后。
    #[test]
    fn unknown_fields_round_trip_through_tiptap_attrs() {
        let source = "<!-- feishu-placeholder\ntype: sheet\nblock_id: doxbcXXX_blk001\ntitle: Q2 OKR\nurl: https://x.feishu.cn/foo\nfuture_quota: 42\n-->";
        let doc = parse_document(source);
        let serialized = serialize_document(&doc);
        assert!(serialized.contains("future_quota: 42"), "{serialized}");
        let type_idx = serialized.find("type: sheet").expect("type 行");
        let url_idx = serialized.find("url: https://x.feishu.cn/foo").expect("url 行");
        let unknown_idx = serialized.find("future_quota: 42").expect("未知字段行");
        assert!(type_idx < url_idx, "{serialized}");
        assert!(url_idx < unknown_idx, "{serialized}");
    }
}
