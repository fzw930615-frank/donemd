//! 飞书文档 URL → token — `SyncRoot/FeishuURLDetector.swift` 的最小移植。
//!
//! 只解决「用户粘进来的这串东西里,文档 token 是哪一段」。**不判别**
//! docx 还是 wiki —— 那由 `pull_command::resolve_docx_token` 的探针确定性地
//! 解决(飞书新版 token 不透明,靠路径段猜 wiki/docx 会在「解析成功后写回
//! docx token、URL 仍是 wiki」的场景反向误判,详见该函数注释)。
//!
//! 这里只做一件事:把 token 抠出来。

/// 支持的路径段 —— 飞书云文档的两种入口。`docs` 是旧版文档(仍在用)。
const DOC_SEGMENTS: [&str; 3] = ["docx", "wiki", "docs"];

/// 从用户输入里抠出文档 token。接受完整 URL,也接受直接粘的裸 token。
///
/// 返回 `None` 表示「看不出这是什么」—— 调用方应提示用户粘贴文档链接,
/// 而不是拿一段垃圾去打 API。
pub fn extract_doc_token(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 裸 token:没有斜杠也没有协议头。飞书 token 是 URL 安全字符,
    // 带斜杠就说明用户贴的是路径/链接,该走下面的解析。
    if !trimmed.contains('/') {
        return sanitize_token(trimmed);
    }

    // 剥掉 query 与 fragment,剩下的按 `/` 切段。
    let path_only = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed);
    let segments: Vec<&str> = path_only
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // 找 docx/wiki/docs 段,取它后面那一段。从后往前找 —— 租户域名里
    // 出现 "docs" 之类的可能性比路径里低,但真出现时应以最后一个为准。
    for (i, seg) in segments.iter().enumerate().rev() {
        if DOC_SEGMENTS.contains(&seg.to_ascii_lowercase().as_str()) {
            return segments.get(i + 1).and_then(|t| sanitize_token(t));
        }
    }
    None
}

/// token 合法性下限:非空、只含 URL 安全字符。飞书 token 是
/// 字母数字加少量符号;放进来的东西要直接拼进 API 路径/查询,
/// 所以这里拒掉明显不是 token 的输入(空格、中文、控制字符)。
fn sanitize_token(candidate: &str) -> Option<String> {
    let token = candidate.trim();
    if token.is_empty() {
        return None;
    }
    let ok = token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok {
        Some(token.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_from_docx_url() {
        assert_eq!(
            extract_doc_token("https://mi.feishu.cn/docx/doxcnAbc123XYZ").as_deref(),
            Some("doxcnAbc123XYZ")
        );
    }

    /// 用户的真实场景:wiki 链接。
    #[test]
    fn extracts_from_wiki_url() {
        assert_eq!(
            extract_doc_token("https://mi.feishu.cn/wiki/WfbgwfAOeiq534k5EvScYzBdnve").as_deref(),
            Some("WfbgwfAOeiq534k5EvScYzBdnve")
        );
    }

    #[test]
    fn extracts_from_legacy_docs_url() {
        assert_eq!(
            extract_doc_token("https://x.feishu.cn/docs/oldTokenHere").as_deref(),
            Some("oldTokenHere")
        );
    }

    #[test]
    fn handles_larksuite_and_other_hosts() {
        assert_eq!(
            extract_doc_token("https://example.larksuite.com/docx/doxcnLark").as_deref(),
            Some("doxcnLark")
        );
    }

    #[test]
    fn strips_query_and_fragment() {
        assert_eq!(
            extract_doc_token("https://x.feishu.cn/wiki/TOK123?from=space&sheet=1").as_deref(),
            Some("TOK123")
        );
        assert_eq!(
            extract_doc_token("https://x.feishu.cn/docx/TOK123#heading-2").as_deref(),
            Some("TOK123")
        );
    }

    #[test]
    fn tolerates_trailing_slash_and_whitespace() {
        assert_eq!(
            extract_doc_token("  https://x.feishu.cn/docx/TOK123/  ").as_deref(),
            Some("TOK123")
        );
    }

    #[test]
    fn path_segment_match_is_case_insensitive() {
        assert_eq!(
            extract_doc_token("https://x.feishu.cn/DOCX/TOK123").as_deref(),
            Some("TOK123")
        );
    }

    /// 直接粘 token 也认 —— 用户不一定复制整条 URL。
    #[test]
    fn accepts_bare_token() {
        assert_eq!(
            extract_doc_token("WfbgwfAOeiq534k5EvScYzBdnve").as_deref(),
            Some("WfbgwfAOeiq534k5EvScYzBdnve")
        );
        assert_eq!(extract_doc_token("  doxcnAbc  ").as_deref(), Some("doxcnAbc"));
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert_eq!(extract_doc_token(""), None);
        assert_eq!(extract_doc_token("   "), None);
        // 有斜杠但没有已知文档段 —— 看不出是什么
        assert_eq!(extract_doc_token("https://feishu.cn/"), None);
        assert_eq!(extract_doc_token("https://example.com/a/b/c"), None);
        // 带空格/中文的裸串不是 token
        assert_eq!(extract_doc_token("这不是 token"), None);
        assert_eq!(extract_doc_token("有中文"), None);
    }

    /// 已知段后面没东西 → None(而不是返回空 token 去打 API)。
    #[test]
    fn rejects_doc_segment_without_token() {
        assert_eq!(extract_doc_token("https://x.feishu.cn/docx"), None);
        assert_eq!(extract_doc_token("https://x.feishu.cn/docx/"), None);
    }

    /// 路径里出现多次已知段时以最后一个为准。
    #[test]
    fn last_doc_segment_wins() {
        assert_eq!(
            extract_doc_token("https://x.feishu.cn/wiki/spaceX/docx/REALTOKEN").as_deref(),
            Some("REALTOKEN")
        );
    }
}
