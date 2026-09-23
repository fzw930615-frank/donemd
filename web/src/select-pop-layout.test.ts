import { describe, expect, it } from 'vitest';
import {
  computeSelectPopLayout,
  POP_MAX_HEIGHT,
  POP_MIN_HEIGHT,
} from './select-pop-layout';

const VIEWPORT = 680; // settings window height
const faceAt = (top: number) => ({ top, bottom: top + 28, left: 20, width: 300 });

describe('computeSelectPopLayout', () => {
  it('opens downward with full height when space is ample', () => {
    const l = computeSelectPopLayout(faceAt(100), VIEWPORT);
    expect(l.top).toBe(100 + 28 + 4);
    expect(l.bottom).toBeNull();
    expect(l.maxHeight).toBe(POP_MAX_HEIGHT);
    expect(l.left).toBe(20);
    expect(l.width).toBe(300);
  });

  it('clamps max-height to the space left below', () => {
    // spaceBelow = 680 - 528 - 8 = 144 → 144 - 4 = 140
    const l = computeSelectPopLayout(faceAt(500), VIEWPORT);
    expect(l.bottom).toBeNull();
    expect(l.maxHeight).toBe(140);
  });

  it('flips upward when below is cramped and above has more room', () => {
    // spaceBelow = 44, spaceAbove = 592
    const l = computeSelectPopLayout(faceAt(600), VIEWPORT);
    expect(l.top).toBeNull();
    expect(l.bottom).toBe(VIEWPORT - 600 + 4);
    expect(l.maxHeight).toBe(POP_MAX_HEIGHT);
  });

  it('stays downward with the height floor when below is cramped but above is worse', () => {
    // Tiny window, face near top: spaceBelow = 54, spaceAbove = 52 → down, floor.
    const l = computeSelectPopLayout(faceAt(60), 150);
    expect(l.top).not.toBeNull();
    expect(l.maxHeight).toBe(POP_MIN_HEIGHT);
  });

  it('flips upward in a tiny window when the face sits low', () => {
    // spaceBelow = 4, spaceAbove = 102 → up, 102 - 4 = 98
    const l = computeSelectPopLayout(faceAt(110), 150);
    expect(l.top).toBeNull();
    expect(l.maxHeight).toBe(98);
  });

  it('never exceeds POP_MAX_HEIGHT even with a huge viewport', () => {
    const l = computeSelectPopLayout(faceAt(40), 4000);
    expect(l.maxHeight).toBe(POP_MAX_HEIGHT);
  });

  it('opens downward exactly at the cramped-space boundary', () => {
    // spaceBelow = 680 - 552 - 8 = 120 → not < COMFORTABLE_SPACE → down, 116.
    const l = computeSelectPopLayout(faceAt(524), VIEWPORT);
    expect(l.top).not.toBeNull();
    expect(l.maxHeight).toBe(116);
  });

  it('anchors an off-screen-below face at the viewport bottom, flipping up', () => {
    // Page scrolled down: the face sits at y≈1374 of a 680px window. Both
    // edges clamp to 680 → flip up, popup pinned near the viewport bottom.
    const l = computeSelectPopLayout(faceAt(1000), VIEWPORT);
    expect(l.top).toBeNull();
    expect(l.bottom).toBe(4);
    expect(l.maxHeight).toBe(POP_MAX_HEIGHT);
    // The popup's top edge (bottom edge at 680 - 4 = 676, minus height).
    const popTop = VIEWPORT - (l.bottom as number) - POP_MAX_HEIGHT;
    expect(popTop).toBeGreaterThanOrEqual(0);
  });

  it('anchors an off-screen-above face at the viewport top, opening down', () => {
    // Negative rect (scrolled past the face): clamps to 0 → opens downward
    // from the viewport top.
    const l = computeSelectPopLayout(faceAt(-100), VIEWPORT);
    expect(l.bottom).toBeNull();
    expect(l.top).toBe(4);
    expect(l.maxHeight).toBe(POP_MAX_HEIGHT);
  });

  it('handles a face straddling the viewport bottom', () => {
    // top=650, bottom=678: below-space is negative → flip up.
    const l = computeSelectPopLayout(faceAt(650), VIEWPORT);
    expect(l.top).toBeNull();
    expect(l.bottom).toBe(VIEWPORT - 650 + 4);
    expect(l.maxHeight).toBe(POP_MAX_HEIGHT);
  });
});
