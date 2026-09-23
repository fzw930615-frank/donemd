// doc-size.ts unit tests — port of donemdTests/DocumentSizeEstimateTests.swift
// (#82), keeping the shared thresholds and display formats pinned.
import { describe, expect, it } from 'vitest';
import {
  bytesReadable,
  charactersReadable,
  estimateDocSize,
  TIER_VERDICT,
  tokensCompact,
} from './doc-size';

describe('estimateDocSize', () => {
  it('empty document is a zero comfortable estimate', () => {
    const est = estimateDocSize('');
    expect(est).toEqual({ bytes: 0, chars: 0, tokens: 0, tier: 'comfortable' });
  });

  it('byte vs character count differs for CJK (3 UTF-8 bytes per han char)', () => {
    const est = estimateDocSize('你好');
    expect(est.bytes).toBe(6);
    expect(est.chars).toBe(2);
  });

  it('latin tokens ≈ chars / 4', () => {
    const est = estimateDocSize('a'.repeat(400));
    expect(est.tokens).toBe(100);
  });

  it('CJK tokens use the per-character 0.6 weight', () => {
    const est = estimateDocSize('汉'.repeat(100));
    expect(est.tokens).toBe(60);
  });

  it('mixed text adds both classes', () => {
    // 10 CJK (→6) + 40 latin (→10) = 16
    const est = estimateDocSize('汉'.repeat(10) + 'a'.repeat(40));
    expect(est.tokens).toBe(16);
  });

  it('full-width punctuation counts as CJK', () => {
    // 5 han + 5 full-width punctuation = 10 CJK → 6 tokens
    const est = estimateDocSize('你好世界啊。，！？：');
    expect(est.tokens).toBe(6);
  });

  it('tier boundaries: ≤16K comfortable, 16–64K large, >64K tooLarge', () => {
    // Drive tokens via latin chars (4 chars/token) for exact counts.
    expect(estimateDocSize('a'.repeat(64_000)).tier).toBe('comfortable'); // 16000
    expect(estimateDocSize('a'.repeat(64_004)).tier).toBe('large'); // 16001
    expect(estimateDocSize('a'.repeat(256_000)).tier).toBe('large'); // 64000
    expect(estimateDocSize('a'.repeat(256_004)).tier).toBe('tooLarge'); // 64001
  });
});

describe('display helpers', () => {
  it('tokensCompact below 1000 shows the raw number', () => {
    expect(tokensCompact(0)).toBe('≈0 tokens');
    expect(tokensCompact(999)).toBe('≈999 tokens');
  });

  it('tokensCompact keeps one decimal below 10K', () => {
    expect(tokensCompact(4200)).toBe('≈4.2K tokens');
  });

  it('tokensCompact rounds to whole K at or above 10K', () => {
    expect(tokensCompact(40_000)).toBe('≈40K tokens');
    expect(tokensCompact(10_400)).toBe('≈10K tokens');
  });

  it('bytesReadable uses 1024-based KB, raw bytes under 1 KB', () => {
    expect(bytesReadable(512)).toBe('512 B');
    expect(bytesReadable(1024)).toBe('1 KB');
    expect(bytesReadable(34_816)).toBe('34 KB');
  });

  it('charactersReadable groups thousands and appends 字', () => {
    expect(charactersReadable(8200)).toBe('8,200 字');
    expect(charactersReadable(42)).toBe('42 字');
  });

  it('every tier has a plain-language verdict', () => {
    expect(TIER_VERDICT.comfortable).toBe('AI 可轻松读完整篇');
    expect(TIER_VERDICT.large).toBe('较大，部分 AI 需分段读');
    expect(TIER_VERDICT.tooLarge).toBe('过大，建议拆分后再给 AI');
  });
});
