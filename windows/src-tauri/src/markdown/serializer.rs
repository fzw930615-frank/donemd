//! Port of `Serializer.swift` — Tiptap document JSON → canonical Markdown.
//!
//! Canonical form (ADR-0002):
//!   ATX headings · `-` bullets · explicit `<n>. ` ordered items · exactly one
//!   blank line between blocks · ``` fenced code · pipe tables without
//!   padding · 2-space continuation indent · runtime asset URLs rewrite to
//!   `./assets/<file>` · exactly one trailing newline.
//!
//! `parse → serialize` is a fixed point on canonical input.

use serde_json::Value;

use super::placeholder::{self, FeishuPlaceholder};
use super::tiptap;

/// Serialize a Tiptap doc to Markdown. Empty/invalid docs ⇒ `"\n"`.
pub fn serialize_document(root: &Value) -> String {
    if tiptap::node_type(root) != "doc" {
        return "\n".to_string();
    }
    let parts: Vec<String> = tiptap::content(root)
        .iter()
        .filter_map(serialize_block)
        .collect();
    if parts.is_empty() {
        return "\n".to_string();
    }
    parts.join("\n\n") + "\n"
}

fn serialize_block(node: &Value) -> Option<String> {
    match tiptap::node_type(node) {
        "paragraph" => Some(serialize_inline_children(tiptap::content(node))),
        "heading" => {
            let level = tiptap::attr_i64(node, "level").unwrap_or(1).clamp(1, 6) as usize;
            let prefix = "#".repeat(level);
            let inline = serialize_inline_children(tiptap::content(node));
            Some(if inline.is_empty() {
                prefix
            } else {
                format!("{prefix} {inline}")
            })
        }
        "blockquote" => {
            let inner = tiptap::content(node)
                .iter()
                .filter_map(serialize_block)
                .collect::<Vec<_>>()
                .join("\n\n");
            Some(prefix_lines(&inner))
        }
        "callout" => {
            let callout_type = tiptap::attr_str(node, "type").unwrap_or("note").to_uppercase();
            let body = tiptap::content(node)
                .iter()
                .filter_map(serialize_block)
                .collect::<Vec<_>>()
                .join("\n\n");
            let inner = if body.is_empty() {
                format!("[!{callout_type}]")
            } else {
                format!("[!{callout_type}]\n{body}")
            };
            Some(prefix_lines(&inner))
        }
        "bulletList" => Some(serialize_list(node, false)),
        "orderedList" => Some(serialize_list(node, true)),
        "taskList" => Some(serialize_task_list(node)),
        "table" => Some(serialize_table(node)),
        "codeBlock" => {
            let language = tiptap::attr_str(node, "language").unwrap_or("");
            let body: String = tiptap::content(node)
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect();
            Some(format!("```{language}\n{body}\n```"))
        }
        "horizontalRule" => Some("---".to_string()),
        "image" => Some(serialize_inline(node)),
        "video" => {
            let src = rewrite_src_for_disk(tiptap::attr_str(node, "src").unwrap_or(""));
            Some(format!("<video controls src=\"{src}\"></video>"))
        }
        "raw_markdown_block" => {
            Some(tiptap::attr_str(node, "raw").unwrap_or("").to_string())
        }
        "math_block" => {
            let latex = tiptap::attr_str(node, "latex").unwrap_or("");
            Some(format!("$$\n{latex}\n$$"))
        }
        "feishu_placeholder_block" => Some(serialize_feishu_placeholder(node)),
        _ => None,
    }
}

/// `> ` line prefixing shared by blockquote and callout (blank lines get a
/// bare `>`).
fn prefix_lines(inner: &str) -> String {
    inner
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn serialize_feishu_placeholder(node: &Value) -> String {
    let unknown_fields: Vec<(String, String)> = node
        .get("attrs")
        .and_then(|a| a.get("unknown_fields"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let k = item.get("key")?.as_str()?.to_string();
                    let v = item.get("value")?.as_str()?.to_string();
                    Some((k, v))
                })
                .collect()
        })
        .unwrap_or_default();
    placeholder::serialize(&FeishuPlaceholder {
        block_type: tiptap::attr_str(node, "type").unwrap_or("").to_string(),
        block_id: tiptap::attr_str(node, "block_id").unwrap_or("").to_string(),
        block_token: tiptap::attr_str(node, "block_token").map(str::to_string),
        title: tiptap::attr_str(node, "title").unwrap_or("").to_string(),
        summary: tiptap::attr_str(node, "summary").map(str::to_string),
        url: tiptap::attr_str(node, "url").unwrap_or("").to_string(),
        created_in_feishu_at: tiptap::attr_str(node, "created_in_feishu_at").map(str::to_string),
        unknown_fields,
    })
}

fn serialize_list(list: &Value, ordered: bool) -> String {
    let items = tiptap::content(list);
    let start = if ordered {
        tiptap::attr_i64(list, "start").unwrap_or(1)
    } else {
        1
    };
    let mut lines: Vec<String> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let bullet = if ordered {
            format!("{}. ", start + index as i64)
        } else {
            "- ".to_string()
        };
        let indent = " ".repeat(bullet.chars().count());
        let body: Vec<String> = tiptap::content(item)
            .iter()
            .filter_map(serialize_block)
            .collect();
        for (block_index, block) in body.iter().enumerate() {
            for (line_index, line) in block.split('\n').enumerate() {
                if block_index == 0 && line_index == 0 {
                    lines.push(format!("{bullet}{line}"));
                } else if line.is_empty() {
                    lines.push(String::new());
                } else {
                    lines.push(format!("{indent}{line}"));
                }
            }
            if block_index + 1 < body.len() {
                lines.push(String::new());
            }
        }
    }
    lines.join("\n")
}

