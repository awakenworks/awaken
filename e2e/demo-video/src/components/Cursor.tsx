import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';

export const Cursor: React.FC<{ x: number; y: number; click: boolean }> = ({ x, y, click }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const appear = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 12 });
  const size = 26;
  const ripple = click ? interpolate(frame, [4, 22], [0, 1], { extrapolateRight: 'clamp' }) : 0;
  return (
    <>
      {click ? (
        <div
          style={{
            position: 'absolute',
            left: x - 30,
            top: y - 30,
            width: 60,
            height: 60,
            borderRadius: '50%',
            border: '2px solid rgba(56,189,248,0.8)',
            transform: `scale(${ripple})`,
            opacity: 1 - ripple,
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
          transform: `scale(${appear})`,
        }}
      />
    </>
  );
};
