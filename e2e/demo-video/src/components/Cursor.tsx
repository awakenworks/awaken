import React from 'react';

/**
 * Presentational cursor. Position/opacity/click are supplied by the global
 * camera timeline (`cursorAt`) rather than computed locally, so the pointer is
 * one continuous travelling object across the whole cut instead of popping into
 * place per shot. Lives inside the CameraRig, in output-pixel space, so it
 * scales and pans with the background it points at.
 */
export const Cursor: React.FC<{
  x: number;
  y: number;
  opacity: number;
  clickProgress: number;
  size?: number;
}> = ({ x, y, opacity, clickProgress, size = 26 }) => {
  const rippling = clickProgress > 0 && clickProgress < 1;
  return (
    <div style={{ opacity }}>
      {rippling ? (
        <div
          style={{
            position: 'absolute',
            left: x - 30,
            top: y - 30,
            width: 60,
            height: 60,
            borderRadius: '50%',
            border: '2px solid rgba(56,189,248,0.8)',
            transform: `scale(${clickProgress})`,
            opacity: 1 - clickProgress,
          }}
        />
      ) : null}
      <div
        style={{
          position: 'absolute',
          left: x - size / 2,
          top: y - size / 2,
          width: size,
          height: size,
          borderRadius: '50%',
          background: 'rgba(56,189,248,0.45)',
          border: '2px solid rgba(56,189,248,0.95)',
          boxShadow: '0 0 16px 4px rgba(56,189,248,0.55)',
        }}
      />
    </div>
  );
};
