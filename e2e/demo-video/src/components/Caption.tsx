import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';

export const Caption: React.FC<{ text: string }> = ({ text }) => {
  const frame = useCurrentFrame();
  const { fps, durationInFrames } = useVideoConfig();
  const enter = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 10 });
  const exit = interpolate(
    frame,
    [durationInFrames - 8, durationInFrames],
    [1, 0],
    { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' },
  );
  const opacity = Math.min(enter, exit);
  const y = interpolate(enter, [0, 1], [24, 0]);
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
