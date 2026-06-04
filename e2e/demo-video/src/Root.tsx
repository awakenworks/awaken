import React from 'react';
import { Composition } from 'remotion';
import { Demo, TRANSITION_FRAMES } from './Demo';
import { parseManifest, type Manifest, type Shot } from './manifest-types';
import { selectHighlight, selectGifShots } from './highlight';
import { totalDurationInFrames } from './timing';

const FPS = 30;
const WIDTH = 1920;
const HEIGHT = 1200;

type Props = { manifest: Manifest; shots: Shot[] };

const stub: Props = {
  manifest: { locale: 'en', width: 3200, height: 2000, shots: [] },
  shots: [],
};

function metaFor(rawProps: unknown, pick: (m: Manifest) => Shot[]) {
  const raw = rawProps as any;
  if (!raw || !Array.isArray(raw.shots) || raw.shots.length === 0) {
    return { durationInFrames: 1, props: stub };
  }
  const manifest = parseManifest(raw);
  const shots = pick(manifest);
  return {
    durationInFrames: totalDurationInFrames(shots, FPS, TRANSITION_FRAMES),
    props: { manifest, shots } satisfies Props,
  };
}

export const RemotionRoot: React.FC = () => (
  <>
    <Composition
      id="DemoLong"
      component={Demo}
      fps={FPS}
      width={WIDTH}
      height={HEIGHT}
      durationInFrames={1}
      defaultProps={stub}
      calculateMetadata={({ props }) => metaFor(props, (m) => m.shots)}
    />
    <Composition
      id="DemoHighlight"
      component={Demo}
      fps={FPS}
      width={WIDTH}
      height={HEIGHT}
      durationInFrames={1}
      defaultProps={stub}
      calculateMetadata={({ props }) => metaFor(props, (m) => selectHighlight(m.shots))}
    />
    <Composition
      id="DemoGif"
      component={Demo}
      fps={FPS}
      width={WIDTH}
      height={HEIGHT}
      durationInFrames={1}
      defaultProps={stub}
      calculateMetadata={({ props }) => metaFor(props, (m) => selectGifShots(m.shots))}
    />
  </>
);
