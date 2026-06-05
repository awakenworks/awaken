import { describe, it, expect } from 'vitest';
import { captionSegments } from './captions';
import type { Shot } from './manifest-types';
import type { ShotWindow } from './timing';

const shot = (caption?: string, title?: string): Shot => ({
  scene: 'x',
  index: 0,
  hold: 2,
  image: title ? undefined : 'a.png',
  caption,
  title,
});

const wins = (n: number): ShotWindow[] =>
  Array.from({ length: n }, (_, i) => ({ start: i * 10, end: i * 10 + 10 }));

describe('captionSegments', () => {
  it('coalesces consecutive same-caption beats into one segment', () => {
    const shots = [shot('A'), shot('A'), shot('B')];
    const segs = captionSegments(shots, wins(3));
    expect(segs).toEqual([
      { text: 'A', start: 0, end: 20 },
      { text: 'B', start: 20, end: 30 },
    ]);
  });

  it('does not merge same captions separated by a title card', () => {
    const shots = [shot('A'), shot(undefined, 'Title'), shot('A')];
    const segs = captionSegments(shots, wins(3));
    expect(segs).toEqual([
      { text: 'A', start: 0, end: 10 },
      { text: 'A', start: 20, end: 30 },
    ]);
  });

  it('skips captionless and title beats', () => {
    const shots = [shot(undefined, 'Title'), shot(''), shot('Only')];
    const segs = captionSegments(shots, wins(3));
    expect(segs).toEqual([{ text: 'Only', start: 20, end: 30 }]);
  });
});
