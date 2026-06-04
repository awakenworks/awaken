import React from 'react';
import {
  AbsoluteFill,
  Img,
  staticFile,
  useCurrentFrame,
  useVideoConfig,
  interpolate,
} from 'remotion';
import type { Shot } from '../manifest-types';
import { Caption } from './Caption';
import { Cursor } from './Cursor';
import { TitleCard } from './TitleCard';

export const ShotView: React.FC<{ shot: Shot; srcWidth: number; srcHeight: number }> = ({
  shot,
  srcWidth,
  srcHeight,
}) => {
  const frame = useCurrentFrame();
  const { width, height, durationInFrames } = useVideoConfig();

  if (shot.title) {
    return <TitleCard title={shot.title} subtitle={shot.subtitle ?? ''} />;
  }

  const scaleX = width / srcWidth;
  const scaleY = height / srcHeight;
  const progress = interpolate(frame, [0, durationInFrames], [0, 1], {
    extrapolateRight: 'clamp',
  });

  let transform = `scale(${interpolate(progress, [0, 1], [1, 1.04])})`;
  let transformOrigin = 'center center';
  if (shot.focus) {
    const cxPct = (((shot.focus.x + shot.focus.w / 2) * scaleX) / width) * 100;
    const cyPct = (((shot.focus.y + shot.focus.h / 2) * scaleY) / height) * 100;
    transformOrigin = `${cxPct}% ${cyPct}%`;
    transform = `scale(${interpolate(progress, [0, 1], [1, 1.12])})`;
  }

  return (
    <AbsoluteFill style={{ backgroundColor: '#020617', overflow: 'hidden' }}>
      <AbsoluteFill style={{ transform, transformOrigin }}>
        {shot.image ? (
          <Img
            src={staticFile(shot.image)}
            style={{ width: '100%', height: '100%', objectFit: 'cover' }}
          />
        ) : null}
      </AbsoluteFill>
      {shot.cursor ? (
        <Cursor x={shot.cursor.x * scaleX} y={shot.cursor.y * scaleY} click={shot.click === true} />
      ) : null}
      {shot.caption ? <Caption text={shot.caption} /> : null}
    </AbsoluteFill>
  );
};
