//! `FeishuCalloutType.swift` 的移植 — Done.md 的 5 个 GitHub 风格
//! callout 类型与飞书 `(emoji, background_color)` 载荷之间的规范映射。
//!
//! 这是「PRD § 飞书 callout 类型映射表」引用的**单一真源**:生产代码、
//! 测试、PRD 都指向这里。表要变(比如换 emoji),改这里并同步
//! PRD/CONTEXT.md 文字。
//!
//! 解析方向:
//! - **本地 → 飞书**:按类型取 `emoji` + `background_color`。
//! - **飞书 → 本地**:先按 `background_color` 分发(与 5 色一一对应)。
//!   `emoji` 仅信息性 — 用户可能已在飞书手动改过,类型仍需稳定。
//!
//! 本文件同时收编了 `FeishuBlockEncoder.swift` 里的浅色调色板表
//! (int 1-15 ↔ `light-*` 名):飞书线上用整数编码 callout 背景色,
//! Done.md 全代码面用字符串名,编码/解码都经这一对函数中转。

/// Done.md 的 5 个 GitHub 风格 callout 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeishuCalloutType {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl FeishuCalloutType {
    /// `CaseIterable` 对应物。
    pub const ALL: [FeishuCalloutType; 5] = [
        FeishuCalloutType::Note,
        FeishuCalloutType::Tip,
        FeishuCalloutType::Important,
        FeishuCalloutType::Warning,
        FeishuCalloutType::Caution,
    ];

    pub fn emoji(&self) -> &'static str {
        match self {
            FeishuCalloutType::Note => "💡",
            FeishuCalloutType::Tip => "✨",
            FeishuCalloutType::Important => "❗",
            FeishuCalloutType::Warning => "⚠️",
            FeishuCalloutType::Caution => "🚨",
        }
    }

    /// 飞书 callout 端点要求的线上 `emoji_id` — **与 [`Self::emoji`]
    /// 不同**:线上要命名 ID(`"bulb"`、`"sparkles"` …),不是 Unicode
    /// 字形。发 Unicode 码点会被 1770006 schema mismatch 拒收
    /// (2026-06-04 真机确认)。
    ///
    /// 名字来源:https://open.feishu.cn/document/docs/docs/data-structure/emoji
    /// (与 feishu-mcp-pro 的 CALLOUT_EMOJI_MAP 交叉核对)。只需要
    /// 5 条映射 — converter 发出的 callout 只有这 5 个规范类型,
    /// 用户随手敲的 emoji 不会走到这条路径,不需要全量 ~900 条
    /// Unicode→命名 查找表。
    pub fn wire_emoji_id(&self) -> &'static str {
        match self {
            FeishuCalloutType::Note => "bulb",
            FeishuCalloutType::Tip => "sparkles",
            FeishuCalloutType::Important => "exclamation",
            FeishuCalloutType::Warning => "warning",
            FeishuCalloutType::Caution => "rotating_light",
        }
    }

    pub fn background_color(&self) -> &'static str {
        match self {
            FeishuCalloutType::Note => "light-blue",
            FeishuCalloutType::Tip => "light-green",
            FeishuCalloutType::Important => "light-purple",
            FeishuCalloutType::Warning => "light-yellow",
            FeishuCalloutType::Caution => "light-red",
        }
    }

    /// Swift 的 `rawValue` — converter 的 Tiptap callout 节点
    /// `attrs.type` 用这个小写名(`note` … `caution`,与
    /// `markdown/convert.rs` 的 callout 属性同源);序列化成 GitHub
    /// 语法时再大写为 `[!NOTE]`。
    pub fn as_str(&self) -> &'static str {
        match self {
            FeishuCalloutType::Note => "note",
            FeishuCalloutType::Tip => "tip",
            FeishuCalloutType::Important => "important",
            FeishuCalloutType::Warning => "warning",
            FeishuCalloutType::Caution => "caution",
        }
    }

    /// Swift `FeishuCalloutType(rawValue:)` — Tiptap `attrs.type`
    /// (调用前已 lowercased)反查类型;未知名返回 `None`,调用方
    /// 回落 `Note`(converter 的发射路径)。
    pub fn from_raw_name(name: &str) -> Option<FeishuCalloutType> {
        FeishuCalloutType::ALL
            .into_iter()
            .find(|t| t.as_str() == name)
    }

    /// 从飞书 callout 载荷反查类型:颜色是主判据,emoji 被忽略(用户
    /// 可能已在飞书改过 emoji,类型要稳定)。未知颜色回落 `Note`
    /// (最中性的近似)— 规范 5 色之外的 callout 不会让 converter 崩,
    /// 真机文档若需要更宽的调色板,验收时能看到。
    pub fn from(_emoji: Option<&str>, background_color: Option<&str>) -> FeishuCalloutType {
        match background_color {
            Some("light-blue") => FeishuCalloutType::Note,
            Some("light-green") => FeishuCalloutType::Tip,
            Some("light-purple") => FeishuCalloutType::Important,
            Some("light-yellow") => FeishuCalloutType::Warning,
            Some("light-red") => FeishuCalloutType::Caution,
            _ => FeishuCalloutType::Note,
        }
    }
}

