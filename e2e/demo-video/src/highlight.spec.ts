import { describe, it, expect } from 'vitest';
import { selectHighlight, selectGifShots, HIGHLIGHT_SCENES } from './highlight';
import type { Shot } from './manifest-types';

const s = (scene: string): Shot => ({ scene, index: 0, hold: 2, image: 'a.png' });
const sc = (scene: string, image: string, caption?: string): Shot =>
  ({ scene, index: 0, hold: 2, image, caption });

describe('selectHighlight', () => {
  it('keeps only highlight scenes, preserving order', () => {
    const shots = [s('01-intro'), s('02-providers'), s('05-agents-list'), s('09-sandbox'), s('12-eval')];
    const out = selectHighlight(shots);
    expect(out.map((x) => x.scene)).toEqual(['02-providers', '09-sandbox', '12-eval']);
  });

  it('exposes the default highlight scene list', () => {
    expect(HIGHLIGHT_SCENES).toContain('04-assistant-create-agent');
  });
});

describe('selectGifShots', () => {
  it('picks the last captioned image shot per highlight scene with a long hold', () => {
    const shots = [
      sc('01-intro', 'x.png', 'skip me'),
      sc('02-providers', 'a.png', 'open'),
      sc('02-providers', 'b.png', 'Connection OK'),
      sc('09-sandbox', 'c.png'),
      sc('09-sandbox', 'd.png', 'tool card'),
    ];
    const out = selectGifShots(shots, 2.4);
    expect(out.map((x) => x.scene)).toEqual(['02-providers', '09-sandbox']);
    expect(out[0].caption).toBe('Connection OK');
    expect(out[1].caption).toBe('tool card');
    expect(out[0].hold).toBe(2.4);
    expect(out[0].cursor).toBeUndefined();
  });

  it('applies narration caption overrides and a constant zoom', () => {
    const shots = [sc('02-providers', 'a.png', 'Connection OK')];
    const out = selectGifShots(shots, 2.2, { '02-providers': '1 · Connect a real model' }, 1.18);
    expect(out[0].caption).toBe('1 · Connect a real model');
    expect(out[0].zoom).toBe(1.18);
    expect(out[0].hold).toBe(2.2);
  });
});
