//! Port of `ASTConverter.swift` — block/inline tree → Tiptap document JSON.
//!
//! Behavior contract carried over from the Swift original:
//!   - marks accumulate down inline ancestors onto leaf text nodes
//!   - soft break ⇒ single space; adjacent same-mark text nodes coalesce
//!     (first parse is already canonical — the parse-stable invariant #56)
//!   - a paragraph whose only real content is one image hoists the image to
//!     block level (ProseMirror undo would otherwise abort)
//!   - `$$…$$` paragraphs become `math_block` with the latex recovered from
//!     the verbatim source slice; `$…$` inside text becomes `math_inline`
//!     under strict opener/closer rules
//!   - lone `<video …>` (block HTML or a paragraph of inline HTML) becomes a
//!     `video` node; other HTML becomes a verbatim `raw_markdown_block`
//!   - `<!-- feishu-placeholder … -->` comments become typed placeholder nodes
//!   - blockquotes leading with `[!TYPE]` become `callout` nodes
//!   - any list containing a checkbox item becomes a taskList; checkbox-less
//!     items coerce to unchecked
//!   - `./assets/<file>` (and bare `assets/<file>`) image/video srcs rewrite
//!     to the runtime asset URL; everything else passes through

use serde_json::{Map, Value};

use super::ast::{Block, Inline, ListItem};
use super::placeholder;
use super::tiptap;
use crate::assets;

pub fn convert_document(blocks: &[Block], source: &str) -> Value {
    let converted: Vec<Value> = blocks.iter().filter_map(|b| convert_block(b, source)).collect();
    // Tiptap's doc schema requires `block+`; empty input ⇒ single empty paragraph.
    if converted.is_empty() {
        tiptap::node_with_content("doc", vec![tiptap::node("paragraph")])
    } else {
        tiptap::node_with_content("doc", converted)
    }
}

fn convert_block(block: &Block, source: &str) -> Option<Value> {
    match block {
        Block::Heading { level, children } => {
            let mut n = tiptap::node_with_content("heading", convert_inline(children));
            let mut attrs = Map::new();
            attrs.insert("level".into(), Value::from(*level as i64));
            n["attrs"] = Value::Object(attrs);
            Some(n)
        }
        Block::Paragraph { children, range } => {
            if let Some(math) = convert_math_block_if_matched(children, range, source) {
                return Some(math);
            }
            if let Some(video) = convert_video_if_matched(children) {
                return Some(video);
            }
            let inline = convert_inline(children);
            // Hoist a lone image out of its paragraph wrapper (block-level schema).
            if let Some(img) = sole_image_node(&inline) {
                return Some(img);
            }
            Some(tiptap::node_with_content("paragraph", inline))
        }
        Block::BlockQuote { children } => {
            if let Some(callout) = convert_callout_if_matched(children, source) {
                return Some(callout);
            }
            let inner: Vec<Value> = children
                .iter()
                .filter_map(|b| convert_block(b, source))
                .collect();
            Some(tiptap::node_with_content("blockquote", inner))
        }
        Block::List { start, items } => Some(convert_list(*start, items, source)),
        Block::CodeBlock { lang, code } => {
            let mut n = tiptap::node("codeBlock");
            if let Some(lang) = lang {
                let mut attrs = Map::new();
                attrs.insert("language".into(), Value::from(lang.clone()));
                n["attrs"] = Value::Object(attrs);
            }
            // Trim the trailing newline the parser preserves at block end.
            let code = code.strip_suffix('\n').unwrap_or(code);
            if !code.is_empty() {
                n["content"] = Value::Array(vec![tiptap::text(code, None)]);
            }
            Some(n)
        }
        Block::ThematicBreak => Some(tiptap::node("horizontalRule")),
        Block::Table { head, rows } => Some(convert_table(head, rows)),
        Block::HtmlBlock { raw } => {
            let raw = raw.trim_matches('\n').to_string();
            if let Some(p) = placeholder::parse(&raw) {
                return Some(feishu_placeholder_block(&p));
            }
            if let Some(src) = parse_video_src(&raw) {
                return Some(video_node(&rewrite_src_for_runtime(&src)));
            }
            Some(raw_markdown_block(&raw))
        }
    }
}

