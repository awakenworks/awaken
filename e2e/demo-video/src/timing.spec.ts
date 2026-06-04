import { describe, it, expect } from 'vitest';
import { holdFrames, totalDurationInFrames } from './timing';
import type { Shot } from './manifest-types';

const s = (hold: number, transition: Shot['transition']): Shot =>
  ({ scene: 'x', index: 0, hold, transition, image: 'a.png' });

describe('timing', () => {
  it('holdFrames rounds seconds to frames (min 1)', () => {
    expect(holdFrames(s(2, 'fade'), 30)).toBe(60);
    expect(holdFrames(s(0.01, 'fade'), 30)).toBe(1);
  });

  it('totalDurationInFrames subtracts one overlap per non-cut transition', () => {
    const shots = [s(2, 'fade'), s(3, 'fade'), s(1, 'cut')];
    // 60 + 90 + 30 = 180 holds; one fade overlap (index 1) of 12 -> 168
    expect(totalDurationInFrames(shots, 30, 12)).toBe(168);
  });

  it('never returns less than 1', () => {
    expect(totalDurationInFrames([s(0.01, 'fade')], 30, 12)).toBe(1);
  });
});
