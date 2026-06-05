import React from 'react';
import { Composition } from 'remotion';
import { Demo, TRANSITION_FRAMES, type CameraProfile } from './Demo';
import { parseManifest, type Manifest, type Shot } from './manifest-types';
import { selectHighlight, selectGifShots, GIF_SCENES } from './highlight';
import { totalDurationInFrames } from './timing';

const FPS = 30;
const WIDTH = 1920;
const HEIGHT = 1200;

type Props = { manifest: Manifest; shots: Shot[]; profile?: CameraProfile };

// MP4 (full walkthrough / highlight cut): calm camera, gentle push, light dim —
// the viewer has time, so we keep context and avoid motion fatigue.
const MP4_PROFILE: CameraProfile = { zMax: 1.4, focusFill: 0.6, moveSeconds: 0.85, dim: 0.2 };
// GIF: tighter zoom (small render needs legibility), stronger spotlight. Slower,
// fully-settled cursor travel (0.8s) so the eye can follow each move — short,
// jumpy hops read as "too fast" and break the sense of one continuous flow.
const GIF_PROFILE: CameraProfile = { zMax: 1.55, focusFill: 0.5, moveSeconds: 0.8, dim: 0.34 };

const stub: Props = {
  manifest: { locale: 'en', width: 3200, height: 2000, shots: [] },
  shots: [],
};

function metaFor(rawProps: unknown, pick: (m: Manifest) => Shot[], profile: CameraProfile) {
  const raw = rawProps as any;
  if (!raw || !Array.isArray(raw.shots) || raw.shots.length === 0) {
    return { durationInFrames: 1, props: stub };
  }
  const manifest = parseManifest(raw);
  const shots = pick(manifest);
  return {
    durationInFrames: totalDurationInFrames(shots, FPS, TRANSITION_FRAMES),
    props: { manifest, shots, profile } satisfies Props,
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
      calculateMetadata={({ props }) => metaFor(props, (m) => m.shots, MP4_PROFILE)}
    />
    <Composition
      id="DemoHighlight"
      component={Demo}
      fps={FPS}
      width={WIDTH}
      height={HEIGHT}
      durationInFrames={1}
      defaultProps={stub}
      calculateMetadata={({ props }) => metaFor(props, (m) => selectHighlight(m.shots), MP4_PROFILE)}
    />
    <Composition
      id="DemoGif"
      component={Demo}
      fps={FPS}
      width={WIDTH}
      height={HEIGHT}
      durationInFrames={1}
      defaultProps={stub}
      calculateMetadata={({ props }) =>
        metaFor(
          props,
          // Slower holds so clicks and reads don't flash past: interaction beats
          // linger long enough to register the click, payoff beats long enough
          // to read the caption's "why".
          (m) => selectGifShots(m.shots, { scenes: GIF_SCENES, holdSeconds: 2.0, actionHoldSeconds: 1.4 }),
          GIF_PROFILE,
        )
      }
    />
  </>
);