fn serialize_task_list(list: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    for item in tiptap::content(list) {
        let checked = tiptap::attr_bool(item, "checked").unwrap_or(false);
        let bullet = if checked { "- [x] " } else { "- [ ] " };
        let indent = " ".repeat(bullet.len());
        let body: Vec<String> = tiptap::content(item)
            .iter()
            .filter_map(serialize_block)
            .collect();
        for (block_index, block) in body.iter().enumerate() {
            for (line_index, line) in block.split('\n').enumerate() {
                if block_index == 0 && line_index == 0 {
                    lines.push(format!("{bullet}{line}"));
                } else if line.is_empty() {
                    lines.push(String::new());
                } else {
                    lines.push(format!("{indent}{line}"));
                }
            }
            if block_index + 1 < body.len() {
                lines.push(String::new());
            }
        }
    }
    lines.join("\n")
}

/// GFM table: pipe-delimited, no manual padding. First row is the header,
/// followed by a `|---|` alignment row.
fn serialize_table(table: &Value) -> String {
    let rows = tiptap::content(table);
    let Some(first) = rows.first() else {
        return String::new();
    };
    let header_cells: Vec<String> = tiptap::content(first).iter().map(cell_inline).collect();
    let separator: Vec<&str> = header_cells.iter().map(|_| "---").collect();
    let mut lines = vec![
        format!("| {} |", header_cells.join(" | ")),
        format!("| {} |", separator.join(" | ")),
    ];
    for row in &rows[1..] {
        let cells: Vec<String> = tiptap::content(row).iter().map(cell_inline).collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

/// A cell's first block collapsed to a single inline string — GFM tables
/// can't express block structure.
fn cell_inline(cell: &Value) -> String {
    let blocks = tiptap::content(cell);
    let Some(first) = blocks.first() else {
        return String::new();
    };
    let inline = tiptap::content(first);
    if !inline.is_empty() {
        serialize_inline_children(inline)
    } else {
        collect_text(first)
    }
}

/// Depth-first concatenation of every descendant `text` — last-resort
/// fallback for a cell block with no direct inline content.
fn collect_text(node: &Value) -> String {
    let mut out = node
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    for child in tiptap::content(node) {
        out += &collect_text(child);
    }
    out
}

// MARK: - inline

fn serialize_inline_children(nodes: &[Value]) -> String {
    nodes.iter().map(serialize_inline).collect()
}

fn serialize_inline(node: &Value) -> String {
    match tiptap::node_type(node) {
        "text" => apply_marks(
            node.get("text").and_then(Value::as_str).unwrap_or(""),
            node.get("marks").and_then(Value::as_array),
        ),
        "hardBreak" => "\\\n".to_string(),
        "image" => {
            let src = rewrite_src_for_disk(tiptap::attr_str(node, "src").unwrap_or(""));
            let alt = tiptap::attr_str(node, "alt").unwrap_or("");
            match tiptap::attr_str(node, "title") {
                Some(title) if !title.is_empty() => format!("![{alt}]({src} \"{title}\")"),
                _ => format!("![{alt}]({src})"),
            }
        }
        "math_inline" => {
            let latex = tiptap::attr_str(node, "latex").unwrap_or("");
            format!("${latex}$")
        }
        // Unknown inline node: best-effort serialize children.
        _ => serialize_inline_children(tiptap::content(node)),
    }
}

/// Apply marks innermost-first so the resulting Markdown nests correctly.
fn apply_marks(text: &str, marks: Option<&Vec<Value>>) -> String {
    let mut result = text.to_string();
    for mark in marks.cloned().unwrap_or_default() {
        match mark.get("type").and_then(Value::as_str).unwrap_or("") {
            "code" => result = format!("`{result}`"),
            "italic" => result = format!("*{result}*"),
            "bold" => result = format!("**{result}**"),
            "strike" => result = format!("~~{result}~~"),
            "link" => {
                let href = mark
                    .get("attrs")
                    .and_then(|a| a.get("href"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let title = mark
                    .get("attrs")
                    .and_then(|a| a.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                result = if title.is_empty() {
                    format!("[{result}]({href})")
                } else {
                    format!("[{result}]({href} \"{title}\")")
                };
            }
            _ => {}
        }
    }
    result
}

// MARK: - asset src rewrite

/// Runtime asset URL → disk-form `./assets/<filename>`. Both platform forms
/// are accepted: `donemd-asset://<file>` (macOS) and
/// `http://donemd-asset.localhost/<file>` (Windows/WebView2) — documents move
/// between platforms only in disk form.
pub fn rewrite_src_for_disk(src: &str) -> String {
    const SCHEME: &str = "donemd-asset://";
    const WINDOWS: &str = "http://donemd-asset.localhost/";
    if let Some(rest) = src.strip_prefix(SCHEME) {
        return format!("./assets/{rest}");
    }
    if let Some(rest) = src.strip_prefix(WINDOWS) {
        return format!("./assets/{rest}");
    }
    src.to_string()
}
