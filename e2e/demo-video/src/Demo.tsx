import React from 'react';
import { AbsoluteFill, useCurrentFrame, useVideoConfig } from 'remotion';
import { TransitionSeries, linearTiming } from '@remotion/transitions';
import { fade } from '@remotion/transitions/fade';
import { slide } from '@remotion/transitions/slide';
import type { Manifest, Shot } from './manifest-types';
import { holdFrames } from './timing';
import {
  activeShotIndex,
  buildTimeline,
  cameraAt,
  cursorAt,
  type CameraOptions,
  type Timeline,
} from './camera';
import { captionSegments } from './captions';
import { ShotBackground } from './components/Shot';
import { Cursor } from './components/Cursor';
import { Highlight } from './components/Highlight';
import { Caption } from './components/Caption';

export const TRANSITION_FRAMES = 12;

/** Per-composition camera/highlight knobs (MP4 vs GIF differ in feel). */
export interface CameraProfile {
  zMax?: number;
  focusFill?: number;
  moveSeconds?: number;
  rippleSeconds?: number;
  /** Spotlight dim strength for the highlight (0 = none). */
  dim?: number;
}

const clamp01 = (v: number) => Math.min(1, Math.max(0, v));

/** The global camera transform: background, highlight, and cursor all live
 *  inside it so they zoom/pan as one. */
const CameraStage: React.FC<{ tl: Timeline; children: React.ReactNode }> = ({ tl, children }) => {
  const frame = useCurrentFrame();
  const c = cameraAt(tl, frame);
  return (
    <AbsoluteFill
      style={{
        transform: `scale(${c.scale}) translate(${c.tx}px, ${c.ty}px)`,
        transformOrigin: '0 0',
      }}
    >
      {children}
    </AbsoluteFill>
  );
};

/** Background screenshots, crossfading between shots (the only thing the
 *  TransitionSeries still owns). */
const BackgroundSeries: React.FC<{ shots: Shot[]; fps: number }> = ({ shots, fps }) => {
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
        <ShotBackground shot={s} />
      </TransitionSeries.Sequence>,
    );
  });
  return <TransitionSeries>{children}</TransitionSeries>;
};

const HighlightLayer: React.FC<{ shots: Shot[]; tl: Timeline; k: number; dim?: number }> = ({
  shots,
  tl,
  k,
  dim,
}) => {
  const frame = useCurrentFrame();
  const i = activeShotIndex(tl, frame);
  const shot = shots[i];
  if (!shot?.focus) return null;
  const win = tl.windows[i];
  const local = frame - win.start;
  const len = win.end - win.start;
  // Ring blooms as the cursor lands (~half the travel in), holds, fades at end.
  const appear = clamp01((local - tl.moveFrames * 0.5) / (tl.moveFrames * 0.5));
  const disappear = clamp01((len - local) / 6);
  const opacity = Math.min(appear, disappear);
  const rect = {
    x: shot.focus.x * k,
    y: shot.focus.y * k,
    w: shot.focus.w * k,
    h: shot.focus.h * k,
  };
  return <Highlight rect={rect} opacity={opacity} dim={dim} />;
};

const CursorLayer: React.FC<{ tl: Timeline }> = ({ tl }) => {
  const frame = useCurrentFrame();
  const c = cursorAt(tl, frame);
  if (c.opacity <= 0) return null;
  return <Cursor x={c.x} y={c.y} opacity={c.opacity} clickProgress={c.clickProgress} />;
};

const CaptionLayer: React.FC<{ shots: Shot[]; tl: Timeline }> = ({ shots, tl }) => {
  const frame = useCurrentFrame();
  const segments = React.useMemo(() => captionSegments(shots, tl.windows), [shots, tl]);
  const seg = segments.find((s) => frame >= s.start && frame < s.end);
  if (!seg) return null;
  const local = frame - seg.start;
  const len = seg.end - seg.start;
  const opacity = Math.min(clamp01(local / 8), clamp01((len - local) / 8));
  return <Caption text={seg.text} opacity={opacity} />;
};

export const Demo: React.FC<{ manifest: Manifest; shots: Shot[]; profile?: CameraProfile }> = ({
  manifest,
  shots,
  profile,
}) => {
  const { fps, width, height } = useVideoConfig();
  if (shots.length === 0) return <AbsoluteFill style={{ backgroundColor: '#020617' }} />;

  const opts: CameraOptions = {
    srcWidth: manifest.width,
    srcHeight: manifest.height,
    outWidth: width,
    outHeight: height,
    fps,
    transitionFrames: TRANSITION_FRAMES,
    zMax: profile?.zMax,
    focusFill: profile?.focusFill,
    moveSeconds: profile?.moveSeconds,
    rippleSeconds: profile?.rippleSeconds,
  };
  const tl = buildTimeline(shots, opts);
  const k = width / manifest.width;

  return (
    <AbsoluteFill style={{ backgroundColor: '#020617' }}>
      <CameraStage tl={tl}>
        <BackgroundSeries shots={shots} fps={fps} />
        <HighlightLayer shots={shots} tl={tl} k={k} dim={profile?.dim} />
        <CursorLayer tl={tl} />
      </CameraStage>
      <CaptionLayer shots={shots} tl={tl} />
    </AbsoluteFill>
  );
};
