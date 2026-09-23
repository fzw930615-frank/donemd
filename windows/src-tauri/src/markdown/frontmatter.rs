//! Port of `Frontmatter.swift` + `FrontmatterEngine.swift`.
//!
//! Splits a `.md` source into (frontmatter, body) and rebuilds it losslessly.
//! Promises (ADR-0005 / ADR-0002 § Frontmatter):
//!   - parse → serialize is a fixed point: user fields round-trip byte-for-byte
//!     as raw text blocks; only the `feishu:` subtree is regenerated.
//!   - bad YAML ⇒ no frontmatter, whole source is body.
//!   - `+++` (TOML fence) is rejected.
//!   - scan bounded to 1 MB.

/// Cap on the region scanned for a closing `---`.
pub const MAX_FENCE_SCAN_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frontmatter {
    /// Top-level user keys in original order, each a raw text slice from
    /// `<key>:` through the line preceding the next key (incl. trailing \n
    /// unless last).
    pub user_fields: Vec<(String, String)>,
    /// Typed `feishu:` subtree; `None` when absent.
    pub feishu: Option<FeishuFrontmatter>,
    /// Position of `feishu:` among all top-level keys at parse time.
    pub feishu_original_index: Option<usize>,
    /// The source had an explicit `---` fence (even an empty one).
    pub has_fence: bool,
}

