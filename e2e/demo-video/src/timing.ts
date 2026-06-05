import type { Shot } from './manifest-types';

export function holdFrames(shot: Shot, fps: number): number {
  return Math.max(1, Math.round(shot.hold * fps));
}

/** Absolute frame window [start, end) for each shot on the composition's global
 *  timeline. A non-cut transition overlaps its predecessor by `transitionFrames`
 *  (the crossfade), so a shot's start is pulled back into the prior shot's tail.
 *  This is the single source of truth the camera/cursor timeline indexes into so
 *  motion never drifts out of phase with the background crossfades. */
export interface ShotWindow {
  start: number;
  end: number;
}

export function shotWindows(
  shots: Shot[],
  fps: number,
  transitionFrames: number,
): ShotWindow[] {
  const out: ShotWindow[] = [];
  let prevEnd = 0;
  shots.forEach((s, i) => {
    const len = holdFrames(s, fps);
    const overlap = i > 0 && (s.transition ?? 'fade') !== 'cut' ? transitionFrames : 0;
    const start = i === 0 ? 0 : prevEnd - overlap;
    const end = start + len;
    out.push({ start, end });
    prevEnd = end;
  });
  return out;
}

export function totalDurationInFrames(
  shots: Shot[],
  fps: number,
  transitionFrames: number,
): number {
  if (shots.length === 0) return 1;
  const w = shotWindows(shots, fps, transitionFrames);
  return Math.max(1, w[w.length - 1].end);
}
