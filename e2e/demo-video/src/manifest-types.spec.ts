import { describe, it, expect } from 'vitest';
import { parseManifest } from './manifest-types';

const valid = {
  locale: 'en',
  width: 3200,
  height: 2000,
  shots: [
    { scene: '01-intro', hold: 2.6, title: 'Awaken', subtitle: 'x' },
    { scene: '02-providers', hold: 2.2, image: '02-providers-01.png', caption: 'Providers', cursor: { x: 10, y: 20 }, click: true },
  ],
};

describe('parseManifest', () => {
  it('accepts a valid manifest and defaults transition to fade', () => {
    const m = parseManifest(valid);
    expect(m.shots).toHaveLength(2);
    expect(m.shots[1].transition).toBe('fade');
    expect(m.shots[1].click).toBe(true);
    expect(m.shots[0].image).toBeUndefined();
  });

  it('rejects empty shots', () => {
    expect(() => parseManifest({ ...valid, shots: [] })).toThrow(/no shots/);
  });

  it('rejects a bad locale', () => {
    expect(() => parseManifest({ ...valid, locale: 'fr' })).toThrow(/locale/);
  });

  it('rejects a shot with neither image nor title', () => {
    expect(() => parseManifest({ ...valid, shots: [{ scene: 'x', hold: 1 }] })).toThrow(/image or title/);
  });
});