impl Frontmatter {
    pub fn is_effectively_empty(&self) -> bool {
        self.user_fields.is_empty() && self.feishu.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeishuFrontmatter {
    pub doc_token: Option<String>,
    pub doc_url: Option<String>,
    pub last_pulled_revision: Option<i64>,
    /// Kept as the original ISO-8601 string — nothing in the port does date
    /// math on it, and a verbatim string guarantees round-trip fidelity.
    pub last_pushed_at: Option<String>,
    pub placeholder_blocks: Vec<PlaceholderBlockRef>,
    /// Unrecognized `feishu:` sub-keys, stored as ready-to-emit YAML text
    /// (column-0 form; indented by 2 on emit).
    pub unknown_fields: Vec<String>,
}

impl FeishuFrontmatter {
    pub fn is_empty(&self) -> bool {
        self.doc_token.is_none()
            && self.doc_url.is_none()
            && self.last_pulled_revision.is_none()
            && self.last_pushed_at.is_none()
            && self.placeholder_blocks.is_empty()
            && self.unknown_fields.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderBlockRef {
    pub block_id: String,
    pub block_type: String,
    pub title: Option<String>,
}

/// Detect a leading frontmatter block; return `(frontmatter, body)`.
/// Any failure ⇒ `(Frontmatter::default(), source)`.
pub fn parse(source: &str) -> (Frontmatter, &str) {
    let Some((yaml_body, body)) = split_fence(source) else {
        return (Frontmatter::default(), source);
    };
    match parse_yaml_body(yaml_body) {
        Some(fm) => (fm, body),
        // Malformed YAML degrades to "no frontmatter"; the original `---`
        // lines stay in the body (thematic break + paragraph).
        None => (Frontmatter::default(), source),
    }
}

/// Re-emit `(frontmatter, body)` as one `.md` source.
pub fn serialize(frontmatter: &Frontmatter, body: &str) -> String {
    let Some(yaml_body) = emit_yaml_body(frontmatter) else {
        return body.to_string();
    };
    format!("---\n{yaml_body}---\n{body}")
}

/// Merge: user fields preserved verbatim; `feishu:` replaced wholesale.
pub fn merge(existing: &Frontmatter, incoming: Frontmatter) -> Frontmatter {
    let mut result = existing.clone();
    if let Some(new_feishu) = incoming.feishu {
        result.feishu = Some(new_feishu);
        if result.feishu_original_index.is_none() {
            result.feishu_original_index = Some(result.user_fields.len());
        }
        result.has_fence = true;
    }
    result
}

// MARK: - fence splitting

/// Locate `---\n … \n---\n`; return (yaml body, residual body text).
fn split_fence(source: &str) -> Option<(&str, &str)> {
    if !source.starts_with("---\n") {
        return None;
    }
    let body_start = 4;
    let scan_end = source
        .len()
        .min(body_start + MAX_FENCE_SCAN_BYTES);
    let region = &source[body_start..scan_end];
    let mut offset = 0;
    for line in region.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        if trimmed == "---" {
            let closing_start = body_start + offset;
            let after = &source[closing_start..];
            // Body starts on the line after the closing fence.
            let body = match after.find('\n') {
                Some(nl) => &after[nl + 1..],
                None => "",
            };
            return Some((&source[body_start..closing_start], body));
        }
        offset += line.len();
    }
    // Also handle a closing fence at exact EOF without trailing newline.
    if region.ends_with("---") && region.len() >= 3 {
        let closing_start = body_start + region.len() - 3;
        // Ensure it's on its own line.
        if closing_start == body_start || source.as_bytes()[closing_start - 1] == b'\n' {
            return Some((&source[body_start..closing_start], ""));
        }
    }
    None
}

// MARK: - parse helpers

fn parse_yaml_body(yaml_body: &str) -> Option<Frontmatter> {
    if yaml_body.trim().is_empty() {
        // Fence present but no fields — keep has_fence so the fence round-trips.
        return Some(Frontmatter {
            has_fence: true,
            ..Default::default()
        });
    }
    let parsed: serde_yaml::Value = serde_yaml::from_str(yaml_body).ok()?;
    let mapping = parsed.as_mapping()?; // top level must be a mapping

    let key_positions = top_level_key_positions(yaml_body);
    let lines: Vec<&str> = yaml_body.split('\n').collect();

    let mut user_fields: Vec<(String, String)> = Vec::new();
    let mut feishu_index: Option<usize> = None;
    let mut feishu: Option<FeishuFrontmatter> = None;

    for (i, (key, line_index)) in key_positions.iter().enumerate() {
        let start_line = if i == 0 { 0 } else { *line_index };
        let end_line = if i + 1 < key_positions.len() {
            key_positions[i + 1].1
        } else {
            lines.len()
        };
        let mut raw_block = lines[start_line..end_line].join("\n");
        if end_line < lines.len() {
            raw_block.push('\n');
        }

        if key == "feishu" {
            feishu_index = Some(i);
            feishu = Some(match mapping.get(serde_yaml::Value::String("feishu".into())) {
                Some(serde_yaml::Value::Mapping(m)) => parse_feishu_mapping(m),
                // Present but not a mapping (e.g. `feishu: ~`) → empty subtree;
                // round-trip will normalize.
                _ => FeishuFrontmatter::default(),
            });
        } else {
            user_fields.push((key.clone(), raw_block));
        }
    }

    Some(Frontmatter {
        user_fields,
        feishu,
        feishu_original_index: feishu_index,
        has_fence: true,
    })
}

/// Top-level `<ident>:` lines in source order (simple identifiers only —
/// quoted/dotted keys fall outside the schema and keep riding inside the
/// previous key's raw block, same trade-off as the Swift version).
fn top_level_key_positions(yaml_body: &str) -> Vec<(String, usize)> {
    yaml_body
        .split('\n')
        .enumerate()
        .filter_map(|(i, line)| match_top_level_key(line).map(|k| (k, i)))
        .collect()
}

fn match_top_level_key(line: &str) -> Option<String> {
    let mut chars = line.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut end = first.len_utf8();
    for (i, c) in chars {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            end = i + c.len_utf8();
        } else if c == ':' {
            // After the colon: whitespace or EOL required (block-mapping key).
            let after = &line[i + 1..];
            if after.is_empty() || after.starts_with(' ') || after.starts_with('\t') {
                return Some(line[..i].to_string());
            }
            return None;
        } else {
            return None;
        }
    }
    let _ = end;
    None
}

fn parse_feishu_mapping(m: &serde_yaml::Mapping) -> FeishuFrontmatter {
    let mut out = FeishuFrontmatter::default();
    for (k, v) in m {
        let Some(key) = k.as_str() else { continue };
        match key {
            "doc_token" => out.doc_token = v.as_str().map(str::to_string),
            "doc_url" => out.doc_url = v.as_str().map(str::to_string),
            "last_pulled_revision" => out.last_pulled_revision = v.as_i64(),
            "last_pushed_at" => out.last_pushed_at = v.as_str().map(str::to_string),
            "placeholder_blocks" => {
                if let Some(seq) = v.as_sequence() {
                    for entry in seq {
                        let Some(em) = entry.as_mapping() else { continue };
                        let get = |name: &str| {
                            em.get(serde_yaml::Value::String(name.into()))
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        };
                        let (Some(block_id), Some(block_type)) = (get("block_id"), get("type"))
                        else {
                            continue;
                        };
                        out.placeholder_blocks.push(PlaceholderBlockRef {
                            block_id,
                            block_type,
                            title: get("title"),
                        });
                    }
                }
            }
            _ => {
                // Unknown sub-key: re-serialize the value as YAML text for
                // verbatim-ish round-trip (ADR-0005 accepts normalization here).
                if let Some(raw) = encode_unknown_field(key, v) {
                    out.unknown_fields.push(raw);
                }
            }
        }
    }
    out
}

/// `key: <yaml>` text, column-0, trailing newline — ready to indent under
/// `feishu:` at emit time.
fn encode_unknown_field(key: &str, value: &serde_yaml::Value) -> Option<String> {
    let mut serialized = serde_yaml::to_string(value).ok()?;
    while serialized.ends_with('\n') {
        serialized.pop();
    }
    // serde_yaml wraps documents with `---`; strip that marker line.
    if let Some(rest) = serialized.strip_prefix("---\n") {
        serialized = rest.to_string();
    } else if serialized == "---" {
        serialized.clear();
    }
    match value {
        serde_yaml::Value::Sequence(_) | serde_yaml::Value::Mapping(_) => {
            let indented = serialized
                .split('\n')
                .map(|l| if l.is_empty() { String::new() } else { format!("  {l}") })
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!("{key}:\n{indented}\n"))
        }
        _ => Some(format!("{key}: {serialized}\n")),
    }
}

// MARK: - serialize helpers

fn emit_yaml_body(frontmatter: &Frontmatter) -> Option<String> {
    if !frontmatter.has_fence && frontmatter.is_effectively_empty() {
        return None;
    }
    let user_fields = &frontmatter.user_fields;
    let Some(feishu) = &frontmatter.feishu else {
        return Some(user_fields.iter().map(|(_, raw)| raw.clone()).collect());
    };
    // Splice the regenerated feishu subtree back at its original index.
    let insert_at = frontmatter
        .feishu_original_index
        .unwrap_or(user_fields.len())
        .min(user_fields.len());
    let mut output = String::new();
    for (_, raw) in &user_fields[..insert_at] {
        output += raw;
    }
    output += &emit_feishu_subtree(feishu);
    for (_, raw) in &user_fields[insert_at..] {
        output += raw;
    }
    Some(output)
}

fn emit_feishu_subtree(feishu: &FeishuFrontmatter) -> String {
    if feishu.is_empty() {
        return "feishu: {}\n".to_string();
    }
    let mut s = String::from("feishu:\n");
    if let Some(token) = &feishu.doc_token {
        s += &format!("  doc_token: {}\n", yaml_emit_scalar(token));
    }
    if let Some(url) = &feishu.doc_url {
        s += &format!("  doc_url: {}\n", yaml_emit_scalar(url));
    }
    if let Some(rev) = feishu.last_pulled_revision {
        s += &format!("  last_pulled_revision: {rev}\n");
    }
    if let Some(date) = &feishu.last_pushed_at {
        s += &format!("  last_pushed_at: {}\n", yaml_emit_scalar(date));
    }
    if !feishu.placeholder_blocks.is_empty() {
        s += "  placeholder_blocks:\n";
        for block in &feishu.placeholder_blocks {
            s += &format!("    - block_id: {}\n", yaml_emit_scalar(&block.block_id));
            s += &format!("      type: {}\n", yaml_emit_scalar(&block.block_type));
            if let Some(title) = &block.title {
                s += &format!("      title: {}\n", yaml_emit_scalar(title));
            }
        }
    }
    for unknown in &feishu.unknown_fields {
        s += &indent_unknown_field(unknown);
    }
    s
}

fn indent_unknown_field(raw: &str) -> String {
    let mut lines: Vec<&str> = raw.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
        .iter()
        .map(|l| if l.is_empty() { String::new() } else { format!("  {l}") })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Plain-when-safe, quoted-otherwise scalar emission (Yams parity).
fn yaml_emit_scalar(s: &str) -> String {
    let mut out = serde_yaml::to_string(s).unwrap_or_else(|_| s.to_string());
    while out.ends_with('\n') {
        out.pop();
    }
    if let Some(rest) = out.strip_prefix("---\n") {
        out = rest.to_string();
    }
    // serde_yaml emits "..." for strings needing double quotes and appends
    // nothing otherwise; Yams prefers single quotes. Both are valid YAML and
    // reparse identically — feishu fields are app-managed, normalization is
    // expected (ADR-0005).
    out
}
