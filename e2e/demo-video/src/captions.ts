import type { Shot } from './manifest-types';
import type { ShotWindow } from './timing';

/**
 * Captions render OUTSIDE the camera transform (fixed lower-third) so they stay
 * readable while the camera pushes and pans. Because a caption persists across
 * several shots (nav → click → result all carry the same line), we coalesce
 * consecutive same-caption shots into one segment and fade only at segment
 * boundaries — otherwise the caption would flicker on every beat.
 */
export interface CaptionSegment {
  text: string;
  start: number;
  end: number;
}

export function captionSegments(shots: Shot[], windows: ShotWindow[]): CaptionSegment[] {
  const segs: CaptionSegment[] = [];
  let lastIdx = -2;
  shots.forEach((s, i) => {
    const text = s.title ? '' : (s.caption ?? '');
    if (!text) return; // title cards & captionless beats break any run
    const w = windows[i];
    const last = segs[segs.length - 1];
    if (last && last.text === text && i === lastIdx + 1) {
      last.end = w.end; // extend the live segment over this consecutive beat
    } else {
      segs.push({ text, start: w.start, end: w.end });
    }
    lastIdx = i;
  });
  return segs;
}
