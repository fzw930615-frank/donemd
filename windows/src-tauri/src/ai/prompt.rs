//! Port of `CommandPromptBuilder.swift` — pure functions, no side effects.
//! Composes `command + selection + context` into the chat messages handed to
//! the provider client. Kept byte-identical in *content* with the Swift
//! prompts so both platforms elicit the same model behaviour.

/// An AI 助手 command. The `kind` string is what the JS side sends in the
/// `aiCommand` envelope (`VisualWebView.aiCommand(kind:arg:)` on macOS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiCommand {
    // 改写组
    Polish,
    Formal,
    Colloquial,
    Simplify,
    CustomRewrite(String),
    // 翻译组
    TranslateToEnglish,
    TranslateToChinese,
    TranslateTo(String),
    // 转换组
    Summarize,
    Expand,
    Outline,
    ToTable,
    // 延伸组
    ContinueWriting,
    // 生成组 (slash-command, inputOnly)
    WriteOutline(String),
    ExpandTopic(String),
    FreePrompt(String),
}

/// How much document context travels with the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextScope {
    /// Only the selected text (翻译类).
    SelectionOnly,
    /// Selection paragraph ± N paragraphs (改写 / 转换).
    SurroundingParagraphs,
    /// The whole document (续写).
    WholeDocument,
    /// Only the user-typed input (生成类).
    InputOnly,
}

impl AiCommand {
    /// Map a JS command kind (+ optional arg) to a command. Returns None for
    /// unrecognized kinds (macOS falls back to polish there, but the bridge
    /// logs and drops — matching VisualWebView's `guard` behaviour).
    pub fn from_kind(kind: &str, arg: Option<&str>) -> Option<AiCommand> {
        let arg = arg.unwrap_or("");
        Some(match kind {
            "polish" => AiCommand::Polish,
            "formal" => AiCommand::Formal,
            "colloquial" => AiCommand::Colloquial,
            "simplify" => AiCommand::Simplify,
            "customRewrite" => AiCommand::CustomRewrite(arg.to_string()),
            "translateToEnglish" => AiCommand::TranslateToEnglish,
            "translateToChinese" => AiCommand::TranslateToChinese,
            "translateTo" => AiCommand::TranslateTo(if arg.is_empty() { "英文".into() } else { arg.into() }),
            "summarize" => AiCommand::Summarize,
            "expand" => AiCommand::Expand,
            "outline" => AiCommand::Outline,
            "toTable" => AiCommand::ToTable,
            "continueWriting" => AiCommand::ContinueWriting,
            "writeOutline" => AiCommand::WriteOutline(arg.to_string()),
            "expandTopic" => AiCommand::ExpandTopic(arg.to_string()),
            "freePrompt" => AiCommand::FreePrompt(arg.to_string()),
            _ => return None,
        })
    }

    /// The Chinese instruction sent as the `system` message. Most carry an
    /// anti-preamble tail so the model doesn't wrap output in chatter that
    /// would land inside the document on replace.
    pub fn instruction(&self) -> String {
        const DIRECT: &str = "直接输出结果，不要任何前后说明。";
        match self {
            AiCommand::Polish => format!("在不改变原意的前提下，让以下文本更自然流畅。{DIRECT}"),
            AiCommand::Formal => format!("将以下文本改写为正式书面语。{DIRECT}"),
            AiCommand::Colloquial => format!("将以下文本改写为日常口语。{DIRECT}"),
            AiCommand::Simplify => format!("在保留要点的前提下精简以下文本。{DIRECT}"),
            AiCommand::CustomRewrite(intent) => {
                format!("请按以下要求改写文本：{intent}。{DIRECT}")
            }
            AiCommand::TranslateToEnglish => {
                format!("将以下文本翻译为英文，保留原有的 Markdown 结构。{DIRECT}")
            }
            AiCommand::TranslateToChinese => {
                format!("将以下文本翻译为中文，保留原有的 Markdown 结构。{DIRECT}")
            }
            AiCommand::TranslateTo(lang) => {
                format!("将以下文本翻译为{lang}，保留原有的 Markdown 结构。{DIRECT}")
            }
            AiCommand::Summarize => format!("用 1-3 句话总结以下文本的要点。{DIRECT}"),
            AiCommand::Expand => format!("在以下文本的基础上展开补充，使内容更充实。{DIRECT}"),
            AiCommand::Outline => format!("提取以下文本的要点，列为 Markdown 大纲。{DIRECT}"),
            // Deliberately NOT the strict anti-preamble: the command must be
            // able to say "不适合" instead of forcing a table.
            AiCommand::ToTable => {
                "如果以下内容适合表格化，输出一个 Markdown 表格；否则只回复「不适合」。不要任何额外说明。"
                    .to_string()
            }
            AiCommand::ContinueWriting => format!("基于以下文本，自然地续写一段。{DIRECT}"),
            AiCommand::WriteOutline(topic) => {
                format!("请围绕「{topic}」生成一个 Markdown 大纲。{DIRECT}")
            }
            AiCommand::ExpandTopic(topic) => format!("请围绕「{topic}」展开写成段落。{DIRECT}"),
            AiCommand::FreePrompt(_) => format!("请完成用户的要求。{DIRECT}"),
        }
    }

