import React from 'react';
import { AbsoluteFill, useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';

export const TitleCard: React.FC<{ title: string; subtitle: string; link?: string }> = ({
  title,
  subtitle,
  link,
}) => {
  const frame = useCurrentFrame();
  const { fps, durationInFrames } = useVideoConfig();
  const enter = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 18 });
  const exit = interpolate(
    frame,
    [durationInFrames - 12, durationInFrames],
    [1, 0],
    { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' },
  );
  const opacity = Math.min(enter, exit);
  return (
    <AbsoluteFill
      style={{
        background: 'radial-gradient(ellipse at center,#0b1220,#020617)',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 18,
        opacity,
      }}
    >
      <div
        style={{
          fontSize: 84,
          fontWeight: 800,
          letterSpacing: -1,
          fontFamily: 'system-ui,-apple-system,sans-serif',
          background: 'linear-gradient(90deg,#38bdf8,#a78bfa)',
          WebkitBackgroundClip: 'text',
          backgroundClip: 'text',
          color: 'transparent',
          transform: `scale(${interpolate(enter, [0, 1], [0.96, 1])})`,
        }}
      >
        {title}
      </div>
      <div
        style={{
          fontSize: 34,
          color: '#94a3b8',
          fontWeight: 500,
          maxWidth: '70%',
          textAlign: 'center',
          fontFamily: 'system-ui,-apple-system,sans-serif',
        }}
      >
        {subtitle}
      </div>
      {link ? (
        <div
          style={{
            marginTop: 10,
            fontSize: 26,
            fontWeight: 600,
            color: '#38bdf8',
            fontFamily: 'ui-monospace,SFMono-Regular,Menlo,monospace',
            letterSpacing: 0.5,
          }}
        >
          {link}
        </div>
      ) : null}
    </AbsoluteFill>
  );
};
