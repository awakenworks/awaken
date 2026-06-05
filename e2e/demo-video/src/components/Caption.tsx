import React from 'react';

/**
 * Presentational caption. Opacity (fade in/out at segment boundaries) is
 * supplied by CaptionLayer from the global timeline so the line stays steady
 * across the several beats that share it. Rendered OUTSIDE the CameraRig and
 * pinned to the output frame's lower-third, so the camera's zoom/pan never
 * scales or shifts the text.
 */
export const Caption: React.FC<{ text: string; opacity: number }> = ({ text, opacity }) => {
  const y = (1 - opacity) * 24;
  return (
    <div
      style={{
        position: 'absolute',
        left: '50%',
        bottom: 48,
        transform: `translateX(-50%) translateY(${y}px)`,
        opacity,
        maxWidth: '80%',
        padding: '16px 30px',
        borderRadius: 18,
        background: 'rgba(2,6,23,0.86)',
        color: '#e2e8f0',
        font: '600 30px/1.4 system-ui,-apple-system,sans-serif',
        letterSpacing: 0.3,
        border: '1px solid rgba(56,189,248,0.45)',
        boxShadow: '0 14px 50px rgba(0,0,0,0.5)',
        textAlign: 'center',
        backdropFilter: 'blur(8px)',
      }}
    >
      {text}
    </div>
  );
};