fn convert_list(start: Option<u64>, items: &[ListItem], source: &str) -> Value {
    match start {
        Some(s) => {
            let converted: Vec<Value> = items.iter().map(|i| convert_list_item(i, source)).collect();
            let mut n = tiptap::node_with_content("orderedList", converted);
            if s != 1 {
                let mut attrs = Map::new();
                attrs.insert("start".into(), Value::from(s as i64));
                n["attrs"] = Value::Object(attrs);
            }
            n
        }
        None => {
            // GFM task list: any checkbox item turns the whole list into a
            // taskList; items missing a checkbox coerce to unchecked so the
            // list stays schema-consistent.
            if items.iter().any(|i| i.checkbox.is_some()) {
                tiptap::node_with_content(
                    "taskList",
                    items.iter().map(|i| convert_task_item(i, source)).collect(),
                )
            } else {
                tiptap::node_with_content(
                    "bulletList",
                    items.iter().map(|i| convert_list_item(i, source)).collect(),
                )
            }
        }
    }
}

fn convert_list_item(item: &ListItem, source: &str) -> Value {
    let inner: Vec<Value> = item
        .blocks
        .iter()
        .filter_map(|b| convert_block(b, source))
        .collect();
    tiptap::node_with_content("listItem", inner)
}

fn convert_task_item(item: &ListItem, source: &str) -> Value {
    let inner: Vec<Value> = item
        .blocks
        .iter()
        .filter_map(|b| convert_block(b, source))
        .collect();
    let mut n = tiptap::node_with_content("taskItem", inner);
    let mut attrs = Map::new();
    attrs.insert("checked".into(), Value::Bool(item.checkbox.unwrap_or(false)));
    n["attrs"] = Value::Object(attrs);
    n
}

fn convert_table(head: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>]) -> Value {
    let mut out_rows: Vec<Value> = Vec::new();
    out_rows.push(tiptap::node_with_content(
        "tableRow",
        head.iter().map(|c| wrap_cell(c, "tableHeader")).collect(),
    ));
    for row in rows {
        out_rows.push(tiptap::node_with_content(
            "tableRow",
            row.iter().map(|c| wrap_cell(c, "tableCell")).collect(),
        ));
    }
    tiptap::node_with_content("table", out_rows)
}

/// Tiptap's table-cell schema requires `block+` — wrap cell inlines in a
/// paragraph (empty content when the cell is empty).
fn wrap_cell(cell: &[Inline], kind: &str) -> Value {
    let inline = convert_inline(cell);
    let paragraph = if inline.is_empty() {
        tiptap::node("paragraph")
    } else {
        tiptap::node_with_content("paragraph", inline)
    };
    tiptap::node_with_content(kind, vec![paragraph])
}

// MARK: - callouts

/// GitHub callout: a blockquote whose first paragraph leads with `[!TYPE]`.
/// Returns None for plain blockquotes. codeBlock/table/hr descendants are
/// dropped (飞书高亮块 schema constraint), and an empty body seeds one empty
/// paragraph (`(paragraph|…)+` requires at least one block).
fn convert_callout_if_matched(children: &[Block], source: &str) -> Option<Value> {
    let Block::Paragraph {
        children: para_inlines,
        ..
    } = children.first()?
    else {
        return None;
    };
    // pulldown-cmark parses `[!NOTE]` as a would-be link and, on failure,
    // splits it into `Text("[") Text("!NOTE") Text("]")` — swift-markdown
    // hands us a single Text node instead. Join the leading text run and
    // match the marker against the whole run: it must BE the marker, not
    // merely start with it (so `[!NOTE] tail` stays a plain blockquote).
    let mut text_run = 0;
    let mut token = String::new();
    for inl in para_inlines {
        match inl {
            Inline::Text(t) => {
                token.push_str(t);
                text_run += 1;
            }
            _ => break,
        }
    }
    if !(token.starts_with("[!") && token.ends_with(']')) {
        return None;
    }
    let type_raw = &token[2..token.len() - 1];
    if type_raw.is_empty() || !type_raw.chars().all(|c| c.is_alphabetic()) {
        return None;
    }
    let callout_type = type_raw.to_lowercase();

    // Strip the marker's text run and one following soft break; the rest is
    // body inline.
    let mut remaining: &[Inline] = &para_inlines[text_run..];
    if matches!(remaining.first(), Some(Inline::SoftBreak)) {
        remaining = &remaining[1..];
    }

    let mut content: Vec<Value> = Vec::new();
    if !remaining.is_empty() {
        let inline_nodes = convert_inline(remaining);
        content.push(tiptap::node_with_content("paragraph", inline_nodes));
    }

    const DISALLOWED: &[&str] = &["codeBlock", "table", "horizontalRule"];
    for child in &children[1..] {
        if let Some(block) = convert_block(child, source) {
            if DISALLOWED.contains(&tiptap::node_type(&block)) {
                continue;
            }
            content.push(block);
        }
    }
    if content.is_empty() {
        content.push(tiptap::node("paragraph"));
    }

    let mut n = tiptap::node_with_content("callout", content);
    let mut attrs = Map::new();
    attrs.insert("type".into(), Value::from(callout_type));
    n["attrs"] = Value::Object(attrs);
    Some(n)
}

