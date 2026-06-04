import React from 'react';
import { useVideoConfig } from 'remotion';
import { TransitionSeries, linearTiming } from '@remotion/transitions';
import { fade } from '@remotion/transitions/fade';
import { slide } from '@remotion/transitions/slide';
import type { Manifest, Shot } from './manifest-types';
import { holdFrames } from './timing';
import { ShotView } from './components/Shot';

export const TRANSITION_FRAMES = 12;

export const Demo: React.FC<{ manifest: Manifest; shots: Shot[] }> = ({ manifest, shots }) => {
  const { fps } = useVideoConfig();
  const { width: srcW, height: srcH } = manifest;
  const children: React.ReactNode[] = [];

  shots.forEach((s, i) => {
    if (i > 0 && (s.transition ?? 'fade') !== 'cut') {
      const presentation = s.transition === 'slide' ? slide() : fade();
      children.push(
        <TransitionSeries.Transition
          key={`t-${i}`}
          presentation={presentation}
          timing={linearTiming({ durationInFrames: TRANSITION_FRAMES })}
        />,
      );
    }
    children.push(
      <TransitionSeries.Sequence key={`s-${i}`} durationInFrames={holdFrames(s, fps)}>
        <ShotView shot={s} srcWidth={srcW} srcHeight={srcH} />
      </TransitionSeries.Sequence>,
    );
  });

  return <TransitionSeries>{children}</TransitionSeries>;
};
