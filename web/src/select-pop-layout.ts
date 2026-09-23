/**
 * Viewport-aware layout for the custom select popup (settings.ts).
 *
 * The settings page renders provider cards with `overflow: hidden` (rounded
 * corners), which clips any absolutely-positioned popup inside them — an
 * 8+ model list became unreachable. The popup is therefore `position: fixed`
 * (escapes ancestor clipping) and this pure helper decides where it lands:
 * below the face when there's room, flipped above when there isn't, with
 * max-height clamped to whichever side it opens into.
 */

export interface FaceRect {
  top: number;
  bottom: number;
  left: number;
  width: number;
}

export interface PopLayout {
  left: number;
  width: number;
  /** px from viewport top when opening downward, null when flipped up. */
  top: number | null;
  /** px from viewport bottom when flipped up, null when opening downward. */
  bottom: number | null;
  maxHeight: number;
}

export const POP_MAX_HEIGHT = 240;
export const POP_MIN_HEIGHT = 60;
const VIEWPORT_MARGIN = 8;
const FACE_GAP = 4;
/** Below this much room the list feels cramped — prefer the other side. */
const COMFORTABLE_SPACE = 120;

export function computeSelectPopLayout(face: FaceRect, viewportHeight: number): PopLayout {
  // Clamp the face to the viewport first: a click can land mid-scroll with
  // the face partly or fully off-screen, and both space computations (and
  // the flip anchor) would go negative and place the popup off-screen too.
  const faceTop = Math.min(Math.max(face.top, 0), viewportHeight);
  const faceBottom = Math.min(Math.max(face.bottom, 0), viewportHeight);
  const spaceBelow = viewportHeight - faceBottom - VIEWPORT_MARGIN;
  const spaceAbove = faceTop - VIEWPORT_MARGIN;
  const openUp = spaceBelow < COMFORTABLE_SPACE && spaceAbove > spaceBelow;
  const base = { left: face.left, width: face.width };
  if (openUp) {
    return {
      ...base,
      top: null,
      bottom: viewportHeight - faceTop + FACE_GAP,
      maxHeight: Math.max(POP_MIN_HEIGHT, Math.min(POP_MAX_HEIGHT, spaceAbove - FACE_GAP)),
    };
  }
  return {
    ...base,
    top: faceBottom + FACE_GAP,
    bottom: null,
    maxHeight: Math.max(POP_MIN_HEIGHT, Math.min(POP_MAX_HEIGHT, spaceBelow - FACE_GAP)),
  };
}