// MARK: - math

/// `$$…$$` display math. The paragraph must be pure text/breaks; the latex
/// body is sliced from the source verbatim (the inline tree mangles `\\`).
fn convert_math_block_if_matched(
    children: &[Inline],
    range: &std::ops::Range<usize>,
    source: &str,
) -> Option<Value> {
    for child in children {
        match child {
            Inline::Text(_) | Inline::SoftBreak | Inline::HardBreak => {}
            _ => return None,
        }
    }
    let raw = source.get(range.clone())?;
    let trimmed = raw.trim();
    if !(trimmed.starts_with("$$") && trimmed.ends_with("$$") && trimmed.len() >= 4) {
        return None;
    }
    let mut inner = &trimmed[2..trimmed.len() - 2];
    // Strip exactly one leading/trailing newline (canonical 3-line form);
    // everything else survives verbatim.
    if let Some(rest) = inner.strip_prefix('\n') {
        inner = rest;
    }
    if let Some(rest) = inner.strip_suffix('\n') {
        inner = rest;
    }
    if inner.is_empty() {
        return None; // `$$$$` / `$$\n$$` stay literal paragraphs
    }
    let mut n = tiptap::node("math_block");
    let mut attrs = Map::new();
    attrs.insert("latex".into(), Value::from(inner));
    n["attrs"] = Value::Object(attrs);
    Some(n)
}

// MARK: - video

/// A paragraph consisting solely of `<video …></video>` inline-HTML fragments
/// (plus whitespace filler) becomes a `video` node.
fn convert_video_if_matched(children: &[Inline]) -> Option<Value> {
    let mut raw = String::new();
    let mut saw_html = false;
    for child in children {
        match child {
            Inline::InlineHtml(h) => {
                raw.push_str(h);
                saw_html = true;
            }
            Inline::Text(t) if t.trim().is_empty() => {}
            _ => return None,
        }
    }
    if !saw_html {
        return None;
    }
    let trimmed = raw.trim();
    if !trimmed.to_lowercase().starts_with("<video") {
        return None;
    }
    match parse_video_src(trimmed) {
        Some(src) => Some(video_node(&rewrite_src_for_runtime(&src))),
        None => Some(raw_markdown_block(trimmed)),
    }
}

fn video_node(runtime_src: &str) -> Value {
    let mut n = tiptap::node("video");
    let mut attrs = Map::new();
    attrs.insert("src".into(), Value::from(runtime_src));
    n["attrs"] = Value::Object(attrs);
    n
}

/// Extract `src="…"` / `src='…'` from a `<video …>` tag; None ⇒ caller falls
/// back to a verbatim raw block.
pub fn parse_video_src(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !trimmed.to_lowercase().starts_with("<video") {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..].len() >= 3 && trimmed[i..].to_lowercase().starts_with("src") {
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                    let quote = bytes[j];
                    let value_start = j + 1;
                    let mut k = value_start;
                    while k < bytes.len() && bytes[k] != quote {
                        k += 1;
                    }
                    if k < bytes.len() {
                        let value = &trimmed[value_start..k];
                        return if value.is_empty() {
                            None
                        } else {
                            Some(value.to_string())
                        };
                    }
                }
            }
        }
        i += 1;
    }
    None
}

// MARK: - feishu placeholder

