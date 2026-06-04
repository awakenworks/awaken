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

  // Shots are static by default — a stable, crisp screenshot. Motion comes from
  // the cursor pop, click ripple, captions, and the crossfade between shots.
  // `zoom` is a constant (non-animated) enlargement used by the GIF beats to
  // make content legible at small sizes; `focus` (if ever set) does a gentle
  // animated push toward a region. Neither is set on the MP4 shots.
  let transform: string | undefined;
  let transformOrigin: string | undefined;
  if (shot.zoom && shot.zoom !== 1) {
    transform = `scale(${shot.zoom})`;
    transformOrigin = 'center center';
  } else if (shot.focus) {
    const progress = interpolate(frame, [0, durationInFrames], [0, 1], {
      extrapolateRight: 'clamp',
    });
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