    pub fn context_scope(&self) -> ContextScope {
        match self {
            AiCommand::Polish
            | AiCommand::Formal
            | AiCommand::Colloquial
            | AiCommand::Simplify
            | AiCommand::CustomRewrite(_)
            | AiCommand::Summarize
            | AiCommand::Expand
            | AiCommand::Outline
            | AiCommand::ToTable => ContextScope::SurroundingParagraphs,
            AiCommand::TranslateToEnglish
            | AiCommand::TranslateToChinese
            | AiCommand::TranslateTo(_) => ContextScope::SelectionOnly,
            AiCommand::ContinueWriting => ContextScope::WholeDocument,
            AiCommand::WriteOutline(_)
            | AiCommand::ExpandTopic(_)
            | AiCommand::FreePrompt(_) => ContextScope::InputOnly,
        }
    }
}

/// The editor-side context the JS bubble menu extracted from the live
/// ProseMirror selection and sent across the bridge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionContext {
    pub selection: String,
    pub paragraph: String,
    /// Blocks before the selection paragraph, document order (nearest last).
    pub before: Vec<String>,
    /// Blocks after it, document order (nearest first).
    pub after: Vec<String>,
    /// Full document markdown — only consulted for WholeDocument scope.
    pub full_document: Option<String>,
}

/// One chat message in the provider-agnostic shape; each client maps it onto
/// its wire format (OpenAI messages[], Anthropic system+messages[], Gemini
/// contents[] + systemInstruction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiMessage {
    pub role: &'static str, // "system" | "user" | "assistant"
    pub content: String,
}

pub struct PromptBuildResult {
    pub messages: Vec<AiMessage>,
    /// True when the 8K guard rebuilt the prompt selection-only.
    pub did_degrade: bool,
}

/// Soft ceiling on total prompt size (chars ≈ tokens, over-estimating — the
/// right bias for a safety ceiling). Past this, rebuild selection-only.
pub const TOKEN_BUDGET: usize = 8000;

pub fn build(command: &AiCommand, context: &SelectionContext, context_range: i64) -> PromptBuildResult {
    let system = command.instruction();
    let user = user_message(command.context_scope(), context, context_range);

    // 8K guard: SelectionOnly / InputOnly have nothing to strip — never degrade.
    let scope = command.context_scope();
    if scope != ContextScope::SelectionOnly
        && scope != ContextScope::InputOnly
        && system.chars().count() + user.chars().count() > TOKEN_BUDGET
    {
        let degraded = user_message(ContextScope::SelectionOnly, context, context_range);
        return PromptBuildResult {
            messages: vec![
                AiMessage { role: "system", content: system },
                AiMessage { role: "user", content: degraded },
            ],
            did_degrade: true,
        };
    }

    PromptBuildResult {
        messages: vec![
            AiMessage { role: "system", content: system },
            AiMessage { role: "user", content: user },
        ],
        did_degrade: false,
    }
}