/// `pub(crate)`:F1 的 `feishu/converter.rs` 把飞书拉取侧的占位块载荷
/// 适配成 `FeishuPlaceholder` 后复用本函数,保证魔法注释输入路径与
/// 飞书 API 输入路径落在同一 Tiptap 节点形状上。
pub(crate) fn feishu_placeholder_block(p: &placeholder::FeishuPlaceholder) -> Value {
    let mut attrs = Map::new();
    attrs.insert("type".into(), Value::from(p.block_type.clone()));
    attrs.insert("block_id".into(), Value::from(p.block_id.clone()));
    attrs.insert("title".into(), Value::from(p.title.clone()));
    attrs.insert("url".into(), Value::from(p.url.clone()));
    if let Some(t) = &p.block_token {
        attrs.insert("block_token".into(), Value::from(t.clone()));
    }
    if let Some(s) = &p.summary {
        attrs.insert("summary".into(), Value::from(s.clone()));
    }
    if let Some(c) = &p.created_in_feishu_at {
        attrs.insert("created_in_feishu_at".into(), Value::from(c.clone()));
    }
    if !p.unknown_fields.is_empty() {
        attrs.insert(
            "unknown_fields".into(),
            Value::Array(
                p.unknown_fields
                    .iter()
                    .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
                    .collect(),
            ),
        );
    }
    let mut n = tiptap::node("feishu_placeholder_block");
    n["attrs"] = Value::Object(attrs);
    n
}

fn raw_markdown_block(raw: &str) -> Value {
    let mut n = tiptap::node("raw_markdown_block");
    let mut attrs = Map::new();
    attrs.insert("raw".into(), Value::from(raw));
    n["attrs"] = Value::Object(attrs);
    n
}

// MARK: - inline

fn convert_inline(children: &[Inline]) -> Vec<Value> {
    let mut nodes = Vec::new();
    for child in children {
        walk_inline(child, &[], &mut nodes);
    }
    coalesce_adjacent_text(nodes)
}

/// Merge adjacent same-mark text runs — the serializer joins inline children
/// onto one line, so a reparse would fuse them anyway; coalescing keeps the
/// FIRST parse canonical (parse-stable invariant #56).
fn coalesce_adjacent_text(nodes: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for node in nodes {
        let mergeable = tiptap::node_type(&node) == "text"
            && node.get("content").is_none()
            && matches!(out.last(), Some(last) if tiptap::node_type(last) == "text"
                        && last.get("content").is_none()
                        && tiptap::marks_equal(last.get("marks"), node.get("marks")));
        if mergeable {
            let last = out.last_mut().unwrap();
            let merged = format!(
                "{}{}",
                last.get("text").and_then(Value::as_str).unwrap_or(""),
                node.get("text").and_then(Value::as_str).unwrap_or("")
            );
            last["text"] = Value::from(merged);
        } else {
            out.push(node);
        }
    }
    out
}

fn walk_inline(inline: &Inline, marks: &[Value], nodes: &mut Vec<Value>) {
    let with = |kind: &str, attrs: Option<Map<String, Value>>| {
        let mut m = marks.to_vec();
        m.push(tiptap::mark(kind, attrs));
        m
    };
    match inline {
        Inline::Text(t) => append_text_with_inline_math(t, marks, nodes),
        Inline::Strong(children) => {
            let m = with("bold", None);
            for c in children {
                walk_inline(c, &m, nodes);
            }
        }
        Inline::Emphasis(children) => {
            let m = with("italic", None);
            for c in children {
                walk_inline(c, &m, nodes);
            }
        }
        Inline::Strikethrough(children) => {
            let m = with("strike", None);
            for c in children {
                walk_inline(c, &m, nodes);
            }
        }
        Inline::Code(code) => nodes.push(tiptap::text(code, Some(with("code", None)))),
        Inline::Link {
            dest,
            title,
            children,
        } => {
            let mut attrs = Map::new();
            attrs.insert("href".into(), Value::from(dest.clone()));
            if !title.is_empty() {
                attrs.insert("title".into(), Value::from(title.clone()));
            }
            let m = with("link", Some(attrs));
            for c in children {
                walk_inline(c, &m, nodes);
            }
        }
        Inline::Image { dest, title, alt } => {
            let mut attrs = Map::new();
            attrs.insert("src".into(), Value::from(rewrite_src_for_runtime(dest)));
            if !alt.is_empty() {
                attrs.insert("alt".into(), Value::from(alt.clone()));
            }
            if !title.is_empty() {
                attrs.insert("title".into(), Value::from(title.clone()));
            }
            let mut n = tiptap::node("image");
            n["attrs"] = Value::Object(attrs);
            nodes.push(n);
        }
        Inline::HardBreak => nodes.push(tiptap::node("hardBreak")),
        Inline::SoftBreak => {
            // CommonMark soft breaks render as a space between words.
            nodes.push(tiptap::text(" ", if marks.is_empty() { None } else { Some(marks.to_vec()) }));
        }
        Inline::InlineHtml(_) => {
            // Inline HTML outside the lone-<video> case is dropped, matching
            // swift-markdown's InlineHTML falling into the default arm.
        }
    }
}

