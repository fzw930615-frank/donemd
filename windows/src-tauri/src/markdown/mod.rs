//! Markdown ⇄ Tiptap engine — the Rust port of `MarkdownEngine.swift`.
//!
//! `parse_document` splits off YAML frontmatter, parses the body with
//! pulldown-cmark (GFM tables / strikethrough / tasklists), and converts the
//! tree to Tiptap JSON. `serialize_document` writes canonical Markdown back
//! with the frontmatter spliced in front. On-disk format is byte-compatible
//! with the macOS app.

pub mod ast;
pub mod convert;
pub mod frontmatter;
pub mod placeholder;
pub mod serializer;
pub mod tiptap;

use serde_json::Value;

/// A parsed `.md` file: frontmatter + Tiptap body.
pub struct ParsedDocument {
    pub frontmatter: frontmatter::Frontmatter,
    pub body: Value,
}

pub fn parse_document(source: &str) -> ParsedDocument {
    let (fm, body) = frontmatter::parse(source);
    let blocks = ast::parse_blocks(body);
    ParsedDocument {
        frontmatter: fm,
        body: convert::convert_document(&blocks, body),
    }
}

pub fn serialize(doc: &ParsedDocument) -> String {
    let body = serializer::serialize_document(&doc.body);
    frontmatter::serialize(&doc.frontmatter, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(md: &str) -> String {
        let doc = parse_document(md);
        serialize(&doc)
    }

    #[test]
    fn empty_doc_is_single_paragraph() {
        let doc = parse_document("");
        assert_eq!(
            doc.body,
            json!({ "type": "doc", "content": [{ "type": "paragraph" }] })
        );
    }

    #[test]
    fn heading_and_paragraph_roundtrip() {
        let md = "# 标题\n\n你好，**世界**。\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn task_list_roundtrip() {
        let md = "- [ ] 待办\n- [x] 完成\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn table_roundtrip() {
        let md = "| 名 | 值 |\n| --- | --- |\n| a | 1 |\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn block_math_roundtrip() {
        let md = "$$\n\\begin{aligned} x &= 1 \\\\ y &= 2 \\end{aligned}\n$$\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn inline_math_strict_rules() {
        // `$5 and $10` is prose, not math.
        let md = "总价 $5 and $10 不变。\n";
        assert_eq!(roundtrip(md), md);
        // Real inline math converts and round-trips.
        let md = "公式 $x^2$ 内联。\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn callout_roundtrip() {
        let md = "> [!NOTE]\n> 注意内容。\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn code_block_roundtrip() {
        let md = "```rust\nfn main() {}\n```\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn frontmatter_preserved() {
        let md = "---\ntitle: 测试\n---\n# 正文\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn feishu_placeholder_roundtrip() {
        let md = "<!-- feishu-placeholder\ntype: board\nblock_id: b1\nblock_token: t1\ntitle: 画板\n-->\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn image_asset_src_rewrite() {
        let doc = parse_document("![alt](./assets/abc.png)\n");
        // Runtime src is the platform asset URL…
        let img = &doc.body["content"][0];
        let src = img["attrs"]["src"].as_str().unwrap();
        assert!(src == "donemd-asset://abc.png" || src == "http://donemd-asset.localhost/abc.png");
        // …and serializes back to the disk form.
        assert_eq!(serialize(&doc), "![alt](./assets/abc.png)\n");
    }

    #[test]
    fn ordered_list_start_preserved() {
        let md = "3. 三\n4. 四\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn video_block_roundtrip() {
        let md = "<video controls src=\"./assets/v.mp4\"></video>\n";
        assert_eq!(roundtrip(md), md);
    }

    // MARK: - fixed-point fixtures (ported from MarkdownEngineRoundTripTests)

    #[test]
    fn nested_bullet_list_roundtrip() {
        // Inner list separated from the outer paragraph by a blank line.
        let md = "- outer\n\n  - inner\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn callout_multi_paragraph_roundtrip() {
        let md = "> [!NOTE]\n> first paragraph\n>\n> second paragraph\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn html_details_block_roundtrip() {
        let md = "<details>\n<summary>Hello</summary>\nworld\n</details>\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn hard_break_roundtrip() {
        let md = "first\\\nsecond\n";
        assert_eq!(roundtrip(md), md);
    }

    #[test]
    fn block_math_multiline_roundtrip() {
        let md = "$$\n\\begin{aligned}\na&=b\n\\end{aligned}\n$$\n";
        assert_eq!(roundtrip(md), md);
    }

    // MARK: - normalization (ported from MarkdownNormalizationTests):
    // non-canonical input must serialize to the canonical form.

    #[test]
    fn setext_headings_normalize_to_atx() {
        assert_eq!(roundtrip("Hello\n=====\n"), "# Hello\n");
        assert_eq!(roundtrip("Hello\n-----\n"), "## Hello\n");
    }

    #[test]
    fn star_and_plus_bullets_normalize_to_dash() {
        assert_eq!(roundtrip("* one\n* two\n"), "- one\n- two\n");
        assert_eq!(roundtrip("+ one\n+ two\n"), "- one\n- two\n");
    }

    #[test]
    fn multiple_blank_lines_collapse_to_one() {
        assert_eq!(roundtrip("first\n\n\n\n\nsecond\n"), "first\n\nsecond\n");
    }

    #[test]
    fn trailing_newline_is_always_exactly_one() {
        assert_eq!(roundtrip("hello"), "hello\n");
    }

    #[test]
    fn underscore_emphasis_normalizes_to_star() {
        assert_eq!(roundtrip("_italic_\n"), "*italic*\n");
        assert_eq!(roundtrip("__bold__\n"), "**bold**\n");
    }

    // MARK: - frontmatter edge cases (ported from FrontmatterEngineTests)

    #[test]
    fn frontmatter_rejects_toml_fence() {
        let source = "+++\ntitle = \"x\"\n+++\n\nbody\n";
        let (fm, body) = frontmatter::parse(source);
        assert!(!fm.has_fence);
        assert_eq!(body, source);
    }

    #[test]
    fn frontmatter_unclosed_fence_falls_back_to_body() {
        let source = "---\ntitle: x\nbody never closes\n";
        let (fm, body) = frontmatter::parse(source);
        assert!(!fm.has_fence);
        assert_eq!(body, source);
    }

    #[test]
    fn frontmatter_corrupt_yaml_falls_back_to_body() {
        let source = "---\nkey: : nope\n---\n\nbody\n";
        let (fm, body) = frontmatter::parse(source);
        assert!(!fm.has_fence);
        assert_eq!(body, source);
    }

    #[test]
    fn frontmatter_fence_not_at_file_start_is_body() {
        let source = "intro\n---\ntitle: x\n---\n";
        let (fm, body) = frontmatter::parse(source);
        assert!(!fm.has_fence);
        assert_eq!(body, source);
    }

    #[test]
    fn frontmatter_empty_fence_block() {
        let source = "---\n---\n\nbody\n";
        let (fm, body) = frontmatter::parse(source);
        assert!(fm.has_fence);
        assert!(fm.user_fields.is_empty());
        assert!(fm.feishu.is_none());
        assert_eq!(body, "\nbody\n");
        // Engine-level (raw body) round-trip is byte-exact, same promise as
        // the Swift FrontmatterEngine…
        assert_eq!(frontmatter::serialize(&fm, body), source);
        // …while the full-document path normalizes the body's leading blank
        // line away (parity with macOS: only the frontmatter itself is
        // guaranteed to reparse equal there).
        let doc = parse_document(source);
        let reparsed = parse_document(&serialize(&doc));
        assert_eq!(reparsed.frontmatter, doc.frontmatter);
    }

    #[test]
    fn frontmatter_user_fields_preserved_verbatim() {
        let source = "---\ntitle: 我的文档\nauthor: shampoo\n---\n\n# 正文\n";
        let (fm, body) = frontmatter::parse(source);
        assert!(fm.has_fence);
        assert_eq!(fm.user_fields.len(), 2);
        assert_eq!(fm.user_fields[0].0, "title");
        assert_eq!(fm.user_fields[1].0, "author");
        assert_eq!(frontmatter::serialize(&fm, body), source);
        let doc = parse_document(source);
        let reparsed = parse_document(&serialize(&doc));
        assert_eq!(reparsed.frontmatter, doc.frontmatter);
    }

    // MARK: - feishu 子树(FrontmatterEngineTests 的 feishu 组)

    /// Swift `testParseRecognizedFeishuFields`。
    #[test]
    fn frontmatter_parse_recognized_feishu_fields() {
        let source = "---\nfeishu:\n  doc_token: doxcnAbc123XYZ\n  doc_url: https://example.feishu.cn/docx/doxcnAbc123XYZ\n  last_pulled_revision: 142\n  last_pushed_at: '2026-05-23T10:23:45+08:00'\n---\n\nbody\n";
        let (fm, body) = frontmatter::parse(source);
        let f = fm.feishu.as_ref().expect("feishu 子树应解析出来");
        assert_eq!(f.doc_token.as_deref(), Some("doxcnAbc123XYZ"));
        assert_eq!(
            f.doc_url.as_deref(),
            Some("https://example.feishu.cn/docx/doxcnAbc123XYZ")
        );
        assert_eq!(f.last_pulled_revision, Some(142));
        assert_eq!(
            f.last_pushed_at.as_deref(),
            Some("2026-05-23T10:23:45+08:00")
        );
        assert!(f.placeholder_blocks.is_empty());
        assert!(f.unknown_fields.is_empty());
        assert_eq!(fm.feishu_original_index, Some(0));
        assert_eq!(body, "\nbody\n");
    }

    /// Swift `testParsePlaceholderBlocksStableOrder` — 顺序稳定 + 字段
    /// 完整(title 缺省为 None)。
    #[test]
    fn frontmatter_parse_placeholder_blocks_stable_order() {
        let source = "---\nfeishu:\n  doc_token: doxcnAbc123XYZ\n  placeholder_blocks:\n    - block_id: doxbcXXX_blk001\n      type: sheet\n      title: Q2 OKR\n    - block_id: doxbcXXX_blk007\n      type: board\n      title: 架构图\n    - block_id: doxbcXXX_blk012\n      type: bitable\n---\nbody\n";
        let (fm, _) = frontmatter::parse(source);
        let f = fm.feishu.as_ref().unwrap();
        assert_eq!(f.placeholder_blocks.len(), 3);
        assert_eq!(f.placeholder_blocks[0].block_id, "doxbcXXX_blk001");
        assert_eq!(f.placeholder_blocks[0].block_type, "sheet");
        assert_eq!(
            f.placeholder_blocks[0].title.as_deref(),
            Some("Q2 OKR")
        );
        assert_eq!(f.placeholder_blocks[1].block_id, "doxbcXXX_blk007");
        assert_eq!(f.placeholder_blocks[2].block_id, "doxbcXXX_blk012");
        assert_eq!(f.placeholder_blocks[2].title, None);
    }

    /// Swift `testParseUnrecognizedFeishuKeys` — 未知子键按出现顺序
    /// 捕获(Rust 存原始 YAML 文本,column-0 形)。
    #[test]
    fn frontmatter_parse_unrecognized_feishu_keys() {
        let source = "---\nfeishu:\n  doc_token: doxcnAbc\n  foo: bar\n  experimental_setting: true\n---\n";
        let (fm, _) = frontmatter::parse(source);
        let f = fm.feishu.as_ref().unwrap();
        assert_eq!(f.unknown_fields.len(), 2);
        assert_eq!(f.unknown_fields[0], "foo: bar\n");
        assert_eq!(f.unknown_fields[1], "experimental_setting: true\n");
    }

    /// Swift `testSerializeIdempotentFullFeishuBlock` — 规范形(键序
    /// 与发射器一致)往返逐字节相等。
    #[test]
    fn frontmatter_serialize_idempotent_full_feishu_block() {
        let source = "---\ntitle: Q2 路线图\nfeishu:\n  doc_token: doxcnAbc123XYZ\n  doc_url: https://example.feishu.cn/docx/doxcnAbc123XYZ\n  last_pulled_revision: 142\n  placeholder_blocks:\n    - block_id: doxbcXXX_blk001\n      type: sheet\n      title: Q2 OKR\n    - block_id: doxbcXXX_blk007\n      type: board\n      title: 架构图\n---\n\n# 正文\n";
        let (fm, body) = frontmatter::parse(source);
        assert_eq!(frontmatter::serialize(&fm, body), source);
        // 全文档路径同样幂等(frontmatter 相等)。
        let doc = parse_document(source);
        let reparsed = parse_document(&serialize(&doc));
        assert_eq!(reparsed.frontmatter, doc.frontmatter);
    }

    /// Swift `testSerializeUnknownFeishuKeyRoundTrips` — 未知子键经
    /// 序列化后存活,二次解析幂等(不要求首遍逐字节)。
    #[test]
    fn frontmatter_serialize_unknown_feishu_key_round_trips() {
        let source = "---\nfeishu:\n  doc_token: doxcnAbc\n  foo: bar\n---\nbody\n";
        let (fm, body) = frontmatter::parse(source);
        let serialized = frontmatter::serialize(&fm, body);
        let (reparsed, rebody) = frontmatter::parse(&serialized);
        assert_eq!(reparsed, fm);
        assert_eq!(rebody, body);
        assert!(serialized.contains("foo: bar"), "{serialized}");
    }

    // MARK: - merge(FrontmatterEngineTests 的 merge 组 + Rust 补充)

    /// Swift `testMergeAppendsFeishuWhenAbsent` — 现有 frontmatter 无
    /// feishu 子树:merge 追加在用户字段之后(index = 字段数)。
    #[test]
    fn frontmatter_merge_appends_feishu_when_absent() {
        let source = "---\ntitle: 我的文档\n---\n\nbody\n";
        let (existing, _) = frontmatter::parse(source);
        let incoming = frontmatter::Frontmatter {
            user_fields: vec![],
            feishu: Some(frontmatter::FeishuFrontmatter {
                doc_token: Some("doxcnNEW".into()),
                ..Default::default()
            }),
            feishu_original_index: Some(0),
            has_fence: true,
        };
        let merged = frontmatter::merge(&existing, incoming);
        assert_eq!(merged.user_fields.len(), 1);
        assert_eq!(merged.user_fields[0].0, "title");
        assert_eq!(
            merged.feishu.as_ref().and_then(|f| f.doc_token.clone()),
            Some("doxcnNEW".to_string())
        );
        // 追加在用户字段之后。
        assert_eq!(merged.feishu_original_index, Some(1));
    }

    /// Swift `testMergeReplacesFeishuPreservingPosition` — feishu 子树
    /// 整体替换,位置保持在原 index(用户字段之间)。
    #[test]
    fn frontmatter_merge_replaces_feishu_preserving_position() {
        let source = "---\ntitle: 我的\nfeishu:\n  doc_token: doxcnOLD\n  last_pulled_revision: 1\nauthor: shampoo\n---\n\nbody\n";
        let (existing, _) = frontmatter::parse(source);
        let incoming = frontmatter::Frontmatter {
            user_fields: vec![],
            feishu: Some(frontmatter::FeishuFrontmatter {
                doc_token: Some("doxcnOLD".into()),
                last_pulled_revision: Some(99),
                ..Default::default()
            }),
            feishu_original_index: None,
            has_fence: true,
        };
        let merged = frontmatter::merge(&existing, incoming);
        // 整体替换:revision 更新为 99。
        assert_eq!(
            merged.feishu.as_ref().and_then(|f| f.last_pulled_revision),
            Some(99)
        );
        // 位置保持在 title 与 author 之间。
        assert_eq!(merged.feishu_original_index, Some(1));
        assert_eq!(
            merged.user_fields.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["title", "author"]
        );
    }

    /// Swift `testMergeNoIncomingFeishuKeepsExisting` — incoming 无
    /// feishu 子树时现有子树原样保留(doc_url / unknown 都不覆写)。
    #[test]
    fn frontmatter_merge_no_incoming_feishu_keeps_existing() {
        let source = "---\nfeishu:\n  doc_token: doxcnSTAY\n  doc_url: https://example.feishu.cn/docx/doxcnSTAY\n  custom_flag: yes\n---\n";
        let (existing, _) = frontmatter::parse(source);
        let merged = frontmatter::merge(&existing, frontmatter::Frontmatter::default());
        let f = merged.feishu.as_ref().expect("现有 feishu 子树必须保留");
        assert_eq!(f.doc_token.as_deref(), Some("doxcnSTAY"));
        assert_eq!(
            f.doc_url.as_deref(),
            Some("https://example.feishu.cn/docx/doxcnSTAY")
        );
        assert_eq!(f.unknown_fields, vec!["custom_flag: yes\n".to_string()]);
    }

    /// Rust 补充:整体替换不是字段级 patch — incoming 子树里没有的
    /// 字段一律清空。拉取协调器要保留 last_pushed_at 等字段时,必须
    /// 在构造 incoming 时自己带上(见 F3 pull)。
    #[test]
    fn frontmatter_merge_wholesale_replacement_drops_absent_fields() {
        let source = "---\nfeishu:\n  doc_token: doxcnOLD\n  doc_url: https://example.feishu.cn/docx/doxcnOLD\n  last_pushed_at: '2026-05-23T10:23:45+08:00'\n---\n";
        let (existing, _) = frontmatter::parse(source);
        let incoming = frontmatter::Frontmatter {
            user_fields: vec![],
            feishu: Some(frontmatter::FeishuFrontmatter {
                doc_token: Some("doxcnNEW".into()),
                last_pulled_revision: Some(7),
                ..Default::default()
            }),
            feishu_original_index: None,
            has_fence: true,
        };
        let merged = frontmatter::merge(&existing, incoming);
        let f = merged.feishu.as_ref().unwrap();
        assert_eq!(f.doc_token.as_deref(), Some("doxcnNEW"));
        assert_eq!(f.last_pulled_revision, Some(7));
        assert_eq!(f.doc_url, None, "incoming 未带的字段清空(整体替换)");
        assert_eq!(f.last_pushed_at, None);
    }

    /// Rust 补充:merge 给无 fence 的文档装上 fence — 序列化时输出
    /// `---` 包裹,首次绑定(plain md → 飞书文档)依赖这条。
    #[test]
    fn frontmatter_merge_sets_has_fence() {
        let (existing, body) = frontmatter::parse("# 正文\n");
        assert!(!existing.has_fence);
        let incoming = frontmatter::Frontmatter {
            user_fields: vec![],
            feishu: Some(frontmatter::FeishuFrontmatter {
                doc_token: Some("doxcnFRESH".into()),
                ..Default::default()
            }),
            feishu_original_index: None,
            has_fence: true,
        };
        let merged = frontmatter::merge(&existing, incoming);
        assert!(merged.has_fence);
        let serialized = frontmatter::serialize(&merged, body);
        assert_eq!(
            serialized, "---\nfeishu:\n  doc_token: doxcnFRESH\n---\n# 正文\n",
            "merge 后必须产出带 fence 的完整 frontmatter"
        );
    }

    /// Rust 补充:merge 不动用户字段的原始文本块(引号 / 列表 / 注
    /// 释逐字存活),feishu 子树插回原位置。
    #[test]
    fn frontmatter_merge_preserves_user_fields_verbatim_and_serializes_at_index() {
        let source = "---\ntitle: '带 单引号 的 标题'\ntags: [roadmap, q2]\nfeishu:\n  doc_token: doxcnOLD\nauthors:\n  - 蛋蛋\n  - 蝌蚪\n---\n\nbody\n";
        let (existing, body) = frontmatter::parse(source);
        let incoming = frontmatter::Frontmatter {
            user_fields: vec![],
            feishu: Some(frontmatter::FeishuFrontmatter {
                doc_token: Some("doxcnNEW".into()),
                last_pulled_revision: Some(42),
                ..Default::default()
            }),
            feishu_original_index: None,
            has_fence: true,
        };
        let merged = frontmatter::merge(&existing, incoming);
        let serialized = frontmatter::serialize(&merged, body);
        // 用户字段逐字;新 feishu 子树插在原 index(第 3 位)。
        assert_eq!(
            serialized,
            "---\ntitle: '带 单引号 的 标题'\ntags: [roadmap, q2]\nfeishu:\n  doc_token: doxcnNEW\n  last_pulled_revision: 42\nauthors:\n  - 蛋蛋\n  - 蝌蚪\n---\n\nbody\n",
            "merge 后用户字段逐字 + feishu 子树在原位"
        );
        // 二次解析幂等。
        let (reparsed, rebody) = frontmatter::parse(&serialized);
        assert_eq!(reparsed, merged);
        assert_eq!(rebody, body);
    }
}
