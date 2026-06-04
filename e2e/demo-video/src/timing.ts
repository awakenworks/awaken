import type { Shot } from './manifest-types';

export function holdFrames(shot: Shot, fps: number): number {
  return Math.max(1, Math.round(shot.hold * fps));
}

export function totalDurationInFrames(
  shots: Shot[],
  fps: number,
  transitionFrames: number,
): number {
  const holds = shots.reduce((acc, s) => acc + holdFrames(s, fps), 0);
  const overlaps = shots.slice(1).filter((s) => (s.transition ?? 'fade') !== 'cut').length;
  return Math.max(1, holds - overlaps * transitionFrames);
}