/// Split a text run into text + `math_inline` nodes. Strict rules (product
/// decision): `$` opens math only when the next char is non-space; the closer
/// is unescaped, preceded by a non-space, and on the same line. `\$` stays
/// literal. Marks apply to the surrounding text only — math nodes are atoms.
fn append_text_with_inline_math(s: &str, marks: &[Value], nodes: &mut Vec<Value>) {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut pending = String::new();
    let mut i = 0;

    macro_rules! flush {
        () => {
            if !pending.is_empty() {
                nodes.push(tiptap::text(
                    &pending,
                    if marks.is_empty() { None } else { Some(marks.to_vec()) },
                ));
                pending.clear();
            }
        };
    }

    while i < n {
        let c = chars[i];
        // Escaped dollar: keep `\$` literal, never a delimiter.
        if c == '\\' && i + 1 < n && chars[i + 1] == '$' {
            pending.push('\\');
            pending.push('$');
            i += 2;
            continue;
        }
        if c == '$' && i + 1 < n && !chars[i + 1].is_whitespace() {
            // Find an unescaped closing `$` on the same line whose preceding
            // char is non-space.
            let mut j = i + 1;
            let mut found: Option<usize> = None;
            while j < n {
                let cj = chars[j];
                if cj == '\n' {
                    break;
                }
                if cj == '$' {
                    let mut back = 0;
                    let mut k = j as isize - 1;
                    while k >= 0 && chars[k as usize] == '\\' {
                        back += 1;
                        k -= 1;
                    }
                    if back % 2 == 0 && !chars[j - 1].is_whitespace() {
                        found = Some(j);
                        break;
                    }
                }
                j += 1;
            }
            // `$$` / `$$$$` are not empty formulas — require ≥1 char inside.
            if let Some(f) = found.filter(|f| *f > i + 1) {
                flush!();
                let latex: String = chars[i + 1..f].iter().collect();
                let mut node = tiptap::node("math_inline");
                let mut attrs = Map::new();
                attrs.insert("latex".into(), Value::from(latex));
                node["attrs"] = Value::Object(attrs);
                nodes.push(node);
                i = f + 1;
                continue;
            }
        }
        pending.push(c);
        i += 1;
    }
    flush!();
}

// MARK: - asset src rewrite

/// Disk `./assets/<file>` (or bare `assets/<file>`) → runtime asset URL.
/// Everything else (absolute paths, http(s), other relative dirs) untouched.
/// First save normalizes back to `./assets/`.
pub fn rewrite_src_for_runtime(src: &str) -> String {
    const CANONICAL: &str = "./assets/";
    const BARE: &str = "assets/";
    let filename = if let Some(rest) = src.strip_prefix(CANONICAL) {
        rest
    } else if let Some(rest) = src.strip_prefix(BARE) {
        rest
    } else {
        return src.to_string();
    };
    assets::asset_url(filename)
}

/// If the converted inline content is exactly one image (plus whitespace-only
/// text), return that image node for hoisting; else None.
fn sole_image_node(inline: &[Value]) -> Option<Value> {
    let mut image: Option<&Value> = None;
    for node in inline {
        match tiptap::node_type(node) {
            "image" => {
                if image.is_some() {
                    return None;
                }
                image = Some(node);
            }
            "text" => {
                if !node
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                {
                    return None;
                }
            }
            _ => return None,
        }
    }
    image.cloned()
}