/// 浅色调色板(原 `FeishuBlockEncoder.swift` 文件级表):飞书线上用
/// 整数 1-15 编码 callout 的 `background_color`,Done.md 的字符串面
/// (FeishuCalloutType / converter / 测试)用 `light-blue` 等名字。
///
/// 数字于 2026-05-30 与 feishu-mcp-pro 的 color-maps.js LIGHT_COLORS
/// 表交叉核对。别名(red ↔ light-red 等)解析到同一数字;反查返回
/// `light-*` 规范形。9-15 飞书保留但未映射到 Done.md 规范色 —
/// 编解码仍可经整数往返。
const LIGHT_COLOR_NAMES: &[(&str, i64)] = &[
    ("light-red", 1),
    ("light-orange", 2),
    ("light-yellow", 3),
    ("light-green", 4),
    ("light-blue", 5),
    ("light-purple", 6),
    ("light-gray", 7),
    ("dark-gray", 8),
];

/// 别名 — 同一数字的另一种拼法(用户/飞书 UI 有时会发);回写时用
/// 规范的 `light-*` 形。
const LIGHT_COLOR_ALIASES: &[(&str, i64)] = &[
    ("red", 1),
    ("orange", 2),
    ("yellow", 3),
    ("green", 4),
    ("blue", 5),
    ("purple", 6),
    ("light-grey", 7),
    ("gray", 7),
    ("grey", 7),
    ("pale-gray", 7),
    ("dark-grey", 8),
];

/// `light-*` 名(含别名,大小写不敏感)→ 线上整数。未知名返回 `None`
/// — 推送侧随即省略 background_color 字段(与 nil 同效),不阻塞。
pub fn light_color_number_for_name(name: &str) -> Option<i64> {
    let lowered = name.to_ascii_lowercase();
    LIGHT_COLOR_NAMES
        .iter()
        .chain(LIGHT_COLOR_ALIASES.iter())
        .find(|(n, _)| *n == lowered)
        .map(|(_, num)| *num)
}

