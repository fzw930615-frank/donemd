// AI-readability estimate for the document body — a TypeScript port of
// `donemd/Outline/DocumentSizeEstimate.swift` (#82). The macOS top bar is
// native SwiftUI and computes in Swift; the Windows source pane is a bare
// webview, so the readout lives in-page and computes from the markdown this
// pane already receives (no native round-trip).
//
// The metric is a deliberately-labeled `≈` token estimate from a
// CJK-weighted character heuristic: CJK ≈ 0.6 token/char, everything else
// ≈ 1 token per 4 chars. Every model's tokenizer differs; the estimate only
// needs to place the doc in a readability tier.

export type DocSizeTier = 'comfortable' | 'large' | 'tooLarge';

export interface DocSizeEstimate {
  bytes: number;
  chars: number;
  tokens: number;
  tier: DocSizeTier;
}

// Tier ceilings in estimated tokens (shared definition with the Swift side).
const COMFORTABLE_CEILING = 16_000;
const LARGE_CEILING = 64_000;

export function estimateDocSize(bodyMarkdown: string): DocSizeEstimate {
  const bytes = new TextEncoder().encode(bodyMarkdown).length;
  // Code points (≈ Swift's Character count for CJK prose; grapheme clusters
  // would need Intl.Segmenter — overkill for a tooltip).
  const chars = [...bodyMarkdown].length;
  let cjk = 0;
  let other = 0;
  for (const ch of bodyMarkdown) {
    if (isCJK(ch.codePointAt(0)!)) cjk += 1;
    else other += 1;
  }
  const tokens = Math.round(cjk * 0.6 + other / 4);
  const tier: DocSizeTier =
    tokens <= COMFORTABLE_CEILING
      ? 'comfortable'
      : tokens <= LARGE_CEILING
        ? 'large'
        : 'tooLarge';
  return { bytes, chars, tokens, tier };
}

/// Ranges dense enough to warrant the per-character token weight (same table
/// as the Swift `isCJK`).
function isCJK(cp: number): boolean {
  return (
    (cp >= 0x3040 && cp <= 0x30ff) || // Hiragana + Katakana
    (cp >= 0x3400 && cp <= 0x4dbf) || // CJK Extension A
    (cp >= 0x4e00 && cp <= 0x9fff) || // CJK Unified Ideographs
    (cp >= 0xf900 && cp <= 0xfaff) || // CJK Compatibility Ideographs
    (cp >= 0xff00 && cp <= 0xffef) || // Full-width forms (、。!?)
    (cp >= 0xac00 && cp <= 0xd7af) || // Hangul syllables
    (cp >= 0x20000 && cp <= 0x2a6df) // CJK Extension B
  );
}

/// Plain-language verdict — what an AI can do with the doc, no tier jargon.
export const TIER_VERDICT: Record<DocSizeTier, string> = {
  comfortable: 'AI 可轻松读完整篇',
  large: '较大，部分 AI 需分段读',
  tooLarge: '过大，建议拆分后再给 AI',
};

/// "≈12K tokens" — rounds to K above 1000, one decimal below 10K.
export function tokensCompact(tokens: number): string {
  if (tokens >= 1000) {
    const k = tokens / 1000;
    return k < 10 ? `≈${k.toFixed(1)}K tokens` : `≈${Math.round(k)}K tokens`;
  }
  return `≈${tokens} tokens`;
}

/// "34 KB" (1024-based, no decimals; raw bytes under 1 KB) — tooltip detail.
export function bytesReadable(bytes: number): string {
  return bytes >= 1024 ? `${Math.round(bytes / 1024)} KB` : `${bytes} B`;
}

/// "8,200 字" — thousands-grouped character count (tooltip detail, mirrors
/// the Swift `charactersReadable`). Fixed en-US grouping so it's comma-style
/// regardless of the host locale.
export function charactersReadable(chars: number): string {
  return `${chars.toLocaleString('en-US')} 字`;
}
