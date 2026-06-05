import React from 'react';
import type { Rect } from '../manifest-types';

/**
 * The highlight is the cut's primary expressive device: a rounded glow ring
 * around the element the cursor is acting on, plus a spotlight that gently dims
 * everything outside it. The dim is drawn with a huge spread box-shadow so the
 * ring's interior stays clear and the rest of the frame recedes — the eye goes
 * straight to the element without needing a caption to explain it. Rendered in
 * output-pixel space inside the CameraRig, so it tracks the element under zoom.
 */
export const Highlight: React.FC<{ rect: Rect; opacity: number; dim?: number }> = ({
  rect,
  opacity,
  dim = 0.32,
}) => {
  if (opacity <= 0) return null;
  return (
    <div
      style={{
        position: 'absolute',
        left: rect.x,
        top: rect.y,
        width: rect.w,
        height: rect.h,
        borderRadius: 14,
        border: '2px solid rgba(56,189,248,0.9)',
        boxShadow: `0 0 0 9999px rgba(2,6,23,${dim * opacity}), 0 0 26px 6px rgba(56,189,248,0.5)`,
        opacity,
      }}
    />
  );
};