/// 线上整数 → 规范 `light-*` 名。9-15 等未映射值返回 `None`
/// (拉取侧容忍老/伪响应发字符串,字段整个缺失时也是 `None`)。
pub fn light_color_name_for_number(number: i64) -> Option<&'static str> {
    LIGHT_COLOR_NAMES
        .iter()
        .find(|(_, num)| *num == number)
        .map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Swift `testCalloutAllFiveTypesMapDistinctColors` 的类型层锚点:
    /// 5 类型 → (emoji, wire emoji_id, 背景色) 各自不同且钉死。
    /// wire_emoji_id 必须是命名 ID 而非 Unicode 字形(1770006 修复)。
    #[test]
    fn all_five_types_map_distinct_emoji_wire_ids_and_colors() {
        let cases = [
            (FeishuCalloutType::Note, "💡", "bulb", "light-blue"),
            (FeishuCalloutType::Tip, "✨", "sparkles", "light-green"),
            (FeishuCalloutType::Important, "❗", "exclamation", "light-purple"),
            (FeishuCalloutType::Warning, "⚠️", "warning", "light-yellow"),
            (FeishuCalloutType::Caution, "🚨", "rotating_light", "light-red"),
        ];
        for (t, emoji, wire_id, color) in cases {
            assert_eq!(t.emoji(), emoji, "{t:?}");
            assert_eq!(t.wire_emoji_id(), wire_id, "{t:?}");
            assert_eq!(t.background_color(), color, "{t:?}");
        }
    }

    /// wire_emoji_id 一律 ASCII 命名 ID — 防 Unicode 字形回归进线上路径。
    #[test]
    fn wire_emoji_ids_are_ascii_named_not_unicode() {
        for t in FeishuCalloutType::ALL {
            assert!(t.wire_emoji_id().is_ascii(), "{}: {:?}", t.wire_emoji_id(), t);
            assert_ne!(t.wire_emoji_id(), t.emoji());
        }
    }

    /// Swift `testCalloutTypeReverseLookupIsTotal`:5 色反查全覆盖。
    #[test]
    fn reverse_lookup_is_total() {
        for t in FeishuCalloutType::ALL {
            let resolved =
                FeishuCalloutType::from(Some(t.emoji()), Some(t.background_color()));
            assert_eq!(resolved, t, "{t:?}");
        }
    }

    /// Swift `testCalloutTypeUnknownColorFallsBackToNote`。
    #[test]
    fn unknown_color_falls_back_to_note() {
        assert_eq!(
            FeishuCalloutType::from(Some("🌟"), Some("neon-pink")),
            FeishuCalloutType::Note
        );
        // 字段缺失 / None 也回落 Note。
        assert_eq!(FeishuCalloutType::from(None, None), FeishuCalloutType::Note);
    }

    /// emoji 只是信息性 — 与颜色矛盾时仍以颜色为准。
    #[test]
    fn reverse_lookup_ignores_emoji() {
        assert_eq!(
            FeishuCalloutType::from(Some("🚨"), Some("light-blue")),
            FeishuCalloutType::Note
        );
    }

    #[test]
    fn light_color_roundtrip_eight_names() {
        for (name, num) in LIGHT_COLOR_NAMES {
            assert_eq!(light_color_number_for_name(name), Some(*num), "{name}");
            assert_eq!(light_color_name_for_number(*num), Some(*name), "{num}");
        }
    }

    #[test]
    fn light_color_aliases_resolve_to_same_number() {
        for (alias, num) in LIGHT_COLOR_ALIASES {
            assert_eq!(light_color_number_for_name(alias), Some(*num), "{alias}");
        }
    }

    /// 9-15 飞书保留 — 数字→名不可映,但名→数字这条路本来就没有这些名。
    #[test]
    fn light_color_numbers_nine_to_fifteen_unmapped() {
        for num in 9..=15 {
            assert_eq!(light_color_name_for_number(num), None, "{num}");
        }
    }

    #[test]
    fn light_color_unknown_name_is_none() {
        assert_eq!(light_color_number_for_name("neon-pink"), None);
        assert_eq!(light_color_number_for_name(""), None);
    }

    /// `rawValue` ↔ `from_raw_name` 往返 — converter 的 callout 发射
    /// 路径(`emitCallout`)依赖这对函数,未知名必须回落 None。
    #[test]
    fn raw_name_roundtrip() {
        for t in FeishuCalloutType::ALL {
            assert_eq!(FeishuCalloutType::from_raw_name(t.as_str()), Some(t));
        }
        assert_eq!(FeishuCalloutType::from_raw_name("neon"), None);
        assert_eq!(FeishuCalloutType::from_raw_name(""), None);
        // 大写名不匹配 — 调用方(converter)先 lowercased,这里不做宽容。
        assert_eq!(FeishuCalloutType::from_raw_name("NOTE"), None);
    }

    /// Swift 查找前 `lowercased()` — 保持大小写不敏感。
    #[test]
    fn light_color_lookup_is_case_insensitive() {
        assert_eq!(light_color_number_for_name("Light-Blue"), Some(5));
        assert_eq!(light_color_number_for_name("RED"), Some(1));
    }
}