fn user_message(scope: ContextScope, context: &SelectionContext, context_range: i64) -> String {
    match scope {
        // InputOnly: the typed topic/prompt rides in `selection` — same
        // framing as SelectionOnly, just the bare text.
        ContextScope::SelectionOnly | ContextScope::InputOnly => context.selection.clone(),

        ContextScope::SurroundingParagraphs => {
            // Nearest `context_range` neighbours on each side: last N before
            // (nearest is last), first N after (nearest is first).
            let n = context_range.max(0) as usize;
            let before: &[String] = if n == 0 {
                &[]
            } else {
                let start = context.before.len().saturating_sub(n);
                &context.before[start..]
            };
            let after: &[String] = if n == 0 {
                &[]
            } else {
                &context.after[..context.after.len().min(n)]
            };

            let mut reference: Vec<&str> = Vec::new();
            reference.extend(before.iter().map(String::as_str).filter(|s| !s.is_empty()));
            if !context.paragraph.is_empty() && context.paragraph != context.selection {
                reference.push(&context.paragraph);
            }
            reference.extend(after.iter().map(String::as_str).filter(|s| !s.is_empty()));

            let mut parts: Vec<String> = Vec::new();
            if !reference.is_empty() {
                parts.push("上下文（仅供参考，不要改写）：".to_string());
                parts.extend(reference.iter().map(|s| s.to_string()));
                parts.push(String::new()); // blank line before the target
            }
            parts.push("需要处理的文本：".to_string());
            parts.push(context.selection.clone());
            parts.join("\n")
        }

        ContextScope::WholeDocument => {
            let doc = context.full_document.as_deref().unwrap_or(&context.selection);
            format!("全文：\n{doc}\n\n续写起点为全文末尾。")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(selection: &str, paragraph: &str, before: &[&str], after: &[&str]) -> SelectionContext {
        SelectionContext {
            selection: selection.into(),
            paragraph: paragraph.into(),
            before: before.iter().map(|s| s.to_string()).collect(),
            after: after.iter().map(|s| s.to_string()).collect(),
            full_document: None,
        }
    }

    #[test]
    fn all_kinds_roundtrip_from_kind() {
        // Every contract kind must map (VisualWebView.aiCommand parity).
        for kind in [
            "polish", "formal", "colloquial", "simplify", "customRewrite",
            "translateToEnglish", "translateToChinese", "translateTo",
            "summarize", "expand", "outline", "toTable", "continueWriting",
            "writeOutline", "expandTopic", "freePrompt",
        ] {
            assert!(AiCommand::from_kind(kind, Some("x")).is_some(), "kind {kind}");
        }
        assert!(AiCommand::from_kind("nope", None).is_none());
    }

    #[test]
    fn build_returns_system_then_user() {
        let r = build(&AiCommand::Polish, &ctx("hello", "hello", &[], &[]), 1);
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.messages[0].role, "system");
        assert_eq!(r.messages[1].role, "user");
        assert!(!r.did_degrade);
    }

    #[test]
    fn polish_includes_before_and_after_paragraphs() {
        let c = ctx("选区", "本段", &["上一段"], &["下一段"]);
        let r = build(&AiCommand::Polish, &c, 1);
        let user = &r.messages[1].content;
        assert!(user.contains("上下文（仅供参考，不要改写）："));
        assert!(user.contains("上一段"));
        assert!(user.contains("本段"));
        assert!(user.contains("下一段"));
        assert!(user.contains("需要处理的文本：\n选区"));
    }

    #[test]
    fn context_range_zero_drops_neighbours() {
        let c = ctx("选区", "本段", &["上一段"], &["下一段"]);
        let r = build(&AiCommand::Polish, &c, 0);
        let user = &r.messages[1].content;
        assert!(!user.contains("上一段"));
        assert!(!user.contains("下一段"));
        assert!(user.contains("本段"));
    }

    #[test]
    fn translation_is_selection_only_ignoring_neighbours() {
        let c = ctx("选区", "本段", &["上一段"], &["下一段"]);
        let r = build(&AiCommand::TranslateToEnglish, &c, 3);
        assert_eq!(r.messages[1].content, "选区");
    }

    #[test]
    fn selection_verbatim_survives() {
        let c = ctx("a **b** `c` $x$", "a **b** `c` $x$", &[], &[]);
        let r = build(&AiCommand::Formal, &c, 1);
        assert!(r.messages[1].content.ends_with("a **b** `c` $x$"));
    }

    #[test]
    fn continue_writing_sends_full_document() {
        let mut c = ctx("选区", "本段", &[], &[]);
        c.full_document = Some("# 全文\n\n正文".into());
        let r = build(&AiCommand::ContinueWriting, &c, 1);
        assert_eq!(r.messages[1].content, "全文：\n# 全文\n\n正文\n\n续写起点为全文末尾。");
    }

    #[test]
    fn continue_writing_falls_back_to_selection_without_full_doc() {
        let c = ctx("光标前文本", "本段", &[], &[]);
        let r = build(&AiCommand::ContinueWriting, &c, 1);
        assert_eq!(r.messages[1].content, "全文：\n光标前文本\n\n续写起点为全文末尾。");
    }

    #[test]
    fn input_only_sends_bare_input() {
        let c = ctx("Q3 计划", "", &[], &[]);
        let r = build(&AiCommand::WriteOutline("Q3 计划".into()), &c, 1);
        assert_eq!(r.messages[1].content, "Q3 计划");
        assert!(r.messages[0].content.contains("Q3 计划"));
    }

    #[test]
    fn over_budget_degrades_to_selection_only() {
        let huge = "长".repeat(TOKEN_BUDGET + 100);
        let c = ctx("选区", &huge, &[], &[]);
        let r = build(&AiCommand::Summarize, &c, 1);
        assert!(r.did_degrade);
        assert_eq!(r.messages[1].content, "选区");
    }

    #[test]
    fn selection_only_never_degrades() {
        let huge = "长".repeat(TOKEN_BUDGET + 100);
        let c = ctx(&huge, &huge, &[], &[]);
        let r = build(&AiCommand::TranslateToChinese, &c, 1);
        assert!(!r.did_degrade);
    }

    #[test]
    fn to_table_allows_not_suitable_answer() {
        // The instruction must permit 不适合 rather than forcing a table.
        let i = AiCommand::ToTable.instruction();
        assert!(i.contains("不适合"));
        assert!(!i.contains("直接输出结果"));
    }
}
